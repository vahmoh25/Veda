//! The scheduler.
//!
//! * 32 priority levels with round-robin within a level; a global run queue
//!   shared by all CPUs (the BKL makes a global queue cheap and fair).
//! * Preemptive: a running thread is preempted when its 10 ms time slice
//!   ends and a thread of equal or higher priority is ready, or immediately
//!   when a higher-priority thread wakes up.
//! * Tickless: each CPU arms its local APIC timer for its next event (time
//!   slice end or the earliest sleeping thread's deadline).
//! * Every CPU has an idle thread that halts until an interrupt arrives.

pub mod thread;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub use thread::{State, Thread, WakeReason};

use crate::arch::entry::{TrapFrame, vk_context_switch};
use crate::arch::{apic, cpu, idt, percpu};
use crate::sync::{SpinLock, bkl};
use crate::time;

/// Length of a time slice.
pub const TIME_SLICE_NS: u64 = 10_000_000;
/// Longest an idle CPU sleeps without a timer event.
const IDLE_MAX_NS: u64 = 500_000_000;

/// Per-CPU scheduler state (lives in `PerCpu`).
pub struct CpuSched {
    pub current: Option<Arc<Thread>>,
    pub idle: Option<Arc<Thread>>,
    /// The thread we just switched away from, kept alive until the switch has
    /// completed (its stack was in use during the switch).
    pub prev: Option<Arc<Thread>>,
    pub need_resched: bool,
    pub slice_end: u64,
    pub switched_in_at: u64,
}

impl CpuSched {
    pub const fn new() -> Self {
        CpuSched { current: None, idle: None, prev: None, need_resched: false, slice_end: 0, switched_in_at: 0 }
    }
}

#[allow(clippy::mut_from_ref)]
fn cpu_sched() -> &'static mut CpuSched {
    // SAFETY: each CPU only touches its own scheduler state, with interrupts
    // disabled and the BKL held; callers keep the borrow short.
    unsafe { &mut *percpu::current().sched.get() }
}

struct RunQueue {
    levels: [VecDeque<Arc<Thread>>; 32],
    bitmap: u32,
    len: usize,
}

impl RunQueue {
    fn push(&mut self, t: Arc<Thread>, priority: u8) {
        let p = priority as usize & 31;
        self.levels[p].push_back(t);
        self.bitmap |= 1 << p;
        self.len += 1;
    }

    fn highest(&self) -> Option<u8> {
        if self.bitmap == 0 { None } else { Some(31 - self.bitmap.leading_zeros() as u8) }
    }

    fn pop(&mut self) -> Option<Arc<Thread>> {
        let p = self.highest()? as usize;
        let t = self.levels[p].pop_front();
        if self.levels[p].is_empty() {
            self.bitmap &= !(1 << p);
        }
        self.len -= 1;
        t
    }
}

static RUNQ: SpinLock<RunQueue> =
    SpinLock::new(RunQueue { levels: [const { VecDeque::new() }; 32], bitmap: 0, len: 0 });

/// Blocked threads with a deadline, keyed by (deadline, koid).
static TIMEOUTS: SpinLock<BTreeMap<(u64, u64), Weak<Thread>>> = SpinLock::new(BTreeMap::new());

/// The thread running on this CPU.
pub fn current() -> Arc<Thread> {
    cpu_sched().current.clone().expect("scheduler not initialised on this CPU")
}

/// Like [`current`] but usable before the scheduler is up.
pub fn try_current() -> Option<Arc<Thread>> {
    cpu_sched().current.clone()
}

/// Installs the idle thread of this CPU as its current thread.
pub fn init_cpu(idle: Arc<Thread>) {
    let cs = cpu_sched();
    cs.idle = Some(idle.clone());
    cs.current = Some(idle);
    cs.switched_in_at = time::now_ns();
}

pub fn ready_count() -> usize {
    RUNQ.lock().len
}

/// Makes a new or blocked thread runnable.
pub fn make_ready(t: &Arc<Thread>) {
    let prio = {
        let mut s = t.sched.lock();
        debug_assert!(matches!(s.state, State::New | State::Blocked));
        s.state = State::Ready;
        s.priority
    };
    RUNQ.lock().push(t.clone(), prio);
    after_wakeup(prio);
}

