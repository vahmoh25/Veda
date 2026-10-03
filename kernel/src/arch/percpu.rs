//! Per-CPU data, reached through the `GS` segment base while in the kernel.
//!
//! The first fields have fixed offsets because the assembly entry code uses
//! them (`gs:[OFFSET]`).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::gdt::{Gdt, Tss};
use crate::sched::CpuSched;

pub const MAX_CPUS: usize = 32;

/// Offsets used by assembly.
pub const OFF_USER_RSP: usize = 8;
pub const OFF_KERNEL_RSP: usize = 16;

#[repr(C, align(64))]
pub struct PerCpu {
    /// Pointer to this structure (`gs:[0]`).
    self_ptr: *const PerCpu,
    /// Scratch slot for the user stack pointer during `syscall` entry.
    pub user_rsp: u64,
    /// Top of the running thread's kernel stack (loaded on `syscall` entry).
    pub kernel_rsp: u64,
    pub cpu_id: u32,
    pub apic_id: u32,
    pub gdt: UnsafeCell<Gdt>,
    pub tss: UnsafeCell<Tss>,
    /// Scheduler state owned by this CPU.
    pub sched: UnsafeCell<CpuSched>,
    /// Set by another CPU that wants this one to flush its TLB.
    pub tlb_flush_pending: AtomicBool,
    /// Acknowledgement counter for TLB shootdowns.
    pub tlb_flush_done: AtomicU64,
    pub online: AtomicBool,
    /// Accumulated time spent in the idle thread (ns).
    pub idle_ns: AtomicU64,
    /// This idle CPU was sent a reschedule IPI and has not run the
    /// scheduler since: further wake-ups pick another CPU.
    pub resched_pending: AtomicBool,
}

// SAFETY: each PerCpu is mutated only by its own CPU (or under the BKL);
// cross-CPU fields are atomics.
unsafe impl Sync for PerCpu {}

const fn new_percpu(id: u32) -> PerCpu {
    PerCpu {
        self_ptr: core::ptr::null(),
        user_rsp: 0,
        kernel_rsp: 0,
        cpu_id: id,
        apic_id: 0,
        gdt: UnsafeCell::new(Gdt::new()),
        tss: UnsafeCell::new(Tss::new()),
        sched: UnsafeCell::new(CpuSched::new()),
        tlb_flush_pending: AtomicBool::new(false),
        tlb_flush_done: AtomicU64::new(0),
        online: AtomicBool::new(false),
        idle_ns: AtomicU64::new(0),
        resched_pending: AtomicBool::new(false),
    }
}

struct PerCpuArray([PerCpu; MAX_CPUS]);
// SAFETY: see `PerCpu`.
unsafe impl Sync for PerCpuArray {}

static PERCPU: PerCpuArray = {
    let mut i = 0;
    // `[expr; N]` needs Copy, so build the array element by element.
    let mut arr: [core::mem::MaybeUninit<PerCpu>; MAX_CPUS] = [const { core::mem::MaybeUninit::uninit() }; MAX_CPUS];
    while i < MAX_CPUS {
        arr[i] = core::mem::MaybeUninit::new(new_percpu(i as u32));
        i += 1;
    }
    // SAFETY: every element was initialised above.
    PerCpuArray(unsafe { core::mem::transmute::<[core::mem::MaybeUninit<PerCpu>; MAX_CPUS], [PerCpu; MAX_CPUS]>(arr) })
};

static GS_READY: AtomicBool = AtomicBool::new(false);

/// Points this CPU's `GS` base at its per-CPU block. Must run before anything
/// else touches per-CPU state.
///
/// # Safety
/// Called once per CPU, with `id` unique.
pub unsafe fn install(id: usize, apic_id: u32) {
    let p = &PERCPU.0[id];
    let ptr = p as *const PerCpu as *mut PerCpu;
    // SAFETY: this CPU is the only user of its slot during bring-up.
    unsafe {
        (*ptr).self_ptr = ptr;
        (*ptr).apic_id = apic_id;
        super::cpu::wrmsr(super::cpu::MSR_GS_BASE, ptr as u64);
        super::cpu::wrmsr(super::cpu::MSR_KERNEL_GS_BASE, 0);
    }
    GS_READY.store(true, Ordering::Release);
}

/// The current CPU's per-CPU block.
#[inline]
pub fn current() -> &'static PerCpu {
    let p: *const PerCpu;
    // SAFETY: GS points at our PerCpu while in kernel mode.
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) p, options(nostack, preserves_flags, readonly)) };
    // SAFETY: the pointer refers to a static.
    unsafe { &*p }
}

/// Records the APIC id of CPU `id` before it is started.
///
/// # Safety
/// The CPU must not be running yet.
pub unsafe fn set_apic_id(id: usize, apic_id: u32) {
    let p = &PERCPU.0[id] as *const PerCpu as *mut PerCpu;
    // SAFETY: guaranteed by the caller.
    unsafe { (*p).apic_id = apic_id };
}

/// Current CPU index, or 0 before per-CPU data is installed.
#[inline]
pub fn cpu_id_or_boot() -> u32 {
    if !GS_READY.load(Ordering::Relaxed) {
        return 0;
    }
    let id: u32;
    // SAFETY: GS points at our PerCpu while in kernel mode.
    unsafe { core::arch::asm!("mov {:e}, gs:[24]", out(reg) id, options(nostack, preserves_flags, readonly)) };
    id
}

/// Per-CPU block of CPU `id`.
pub fn get(id: usize) -> &'static PerCpu {
    &PERCPU.0[id]
}

/// Iterates over all CPUs that have come online.
pub fn online() -> impl Iterator<Item = &'static PerCpu> {
    PERCPU.0.iter().filter(|p| p.online.load(Ordering::Acquire))
}

/// Updates the stack used on entry from user mode (syscall and interrupts).
pub fn set_kernel_stack(top: u64) {
    let p = current() as *const PerCpu as *mut PerCpu;
    // SAFETY: only this CPU writes its own entry-stack fields, with
    // interrupts disabled.
    unsafe {
        (*p).kernel_rsp = top;
        (*(*p).tss.get()).rsp[0] = top;
    }
}
