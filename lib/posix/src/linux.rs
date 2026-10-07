//! The Linux x86-64 ABI that musl expects: system call numbers, flags and
//! the layouts of the structures passed through system calls.

#![allow(dead_code)]

/// System call numbers (`arch/x86_64/bits/syscall.h`).
pub mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const OPEN: usize = 2;
    pub const CLOSE: usize = 3;
    pub const STAT: usize = 4;
    pub const FSTAT: usize = 5;
    pub const LSTAT: usize = 6;
    pub const POLL: usize = 7;
    pub const LSEEK: usize = 8;
    pub const MMAP: usize = 9;
    pub const MPROTECT: usize = 10;
    pub const MUNMAP: usize = 11;
    pub const BRK: usize = 12;
    pub const RT_SIGACTION: usize = 13;
    pub const RT_SIGPROCMASK: usize = 14;
    pub const IOCTL: usize = 16;
    pub const PREAD64: usize = 17;
    pub const PWRITE64: usize = 18;
    pub const READV: usize = 19;
    pub const WRITEV: usize = 20;
    pub const ACCESS: usize = 21;
    pub const PIPE: usize = 22;
    pub const SELECT: usize = 23;
    pub const SCHED_YIELD: usize = 24;
    pub const MREMAP: usize = 25;
    pub const MSYNC: usize = 26;
    pub const MINCORE: usize = 27;
    pub const MADVISE: usize = 28;
    pub const DUP: usize = 32;
    pub const DUP2: usize = 33;
    pub const PAUSE: usize = 34;
    pub const NANOSLEEP: usize = 35;
    pub const GETITIMER: usize = 36;
    pub const ALARM: usize = 37;
    pub const SETITIMER: usize = 38;
    pub const GETPID: usize = 39;
    pub const SOCKET: usize = 41;
    pub const SENDTO: usize = 44;
    pub const RECVFROM: usize = 45;
    pub const SENDMSG: usize = 46;
    pub const RECVMSG: usize = 47;
    pub const SHUTDOWN: usize = 48;
    pub const SOCKETPAIR: usize = 53;
    pub const CLONE: usize = 56;
    pub const FORK: usize = 57;
    pub const VFORK: usize = 58;
    pub const EXECVE: usize = 59;
    pub const EXIT: usize = 60;
    pub const WAIT4: usize = 61;
    pub const KILL: usize = 62;
    pub const UNAME: usize = 63;
    pub const FCNTL: usize = 72;
    pub const FLOCK: usize = 73;
    pub const FSYNC: usize = 74;
    pub const FDATASYNC: usize = 75;
    pub const TRUNCATE: usize = 76;
    pub const FTRUNCATE: usize = 77;
    pub const GETDENTS: usize = 78;
    pub const GETCWD: usize = 79;
    pub const CHDIR: usize = 80;
    pub const FCHDIR: usize = 81;
    pub const RENAME: usize = 82;
    pub const MKDIR: usize = 83;
    pub const RMDIR: usize = 84;
    pub const CREAT: usize = 85;
    pub const LINK: usize = 86;
    pub const UNLINK: usize = 87;
    pub const SYMLINK: usize = 88;
    pub const READLINK: usize = 89;
    pub const CHMOD: usize = 90;
    pub const FCHMOD: usize = 91;
    pub const CHOWN: usize = 92;
    pub const FCHOWN: usize = 93;
    pub const LCHOWN: usize = 94;
    pub const UMASK: usize = 95;
    pub const GETTIMEOFDAY: usize = 96;
    pub const GETRLIMIT: usize = 97;
    pub const GETRUSAGE: usize = 98;
    pub const SYSINFO: usize = 99;
    pub const TIMES: usize = 100;
    pub const GETUID: usize = 102;
    pub const GETGID: usize = 104;
    pub const SETUID: usize = 105;
    pub const SETGID: usize = 106;
    pub const GETEUID: usize = 107;
    pub const GETEGID: usize = 108;
    pub const SETPGID: usize = 109;
    pub const GETPPID: usize = 110;
    pub const GETPGRP: usize = 111;
    pub const SETSID: usize = 112;
    pub const SETREUID: usize = 113;
    pub const SETREGID: usize = 114;
    pub const GETGROUPS: usize = 115;
    pub const SETGROUPS: usize = 116;
    pub const SETRESUID: usize = 117;
    pub const GETRESUID: usize = 118;
    pub const SETRESGID: usize = 119;
    pub const GETRESGID: usize = 120;
    pub const GETPGID: usize = 121;
    pub const GETSID: usize = 124;
    pub const RT_SIGPENDING: usize = 127;
    pub const RT_SIGTIMEDWAIT: usize = 128;
    pub const RT_SIGSUSPEND: usize = 130;
    pub const SIGALTSTACK: usize = 131;
    pub const UTIME: usize = 132;
    pub const MKNOD: usize = 133;
    pub const STATFS: usize = 137;
    pub const FSTATFS: usize = 138;
    pub const GETPRIORITY: usize = 140;
    pub const SETPRIORITY: usize = 141;
    pub const SCHED_GETPARAM: usize = 143;
    pub const SCHED_SETSCHEDULER: usize = 144;
    pub const SCHED_GETSCHEDULER: usize = 145;
    pub const SCHED_GET_PRIORITY_MAX: usize = 146;
    pub const SCHED_GET_PRIORITY_MIN: usize = 147;
    pub const MLOCK: usize = 149;
    pub const MUNLOCK: usize = 150;
    pub const MLOCKALL: usize = 151;
    pub const MUNLOCKALL: usize = 152;
    pub const PRCTL: usize = 157;
    pub const ARCH_PRCTL: usize = 158;
    pub const SETRLIMIT: usize = 160;
    pub const SYNC: usize = 162;
    pub const GETTID: usize = 186;
    pub const READAHEAD: usize = 187;
    pub const TKILL: usize = 200;
    pub const TIME: usize = 201;
    pub const FUTEX: usize = 202;
    pub const SCHED_SETAFFINITY: usize = 203;
    pub const SCHED_GETAFFINITY: usize = 204;
    pub const GETDENTS64: usize = 217;
    pub const SET_TID_ADDRESS: usize = 218;
    pub const FADVISE64: usize = 221;
    pub const CLOCK_GETTIME: usize = 228;
    pub const CLOCK_GETRES: usize = 229;
    pub const CLOCK_NANOSLEEP: usize = 230;
    pub const EXIT_GROUP: usize = 231;
    pub const TGKILL: usize = 234;
    pub const UTIMES: usize = 235;
    pub const WAITID: usize = 247;
    pub const OPENAT: usize = 257;
    pub const MKDIRAT: usize = 258;
    pub const MKNODAT: usize = 259;
    pub const FCHOWNAT: usize = 260;
    pub const FUTIMESAT: usize = 261;
    pub const NEWFSTATAT: usize = 262;
    pub const UNLINKAT: usize = 263;
    pub const RENAMEAT: usize = 264;
    pub const LINKAT: usize = 265;
    pub const SYMLINKAT: usize = 266;
    pub const READLINKAT: usize = 267;
    pub const FCHMODAT: usize = 268;
    pub const FACCESSAT: usize = 269;
    pub const PSELECT6: usize = 270;
    pub const PPOLL: usize = 271;
    pub const SET_ROBUST_LIST: usize = 273;
    pub const SYNC_FILE_RANGE: usize = 277;
    pub const UTIMENSAT: usize = 280;
    pub const FALLOCATE: usize = 285;
    pub const DUP3: usize = 292;
    pub const PIPE2: usize = 293;
    pub const PREADV: usize = 295;
    pub const PWRITEV: usize = 296;
    pub const PRLIMIT64: usize = 302;
    pub const SYNCFS: usize = 306;
    pub const GETCPU: usize = 309;
    pub const RENAMEAT2: usize = 316;
    pub const GETRANDOM: usize = 318;
    pub const MEMBARRIER: usize = 324;
    pub const STATX: usize = 332;
    pub const RSEQ: usize = 334;
    pub const CLOSE_RANGE: usize = 436;
    pub const FACCESSAT2: usize = 439;
}

