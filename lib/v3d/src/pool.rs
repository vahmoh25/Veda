//! A small pool of worker threads for the data-parallel rendering phases.
//!
//! [`ThreadPool::run`] executes a closure once on every participating thread
//! (the caller is participant 0) and returns when all of them have finished,
//! so the closure may borrow from the caller's stack. Work inside the
//! closure is divided by participant index or with an atomic counter.
//!
//! Threads come from `vrt::thread` and synchronise with the futex-based
//! `vrt::sync` primitives, so a pool only works inside Veda; with one
//! participant no thread is created and no system call is made (host tests).
//!
//! Workers are started as a chain: the caller wakes worker 1, which wakes
//! worker 2 as soon as it runs, and so on. The kernel sends a wake-up
//! interrupt to one idle CPU per woken thread, always the first idle one it
//! finds, so waking every worker at once from one CPU would interrupt the
//! same idle CPU three times and leave the other workers queued until a
//! time slice ends (~10 ms); a chain makes every wake-up come from a
//! different, running CPU.

use alloc::sync::Arc;
use alloc::vec::Vec;

use vrt::sync::{Condvar, Mutex};

/// The type-erased job published to the workers.
#[derive(Clone, Copy)]
struct JobPtr(*const (dyn Fn(usize) + Sync));

// SAFETY: the job is `Sync` and `run` keeps it alive until every worker is
// done with it.
unsafe impl Send for JobPtr {}

struct State {
    generation: u64,
    job: Option<JobPtr>,
    remaining: usize,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    /// One condition variable per participant (index 0 unused), so that a
    /// specific worker can be woken.
    wake: Vec<Condvar>,
    done: Condvar,
}

/// A fixed set of worker threads.
pub struct ThreadPool {
    shared: Arc<Shared>,
    threads: usize,
}

impl core::fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ThreadPool({} threads)", self.threads)
    }
}

impl ThreadPool {
    /// A pool with `threads` participants (the caller plus `threads - 1`
    /// workers). Falls back to fewer workers if threads cannot be created.
    pub fn new(threads: usize) -> ThreadPool {
        ThreadPool::with_priority(threads, None)
    }

    /// Like [`ThreadPool::new`], with the workers' scheduling priority (see
    /// `vabi::priority`). Workers below the system services' priority let
    /// the compositor run as soon as it wakes, even while every CPU renders.
    pub fn with_priority(threads: usize, priority: Option<usize>) -> ThreadPool {
        let wanted = threads.max(1);
        let shared = Arc::new(Shared {
            state: Mutex::new(State { generation: 0, job: None, remaining: 0, shutdown: false }),
            wake: (0..wanted).map(|_| Condvar::new()).collect(),
            done: Condvar::new(),
        });
        let mut count = 1;
        for index in 1..wanted {
            let s = shared.clone();
            let mut builder = vrt::thread::Builder::new().name("v3d-worker").stack_size(256 * 1024);
            if let Some(p) = priority {
                builder = builder.priority(p);
            }
            let spawned = builder.spawn(move || worker(s, index, wanted));
            match spawned {
                Ok(handle) => {
                    // Workers live as long as the pool; let the handle go.
                    drop(handle);
                    count += 1;
                }
                Err(_) => break,
            }
        }
        if count < wanted {
            // Workers that could not be created must not be waited for:
            // shrink the chain (the started ones exit on shutdown).
            shared.state.lock().shutdown = true;
            for c in &shared.wake {
                c.notify_all();
            }
            return ThreadPool::single();
        }
        ThreadPool { shared, threads: count }
    }

    /// A pool sized to the machine's CPU count (capped at 16), with workers
    /// just below normal priority.
    pub fn for_system() -> ThreadPool {
        let cpus = vrt::object::system_info().map(|i| i.cpu_count as usize).unwrap_or(1);
        ThreadPool::with_priority(cpus.clamp(1, 16), Some(vabi::priority::NORMAL - 2))
    }

    /// A pool without worker threads (runs everything on the caller).
    pub fn single() -> ThreadPool {
        ThreadPool {
            shared: Arc::new(Shared {
                state: Mutex::new(State { generation: 0, job: None, remaining: 0, shutdown: false }),
                wake: Vec::new(),
                done: Condvar::new(),
            }),
            threads: 1,
        }
    }

    /// Number of participants (including the caller).
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Runs `f(i)` for every participant `i` in `0..threads()` in parallel
    /// and waits for all of them.
    pub fn run(&self, f: &(dyn Fn(usize) + Sync)) {
        if self.threads <= 1 {
            f(0);
            return;
        }
        // SAFETY: only the lifetime is erased; this function does not return
        // before every worker has finished calling the job (`remaining`
        // reaches 0), so the reference stays valid while it is used.
        let job: &'static (dyn Fn(usize) + Sync) = unsafe { core::mem::transmute(f) };
        {
            let mut s = self.shared.state.lock();
            s.job = Some(JobPtr(job as *const _));
            s.remaining = self.threads - 1;
            s.generation += 1;
        }
        // Start the chain; worker 1 wakes worker 2 and so on.
        self.shared.wake[1].notify_one();
        f(0);
        let mut s = self.shared.state.lock();
        while s.remaining > 0 {
            s = self.shared.done.wait(s);
        }
        s.job = None;
    }

    /// Runs `f(i)` for `i` in `0..count`, distributing items dynamically.
    pub fn for_each(&self, count: usize, f: &(dyn Fn(usize) + Sync)) {
        let next = core::sync::atomic::AtomicUsize::new(0);
        self.run(&|_| {
            loop {
                let i = next.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if i >= count {
                    break;
                }
                f(i);
            }
        });
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        if self.threads > 1 {
            self.shared.state.lock().shutdown = true;
            for c in &self.shared.wake {
                c.notify_all();
            }
        }
    }
}

fn worker(shared: Arc<Shared>, index: usize, total: usize) {
    let mut seen = 0u64;
    loop {
        let job = {
            let mut s = shared.state.lock();
            while s.generation == seen && !s.shutdown {
                s = shared.wake[index].wait(s);
            }
            if s.shutdown {
                drop(s);
                if index + 1 < total {
                    shared.wake[index + 1].notify_all();
                }
                return;
            }
            seen = s.generation;
            s.job
        };
        // Pass the start signal on before working.
        if index + 1 < total {
            shared.wake[index + 1].notify_one();
        }
        if let Some(job) = job {
            // SAFETY: `run` keeps the job alive until `remaining` reaches 0,
            // which happens only after this call returns.
            unsafe { (*job.0)(index) };
        }
        let mut s = shared.state.lock();
        s.remaining -= 1;
        if s.remaining == 0 {
            drop(s);
            shared.done.notify_all();
        }
    }
}

/// Splits `weights` into `parts` contiguous ranges of similar total weight.
pub(crate) fn partition(weights: &[u32], parts: usize, out: &mut Vec<(usize, usize)>) {
    out.clear();
    let parts = parts.max(1);
    let total: u64 = weights.iter().map(|&w| w as u64).sum();
    let mut start = 0usize;
    let mut acc = 0u64;
    for p in 0..parts {
        let goal = total * (p as u64 + 1) / parts as u64;
        let mut end = start;
        while end < weights.len() && (acc + weights[end] as u64 <= goal || p == parts - 1) {
            acc += weights[end] as u64;
            end += 1;
        }
        // Make sure progress is made when one item outweighs a share.
        if end == start && end < weights.len() && p < parts - 1 && acc < goal {
            acc += weights[end] as u64;
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
}
