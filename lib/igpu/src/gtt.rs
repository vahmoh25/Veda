//! The GPU's global address table (the GGTT): what the display engine
//! scans out is at addresses in it, each 4 KiB page mapped by an 8-byte
//! entry. The table is in the upper half of the GPU's first BAR.
//!
//! The firmware maps its own framebuffer (and perhaps more) at the bottom
//! of the table. The driver maps its pictures higher up, in entries it has
//! found unused, and never touches the firmware's.

use crate::Mmio;
use crate::regs;

const PAGE: u64 = 4096;

/// Where the driver looks for room first: far above where firmware maps
/// its framebuffer.
pub const SEARCH_FROM: u64 = 512 << 20;

/// The first range in `busy` (start, bytes) that `bytes` at `at` overlaps.
fn clash(busy: &[(u64, u64)], at: u64, bytes: u64) -> Option<(u64, u64)> {
    busy.iter().find(|&&(b, n)| b < at + bytes && at < b + n).copied()
}

/// The table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gtt {
    /// Where the entries start in the BAR.
    pub base: u32,
    /// How many there are (each maps a page).
    pub entries: u64,
}

impl Gtt {
    /// The table of a GPU whose first BAR is `bar_size` bytes, with
    /// `gmch_ctl` in its configuration space (`SNB_GMCH_CTRL`). `None` if
    /// they do not describe a table that fits in the BAR's upper half.
    pub fn new(bar_size: u64, gmch_ctl: u16) -> Option<Gtt> {
        let bytes = regs::gmch_table_mib(gmch_ctl) << 20;
        if bytes == 0 || bar_size < 2 * bytes || bar_size > 1 << 31 {
            return None;
        }
        Some(Gtt { base: (bar_size / 2) as u32, entries: bytes / 8 })
    }

    /// The size of the GPU's address space the table maps.
    pub fn span(&self) -> u64 {
        self.entries * PAGE
    }

    fn entry(&self, address: u64) -> u32 {
        self.base + (address / PAGE * 8) as u32
    }

    /// The physical address of the page at `address`, if it is mapped.
    pub fn translate(&self, mmio: &impl Mmio, address: u64) -> Option<u64> {
        if address >= self.span() {
            return None;
        }
        let pte = mmio.read64(self.entry(address));
        (pte & regs::PTE_PRESENT != 0).then_some((pte & regs::PTE_ADDR_MASK) + address % PAGE)
    }

    /// Room for `bytes` at a multiple of `align` at or above `from`, below
    /// 4 GiB (planes take 32-bit addresses), where no entry is in use: its
    /// address.
    pub fn find_free(&self, mmio: &impl Mmio, from: u64, bytes: u64, align: u64) -> Option<u64> {
        let pages = bytes.div_ceil(PAGE);
        let limit = self.limit();
        let mut start = from.next_multiple_of(align);
        'search: while start + pages * PAGE <= limit {
            for page in 0..pages {
                let address = start + page * PAGE;
                if mmio.read64(self.entry(address)) & regs::PTE_PRESENT != 0 {
                    // In use: try beyond it.
                    start = (address + PAGE).next_multiple_of(align);
                    continue 'search;
                }
            }
            return Some(start);
        }
        None
    }

    /// Room for `bytes` at a multiple of `align` at or above `from` and
    /// below 4 GiB that nothing in `busy` (ranges the display engine scans
    /// out, or that are set aside: start, bytes) overlaps. Where no entry
    /// is in use if there is such room; otherwise the first clear of
    /// `busy`, whatever its entries map (in a table the firmware filled
    /// with entries for a scratch page, or never cleared). Its address, and
    /// whether its entries were unused.
    pub fn find_room(
        &self,
        mmio: &impl Mmio,
        from: u64,
        bytes: u64,
        align: u64,
        busy: &[(u64, u64)],
    ) -> Option<(u64, bool)> {
        let mut start = from;
        while let Some(at) = self.find_free(mmio, start, bytes, align) {
            match clash(busy, at, bytes) {
                Some((b, n)) => start = b + n,
                None => return Some((at, true)),
            }
        }
        self.find_clear(from, bytes, align, busy).map(|at| (at, false))
    }

    /// Room for `bytes` at a multiple of `align` at or above `from` and
    /// below 4 GiB that nothing in `busy` overlaps, whatever its entries
    /// map (none of them is read).
    pub fn find_clear(&self, from: u64, bytes: u64, align: u64, busy: &[(u64, u64)]) -> Option<u64> {
        let mut start = from.next_multiple_of(align);
        while start + bytes <= self.limit() {
            match clash(busy, start, bytes) {
                Some((b, n)) => start = (b + n).next_multiple_of(align),
                None => return Some(start),
            }
        }
        None
    }

    /// Where planes can reach: the table's span, at most 4 GiB.
    fn limit(&self) -> u64 {
        self.span().min(1 << 32)
    }

    /// Maps `bytes` of physically contiguous memory at `phys` to `address`,
    /// then makes the GPU see the change.
    pub fn map(&self, mmio: &impl Mmio, address: u64, phys: u64, bytes: u64) {
        for page in 0..bytes.div_ceil(PAGE) {
            let pte = ((phys + page * PAGE) & regs::PTE_ADDR_MASK) | regs::PTE_PRESENT;
            mmio.write64(self.entry(address + page * PAGE), pte);
        }
        self.flush(mmio);
    }

    /// Unmaps `bytes` at `address`.
    pub fn unmap(&self, mmio: &impl Mmio, address: u64, bytes: u64) {
        for page in 0..bytes.div_ceil(PAGE) {
            mmio.write64(self.entry(address + page * PAGE), 0);
        }
        self.flush(mmio);
    }

    /// The writes to the table reach it (a read of the last one), then the
    /// GPU drops what it cached of it.
    fn flush(&self, mmio: &impl Mmio) {
        let _ = mmio.read64(self.base);
        mmio.write(regs::GFX_FLUSH_CNTL, regs::GFX_FLUSH_CNTL_EN);
    }
}
