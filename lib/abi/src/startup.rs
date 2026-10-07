//! The process startup message.
//!
//! Every new process is started with exactly one handle: a channel (passed in
//! `rdi` to the entry point) carrying a single *startup message* written by
//! its creator. The message carries the program arguments, environment and
//! working directory, and a set of handles tagged with [roles](role) so the
//! new process can find its service registry, its framebuffer, its standard
//! input and output, and so on.
//!
//! Wire format (little-endian):
//!
//! ```text
//! u32 magic = "VSTA", u32 version, u32 n_args, u32 n_env, u32 n_handles
//! n_args  x (u32 len, bytes)        program name is args[0]
//! n_env   x (u32 len, bytes)        "KEY=VALUE"
//! u32 len, bytes                    working directory (absolute; empty: "/")
//! n_handles x u32 role              roles of the message's handles, in order
//! ```
//!
//! ELF (POSIX) programs find their arguments and environment on their stack
//! as the System V ABI describes, put there by the process that loaded
//! them, so their startup message leaves both out (a message holds at most
//! [`CHANNEL_MAX_BYTES`](crate::CHANNEL_MAX_BYTES), a command line may be
//! longer).

/// `"VSTA"`
pub const MAGIC: u32 = u32::from_le_bytes(*b"VSTA");
pub const VERSION: u32 = 2;

/// Well-known handle roles.
pub mod role {
    /// The root hardware resource (only given to `init`).
    pub const ROOT_RESOURCE: u32 = 1;
    /// VMO holding the initial ramdisk archive.
    pub const INITRD: u32 = 2;
    /// Physical VMO of the boot framebuffer.
    pub const FRAMEBUFFER: u32 = 3;
    /// VMO holding a [`crate::KernelBootInfo`].
    pub const BOOT_INFO: u32 = 4;
    /// Channel to the service registry (`init`).
    pub const REGISTRY: u32 = 16;
    /// Channel on which a service receives incoming client connections.
    pub const SERVICE_LISTENER: u32 = 17;
    /// VMO with the state of the terminal the program runs in (rows,
    /// columns, modes); see `vproto::tty`.
    pub const TERMINAL: u32 = 18;
    /// File descriptor `n` of a POSIX program has role `FD + n`
    /// (`n < MAX_FDS`): a socket (a pipe or a terminal) or a channel to an
    /// open file.
    pub const FD: u32 = 0x100;
    /// File descriptors that can be passed at startup.
    pub const MAX_FDS: u32 = 0x100;
    /// First application-defined role.
    pub const USER: u32 = 0x1000;

    /// The file descriptor a role stands for.
    pub const fn fd(role: u32) -> Option<u32> {
        if role >= FD && role < FD + MAX_FDS { Some(role - FD) } else { None }
    }
}

/// Errors produced while decoding a startup message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupError {
    BadMagic,
    Truncated,
    BadUtf8,
}

/// A decoded, borrowed view of a startup message.
#[derive(Debug, Clone, Copy)]
pub struct StartupView<'a> {
    data: &'a [u8],
    n_args: usize,
    n_env: usize,
    args_off: usize,
    env_off: usize,
    cwd_off: usize,
    roles_off: usize,
    n_handles: usize,
}

fn u32_at(d: &[u8], off: usize) -> Result<u32, StartupError> {
    d.get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or(StartupError::Truncated)
}

/// Skips `n` length-prefixed strings starting at `off`, validating them.
fn skip_strings(d: &[u8], mut off: usize, n: usize) -> Result<usize, StartupError> {
    for _ in 0..n {
        let len = u32_at(d, off)? as usize;
        let end = (off + 4).checked_add(len).ok_or(StartupError::Truncated)?;
        let s = d.get(off + 4..end).ok_or(StartupError::Truncated)?;
        core::str::from_utf8(s).map_err(|_| StartupError::BadUtf8)?;
        off = end;
    }
    Ok(off)
}

/// The validated string at `off`.
fn string_at(d: &[u8], off: usize) -> &str {
    let len = u32::from_le_bytes(d[off..off + 4].try_into().unwrap()) as usize;
    core::str::from_utf8(&d[off + 4..off + 4 + len]).unwrap_or("")
}

