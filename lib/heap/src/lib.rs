//! A TLSF (two-level segregated fit) allocator.
//!
//! TLSF gives O(1) allocation and deallocation with low fragmentation:
//! free blocks are kept in 2D-indexed segregated lists (first level = power
//! of two, second level = 32 linear subdivisions), with bitmaps to find a
//! suitable non-empty list in constant time. Adjacent free blocks are merged
//! immediately using boundary information.
//!
//! The allocator manages one contiguous region that can grow at its end
//! (`Tlsf::grow`); it is not thread-safe by itself (wrap it in a lock).
//!
//! Block layout (all blocks 16-byte aligned):
//!
//! ```text
//! +-----------------+------------------+---------------------------------+
//! | prev_phys (8 B) | size|flags (8 B) | payload (size bytes)            |
//! +-----------------+------------------+---------------------------------+
//!                                       free blocks keep their free-list
//!                                       links in the first 16 payload bytes
//! ```
//!
//! The region ends with a zero-size "sentinel" block that is always in use,
//! so the last real block never needs a bounds check when merging.

#![no_std]

#[cfg(test)]
extern crate std;

use core::ptr::null_mut;

const ALIGN: usize = 16;
const HEADER: usize = 16;
const MIN_PAYLOAD: usize = 16;
const SL_BITS: u32 = 5;
const SL_COUNT: usize = 1 << SL_BITS;
const FL_SHIFT: u32 = SL_BITS + 4; // sizes below 2^9 use first level 0, linearly
const FL_COUNT: usize = 40;
const SMALL_LIMIT: usize = 1 << FL_SHIFT;

const FREE: usize = 1;
const SIZE_MASK: usize = !(ALIGN - 1);

#[repr(C)]
struct Block {
    prev_phys: *mut Block,
    size_flags: usize,
    // Valid only while free:
    next_free: *mut Block,
    prev_free: *mut Block,
}

impl Block {
    #[inline]
    fn size(&self) -> usize {
        self.size_flags & SIZE_MASK
    }

    #[inline]
    fn is_free(&self) -> bool {
        self.size_flags & FREE != 0
    }

    #[inline]
    unsafe fn payload(b: *mut Block) -> *mut u8 {
        // SAFETY: the payload follows the header.
        unsafe { (b as *mut u8).add(HEADER) }
    }

    #[inline]
    unsafe fn from_payload(p: *mut u8) -> *mut Block {
        // SAFETY: inverse of `payload`.
        unsafe { p.sub(HEADER) as *mut Block }
    }

    #[inline]
    unsafe fn next_phys(b: *mut Block) -> *mut Block {
        // SAFETY: blocks tile the region; the sentinel terminates it.
        unsafe { (b as *mut u8).add(HEADER + (*b).size()) as *mut Block }
    }
}

/// Maps a size to its (first level, second level) list.
#[inline]
fn mapping(size: usize) -> (usize, usize) {
    if size < SMALL_LIMIT {
        (0, size / (SMALL_LIMIT / SL_COUNT))
    } else {
        let fl = usize::BITS - 1 - size.leading_zeros();
        let sl = (size >> (fl - SL_BITS)) & (SL_COUNT - 1);
        ((fl - FL_SHIFT + 1) as usize, sl)
    }
}

/// Rounds `size` up so that any block in the selected list is big enough.
#[inline]
fn round_for_search(size: usize) -> usize {
    if size >= SMALL_LIMIT {
        let fl = usize::BITS - 1 - size.leading_zeros();
        let round = (1usize << (fl - SL_BITS)) - 1;
        size + round
    } else {
        size
    }
}

pub struct Tlsf {
    fl_bitmap: u64,
    sl_bitmap: [u32; FL_COUNT],
    lists: [[*mut Block; SL_COUNT]; FL_COUNT],
    /// The sentinel block at the end of the managed region (null if empty).
    sentinel: *mut Block,
    /// Bytes handed out (payload sizes of used blocks).
    used: usize,
    /// Total bytes of region managed.
    total: usize,
}

