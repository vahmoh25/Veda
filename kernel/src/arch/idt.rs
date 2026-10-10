//! Interrupt descriptor table, vector assignment and the trap dispatcher.

use core::arch::asm;

use super::entry::{TrapFrame, vk_isr_stubs};
use super::gdt::{IST_DOUBLE_FAULT, IST_MACHINE_CHECK, IST_NMI, KERNEL_CS};
use crate::sync::bkl;

/// The vectors of devices' interrupts (I/O APIC inputs and MSIs), each
/// allocated when its interrupt is set up.
pub const DEVICE_VECTOR_FIRST: u8 = 0x30;
pub const DEVICE_VECTOR_LAST: u8 = 0xEF;
pub const TIMER_VECTOR: u8 = 0xF0;
pub const RESCHED_VECTOR: u8 = 0xF1;
pub const TLB_VECTOR: u8 = 0xF2;
pub const HALT_VECTOR: u8 = 0xF3;
/// Makes a CPU that runs a guest leave it (to see an interrupt for the
/// guest, or a request to its scheduler); it does nothing else.
pub const KICK_VECTOR: u8 = 0xF4;
/// The IOMMU's fault events.
pub const IOMMU_VECTOR: u8 = 0xF5;
pub const SPURIOUS_VECTOR: u8 = 0xFF;

#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_lo: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_hi: u32,
    reserved: u32,
}

impl IdtEntry {
    const EMPTY: IdtEntry =
        IdtEntry { offset_lo: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_hi: 0, reserved: 0 };

    fn new(handler: u64, ist: u8, dpl: u8) -> IdtEntry {
        IdtEntry {
            offset_lo: handler as u16,
            selector: KERNEL_CS,
            ist,
            type_attr: 0x8E | (dpl << 5), // present, interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_hi: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, align(16))]
struct Idt([IdtEntry; 256]);

static mut IDT: Idt = Idt([IdtEntry::EMPTY; 256]);

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

/// Builds the shared IDT (once, on the BSP).
pub fn init() {
    // The stubs are 16 bytes apart starting at `vk_isr_stubs`.
    let base = core::ptr::addr_of!(vk_isr_stubs) as u64;
    // SAFETY: single-threaded early boot; nothing reads the IDT yet.
    let idt = unsafe { &mut *core::ptr::addr_of_mut!(IDT) };
    for (v, entry) in idt.0.iter_mut().enumerate() {
        let ist = match v {
            8 => IST_DOUBLE_FAULT,
            2 => IST_NMI,
            18 => IST_MACHINE_CHECK,
            _ => 0,
        };
        *entry = IdtEntry::new(base + v as u64 * 16, ist, 0);
    }
}

/// Loads the IDT on the current CPU.
pub fn load() {
    let ptr = IdtPointer { limit: (core::mem::size_of::<Idt>() - 1) as u16, base: core::ptr::addr_of!(IDT) as u64 };
    // SAFETY: the IDT is fully initialised and lives forever.
    unsafe { asm!("lidt [{}]", in(reg) &ptr, options(nostack)) };
}

pub const EXCEPTION_NAMES: [&str; 32] = [
    "Divide error",
    "Debug",
    "Non-maskable interrupt",
    "Breakpoint",
    "Overflow",
    "Bound range exceeded",
    "Invalid opcode",
    "Device not available",
    "Double fault",
    "Coprocessor segment overrun",
    "Invalid TSS",
    "Segment not present",
    "Stack-segment fault",
    "General protection fault",
    "Page fault",
    "Reserved",
    "x87 floating-point exception",
    "Alignment check",
    "Machine check",
    "SIMD floating-point exception",
    "Virtualization exception",
    "Control protection exception",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Hypervisor injection exception",
    "VMM communication exception",
    "Security exception",
    "Reserved",
];

/// Entry point from `vk_interrupt_common`.
pub extern "sysv64" fn trap_dispatch(frame: &mut TrapFrame) {
    let vector = frame.vector as u8;
    match vector {
        // These never take the BKL: they must work while another CPU holds it.
        TLB_VECTOR => {
            super::tlb::handle_ipi();
            super::apic::eoi();
            return;
        }
        HALT_VECTOR => super::cpu::halt_forever(),
        KICK_VECTOR => {
            super::apic::eoi();
            return;
        }
        SPURIOUS_VECTOR | 0x20..=0x2F => return,
        2 => crate::panic::nmi(frame),
        _ => {}
    }

    if !frame.is_user() && vector < 32 {
        // A CPU exception in kernel mode is always a kernel bug.
        crate::panic::kernel_exception(frame);
    }

    bkl::acquire();
    match vector {
        0..=31 => crate::sched::user_exception(frame),
        TIMER_VECTOR => {
            super::apic::eoi();
            crate::sched::timer_interrupt();
        }
        RESCHED_VECTOR => {
            super::apic::eoi();
            crate::sched::request_resched();
        }
        IOMMU_VECTOR => {
            crate::iommu::fault_interrupt();
            super::apic::eoi();
        }
        v @ DEVICE_VECTOR_FIRST..=DEVICE_VECTOR_LAST => {
            crate::object::interrupt::dispatch(v);
            super::apic::eoi();
        }
        _ => {
            crate::kwarn!("unexpected interrupt vector {:#x}", vector);
            super::apic::eoi();
        }
    }
    if frame.is_user() {
        crate::sched::return_to_user_hook(frame);
    }
    bkl::release();
}
