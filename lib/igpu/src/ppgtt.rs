//! The GPU's address spaces for its engines (per-process GTTs, "PPGTT"):
//! four levels of page tables, as the GPU walks them from Gen8 on. A
//! 48-bit address picks one of 512 entries at each level (its bits 47:39,
//! 38:30, 29:21 and 20:12); each table is a 4 KiB page of 8-byte entries,
//! and the last level's entries map 4 KiB pages.
//!
//! No entry is ever empty: as in i915, what is not mapped reaches a
//! scratch page of the space's own, through scratch tables at each level.
//! The engines read ahead of what they were given (the command streamer
//! past the end of a batch), and a fault there would stop them.
//!
//! The driver keeps a copy of the tree it wrote, to know which tables
//! exist and how many of their entries are in use: a table is freed when
//! its last entry is. The tables themselves are wherever [`TableMemory`]
//! puts them (pages the GPU reaches, or a model's in host tests).
//!
//! What a change means for the GPU's translation caches is the caller's
//! business: unmapped pages may still be in them until the next submission
//! invalidates them, which every submission does first.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// Where page tables live.
pub trait TableMemory {
    /// A zeroed page (for a table, or the scratch page): its physical
    /// address.
    fn alloc(&mut self) -> Option<u64>;
    fn free(&mut self, page: u64);
    /// Writes entry `index` of the table at `table`.
    fn write(&mut self, table: u64, index: usize, entry: u64);
    /// Sets every entry of the table at `table` to `entry`.
    fn fill(&mut self, table: u64, entry: u64) {
        for i in 0..ENTRIES {
            self.write(table, i, entry);
        }
    }
}

pub const ENTRIES: usize = 512;
const PAGE: u64 = 4096;
/// The levels: 4 (the root, PML4) down to 1 (page tables).
const LEVELS: u32 = 4;
/// The bytes of addresses an address space has.
pub const SIZE: u64 = 1 << 48;

/// An entry is in use (`GEN8_PAGE_PRESENT`) ...
pub const PRESENT: u64 = 1 << 0;
/// ... and the GPU may write through it (`GEN8_PAGE_RW`).
pub const WRITABLE: u64 = 1 << 1;
/// The bits of an entry that hold the address of a page or table.
pub const ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// The bits of a page's entry that select its PAT entry (Gen12:
/// `GEN12_PPGTT_PTE_PAT0..2`, bits 3, 4 and 7), for PAT index `index`.
/// Directory entries use index 0 (`PPAT_CACHED_PDE`).
pub const fn pat_bits(index: u8) -> u64 {
    let i = index as u64;
    ((i & 1) << 3) | (((i >> 1) & 1) << 4) | (((i >> 2) & 1) << 7)
}

/// Why a change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No page for a table.
    NoMemory,
    /// Not page-aligned, or past the end of the address space.
    Invalid,
}

struct Table {
    phys: u64,
    /// The tables below; empty at level 1, whose entries map pages.
    below: Vec<Option<Box<Table>>>,
    /// Level 1: which entries map a page.
    present: [u64; ENTRIES / 64],
    /// Entries in use.
    used: u16,
}

impl Table {
    fn has(&self, i: usize) -> bool {
        self.present[i / 64] & (1 << (i % 64)) != 0
    }

    fn free(self, m: &mut impl TableMemory) {
        for t in self.below.into_iter().flatten() {
            t.free(m);
        }
        m.free(self.phys);
    }
}

/// Entry `level`'s index of `address`.
fn index(address: u64, level: u32) -> usize {
    ((address >> (12 + 9 * (level - 1))) & (ENTRIES as u64 - 1)) as usize
}

/// An address space.
pub struct AddressSpace {
    root: Table,
    /// The scratch page, then the scratch tables of levels 1 to 3: what an
    /// entry of level `n` holds when unused is `scratch[n - 1]`'s.
    scratch: [u64; LEVELS as usize],
}

impl AddressSpace {
    pub fn new(m: &mut impl TableMemory) -> Result<AddressSpace, Error> {
        let mut scratch = [0u64; LEVELS as usize];
        for i in 0..scratch.len() {
            let Some(page) = m.alloc() else {
                for &p in &scratch[..i] {
                    m.free(p);
                }
                return Err(Error::NoMemory);
            };
            scratch[i] = page;
            if i > 0 {
                m.fill(page, scratch[i - 1] | PRESENT | WRITABLE);
            }
        }
        let mut space = AddressSpace { root: Table { phys: 0, below: Vec::new(), present: [0; 8], used: 0 }, scratch };
        match space.new_table(m, LEVELS) {
            Ok(root) => {
                space.root = root;
                Ok(space)
            }
            Err(e) => {
                for p in space.scratch {
                    m.free(p);
                }
                Err(e)
            }
        }
    }

