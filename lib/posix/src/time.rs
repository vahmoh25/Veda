//! Clocks and sleeping.

use crate::error::SysResult;
use crate::linux::errno::{EFAULT, EINVAL};
use crate::linux::{Timespec, Timeval, Tms, clock};
use crate::user;

/// Nanoseconds since boot (never goes backwards).
pub fn monotonic_ns() -> u64 {
    vrt::time::now_ns()
}

/// Nanoseconds since 1970-01-01 00:00:00 UTC.
pub fn realtime_ns() -> u64 {
    vrt::sys::clock_get(vabi::clock::UTC)
}

/// CPU time this process has used.
fn cpu_ns() -> u64 {
    vrt::object::process_self_info().map_or(0, |i| i.cpu_time_ns)
}

fn now(clk: u32) -> Result<u64, isize> {
    Ok(match clk {
        clock::REALTIME | clock::REALTIME_COARSE => realtime_ns(),
        clock::MONOTONIC | clock::MONOTONIC_RAW | clock::MONOTONIC_COARSE | clock::BOOTTIME => monotonic_ns(),
        clock::PROCESS_CPUTIME_ID | clock::THREAD_CPUTIME_ID => cpu_ns(),
        _ => return Err(EINVAL),
    })
}

pub unsafe fn clock_gettime(clk: u32, ts: usize) -> SysResult {
    let t = Timespec::from_ns(now(clk)?);
    // SAFETY: the program passed a timespec.
    unsafe { user::write(ts, t)? };
    Ok(0)
}

pub unsafe fn clock_getres(clk: u32, ts: usize) -> SysResult {
    now(clk)?;
    if ts != 0 {
        // SAFETY: as above.
        unsafe { user::write(ts, Timespec { sec: 0, nsec: 1 })? };
    }
    Ok(0)
}

pub unsafe fn gettimeofday(tv: usize) -> SysResult {
    if tv != 0 {
        let ns = realtime_ns();
        // SAFETY: the program passed a timeval.
        unsafe {
            user::write(tv, Timeval { sec: (ns / 1_000_000_000) as i64, usec: ((ns % 1_000_000_000) / 1000) as i64 })?
        };
    }
    Ok(0)
}

pub unsafe fn time(out: usize) -> SysResult {
    let s = (realtime_ns() / 1_000_000_000) as i64;
    if out != 0 {
        // SAFETY: the program passed a time_t.
        unsafe { user::write(out, s)? };
    }
    Ok(s as usize)
}

/// `clock_nanosleep` (and `nanosleep`, which sleeps on the monotonic
/// clock). Sleeps are never interrupted, so `rem` is never written.
pub unsafe fn clock_nanosleep(clk: u32, flags: u32, req: usize) -> SysResult {
    if req == 0 {
        return Err(EFAULT);
    }
    // SAFETY: the program passed a timespec.
    let t: Timespec = unsafe { user::read(req)? };
    let ns = t.to_ns().ok_or(EINVAL)?;
    let current = now(clk)?;
    let wait = if flags & clock::TIMER_ABSTIME != 0 { ns.saturating_sub(current) } else { ns };
    vrt::time::sleep_until(monotonic_ns().saturating_add(wait));
    Ok(0)
}

/// `times`: the process's CPU time in clock ticks (100 a second).
pub unsafe fn times(buf: usize) -> SysResult {
    if buf != 0 {
        let ticks = (cpu_ns() / 10_000_000) as i64;
        // SAFETY: the program passed a struct tms.
        unsafe { user::write(buf, Tms { utime: ticks, ..Default::default() })? };
    }
    Ok((monotonic_ns() / 10_000_000) as usize)
}
