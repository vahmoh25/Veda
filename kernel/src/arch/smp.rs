//! Bringing up application processors (APs).
//!
//! Each AP is started with the INIT-SIPI-SIPI sequence. It begins in 16-bit
//! real mode at the trampoline copied to physical address 0x8000, switches
//! through 32-bit protected mode into long mode using a temporary page table
//! that identity-maps the trampoline and shares the kernel half, and then
//! jumps to [`ap_entry`] on its idle thread's stack.

use alloc::sync::Arc;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, Ordering};

use super::{apic, cpu, gdt, idt, percpu};
use crate::mm::{paging, phys, phys_to_virt};
use crate::sched::{self, Thread};
use crate::sync::{SpinLock, bkl};

const TRAMPOLINE_PHYS: u64 = 0x8000;

global_asm!(
    r#"
    .section .text
    .code16
    .globl vk_ap_trampoline_start
vk_ap_trampoline_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    lgdtl 0x8000 + (ap_gdt_ptr - vk_ap_trampoline_start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x08, $(0x8000 + ap_pm32 - vk_ap_trampoline_start)

    .code32
ap_pm32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %cr4, %eax
    orl $0xA0, %eax
    movl %eax, %cr4
    movl (0x8000 + ap_slot_cr3 - vk_ap_trampoline_start), %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx
    rdmsr
    orl $0x901, %eax
    wrmsr
    movl %cr0, %eax
    orl $0x80010000, %eax
    movl %eax, %cr0
    ljmpl $0x18, $(0x8000 + ap_lm64 - vk_ap_trampoline_start)

    .code64
ap_lm64:
    movq (0x8000 + ap_slot_stack - vk_ap_trampoline_start), %rsp
    movq (0x8000 + ap_slot_arg - vk_ap_trampoline_start), %rdi
    movq (0x8000 + ap_slot_entry - vk_ap_trampoline_start), %rax
    jmpq *%rax

    .balign 8
ap_gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .quad 0x00AF9A000000FFFF
ap_gdt_ptr:
    .word 31
    .long 0x8000 + (ap_gdt - vk_ap_trampoline_start)
    .balign 8
ap_slot_cr3:
    .quad 0
ap_slot_stack:
    .quad 0
ap_slot_entry:
    .quad 0
ap_slot_arg:
    .quad 0
    .globl vk_ap_trampoline_end
vk_ap_trampoline_end:
"#,
    options(att_syntax)
);

unsafe extern "C" {
    static vk_ap_trampoline_start: u8;
    static vk_ap_trampoline_end: u8;
}

/// Idle threads created by the BSP for each AP.
static IDLE_THREADS: SpinLock<[Option<Arc<Thread>>; percpu::MAX_CPUS]> = SpinLock::new([const { None }; percpu::MAX_CPUS]);
static STARTED: AtomicU32 = AtomicU32::new(0);

/// Programs the syscall MSRs on the current CPU.
pub fn init_syscall_msrs() {
    // SAFETY: standard SYSCALL/SYSRET configuration; the selectors match
    // the GDT layout in `gdt`.
    unsafe {
        cpu::wrmsr(cpu::MSR_STAR, ((gdt::KERNEL_CS as u64) << 32) | (0x10u64 << 48));
        cpu::wrmsr(cpu::MSR_LSTAR, super::entry::vk_syscall_entry as usize as u64);
        // Mask IF, TF, DF, AC and NT on entry.
        cpu::wrmsr(cpu::MSR_SFMASK, 0x0004_4700);
    }
}

/// Allocates the IST stacks (double fault, NMI, machine check) of a CPU.
pub fn init_ist(id: usize) {
    let p = percpu::get(id);
    for slot in 0..3 {
        let stack = crate::mm::kvirt::KernelStack::new(4).expect("allocating an IST stack");
        // SAFETY: only this CPU's bring-up code writes its TSS.
        unsafe { (*p.tss.get()).ist[slot] = stack.top() };
        core::mem::forget(stack); // lives forever
    }
}

/// Per-CPU initialisation shared by the BSP and APs (after GDT/GS setup).
pub fn init_this_cpu(id: usize) {
    idt::load();
    cpu::init_cpu_features();
    init_syscall_msrs();
    init_ist(id);
}

extern "sysv64" fn ap_entry(index: u64) -> ! {
    let id = index as usize;
    let p = percpu::get(id);
    // SAFETY: per-CPU structures of this CPU, initialised exactly once.
    unsafe {
        gdt::load(p.gdt.get(), p.tss.get());
        percpu::install(id, p.apic_id);
        cpu::write_cr3(paging::kernel_pml4());
    }
    init_this_cpu(id);
    apic::init_local();
    let idle = IDLE_THREADS.lock()[id].take().expect("AP started without an idle thread");
    sched::init_cpu(idle);
    p.online.store(true, Ordering::Release);
    STARTED.fetch_add(1, Ordering::AcqRel);
    cpu::ONLINE_CPUS.fetch_add(1, Ordering::AcqRel);
    bkl::acquire();
    sched::idle_loop()
}

/// Starts every AP listed in the MADT. Returns the number started.
pub fn start_aps(max_cpus: usize) -> usize {
    let acpi = crate::acpi::ACPI.expect();
    let bsp = apic::id();
    let targets: alloc::vec::Vec<u32> =
        acpi.cpus.iter().map(|c| c.apic_id).filter(|&a| a != bsp).take(max_cpus.saturating_sub(1)).collect();
    if targets.is_empty() {
        return 0;
    }

    // Temporary page table below 4 GiB: identity-map the first 2 MiB and
    // share the kernel half.
    let pml4 = phys::alloc_contiguous(1, 4096, 1 << 32).expect("no low memory for the AP page table");
    let pdpt = phys::alloc_zeroed().unwrap();
    let pd = phys::alloc_zeroed().unwrap();
    // SAFETY: freshly allocated table pages, direct-mapped.
    unsafe {
        let k = &*(phys_to_virt(paging::kernel_pml4()) as *const [u64; 512]);
        let t = &mut *(phys_to_virt(pml4) as *mut [u64; 512]);
        t[256..].copy_from_slice(&k[256..]);
        t[0] = pdpt | paging::PRESENT | paging::WRITABLE;
        (*(phys_to_virt(pdpt) as *mut [u64; 512]))[0] = pd | paging::PRESENT | paging::WRITABLE;
        (*(phys_to_virt(pd) as *mut [u64; 512]))[0] = paging::PRESENT | paging::WRITABLE | paging::HUGE;
    }

    // Copy the trampoline to 0x8000 (below 1 MiB, reserved from the allocator).
    // SAFETY: linker symbols delimiting the trampoline code.
    let (start, end) = unsafe {
        (core::ptr::addr_of!(vk_ap_trampoline_start) as usize, core::ptr::addr_of!(vk_ap_trampoline_end) as usize)
    };
    let len = end - start;
    let dst = phys_to_virt(TRAMPOLINE_PHYS) as *mut u8;
    // SAFETY: 0x8000 is conventional memory below the allocator's floor.
    unsafe { core::ptr::copy_nonoverlapping(start as *const u8, dst, len) };
    let slot = |sym: usize| (dst as usize + (sym - start)) as *mut u64;
    let slots_base = end - 32; // cr3, stack, entry, arg (8 bytes each)

    for (i, &apic_id) in targets.iter().enumerate() {
        let id = i + 1;
        if id >= percpu::MAX_CPUS {
            break;
        }
        let Some(idle) = Thread::new_kernel("idle", sched::ap_idle_entry, 0, true) else { break };
        let stack_top = idle.kernel_stack_top;
        IDLE_THREADS.lock()[id] = Some(idle);
        // SAFETY: AP not running yet; only we write its per-CPU block.
        unsafe { percpu::set_apic_id(id, apic_id) };
        // SAFETY: writing the trampoline's data slots.
        unsafe {
            slot(slots_base).write_volatile(pml4);
            slot(slots_base + 8).write_volatile(stack_top - 8);
            slot(slots_base + 16).write_volatile(ap_entry as usize as u64);
            slot(slots_base + 24).write_volatile(id as u64);
        }
        let before = STARTED.load(Ordering::Acquire);
        apic::send_ipi(apic_id, apic::ICR_INIT);
        crate::time::busy_wait_ms(10);
        for _ in 0..2 {
            apic::send_ipi(apic_id, apic::ICR_STARTUP | (TRAMPOLINE_PHYS >> 12) as u32);
            crate::time::udelay(300);
            if STARTED.load(Ordering::Acquire) != before {
                break;
            }
        }
        // Allow a generous delay under emulation.
        let deadline = crate::time::now_ns() + 1_000_000_000;
        while STARTED.load(Ordering::Acquire) == before && crate::time::now_ns() < deadline {
            core::hint::spin_loop();
        }
        if STARTED.load(Ordering::Acquire) == before {
            crate::kwarn!("smp: CPU with APIC id {} did not start", apic_id);
            IDLE_THREADS.lock()[id] = None;
        }
    }
    STARTED.load(Ordering::Acquire) as usize
}
