//! Kernel virtual address regions outside the direct map:
//!
//! * MMIO windows (uncached or write-combining device registers), and
//! * kernel stacks, each preceded by an unmapped guard page so that a stack
//!   overflow faults instead of silently corrupting memory.

use alloc::vec::Vec;

use super::paging::{self, Cache, Flags};
use super::{PAGE_SIZE, phys};
use crate::sync::SpinLock;

const MMIO_BASE: u64 = 0xFFFF_C000_0000_0000;
const MMIO_LIMIT: u64 = MMIO_BASE + (64 << 30);
const STACK_BASE: u64 = 0xFFFF_C800_0000_0000;
/// Virtual size reserved per stack slot (stack + guard).
const STACK_SLOT: u64 = 128 * 1024;

struct State {
    mmio_next: u64,
    stack_next: u64,
    free_stack_slots: Vec<u64>,
}

static STATE: SpinLock<State> =
    SpinLock::new(State { mmio_next: MMIO_BASE, stack_next: STACK_BASE, free_stack_slots: Vec::new() });

/// Maps a physical MMIO range and returns the virtual address of `phys`.
pub fn map_mmio(phys_addr: u64, size: u64, cache: Cache) -> u64 {
    let start = phys_addr & !(PAGE_SIZE - 1);
    let pages = (phys_addr + size - start).div_ceil(PAGE_SIZE);
    let virt = {
        let mut s = STATE.lock();
        let v = s.mmio_next;
        s.mmio_next += (pages + 1) * PAGE_SIZE;
        assert!(s.mmio_next < MMIO_LIMIT, "kernel MMIO window exhausted");
        v
    };
    let flags = Flags { writable: true, executable: false, user: false, global: true, cache };
    for i in 0..pages {
        paging::map(paging::kernel_pml4(), virt + i * PAGE_SIZE, start + i * PAGE_SIZE, flags, true)
            .expect("out of memory mapping MMIO");
    }
    virt + (phys_addr - start)
}

/// A kernel stack with a guard page below it.
pub struct KernelStack {
    base: u64,
    pages: u64,
}

impl KernelStack {
    /// Allocates a stack of `pages` pages (at most `STACK_SLOT` minus guard).
    pub fn new(pages: u64) -> Option<KernelStack> {
        assert!((pages + 1) * PAGE_SIZE <= STACK_SLOT);
        let slot = {
            let mut s = STATE.lock();
            s.free_stack_slots.pop().unwrap_or_else(|| {
                let v = s.stack_next;
                s.stack_next += STACK_SLOT;
                v
            })
        };
        // The guard page is the lowest page of the slot and stays unmapped.
        let base = slot + STACK_SLOT - pages * PAGE_SIZE;
        for i in 0..pages {
            let Some(frame) = phys::alloc() else {
                // Roll back what we mapped so far.
                for j in 0..i {
                    if let Some(f) = paging::unmap(paging::kernel_pml4(), base + j * PAGE_SIZE) {
                        phys::free(f);
                    }
                }
                STATE.lock().free_stack_slots.push(slot);
                return None;
            };
            // Not global: freed stacks must be flushable with a CR3 reload.
            let flags = Flags { global: false, ..Flags::KERNEL_RW };
            paging::map(paging::kernel_pml4(), base + i * PAGE_SIZE, frame, flags, true).ok()?;
        }
        Some(KernelStack { base, pages })
    }

    pub fn top(&self) -> u64 {
        self.base + self.pages * PAGE_SIZE
    }

    pub fn bottom(&self) -> u64 {
        self.base
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        for i in 0..self.pages {
            let va = self.base + i * PAGE_SIZE;
            if let Some(frame) = paging::unmap(paging::kernel_pml4(), va) {
                phys::free(frame);
            }
        }
        // Other CPUs may still cache translations of this stack.
        crate::arch::tlb::shootdown(self.base, self.pages * PAGE_SIZE);
        let slot = self.base + self.pages * PAGE_SIZE - STACK_SLOT;
        STATE.lock().free_stack_slots.push(slot);
    }
}

/// Returns `true` if `addr` lies in a kernel stack guard page.
pub fn is_stack_guard(addr: u64) -> bool {
    addr >= STACK_BASE && addr < STACK_BASE + (1 << 39) && (addr - STACK_BASE) % STACK_SLOT < PAGE_SIZE
}
