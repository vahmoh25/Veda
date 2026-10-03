//! Copying data between the kernel and the current user address space.
//!
//! User memory can be changed concurrently by other threads of the process
//! running on other CPUs, so the kernel never keeps references into it: data
//! is validated (and faulted in) and then copied. Because mappings can only
//! change under the BKL, which the caller holds, a validated range cannot
//! disappear during the copy.

use alloc::vec::Vec;

use vabi::Error;

use crate::arch::cpu::{user_access_begin, user_access_end};
use crate::sched;

fn validate(addr: u64, len: usize, write: bool) -> Result<(), Error> {
    if len == 0 {
        return Ok(());
    }
    let thread = sched::current();
    let aspace = thread.aspace.as_ref().ok_or(Error::Fault)?;
    if aspace.ensure_range(addr, len as u64, write) { Ok(()) } else { Err(Error::Fault) }
}

/// Copies `out.len()` bytes from user address `src`.
pub fn copy_from_user(out: &mut [u8], src: u64) -> Result<(), Error> {
    validate(src, out.len(), false)?;
    user_access_begin();
    // SAFETY: the range was validated and is mapped readable.
    unsafe { core::ptr::copy_nonoverlapping(src as *const u8, out.as_mut_ptr(), out.len()) };
    user_access_end();
    Ok(())
}

/// Copies `data` to user address `dst`.
pub fn copy_to_user(dst: u64, data: &[u8]) -> Result<(), Error> {
    validate(dst, data.len(), true)?;
    user_access_begin();
    // SAFETY: the range was validated and is mapped writable.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len()) };
    user_access_end();
    Ok(())
}

/// Reads a plain-data value from user memory.
pub fn read<T: Copy>(src: u64) -> Result<T, Error> {
    let mut v = core::mem::MaybeUninit::<T>::uninit();
    // SAFETY: we fill every byte of `v` before assuming it initialised; the
    // ABI types read this way are valid for any bit pattern.
    let bytes = unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, core::mem::size_of::<T>()) };
    copy_from_user(bytes, src)?;
    // SAFETY: fully initialised above.
    Ok(unsafe { v.assume_init() })
}

/// Writes a plain-data value to user memory.
pub fn write<T: Copy>(dst: u64, value: &T) -> Result<(), Error> {
    // SAFETY: viewing a Copy value's bytes.
    let bytes = unsafe { core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>()) };
    copy_to_user(dst, bytes)
}

/// Copies a user buffer into a new kernel vector (bounded by `max`).
pub fn read_vec(src: u64, len: usize, max: usize) -> Result<Vec<u8>, Error> {
    if len > max {
        return Err(Error::InvalidArgs);
    }
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
    v.resize(len, 0);
    copy_from_user(&mut v, src)?;
    Ok(v)
}

/// Reads an array of `u32` values (e.g. handle lists).
pub fn read_u32s(src: u64, count: usize, max: usize) -> Result<Vec<u32>, Error> {
    let bytes = read_vec(src, count.checked_mul(4).ok_or(Error::InvalidArgs)?, max * 4)?;
    Ok(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}
