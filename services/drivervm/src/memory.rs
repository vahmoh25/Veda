//! The guest's memory, as the monitor reaches it.
//!
//! The guest's RAM is one VMO, mapped here as it is mapped at
//! guest-physical address 0. The guest (another processor of it) may write
//! any of it at any moment: what the monitor reads from it, it copies out
//! first and checks after, and it keeps no reference into it.

use vrt::vm::Mapping;

/// Plain data: any bytes are a valid value (requests of the bridge).
///
/// # Safety
/// Implementors are `repr(C)` structures of integers, without padding.
pub unsafe trait Plain: Copy + Default {}

// SAFETY: integers and structures of them, without padding (the bridge's
// layouts are checked by size in `vhv::bridge`).
unsafe impl Plain for u32 {}
// SAFETY: as above.
unsafe impl Plain for u64 {}
// SAFETY: as above.
unsafe impl Plain for vabi::WaitItem {}
macro_rules! plain {
    ($($t:ty),*) => {$(
        // SAFETY: a request of the bridge's or the platform's: integers,
        // no padding.
        unsafe impl Plain for $t {}
    )*};
}
plain!(
    vhv::bridge::Setup,
    vhv::bridge::Close,
    vhv::bridge::Duplicate,
    vhv::bridge::ObjectInfo,
    vhv::bridge::Signal,
    vhv::bridge::Wait,
    vhv::bridge::Cancel,
    vhv::bridge::ChannelCreate,
    vhv::bridge::ChannelWrite,
    vhv::bridge::ChannelRead,
    vhv::bridge::EventCreate,
    vhv::bridge::Vmo,
    vhv::bridge::VmoCopy,
    vhv::bridge::VmoMap,
    vhv::bridge::Bootstrap,
    vhv::bridge::Clock,
    vhv::bridge::Completion,
    vhv::platform::Variable
);

pub struct GuestMemory {
    map: Mapping,
}

impl GuestMemory {
    pub fn new(map: Mapping) -> GuestMemory {
        GuestMemory { map }
    }

    pub fn vmo(&self) -> &vrt::object::Vmo {
        self.map.vmo()
    }

    pub fn size(&self) -> u64 {
        self.map.len() as u64
    }

    /// Whether `[gpa, gpa+len)` is RAM.
    pub fn contains(&self, gpa: u64, len: u64) -> bool {
        gpa.checked_add(len).is_some_and(|end| end <= self.size())
    }

    /// Copies guest memory at `gpa` into `out` (`false`: not RAM).
    pub fn read(&self, gpa: u64, out: &mut [u8]) -> bool {
        if !self.contains(gpa, out.len() as u64) {
            return false;
        }
        // SAFETY: inside the mapping (checked); a copy, so what the guest
        // writes meanwhile changes only what was read.
        unsafe { core::ptr::copy_nonoverlapping(self.map.as_ptr().add(gpa as usize), out.as_mut_ptr(), out.len()) };
        true
    }

    /// Copies `data` into guest memory at `gpa` (`false`: not RAM).
    pub fn write(&self, gpa: u64, data: &[u8]) -> bool {
        if !self.contains(gpa, data.len() as u64) {
            return false;
        }
        // SAFETY: as in `read`.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.map.as_ptr().add(gpa as usize), data.len()) };
        true
    }

    /// Reads a value of plain data at `gpa`.
    pub fn read_obj<T: Plain>(&self, gpa: u64) -> Option<T> {
        let mut v = T::default();
        // SAFETY: `T` is plain data: its bytes may be anything.
        let bytes = unsafe { core::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, core::mem::size_of::<T>()) };
        self.read(gpa, bytes).then_some(v)
    }

    /// Writes a value of plain data at `gpa`.
    pub fn write_obj<T: Plain>(&self, gpa: u64, v: &T) -> bool {
        // SAFETY: viewing plain data as bytes.
        let bytes = unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) };
        self.write(gpa, bytes)
    }
}