/// `errno` values.
pub mod errno {
    pub const EPERM: isize = 1;
    pub const ENOENT: isize = 2;
    pub const ESRCH: isize = 3;
    pub const EINTR: isize = 4;
    pub const EIO: isize = 5;
    pub const ENXIO: isize = 6;
    pub const E2BIG: isize = 7;
    pub const ENOEXEC: isize = 8;
    pub const EBADF: isize = 9;
    pub const ECHILD: isize = 10;
    pub const EAGAIN: isize = 11;
    pub const ENOMEM: isize = 12;
    pub const EACCES: isize = 13;
    pub const EFAULT: isize = 14;
    pub const EBUSY: isize = 16;
    pub const EEXIST: isize = 17;
    pub const EXDEV: isize = 18;
    pub const ENODEV: isize = 19;
    pub const ENOTDIR: isize = 20;
    pub const EISDIR: isize = 21;
    pub const EINVAL: isize = 22;
    pub const ENFILE: isize = 23;
    pub const EMFILE: isize = 24;
    pub const ENOTTY: isize = 25;
    pub const EFBIG: isize = 27;
    pub const ENOSPC: isize = 28;
    pub const ESPIPE: isize = 29;
    pub const EROFS: isize = 30;
    pub const EMLINK: isize = 31;
    pub const EPIPE: isize = 32;
    pub const ERANGE: isize = 34;
    pub const ENAMETOOLONG: isize = 36;
    pub const ENOSYS: isize = 38;
    pub const ENOTEMPTY: isize = 39;
    pub const ELOOP: isize = 40;
    pub const EILSEQ: isize = 84;
    pub const ENOTSOCK: isize = 88;
    pub const EOPNOTSUPP: isize = 95;
    pub const EAFNOSUPPORT: isize = 97;
    pub const EISCONN: isize = 106;
    pub const ETIMEDOUT: isize = 110;
}

