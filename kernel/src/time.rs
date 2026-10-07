//! Timekeeping.
//!
//! The monotonic clock is derived from the TSC, calibrated at boot against
//! the HPET (or the legacy PIT when no HPET exists). Wall-clock time is the
//! firmware RTC reading taken by the loader, advanced by the monotonic clock.
//! The RTC keeps UTC (unless the firmware says otherwise); local time is UTC
//! plus the offset of the `tz=` boot option.

use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

use crate::arch::cpu::rdtsc;
use crate::arch::port::{inb, outb};

static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC tick in 32.32 fixed point.
static NS_PER_TICK_FP: AtomicU64 = AtomicU64::new(0);
/// Seconds since the Unix epoch (local time) at `TSC_BOOT`.
static BOOT_EPOCH_SECS: AtomicI64 = AtomicI64::new(0);
/// Local time minus UTC, in seconds.
static UTC_OFFSET_S: AtomicI64 = AtomicI64::new(0);
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

/// Wall-clock time in nanoseconds since the Unix epoch, in local time.
pub fn realtime_ns() -> u64 {
    (BOOT_EPOCH_SECS.load(Ordering::Relaxed).max(0) as u64) * 1_000_000_000 + now_ns()
}

/// Wall-clock time in nanoseconds since the Unix epoch, in UTC.
pub fn utc_ns() -> u64 {
    let offset = UTC_OFFSET_S.load(Ordering::Relaxed) * 1_000_000_000;
    (realtime_ns() as i64).saturating_sub(offset).max(0) as u64
}

/// Parses a time zone offset: `+02:00`, `-05:30`, `+0100`, `+2`, `UTC+2`.
pub fn parse_utc_offset(s: &str) -> Option<i64> {
    let s = s.strip_prefix("UTC").or_else(|| s.strip_prefix("utc")).unwrap_or(s);
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (1, s),
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
        None if rest.len() == 4 => (rest[..2].parse::<i64>().ok()?, rest[2..].parse::<i64>().ok()?),
        None => (rest.parse::<i64>().ok()?, 0),
    };
    if h > 14 || m >= 60 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
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

/// Sets the wall clock from the firmware's reading. `tz` (seconds east of
/// UTC, from the `tz=` boot option) gives local time; without it, the
/// firmware's own time zone is used, or UTC.
pub fn set_boot_time(t: &bootinfo::BootTime, tz: Option<i64>) {
    if t.valid == 0 {
        return;
    }
    let days = days_from_civil(t.year as i64, t.month as i64, t.day as i64);
    let secs = days * 86_400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64;
    let firmware_offset = if t.utc_offset_minutes == i16::MAX { 0 } else { t.utc_offset_minutes as i64 * 60 };
    let offset = tz.unwrap_or(firmware_offset);
    UTC_OFFSET_S.store(offset, Ordering::Relaxed);
    BOOT_EPOCH_SECS.store(secs - firmware_offset + offset, Ordering::Relaxed);
    let sign = if offset < 0 { '-' } else { '+' };
    crate::kinfo!(
        "time: {:04}-{:02}-{:02} {:02}:{:02} (firmware), local time UTC{}{:02}:{:02}",
        t.year,
        t.month,
        t.day,
        t.hour,
        t.minute,
        sign,
        offset.abs() / 3600,
        offset.abs() / 60 % 60
    );
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

/// One measurement of the TSC frequency over about `ms` milliseconds. With
/// an HPET the TSC ticks are divided by the time that really passed, which
/// can exceed `ms` when the (virtual) CPU is descheduled during the wait.
fn measure_tsc_hz(ms: u64) -> u64 {
    let period_fs = HPET_PERIOD_FS.load(Ordering::Relaxed);
    if HPET_BASE.load(Ordering::Relaxed) != 0 && period_fs != 0 {
        let (h0, t0) = (hpet_counter(), rdtsc());
        busy_wait_ms(ms);
        let (t1, h1) = (rdtsc(), hpet_counter());
        let elapsed_fs = (h1.wrapping_sub(h0) as u128 * period_fs as u128).max(1);
        return ((t1 - t0) as u128 * 1_000_000_000_000_000 / elapsed_fs) as u64;
    }
    // The PIT fallback cannot tell how long the wait really took.
    let t0 = rdtsc();
    busy_wait_ms(ms);
    (rdtsc() - t0) * 1000 / ms
}

/// The TSC frequency an Intel processor reports itself (CPUID leaf 0x15:
/// the TSC's ratio to the core crystal, and the crystal's frequency or,
/// where leaf 0x15 leaves it out, the base frequency of leaf 0x16, as
/// Linux reads them). Not under a hypervisor, whose TSC may run at another
/// rate than the host's processor says.
fn reported_tsc_hz() -> Option<u64> {
    use crate::arch::cpu::cpuid;
    let vendor = cpuid(0, 0);
    let intel = (vendor.ebx, vendor.edx, vendor.ecx) == (0x756E_6547, 0x4965_6E69, 0x6C65_746E);
    let hypervisor = cpuid(1, 0).ecx & (1 << 31) != 0;
    if !intel || hypervisor || vendor.eax < 0x15 {
        return None;
    }
    let ratio = cpuid(0x15, 0);
    let (denominator, numerator) = (ratio.eax as u64, ratio.ebx as u64);
    if denominator == 0 || numerator == 0 {
        return None;
    }
    let crystal_hz = match ratio.ecx as u64 {
        0 if vendor.eax >= 0x16 => cpuid(0x16, 0).eax as u64 * 1_000_000 * denominator / numerator,
        hz => hz,
    };
    (crystal_hz != 0).then(|| crystal_hz * numerator / denominator)
}

/// How the TSC frequency was found: "HPET", "PIT" or "processor".
static TSC_SOURCE: AtomicUsize = AtomicUsize::new(0);
const SOURCES: [&str; 3] = ["HPET", "PIT", "processor"];

pub fn tsc_source() -> &'static str {
    SOURCES[TSC_SOURCE.load(Ordering::Relaxed)]
}

