//! System calls on paths: opening, metadata, directories, names.

use alloc::string::String;

use vproto::fs::{Stat, open_flags};

use crate::error::{self, SysResult};
use crate::fd::{self, Description, Object};
use crate::file::{Directory, VfsFile};
use crate::linux::errno::*;
use crate::linux::{self, Statfs, Timespec, access, at, o};
use crate::path::Resolved;
use crate::stream::Stream;
use crate::{drm, tty, user, vfs};

/// The VFS flags for POSIX open flags.
fn vfs_flags(flags: u32) -> u32 {
    let mut f = match flags & o::ACCMODE {
        o::WRONLY => open_flags::WRITE,
        o::RDWR => open_flags::READ | open_flags::WRITE,
        _ => open_flags::READ,
    };
    if flags & o::CREAT != 0 {
        f |= open_flags::CREATE;
        if flags & o::EXCL != 0 {
            f |= open_flags::EXCLUSIVE;
        }
    }
    if flags & o::TRUNC != 0 && flags & o::ACCMODE != o::RDONLY {
        f |= open_flags::TRUNCATE;
    }
    if flags & o::APPEND != 0 {
        f |= open_flags::APPEND;
    }
    f
}

/// Descriptions for the names of `/dev` that differ between processes:
/// the terminal, the program's own descriptors, and the GPU's render node
/// (a session of its own with the GPU's driver).
fn open_special(path: &str) -> Option<Result<alloc::sync::Arc<Description>, isize>> {
    let fd_alias = match path {
        drm::PATH => {
            return Some(drm::open().map(|d| Description::new(Object::Drm(alloc::boxed::Box::new(d)), o::RDWR)));
        }
        "/dev/stdin" => 0,
        "/dev/stdout" => 1,
        "/dev/stderr" => 2,
        "/dev/tty" => {
            let tty = tty::socket_koid();
            let found = fd::snapshot().into_iter().find_map(|(_, d, _)| match &d.object {
                Object::Stream(s) if Some(s.koid) == tty => Some(s.socket.duplicate(None)),
                _ => None,
            });
            return Some(match found {
                Some(Ok(s)) => Ok(Description::new(Object::Stream(Stream::new(s)), o::RDWR)),
                Some(Err(e)) => Err(error::kernel(e)),
                None => Err(ENXIO),
            });
        }
        p => p.strip_prefix("/dev/fd/")?.parse::<i32>().ok()?,
    };
    Some(fd::get(fd_alias))
}

/// Opens `r` with POSIX `flags`; the description to install.
pub fn open_resolved(r: &Resolved, flags: u32) -> Result<alloc::sync::Arc<Description>, isize> {
    if let Some(special) = open_special(&r.path) {
        return special;
    }
    if flags & o::TMPFILE == o::TMPFILE {
        return Err(EOPNOTSUPP);
    }
    let accmode = flags & o::ACCMODE;
    let wants_dir = flags & o::DIRECTORY != 0 || r.dir_only;
    if !wants_dir || flags & o::CREAT != 0 {
        if r.dir_only && flags & o::CREAT != 0 {
            return Err(EISDIR);
        }
        match error::ipc(vfs::with(|c| c.open_file(r.path.clone(), vfs_flags(flags)))?) {
            Ok((channel, _)) => {
                return Ok(Description::new(Object::File(VfsFile::new(channel)), flags));
            }
            // A directory can be opened for reading (to list it, to stat
            // it, as a base for *at calls).
            Err(EISDIR) if accmode == o::RDONLY && flags & o::CREAT == 0 => {}
            Err(e) => return Err(e),
        }
    }
    let st = vfs::stat(&r.path)?;
    if !st.is_dir {
        return Err(ENOTDIR);
    }
    if accmode != o::RDONLY {
        return Err(EISDIR);
    }
    Ok(Description::new(Object::Dir(Directory::new(r.path.clone())), flags))
}

pub unsafe fn openat(dirfd: i32, name: usize, flags: u32) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?;
    let desc = open_resolved(&r, flags)?;
    fd::insert(desc, flags & o::CLOEXEC != 0, 0).map(|n| n as usize)
}

/// What `fstat` says about a description.
pub fn stat_desc(desc: &Description) -> Result<linux::Stat, isize> {
    let pseudo = |mode: u32, ino: u64| linux::Stat {
        dev: 0,
        ino,
        nlink: 1,
        mode,
        uid: vfs::UID,
        gid: vfs::UID,
        blksize: 4096,
        ..Default::default()
    };
    Ok(match &desc.object {
        Object::File(f) => vfs::to_linux(&f.stat()?),
        Object::Dir(d) => vfs::to_linux(&vfs::stat(&d.path)?),
        Object::Stream(s) if s.is_tty() => pseudo(linux::mode::IFCHR | 0o620, s.koid),
        Object::Stream(s) => pseudo(linux::mode::IFIFO | 0o600, s.koid),
        Object::Null | Object::Log => pseudo(linux::mode::IFCHR | 0o666, 0),
        Object::Drm(_) => linux::Stat { rdev: drm::RDEV, ..pseudo(linux::mode::IFCHR | 0o666, 0) },
    })
}

