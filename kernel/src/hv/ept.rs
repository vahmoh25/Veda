//! Extended page tables: a guest's physical address space.
//!
//! Four levels of tables, as the processor's own paging but with the
//! permissions (read, write, execute) and the memory type in each leaf.
//! Leaves map 4 KiB pages. A table is freed with the address space.

use alloc::vec::Vec;

use crate::mm::paging::Cache;
use crate::mm::{PAGE_SIZE, phys, phys_to_virt};

const READ: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const EXECUTE: u64 = 1 << 2;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Memory types of leaves (bits 3-5).
const TYPE_UC: u64 = 0;
const TYPE_WC: u64 = 1 << 3;
const TYPE_WB: u64 = 6 << 3;

/// Guest-physical addresses are below this (four-level tables).
pub const LIMIT: u64 = 1 << 48;

/// What a guest may do with a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

pub struct Ept {
    root: u64,
}

fn table(frame: u64) -> &'static mut [u64; 512] {
    // SAFETY: table frames are owned by their `Ept` and direct-mapped.
    unsafe { &mut *(phys_to_virt(frame) as *mut [u64; 512]) }
}

fn index(gpa: u64, level: u32) -> usize {
    ((gpa >> (12 + 9 * level)) & 0x1FF) as usize
}

impl Ept {
    pub fn new() -> Option<Ept> {
        Some(Ept { root: phys::alloc_zeroed()? })
    }

    /// The EPT pointer: the root, write-back tables, a four-level walk.
    pub fn pointer(&self) -> u64 {
        self.root | (6 | (3 << 3))
    }

    /// Maps the 4 KiB page at `gpa` to `frame`. The memory type follows
    /// the host's mapping of the memory (`cache`); the guest's own page
    /// attributes combine with it, as with the processor's MTRRs.
    pub fn map(&mut self, gpa: u64, frame: u64, access: Access, cache: Cache) -> Result<(), ()> {
        let mut t = self.root;
        for level in (1..=3).rev() {
            let e = &mut table(t)[index(gpa, level)];
            if *e & (READ | WRITE | EXECUTE) == 0 {
                let next = phys::alloc_zeroed().ok_or(())?;
                *e = next | READ | WRITE | EXECUTE;
            }
            t = *e & ADDR_MASK;
        }
        let memory_type = match cache {
            Cache::WriteBack => TYPE_WB,
            Cache::WriteCombining => TYPE_WC,
            Cache::Uncached => TYPE_UC,
        };
        let mut bits = memory_type;
        if access.read {
            bits |= READ;
        }
        if access.write {
            bits |= WRITE;
        }
        if access.execute {
            bits |= EXECUTE;
        }
        table(t)[index(gpa, 0)] = (frame & ADDR_MASK) | bits;
        Ok(())
    }

    /// Removes the mapping of the page at `gpa`, if any. The caller makes
    /// every processor forget it before the page is reused.
    pub fn unmap(&mut self, gpa: u64) {
        let mut t = self.root;
        for level in (1..=3).rev() {
            let e = table(t)[index(gpa, level)];
            if e & (READ | WRITE | EXECUTE) == 0 {
                return;
            }
            t = e & ADDR_MASK;
        }
        table(t)[index(gpa, 0)] = 0;
    }

    /// The frame `gpa` is mapped to.
    #[allow(dead_code)]
    pub fn translate(&self, gpa: u64) -> Option<u64> {
        let mut t = self.root;
        for level in (0..=3).rev() {
            let e = table(t)[index(gpa, level)];
            if e & (READ | WRITE | EXECUTE) == 0 {
                return None;
            }
            t = e & ADDR_MASK;
        }
        Some(t + (gpa & (PAGE_SIZE - 1)))
    }
}

impl Drop for Ept {
    fn drop(&mut self) {
        // Every table, leaves' frames excepted (they belong to VMOs).
        let mut tables: Vec<(u64, u32)> = alloc::vec![(self.root, 3)];
        while let Some((t, level)) = tables.pop() {
            if level > 0 {
                for &e in table(t).iter() {
                    if e & (READ | WRITE | EXECUTE) != 0 {
                        tables.push((e & ADDR_MASK, level - 1));
                    }
                }
            }
            phys::free(t);
        }
    }
}
