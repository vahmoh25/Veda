//! The kernel heap (`GlobalAlloc`).
//!
//! Small allocations (up to 2 KiB) come from per-size-class slabs carved out
//! of direct-mapped 4 KiB frames; freed blocks go on intrusive free lists.
//! Larger allocations take physically contiguous frames directly. All memory
//! is addressed through the direct map, so no page-table work is needed.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use super::{PAGE_SIZE, phys, phys_to_virt, virt_to_phys};
use crate::sync::SpinLock;

const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

struct FreeBlock {
    next: *mut FreeBlock,
}

struct Slabs {
    heads: [*mut FreeBlock; CLASSES.len()],
    /// Bytes currently allocated (for diagnostics).
    in_use: usize,
}

// SAFETY: the raw pointers are only touched under the lock.
unsafe impl Send for Slabs {}

pub struct KernelHeap {
    slabs: SpinLock<Slabs>,
}

fn class_index(size: usize) -> Option<usize> {
    CLASSES.iter().position(|&c| c >= size)
}

impl KernelHeap {
    pub const fn new() -> Self {
        KernelHeap { slabs: SpinLock::new(Slabs { heads: [null_mut(); CLASSES.len()], in_use: 0 }) }
    }

    fn refill(slabs: &mut Slabs, class: usize) -> bool {
        let Some(frame) = phys::alloc() else { return false };
        let size = CLASSES[class];
        let base = phys_to_virt(frame) as usize;
        let count = PAGE_SIZE as usize / size;
        // Thread the new page's blocks onto the free list.
        for i in (0..count).rev() {
            let block = (base + i * size) as *mut FreeBlock;
            // SAFETY: `block` lies inside the frame we own.
            unsafe { (*block).next = slabs.heads[class] };
            slabs.heads[class] = block;
        }
        true
    }
}

// SAFETY: blocks handed out are disjoint and properly aligned (class sizes
// are powers of two, slab pages are page aligned, large allocations are
// page aligned).
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align()).max(1);
        if let Some(class) = class_index(size).filter(|_| layout.align() <= 2048) {
            let mut slabs = self.slabs.lock();
            if slabs.heads[class].is_null() && !Self::refill(&mut slabs, class) {
                return null_mut();
            }
            let block = slabs.heads[class];
            // SAFETY: non-null free-list entries point at free blocks.
            slabs.heads[class] = unsafe { (*block).next };
            slabs.in_use += CLASSES[class];
            return block as *mut u8;
        }
        let pages = (layout.size() as u64).div_ceil(PAGE_SIZE);
        match phys::alloc_contiguous(pages, (layout.align() as u64).max(PAGE_SIZE), u64::MAX) {
            Some(p) => {
                self.slabs.lock().in_use += (pages * PAGE_SIZE) as usize;
                phys_to_virt(p) as *mut u8
            }
            None => null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size().max(layout.align()).max(1);
        if let Some(class) = class_index(size).filter(|_| layout.align() <= 2048) {
            let mut slabs = self.slabs.lock();
            let block = ptr as *mut FreeBlock;
            // SAFETY: `ptr` was allocated from this class and is now free.
            unsafe { (*block).next = slabs.heads[class] };
            slabs.heads[class] = block;
            slabs.in_use -= CLASSES[class];
            return;
        }
        let pages = (layout.size() as u64).div_ceil(PAGE_SIZE);
        phys::free_contiguous(virt_to_phys(ptr as u64), pages);
        self.slabs.lock().in_use -= (pages * PAGE_SIZE) as usize;
    }
}
