//! Physical page frame allocator.
//!
//! A bitmap with one bit per 4 KiB frame (1 = in use) covering all RAM. A
//! rolling search hint makes single-frame allocation fast in the common case;
//! contiguous allocations (DMA buffers, large kernel objects) scan for runs.
//!
//! It also remembers where the RAM is that the kernel owns (free RAM, its
//! own image, the boot data, the loader's memory it takes back): physical
//! VMOs, which drivers map devices' memory with, may not reach it.

use bootinfo::{MemoryKind, MemoryRegion};

use super::{PAGE_SIZE, phys_to_virt};
use crate::sync::SpinLock;

struct Bitmap {
    words: &'static mut [u64],
    frames: u64,
    free: u64,
    total_usable: u64,
    hint: usize,
}

static ALLOCATOR: SpinLock<Option<Bitmap>> = SpinLock::new(None);

/// The kernel's RAM, as ranges of physical addresses (adjacent regions
/// merged); past the last slot, the last range grows to cover the rest (more
/// is taken for RAM, never less).
static RAM: SpinLock<([(u64, u64); 64], usize)> = SpinLock::new(([(0, 0); 64], 0));

/// The kinds of the memory map's regions that are the kernel's RAM.
fn is_kernel_ram(kind: MemoryKind) -> bool {
    matches!(
        kind,
        MemoryKind::Usable
            | MemoryKind::LoaderReclaimable
            | MemoryKind::Kernel
            | MemoryKind::Initrd
            | MemoryKind::BootData
    )
}

/// Whether `[base, base + size)` reaches the kernel's RAM.
pub fn overlaps_ram(base: u64, size: u64) -> bool {
    let end = base.saturating_add(size);
    let ram = RAM.lock();
    ram.0[..ram.1].iter().any(|&(s, e)| base < e && s < end)
}

fn remember_ram(map: &[MemoryRegion]) {
    let mut regions: [(u64, u64); 256] = [(0, 0); 256];
    let mut n = 0;
    for r in map.iter().filter(|r| is_kernel_ram(r.kind)) {
        if n == regions.len() {
            // Even more regions: the last takes them in.
            regions[n - 1].1 = regions[n - 1].1.max(r.end());
            continue;
        }
        regions[n] = (r.base, r.end());
        n += 1;
    }
    regions[..n].sort_unstable();
    let mut ram = RAM.lock();
    let (slots, used) = &mut *ram;
    for &(s, e) in &regions[..n] {
        match *used {
            u if u > 0 && s <= slots[u - 1].1 => slots[u - 1].1 = slots[u - 1].1.max(e),
            u if u < slots.len() => {
                slots[u] = (s, e);
                *used += 1;
            }
            u => slots[u - 1].1 = slots[u - 1].1.max(e),
        }
    }
}

/// Physical memory below this address is never handed out (real-mode area,
/// AP trampoline).
const LOW_RESERVED: u64 = 0x10_0000;

impl Bitmap {
    fn set(&mut self, frame: u64, used: bool) {
        let (w, b) = ((frame / 64) as usize, frame % 64);
        let was = self.words[w] & (1 << b) != 0;
        if used && !was {
            self.words[w] |= 1 << b;
            self.free -= 1;
        } else if !used && was {
            self.words[w] &= !(1 << b);
            self.free += 1;
        }
    }

    fn is_used(&self, frame: u64) -> bool {
        self.words[(frame / 64) as usize] & (1 << (frame % 64)) != 0
    }

    fn alloc_one(&mut self) -> Option<u64> {
        let n = self.words.len();
        for i in 0..n {
            let w = (self.hint + i) % n;
            if self.words[w] != u64::MAX {
                let bit = (!self.words[w]).trailing_zeros() as u64;
                let frame = w as u64 * 64 + bit;
                if frame >= self.frames {
                    continue;
                }
                self.set(frame, true);
                self.hint = w;
                return Some(frame);
            }
        }
        None
    }

    fn alloc_run(&mut self, count: u64, align: u64, max_frame: u64) -> Option<u64> {
        let align = align.max(1);
        let limit = self.frames.min(max_frame);
        let mut start = 0u64;
        while start + count <= limit {
            if !start.is_multiple_of(align) {
                start = start.next_multiple_of(align);
                continue;
            }
            match (start..start + count).find(|&f| self.is_used(f)) {
                Some(used) => start = (used + 1).next_multiple_of(align),
                None => {
                    for f in start..start + count {
                        self.set(f, true);
                    }
                    return Some(start);
                }
            }
        }
        None
    }
}