/// `open` flags.
pub mod o {
    pub const ACCMODE: u32 = 3;
    pub const RDONLY: u32 = 0;
    pub const WRONLY: u32 = 1;
    pub const RDWR: u32 = 2;
    pub const CREAT: u32 = 0o100;
    pub const EXCL: u32 = 0o200;
    pub const NOCTTY: u32 = 0o400;
    pub const TRUNC: u32 = 0o1000;
    pub const APPEND: u32 = 0o2000;
    pub const NONBLOCK: u32 = 0o4000;
    pub const DIRECTORY: u32 = 0o200000;
    pub const NOFOLLOW: u32 = 0o400000;
    pub const CLOEXEC: u32 = 0o2000000;
    pub const PATH: u32 = 0o10000000;
    pub const TMPFILE: u32 = 0o20000000;
}

/// `*at` flags.
pub mod at {
    /// `dirfd` meaning the working directory.
    pub const FDCWD: i32 = -100;
    pub const SYMLINK_NOFOLLOW: u32 = 0x100;
    pub const REMOVEDIR: u32 = 0x200;
    pub const EACCESS: u32 = 0x200;
    pub const SYMLINK_FOLLOW: u32 = 0x400;
    pub const EMPTY_PATH: u32 = 0x1000;
}

/// `access` modes.
pub mod access {
    pub const F_OK: u32 = 0;
    pub const X_OK: u32 = 1;
    pub const W_OK: u32 = 2;
    pub const R_OK: u32 = 4;
}

/// `st_mode` file types.
pub mod mode {
    pub const IFMT: u32 = 0o170000;
    pub const IFIFO: u32 = 0o010000;
    pub const IFCHR: u32 = 0o020000;
    pub const IFDIR: u32 = 0o040000;
    pub const IFREG: u32 = 0o100000;
}

/// `fcntl` commands and descriptor flags.
pub mod fcntl {
    pub const DUPFD: u32 = 0;
    pub const GETFD: u32 = 1;
    pub const SETFD: u32 = 2;
    pub const GETFL: u32 = 3;
    pub const SETFL: u32 = 4;
    pub const GETLK: u32 = 5;
    pub const SETLK: u32 = 6;
    pub const SETLKW: u32 = 7;
    pub const SETOWN: u32 = 8;
    pub const GETOWN: u32 = 9;
    pub const DUPFD_CLOEXEC: u32 = 1030;
    pub const FD_CLOEXEC: u32 = 1;
    /// `l_type` of a lock that is not held.
    pub const F_UNLCK: i16 = 2;
}