impl<'a> StartupView<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, StartupError> {
        if u32_at(data, 0)? != MAGIC || u32_at(data, 4)? != VERSION {
            return Err(StartupError::BadMagic);
        }
        let n_args = u32_at(data, 8)? as usize;
        let n_env = u32_at(data, 12)? as usize;
        let n_handles = u32_at(data, 16)? as usize;
        let args_off = 20;
        let env_off = skip_strings(data, args_off, n_args)?;
        let cwd_off = skip_strings(data, env_off, n_env)?;
        let roles_off = skip_strings(data, cwd_off, 1)?;
        if n_handles.checked_mul(4).and_then(|n| n.checked_add(roles_off)).is_none_or(|end| data.len() < end) {
            return Err(StartupError::Truncated);
        }
        Ok(StartupView { data, n_args, n_env, args_off, env_off, cwd_off, roles_off, n_handles })
    }

    fn strings(&self, off: usize, n: usize) -> impl Iterator<Item = &'a str> + 'a {
        let data = self.data;
        let mut off = off;
        (0..n).map(move |_| {
            let s = string_at(data, off);
            off += 4 + s.len();
            s
        })
    }

    pub fn args(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.strings(self.args_off, self.n_args)
    }

    pub fn env(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.strings(self.env_off, self.n_env)
    }

    /// The working directory (`"/"` if the creator gave none).
    pub fn cwd(&self) -> &'a str {
        match string_at(self.data, self.cwd_off) {
            "" => "/",
            cwd => cwd,
        }
    }

    /// The role of the `i`-th handle in the message.
    pub fn role(&self, i: usize) -> Option<u32> {
        if i >= self.n_handles {
            return None;
        }
        u32_at(self.data, self.roles_off + i * 4).ok()
    }

    pub fn handle_count(&self) -> usize {
        self.n_handles
    }
}

/// What a startup message says (the handles travel alongside it in the same
/// channel message, in the order of `roles`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Startup<'s> {
    pub args: &'s [&'s str],
    pub env: &'s [&'s str],
    /// Absolute working directory (empty: `/`).
    pub cwd: &'s str,
    pub roles: &'s [u32],
}

impl Startup<'_> {
    /// Serialises the message into `out`.
    pub fn encode(&self, out: &mut impl FnMut(&[u8])) {
        out(&MAGIC.to_le_bytes());
        out(&VERSION.to_le_bytes());
        out(&(self.args.len() as u32).to_le_bytes());
        out(&(self.env.len() as u32).to_le_bytes());
        out(&(self.roles.len() as u32).to_le_bytes());
        for s in self.args.iter().chain(self.env).chain([&self.cwd]) {
            out(&(s.len() as u32).to_le_bytes());
            out(s.as_bytes());
        }
        for r in self.roles {
            out(&r.to_le_bytes());
        }
    }

    /// The size of the encoded message.
    pub fn encoded_len(&self) -> usize {
        let strings: usize = self.args.iter().chain(self.env).chain([&self.cwd]).map(|s| 4 + s.len()).sum();
        20 + strings + 4 * self.roles.len()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    fn encode(s: &Startup) -> Vec<u8> {
        let mut v = Vec::new();
        s.encode(&mut |b| v.extend_from_slice(b));
        assert_eq!(v.len(), s.encoded_len());
        v
    }

    #[test]
    fn round_trip() {
        let msg = Startup {
            args: &["cc", "-o", "hello", "hello.c"],
            env: &["HOME=/home/user", "PATH=/system/bin"],
            cwd: "/home/user/src",
            roles: &[role::REGISTRY, role::FD, role::FD + 1, role::FD + 2, role::TERMINAL],
        };
        let bytes = encode(&msg);
        let v = StartupView::parse(&bytes).unwrap();
        assert_eq!(v.args().collect::<Vec<_>>(), msg.args);
        assert_eq!(v.env().collect::<Vec<_>>(), msg.env);
        assert_eq!(v.cwd(), "/home/user/src");
        assert_eq!(v.handle_count(), 5);
        assert_eq!((0..5).map(|i| v.role(i).unwrap()).collect::<Vec<_>>(), msg.roles);
        assert_eq!(v.role(5), None);
    }

    #[test]
    fn empty_cwd_is_root() {
        let bytes = encode(&Startup { args: &["init"], ..Default::default() });
        let v = StartupView::parse(&bytes).unwrap();
        assert_eq!(v.cwd(), "/");
        assert_eq!(v.env().count(), 0);
    }

    #[test]
    fn rejects_damage() {
        let bytes = encode(&Startup { args: &["a"], env: &["B=c"], cwd: "/d", roles: &[role::REGISTRY] });
        for len in 0..bytes.len() {
            assert!(StartupView::parse(&bytes[..len]).is_err(), "accepted {len} of {} bytes", bytes.len());
        }
        let mut old = bytes.clone();
        old[4] = 1;
        assert_eq!(StartupView::parse(&old).unwrap_err(), StartupError::BadMagic);
        let mut huge = bytes.clone();
        huge[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(StartupView::parse(&huge).unwrap_err(), StartupError::Truncated);
        let mut bad = bytes;
        bad[24] = 0xFF;
        assert_eq!(StartupView::parse(&bad).unwrap_err(), StartupError::BadUtf8);
    }

    #[test]
    fn fd_roles() {
        assert_eq!(role::fd(role::FD), Some(0));
        assert_eq!(role::fd(role::FD + 2), Some(2));
        assert_eq!(role::fd(role::FD + role::MAX_FDS), None);
        assert_eq!(role::fd(role::REGISTRY), None);
    }
}
