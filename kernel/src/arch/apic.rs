//! Local APIC (x2APIC or memory-mapped xAPIC), I/O APIC and legacy PIC.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::cpu::{self, rdmsr, wrmsr};
use super::idt::{IOAPIC_VECTOR_BASE, SPURIOUS_VECTOR, TIMER_VECTOR};
use super::port::outb;
use crate::sync::SpinLock;

static X2APIC: AtomicBool = AtomicBool::new(false);
/// Virtual address of the xAPIC MMIO window (when not in x2APIC mode).
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0);

const REG_ID: u32 = 0x20;
const REG_EOI: u32 = 0xB0;
const REG_SVR: u32 = 0xF0;
const REG_ESR: u32 = 0x280;
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;
const REG_TIMER_INIT: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3E0;

fn read(reg: u32) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        rdmsr(0x800 + (reg >> 4)) as u32
    } else {
        let base = XAPIC_BASE.load(Ordering::Relaxed);
        // SAFETY: the xAPIC window is mapped uncached at `base`.
        unsafe { core::ptr::read_volatile((base + reg as u64) as *const u32) }
    }
}

fn write(reg: u32, value: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: valid x2APIC MSR.
        unsafe { wrmsr(0x800 + (reg >> 4), value as u64) };
    } else {
        let base = XAPIC_BASE.load(Ordering::Relaxed);
        // SAFETY: the xAPIC window is mapped uncached at `base`.
        unsafe { core::ptr::write_volatile((base + reg as u64) as *mut u32, value) };
    }
}

/// Masks and remaps the 8259 PICs so they never deliver interrupts.
pub fn disable_pic() {
    // SAFETY: standard 8259 programming sequence.
    unsafe {
        outb(0x20, 0x11);
        outb(0xA0, 0x11);
        outb(0x21, 0x20);
        outb(0xA1, 0x28);
        outb(0x21, 0x04);
        outb(0xA1, 0x02);
        outb(0x21, 0x01);
        outb(0xA1, 0x01);
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);
    }
}

/// Selects x2APIC or xAPIC mode (BSP only, before [`init_local`]).
pub fn select_mode(xapic_phys: u64) {
    if cpu::features().x2apic {
        X2APIC.store(true, Ordering::Relaxed);
    } else {
        let virt = crate::mm::kvirt::map_mmio(xapic_phys, 4096, crate::mm::paging::Cache::Uncached);
        XAPIC_BASE.store(virt, Ordering::Relaxed);
    }
}

pub fn is_x2apic() -> bool {
    X2APIC.load(Ordering::Relaxed)
}

/// Enables and configures the local APIC of the current CPU.
pub fn init_local() {
    // SAFETY: enabling the APIC (and x2APIC mode when selected).
    unsafe {
        let mut base = rdmsr(cpu::MSR_APIC_BASE) | (1 << 11);
        if is_x2apic() {
            base |= 1 << 10;
        }
        wrmsr(cpu::MSR_APIC_BASE, base);
    }
    write(REG_SVR, 0x100 | SPURIOUS_VECTOR as u32);
    write(REG_LVT_LINT0, 1 << 16);
    write(REG_LVT_LINT1, 1 << 16);
    write(REG_LVT_ERROR, 1 << 16);
    write(REG_ESR, 0);
    write(REG_LVT_TIMER, 1 << 16);
    write(0x80, 0); // TPR: accept everything
    eoi();
}

pub fn id() -> u32 {
    let raw = read(REG_ID);
    if is_x2apic() { raw } else { raw >> 24 }
}

#[inline]
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Timer programming mode chosen at calibration time.
static TSC_DEADLINE_MODE: AtomicBool = AtomicBool::new(false);
/// LAPIC timer ticks per millisecond (divide-by-16), when not in TSC mode.
static TICKS_PER_MS: AtomicU64 = AtomicU64::new(0);

pub fn use_tsc_deadline(enable: bool) {
    TSC_DEADLINE_MODE.store(enable, Ordering::Relaxed);
}

/// Measures the LAPIC timer frequency with `wait_ms` (a calibrated busy-wait).
pub fn calibrate_timer(wait_ms: &dyn Fn(u64)) {
    write(REG_TIMER_DIVIDE, 0b0011); // divide by 16
    write(REG_LVT_TIMER, 1 << 16);
    write(REG_TIMER_INIT, u32::MAX);
    wait_ms(10);
    let elapsed = u32::MAX - read(REG_TIMER_CURRENT);
    write(REG_TIMER_INIT, 0);
    TICKS_PER_MS.store((elapsed / 10).max(1) as u64, Ordering::Relaxed);
}

/// Arms the timer to fire once at TSC value `deadline_tsc` (TSC-deadline
/// mode) or after `delta_ns` nanoseconds (one-shot mode).
pub fn arm_timer(deadline_tsc: u64, delta_ns: u64) {
    if TSC_DEADLINE_MODE.load(Ordering::Relaxed) {
        write(REG_LVT_TIMER, TIMER_VECTOR as u32 | (0b10 << 17));
        // SAFETY: TSC_DEADLINE is supported in this mode.
        unsafe { wrmsr(cpu::MSR_TSC_DEADLINE, deadline_tsc.max(1)) };
    } else {
        let ticks =
            (delta_ns.saturating_mul(TICKS_PER_MS.load(Ordering::Relaxed)) / 1_000_000).clamp(1, u32::MAX as u64);
        write(REG_TIMER_DIVIDE, 0b0011);
        write(REG_LVT_TIMER, TIMER_VECTOR as u32);
        write(REG_TIMER_INIT, ticks as u32);
    }
}