/// Measures the TSC frequency and starts the monotonic clock. A timer that
/// measures more than 5% off what the processor reports is not believed
/// (a gated or emulated PIT can count at any speed).
pub fn calibrate_tsc() {
    // The median of five samples ignores outliers from emulator jitter.
    let mut samples = [0u64; 5];
    for s in &mut samples {
        *s = measure_tsc_hz(10);
    }
    samples.sort_unstable();
    let measured = samples[samples.len() / 2].max(1_000_000);
    let timer = if HPET_BASE.load(Ordering::Relaxed) != 0 { 0 } else { 1 };
    let (hz, source) = match reported_tsc_hz() {
        Some(reported) if measured.abs_diff(reported) > reported / 20 => {
            crate::kinfo!(
                "time: the {} measured the TSC at {} MHz; the processor says {} MHz",
                SOURCES[timer],
                measured / 1_000_000,
                reported / 1_000_000
            );
            (reported, 2)
        }
        _ => (measured, timer),
    };
    TSC_SOURCE.store(source, Ordering::Relaxed);
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

// ---- the same TSC on every processor -------------------------------------
//
// The clock is the TSC of whichever processor reads it, and timers fire when
// that processor's TSC reaches a deadline: every processor's must agree.
// They all count the same clock, each offset by its `IA32_TSC_ADJUST`, which
// firmware may leave different from processor to processor (writing a TSC
// moves it). Then the clock jumps as threads move between processors, and
// timers fire early or late by as much. So, as Linux does, the boot
// processor's adjustment is set to 0 before the clock starts, and each
// other processor's to the same as it starts; an exchange of readings with
// the boot processor then checks that they agree, and corrects what is left.

/// Sets the boot processor's TSC adjustment to 0 (before the clock starts).
/// The adjustment the firmware left, if it was not 0.
pub fn reset_boot_tsc() -> Option<i64> {
    if !crate::arch::cpu::features().tsc_adjust {
        return None;
    }
    let adjust = crate::arch::cpu::rdmsr(crate::arch::cpu::MSR_TSC_ADJUST) as i64;
    if adjust == 0 {
        return None;
    }
    // SAFETY: TSC_ADJUST exists (CPUID); nothing has used the TSC as a clock
    // yet.
    unsafe { crate::arch::cpu::wrmsr(crate::arch::cpu::MSR_TSC_ADJUST, 0) };
    Some(adjust)
}

/// Exchanges of readings per check, and how long a starting processor waits
/// for an answer before it gives up.
const SYNC_ROUNDS: u32 = 32;
const SYNC_TIMEOUT_MS: u64 = 100;

/// The exchange: a starting processor makes the step odd to ask for the boot
/// processor's TSC, which the boot processor answers by storing it and
/// making the step even again.
static SYNC_STEP: AtomicU64 = AtomicU64::new(0);
static SYNC_BOOT_TSC: AtomicU64 = AtomicU64::new(0);

/// The boot processor's part: answers a starting processor that waits for
/// its TSC. Called while it waits for the processor to start.
pub fn serve_tsc_sync() {
    let step = SYNC_STEP.load(Ordering::Acquire);
    if step % 2 == 1 {
        SYNC_BOOT_TSC.store(crate::arch::cpu::rdtsc_ordered(), Ordering::Relaxed);
        SYNC_STEP.store(step + 1, Ordering::Release);
    }
}

/// How a starting processor's TSC compared with the boot processor's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TscSync {
    /// Its adjustment as the firmware left it (set to 0 since).
    pub firmware_adjust: i64,
    /// How many ticks it was behind the boot processor's after that, and
    /// is now (negative: ahead).
    pub behind: i64,
    pub left: i64,
    /// The shortest exchange's round trip (ticks): how exactly that is
    /// known.
    pub uncertainty: u64,
    /// The boot processor did not answer.
    pub failed: bool,
}

