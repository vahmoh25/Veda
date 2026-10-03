//! Kernel synchronisation.
//!
//! # Concurrency model
//!
//! Vindows uses a *big kernel lock* (BKL), like seL4 on SMP: a CPU must hold
//! the BKL whenever it executes kernel code that touches shared state. Kernel
//! code runs with interrupts disabled and is never preempted, so the kernel is
//! effectively single-threaded and kernel operations are short. User code runs
//! in parallel on all CPUs.
//!
//! The lock is acquired on every entry from user mode (system call, interrupt,
//! exception) and released on the way back. A context switch hands the lock
//! from the outgoing to the incoming thread.
//!
//! Individual objects additionally use [`SpinLock`] for interior mutability so
//! that the code stays sound by construction. Under the BKL those locks are
//! never contended; they detect accidental recursive locking on one CPU, which
//! would otherwise be a silent deadlock.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::percpu;

/// Owner value of an unlocked lock. Owners are stored as `cpu_id + 1` so that
/// a fresh lock is all zeroes (large lock-protected statics then live in .bss).
const NO_OWNER: u32 = 0;

/// A spinlock that panics on recursive acquisition by the same CPU.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    owner: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `data`.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), owner: AtomicU32::new(NO_OWNER), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> SpinLock<T> {
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let me = percpu::cpu_id_or_boot() + 1;
        loop {
            if self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                self.owner.store(me, Ordering::Relaxed);
                return SpinLockGuard { lock: self };
            }
            if self.owner.load(Ordering::Relaxed) == me {
                panic!("SpinLock: recursive lock on CPU {}", me - 1);
            }
            core::hint::spin_loop();
        }
    }
}

pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(NO_OWNER, Ordering::Relaxed);
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// The big kernel lock.
pub mod bkl {
    use super::*;

    static LOCKED: AtomicBool = AtomicBool::new(false);
    static OWNER: AtomicU32 = AtomicU32::new(NO_OWNER);

    /// Acquires the BKL. While spinning, this CPU keeps servicing TLB
    /// shootdown requests so that the holder can safely wait for them.
    pub fn acquire() {
        debug_assert!(!crate::arch::cpu::interrupts_enabled(), "the BKL is taken with interrupts disabled");
        let me = percpu::cpu_id_or_boot() + 1;
        loop {
            if LOCKED.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                OWNER.store(me, Ordering::Relaxed);
                return;
            }
            if OWNER.load(Ordering::Relaxed) == me {
                panic!("BKL acquired recursively on CPU {}", me - 1);
            }
            crate::arch::tlb::service_pending();
            core::hint::spin_loop();
        }
    }

    pub fn release() {
        debug_assert!(held_by_me(), "BKL released by a CPU that does not hold it");
        OWNER.store(NO_OWNER, Ordering::Relaxed);
        LOCKED.store(false, Ordering::Release);
    }

    pub fn held_by_me() -> bool {
        LOCKED.load(Ordering::Relaxed) && OWNER.load(Ordering::Relaxed) == percpu::cpu_id_or_boot() + 1
    }
}

/// A value initialised exactly once during boot, then read-only.
pub struct Once<T> {
    ready: AtomicBool,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: written once before `ready` is published, read-only afterwards.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Once { ready: AtomicBool::new(false), value: UnsafeCell::new(None) }
    }

    /// Stores the value. Panics if called twice.
    pub fn set(&self, value: T) {
        assert!(!self.ready.load(Ordering::Acquire), "Once::set called twice");
        // SAFETY: no reader can observe the value before `ready` is set.
        unsafe { *self.value.get() = Some(value) };
        self.ready.store(true, Ordering::Release);
    }

    pub fn get(&self) -> Option<&T> {
        if self.ready.load(Ordering::Acquire) {
            // SAFETY: published and never mutated again.
            unsafe { (*self.value.get()).as_ref() }
        } else {
            None
        }
    }

    /// Returns the value, panicking if it was not initialised.
    pub fn expect(&self) -> &T {
        self.get().expect("Once value used before initialisation")
    }
}
