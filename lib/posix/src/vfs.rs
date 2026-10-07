//! The file system service, the working directory and file metadata.

use alloc::string::String;

use vproto::fs::{Stat, vfs};
use vrt::sync::Mutex;

use crate::error;
use crate::fd::{self, Object};
use crate::linux::errno::{EIO, ENOTDIR};
use crate::linux::{self, at, mode};
use crate::path::{self, Resolved};

/// The connection to the VFS, made on first use.
static VFS: Mutex<Option<vfs::Client>> = Mutex::new(None);

/// The working directory (absolute, normalised).
static CWD: Mutex<String> = Mutex::new(String::new());

/// Runs `f` with the VFS connection. Calls from several threads take turns.
pub fn with<R>(f: impl FnOnce(&vfs::Client) -> R) -> Result<R, isize> {
    let mut slot = VFS.lock();
    if slot.is_none() {
        let channel = vproto::connect(vfs::NAME).map_err(|_| EIO)?;
        *slot = Some(vfs::Client::new(channel));
    }
    Ok(f(slot.as_ref().unwrap()))
}

pub fn cwd() -> String {
    let c = CWD.lock();
    if c.is_empty() { String::from("/") } else { c.clone() }
}

pub fn set_cwd(dir: String) {
    *CWD.lock() = dir;
}

/// Resolves `name` against the directory `dirfd` (`AT_FDCWD`: the working
/// directory).
pub fn resolve_at(dirfd: i32, name: &[u8]) -> Result<Resolved, isize> {
    if name.first() == Some(&b'/') {
        return path::resolve("/", name);
    }
    if dirfd == at::FDCWD {
        return path::resolve(&cwd(), name);
    }
    let desc = fd::get(dirfd)?;
    let Object::Dir(d) = &desc.object else { return Err(ENOTDIR) };
    path::resolve(&d.path, name)
}

/// What the VFS knows about `path`.
pub fn stat(path: &str) -> Result<Stat, isize> {
    error::ipc(with(|c| c.stat(path.into()))?)
}

/// `stat`, and `ENOTDIR` if a name ending in a slash is not a directory.
pub fn stat_resolved(r: &Resolved) -> Result<Stat, isize> {
    let st = stat(&r.path)?;
    if r.dir_only && !st.is_dir {
        return Err(ENOTDIR);
    }
    Ok(st)
}

/// The user and group POSIX programs run as.
pub const UID: u32 = vrt::process::UID;

/// The POSIX view of a file's metadata.
pub fn to_linux(st: &Stat) -> linux::Stat {
    let (kind, perms, nlink) = if st.is_dir {
        (mode::IFDIR, 0o755, 2)
    } else if st.char_device {
        (mode::IFCHR, 0o666, 1)
    } else if st.executable {
        (mode::IFREG, 0o755, 1)
    } else {
        (mode::IFREG, 0o644, 1)
    };
    // Nothing on a read-only file system can be written.
    let perms = if st.read_only { perms & !0o222 } else { perms };
    let time = linux::Timespec::from_ns(st.modified);
    linux::Stat {
        dev: st.device,
        ino: st.inode,
        nlink,
        mode: kind | perms,
        uid: UID,
        gid: UID,
        rdev: if st.char_device { st.inode } else { 0 },
        size: st.size as i64,
        blksize: 4096,
        blocks: st.size.div_ceil(512) as i64,
        atime: time,
        mtime: time,
        ctime: time,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata() {
        let file = Stat { size: 1000, modified: 1_700_000_000_500_000_000, inode: 42, device: 1, ..Default::default() };
        let s = to_linux(&file);
        assert_eq!(s.mode, 0o100644);
        assert_eq!((s.ino, s.dev, s.nlink, s.size, s.blocks), (42, 1, 1, 1000, 2));
        assert_eq!(s.mtime, linux::Timespec { sec: 1_700_000_000, nsec: 500_000_000 });
        assert_eq!(s.uid, 1000);

        let program = Stat { executable: true, read_only: true, ..file };
        assert_eq!(to_linux(&program).mode, 0o100555);
        let dir = Stat { is_dir: true, ..Default::default() };
        assert_eq!(to_linux(&dir).mode, 0o40755);
        assert_eq!(to_linux(&dir).nlink, 2);
        let null = Stat { char_device: true, inode: 3, ..Default::default() };
        assert_eq!(to_linux(&null).mode, 0o20666);
    }
}