/// Preemption and idle-CPU wake-up after `prio` became runnable.
fn after_wakeup(prio: u8) {
    let me = percpu::cpu_id_or_boot();
    if let Some(cur) = try_current() {
        if cur.is_idle || prio > cur.priority() {
            cpu_sched().need_resched = true;
        }
    }
    // Wake one idle CPU (other than us) to pick up the work.
    for p in percpu::online() {
        if p.cpu_id == me {
            continue;
        }
        // SAFETY: reading another CPU's current thread pointer under the BKL;
        // it only changes while that CPU holds the BKL.
        let idle = unsafe { (*p.sched.get()).current.as_ref().is_some_and(|t| t.is_idle) };
        if idle {
            apic::send_ipi(p.apic_id, idt::RESCHED_VECTOR as u32);
            break;
        }
    }
}

/// Wakes a blocked thread. No effect on threads in other states.
pub fn wake(t: &Arc<Thread>, reason: WakeReason) {
    let prio = {
        let mut s = t.sched.lock();
        if s.state != State::Blocked {
            return;
        }
        s.state = State::Ready;
        s.wake_reason = reason;
        if let Some(key) = s.timeout_key.take() {
            TIMEOUTS.lock().remove(&key);
        }
        s.priority
    };
    RUNQ.lock().push(t.clone(), prio);
    after_wakeup(prio);
}

/// Blocks the current thread until [`wake`] is called or `deadline`
/// (monotonic ns) passes.
pub fn block(deadline: Option<u64>) -> WakeReason {
    let cur = current();
    if cur.kill_pending.load(Ordering::Acquire) {
        return WakeReason::Killed;
    }
    if let Some(d) = deadline {
        if d <= time::now_ns() {
            return WakeReason::TimedOut;
        }
    }
    {
        let mut s = cur.sched.lock();
        s.state = State::Blocked;
        s.wake_reason = WakeReason::None;
        if let Some(d) = deadline {
            let key = (d, cur.koid);
            TIMEOUTS.lock().insert(key, Arc::downgrade(&cur));
            s.timeout_key = Some(key);
        }
    }
    schedule();
    let mut s = cur.sched.lock();
    if let Some(key) = s.timeout_key.take() {
        TIMEOUTS.lock().remove(&key);
    }
    s.wake_reason
}

/// Sleeps until `deadline` (monotonic ns).
pub fn sleep_until(deadline: u64) -> WakeReason {
    loop {
        match block(Some(deadline)) {
            WakeReason::TimedOut => return WakeReason::TimedOut,
            WakeReason::Killed => return WakeReason::Killed,
            _ if time::now_ns() >= deadline => return WakeReason::TimedOut,
            _ => {}
        }
    }
}

/// Wakes every thread whose deadline has passed.
fn expire_timeouts(now: u64) {
    loop {
        let t = {
            let mut q = TIMEOUTS.lock();
            match q.first_key_value() {
                Some((&(d, _), _)) if d <= now => q.pop_first().and_then(|(_, w)| w.upgrade()),
                _ => break,
            }
        };
        if let Some(t) = t {
            // The key was already removed; clear it so `wake` does not look.
            t.sched.lock().timeout_key = None;
            wake(&t, WakeReason::TimedOut);
        }
    }
}

fn earliest_timeout() -> Option<u64> {
    TIMEOUTS.lock().first_key_value().map(|(&(d, _), _)| d)
}

/// Arms this CPU's timer for its next event.
fn arm_timer(running_idle: bool) {
    let now = time::now_ns();
    let mut deadline = if running_idle { now + IDLE_MAX_NS } else { cpu_sched().slice_end };
    if let Some(t) = earliest_timeout() {
        deadline = deadline.min(t);
    }
    let deadline = deadline.max(now + 50_000);
    apic::arm_timer(time::ns_to_tsc(deadline), deadline - now);
}