/// The starting processor's part: sets its TSC alike the boot processor's.
/// Runs before the processor counts as started, while the boot processor
/// answers ([`serve_tsc_sync`]).
pub fn sync_tsc() -> TscSync {
    use crate::arch::cpu::{MSR_TSC_ADJUST, features, rdmsr, wrmsr};
    let mut result = TscSync::default();
    let adjustable = features().tsc_adjust;
    if adjustable {
        result.firmware_adjust = rdmsr(MSR_TSC_ADJUST) as i64;
        if result.firmware_adjust != 0 {
            // SAFETY: TSC_ADJUST exists; this processor keeps no time yet.
            unsafe { wrmsr(MSR_TSC_ADJUST, 0) };
        }
    }
    let Some((behind, rtt)) = boot_tsc_offset() else {
        result.failed = true;
        return result;
    };
    (result.behind, result.left, result.uncertainty) = (behind, behind, rtt);
    // Corrected only when clearly more than the exchange can tell.
    if adjustable && behind.unsigned_abs() > rtt {
        // SAFETY: as above; moves this processor's TSC forward by `behind`.
        unsafe { wrmsr(MSR_TSC_ADJUST, (rdmsr(MSR_TSC_ADJUST) as i64).wrapping_add(behind) as u64) };
        if let Some((left, rtt)) = boot_tsc_offset() {
            (result.left, result.uncertainty) = (left, rtt);
        }
    }
    result
}

/// How many ticks this processor's TSC is behind the boot processor's, and
/// the round trip of the exchange it was measured with (the shortest of
/// several: the boot processor's reading falls inside it). `None` if the
/// boot processor does not answer.
fn boot_tsc_offset() -> Option<(i64, u64)> {
    use crate::arch::cpu::{rdtsc, rdtsc_ordered};
    let timeout = TSC_HZ.load(Ordering::Relaxed) / 1000 * SYNC_TIMEOUT_MS;
    let wait_for = |step: u64, since: u64| {
        while SYNC_STEP.load(Ordering::Acquire) != step {
            if rdtsc().wrapping_sub(since) > timeout {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    };
    let mut best: Option<(u64, i64)> = None;
    for _ in 0..SYNC_ROUNDS {
        // An unanswered request (from a processor that gave up) is answered
        // first.
        let mut step = SYNC_STEP.load(Ordering::Acquire);
        if step % 2 == 1 {
            if !wait_for(step + 1, rdtsc()) {
                return None;
            }
            step += 1;
        }
        let t1 = rdtsc_ordered();
        SYNC_STEP.store(step + 1, Ordering::Release);
        if !wait_for(step + 2, t1) {
            return None;
        }
        let t2 = rdtsc_ordered();
        let boot = SYNC_BOOT_TSC.load(Ordering::Relaxed);
        let rtt = t2.wrapping_sub(t1);
        let midpoint = t1 + rtt / 2;
        let behind = boot.wrapping_sub(midpoint) as i64;
        if best.is_none_or(|(b, _)| rtt < b) {
            best = Some((rtt, behind));
        }
    }
    best.map(|(rtt, behind)| (behind, rtt))
}

/// Ticks in nanoseconds (for the log).
pub fn ticks_to_ns(ticks: i64) -> i64 {
    let hz = TSC_HZ.load(Ordering::Relaxed).max(1) as i128;
    (ticks as i128 * 1_000_000_000 / hz) as i64
}
