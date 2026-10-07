//! The Linux system call interface, call by call.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::error::SysResult;
use crate::fd::{self, Object};
use crate::linux::errno::*;
use crate::linux::{Timespec, Timeval, at, clock, nr, o};
use crate::{fs, io, mem, process, signal, socket, system, thread, time, user, vfs};

/// Deadline (monotonic ns) of a relative timeout, `None` for none.
unsafe fn deadline_ts(ts: usize) -> Result<Option<u64>, isize> {
    if ts == 0 {
        return Ok(None);
    }
    // SAFETY: the program passed a timespec.
    let t: Timespec = unsafe { user::read(ts)? };
    Ok(Some(time::monotonic_ns().saturating_add(t.to_ns().ok_or(EINVAL)?)))
}

unsafe fn deadline_tv(tv: usize) -> Result<Option<u64>, isize> {
    if tv == 0 {
        return Ok(None);
    }
    // SAFETY: the program passed a timeval.
    let t: Timeval = unsafe { user::read(tv)? };
    if t.sec < 0 || !(0..1_000_000).contains(&t.usec) {
        return Err(EINVAL);
    }
    let ns = (t.sec as u64).saturating_mul(1_000_000_000).saturating_add(t.usec as u64 * 1000);
    Ok(Some(time::monotonic_ns().saturating_add(ns)))
}

/// `fallocate` with mode 0: makes sure the file is at least
/// `offset + len` bytes long.
fn fallocate(fd: i32, mode: u32, offset: i64, len: i64) -> SysResult {
    if mode != 0 {
        return Err(EOPNOTSUPP);
    }
    let end = offset.checked_add(len).filter(|&e| offset >= 0 && len > 0 && e >= 0).ok_or(EINVAL)? as u64;
    let desc = fd::get(fd)?;
    let Object::File(f) = &desc.object else { return Err(ENODEV) };
    if f.stat()?.size < end {
        f.truncate(end)?;
    }
    Ok(0)
}

/// Accepts the ids of the one user there is.
fn set_ids(ids: &[usize]) -> SysResult {
    let ok = ids.iter().all(|&id| id as u32 == vfs::UID || id as i32 == -1);
    if ok { Ok(0) } else { Err(EPERM) }
}

/// System calls that are not supported, each reported once.
static REPORTED: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

fn unsupported(n: usize) -> SysResult {
    if n < 512 && REPORTED[n / 64].fetch_or(1 << (n % 64), Ordering::Relaxed) & (1 << (n % 64)) == 0 {
        let name = vrt::object::process_self_info().map(|i| alloc::string::String::from(i.name())).unwrap_or_default();
        vrt::println!("{}: system call {} is not supported", name, n);
    }
    Err(ENOSYS)
}

