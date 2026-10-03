//! Builds the initial x86-64 4-level page tables handed to the kernel.
//!
//! While boot services are active the firmware identity-maps all memory, so
//! table pages can be written through their physical addresses.

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const GIB: u64 = 1 << 30;
const MIB2: u64 = 2 << 20;

/// Allocates one zeroed, page-aligned physical page for a page table.
pub trait FrameSource {
    fn alloc_zeroed_page(&mut self) -> Result<u64, &'static str>;
}

pub struct PageTables<'a, F: FrameSource> {
    pub pml4: u64,
    frames: &'a mut F,
    huge_1g: bool,
}

fn table(phys: u64) -> &'static mut [u64; 512] {
    // SAFETY: `phys` is a page-table page we allocated; the firmware's
    // identity map makes it addressable at its physical address.
    unsafe { &mut *(phys as *mut [u64; 512]) }
}

fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * level)) & 0x1FF) as usize
}

impl<'a, F: FrameSource> PageTables<'a, F> {
    pub fn new(frames: &'a mut F, huge_1g: bool) -> Result<Self, &'static str> {
        let pml4 = frames.alloc_zeroed_page()?;
        Ok(PageTables { pml4, frames, huge_1g })
    }

    /// Returns the next-level table referenced by `entry`, creating it.
    fn next(&mut self, table_phys: u64, idx: usize) -> Result<u64, &'static str> {
        let t = table(table_phys);
        if t[idx] & PRESENT == 0 {
            let new = self.frames.alloc_zeroed_page()?;
            // Intermediate entries are permissive; leaf entries restrict.
            t[idx] = new | PRESENT | WRITABLE;
        } else if t[idx] & HUGE != 0 {
            return Err("mapping conflicts with an existing huge page");
        }
        Ok(t[idx] & ADDR_MASK)
    }

    /// Maps one 4 KiB page.
    pub fn map_4k(&mut self, virt: u64, phys: u64, flags: u64) -> Result<(), &'static str> {
        let pdpt = self.next(self.pml4, index(virt, 3))?;
        let pd = self.next(pdpt, index(virt, 2))?;
        let pt = self.next(pd, index(virt, 1))?;
        table(pt)[index(virt, 0)] = (phys & ADDR_MASK) | flags | PRESENT;
        Ok(())
    }

    /// Maps the physical range `[0, limit)` at virtual address `base`, using
    /// the largest pages available.
    pub fn map_linear(&mut self, base: u64, limit: u64, flags: u64) -> Result<(), &'static str> {
        let mut phys = 0;
        while phys < limit {
            let virt = base + phys;
            let pdpt = self.next(self.pml4, index(virt, 3))?;
            if self.huge_1g {
                table(pdpt)[index(virt, 2)] = phys | flags | PRESENT | HUGE;
                phys += GIB;
            } else {
                let pd = self.next(pdpt, index(virt, 2))?;
                for i in 0..512 {
                    table(pd)[i] = (phys + i as u64 * MIB2) | flags | PRESENT | HUGE;
                }
                phys += GIB;
            }
        }
        Ok(())
    }

    /// Makes PML4 slot `dst` share the subtree of slot `src` (used to alias
    /// the identity map and the direct map onto the same lower-level tables).
    pub fn alias_pml4_slot(&mut self, dst: usize, src: usize, extra_flags: u64) {
        let t = table(self.pml4);
        t[dst] = t[src] | extra_flags;
    }

    pub fn pml4_entry(&self, slot: usize) -> u64 {
        table(self.pml4)[slot]
    }

    pub fn set_pml4_entry(&mut self, slot: usize, value: u64) {
        table(self.pml4)[slot] = value;
    }
}
