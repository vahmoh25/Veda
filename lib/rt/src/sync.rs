//! Synchronisation primitives for user space.
//!
//! [`Mutex`] and [`Condvar`] are futex-based: uncontended operations never
//! enter the kernel. [`SpinLock`] is for tiny critical sections such as the
//! allocator, where a system call would cost more than spinning.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vabi::{Error, nr};

use crate::sys::call;

fn futex_wait(word: &AtomicU32, expected: u32, deadline: u64) -> Result<(), Error> {
    call(nr::FUTEX_WAIT, [word as *const AtomicU32 as usize, expected as usize, deadline as usize, 0, 0, 0]).map(|_| ())
}

fn futex_wake(word: &AtomicU32, count: usize) {
    let _ = call(nr::FUTEX_WAKE, [word as *const AtomicU32 as usize, count, 0, 0, 0, 0]);
}

/// A simple test-and-set spinlock (yields to the scheduler under contention).
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises access.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let mut spins = 0u32;
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            spins += 1;
            if spins > 100 {
                let _ = call(nr::YIELD, [0; 6]);
                spins = 0;
            } else {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// A futex-based mutex (states: 0 unlocked, 1 locked, 2 locked + waiters).
pub struct Mutex<T: ?Sized> {
    state: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises access.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Mutex { state: AtomicU32::new(0), data: UnsafeCell::new(v) }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            self.lock_slow();
        }
        MutexGuard { mutex: self }
    }

    #[cold]
    fn lock_slow(&self) {
        let mut spins = 0;
        loop {
            // Spin briefly before sleeping.
            if spins < 40 {
                if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return;
                }
                spins += 1;
                core::hint::spin_loop();
                continue;
            }
            if self.state.swap(2, Ordering::Acquire) == 0 {
                return;
            }
            let _ = futex_wait(&self.state, 2, vabi::DEADLINE_INFINITE);
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).ok().map(|_| MutexGuard { mutex: self })
    }

    fn unlock(&self) {
        if self.state.swap(0, Ordering::Release) == 2 {
            futex_wake(&self.state, 1);
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Mutex::new(T::default())
    }
}

pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

/// A condition variable for use with [`Mutex`].
pub struct Condvar {
    seq: AtomicU32,
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

impl Condvar {
    pub const fn new() -> Self {
        Condvar { seq: AtomicU32::new(0) }
    }

    /// Atomically releases `guard`, waits for a notification (or `deadline`),
    /// and re-acquires the lock. Returns `false` on timeout.
    pub fn wait_until<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>, deadline: u64) -> (MutexGuard<'a, T>, bool) {
        let seq = self.seq.load(Ordering::Acquire);
        let mutex = guard.mutex;
        drop(guard);
        let r = futex_wait(&self.seq, seq, deadline);
        (mutex.lock(), r != Err(Error::TimedOut))
    }

    pub fn wait<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.wait_until(guard, vabi::DEADLINE_INFINITE).0
    }

    pub fn notify_one(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        futex_wake(&self.seq, 1);
    }

    pub fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        futex_wake(&self.seq, usize::MAX >> 1);
    }
}

/// One-time initialisation.
pub struct Once {
    state: AtomicU32, // 0 = new, 1 = running, 2 = done
}

impl Default for Once {
    fn default() -> Self {
        Self::new()
    }
}

impl Once {
    pub const fn new() -> Self {
        Once { state: AtomicU32::new(0) }
    }

    pub fn call_once(&self, f: impl FnOnce()) {
        if self.state.load(Ordering::Acquire) == 2 {
            return;
        }
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire).is_ok() {
            f();
            self.state.store(2, Ordering::Release);
            futex_wake(&self.state, usize::MAX >> 1);
            return;
        }
        while self.state.load(Ordering::Acquire) != 2 {
            let _ = futex_wait(&self.state, 1, vabi::DEADLINE_INFINITE);
        }
    }

    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }
}

/// A value computed on first access.
pub struct Lazy<T, F = fn() -> T> {
    once: Once,
    init: UnsafeCell<Option<F>>,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: initialisation is serialised by `Once`; afterwards the value is
// only read.
unsafe impl<T: Send + Sync, F: Send> Sync for Lazy<T, F> {}

impl<T, F: FnOnce() -> T> Lazy<T, F> {
    pub const fn new(f: F) -> Self {
        Lazy { once: Once::new(), init: UnsafeCell::new(Some(f)), value: UnsafeCell::new(None) }
    }

    pub fn get(&self) -> &T {
        self.once.call_once(|| {
            // SAFETY: only one caller runs this closure.
            let f = unsafe { (*self.init.get()).take() }.expect("Lazy initialiser already taken");
            // SAFETY: as above.
            unsafe { *self.value.get() = Some(f()) };
        });
        // SAFETY: initialised and never mutated again.
        unsafe { (*self.value.get()).as_ref().unwrap() }
    }
}

impl<T, F: FnOnce() -> T> Deref for Lazy<T, F> {
    type Target = T;
    fn deref(&self) -> &T {
        self.get()
    }
}