/// Picks the next thread and switches to it if it differs from the current
/// one. The current thread must already have set its state if it is
/// blocking or exiting; a `Running` current thread is preempted only if
/// another thread of at least its priority is ready.
pub fn schedule() {
    let cs = cpu_sched();
    cs.need_resched = false;
    let cur = cs.current.clone().expect("schedule() without a current thread");
    let cur_state = cur.state();
    let next = {
        let mut rq = RUNQ.lock();
        if cur_state == State::Running && !cur.is_idle {
            match rq.highest() {
                Some(p) if p >= cur.priority() => {
                    let next = rq.pop().unwrap();
                    cur.sched.lock().state = State::Ready;
                    let prio = cur.priority();
                    rq.push(cur.clone(), prio);
                    next
                }
                _ => {
                    drop(rq);
                    // Keep running; start a fresh slice if this one ended.
                    if time::now_ns() >= cs.slice_end {
                        cs.slice_end = time::now_ns() + TIME_SLICE_NS;
                    }
                    arm_timer(false);
                    return;
                }
            }
        } else {
            match rq.pop() {
                Some(t) => t,
                None if cur.is_idle => {
                    drop(rq);
                    arm_timer(true);
                    return;
                }
                None => cs.idle.clone().expect("no idle thread"),
            }
        }
    };
    switch_to(cur, next);
}

/// Performs the actual switch from `prev` (the current thread) to `next`.
fn switch_to(prev: Arc<Thread>, next: Arc<Thread>) {
    let cs = cpu_sched();
    let now = time::now_ns();
    let ran = now.saturating_sub(cs.switched_in_at);
    prev.cpu_time_ns.fetch_add(ran, Ordering::Relaxed);
    if prev.is_idle {
        percpu::current().idle_ns.fetch_add(ran, Ordering::Relaxed);
    }
    cs.switched_in_at = now;

    if !prev.is_idle {
        prev.save_fpu();
        // SAFETY: the scheduler owns the contexts under the BKL.
        unsafe { prev.ctx().fs_base = cpu::rdmsr(cpu::MSR_FS_BASE) };
    }
    if !next.is_idle {
        next.restore_fpu();
        // SAFETY: as above.
        unsafe { cpu::wrmsr(cpu::MSR_FS_BASE, next.ctx().fs_base) };
    }
    match &next.aspace {
        Some(a) => a.activate(),
        None => {
            let k = crate::mm::paging::kernel_pml4();
            if cpu::read_cr3() != k {
                // SAFETY: the kernel PML4 maps all kernel code and data.
                unsafe { cpu::write_cr3(k) };
            }
        }
    }
    if next.kernel_stack_top != 0 {
        percpu::set_kernel_stack(next.kernel_stack_top);
    }
    {
        let mut s = next.sched.lock();
        s.state = State::Running;
        s.cpu = percpu::cpu_id_or_boot();
    }
    cs.slice_end = now + TIME_SLICE_NS;
    arm_timer(next.is_idle);

    // SAFETY: contexts are only touched here, under the BKL.
    let prev_rsp: *mut u64 = unsafe { &mut prev.ctx().rsp };
    // SAFETY: as above.
    let next_rsp = unsafe { next.ctx().rsp };
    if next_rsp == 0 {
        panic!(
            "switching to thread {} ({} of {}, idle={}, state={:?}) with no saved context; prev {} ({})",
            next.koid,
            next.name.as_str(),
            next.process_name(),
            next.is_idle,
            next.state(),
            prev.koid,
            prev.process_name()
        );
    }
    cs.current = Some(next);
    cs.prev = Some(prev);
    // SAFETY: `next_rsp` is a stack prepared by `Thread::new_*` or saved by
    // a previous switch; `prev` stays alive in `cs.prev` until the switch is
    // complete.
    unsafe { vk_context_switch(prev_rsp, next_rsp) };
    finish_switch();
}

/// Runs on the incoming thread right after a switch: releases the outgoing
/// thread (possibly freeing it if it died).
fn finish_switch() {
    let prev = cpu_sched().prev.take();
    drop(prev);
}

