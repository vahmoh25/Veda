//! Address-space management for the current (or a child) process.

use vabi::{Error, map_flags, nr};

use crate::object::{Process, Vmo};
use crate::sys::call;

pub use vabi::map_flags::{COMMIT, EXECUTE, FIXED, READ, WRITE};

/// Maps `len` bytes of `vmo` (from `offset`) into `process` (or the caller
/// when `None`). `addr` is a hint (or exact with [`FIXED`]); 0 lets the kernel
/// choose. Returns the mapped address.
pub fn map(process: Option<&Process>, vmo: &Vmo, offset: usize, len: usize, addr: usize, flags: usize) -> Result<usize, Error> {
    let p = process.map(|p| p.raw()).unwrap_or(vabi::INVALID_HANDLE);
    call(nr::VM_MAP, [p as usize, vmo.raw() as usize, offset, len, addr, flags])
}

pub fn unmap(process: Option<&Process>, addr: usize, len: usize) -> Result<(), Error> {
    let p = process.map(|p| p.raw()).unwrap_or(vabi::INVALID_HANDLE);
    call(nr::VM_UNMAP, [p as usize, addr, len, 0, 0, 0]).map(|_| ())
}

pub fn protect(process: Option<&Process>, addr: usize, len: usize, flags: usize) -> Result<(), Error> {
    let p = process.map(|p| p.raw()).unwrap_or(vabi::INVALID_HANDLE);
    call(nr::VM_PROTECT, [p as usize, addr, len, flags, 0, 0]).map(|_| ())
}

/// A VMO mapped into this process for the lifetime of the value.
pub struct Mapping {
    vmo: Vmo,
    addr: usize,
    len: usize,
}

impl Mapping {
    /// Creates an anonymous, read-write mapping of `len` bytes.
    pub fn anonymous(len: usize) -> Result<Mapping, Error> {
        let vmo = Vmo::create(len)?;
        Mapping::new(vmo, len, map_flags::READ | map_flags::WRITE)
    }

    /// Maps the first `len` bytes of `vmo`.
    pub fn new(vmo: Vmo, len: usize, flags: usize) -> Result<Mapping, Error> {
        let addr = map(None, &vmo, 0, len, 0, flags)?;
        Ok(Mapping { vmo, addr, len })
    }

    pub fn addr(&self) -> usize {
        self.addr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn vmo(&self) -> &Vmo {
        &self.vmo
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }

    /// # Safety
    /// The caller must ensure no other code (in this or other processes
    /// sharing the VMO) writes the memory concurrently in a conflicting way.
    pub unsafe fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is valid for `len` bytes while `self` lives.
        unsafe { core::slice::from_raw_parts(self.addr as *const u8, self.len) }
    }

    /// # Safety
    /// See [`Mapping::as_slice`].
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut_slice(&self) -> &mut [u8] {
        // SAFETY: as above.
        unsafe { core::slice::from_raw_parts_mut(self.addr as *mut u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unmap(None, self.addr, self.len);
    }
}
