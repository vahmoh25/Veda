//! Memory: `mmap` and friends.
//!
//! Private anonymous memory is the kernel's private memory (`vm_allocate`):
//! its pages are freed as soon as they are unmapped, parts of mappings
//! included, or dropped with `MADV_DONTNEED`. Other mappings get a VMO of
//! their own, which the mapping keeps alive (the handle is closed at once).
//! Either way `munmap` and `mprotect` are plain kernel calls on address
//! ranges. A file mapping is a private copy of the file's bytes: changes
//! are never written back, and shared writable file mappings are refused.

use vabi::{Error, map_flags};
use vrt::object::Vmo;
use vrt::vm;

use crate::error::{self, SysResult};
use crate::fd::{self, Object};
use crate::linux::errno::*;
use crate::linux::mman::*;

const PAGE: usize = vabi::PAGE_SIZE;

/// The kernel's mapping flags for POSIX protections.
fn map_prot(prot: u32) -> usize {
    let mut f = 0;
    if prot & PROT_READ != 0 {
        f |= map_flags::READ;
    }
    // x86 pages that can be written or executed can be read.
    if prot & PROT_WRITE != 0 {
        f |= map_flags::READ | map_flags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        f |= map_flags::READ | map_flags::EXECUTE;
    }
    f
}

pub unsafe fn mmap(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, offset: i64) -> SysResult {
    if len == 0
        || offset < 0
        || !(offset as usize).is_multiple_of(PAGE)
        || prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0
    {
        return Err(EINVAL);
    }
    let len = len.checked_next_multiple_of(PAGE).ok_or(ENOMEM)?;
    let kind = flags & MAP_TYPE;
    if !matches!(kind, MAP_SHARED | MAP_PRIVATE | MAP_SHARED_VALIDATE) {
        return Err(EINVAL);
    }
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
    if fixed && !addr.is_multiple_of(PAGE) {
        return Err(EINVAL);
    }
    let file = if flags & MAP_ANONYMOUS != 0 {
        None
    } else {
        let desc = fd::get(fd)?;
        if !desc.readable() {
            return Err(EACCES);
        }
        if kind != MAP_PRIVATE && prot & PROT_WRITE != 0 {
            return Err(ENODEV);
        }
        Some(desc)
    };
    if let Some(desc) = &file
        && !matches!(desc.object, Object::File(_))
    {
        return Err(ENODEV);
    }

    // A file's bytes go in through a writable mapping, then the
    // protections the program asked for are applied.
    let initial = if file.is_some() { map_flags::READ | map_flags::WRITE } else { map_prot(prot) };
    let mut f = initial;
    if flags & MAP_POPULATE != 0 {
        f |= map_flags::COMMIT;
    }
    if fixed {
        if flags & MAP_FIXED != 0 && flags & MAP_FIXED_NOREPLACE == 0 {
            // MAP_FIXED replaces whatever was there.
            match vm::unmap(None, addr, len) {
                Ok(()) | Err(Error::NotFound) => {}
                Err(e) => return Err(error::kernel(e)),
            }
        }
        f |= map_flags::FIXED;
    }
    let hint = addr & !(PAGE - 1);
    let mapped = if file.is_none() && kind == MAP_PRIVATE {
        vm::allocate(None, len, hint, f)
    } else {
        Vmo::create(len).and_then(|vmo| vm::map(None, &vmo, 0, len, hint, f))
    };
    let at = mapped.map_err(|e| match e {
        Error::AlreadyExists => EEXIST,
        Error::NoMemory | Error::OutOfRange => ENOMEM,
        e => error::kernel(e),
    })?;
    if let Some(desc) = file {
        let Object::File(file) = &desc.object else { unreachable!() };
        // SAFETY: the mapping was just made, `len` bytes, writable.
        let buf = unsafe { core::slice::from_raw_parts_mut(at as *mut u8, len) };
        if let Err(e) = file.pread(buf, offset as u64) {
            let _ = vm::unmap(None, at, len);
            return Err(e);
        }
        if map_prot(prot) != initial
            && let Err(e) = vm::protect(None, at, len, map_prot(prot))
        {
            let _ = vm::unmap(None, at, len);
            return Err(error::kernel(e));
        }
    }
    Ok(at)
}

pub fn munmap(addr: usize, len: usize) -> SysResult {
    if !addr.is_multiple_of(PAGE) || len == 0 {
        return Err(EINVAL);
    }
    match vm::unmap(None, addr, len) {
        // Nothing mapped there is fine.
        Ok(()) | Err(Error::NotFound) => Ok(0),
        Err(e) => Err(error::kernel(e)),
    }
}

pub fn mprotect(addr: usize, len: usize, prot: u32) -> SysResult {
    if !addr.is_multiple_of(PAGE) || prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    vm::protect(None, addr, len, map_prot(prot)).map(|_| 0).map_err(|e| match e {
        Error::NotFound | Error::OutOfRange => ENOMEM,
        Error::AccessDenied => EACCES,
        e => error::kernel(e),
    })
}

/// `madvise`. Dropping pages (`MADV_DONTNEED`, `MADV_FREE`) frees them at
/// once in private anonymous memory, which reads as zeros afterwards; other
/// memory has nowhere to take its contents back from, so the advice is
/// refused there (`EINVAL`, as on Linux where it cannot apply). Other
/// advice is taken as given.
pub fn madvise(addr: usize, len: usize, advice: u32) -> SysResult {
    if !addr.is_multiple_of(PAGE) {
        return Err(EINVAL);
    }
    let len = len.checked_next_multiple_of(PAGE).ok_or(EINVAL)?;
    if len == 0 {
        return Ok(0);
    }
    match advice {
        MADV_DONTNEED | MADV_FREE => vm::decommit(None, addr, len).map(|()| 0).map_err(|e| match e {
            Error::NotFound => ENOMEM,
            Error::NotSupported => EINVAL,
            e => error::kernel(e),
        }),
        MADV_REMOVE => Err(EINVAL),
        _ => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protections() {
        assert_eq!(map_prot(0), 0);
        assert_eq!(map_prot(PROT_READ), map_flags::READ);
        assert_eq!(map_prot(PROT_WRITE), map_flags::READ | map_flags::WRITE);
        assert_eq!(map_prot(PROT_EXEC), map_flags::READ | map_flags::EXECUTE);
    }
}