/// `lseek` origins.
pub mod seek {
    pub const SET: u32 = 0;
    pub const CUR: u32 = 1;
    pub const END: u32 = 2;
}

/// `mmap` protections and flags.
pub mod mman {
    pub const PROT_READ: u32 = 1;
    pub const PROT_WRITE: u32 = 2;
    pub const PROT_EXEC: u32 = 4;
    pub const MAP_SHARED: u32 = 0x01;
    pub const MAP_PRIVATE: u32 = 0x02;
    pub const MAP_SHARED_VALIDATE: u32 = 0x03;
    pub const MAP_TYPE: u32 = 0x0f;
    pub const MAP_FIXED: u32 = 0x10;
    pub const MAP_ANONYMOUS: u32 = 0x20;
    pub const MAP_POPULATE: u32 = 0x8000;
    pub const MAP_FIXED_NOREPLACE: u32 = 0x100000;
    pub const MADV_DONTNEED: u32 = 4;
    pub const MADV_FREE: u32 = 8;
    pub const MADV_REMOVE: u32 = 9;
}

/// Terminal and descriptor `ioctl` requests.
pub mod ioctl {
    pub const TCGETS: u32 = 0x5401;
    pub const TCSETS: u32 = 0x5402;
    pub const TCSETSW: u32 = 0x5403;
    pub const TCSETSF: u32 = 0x5404;
    pub const TCSBRK: u32 = 0x5409;
    pub const TCXONC: u32 = 0x540A;
    pub const TCFLSH: u32 = 0x540B;
    pub const TIOCSCTTY: u32 = 0x540E;
    pub const TIOCGPGRP: u32 = 0x540F;
    pub const TIOCSPGRP: u32 = 0x5410;
    pub const TIOCGWINSZ: u32 = 0x5413;
    pub const TIOCSWINSZ: u32 = 0x5414;
    pub const FIONREAD: u32 = 0x541B;
    pub const FIONBIO: u32 = 0x5421;
    pub const TIOCNOTTY: u32 = 0x5422;
    pub const FIONCLEX: u32 = 0x5450;
    pub const FIOCLEX: u32 = 0x5451;
}

/// `poll` events.
pub mod poll {
    pub const IN: i16 = 0x001;
    pub const PRI: i16 = 0x002;
    pub const OUT: i16 = 0x004;
    pub const ERR: i16 = 0x008;
    pub const HUP: i16 = 0x010;
    pub const NVAL: i16 = 0x020;
    pub const RDNORM: i16 = 0x040;
    pub const WRNORM: i16 = 0x100;
}

/// Clocks of `clock_gettime`.
pub mod clock {
    pub const REALTIME: u32 = 0;
    pub const MONOTONIC: u32 = 1;
    pub const PROCESS_CPUTIME_ID: u32 = 2;
    pub const THREAD_CPUTIME_ID: u32 = 3;
    pub const MONOTONIC_RAW: u32 = 4;
    pub const REALTIME_COARSE: u32 = 5;
    pub const MONOTONIC_COARSE: u32 = 6;
    pub const BOOTTIME: u32 = 7;
    /// `clock_nanosleep` flag: the time is absolute.
    pub const TIMER_ABSTIME: u32 = 1;
}

/// `clone` flags.
pub mod clone {
    pub const VM: u32 = 0x100;
    pub const FS: u32 = 0x200;
    pub const FILES: u32 = 0x400;
    pub const SIGHAND: u32 = 0x800;
    pub const VFORK: u32 = 0x4000;
    pub const THREAD: u32 = 0x10000;
    pub const SETTLS: u32 = 0x80000;
    pub const PARENT_SETTID: u32 = 0x100000;
    pub const CHILD_CLEARTID: u32 = 0x200000;
    pub const CHILD_SETTID: u32 = 0x1000000;
}

