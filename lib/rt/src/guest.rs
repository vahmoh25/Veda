//! Veda's system calls in a Linux guest of the driver VM.
//!
//! A program built for the guest (with `--cfg veda_guest`, which the
//! guest's target sets) makes its system calls here instead of with
//! `syscall`: the ones on Veda's objects, and its log, through the bridge
//! (`/dev/veda`, `vhv::bridge`), the rest with Linux's own (time from the
//! TSC, which the guest reads as Veda does; futexes; sleeping). So the
//! program uses `vrt`, and everything built on it (`vipc`, `vproto`), as a
//! Veda program does. What makes no sense in the guest (processes,
//! hardware, virtual machines) is `NotSupported`.

use core::arch::asm;
use core::sync::atomic::{AtomicI32, Ordering};

use vabi::{ClockInfo, Error, HandleBasicInfo, RawHandle, WaitItem, clock, info_topic, map_flags, nr};
use vhv::bridge::{self as b, op};

use crate::sync::Lazy;

const SYS_WRITE: usize = 1;
const SYS_OPEN: usize = 2;
const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_IOCTL: usize = 16;
const SYS_SCHED_YIELD: usize = 24;
const SYS_NANOSLEEP: usize = 35;
const SYS_FUTEX: usize = 202;
const SYS_EXIT_GROUP: usize = 231;
const SYS_GETRANDOM: usize = 318;
const EINTR: isize = 4;
const ETIMEDOUT: isize = 110;
const EAGAIN: isize = 11;
const O_RDWR: usize = 2;
const O_CLOEXEC: usize = 0o2_000_000;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;
const FUTEX_WAIT_PRIVATE: usize = 128;
const FUTEX_WAKE_PRIVATE: usize = 129;

