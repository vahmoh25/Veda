//! The process startup message.
//!
//! Every new process is started with exactly one handle: a channel (passed in
//! `rdi` to the entry point) carrying a single *startup message* written by
//! its creator. The message carries the program arguments, environment and a
//! set of handles tagged with [roles](role) so the new process can find its
//! service registry, its framebuffer, and so on.
//!
//! Wire format (little-endian):
//!
//! ```text
//! u32 magic = "VSTA", u32 version, u32 n_args, u32 n_env, u32 n_handles
//! n_args  x (u32 len, bytes)        program name is args[0]
//! n_env   x (u32 len, bytes)        "KEY=VALUE"
//! n_handles x u32 role              roles of the message's handles, in order
//! ```

/// `"VSTA"`
pub const MAGIC: u32 = u32::from_le_bytes(*b"VSTA");
pub const VERSION: u32 = 1;

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
    /// First application-defined role.
    pub const USER: u32 = 0x1000;
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
        let s = d.get(off + 4..off + 4 + len).ok_or(StartupError::Truncated)?;
        core::str::from_utf8(s).map_err(|_| StartupError::BadUtf8)?;
        off += 4 + len;
    }
    Ok(off)
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
        let roles_off = skip_strings(data, env_off, n_env)?;
        if data.len() < roles_off + n_handles * 4 {
            return Err(StartupError::Truncated);
        }
        Ok(StartupView { data, n_args, n_env, args_off, env_off, roles_off, n_handles })
    }

    fn strings(&self, off: usize, n: usize) -> impl Iterator<Item = &'a str> + 'a {
        let data = self.data;
        let mut off = off;
        (0..n).map(move |_| {
            let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&data[off + 4..off + 4 + len]).unwrap_or("");
            off += 4 + len;
            s
        })
    }

    pub fn args(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.strings(self.args_off, self.n_args)
    }

    pub fn env(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.strings(self.env_off, self.n_env)
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

/// Serialises a startup message into `out` (bytes only; the handles travel
/// alongside in the same channel message, in the same order as `roles`).
pub fn encode(args: &[&str], env: &[&str], roles: &[u32], out: &mut impl FnMut(&[u8])) {
    out(&MAGIC.to_le_bytes());
    out(&VERSION.to_le_bytes());
    out(&(args.len() as u32).to_le_bytes());
    out(&(env.len() as u32).to_le_bytes());
    out(&(roles.len() as u32).to_le_bytes());
    for s in args.iter().chain(env) {
        out(&(s.len() as u32).to_le_bytes());
        out(s.as_bytes());
    }
    for r in roles {
        out(&r.to_le_bytes());
    }
}
