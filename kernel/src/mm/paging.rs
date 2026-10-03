//! x86-64 four-level page tables.
//!
//! The kernel half of every address space (PML4 slots 256..512) is shared:
//! all 256 kernel PDPTs are allocated at boot, and new address spaces copy
//! those 256 entries, so later kernel mappings are visible everywhere.

use super::{PAGE_SIZE, phys, phys_to_virt};
use crate::sync::Once;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const PWT: u64 = 1 << 3;
pub const PCD: u64 = 1 << 4;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Memory type for a mapping (see the PAT layout in `arch::cpu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    WriteBack,
    WriteCombining,
    Uncached,
}

impl Cache {
    pub fn pte_bits(self) -> u64 {
        match self {
            Cache::WriteBack => 0,
            Cache::WriteCombining => PWT,
            Cache::Uncached => PCD | PWT,
        }
    }
}

/// Leaf mapping attributes.
#[derive(Debug, Clone, Copy)]
pub struct Flags {
    pub writable: bool,
    pub executable: bool,
    pub user: bool,
    pub global: bool,
    pub cache: Cache,
}

impl Flags {
    pub const KERNEL_RW: Flags =
        Flags { writable: true, executable: false, user: false, global: true, cache: Cache::WriteBack };

    pub fn bits(self) -> u64 {
        let mut b = PRESENT | self.cache.pte_bits();
        if self.writable {
            b |= WRITABLE;
        }
        if !self.executable {
            b |= NO_EXECUTE;
        }
        if self.user {
            b |= USER;
        }
        if self.global {
            b |= GLOBAL;
        }
        b
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    NoMemory,
    AlreadyMapped,
}

fn table(phys: u64) -> &'static mut [u64; 512] {
    // SAFETY: page-table pages are direct-mapped and owned by the paging code.
    unsafe { &mut *(phys_to_virt(phys) as *mut [u64; 512]) }
}

#[inline]
fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * level)) & 0x1FF) as usize
}

/// Returns the next-level table for `entry`, allocating it if needed.
fn next_table(t: &mut [u64; 512], idx: usize, user: bool) -> Result<u64, MapError> {
    if t[idx] & PRESENT == 0 {
        let frame = phys::alloc_zeroed().ok_or(MapError::NoMemory)?;
        t[idx] = frame | PRESENT | WRITABLE | if user { USER } else { 0 };
    }
    debug_assert!(t[idx] & HUGE == 0, "walking through a huge page");
    Ok(t[idx] & ADDR_MASK)
}

/// Maps one 4 KiB page in the hierarchy rooted at `pml4`.
pub fn map(pml4: u64, virt: u64, phys_addr: u64, flags: Flags, overwrite: bool) -> Result<(), MapError> {
    let user = flags.user;
    let pdpt = next_table(table(pml4), index(virt, 3), user)?;
    let pd = next_table(table(pdpt), index(virt, 2), user)?;
    let pt = next_table(table(pd), index(virt, 1), user)?;
    let e = &mut table(pt)[index(virt, 0)];
    if *e & PRESENT != 0 && !overwrite {
        return Err(MapError::AlreadyMapped);
    }
    *e = (phys_addr & ADDR_MASK) | flags.bits();
    Ok(())
}

/// Removes the 4 KiB mapping at `virt`, returning the frame it mapped. The
/// caller is responsible for TLB invalidation.
pub fn unmap(pml4: u64, virt: u64) -> Option<u64> {
    let mut t = table(pml4);
    for level in (1..=3).rev() {
        let e = t[index(virt, level)];
        if e & PRESENT == 0 || e & HUGE != 0 {
            return None;
        }
        t = table(e & ADDR_MASK);
    }
    let e = &mut t[index(virt, 0)];
    if *e & PRESENT == 0 {
        return None;
    }
    let frame = *e & ADDR_MASK;
    *e = 0;
    Some(frame)
}

/// Translates `virt`, returning the physical address and raw leaf entry.
pub fn translate(pml4: u64, virt: u64) -> Option<(u64, u64)> {
    let mut t = table(pml4);
    for level in (1..=3).rev() {
        let e = t[index(virt, level)];
        if e & PRESENT == 0 {
            return None;
        }
        if e & HUGE != 0 {
            let size = 1u64 << (12 + 9 * level);
            return Some(((e & ADDR_MASK & !(size - 1)) + (virt & (size - 1)), e));
        }
        t = table(e & ADDR_MASK);
    }
    let e = t[index(virt, 0)];
    if e & PRESENT == 0 { None } else { Some(((e & ADDR_MASK) + (virt & 0xFFF), e)) }
}

/// Changes the flags of an existing 4 KiB mapping.
pub fn protect(pml4: u64, virt: u64, flags: Flags) -> bool {
    let mut t = table(pml4);
    for level in (1..=3).rev() {
        let e = t[index(virt, level)];
        if e & PRESENT == 0 || e & HUGE != 0 {
            return false;
        }
        t = table(e & ADDR_MASK);
    }
    let e = &mut t[index(virt, 0)];
    if *e & PRESENT == 0 {
        return false;
    }
    *e = (*e & ADDR_MASK) | flags.bits();
    true
}

/// The kernel's own top-level table (used by idle and kernel threads).
static KERNEL_PML4: Once<u64> = Once::new();

pub fn kernel_pml4() -> u64 {
    *KERNEL_PML4.expect()
}

/// Builds the kernel address space: direct map of physical memory (huge
/// pages), the kernel image with per-section permissions, and empty PDPTs for
/// every other kernel slot. Then switches to it.
pub fn init_kernel_space(boot: &bootinfo::BootInfo) {
    let pml4 = phys::alloc_zeroed().expect("out of memory for the kernel PML4");
    let pml4_t = table(pml4);
    for slot in 256..512 {
        let pdpt = phys::alloc_zeroed().expect("out of memory for kernel PDPTs");
        pml4_t[slot] = pdpt | PRESENT | WRITABLE;
    }

    // Direct map with 1 GiB pages when available, otherwise 2 MiB pages.
    let huge_1g = crate::arch::cpu::features().page_1g;
    let base = bootinfo::HHDM_BASE;
    let mut addr = 0u64;
    while addr < boot.hhdm_limit {
        let virt = base + addr;
        let pdpt = table(pml4_t[index(virt, 3)] & ADDR_MASK);
        let flags = PRESENT | WRITABLE | GLOBAL | NO_EXECUTE;
        if huge_1g {
            pdpt[index(virt, 2)] = addr | flags | HUGE;
            addr += 1 << 30;
        } else {
            let pd = next_table(pdpt, index(virt, 2), false).expect("out of memory for the direct map");
            for i in 0..512u64 {
                table(pd)[i as usize] = (addr + i * (2 << 20)) | flags | HUGE;
            }
            addr += 1 << 30;
        }
    }

    // Kernel image: walk our own PE section table (the headers are mapped at
    // the image base).
    let k = &boot.kernel;
    // SAFETY: the loader mapped the whole image, headers included.
    let image = unsafe { core::slice::from_raw_parts(k.virt_base as *const u8, k.size as usize) };
    let pe = vpe::PeImage::parse(image).expect("kernel image headers are corrupt");
    let mut off = 0;
    while off < k.size {
        let rva = off as u32;
        let mut flags = Flags { writable: false, executable: false, user: false, global: true, cache: Cache::WriteBack };
        for s in pe.sections() {
            let end = s.virtual_address + s.virtual_size.next_multiple_of(PAGE_SIZE as u32);
            if rva >= s.virtual_address && rva < end {
                flags.writable |= s.writable();
                flags.executable |= s.executable();
            }
        }
        map(pml4, k.virt_base + off, k.phys_base + off, flags, true).expect("mapping the kernel image");
        off += PAGE_SIZE;
    }

    KERNEL_PML4.set(pml4);
    // SAFETY: the new tables map the kernel image, the direct map (which
    // holds the current stack) and everything the kernel references.
    unsafe { crate::arch::cpu::write_cr3(pml4) };
}

/// Creates a new top-level table sharing the kernel half.
pub fn new_user_pml4() -> Option<u64> {
    let pml4 = phys::alloc_zeroed()?;
    let src = table(kernel_pml4());
    let dst = table(pml4);
    dst[256..512].copy_from_slice(&src[256..512]);
    Some(pml4)
}

/// Frees all page-table pages of the user half (not the mapped frames) and
/// the PML4 itself.
pub fn free_user_pml4(pml4: u64) {
    fn free_level(t: u64, level: u32) {
        if level > 0 {
            for &e in table(t).iter() {
                if e & PRESENT != 0 && e & HUGE == 0 {
                    free_level(e & ADDR_MASK, level - 1);
                }
            }
        }
        phys::free(t);
    }
    for slot in 0..256 {
        let e = table(pml4)[slot];
        if e & PRESENT != 0 {
            free_level(e & ADDR_MASK, 2);
        }
    }
    phys::free(pml4);
}