    /// The physical address of the root table (what a context's PDP0
    /// registers hold).
    pub fn root(&self) -> u64 {
        self.root.phys
    }

    /// The scratch page, which every address no page is mapped at reaches.
    pub fn scratch_page(&self) -> u64 {
        self.scratch[0]
    }

    /// What an unused entry of a table at `level` holds.
    fn unused(&self, level: u32) -> u64 {
        self.scratch[level as usize - 1] | PRESENT | WRITABLE
    }

    /// A table of `level`, its entries unused.
    fn new_table(&self, m: &mut impl TableMemory, level: u32) -> Result<Table, Error> {
        let phys = m.alloc().ok_or(Error::NoMemory)?;
        m.fill(phys, self.unused(level));
        let below = if level > 1 { (0..ENTRIES).map(|_| None).collect() } else { Vec::new() };
        Ok(Table { phys, below, present: [0; ENTRIES / 64], used: 0 })
    }

    /// Maps `pages` (physical addresses of 4 KiB pages) from `address`, each
    /// entry with `flags` besides its address (PAT bits; present and
    /// writable always). Tables are made as needed; on failure what was
    /// mapped stays mapped (the caller unmaps the range).
    pub fn map(&mut self, m: &mut impl TableMemory, address: u64, pages: &[u64], flags: u64) -> Result<(), Error> {
        let bytes = pages.len() as u64 * PAGE;
        if !address.is_multiple_of(PAGE) || address.checked_add(bytes).is_none_or(|end| end > SIZE) {
            return Err(Error::Invalid);
        }
        for (i, &page) in pages.iter().enumerate() {
            let at = address + i as u64 * PAGE;
            let unused = [self.unused(1), self.unused(2), self.unused(3)];
            let pt = Self::table_for(&mut self.root, m, at, LEVELS, &unused)?;
            let slot = index(at, 1);
            m.write(pt.phys, slot, (page & ADDRESS_MASK) | flags | PRESENT | WRITABLE);
            if !pt.has(slot) {
                pt.present[slot / 64] |= 1 << (slot % 64);
                pt.used += 1;
            }
        }
        Ok(())
    }

    /// The page table (level 1) that maps `address`, made with the tables
    /// above it if missing. `unused[n - 1]` is what a new table of level `n`
    /// is filled with.
    fn table_for<'a>(
        t: &'a mut Table,
        m: &mut impl TableMemory,
        address: u64,
        level: u32,
        unused: &[u64; 3],
    ) -> Result<&'a mut Table, Error> {
        if level == 1 {
            return Ok(t);
        }
        let i = index(address, level);
        if t.below[i].is_none() {
            let phys = m.alloc().ok_or(Error::NoMemory)?;
            m.fill(phys, unused[level as usize - 2]);
            let below = if level > 2 { (0..ENTRIES).map(|_| None).collect() } else { Vec::new() };
            m.write(t.phys, i, phys | PRESENT | WRITABLE);
            t.below[i] = Some(Box::new(Table { phys, below, present: [0; ENTRIES / 64], used: 0 }));
            t.used += 1;
        }
        let next = t.below[i].as_deref_mut().ok_or(Error::Invalid)?;
        Self::table_for(next, m, address, level - 1, unused)
    }

    /// Unmaps `count` pages from `address` (those mapped): they reach the
    /// scratch page again. Tables left empty are freed.
    pub fn unmap(&mut self, m: &mut impl TableMemory, address: u64, count: u64) {
        let unused = [self.unused(1), self.unused(2), self.unused(3), self.unused(4)];
        for i in 0..count {
            let at = address + i * PAGE;
            if at >= SIZE {
                break;
            }
            Self::clear(&mut self.root, m, at, LEVELS, &unused);
        }
    }

    /// Clears `address`'s entry below `t` (at `level`); whether `t` is empty
    /// now.
    fn clear(t: &mut Table, m: &mut impl TableMemory, address: u64, level: u32, unused: &[u64; 4]) -> bool {
        let i = index(address, level);
        if level == 1 {
            if t.has(i) {
                m.write(t.phys, i, unused[0]);
                t.present[i / 64] &= !(1 << (i % 64));
                t.used -= 1;
            }
            return t.used == 0;
        }
        let Some(below) = t.below[i].as_deref_mut() else { return t.used == 0 };
        if Self::clear(below, m, address, level - 1, unused)
            && let Some(empty) = t.below[i].take()
        {
            m.write(t.phys, i, unused[level as usize - 1]);
            empty.free(m);
            t.used -= 1;
        }
        t.used == 0 && level < LEVELS
    }

    /// Frees every table, and the scratch page.
    pub fn destroy(self, m: &mut impl TableMemory) {
        self.root.free(m);
        for p in self.scratch {
            m.free(p);
        }
    }
}