/// `newfstatat` (and `stat`, `lstat`, `fstat`).
pub unsafe fn fstatat(dirfd: i32, name: usize, buf: usize, flags: u32) -> SysResult {
    // SAFETY: the program passed a string.
    let name = unsafe { user::cstr(name)? };
    let st = if name.is_empty() {
        if flags & at::EMPTY_PATH == 0 {
            return Err(ENOENT);
        }
        if dirfd == at::FDCWD {
            stat_path(&crate::path::resolve(&vfs::cwd(), b".")?)?
        } else {
            stat_desc(&*fd::get(dirfd)?)?
        }
    } else {
        stat_path(&vfs::resolve_at(dirfd, name)?)?
    };
    // SAFETY: the program passed a `struct stat`.
    unsafe { user::write(buf, st)? };
    Ok(0)
}

fn stat_path(r: &Resolved) -> Result<linux::Stat, isize> {
    if let Some(special) = open_special(&r.path) {
        return stat_desc(&*special?);
    }
    Ok(vfs::to_linux(&vfs::stat_resolved(r)?))
}

pub unsafe fn fstat(fd: i32, buf: usize) -> SysResult {
    let st = stat_desc(&*fd::get(fd)?)?;
    // SAFETY: the program passed a `struct stat`.
    unsafe { user::write(buf, st)? };
    Ok(0)
}

pub unsafe fn faccessat(dirfd: i32, name: usize, mode: u32) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?;
    if let Some(special) = open_special(&r.path) {
        return special.map(|_| 0);
    }
    let st = vfs::stat_resolved(&r)?;
    if mode & access::W_OK != 0 && st.read_only {
        return Err(EROFS);
    }
    if mode & access::X_OK != 0 && !st.is_dir && !st.executable {
        return Err(EACCES);
    }
    Ok(0)
}

pub unsafe fn mkdirat(dirfd: i32, name: usize) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?;
    error::ipc(vfs::with(|c| c.mkdir(r.path.clone()))?)?;
    Ok(0)
}

pub unsafe fn unlinkat(dirfd: i32, name: usize, flags: u32) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?;
    let st = vfs::stat(&r.path)?;
    if flags & at::REMOVEDIR != 0 {
        if !st.is_dir {
            return Err(ENOTDIR);
        }
    } else if st.is_dir || r.dir_only {
        return Err(EISDIR);
    }
    error::ipc(vfs::with(|c| c.remove(r.path.clone()))?)?;
    Ok(0)
}

pub unsafe fn renameat(olddir: i32, old: usize, newdir: i32, new: usize, flags: u32) -> SysResult {
    const NOREPLACE: u32 = 1;
    if flags & !NOREPLACE != 0 {
        return Err(EINVAL);
    }
    // SAFETY: the program passed strings.
    let (from, to) =
        unsafe { (vfs::resolve_at(olddir, user::cstr(old)?)?, vfs::resolve_at(newdir, user::cstr(new)?)?) };
    if from.path == "/" || to.path == "/" {
        return Err(EBUSY);
    }
    let (a, b) = (from.path.clone(), to.path.clone());
    if flags & NOREPLACE != 0 {
        error::ipc(vfs::with(|c| c.rename(a, b))?)?;
    } else {
        error::ipc(vfs::with(|c| c.replace(a, b))?)?;
    }
    Ok(0)
}

pub unsafe fn getcwd(buf: usize, size: usize) -> SysResult {
    let cwd = vfs::cwd();
    if size <= cwd.len() {
        return Err(ERANGE);
    }
    // SAFETY: the program passed `size` bytes.
    let out = unsafe { user::slice_mut(buf, size)? };
    out[..cwd.len()].copy_from_slice(cwd.as_bytes());
    out[cwd.len()] = 0;
    Ok(cwd.len() + 1)
}

fn change_dir(path: String) -> SysResult {
    if !vfs::stat(&path)?.is_dir {
        return Err(ENOTDIR);
    }
    vfs::set_cwd(path);
    Ok(0)
}

pub unsafe fn chdir(name: usize) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(at::FDCWD, unsafe { user::cstr(name)? })?;
    change_dir(r.path)
}

pub fn fchdir(fd: i32) -> SysResult {
    let desc = fd::get(fd)?;
    let Object::Dir(d) = &desc.object else { return Err(ENOTDIR) };
    change_dir(d.path.clone())
}

/// There are no symbolic links: every name that exists is not one.
pub unsafe fn readlinkat(dirfd: i32, name: usize) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?;
    vfs::stat(&r.path)?;
    Err(EINVAL)
}

