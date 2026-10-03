//! The global allocator.
//!
//! Small and medium allocations come from a TLSF allocator (`vheap`) over a
//! large reserved region: the runtime maps one big anonymous VMO whose pages
//! the kernel commits on first touch, and grows the TLSF region through it.
//! Allocations of 4 MiB and more get a dedicated VMO each, so freeing them
//! returns the memory to the system immediately.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use vabi::map_flags;
use vheap::Tlsf;

use crate::object::Vmo;
use crate::sync::SpinLock;

/// Address space reserved for the heap (committed lazily).
const HEAP_RESERVE: usize = 32 << 30;
/// Granularity of region growth.
const GROW_STEP: usize = 2 << 20;
/// Allocations at least this large get their own VMO.
const LARGE_THRESHOLD: usize = 4 << 20;

struct HeapState {
    tlsf: Tlsf,
    base: usize,
    reserved_end: usize,
}

pub struct Heap {
    state: SpinLock<HeapState>,
}

impl Heap {
    pub const fn new() -> Heap {
        Heap { state: SpinLock::new(HeapState { tlsf: Tlsf::new(), base: 0, reserved_end: 0 }) }
    }

    fn reserve(state: &mut HeapState) -> bool {
        let Ok(vmo) = Vmo::create(HEAP_RESERVE) else { return false };
        let Ok(addr) = vmo.map(0, HEAP_RESERVE, map_flags::READ | map_flags::WRITE) else { return false };
        // The mapping keeps the VMO alive; the handle itself is not needed.
        drop(vmo);
        state.base = addr;
        state.reserved_end = addr + HEAP_RESERVE;
        true
    }

    fn grow(state: &mut HeapState, at_least: usize) -> bool {
        if state.base == 0 && !Self::reserve(state) {
            return false;
        }
        let start = if state.tlsf.total_bytes() == 0 { state.base } else { state.tlsf.end_of_region() as usize };
        let len = (at_least + 64).next_multiple_of(GROW_STEP);
        if start + len > state.reserved_end {
            return false;
        }
        // SAFETY: fresh, exclusively owned part of the reserved region.
        unsafe { state.tlsf.grow(start as *mut u8, len) };
        true
    }

    pub fn stats(&self) -> (usize, usize) {
        let s = self.state.lock();
        (s.tlsf.used_bytes(), s.tlsf.total_bytes())
    }
}

fn large_alloc(layout: Layout) -> *mut u8 {
    let size = layout.size().next_multiple_of(4096);
    let Ok(vmo) = Vmo::create(size) else { return null_mut() };
    match vmo.map(0, size, map_flags::READ | map_flags::WRITE) {
        Ok(addr) => addr as *mut u8,
        Err(_) => null_mut(),
    }
}

// SAFETY: TLSF hands out disjoint, correctly aligned blocks; large blocks are
// page-aligned mappings.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= LARGE_THRESHOLD && layout.align() <= 4096 {
            return large_alloc(layout);
        }
        let mut s = self.state.lock();
        loop {
            if let Ok(p) = s.tlsf.alloc(layout.size(), layout.align()) {
                return p;
            }
            if !Heap::grow(&mut s, layout.size() + layout.align()) {
                return null_mut();
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() >= LARGE_THRESHOLD && layout.align() <= 4096 {
            let _ = crate::vm::unmap(None, ptr as usize, layout.size().next_multiple_of(4096));
            return;
        }
        // SAFETY: `ptr` came from this TLSF instance.
        unsafe { self.state.lock().tlsf.free(ptr) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let small_old = layout.size() < LARGE_THRESHOLD;
        let small_new = new_size < LARGE_THRESHOLD;
        if small_old && small_new && layout.align() <= 16 {
            // SAFETY: `ptr` is a live TLSF allocation.
            if unsafe { self.state.lock().tlsf.resize_in_place(ptr, new_size) } {
                return ptr;
            }
        }
        // SAFETY: standard allocate-copy-free fallback.
        unsafe {
            let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
            let new = self.alloc(new_layout);
            if !new.is_null() {
                core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            new
        }
    }
}
