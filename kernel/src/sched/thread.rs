//! Threads: the unit of scheduling.

use alloc::alloc::{alloc_zeroed, dealloc};
use alloc::sync::Arc;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::cpu;
use crate::arch::entry::{TrapFrame, vk_kernel_thread_trampoline, vk_thread_trampoline};
use crate::mm::aspace::AddressSpace;
use crate::mm::kvirt::KernelStack;
use crate::object::process::Process;
use crate::object::{Name, Signals};
use crate::sync::SpinLock;

/// Kernel stack size of every thread.
pub const KERNEL_STACK_PAGES: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Created but not started.
    New,
    Ready,
    Running,
    Blocked,
    Dead,
}

/// Why a blocked thread was woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    None,
    Signaled,
    TimedOut,
    Killed,
}

pub struct SchedData {
    pub state: State,
    pub priority: u8,
    pub wake_reason: WakeReason,
    /// Key in the timeout queue while blocked with a deadline.
    pub timeout_key: Option<(u64, u64)>,
    pub cpu: u32,
}

/// FPU/SSE/AVX register save area (64-byte aligned for XSAVE).
struct FpuArea {
    ptr: *mut u8,
    layout: Layout,
}

impl FpuArea {
    fn new() -> Option<FpuArea> {
        let size = cpu::features().xsave_size.max(512) as usize;
        let layout = Layout::from_size_align(size, 64).ok()?;
        // SAFETY: non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        // Default x87 control word and MXCSR (all exceptions masked). With
        // XSTATE_BV = 0 everything else starts in its initial state.
        // SAFETY: inside the freshly allocated area.
        unsafe {
            (ptr as *mut u16).write(0x037F);
            (ptr.add(24) as *mut u32).write(0x1F80);
        }
        Some(FpuArea { ptr, layout })
    }

    fn save(&self) {
        let f = cpu::features();
        // SAFETY: the area is large enough and 64-byte aligned.
        unsafe {
            if f.xsave {
                core::arch::asm!("xsave64 [{}]", in(reg) self.ptr, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack));
            } else {
                core::arch::asm!("fxsave64 [{}]", in(reg) self.ptr, options(nostack));
            }
        }
    }

    fn restore(&self) {
        let f = cpu::features();
        // SAFETY: the area holds a valid saved (or initial) state.
        unsafe {
            if f.xsave {
                core::arch::asm!("xrstor64 [{}]", in(reg) self.ptr, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack));
            } else {
                core::arch::asm!("fxrstor64 [{}]", in(reg) self.ptr, options(nostack));
            }
        }
    }
}

impl Drop for FpuArea {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with the same layout.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// Mutable execution context, touched only by the CPU switching the thread
/// in or out (under the BKL).
pub struct Context {
    pub rsp: u64,
    pub fs_base: u64,
    fpu: Option<FpuArea>,
}

pub struct Thread {
    pub koid: u64,
    pub name: Name,
    pub process: Option<Arc<Process>>,
    pub aspace: Option<Arc<AddressSpace>>,
    kstack: Option<KernelStack>,
    pub kernel_stack_top: u64,
    pub sched: SpinLock<SchedData>,
    ctx: UnsafeCell<Context>,
    pub signals: Signals,
    pub cpu_time_ns: AtomicU64,
    pub kill_pending: AtomicBool,
    pub is_idle: bool,
}

// SAFETY: `ctx` is only accessed by the scheduler under the BKL; everything
// else is synchronised.
unsafe impl Sync for Thread {}
// SAFETY: as above.
unsafe impl Send for Thread {}

impl Thread {
    fn build(
        name: &str,
        process: Option<Arc<Process>>,
        aspace: Option<Arc<AddressSpace>>,
        kstack: Option<KernelStack>,
        is_idle: bool,
    ) -> Option<Arc<Thread>> {
        let top = kstack.as_ref().map(|s| s.top()).unwrap_or(0);
        Some(Arc::new(Thread {
            koid: crate::object::new_koid(),
            name: Name::new(name),
            process,
            aspace,
            kstack,
            kernel_stack_top: top,
            sched: SpinLock::new(SchedData {
                state: State::New,
                priority: vabi::priority::NORMAL as u8,
                wake_reason: WakeReason::None,
                timeout_key: None,
                cpu: 0,
            }),
            ctx: UnsafeCell::new(Context { rsp: 0, fs_base: 0, fpu: if is_idle { None } else { Some(FpuArea::new()?) } }),
            signals: Signals::new(0),
            cpu_time_ns: AtomicU64::new(0),
            kill_pending: AtomicBool::new(false),
            is_idle,
        }))
    }

