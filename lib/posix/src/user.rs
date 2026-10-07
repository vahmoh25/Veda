//! Arguments passed by pointer.
//!
//! The C library and the POSIX layer share an address space: a system call
//! argument that points at memory is used in place. A null pointer is
//! `EFAULT`; any other bad pointer faults as the program's own access
//! would (system calls on Linux return `EFAULT` there, which no correct
//! program relies on).

use alloc::vec::Vec;

use crate::linux::errno::{EFAULT, ENAMETOOLONG};
use crate::path::PATH_MAX;

/// A NUL-terminated string (at most `PATH_MAX` bytes before the NUL).
///
/// # Safety
/// `p` must be null or point at a NUL-terminated string that outlives the
/// call.
pub unsafe fn cstr<'a>(p: usize) -> Result<&'a [u8], isize> {
    if p == 0 {
        return Err(EFAULT);
    }
    let p = p as *const u8;
    let mut len = 0;
    // SAFETY: the caller promises a NUL-terminated string.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
        if len >= PATH_MAX {
            return Err(ENAMETOOLONG);
        }
    }
    // SAFETY: `len` bytes were just read.
    Ok(unsafe { core::slice::from_raw_parts(p, len) })
}

/// A NULL-terminated array of strings of any length (`argv`, `envp`).
///
/// # Safety
/// `p` must be null or point at a NULL-terminated array of NUL-terminated
/// strings that outlive the call.
pub unsafe fn cstr_array<'a>(p: usize) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    if p == 0 {
        return out;
    }
    let p = p as *const *const u8;
    let mut i = 0;
    loop {
        // SAFETY: the caller promises a NULL-terminated array.
        let s = unsafe { *p.add(i) };
        if s.is_null() {
            return out;
        }
        let mut len = 0;
        // SAFETY: each element is a NUL-terminated string.
        while unsafe { *s.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` bytes were just read.
        out.push(unsafe { core::slice::from_raw_parts(s, len) });
        i += 1;
    }
}

/// Reads a value the program passed.
///
/// # Safety
/// `p` must be null or point at a readable `T`.
pub unsafe fn read<T: Copy>(p: usize) -> Result<T, isize> {
    if p == 0 {
        return Err(EFAULT);
    }
    // SAFETY: per the caller; unaligned reads are fine.
    Ok(unsafe { core::ptr::read_unaligned(p as *const T) })
}

/// Stores a value for the program.
///
/// # Safety
/// `p` must be null or point at a writable `T`.
pub unsafe fn write<T: Copy>(p: usize, v: T) -> Result<(), isize> {
    if p == 0 {
        return Err(EFAULT);
    }
    // SAFETY: per the caller.
    unsafe { core::ptr::write_unaligned(p as *mut T, v) };
    Ok(())
}

/// A buffer the program passed.
///
/// # Safety
/// `p` must point at `len` readable bytes (or `len` must be 0).
pub unsafe fn slice<'a>(p: usize, len: usize) -> Result<&'a [u8], isize> {
    if len == 0 {
        return Ok(&[]);
    }
    if p == 0 {
        return Err(EFAULT);
    }
    // SAFETY: per the caller.
    Ok(unsafe { core::slice::from_raw_parts(p as *const u8, len) })
}

/// A buffer the program passed for filling.
///
/// # Safety
/// `p` must point at `len` writable bytes (or `len` must be 0) not
/// otherwise referenced during the call.
pub unsafe fn slice_mut<'a>(p: usize, len: usize) -> Result<&'a mut [u8], isize> {
    if len == 0 {
        return Ok(&mut []);
    }
    if p == 0 {
        return Err(EFAULT);
    }
    // SAFETY: per the caller.
    Ok(unsafe { core::slice::from_raw_parts_mut(p as *mut u8, len) })
}