pub unsafe fn truncate(name: usize, len: i64) -> SysResult {
    let len = u64::try_from(len).map_err(|_| EINVAL)?;
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(at::FDCWD, unsafe { user::cstr(name)? })?;
    let (channel, _) = error::ipc(vfs::with(|c| c.open_file(r.path.clone(), open_flags::WRITE))?)?;
    VfsFile::new(channel).truncate(len)?;
    Ok(0)
}

/// Sets modification times (`utimensat`); access times are not kept.
pub unsafe fn utimensat(dirfd: i32, name: usize, times: usize) -> SysResult {
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    // The new modification time (0: now), or `None` to leave it.
    let modified = if times == 0 {
        Some(0)
    } else {
        // SAFETY: the program passed two timespecs.
        let t: [Timespec; 2] = unsafe { user::read(times)? };
        match t[1].nsec {
            UTIME_OMIT => None,
            UTIME_NOW => Some(0),
            _ => Some(t[1].to_ns().ok_or(EINVAL)?.max(1)),
        }
    };
    if name == 0 {
        // futimens: the descriptor itself.
        let desc = fd::get(dirfd)?;
        return match (&desc.object, modified) {
            (Object::File(f), Some(m)) => f.set_modified(m).map(|_| 0),
            (Object::Dir(d), Some(m)) => error::ipc(vfs::with(|c| c.set_modified(d.path.clone(), m))?).map(|_| 0),
            _ => Ok(0),
        };
    }
    // SAFETY: the program passed a string.
    let path = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?.path;
    match modified {
        Some(m) => error::ipc(vfs::with(|c| c.set_modified(path, m))?)?,
        None => drop(vfs::stat(&path)?),
    }
    Ok(0)
}

/// `chmod` and `chown`: Veda keeps no owners or permission bits; a name
/// that exists accepts any.
pub unsafe fn chmodat(dirfd: i32, name: usize) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(dirfd, unsafe { user::cstr(name)? })?;
    let st = vfs::stat(&r.path)?;
    if st.read_only { Err(EROFS) } else { Ok(0) }
}

/// What `statfs` reports for a file system.
fn statfs_of(path: &str) -> Result<Statfs, isize> {
    let space = error::ipc(vfs::with(|c| c.space(path.into()))?)?;
    let st: Stat = vfs::stat(path)?;
    const BLOCK: u64 = 4096;
    let blocks = space.total / BLOCK;
    let free = space.total.saturating_sub(space.used) / BLOCK;
    Ok(Statfs {
        kind: 0x0102_1994, // TMPFS_MAGIC: a memory file system
        bsize: BLOCK as i64,
        blocks,
        bfree: free,
        bavail: free,
        files: 1 << 20,
        ffree: 1 << 20,
        fsid: [st.device as i32, 0],
        namelen: 255,
        frsize: BLOCK as i64,
        flags: if st.read_only { 1 } else { 0 },
        spare: [0; 4],
    })
}

pub unsafe fn statfs(name: usize, buf: usize) -> SysResult {
    // SAFETY: the program passed a string.
    let r = vfs::resolve_at(at::FDCWD, unsafe { user::cstr(name)? })?;
    let s = statfs_of(&r.path)?;
    // SAFETY: the program passed a `struct statfs`.
    unsafe { user::write(buf, s)? };
    Ok(0)
}

pub unsafe fn fstatfs(fd: i32, buf: usize) -> SysResult {
    let desc = fd::get(fd)?;
    let path = match &desc.object {
        Object::Dir(d) => d.path.clone(),
        _ => vfs::cwd(),
    };
    let s = statfs_of(&path)?;
    // SAFETY: the program passed a `struct statfs`.
    unsafe { user::write(buf, s)? };
    Ok(0)
}

pub unsafe fn getdents64(fd: i32, buf: usize, len: usize) -> SysResult {
    let desc = fd::get(fd)?;
    let Object::Dir(d) = &desc.object else { return Err(ENOTDIR) };
    // SAFETY: the program passed `len` bytes.
    d.getdents64(unsafe { user::slice_mut(buf, len)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_flags_for_the_vfs() {
        assert_eq!(vfs_flags(o::RDONLY), open_flags::READ);
        assert_eq!(vfs_flags(o::RDONLY | o::TRUNC), open_flags::READ);
        assert_eq!(
            vfs_flags(o::WRONLY | o::CREAT | o::TRUNC),
            open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE
        );
        assert_eq!(
            vfs_flags(o::RDWR | o::CREAT | o::EXCL),
            open_flags::READ | open_flags::WRITE | open_flags::CREATE | open_flags::EXCLUSIVE
        );
        assert_eq!(vfs_flags(o::WRONLY | o::APPEND), open_flags::WRITE | open_flags::APPEND);
        assert_eq!(vfs_flags(o::RDONLY | o::APPEND), open_flags::READ | open_flags::APPEND);
        // EXCL without CREAT means nothing.
        assert_eq!(vfs_flags(o::RDONLY | o::EXCL), open_flags::READ);
    }
}
