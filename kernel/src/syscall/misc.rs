//! Miscellaneous system calls: logging, clocks, sleeping, system info.

use alloc::vec;

use vabi::{Error, RawHandle, SystemInfo, clock, power_action, resource_kind};

use super::{SysResult, deadline, get_resource, ok};
use crate::mm::user;
use crate::sched::{self, WakeReason};
use crate::time;

pub fn debug_write(ptr: usize, len: usize) -> SysResult {
    let len = len.min(4096);
    let bytes = user::read_vec(ptr as u64, len, 4096)?;
    let cur = sched::current();
    crate::log::write_user(cur.process_name(), &bytes);
    ok(len)
}

pub fn log_read(offset: usize, buf: usize, len: usize) -> SysResult {
    let mut tmp = vec![0u8; len.min(64 * 1024)];
    let (n, next) = crate::log::read(offset as u64, &mut tmp);
    user::copy_to_user(buf as u64, &tmp[..n])?;
    Ok((n, Some(next as usize)))
}

pub fn clock_get(id: usize) -> SysResult {
    match id {
        clock::MONOTONIC => ok(time::now_ns() as usize),
        clock::REALTIME => ok(time::realtime_ns() as usize),
        _ => Err(Error::InvalidArgs),
    }
}

pub fn sleep(d: usize) -> SysResult {
    match deadline(d as u64) {
        Some(d) => match sched::sleep_until(d) {
            WakeReason::Killed => Err(Error::Canceled),
            _ => ok(0),
        },
        None => match sched::block(None) {
            WakeReason::Killed => Err(Error::Canceled),
            _ => ok(0),
        },
    }
}

pub fn system_info(ptr: usize) -> SysResult {
    let (total, free) = crate::mm::phys::stats();
    let mut version = [0u8; 32];
    let v = concat!("Vindows ", env!("CARGO_PKG_VERSION"));
    version[..v.len()].copy_from_slice(v.as_bytes());
    let procs = crate::object::process::all();
    let info = SystemInfo {
        version,
        cpu_count: crate::arch::percpu::online().count() as u32,
        page_size: 4096,
        total_memory: total,
        free_memory: free,
        uptime_ns: time::now_ns(),
        process_count: procs.iter().filter(|p| !p.is_terminated()).count() as u32,
        thread_count: procs.iter().map(|p| p.thread_count() as u32).sum(),
        idle_time_ns: sched::total_idle_ns(),
        cpu_brand: crate::arch::cpu::brand_string(),
    };
    user::write(ptr as u64, &info)?;
    ok(0)
}

pub fn system_power(res: RawHandle, action: usize) -> SysResult {
    let r = get_resource(res, vabi::Rights::NONE)?;
    if !r.permits(resource_kind::POWER, 0, 0) {
        return Err(Error::AccessDenied);
    }
    match action {
        power_action::POWER_OFF => {
            crate::kinfo!("power: shutting down");
            crate::acpi::power_off();
            // QEMU's isa-debug-exit device (used by automated tests).
            // SAFETY: harmless if the device is absent.
            unsafe { crate::arch::port::outb(0xF4, 0x10) };
            Err(Error::NotSupported)
        }
        power_action::REBOOT => {
            crate::kinfo!("power: rebooting");
            crate::acpi::reboot();
            Err(Error::NotSupported)
        }
        _ => Err(Error::InvalidArgs),
    }
}

pub fn random(buf: usize, len: usize) -> SysResult {
    if len > 4096 {
        return Err(Error::InvalidArgs);
    }
    let mut tmp = vec![0u8; len];
    crate::arch::cpu::hardware_random(&mut tmp);
    user::copy_to_user(buf as u64, &tmp)?;
    ok(len)
}