/// `futex` operations.
pub mod futex {
    pub const WAIT: u32 = 0;
    pub const WAKE: u32 = 1;
    pub const REQUEUE: u32 = 3;
    pub const CMP_REQUEUE: u32 = 4;
    pub const WAKE_OP: u32 = 5;
    pub const WAIT_BITSET: u32 = 9;
    pub const WAKE_BITSET: u32 = 10;
    pub const PRIVATE_FLAG: u32 = 128;
    pub const CLOCK_REALTIME: u32 = 256;
    pub const CMD_MASK: u32 = !(PRIVATE_FLAG | CLOCK_REALTIME);
}

/// `wait4`/`waitid` options and kinds.
pub mod wait {
    pub const NOHANG: u32 = 1;
    pub const UNTRACED: u32 = 2;
    pub const EXITED: u32 = 4;
    pub const NOWAIT: u32 = 0x0100_0000;
    pub const P_ALL: u32 = 0;
    pub const P_PID: u32 = 1;
    pub const P_PGID: u32 = 2;
    /// `si_code` of a child that exited / was killed.
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
}

/// Resource limits.
pub mod rlimit {
    pub const CPU: u32 = 0;
    pub const FSIZE: u32 = 1;
    pub const DATA: u32 = 2;
    pub const STACK: u32 = 3;
    pub const CORE: u32 = 4;
    pub const NOFILE: u32 = 7;
    pub const AS: u32 = 9;
    pub const INFINITY: u64 = u64::MAX;
}

/// Signals.
pub mod sig {
    pub const HUP: u32 = 1;
    pub const INT: u32 = 2;
    pub const QUIT: u32 = 3;
    pub const ILL: u32 = 4;
    pub const TRAP: u32 = 5;
    pub const ABRT: u32 = 6;
    pub const BUS: u32 = 7;
    pub const FPE: u32 = 8;
    pub const KILL: u32 = 9;
    pub const USR1: u32 = 10;
    pub const SEGV: u32 = 11;
    pub const USR2: u32 = 12;
    pub const PIPE: u32 = 13;
    pub const ALRM: u32 = 14;
    pub const TERM: u32 = 15;
    pub const CHLD: u32 = 17;
    pub const CONT: u32 = 18;
    pub const STOP: u32 = 19;
    pub const TSTP: u32 = 20;
    pub const TTIN: u32 = 21;
    pub const TTOU: u32 = 22;
    pub const URG: u32 = 23;
    pub const WINCH: u32 = 28;
    /// Signals are numbered 1 to `NSIG - 1`.
    pub const NSIG: u32 = 65;
    pub const DFL: usize = 0;
    pub const IGN: usize = 1;
    pub const SA_SIGINFO: u64 = 4;
    pub const SA_NODEFER: u64 = 0x4000_0000;
    pub const SA_RESETHAND: u64 = 0x8000_0000;
    pub const BLOCK: u32 = 0;
    pub const UNBLOCK: u32 = 1;
    pub const SETMASK: u32 = 2;
}

/// `struct stat` (the kernel's, which is also musl's on x86-64).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub _pad0: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i64,
    pub blocks: i64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub _unused: [i64; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

impl Timespec {
    pub fn from_ns(ns: u64) -> Timespec {
        Timespec { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as i64 }
    }

    /// Nanoseconds, or `None` for a negative or malformed time.
    pub fn to_ns(self) -> Option<u64> {
        if self.sec < 0 || !(0..1_000_000_000).contains(&self.nsec) {
            return None;
        }
        Some((self.sec as u64).saturating_mul(1_000_000_000).saturating_add(self.nsec as u64))
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timeval {
    pub sec: i64,
    pub usec: i64,
}

/// `struct winsize`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Winsize {
    pub row: u16,
    pub col: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// The kernel's `struct termios` (what `TCGETS` and `TCSETS` carry).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; 19],
}

/// `struct utsname`.
#[repr(C)]
pub struct Utsname {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

/// `struct rusage`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Rusage {
    pub utime: Timeval,
    pub stime: Timeval,
    pub rest: [i64; 14],
}

/// `struct tms`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Tms {
    pub utime: i64,
    pub stime: i64,
    pub cutime: i64,
    pub cstime: i64,
}

/// The kernel's `struct sysinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Sysinfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub _pad: u16,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
}

/// `struct rlimit`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Rlimit {
    pub cur: u64,
    pub max: u64,
}

