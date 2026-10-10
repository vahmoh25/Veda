//! The Linux system calls the driver VM's programs make that Rust's `std`
//! does not: mounting, powering off, `ioctl` and `poll`. Raw, on x86-64.

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

const SYS_POLL: usize = 7;
const SYS_IOCTL: usize = 16;
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

/// Waits up to `timeout_ms` (negative: forever) for `events` on `fd`;
/// returns the events that came (0 on timeout).
pub fn poll(fd: RawFd, events: i16, timeout_ms: i32) -> io::Result<i16> {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    let mut p = PollFd { fd, events, revents: 0 };
    // SAFETY: one pollfd, which the kernel writes back.
    let n = unsafe { syscall(SYS_POLL, [&mut p as *mut PollFd as usize, 1, timeout_ms as isize as usize, 0, 0])? };
    Ok(if n == 0 { 0 } else { p.revents })
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
