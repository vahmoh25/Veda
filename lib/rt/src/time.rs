//! Clocks, durations and sleeping.

pub use core::time::Duration;

use vabi::{clock, nr};

use crate::sys::{call, clock_get};

/// Nanoseconds since boot.
pub fn now_ns() -> u64 {
    clock_get(clock::MONOTONIC)
}

/// A monotonic timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64);

impl Instant {
    pub fn now() -> Instant {
        Instant(now_ns())
    }

    pub fn from_nanos(ns: u64) -> Instant {
        Instant(ns)
    }

    pub fn as_nanos(&self) -> u64 {
        self.0
    }

    pub fn elapsed(&self) -> Duration {
        Duration::from_nanos(now_ns().saturating_sub(self.0))
    }

    pub fn duration_since(&self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    /// Deadline value for system calls.
    pub fn deadline(&self) -> u64 {
        self.0
    }
}

impl core::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Instant(self.0.saturating_add(d.as_nanos() as u64))
    }
}

impl core::ops::Sub for Instant {
    type Output = Duration;
    fn sub(self, other: Instant) -> Duration {
        self.duration_since(other)
    }
}

/// Deadline `d` from now, for system calls.
pub fn deadline_after(d: Duration) -> u64 {
    now_ns().saturating_add(d.as_nanos() as u64)
}

pub fn sleep(d: Duration) {
    sleep_until(deadline_after(d));
}

pub fn sleep_until(deadline: u64) {
    let _ = call(nr::SLEEP, [deadline as usize, 0, 0, 0, 0, 0]);
}

/// Wall-clock time: seconds since 1970-01-01 in local time.
pub fn unix_time_ns() -> u64 {
    clock_get(clock::REALTIME)
}

/// A broken-down local date and time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 0 = Sunday.
    pub weekday: u8,
}

impl DateTime {
    pub fn now() -> DateTime {
        DateTime::from_unix(unix_time_ns() / 1_000_000_000)
    }

    /// Converts seconds since the epoch (civil-from-days algorithm).
    pub fn from_unix(secs: u64) -> DateTime {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        DateTime {
            year: (if m <= 2 { y + 1 } else { y }) as i32,
            month: m as u8,
            day: d as u8,
            hour: (rem / 3600) as u8,
            minute: ((rem / 60) % 60) as u8,
            second: (rem % 60) as u8,
            weekday: ((days + 4).rem_euclid(7)) as u8,
        }
    }

    pub fn month_name(&self) -> &'static str {
        const NAMES: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        NAMES[(self.month.clamp(1, 12) - 1) as usize]
    }

    pub fn weekday_name(&self) -> &'static str {
        const NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
        NAMES[self.weekday as usize % 7]
    }
}
