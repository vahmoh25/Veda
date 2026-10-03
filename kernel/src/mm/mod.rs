//! Memory management: physical frames, page tables, the kernel heap, kernel
//! virtual allocations, memory objects (VMOs) and user address spaces.

pub mod aspace;
pub mod heap;
pub mod kvirt;
pub mod paging;
pub mod phys;
pub mod user;
pub mod vmo;

pub const PAGE_SIZE: u64 = 4096;

/// Virtual address of physical address `phys` in the direct map.
#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    bootinfo::HHDM_BASE + phys
}

/// Inverse of [`phys_to_virt`] for direct-map addresses.
#[inline]
pub fn virt_to_phys(virt: u64) -> u64 {
    debug_assert!(virt >= bootinfo::HHDM_BASE);
    virt - bootinfo::HHDM_BASE
}

#[inline]
pub const fn page_align_down(x: u64) -> u64 {
    x & !(PAGE_SIZE - 1)
}

#[inline]
pub const fn page_align_up(x: u64) -> u64 {
    (x + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}