// SAFETY: the raw pointers refer to memory owned by the allocator; callers
// serialise access with a lock.
unsafe impl Send for Tlsf {}

/// Error returned when no block can satisfy a request; the caller should
/// [`Tlsf::grow`] the region and retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory;

impl Default for Tlsf {
    fn default() -> Self {
        Self::new()
    }
}

impl Tlsf {
    pub const fn new() -> Tlsf {
        Tlsf {
            fl_bitmap: 0,
            sl_bitmap: [0; FL_COUNT],
            lists: [[null_mut(); SL_COUNT]; FL_COUNT],
            sentinel: null_mut(),
            used: 0,
            total: 0,
        }
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    pub fn total_bytes(&self) -> usize {
        self.total
    }

    unsafe fn insert(&mut self, b: *mut Block) {
        // SAFETY: `b` is a valid free block not in any list.
        unsafe {
            let (fl, sl) = mapping((*b).size());
            let head = self.lists[fl][sl];
            (*b).next_free = head;
            (*b).prev_free = null_mut();
            if !head.is_null() {
                (*head).prev_free = b;
            }
            self.lists[fl][sl] = b;
            self.fl_bitmap |= 1 << fl;
            self.sl_bitmap[fl] |= 1 << sl;
        }
    }

    unsafe fn remove(&mut self, b: *mut Block) {
        // SAFETY: `b` is a valid free block in the list for its size.
        unsafe {
            let (fl, sl) = mapping((*b).size());
            let (prev, next) = ((*b).prev_free, (*b).next_free);
            if !next.is_null() {
                (*next).prev_free = prev;
            }
            if prev.is_null() {
                self.lists[fl][sl] = next;
                if next.is_null() {
                    self.sl_bitmap[fl] &= !(1 << sl);
                    if self.sl_bitmap[fl] == 0 {
                        self.fl_bitmap &= !(1 << fl);
                    }
                }
            } else {
                (*prev).next_free = next;
            }
        }
    }

    /// Finds a free block of at least `size` bytes and removes it from its list.
    fn find(&mut self, size: usize) -> Option<*mut Block> {
        if let Some(b) = self.find_good_fit(size) {
            return Some(b);
        }
        // The rounded search skips the list `size` itself maps to, which may
        // still hold a block that is large enough: scan it (first fit).
        let (fl, sl) = mapping(size);
        if fl >= FL_COUNT {
            return None;
        }
        let mut b = self.lists[fl][sl];
        while !b.is_null() {
            // SAFETY: list entries are valid free blocks.
            unsafe {
                if (*b).size() >= size {
                    self.remove(b);
                    return Some(b);
                }
                b = (*b).next_free;
            }
        }
        None
    }

    /// Constant-time search: rounds the size up so that any block in the
    /// selected list fits.
    fn find_good_fit(&mut self, size: usize) -> Option<*mut Block> {
        let (mut fl, sl) = mapping(round_for_search(size));
        if fl >= FL_COUNT {
            return None;
        }
        let mut sl_map = self.sl_bitmap[fl] & (!0u32 << sl);
        if sl_map == 0 {
            let fl_map = self.fl_bitmap & (!0u64).checked_shl(fl as u32 + 1).unwrap_or(0);
            if fl_map == 0 {
                return None;
            }
            fl = fl_map.trailing_zeros() as usize;
            sl_map = self.sl_bitmap[fl];
        }
        let sl = sl_map.trailing_zeros() as usize;
        let b = self.lists[fl][sl];
        // SAFETY: bitmaps only mark non-empty lists.
        unsafe { self.remove(b) };
        Some(b)
    }

    /// Splits `b` so it has exactly `size` payload bytes if the remainder can
    /// form a block; inserts the remainder as a free block.
    unsafe fn split(&mut self, b: *mut Block, size: usize) {
        // SAFETY: `b` is a valid block not in any free list.
        unsafe {
            let total = (*b).size();
            if total >= size + HEADER + MIN_PAYLOAD {
                let rest = (Block::payload(b)).add(size) as *mut Block;
                (*rest).prev_phys = b;
                (*rest).size_flags = (total - size - HEADER) | FREE;
                (*b).size_flags = size | ((*b).size_flags & FREE);
                let after = Block::next_phys(rest);
                (*after).prev_phys = rest;
                self.merge_and_insert(rest);
            }
        }
    }

    /// Merges a free block with free physical neighbours and inserts it.
    unsafe fn merge_and_insert(&mut self, mut b: *mut Block) {
        // SAFETY: neighbours are valid blocks (the sentinel bounds the end).
        unsafe {
            let next = Block::next_phys(b);
            if (*next).is_free() {
                self.remove(next);
                (*b).size_flags = ((*b).size() + HEADER + (*next).size()) | FREE;
                (*Block::next_phys(b)).prev_phys = b;
            }
            let prev = (*b).prev_phys;
            if !prev.is_null() && (*prev).is_free() {
                self.remove(prev);
                (*prev).size_flags = ((*prev).size() + HEADER + (*b).size()) | FREE;
                (*Block::next_phys(prev)).prev_phys = prev;
                b = prev;
            }
            self.insert(b);
        }
    }

    /// Adds `[start, start + len)` to the region. The first call establishes
    /// the region; later calls must pass memory that starts exactly where the
    /// previous region ended (`end_of_region`).
    ///
    /// # Safety
    /// The memory must be valid, writable and unused for the allocator's life.
    pub unsafe fn grow(&mut self, start: *mut u8, len: usize) {
        let start_addr = start as usize;
        assert!(start_addr % ALIGN == 0 && len >= 2 * HEADER + MIN_PAYLOAD && len % ALIGN == 0);
        // SAFETY: the caller hands us exclusive memory.
        unsafe {
            let block = if self.sentinel.is_null() {
                let b = start as *mut Block;
                (*b).prev_phys = null_mut();
                (*b).size_flags = (len - 2 * HEADER) | FREE;
                b
            } else {
                // The old sentinel becomes the header of the new free block.
                assert_eq!(self.sentinel as usize + HEADER, start_addr, "TLSF regions must be contiguous");
                let b = self.sentinel;
                (*b).size_flags = (len - HEADER) | FREE;
                b
            };
            let sentinel = Block::next_phys(block);
            (*sentinel).prev_phys = block;
            (*sentinel).size_flags = 0; // used, size 0
            self.sentinel = sentinel;
            self.total += len;
            self.merge_and_insert(block);
        }
    }

    /// Address just past the end of the managed region.
    pub fn end_of_region(&self) -> *mut u8 {
        if self.sentinel.is_null() { null_mut() } else { (self.sentinel as usize + HEADER) as *mut u8 }
    }

    /// Allocates `size` bytes aligned to `align` (a power of two).
    pub fn alloc(&mut self, size: usize, align: usize) -> Result<*mut u8, OutOfMemory> {
        let size = size.max(MIN_PAYLOAD).checked_next_multiple_of(ALIGN).ok_or(OutOfMemory)?;
        if align <= ALIGN {
            let b = self.find(size).ok_or(OutOfMemory)?;
            // SAFETY: `b` is a free block of at least `size` bytes.
            unsafe {
                (*b).size_flags &= !FREE;
                self.split(b, size);
                self.used += (*b).size();
                return Ok(Block::payload(b));
            }
        }
        // Over-allocate so an aligned payload with room for a leading free
        // block fits inside.
        let padded = size.checked_add(align + HEADER + MIN_PAYLOAD).ok_or(OutOfMemory)?;
        let b = self.find(padded).ok_or(OutOfMemory)?;
        // SAFETY: as above; all pointers stay inside `b`.
        unsafe {
            let payload = Block::payload(b) as usize;
            let mut aligned = payload.next_multiple_of(align);
            if aligned != payload && aligned - payload < HEADER + MIN_PAYLOAD {
                aligned += align;
            }
            let mut blk = b;
            if aligned != payload {
                // Carve the leading gap into its own free block.
                let gap = aligned - payload; // >= HEADER + MIN_PAYLOAD
                let nb = (aligned - HEADER) as *mut Block;
                (*nb).prev_phys = b;
                (*nb).size_flags = (*b).size() - gap;
                (*b).size_flags = (gap - HEADER) | FREE;
                (*Block::next_phys(nb)).prev_phys = nb;
                self.insert(b);
                blk = nb;
            } else {
                (*blk).size_flags &= !FREE;
            }
            self.split(blk, size);
            self.used += (*blk).size();
            Ok(Block::payload(blk))
        }
    }

    /// Frees a pointer returned by [`Tlsf::alloc`].
    ///
    /// # Safety
    /// `p` must come from this allocator and not have been freed.
    pub unsafe fn free(&mut self, p: *mut u8) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let b = Block::from_payload(p);
            debug_assert!(!(*b).is_free(), "double free");
            self.used -= (*b).size();
            (*b).size_flags |= FREE;
            self.merge_and_insert(b);
        }
    }

    /// Attempts to resize an allocation in place. Returns `true` on success.
    ///
    /// # Safety
    /// `p` must be a live allocation from this allocator.
    pub unsafe fn resize_in_place(&mut self, p: *mut u8, new_size: usize) -> bool {
        let Some(new_size) = new_size.max(MIN_PAYLOAD).checked_next_multiple_of(ALIGN) else { return false };
        // SAFETY: guaranteed by the caller.
        unsafe {
            let b = Block::from_payload(p);
            let cur = (*b).size();
            if new_size <= cur {
                self.used -= cur;
                self.split(b, new_size);
                self.used += (*b).size();
                return true;
            }
            let next = Block::next_phys(b);
            if (*next).is_free() && cur + HEADER + (*next).size() >= new_size {
                self.remove(next);
                self.used -= cur;
                (*b).size_flags = cur + HEADER + (*next).size();
                (*Block::next_phys(b)).prev_phys = b;
                self.split(b, new_size);
                self.used += (*b).size();
                return true;
            }
            false
        }
    }

    /// Usable size of an allocation.
    ///
    /// # Safety
    /// `p` must be a live allocation from this allocator.
    pub unsafe fn usable_size(p: *mut u8) -> usize {
        // SAFETY: guaranteed by the caller.
        unsafe { (*Block::from_payload(p)).size() }
    }

    /// Checks internal invariants (tests and debugging).
    pub fn check(&self) -> Result<(), &'static str> {
        if self.sentinel.is_null() {
            return Ok(());
        }
        // Walk backwards from the sentinel through prev_phys.
        // SAFETY: read-only walk over valid blocks.
        unsafe {
            let mut b = (*self.sentinel).prev_phys;
            let mut next = self.sentinel;
            let mut prev_free = false;
            while !b.is_null() {
                if Block::next_phys(b) != next {
                    return Err("prev_phys/next_phys mismatch");
                }
                if (*b).is_free() && prev_free {
                    return Err("two adjacent free blocks");
                }
                prev_free = (*b).is_free();
                next = b;
                b = (*b).prev_phys;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[repr(align(4096))]
    struct Arena([u8; 1 << 20]);

    fn arena() -> std::boxed::Box<Arena> {
        // SAFETY: zeroed bytes are a valid Arena.
        unsafe { std::boxed::Box::new_zeroed().assume_init() }
    }

    #[test]
    fn mapping_is_monotonic() {
        let mut last = (0, 0);
        for size in (16..1 << 22).step_by(16) {
            let m = mapping(size);
            assert!(m >= last, "mapping not monotonic at {size}");
            last = m;
        }
    }

    #[test]
    fn alloc_free_roundtrip() {
        let mut a = arena();
        let mut t = Tlsf::new();
        unsafe { t.grow(a.0.as_mut_ptr(), a.0.len()) };
        let mut ptrs = Vec::new();
        for i in 0..2000 {
            let size = (i * 37) % 400 + 1;
            let p = t.alloc(size, 16).unwrap();
            assert_eq!(p as usize % 16, 0);
            unsafe { core::ptr::write_bytes(p, (i % 251) as u8, size) };
            ptrs.push((p, size, (i % 251) as u8));
        }
        t.check().unwrap();
        for (i, &(p, size, v)) in ptrs.iter().enumerate() {
            let s = unsafe { core::slice::from_raw_parts(p, size) };
            assert!(s.iter().all(|&b| b == v), "corruption in block {i}");
        }
        // Free every other block, then the rest.
        for (p, _, _) in ptrs.iter().step_by(2) {
            unsafe { t.free(*p) };
        }
        t.check().unwrap();
        for (p, _, _) in ptrs.iter().skip(1).step_by(2) {
            unsafe { t.free(*p) };
        }
        t.check().unwrap();
        assert_eq!(t.used_bytes(), 0);
        // Everything merged back into one block: a near-full allocation fits.
        let big = t.alloc(a.0.len() - 4 * HEADER, 16).unwrap();
        unsafe { t.free(big) };
    }

    #[test]
    fn aligned_allocations() {
        let mut a = arena();
        let mut t = Tlsf::new();
        unsafe { t.grow(a.0.as_mut_ptr(), a.0.len()) };
        let mut ptrs = Vec::new();
        for &align in &[32usize, 64, 128, 256, 4096] {
            for size in [1usize, 100, 5000] {
                let p = t.alloc(size, align).unwrap();
                assert_eq!(p as usize % align, 0);
                ptrs.push(p);
            }
        }
        t.check().unwrap();
        for p in ptrs {
            unsafe { t.free(p) };
        }
        t.check().unwrap();
        assert_eq!(t.used_bytes(), 0);
    }

    #[test]
    fn grow_extends_region() {
        let mut a = arena();
        let mut t = Tlsf::new();
        let half = a.0.len() / 2;
        unsafe { t.grow(a.0.as_mut_ptr(), half) };
        assert!(t.alloc(half, 16).is_err());
        let end = t.end_of_region();
        assert_eq!(end as usize, a.0.as_ptr() as usize + half);
        unsafe { t.grow(end, half) };
        let p = t.alloc(half, 16).unwrap();
        t.check().unwrap();
        unsafe { t.free(p) };
        t.check().unwrap();
    }

    #[test]
    fn resize_in_place_grows_into_free_neighbour() {
        let mut a = arena();
        let mut t = Tlsf::new();
        unsafe { t.grow(a.0.as_mut_ptr(), a.0.len()) };
        let p = t.alloc(100, 16).unwrap();
        let q = t.alloc(100, 16).unwrap();
        unsafe { t.free(q) };
        assert!(unsafe { t.resize_in_place(p, 200) });
        assert!(unsafe { Tlsf::usable_size(p) } >= 200);
        assert!(unsafe { t.resize_in_place(p, 50) });
        t.check().unwrap();
        unsafe { t.free(p) };
        assert_eq!(t.used_bytes(), 0);
    }

    #[test]
    fn randomized_stress() {
        let mut a = arena();
        let mut t = Tlsf::new();
        unsafe { t.grow(a.0.as_mut_ptr(), a.0.len()) };
        let mut live: Vec<(*mut u8, usize)> = Vec::new();
        let mut x = 0x1234_5678u64;
        for _ in 0..50_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if live.len() > 200 || (x % 3 == 0 && !live.is_empty()) {
                let i = (x as usize >> 8) % live.len();
                let (p, _) = live.swap_remove(i);
                unsafe { t.free(p) };
            } else {
                let size = (x as usize >> 16) % 4000 + 1;
                let align = 1 << ((x >> 40) % 8);
                if let Ok(p) = t.alloc(size, align) {
                    assert_eq!(p as usize % align.max(16), 0);
                    live.push((p, size));
                }
            }
        }
        t.check().unwrap();
        for (p, _) in live {
            unsafe { t.free(p) };
        }
        assert_eq!(t.used_bytes(), 0);
        t.check().unwrap();
    }
}