/// Runs Linux system call `n`.
///
/// # Safety
/// The arguments are the program's: pointers must be valid for the call.
pub unsafe fn dispatch(n: usize, a: [usize; 6]) -> SysResult {
    let fd0 = a[0] as i32;
    // SAFETY (whole match): the arguments are passed on as the program gave
    // them, each call documenting what it requires of them.
    unsafe {
        match n {
            nr::READ => io::read(fd0, a[1], a[2]),
            nr::WRITE => io::write(fd0, a[1], a[2]),
            nr::OPEN => fs::openat(at::FDCWD, a[0], a[1] as u32),
            nr::CREAT => fs::openat(at::FDCWD, a[0], o::CREAT | o::WRONLY | o::TRUNC),
            nr::OPENAT => fs::openat(fd0, a[1], a[2] as u32),
            nr::CLOSE => io::close(fd0),
            nr::STAT => fs::fstatat(at::FDCWD, a[0], a[1], 0),
            nr::LSTAT => fs::fstatat(at::FDCWD, a[0], a[1], at::SYMLINK_NOFOLLOW),
            nr::FSTAT => fs::fstat(fd0, a[1]),
            nr::NEWFSTATAT => fs::fstatat(fd0, a[1], a[2], a[3] as u32),
            nr::POLL => {
                let ms = a[2] as i32;
                let deadline = (ms >= 0).then(|| time::monotonic_ns().saturating_add(ms as u64 * 1_000_000));
                io::poll(a[0], a[1], deadline)
            }
            nr::PPOLL => io::poll(a[0], a[1], deadline_ts(a[2])?),
            nr::SELECT => io::select(fd0, a[1], a[2], a[3], deadline_tv(a[4])?),
            nr::PSELECT6 => io::select(fd0, a[1], a[2], a[3], deadline_ts(a[4])?),
            nr::LSEEK => io::lseek(fd0, a[1] as i64, a[2] as u32),
            nr::MMAP => mem::mmap(a[0], a[1], a[2] as u32, a[3] as u32, a[4] as i32, a[5] as i64),
            nr::MPROTECT => mem::mprotect(a[0], a[1], a[2] as u32),
            nr::MUNMAP => mem::munmap(a[0], a[1]),
            // There is no program break: allocators map memory instead.
            nr::BRK => Ok(0),
            nr::MREMAP | nr::MINCORE => Err(ENOSYS),
            nr::MADVISE => mem::madvise(a[0], a[1], a[2] as u32),
            nr::MSYNC | nr::MLOCK | nr::MUNLOCK | nr::MLOCKALL | nr::MUNLOCKALL => Ok(0),
            nr::RT_SIGACTION => signal::rt_sigaction(a[0] as u32, a[1], a[2], a[3]),
            nr::RT_SIGPROCMASK => signal::rt_sigprocmask(a[0] as u32, a[1], a[2], a[3]),
            nr::RT_SIGPENDING => signal::rt_sigpending(a[0]),
            nr::RT_SIGSUSPEND => signal::rt_sigsuspend(a[0]),
            nr::RT_SIGTIMEDWAIT => signal::rt_sigtimedwait(a[0], a[1], a[2]),
            nr::SIGALTSTACK => signal::sigaltstack(a[1]),
            nr::IOCTL => io::ioctl(fd0, a[1] as u32, a[2]),
            nr::PREAD64 => io::pread(fd0, a[1], a[2], a[3] as i64),
            nr::PWRITE64 => io::pwrite(fd0, a[1], a[2], a[3] as i64),
            nr::READV => io::readv(fd0, a[1], a[2], None),
            nr::WRITEV => io::writev(fd0, a[1], a[2], None),
            nr::PREADV => io::readv(fd0, a[1], a[2], Some(a[3] as i64)),
            nr::PWRITEV => io::writev(fd0, a[1], a[2], Some(a[3] as i64)),
            nr::ACCESS => fs::faccessat(at::FDCWD, a[0], a[1] as u32),
            nr::FACCESSAT | nr::FACCESSAT2 => fs::faccessat(fd0, a[1], a[2] as u32),
            nr::PIPE => io::pipe2(a[0], 0),
            nr::PIPE2 => io::pipe2(a[0], a[1] as u32),
            nr::SOCKETPAIR => socket::socketpair(a[0], a[1], a[3]),
            nr::SOCKET => Err(EAFNOSUPPORT),
            nr::SENDTO => socket::sendto(fd0, a[1], a[2], a[3] as u32, a[4]),
            nr::RECVFROM => socket::recvfrom(fd0, a[1], a[2], a[3] as u32, a[5]),
            nr::SENDMSG => socket::sendmsg(fd0, a[1], a[2] as u32),
            nr::RECVMSG => socket::recvmsg(fd0, a[1], a[2] as u32),
            nr::SHUTDOWN => socket::shutdown(fd0, a[1] as u32),
            nr::SCHED_YIELD => {
                vrt::thread::yield_now();
                Ok(0)
            }
            nr::DUP => io::dup(fd0),
            nr::DUP2 => io::dup3(fd0, a[1] as i32, 0, true),
            nr::DUP3 => io::dup3(fd0, a[1] as i32, a[2] as u32, false),
            nr::CLOSE_RANGE => io::close_range(a[0] as u32, a[1] as u32, a[2] as u32),
            nr::PAUSE => signal::pause_forever(),
            nr::NANOSLEEP => time::clock_nanosleep(clock::MONOTONIC, 0, a[0]),
            nr::CLOCK_NANOSLEEP => time::clock_nanosleep(a[0] as u32, a[1] as u32, a[2]),
            nr::GETPID | nr::GETTID | nr::GETPGRP | nr::GETPGID | nr::GETSID | nr::SETSID => {
                Ok(process::pid() as usize)
            }
            nr::GETPPID => Ok(process::ppid() as usize),
            nr::SETPGID => Ok(0),
            nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ok(vfs::UID as usize),
            nr::SETUID | nr::SETGID => set_ids(&a[..1]),
            nr::SETREUID | nr::SETREGID => set_ids(&a[..2]),
            nr::SETRESUID | nr::SETRESGID => set_ids(&a[..3]),
            nr::GETRESUID | nr::GETRESGID => {
                for p in &a[..3] {
                    user::write(*p, vfs::UID)?;
                }
                Ok(0)
            }
            nr::GETGROUPS => {
                if a[0] > 0 {
                    user::write(a[1], vfs::UID)?;
                }
                Ok(1)
            }
            nr::SETGROUPS => Err(EPERM),
            // Processes are made from program images (posix_spawn), not
            // by copying this one.
            nr::CLONE | nr::FORK | nr::VFORK | nr::EXECVE => Err(ENOSYS),
            nr::EXIT => thread::exit_thread(),
            nr::EXIT_GROUP => process::exit(a[0] as i32),
            nr::WAIT4 => process::wait4(fd0, a[1], a[2] as u32, a[3]),
            nr::WAITID => process::waitid(a[0] as u32, a[1] as i32, a[2], a[3] as u32),
            nr::KILL => process::kill(fd0, a[1] as u32),
            nr::TKILL => signal::send_self(a[1] as u32),
            nr::TGKILL if fd0 == process::pid() => signal::send_self(a[2] as u32),
            nr::TGKILL => Err(ESRCH),
            nr::UNAME => system::uname(a[0]),
            nr::FCNTL => io::fcntl(fd0, a[1] as u32, a[2]),
            nr::FLOCK | nr::FSYNC | nr::FDATASYNC | nr::SYNC_FILE_RANGE | nr::FADVISE64 | nr::READAHEAD => {
                fd::get(fd0).map(|_| 0)
            }
            nr::SYNC | nr::SYNCFS => Ok(0),
            nr::TRUNCATE => fs::truncate(a[0], a[1] as i64),
            nr::FTRUNCATE => io::ftruncate(fd0, a[1] as i64),
            nr::FALLOCATE => fallocate(fd0, a[1] as u32, a[2] as i64, a[3] as i64),
            nr::GETDENTS64 => fs::getdents64(fd0, a[1], a[2]),
            nr::GETCWD => fs::getcwd(a[0], a[1]),
            nr::CHDIR => fs::chdir(a[0]),
            nr::FCHDIR => fs::fchdir(fd0),
            nr::RENAME => fs::renameat(at::FDCWD, a[0], at::FDCWD, a[1], 0),
            nr::RENAMEAT => fs::renameat(fd0, a[1], a[2] as i32, a[3], 0),
            nr::RENAMEAT2 => fs::renameat(fd0, a[1], a[2] as i32, a[3], a[4] as u32),
            nr::MKDIR => fs::mkdirat(at::FDCWD, a[0]),
            nr::MKDIRAT => fs::mkdirat(fd0, a[1]),
            nr::RMDIR => fs::unlinkat(at::FDCWD, a[0], at::REMOVEDIR),
            nr::UNLINK => fs::unlinkat(at::FDCWD, a[0], 0),
            nr::UNLINKAT => fs::unlinkat(fd0, a[1], a[2] as u32),
            // Veda has neither hard nor symbolic links.
            nr::LINK | nr::LINKAT | nr::SYMLINK | nr::SYMLINKAT | nr::MKNOD | nr::MKNODAT => Err(EPERM),
            nr::READLINK => fs::readlinkat(at::FDCWD, a[0]),
            nr::READLINKAT => fs::readlinkat(fd0, a[1]),
            nr::CHMOD | nr::CHOWN | nr::LCHOWN => fs::chmodat(at::FDCWD, a[0]),
            nr::FCHMODAT | nr::FCHOWNAT => fs::chmodat(fd0, a[1]),
            nr::FCHMOD | nr::FCHOWN => fd::get(fd0).map(|_| 0),
            nr::UMASK => system::umask(a[0] as u32),
            nr::UTIMENSAT => fs::utimensat(fd0, a[1], a[2]),
            nr::STATFS => fs::statfs(a[0], a[1]),
            nr::FSTATFS => fs::fstatfs(fd0, a[1]),
            nr::GETTIMEOFDAY => time::gettimeofday(a[0]),
            nr::TIME => time::time(a[0]),
            nr::CLOCK_GETTIME => time::clock_gettime(a[0] as u32, a[1]),
            nr::CLOCK_GETRES => time::clock_getres(a[0] as u32, a[1]),
            nr::TIMES => time::times(a[0]),
            nr::GETRLIMIT => system::prlimit(0, a[0] as u32, 0, a[1]),
            nr::SETRLIMIT => system::prlimit(0, a[0] as u32, a[1], 0),
            nr::PRLIMIT64 => system::prlimit(fd0, a[1] as u32, a[2], a[3]),
            nr::GETRUSAGE => process::getrusage(fd0, a[1]),
            nr::SYSINFO => system::sysinfo(a[0]),
            nr::PRCTL => system::prctl(a[0] as u32, a[1]),
            nr::ARCH_PRCTL => thread::arch_prctl(a[0] as u32, a[1]),
            nr::FUTEX => thread::futex(a[0], a[1] as u32, a[2] as u32, a[3], a[4], a[5] as u32),
            nr::SET_TID_ADDRESS => thread::set_tid_address(a[0]),
            nr::SET_ROBUST_LIST => Ok(0),
            nr::SCHED_GETAFFINITY => system::sched_getaffinity(a[1], a[2]),
            nr::SCHED_SETAFFINITY => Ok(0),
            nr::SCHED_GETSCHEDULER | nr::SCHED_GET_PRIORITY_MAX | nr::SCHED_GET_PRIORITY_MIN => Ok(0),
            nr::SCHED_GETPARAM => user::write(a[1], 0i32).map(|_| 0),
            nr::SCHED_SETSCHEDULER => Err(EPERM),
            // The raw system call returns 20 - nice.
            nr::GETPRIORITY => Ok(20),
            nr::SETPRIORITY => Ok(0),
            nr::GETCPU => {
                for p in &a[..2] {
                    if *p != 0 {
                        user::write(*p, 0u32)?;
                    }
                }
                Ok(0)
            }
            nr::GETRANDOM => system::getrandom(a[0], a[1]),
            // musl falls back to the calls above.
            nr::STATX | nr::MEMBARRIER | nr::RSEQ => Err(ENOSYS),
            _ => unsupported(n),
        }
    }
}