/// ICR delivery modes.
pub const ICR_INIT: u32 = 0b101 << 8;
pub const ICR_STARTUP: u32 = 0b110 << 8;
const ICR_ASSERT: u32 = 1 << 14;
const ICR_DELIVERY_PENDING: u32 = 1 << 12;
/// Sends an inter-processor interrupt. What was stored before is seen by
/// its target's handler.
pub fn send_ipi(dest_apic: u32, low: u32) {
    if is_x2apic() {
        // SAFETY: the fences only order; then an x2APIC ICR write. Writing
        // the ICR does not wait for earlier stores (it is not serializing):
        // MFENCE drains them, LFENCE keeps the WRMSR after it.
        unsafe {
            core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
            wrmsr(0x830, ((dest_apic as u64) << 32) | (low | ICR_ASSERT) as u64);
        }
    } else {
        write(REG_ICR_HIGH, dest_apic << 24);
        write(REG_ICR_LOW, low | ICR_ASSERT);
        while read(REG_ICR_LOW) & ICR_DELIVERY_PENDING != 0 {
            core::hint::spin_loop();
        }
    }
}

/// An I/O APIC.
struct IoApic {
    base: u64,
    gsi_base: u32,
    count: u32,
}

impl IoApic {
    fn read(&self, reg: u32) -> u32 {
        // SAFETY: the window is mapped uncached.
        unsafe {
            core::ptr::write_volatile(self.base as *mut u32, reg);
            core::ptr::read_volatile((self.base + 0x10) as *const u32)
        }
    }

    fn write(&self, reg: u32, value: u32) {
        // SAFETY: the window is mapped uncached.
        unsafe {
            core::ptr::write_volatile(self.base as *mut u32, reg);
            core::ptr::write_volatile((self.base + 0x10) as *mut u32, value);
        }
    }
}

static IOAPICS: SpinLock<heapless_vec::Vec4<IoApic>> = SpinLock::new(heapless_vec::Vec4::new());

/// Tiny fixed-capacity vector (no allocation needed this early).
mod heapless_vec {
    pub struct Vec4<T> {
        items: [Option<T>; 4],
    }
    impl<T> Vec4<T> {
        pub const fn new() -> Self {
            Vec4 { items: [None, None, None, None] }
        }
        pub fn push(&mut self, v: T) {
            if let Some(slot) = self.items.iter_mut().find(|s| s.is_none()) {
                *slot = Some(v);
            }
        }
        pub fn iter(&self) -> impl Iterator<Item = &T> {
            self.items.iter().flatten()
        }
    }
}

/// Registers an I/O APIC from the MADT and masks all of its inputs.
pub fn add_ioapic(phys: u64, gsi_base: u32) {
    let base = crate::mm::kvirt::map_mmio(phys, 4096, crate::mm::paging::Cache::Uncached);
    let mut io = IoApic { base, gsi_base, count: 0 };
    io.count = ((io.read(1) >> 16) & 0xFF) + 1;
    for i in 0..io.count {
        io.write(0x10 + 2 * i, 1 << 16);
        io.write(0x11 + 2 * i, 0);
    }
    crate::kinfo!("ioapic: {} inputs at GSI {} (phys {:#x})", io.count, gsi_base, phys);
    IOAPICS.lock().push(io);
}

/// Routes `gsi` to vector `IOAPIC_VECTOR_BASE + gsi` on the BSP.
pub fn route_gsi(gsi: u32, level: bool, active_low: bool, masked: bool) -> bool {
    let ioapics = IOAPICS.lock();
    let Some(io) = ioapics.iter().find(|io| gsi >= io.gsi_base && gsi < io.gsi_base + io.count) else {
        return false;
    };
    let pin = gsi - io.gsi_base;
    let mut low = (IOAPIC_VECTOR_BASE as u32 + gsi) & 0xFF;
    if active_low {
        low |= 1 << 13;
    }
    if level {
        low |= 1 << 15;
    }
    if masked {
        low |= 1 << 16;
    }
    let dest = super::percpu::get(0).apic_id;
    io.write(0x11 + 2 * pin, dest << 24);
    io.write(0x10 + 2 * pin, low);
    true
}

/// Masks or unmasks a GSI.
pub fn set_gsi_masked(gsi: u32, masked: bool) {
    let ioapics = IOAPICS.lock();
    if let Some(io) = ioapics.iter().find(|io| gsi >= io.gsi_base && gsi < io.gsi_base + io.count) {
        let reg = 0x10 + 2 * (gsi - io.gsi_base);
        let v = io.read(reg);
        io.write(reg, if masked { v | (1 << 16) } else { v & !(1 << 16) });
    }
}

/// Address/data pair that makes a PCI device's MSI target `vector` on the BSP.
pub fn msi_message(vector: u8) -> (u64, u32) {
    let dest = super::percpu::get(0).apic_id;
    (0xFEE0_0000 | ((dest as u64 & 0xFF) << 12), vector as u32)
}
