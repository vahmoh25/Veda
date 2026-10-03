//! Timekeeping.
//!
//! The monotonic clock is derived from the TSC, calibrated at boot against
//! the HPET (or the legacy PIT when no HPET exists). Wall-clock time is the
//! firmware RTC reading taken by the loader, advanced by the monotonic clock.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::arch::cpu::rdtsc;
use crate::arch::port::{inb, outb};

static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC tick in 32.32 fixed point.
static NS_PER_TICK_FP: AtomicU64 = AtomicU64::new(0);
/// Seconds since the Unix epoch (local time) at `TSC_BOOT`.
static BOOT_EPOCH_SECS: AtomicI64 = AtomicI64::new(0);
static HPET_BASE: AtomicU64 = AtomicU64::new(0);
static HPET_PERIOD_FS: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds since the clock was calibrated (0 before that).
#[inline]
pub fn now_ns() -> u64 {
    let mult = NS_PER_TICK_FP.load(Ordering::Relaxed);
    if mult == 0 {
        return 0;
    }
    let ticks = rdtsc().saturating_sub(TSC_BOOT.load(Ordering::Relaxed));
    ((ticks as u128 * mult as u128) >> 32) as u64
}

/// Converts a monotonic nanosecond timestamp into a TSC value.
pub fn ns_to_tsc(ns: u64) -> u64 {
    let hz = TSC_HZ.load(Ordering::Relaxed);
    TSC_BOOT.load(Ordering::Relaxed).saturating_add(((ns as u128 * hz as u128) / 1_000_000_000) as u64)
}

pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Wall-clock time in nanoseconds since the Unix epoch.
pub fn realtime_ns() -> u64 {
    (BOOT_EPOCH_SECS.load(Ordering::Relaxed).max(0) as u64) * 1_000_000_000 + now_ns()
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn set_boot_time(t: &bootinfo::BootTime) {
    if t.valid == 0 {
        return;
    }
    let days = days_from_civil(t.year as i64, t.month as i64, t.day as i64);
    let secs = days * 86_400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64;
    BOOT_EPOCH_SECS.store(secs, Ordering::Relaxed);
}

/// Registers the HPET found in the ACPI tables.
pub fn set_hpet(phys: u64) {
    let base = crate::mm::kvirt::map_mmio(phys, 4096, crate::mm::paging::Cache::Uncached);
    // SAFETY: the HPET register block is mapped uncached.
    unsafe {
        let caps = core::ptr::read_volatile(base as *const u64);
        HPET_PERIOD_FS.store(caps >> 32, Ordering::Relaxed);
        // Enable the main counter (legacy replacement off).
        let cfg = (base + 0x10) as *mut u64;
        core::ptr::write_volatile(cfg, (core::ptr::read_volatile(cfg) & !0b10) | 1);
    }
    HPET_BASE.store(base, Ordering::Relaxed);
}

fn hpet_counter() -> u64 {
    // SAFETY: mapped in `set_hpet`.
    unsafe { core::ptr::read_volatile((HPET_BASE.load(Ordering::Relaxed) + 0xF0) as *const u64) }
}

/// Busy-waits using the HPET or, failing that, PIT channel 2. Usable before
/// the TSC is calibrated.
pub fn busy_wait_ms(ms: u64) {
    let period = HPET_PERIOD_FS.load(Ordering::Relaxed);
    if HPET_BASE.load(Ordering::Relaxed) != 0 && period != 0 {
        let ticks = ms * 1_000_000_000_000 / period;
        let start = hpet_counter();
        while hpet_counter().wrapping_sub(start) < ticks {
            core::hint::spin_loop();
        }
        return;
    }
    for _ in 0..ms {
        // SAFETY: programming PIT channel 2 (speaker gate) for a 1 ms one-shot.
        unsafe {
            let gate = inb(0x61) & 0xFC;
            outb(0x61, gate | 1);
            outb(0x43, 0b1011_0000);
            outb(0x42, (1193 & 0xFF) as u8);
            outb(0x42, (1193 >> 8) as u8);
            let g = inb(0x61) & 0xFE;
            outb(0x61, g);
            outb(0x61, g | 1);
            while inb(0x61) & 0x20 == 0 {
                core::hint::spin_loop();
            }
        }
    }
}

/// Measures the TSC frequency and starts the monotonic clock.
pub fn calibrate_tsc() {
    // Take the best of three 10 ms samples to reduce emulator jitter.
    let mut best = 0u64;
    for _ in 0..3 {
        let t0 = rdtsc();
        busy_wait_ms(10);
        let hz = (rdtsc() - t0) * 100;
        best = best.max(hz);
    }
    let hz = best.max(1_000_000);
    TSC_HZ.store(hz, Ordering::Relaxed);
    TSC_BOOT.store(rdtsc(), Ordering::Relaxed);
    NS_PER_TICK_FP.store(((1_000_000_000u128 << 32) / hz as u128) as u64, Ordering::Relaxed);
}

/// Busy-waits for `us` microseconds using the calibrated TSC.
pub fn udelay(us: u64) {
    let end = rdtsc() + us * TSC_HZ.load(Ordering::Relaxed) / 1_000_000;
    while rdtsc() < end {
        core::hint::spin_loop();
    }
}