/// `struct iovec`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Iovec {
    pub base: *mut u8,
    pub len: usize,
}

/// The kernel's `struct msghdr` (`sendmsg`, `recvmsg`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Msghdr {
    pub name: usize,
    pub namelen: u32,
    pub iov: usize,
    pub iovlen: usize,
    pub control: usize,
    pub controllen: usize,
    pub flags: i32,
}

/// Flags of `send` and `recv`.
pub mod msg {
    pub const OOB: u32 = 0x1;
    pub const PEEK: u32 = 0x2;
    pub const DONTWAIT: u32 = 0x40;
    pub const WAITALL: u32 = 0x100;
    pub const NOSIGNAL: u32 = 0x4000;
}

/// `struct pollfd`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Pollfd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

/// The kernel's `struct sigaction`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Sigaction {
    pub handler: usize,
    pub flags: u64,
    pub restorer: usize,
    pub mask: u64,
}

/// `struct statfs`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Statfs {
    pub kind: i64,
    pub bsize: i64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub fsid: [i32; 2],
    pub namelen: i64,
    pub frsize: i64,
    pub flags: i64,
    pub spare: [i64; 4],
}

/// `struct flock`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Flock {
    pub kind: i16,
    pub whence: i16,
    pub start: i64,
    pub len: i64,
    pub pid: i32,
}

/// `siginfo_t`, as `waitid` fills it in for a child and signal handlers
/// get it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Siginfo {
    pub signo: i32,
    pub errno: i32,
    pub code: i32,
    pub _pad: i32,
    pub pid: i32,
    pub uid: u32,
    pub status: i32,
    pub _rest: [u8; 100],
}

impl Default for Siginfo {
    fn default() -> Siginfo {
        Siginfo { signo: 0, errno: 0, code: 0, _pad: 0, pid: 0, uid: 0, status: 0, _rest: [0; 100] }
    }
}

/// Directory entry types of `getdents64`.
pub mod dt {
    pub const FIFO: u8 = 1;
    pub const CHR: u8 = 2;
    pub const DIR: u8 = 4;
    pub const REG: u8 = 8;
}

/// `arch_prctl` codes.
pub mod arch {
    pub const SET_FS: u32 = 0x1002;
    pub const GET_FS: u32 = 0x1003;
}

/// `prctl` options.
pub mod prctl {
    pub const SET_NAME: u32 = 15;
    pub const GET_NAME: u32 = 16;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_kernel() {
        use core::mem::{offset_of, size_of};
        assert_eq!(size_of::<Stat>(), 144);
        assert_eq!(offset_of!(Stat, mode), 24);
        assert_eq!(offset_of!(Stat, size), 48);
        assert_eq!(offset_of!(Stat, mtime), 88);
        assert_eq!(size_of::<Termios>(), 36);
        assert_eq!(size_of::<Utsname>(), 390);
        assert_eq!(size_of::<Rusage>(), 144);
        assert_eq!(offset_of!(Sysinfo, procs), 80);
        assert_eq!(offset_of!(Sysinfo, totalhigh), 88);
        assert_eq!(offset_of!(Sysinfo, mem_unit), 104);
        assert_eq!(size_of::<Sigaction>(), 32);
        assert_eq!(size_of::<Statfs>(), 120);
        assert_eq!(size_of::<Flock>(), 32);
        assert_eq!(size_of::<Siginfo>(), 128);
        assert_eq!(size_of::<Pollfd>(), 8);
        assert_eq!(size_of::<Msghdr>(), 56);
        assert_eq!(offset_of!(Msghdr, iov), 16);
        assert_eq!(offset_of!(Msghdr, control), 32);
        assert_eq!(offset_of!(Msghdr, flags), 48);
    }

    #[test]
    fn timespecs() {
        assert_eq!(Timespec::from_ns(1_500_000_001), Timespec { sec: 1, nsec: 500_000_001 });
        assert_eq!(Timespec { sec: 2, nsec: 5 }.to_ns(), Some(2_000_000_005));
        assert_eq!(Timespec { sec: -1, nsec: 0 }.to_ns(), None);
        assert_eq!(Timespec { sec: 0, nsec: 1_000_000_000 }.to_ns(), None);
    }
}