/// First Rust code run by a new user thread (from `vk_thread_trampoline`).
pub extern "sysv64" fn thread_start_hook() {
    finish_switch();
    let cur = current();
    if cur.kill_pending.load(Ordering::Acquire) {
        exit_current();
    }
    bkl::release();
}

/// First Rust code run by a new kernel thread.
pub extern "sysv64" fn kernel_thread_start_hook() {
    finish_switch();
}

/// Requests a reschedule at the next opportunity (RESCHED IPI).
pub fn request_resched() {
    cpu_sched().need_resched = true;
}

/// Local APIC timer interrupt.
pub fn timer_interrupt() {
    let now = time::now_ns();
    expire_timeouts(now);
    let cs = cpu_sched();
    let cur_idle = cs.current.as_ref().is_some_and(|t| t.is_idle);
    if !cur_idle && now >= cs.slice_end {
        cs.need_resched = true;
    }
    if !cs.need_resched {
        arm_timer(cur_idle);
    }
}

/// Called on every return to user mode (with the BKL held): handles
/// preemption and pending kills.
pub fn return_to_user_hook(_frame: &mut TrapFrame) {
    loop {
        if current().kill_pending.load(Ordering::Acquire) {
            exit_current();
        }
        if cpu_sched().need_resched {
            schedule();
            continue;
        }
        break;
    }
}

/// Voluntarily gives up the CPU.
pub fn yield_now() {
    let cs = cpu_sched();
    cs.slice_end = 0;
    schedule();
}

/// Terminates the current thread. Never returns.
pub fn exit_current() -> ! {
    let cur = current();
    {
        let mut s = cur.sched.lock();
        s.state = State::Dead;
        if let Some(key) = s.timeout_key.take() {
            TIMEOUTS.lock().remove(&key);
        }
    }
    cur.signals.update(0, vabi::signals::TERMINATED);
    if let Some(p) = cur.process.clone() {
        p.thread_exited(&cur);
    }
    drop(cur);
    schedule();
    unreachable!("a dead thread was rescheduled");
}

/// Handles a CPU exception raised by user code (BKL held).
pub fn user_exception(frame: &mut TrapFrame) {
    let vector = frame.vector as usize;
    let cur = current();
    if vector == 14 {
        let addr = cpu::read_cr2();
        let write = frame.error_code & 2 != 0;
        let exec = frame.error_code & 16 != 0;
        if let Some(a) = &cur.aspace {
            if a.handle_fault(addr, write, exec) {
                return;
            }
        }
        crate::kerror!(
            "{} (thread {}): page fault at {:#x} ({} {}), rip {:#x}",
            cur.process_name(),
            cur.koid,
            addr,
            if exec { "execute" } else if write { "write" } else { "read" },
            if frame.error_code & 1 != 0 { "protection violation" } else { "not mapped" },
            frame.rip
        );
    } else {
        crate::kerror!(
            "{} (thread {}): {} (error {:#x}) at rip {:#x}, rsp {:#x}",
            cur.process_name(),
            cur.koid,
            idt::EXCEPTION_NAMES[vector],
            frame.error_code,
            frame.rip,
            frame.rsp
        );
    }
    match &cur.process {
        Some(p) => p.kill(vabi::EXIT_CODE_CRASHED),
        None => panic!("kernel thread took a user exception"),
    }
    drop(cur);
    exit_current();
}

/// The idle loop of a CPU. Entered with the BKL held.
pub fn idle_loop() -> ! {
    loop {
        schedule();
        // Nothing to run: halt until the next interrupt.
        bkl::release();
        cpu::enable_interrupts_and_halt();
        bkl::acquire();
    }
}

/// Entry point for idle threads of application processors.
pub extern "sysv64" fn ap_idle_entry(_arg: u64) -> ! {
    idle_loop()
}

/// Snapshot used by `system_info`.
pub fn total_idle_ns() -> u64 {
    percpu::online().map(|p| p.idle_ns.load(Ordering::Relaxed)).sum()
}

/// Threads currently queued (for diagnostics).
pub fn queued_threads() -> Vec<u64> {
    let rq = RUNQ.lock();
    rq.levels.iter().flat_map(|l| l.iter().map(|t| t.koid)).collect()
}