/// A Linux system call: its result, or `-errno`.
fn linux(nr: usize, a: [usize; 6]) -> isize {
    let ret: isize;
    // SAFETY: the callers pass the arguments the call takes; the kernel
    // clobbers rcx and r11.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr as isize => ret,
            in("rdi") a[0],
            in("rsi") a[1],
            in("rdx") a[2],
            in("r10") a[3],
            in("r8") a[4],
            in("r9") a[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// `/dev/veda`, opened the first time it is needed (-1: it cannot be).
static DEVICE: AtomicI32 = AtomicI32::new(0);

fn device() -> Result<usize, Error> {
    match DEVICE.load(Ordering::Acquire) {
        0 => {
            let fd = linux(SYS_OPEN, [c"/dev/veda".as_ptr() as usize, O_RDWR | O_CLOEXEC, 0, 0, 0, 0]);
            let fd = if fd < 0 { -1 } else { fd as i32 };
            match DEVICE.compare_exchange(0, fd, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) if fd >= 0 => Ok(fd as usize),
                Ok(_) => Err(Error::NotSupported),
                // Another thread opened it first.
                Err(other) => {
                    if fd >= 0 {
                        linux(3, [fd as usize, 0, 0, 0, 0, 0]);
                    }
                    if other < 0 { Err(Error::NotSupported) } else { Ok(other as usize) }
                }
            }
        }
        fd if fd < 0 => Err(Error::NotSupported),
        fd => Ok(fd as usize),
    }
}

/// `_IOWR(0xB9, op, size)`.
const fn command(op: u32, size: usize) -> usize {
    (3 << 30) | ((size & 0x3FFF) << 16) | (0xB9 << 8) | op as usize
}

/// Carries out bridge operation `op` with `req`; its Veda status as a
/// result.
fn bridge<T>(op: u32, req: &mut T) -> Result<(), Error> {
    let fd = device()?;
    loop {
        match linux(SYS_IOCTL, [fd, command(op, core::mem::size_of::<T>()), req as *mut T as usize, 0, 0, 0]) {
            0 => break,
            r if r == -EINTR => continue,
            _ => return Err(Error::Io),
        }
    }
    // SAFETY: every request starts with its status (`vhv::bridge`).
    let status = unsafe { *(req as *const T as *const u32) };
    match status {
        b::OK => Ok(()),
        s => Err(Error::from_code(s as i32).unwrap_or(Error::Internal)),
    }
}

/// How Veda's clocks follow the TSC (asked once).
fn clock_info() -> ClockInfo {
    static INFO: Lazy<ClockInfo> = Lazy::new(|| {
        let mut r = b::Clock::default();
        let _ = bridge(op::CLOCK, &mut r);
        r.info
    });
    *INFO
}

fn now_ns() -> u64 {
    let info = clock_info();
    // SAFETY: RDTSC is always available.
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    ((tsc.saturating_sub(info.tsc_at_zero) as u128 * info.ns_per_tick as u128) >> 32) as u64
}

/// How long from now until Veda's monotonic `deadline`, as a Linux
/// `timespec` (`None`: never).
fn timeout(deadline: u64) -> Option<[i64; 2]> {
    (deadline != vabi::DEADLINE_INFINITE).then(|| {
        let ns = deadline.saturating_sub(now_ns());
        [(ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64]
    })
}

/// The wait of `OBJECT_WAIT_MANY` and `OBJECT_WAIT_ONE`.
fn wait(items: &mut [WaitItem], deadline: u64) -> Result<usize, Error> {
    let mut r = b::Wait { count: items.len() as u32, items: items.as_mut_ptr() as u64, deadline, ..b::Wait::default() };
    bridge(op::WAIT, &mut r)?;
    Ok(r.satisfied as usize)
}

/// Makes Veda system call `n`, as [`crate::sys::call2`] does.
pub(crate) fn call(n: usize, a: [usize; 6]) -> Result<(usize, usize), Error> {
    let one = |v: usize| Ok((v, 0));
    match n {
        nr::DEBUG_WRITE => {
            linux(SYS_WRITE, [2, a[0], a[1], 0, 0, 0]);
            one(0)
        }
        nr::CLOCK_GET => {
            let info = clock_info();
            match a[0] {
                clock::MONOTONIC => one(now_ns() as usize),
                clock::REALTIME => one((info.realtime_at_zero + now_ns()) as usize),
                clock::UTC => one((info.utc_at_zero + now_ns()) as usize),
                _ => Err(Error::InvalidArgs),
            }
        }
        nr::CLOCK_INFO => {
            // SAFETY: the caller passes a `ClockInfo` to fill.
            unsafe { (a[0] as *mut ClockInfo).write(clock_info()) };
            one(0)
        }
        nr::SLEEP => {
            loop {
                let Some(t) = timeout(a[0] as u64) else {
                    linux(SYS_NANOSLEEP, [&[i64::MAX, 0] as *const _ as usize, 0, 0, 0, 0, 0]);
                    continue;
                };
                if t == [0, 0] || linux(SYS_NANOSLEEP, [&t as *const _ as usize, 0, 0, 0, 0, 0]) == 0 {
                    break;
                }
            }
            one(0)
        }
        nr::YIELD => {
            linux(SYS_SCHED_YIELD, [0; 6]);
            one(0)
        }
        nr::RANDOM => {
            let r = linux(SYS_GETRANDOM, [a[0], a[1], 0, 0, 0, 0]);
            if r < 0 { Err(Error::Io) } else { one(0) }
        }
        nr::HANDLE_CLOSE => {
            let mut r = b::Close { handle: a[0] as u32, ..b::Close::default() };
            bridge(op::CLOSE, &mut r).map(|_| (0, 0))
        }
        nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE => {
            let rights = if a[1] as u32 == u32::MAX { 0 } else { a[1] as u32 };
            let mut r = b::Duplicate { handle: a[0] as u32, rights, ..b::Duplicate::default() };
            bridge(op::DUPLICATE, &mut r)?;
            if n == nr::HANDLE_REPLACE {
                let mut c = b::Close { handle: a[0] as u32, ..b::Close::default() };
                let _ = bridge(op::CLOSE, &mut c);
            }
            one(r.out as usize)
        }
        nr::OBJECT_INFO if a[1] == info_topic::HANDLE_BASIC => {
            let mut r = b::ObjectInfo { handle: a[0] as u32, ..b::ObjectInfo::default() };
            bridge(op::OBJECT_INFO, &mut r)?;
            if a[3] < core::mem::size_of::<HandleBasicInfo>() {
                return Err(Error::BufferTooSmall);
            }
            let info = HandleBasicInfo { koid: r.koid, object_type: r.object_type, rights: r.rights };
            // SAFETY: the caller passes room for the information.
            unsafe { (a[2] as *mut HandleBasicInfo).write_unaligned(info) };
            one(core::mem::size_of::<HandleBasicInfo>())
        }
        nr::OBJECT_SIGNAL => {
            let mut r = b::Signal { handle: a[0] as u32, clear: a[1] as u32, set: a[2] as u32, ..b::Signal::default() };
            bridge(op::SIGNAL, &mut r).map(|_| (0, 0))
        }
        nr::OBJECT_WAIT_ONE => {
            let mut item = [WaitItem { handle: a[0] as RawHandle, signals: a[1] as u32, ..WaitItem::default() }];
            wait(&mut item, a[2] as u64)?;
            one(item[0].observed as usize)
        }
        nr::OBJECT_WAIT_MANY => {
            // SAFETY: the caller passes `count` items.
            let items = unsafe { core::slice::from_raw_parts_mut(a[0] as *mut WaitItem, a[1]) };
            wait(items, a[2] as u64).map(|n| (n, 0))
        }
        nr::CHANNEL_CREATE => {
            let mut r = b::ChannelCreate::default();
            bridge(op::CHANNEL_CREATE, &mut r)?;
            // SAFETY: the caller passes room for both handles.
            unsafe { (a[0] as *mut [u32; 2]).write_unaligned(r.out) };
            one(0)
        }
        nr::CHANNEL_WRITE => {
            let mut r = b::ChannelWrite {
                handle: a[0] as u32,
                bytes: a[1] as u64,
                bytes_len: a[2] as u32,
                handles: a[3] as u64,
                handles_len: a[4] as u32,
                ..b::ChannelWrite::default()
            };
            bridge(op::CHANNEL_WRITE, &mut r).map(|_| (0, 0))
        }
        nr::CHANNEL_READ => {
            let mut r = b::ChannelRead {
                handle: a[0] as u32,
                bytes: a[1] as u64,
                bytes_capacity: a[2] as u32,
                handles: a[3] as u64,
                handles_capacity: a[4] as u32,
                ..b::ChannelRead::default()
            };
            let result = bridge(op::CHANNEL_READ, &mut r);
            if a[5] != 0 {
                // SAFETY: the caller passes room for both lengths.
                unsafe { (a[5] as *mut [u32; 2]).write_unaligned([r.bytes_len, r.handles_len]) };
            }
            result.map(|_| (0, 0))
        }
        nr::EVENT_CREATE => {
            let mut r = b::EventCreate::default();
            bridge(op::EVENT_CREATE, &mut r)?;
            one(r.out as usize)
        }
        nr::VMO_CREATE => {
            let mut r = b::Vmo { size: a[0] as u64, flags: a[1] as u32, ..b::Vmo::default() };
            bridge(op::VMO_CREATE, &mut r)?;
            one(r.handle as usize)
        }
        nr::VMO_GET_SIZE => {
            let mut r = b::Vmo { handle: a[0] as u32, ..b::Vmo::default() };
            bridge(op::VMO_SIZE, &mut r)?;
            one(r.size as usize)
        }
        nr::VMO_READ | nr::VMO_WRITE => {
            let operation = if n == nr::VMO_READ { op::VMO_READ } else { op::VMO_WRITE };
            let mut done = 0;
            while done < a[3] {
                let len = (a[3] - done).min(b::MAX_COPY as usize);
                let mut r = b::VmoCopy {
                    handle: a[0] as u32,
                    offset: (a[1] + done) as u64,
                    buffer: (a[2] + done) as u64,
                    len: len as u64,
                    ..b::VmoCopy::default()
                };
                bridge(operation, &mut r)?;
                done += len;
            }
            one(a[3])
        }
        nr::LOG_READ => {
            let len = a[2].min(b::MAX_COPY as usize);
            let mut r = b::LogRead { offset: a[0] as u64, buffer: a[1] as u64, len: len as u64, ..b::LogRead::default() };
            bridge(op::LOG_READ, &mut r)?;
            Ok((r.len as usize, r.next as usize))
        }
        nr::VM_MAP => {
            // Only into this program, wherever Linux puts it.
            if a[0] != 0 || a[5] & map_flags::FIXED != 0 {
                return Err(Error::NotSupported);
            }
            let len = a[3].next_multiple_of(4096);
            let mut r = b::VmoMap {
                handle: a[1] as u32,
                offset: a[2] as u64,
                len: len as u64,
                flags: (a[5] & (map_flags::READ | map_flags::WRITE)) as u32,
                ..b::VmoMap::default()
            };
            bridge(op::VMO_MAP, &mut r)?;
            let prot = PROT_READ | if a[5] & map_flags::WRITE != 0 { PROT_WRITE } else { 0 };
            let addr = linux(SYS_MMAP, [0, len, prot, MAP_SHARED, device()?, r.gpa as usize]);
            if addr < 0 {
                return Err(Error::NoMemory);
            }
            one(addr as usize)
        }
        nr::VM_UNMAP if a[0] == 0 => {
            let r = linux(SYS_MUNMAP, [a[1], a[2].next_multiple_of(4096), 0, 0, 0, 0]);
            if r < 0 { Err(Error::InvalidArgs) } else { one(0) }
        }
        nr::PROCESS_EXIT => {
            linux(SYS_EXIT_GROUP, [a[0], 0, 0, 0, 0, 0]);
            one(0)
        }
        nr::FUTEX_WAIT => {
            let t = timeout(a[2] as u64);
            let ts = t.as_ref().map_or(0, |t| t as *const _ as usize);
            match linux(SYS_FUTEX, [a[0], FUTEX_WAIT_PRIVATE, a[1], ts, 0, 0]) {
                r if r == -ETIMEDOUT => Err(Error::TimedOut),
                r if r == -EAGAIN => Err(Error::ShouldWait),
                _ => one(0),
            }
        }
        nr::FUTEX_WAKE => {
            let r = linux(SYS_FUTEX, [a[0], FUTEX_WAKE_PRIVATE, a[1], 0, 0, 0]);
            one(r.max(0) as usize)
        }
        _ => Err(Error::NotSupported),
    }
}

/// The handle the guest starts with in `role` (a new one each time).
pub(crate) fn bootstrap(role: u32) -> Option<RawHandle> {
    let mut r = b::Bootstrap { role, ..b::Bootstrap::default() };
    bridge(op::BOOTSTRAP, &mut r).ok().map(|_| r.out)
}

/// A Linux file descriptor that is readable while one of `signals` of
/// `handle` is active: so that a program waits on Veda's objects and on
/// Linux's files at once, with `poll`. Reading it (4 bytes) gives the
/// signals active then; the next `poll` waits for them again.
pub fn watch(handle: RawHandle, signals: u32) -> Result<std::os::fd::OwnedFd, Error> {
    use std::os::fd::FromRawFd;
    let mut r = b::Watch { handle, signals, ..Default::default() };
    bridge(op::WATCH, &mut r)?;
    // SAFETY: the bridge made the file for this program; nothing else owns it.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(r.fd) })
}

/// A Linux dma-buf of `len` bytes of VMO `vmo` from `offset` (pages),
/// writable by devices if `writable`: so that Linux's drivers use Veda's
/// memory (a display scans it out, a GPU renders into it) without copies.
/// The VMO stays mapped into the guest while the dma-buf lives.
pub fn dmabuf(vmo: RawHandle, offset: u64, len: u64, writable: bool) -> Result<std::os::fd::OwnedFd, Error> {
    use std::os::fd::FromRawFd;
    let flags = map_flags::READ | if writable { map_flags::WRITE } else { 0 };
    let mut r = b::VmoDmabuf { handle: vmo, offset, len, flags: flags as u32, ..Default::default() };
    bridge(op::VMO_DMABUF, &mut r)?;
    // SAFETY: the bridge made the file for this program; nothing else owns it.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(r.fd) })
}