/// Initialises the allocator from the boot memory map. Only `Usable` RAM is
/// made available now; boot-time regions are reclaimed later.
pub fn init(map: &[MemoryRegion]) {
    remember_ram(map);
    let top = map
        .iter()
        .filter(|r| {
            matches!(
                r.kind,
                MemoryKind::Usable | MemoryKind::LoaderReclaimable | MemoryKind::BootData | MemoryKind::AcpiReclaimable
            )
        })
        .map(|r| r.end())
        .max()
        .unwrap_or(0);
    let frames = top / PAGE_SIZE;
    let words = frames.div_ceil(64) as usize;
    let bytes = (words * 8) as u64;

    // Place the bitmap in the first usable region large enough to hold it.
    let region = map
        .iter()
        .find(|r| r.kind == MemoryKind::Usable && r.base >= LOW_RESERVED && r.pages * PAGE_SIZE >= bytes)
        .expect("no usable memory region can hold the frame bitmap");
    let bitmap_phys = region.base;
    // SAFETY: the region is usable RAM, mapped by the direct map, and not yet
    // handed out to anyone.
    let words_slice = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(bitmap_phys) as *mut u64, words) };
    words_slice.fill(u64::MAX);

    let mut bm = Bitmap { words: words_slice, frames, free: 0, total_usable: 0, hint: 0 };
    for r in map.iter().filter(|r| r.kind == MemoryKind::Usable) {
        for f in r.base / PAGE_SIZE..r.end() / PAGE_SIZE {
            if f * PAGE_SIZE >= LOW_RESERVED {
                bm.set(f, false);
                bm.total_usable += 1;
            }
        }
    }
    for f in bitmap_phys / PAGE_SIZE..(bitmap_phys + bytes).div_ceil(PAGE_SIZE) {
        bm.set(f, true);
    }
    *ALLOCATOR.lock() = Some(bm);
}

/// Returns boot-time memory of `kind` to the allocator.
pub fn reclaim(map: &[MemoryRegion], kind: MemoryKind) -> u64 {
    let mut guard = ALLOCATOR.lock();
    let bm = guard.as_mut().expect("frame allocator not initialised");
    let mut n = 0;
    for r in map.iter().filter(|r| r.kind == kind) {
        for f in r.base / PAGE_SIZE..r.end() / PAGE_SIZE {
            if f * PAGE_SIZE >= LOW_RESERVED && f < bm.frames && bm.is_used(f) {
                bm.set(f, false);
                bm.total_usable += 1;
                n += 1;
            }
        }
    }
    n * PAGE_SIZE
}

/// Allocates one frame (contents undefined).
pub fn alloc() -> Option<u64> {
    ALLOCATOR.lock().as_mut()?.alloc_one().map(|f| f * PAGE_SIZE)
}

/// Allocates one zero-filled frame.
pub fn alloc_zeroed() -> Option<u64> {
    let p = alloc()?;
    zero_frames(p, 1);
    Some(p)
}

/// Allocates `count` physically contiguous, zeroed frames aligned to
/// `align` bytes, entirely below `max_addr`.
pub fn alloc_contiguous(count: u64, align: u64, max_addr: u64) -> Option<u64> {
    let mut guard = ALLOCATOR.lock();
    let bm = guard.as_mut()?;
    let f = bm.alloc_run(count, (align / PAGE_SIZE).max(1), max_addr / PAGE_SIZE)?;
    drop(guard);
    zero_frames(f * PAGE_SIZE, count);
    Some(f * PAGE_SIZE)
}

pub fn free(phys: u64) {
    free_contiguous(phys, 1);
}

pub fn free_contiguous(phys: u64, count: u64) {
    let mut guard = ALLOCATOR.lock();
    let bm = guard.as_mut().expect("frame allocator not initialised");
    for f in phys / PAGE_SIZE..phys / PAGE_SIZE + count {
        debug_assert!(bm.is_used(f), "double free of frame {:#x}", f * PAGE_SIZE);
        bm.set(f, false);
    }
}

/// Fills frames with zeroes through the direct map.
pub fn zero_frames(phys: u64, count: u64) {
    let qwords = count * PAGE_SIZE / 8;
    // SAFETY: the frames are owned by the caller and mapped by the direct map.
    unsafe {
        core::arch::asm!("rep stosq", inout("rdi") phys_to_virt(phys) => _, inout("rcx") qwords => _, in("rax") 0u64,
            options(nostack, preserves_flags));
    }
}

/// (total usable bytes, free bytes)
pub fn stats() -> (u64, u64) {
    match ALLOCATOR.lock().as_ref() {
        Some(bm) => (bm.total_usable * PAGE_SIZE, bm.free * PAGE_SIZE),
        None => (0, 0),
    }
}