    /// A thread that will run user code in `process`.
    pub fn new_user(process: &Arc<Process>, name: &str) -> Option<Arc<Thread>> {
        let aspace = process.aspace()?;
        let stack = KernelStack::new(KERNEL_STACK_PAGES)?;
        Thread::build(name, Some(process.clone()), Some(aspace), Some(stack), false)
    }

    /// The idle thread representing the code already running on this CPU
    /// (its context is captured at the first switch away from it).
    pub fn new_idle_current(cpu: u32) -> Arc<Thread> {
        let t = Thread::build("idle", None, None, None, true).expect("out of memory creating the idle thread");
        {
            let mut s = t.sched.lock();
            s.state = State::Running;
            s.priority = 0;
            s.cpu = cpu;
        }
        t
    }

    /// A kernel thread that starts executing `entry(arg)`.
    pub fn new_kernel(name: &str, entry: extern "sysv64" fn(u64) -> !, arg: u64, idle: bool) -> Option<Arc<Thread>> {
        let stack = KernelStack::new(KERNEL_STACK_PAGES)?;
        let t = Thread::build(name, None, None, Some(stack), idle)?;
        // Initial frame popped by vk_context_switch: r15 r14 r13 r12 rbx rbp ret.
        let top = t.kernel_stack_top;
        let frame = [0u64, 0, arg, entry as usize as u64, 0, 0, vk_kernel_thread_trampoline as usize as u64, 0];
        let rsp = top - 16 - 7 * 8; // keeps rsp 16-aligned after `ret`
        // SAFETY: writing the initial frame into our own fresh stack.
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), rsp as *mut u64, 7);
            (*t.ctx.get()).rsp = rsp;
        }
        if idle {
            t.sched.lock().priority = 0;
        }
        Some(t)
    }

    /// Prepares the kernel stack so that the first switch to this thread
    /// enters user mode at `entry` with the given stack and arguments.
    pub fn prepare_user_entry(&self, entry: u64, user_stack: u64, arg0: u64, arg1: u64) {
        let top = self.kernel_stack_top;
        let tf_addr = top - core::mem::size_of::<TrapFrame>() as u64;
        let tf = TrapFrame::new_user(entry, user_stack, arg0, arg1);
        let rsp = tf_addr - 7 * 8;
        let regs = [0u64, 0, 0, 0, 0, 0, vk_thread_trampoline as usize as u64];
        // SAFETY: the thread has not run yet; we own its stack.
        unsafe {
            (tf_addr as *mut TrapFrame).write(tf);
            core::ptr::copy_nonoverlapping(regs.as_ptr(), rsp as *mut u64, 7);
            (*self.ctx.get()).rsp = rsp;
        }
    }

    pub fn state(&self) -> State {
        self.sched.lock().state
    }

    pub fn priority(&self) -> u8 {
        self.sched.lock().priority
    }

    /// # Safety
    /// Only the scheduler may call this, under the BKL.
    pub unsafe fn ctx(&self) -> &mut Context {
        // SAFETY: forwarded to the caller.
        unsafe { &mut *self.ctx.get() }
    }

    pub fn save_fpu(&self) {
        // SAFETY: called by the scheduler on the CPU running this thread.
        if let Some(f) = unsafe { &(*self.ctx.get()).fpu } {
            f.save();
        }
    }

    pub fn restore_fpu(&self) {
        // SAFETY: as above.
        if let Some(f) = unsafe { &(*self.ctx.get()).fpu } {
            f.restore();
        }
    }

    pub fn process_name(&self) -> &str {
        self.process.as_ref().map(|p| p.name.as_str()).unwrap_or("kernel")
    }

    pub fn is_dead(&self) -> bool {
        self.state() == State::Dead
    }

    pub fn mark_kill(&self) {
        self.kill_pending.store(true, Ordering::Release);
    }
}
