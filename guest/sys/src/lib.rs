//! The Linux system calls the driver VM's programs make that Rust's `std`
//! does not: mounting, powering off, signals, `ioctl`, `poll`, and sockets
//! other than the Internet's. Raw, on x86-64. [`netif`] has what the network
//! drivers share: interfaces switched on and off, raw packet sockets.

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

pub mod netif;

const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_POLL: usize = 7;
const SYS_IOCTL: usize = 16;
const SYS_SOCKET: usize = 41;
const SYS_BIND: usize = 49;
const SYS_SETSOCKOPT: usize = 54;
const SYS_KILL: usize = 62;
const SYS_MOUNT: usize = 165;
const SYS_REBOOT: usize = 169;
const REBOOT_MAGIC1: usize = 0xFEE1_DEAD;
const REBOOT_MAGIC2: usize = 672_274_793;
const REBOOT_POWER_OFF: usize = 0x4321_FEDC;

/// A system call with up to five arguments: its result, or the error it
/// returned.
///
/// # Safety
/// The arguments must be what system call `nr` takes (pointers to memory
/// it may read or write as it does).
pub unsafe fn syscall(nr: usize, a: [usize; 5]) -> io::Result<usize> {
    let ret: isize;
    // SAFETY: the caller passes what the system call takes; the kernel
    // clobbers rcx and r11.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") nr as isize => ret,
            in("rdi") a[0],
            in("rsi") a[1],
            in("rdx") a[2],
            in("r10") a[3],
            in("r8") a[4],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if (-4095..0).contains(&ret) { Err(io::Error::from_raw_os_error(-ret as i32)) } else { Ok(ret as usize) }
}

/// A system call with six arguments (`mmap`'s).
///
/// # Safety
/// As [`syscall`]'s.
pub unsafe fn syscall6(nr: usize, a: [usize; 6]) -> io::Result<usize> {
    let ret: isize;
    // SAFETY: as `syscall`'s.
    unsafe {
        std::arch::asm!(
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
    if (-4095..0).contains(&ret) { Err(io::Error::from_raw_os_error(-ret as i32)) } else { Ok(ret as usize) }
}

/// Linux's monotonic clock (ns): what its drivers stamp events with.
pub fn monotonic_ns() -> u64 {
    const SYS_CLOCK_GETTIME: usize = 228;
    const CLOCK_MONOTONIC: usize = 1;
    let mut t = [0i64; 2];
    // SAFETY: the kernel writes a timespec into `t`.
    let _ = unsafe { syscall(SYS_CLOCK_GETTIME, [CLOCK_MONOTONIC, t.as_mut_ptr() as usize, 0, 0, 0]) };
    t[0] as u64 * 1_000_000_000 + t[1] as u64
}

/// The number of an `ioctl` request: direction (`dir`: 1 writes, 2 reads,
/// 3 both, from the caller's side), the size of its argument, its type
/// and number (`_IOC`).
pub const fn ioc(dir: u32, kind: u8, nr: u8, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((kind as u32) << 8) | nr as u32
}

/// `ioctl(fd, request, arg)`.
///
/// # Safety
/// `arg` must be what the request takes: a pointer to memory of its size
/// it may read and write, or a value.
pub unsafe fn ioctl(fd: RawFd, request: u32, arg: usize) -> io::Result<usize> {
    // SAFETY: as the caller promises.
    unsafe { syscall(SYS_IOCTL, [fd as usize, request as usize, arg, 0, 0]) }
}

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;

/// A file to `poll`, the events to wait for, and those that came.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PollFd {
    pub fd: RawFd,
    pub events: i16,
    pub revents: i16,
}

impl PollFd {
    pub fn new(fd: RawFd, events: i16) -> PollFd {
        PollFd { fd, events, revents: 0 }
    }
}

/// Waits up to `timeout_ms` (negative: forever) until one of `fds` has
/// one of its events; returns how many have (0 on timeout).
pub fn poll_many(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    // SAFETY: the pollfds, which the kernel writes back.
    unsafe { syscall(SYS_POLL, [fds.as_mut_ptr() as usize, fds.len(), timeout_ms as isize as usize, 0, 0]) }
}

/// Waits up to `timeout_ms` (negative: forever) for `events` on `fd`;
/// returns the events that came (0 on timeout).
pub fn poll(fd: RawFd, events: i16, timeout_ms: i32) -> io::Result<i16> {
    let mut p = [PollFd::new(fd, events)];
    poll_many(&mut p, timeout_ms)?;
    Ok(p[0].revents)
}

/// `socket(domain, kind, protocol)`, a new file.
pub fn socket(domain: i32, kind: i32, protocol: i32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: no memory involved.
    let fd = unsafe { syscall(SYS_SOCKET, [domain as usize, kind as usize, protocol as usize, 0, 0])? };
    // SAFETY: the kernel just made the file; nothing else owns it.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as RawFd) })
}

/// `bind(fd, address)`: `address` a socket address of the socket's family.
pub fn bind(fd: RawFd, address: &[u8]) -> io::Result<()> {
    // SAFETY: the kernel reads `address`.
    unsafe { syscall(SYS_BIND, [fd as usize, address.as_ptr() as usize, address.len(), 0, 0]).map(|_| ()) }
}

/// `setsockopt(fd, level, name, value)`.
pub fn setsockopt(fd: RawFd, level: i32, name: i32, value: &[u8]) -> io::Result<()> {
    // SAFETY: the kernel reads `value`.
    unsafe {
        syscall(SYS_SETSOCKOPT, [fd as usize, level as usize, name as usize, value.as_ptr() as usize, value.len()])
            .map(|_| ())
    }
}

/// Mounts `source` (a file system of `kind`) at `target`.
pub fn mount(source: &str, target: &str, kind: &str) -> io::Result<()> {
    let c = |s: &str| CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
    let (source, target, kind) = (c(source)?, c(target)?, c(kind)?);
    // SAFETY: three strings that live through the call, no flags or data.
    unsafe {
        syscall(SYS_MOUNT, [source.as_ptr() as usize, target.as_ptr() as usize, kind.as_ptr() as usize, 0, 0])
            .map(|_| ())
    }
}

/// Powers the machine off.
pub fn power_off() -> io::Result<()> {
    // SAFETY: the reboot system call takes no memory.
    unsafe { syscall(SYS_REBOOT, [REBOOT_MAGIC1, REBOOT_MAGIC2, REBOOT_POWER_OFF, 0, 0]).map(|_| ()) }
}

/// Sends `signal` to process `pid`.
pub fn kill(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: the kill system call takes no memory.
    unsafe { syscall(SYS_KILL, [pid as usize, signal as usize, 0, 0, 0]).map(|_| ()) }
}

/// `read(fd, buf)`: what came (`WouldBlock` on a file that does not block
/// and has nothing).
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    unsafe { syscall(SYS_READ, [fd as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0]) }
}

/// `write(fd, buf)`: how much went.
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: the kernel reads `buf`.
    unsafe { syscall(SYS_WRITE, [fd as usize, buf.as_ptr() as usize, buf.len(), 0, 0]) }
}
