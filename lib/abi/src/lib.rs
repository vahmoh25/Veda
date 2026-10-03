//! The Vindows kernel ABI.
//!
//! This crate is the single source of truth for the interface between the
//! microkernel and user space: system call numbers and calling convention,
//! status codes, handle rights, object signals, and the plain-data structures
//! exchanged through system calls. It contains no code that depends on being
//! in the kernel or in user space.
//!
//! # Calling convention
//!
//! System calls use the `syscall` instruction. `rax` holds the call number
//! (see [`nr`]), arguments are passed in `rdi, rsi, rdx, r10, r8, r9`. On
//! return `rax` holds a status: non-negative values are success results,
//! negative values are `-(error code)` (see [`Error`]). Some calls return a
//! second value in `rdx`. `rcx` and `r11` are clobbered by the instruction;
//! all other registers are preserved.
//!
//! # Objects and handles
//!
//! Kernel objects (processes, threads, channels, events, memory objects,
//! interrupts, I/O port ranges and resources) are only reachable through
//! *handles*: per-process 32-bit names that carry a set of [`Rights`].
//! Handles are capabilities — the only way to gain access to an object is to
//! be given a handle to it (through a channel message or at process start).

#![no_std]

pub mod startup;

use core::fmt;

/// A handle value as seen by user space. `0` is never a valid handle.
pub type RawHandle = u32;

/// The invalid handle.
pub const INVALID_HANDLE: RawHandle = 0;

/// Deadline value meaning "wait forever".
pub const DEADLINE_INFINITE: u64 = u64::MAX;

/// Page size used by the kernel for all mappings.
pub const PAGE_SIZE: usize = 4096;

/// Maximum number of data bytes in one channel message.
pub const CHANNEL_MAX_BYTES: usize = 64 * 1024;
/// Maximum number of handles in one channel message.
pub const CHANNEL_MAX_HANDLES: usize = 64;
/// Maximum number of items in one `object_wait_many` call.
pub const WAIT_MANY_MAX: usize = 64;
/// Maximum length of process and thread names.
pub const NAME_MAX: usize = 32;

/// System call numbers.
pub mod nr {
    // --- misc ---------------------------------------------------------
    /// `debug_write(ptr, len)`: append text to the kernel log.
    pub const DEBUG_WRITE: usize = 0;
    /// `log_read(offset, buf, len) -> bytes` (rdx = next offset).
    pub const LOG_READ: usize = 1;
    /// `clock_get(clock_id) -> nanoseconds`.
    pub const CLOCK_GET: usize = 2;
    /// `sleep(deadline_ns)`.
    pub const SLEEP: usize = 3;
    /// `yield()`.
    pub const YIELD: usize = 4;
    /// `system_info(buf: *mut SystemInfo)`.
    pub const SYSTEM_INFO: usize = 5;
    /// `system_power(resource, action)`.
    pub const SYSTEM_POWER: usize = 6;
    /// `random(buf, len)`: fill a buffer with kernel entropy.
    pub const RANDOM: usize = 7;

    // --- handles and objects ------------------------------------------
    /// `handle_close(h)`.
    pub const HANDLE_CLOSE: usize = 10;
    /// `handle_duplicate(h, rights) -> h2`.
    pub const HANDLE_DUPLICATE: usize = 11;
    /// `handle_replace(h, rights) -> h2` (closes `h`).
    pub const HANDLE_REPLACE: usize = 12;
    /// `object_info(h, topic, buf, len) -> bytes`.
    pub const OBJECT_INFO: usize = 13;
    /// `object_signal(h, clear_mask, set_mask)`: user signals only.
    pub const OBJECT_SIGNAL: usize = 14;
    /// `object_wait_one(h, signals, deadline) -> observed`.
    pub const OBJECT_WAIT_ONE: usize = 15;
    /// `object_wait_many(items: *mut WaitItem, count, deadline) -> satisfied`.
    pub const OBJECT_WAIT_MANY: usize = 16;

    // --- channels -----------------------------------------------------
    /// `channel_create(out: *mut [RawHandle; 2])`.
    pub const CHANNEL_CREATE: usize = 20;
    /// `channel_write(h, bytes, n_bytes, handles, n_handles)`.
    pub const CHANNEL_WRITE: usize = 21;
    /// `channel_read(h, bytes, bytes_cap, handles, handles_cap, out: *mut [u32; 2])`.
    pub const CHANNEL_READ: usize = 22;

    // --- events -------------------------------------------------------
    /// `event_create() -> h`.
    pub const EVENT_CREATE: usize = 30;

    // --- memory objects -----------------------------------------------
    /// `vmo_create(size, flags) -> h`.
    pub const VMO_CREATE: usize = 40;
    /// `vmo_read(h, offset, buf, len)`.
    pub const VMO_READ: usize = 41;
    /// `vmo_write(h, offset, buf, len)`.
    pub const VMO_WRITE: usize = 42;
    /// `vmo_get_size(h) -> size`.
    pub const VMO_GET_SIZE: usize = 43;
    /// `vmo_create_physical(resource, paddr, size, cache_policy) -> h`.
    pub const VMO_CREATE_PHYSICAL: usize = 44;
    /// `vmo_create_contiguous(resource, size) -> h` (DMA memory).
    pub const VMO_CREATE_CONTIGUOUS: usize = 45;
    /// `vmo_phys_addr(h, offset) -> paddr` (contiguous/physical VMOs only).
    pub const VMO_PHYS_ADDR: usize = 46;

    // --- address spaces -----------------------------------------------
    /// `vm_map(process, vmo, vmo_offset, len, addr, flags) -> addr`.
    pub const VM_MAP: usize = 50;
    /// `vm_unmap(process, addr, len)`.
    pub const VM_UNMAP: usize = 51;
    /// `vm_protect(process, addr, len, flags)`.
    pub const VM_PROTECT: usize = 52;

    // --- processes and threads ----------------------------------------
    /// `process_create(name, name_len) -> h`.
    pub const PROCESS_CREATE: usize = 60;
    /// `process_start(process, thread, entry, stack, arg_handle, arg1)`.
    pub const PROCESS_START: usize = 61;
    /// `process_exit(code) -> !`.
    pub const PROCESS_EXIT: usize = 62;
    /// `process_kill(h)`.
    pub const PROCESS_KILL: usize = 63;
    /// `thread_create(process, name, name_len) -> h`.
    pub const THREAD_CREATE: usize = 64;
    /// `thread_start(thread, entry, stack, arg0, arg1)`.
    pub const THREAD_START: usize = 65;
    /// `thread_exit() -> !`.
    pub const THREAD_EXIT: usize = 66;
    /// `thread_set_priority(h, priority)`.
    pub const THREAD_SET_PRIORITY: usize = 67;
    /// `thread_set_fs_base(value)`: thread-local storage base.
    pub const THREAD_SET_FS_BASE: usize = 68;
    /// `process_list(buf: *mut ProcessInfo, cap) -> total`.
    pub const PROCESS_LIST: usize = 69;
    /// `process_open(resource, koid) -> h`.
    pub const PROCESS_OPEN: usize = 70;

    // --- futexes ------------------------------------------------------
    /// `futex_wait(addr, expected, deadline)`.
    pub const FUTEX_WAIT: usize = 75;
    /// `futex_wake(addr, count) -> woken`.
    pub const FUTEX_WAKE: usize = 76;

    // --- hardware access (requires resources) -------------------------
    /// `resource_create(parent, kind, base, size) -> h`.
    pub const RESOURCE_CREATE: usize = 80;
    /// `ioport_create(resource, base, count) -> h`.
    pub const IOPORT_CREATE: usize = 81;
    /// `ioport_read(h, port, width) -> value`.
    pub const IOPORT_READ: usize = 82;
    /// `ioport_write(h, port, width, value)`.
    pub const IOPORT_WRITE: usize = 83;
    /// `irq_create(resource, irq, flags) -> h`.
    pub const IRQ_CREATE: usize = 84;
    /// `irq_ack(h)`: re-arm an interrupt after handling it.
    pub const IRQ_ACK: usize = 85;
    /// `msi_create(resource, out: *mut MsiInfo) -> h`.
    pub const MSI_CREATE: usize = 86;
}

/// Error codes returned (negated) by system calls.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An argument was invalid.
    InvalidArgs = 1,
    /// The handle does not exist in the caller's handle table.
    BadHandle = 2,
    /// The handle refers to an object of the wrong type.
    WrongType = 3,
    /// The handle lacks the rights required for the operation.
    AccessDenied = 4,
    /// Not enough memory.
    NoMemory = 5,
    /// The requested entity does not exist.
    NotFound = 6,
    /// The operation would block (non-blocking call, empty queue, ...).
    ShouldWait = 7,
    /// The deadline passed.
    TimedOut = 8,
    /// The other end of a channel was closed.
    PeerClosed = 9,
    /// The supplied buffer is too small.
    BufferTooSmall = 10,
    /// An offset or address lies outside the valid range.
    OutOfRange = 11,
    /// The entity already exists.
    AlreadyExists = 12,
    /// The operation is not supported by this object or system.
    NotSupported = 13,
    /// The object is in the wrong state for the operation.
    BadState = 14,
    /// The operation was cancelled.
    Canceled = 15,
    /// A resource limit was reached.
    LimitReached = 16,
    /// The resource is busy.
    Busy = 17,
    /// An I/O error occurred.
    Io = 18,
    /// Internal kernel error.
    Internal = 19,
    /// A user pointer was invalid.
    Fault = 20,
    /// Unknown system call number.
    UnknownSyscall = 21,
}

impl Error {
    /// All error codes, for lookups.
    pub const ALL: [Error; 21] = [
        Error::InvalidArgs,
        Error::BadHandle,
        Error::WrongType,
        Error::AccessDenied,
        Error::NoMemory,
        Error::NotFound,
        Error::ShouldWait,
        Error::TimedOut,
        Error::PeerClosed,
        Error::BufferTooSmall,
        Error::OutOfRange,
        Error::AlreadyExists,
        Error::NotSupported,
        Error::BadState,
        Error::Canceled,
        Error::LimitReached,
        Error::Busy,
        Error::Io,
        Error::Internal,
        Error::Fault,
        Error::UnknownSyscall,
    ];

    /// Converts a positive error code back into an [`Error`].
    pub fn from_code(code: i32) -> Option<Error> {
        Error::ALL.iter().copied().find(|e| *e as i32 == code)
    }

    /// Encodes the error as a raw syscall return value.
    pub const fn as_return(self) -> isize {
        -(self as i32 as isize)
    }

    /// Decodes a raw syscall return value.
    pub fn from_return(ret: isize) -> core::result::Result<usize, Error> {
        if ret >= 0 { Ok(ret as usize) } else { Err(Error::from_code((-ret) as i32).unwrap_or(Error::Internal)) }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Error::InvalidArgs => "invalid arguments",
            Error::BadHandle => "bad handle",
            Error::WrongType => "wrong object type",
            Error::AccessDenied => "access denied",
            Error::NoMemory => "out of memory",
            Error::NotFound => "not found",
            Error::ShouldWait => "operation would block",
            Error::TimedOut => "timed out",
            Error::PeerClosed => "peer closed",
            Error::BufferTooSmall => "buffer too small",
            Error::OutOfRange => "out of range",
            Error::AlreadyExists => "already exists",
            Error::NotSupported => "not supported",
            Error::BadState => "bad state",
            Error::Canceled => "canceled",
            Error::LimitReached => "limit reached",
            Error::Busy => "busy",
            Error::Io => "I/O error",
            Error::Internal => "internal error",
            Error::Fault => "invalid user pointer",
            Error::UnknownSyscall => "unknown system call",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

/// Result type used throughout the ABI.
pub type Result<T> = core::result::Result<T, Error>;

/// The rights attached to a handle.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rights(pub u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    /// May be duplicated.
    pub const DUPLICATE: Rights = Rights(1 << 0);
    /// May be sent to another process through a channel.
    pub const TRANSFER: Rights = Rights(1 << 1);
    /// May read (channel messages, VMO contents, port input).
    pub const READ: Rights = Rights(1 << 2);
    /// May write (channel messages, VMO contents, port output).
    pub const WRITE: Rights = Rights(1 << 3);
    /// VMO may be mapped executable.
    pub const EXECUTE: Rights = Rights(1 << 4);
    /// VMO may be mapped.
    pub const MAP: Rights = Rights(1 << 5);
    /// May query object information.
    pub const GET_INFO: Rights = Rights(1 << 6);
    /// May change user signals.
    pub const SIGNAL: Rights = Rights(1 << 7);
    /// May wait on the object.
    pub const WAIT: Rights = Rights(1 << 8);
    /// May control a process/thread (map into it, start, kill, prioritise).
    pub const MANAGE: Rights = Rights(1 << 9);

    pub const BASIC: Rights = Rights(Self::DUPLICATE.0 | Self::TRANSFER.0 | Self::GET_INFO.0 | Self::WAIT.0);
    pub const IO: Rights = Rights(Self::READ.0 | Self::WRITE.0);
    pub const ALL: Rights = Rights(0x3FF);

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }
}

impl core::ops::BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights {
        self.union(rhs)
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rights({:#x})", self.0)
    }
}

/// Object signal bits, observed through `object_wait_*`.
pub mod signals {
    /// Channel has at least one message queued.
    pub const READABLE: u32 = 1 << 0;
    /// Channel can accept another message.
    pub const WRITABLE: u32 = 1 << 1;
    /// The other end of the channel has been closed.
    pub const PEER_CLOSED: u32 = 1 << 2;
    /// Event or interrupt is signalled.
    pub const SIGNALED: u32 = 1 << 3;
    /// Process or thread has terminated.
    pub const TERMINATED: u32 = 1 << 4;
    /// Signals reserved for applications (settable with `object_signal`).
    pub const USER_ALL: u32 = 0xFF00_0000;
    pub const USER_0: u32 = 1 << 24;
    pub const USER_1: u32 = 1 << 25;
    pub const USER_2: u32 = 1 << 26;
    pub const USER_3: u32 = 1 << 27;
}

/// One entry of an `object_wait_many` call.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaitItem {
    pub handle: RawHandle,
    /// Signals of interest.
    pub signals: u32,
    /// Signals active when the wait returned (output).
    pub observed: u32,
    pub _reserved: u32,
}

/// Clock identifiers for `clock_get`.
pub mod clock {
    /// Nanoseconds since boot; never goes backwards.
    pub const MONOTONIC: usize = 0;
    /// Nanoseconds since 1970-01-01 00:00:00 in the machine's local time zone
    /// (the RTC time Vindows was booted with, advanced by the monotonic clock).
    pub const REALTIME: usize = 1;
}

/// Kernel object types (reported by `object_info`).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Process = 1,
    Thread = 2,
    Channel = 3,
    Event = 4,
    Vmo = 5,
    Interrupt = 6,
    IoPorts = 7,
    Resource = 8,
}

/// Topics for `object_info`.
pub mod info_topic {
    /// [`super::HandleBasicInfo`] for any handle.
    pub const HANDLE_BASIC: usize = 1;
    /// [`super::ProcessInfo`] for a process handle.
    pub const PROCESS: usize = 2;
    /// [`super::VmoInfo`] for a VMO handle.
    pub const VMO: usize = 3;
    /// [`super::ThreadInfo`] for a thread handle.
    pub const THREAD: usize = 4;
}

/// Basic information about any handle.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HandleBasicInfo {
    /// Kernel object id, unique for the lifetime of the system.
    pub koid: u64,
    pub object_type: u32,
    pub rights: u32,
}

/// Process states reported in [`ProcessInfo::state`].
pub mod process_state {
    pub const RUNNING: u32 = 1;
    pub const EXITED: u32 = 2;
    pub const KILLED: u32 = 3;
    pub const CRASHED: u32 = 4;
}

/// Information about a process.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub koid: u64,
    pub parent_koid: u64,
    pub name: [u8; NAME_MAX],
    pub state: u32,
    pub threads: u32,
    pub exit_code: i64,
    /// Resident memory (committed pages) in bytes.
    pub memory_bytes: u64,
    /// Total CPU time consumed by all threads, in nanoseconds.
    pub cpu_time_ns: u64,
    pub handles: u32,
    pub _reserved: u32,
}

impl Default for ProcessInfo {
    fn default() -> Self {
        ProcessInfo {
            koid: 0,
            parent_koid: 0,
            name: [0; NAME_MAX],
            state: 0,
            threads: 0,
            exit_code: 0,
            memory_bytes: 0,
            cpu_time_ns: 0,
            handles: 0,
            _reserved: 0,
        }
    }
}

impl ProcessInfo {
    pub fn name(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }
}

/// Information about a thread.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadInfo {
    pub koid: u64,
    pub state: u32,
    pub priority: u32,
    pub cpu_time_ns: u64,
}

/// Information about a VMO.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VmoInfo {
    pub size: u64,
    pub committed_bytes: u64,
    pub flags: u32,
    pub _reserved: u32,
}

/// Flags for `vmo_create`.
pub mod vmo_flags {
    /// Commit (allocate) all pages immediately instead of on first touch.
    pub const COMMIT: usize = 1 << 0;
}

/// Flags for `vm_map` / `vm_protect`.
pub mod map_flags {
    pub const READ: usize = 1 << 0;
    pub const WRITE: usize = 1 << 1;
    pub const EXECUTE: usize = 1 << 2;
    /// Map at exactly the given address (fail if occupied).
    pub const FIXED: usize = 1 << 3;
    /// Fault in all pages immediately.
    pub const COMMIT: usize = 1 << 4;
}

/// Cache policies for physical VMOs.
pub mod cache_policy {
    pub const WRITE_BACK: usize = 0;
    pub const WRITE_COMBINING: usize = 1;
    pub const UNCACHED: usize = 2;
}

/// Lowest address user space may map.
pub const USER_SPACE_START: usize = 0x0000_0000_0001_0000;
/// One past the highest user-space address.
pub const USER_SPACE_END: usize = 0x0000_7FFF_FFFF_0000;

/// Thread priorities (0 = lowest, 31 = highest).
pub mod priority {
    pub const IDLE: usize = 0;
    pub const LOW: usize = 8;
    pub const NORMAL: usize = 16;
    pub const HIGH: usize = 24;
    pub const REALTIME: usize = 30;
    pub const MAX: usize = 31;
}

/// Kinds of hardware [`ObjectType::Resource`]s.
pub mod resource_kind {
    /// Grants everything; held by `init` only.
    pub const ROOT: usize = 0;
    /// A range of I/O ports.
    pub const IOPORT: usize = 1;
    /// A range of interrupt lines (GSIs).
    pub const IRQ: usize = 2;
    /// A range of physical MMIO addresses.
    pub const MMIO: usize = 3;
    /// Allocation of DMA-capable (contiguous) memory and MSI vectors.
    pub const DMA: usize = 4;
    /// Power management (shutdown/reboot).
    pub const POWER: usize = 5;
    /// Enumerating and opening arbitrary processes.
    pub const PROCESS: usize = 6;
}

/// Flags for `irq_create`.
pub mod irq_flags {
    /// `irq` is a legacy ISA IRQ number (0-15) rather than a GSI; the kernel
    /// applies the firmware's interrupt source overrides.
    pub const ISA: usize = 1 << 0;
    /// Level triggered (default: edge).
    pub const LEVEL: usize = 1 << 1;
    /// Active low (default: active high).
    pub const ACTIVE_LOW: usize = 1 << 2;
}

/// Result of `msi_create`: program these into the device's MSI capability.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MsiInfo {
    pub address: u64,
    pub data: u32,
    pub vector: u32,
}

/// Actions for `system_power`.
pub mod power_action {
    pub const POWER_OFF: usize = 1;
    pub const REBOOT: usize = 2;
}

/// Global system information returned by `system_info`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemInfo {
    /// Kernel version string (NUL padded).
    pub version: [u8; 32],
    pub cpu_count: u32,
    pub page_size: u32,
    pub total_memory: u64,
    pub free_memory: u64,
    pub uptime_ns: u64,
    pub process_count: u32,
    pub thread_count: u32,
    /// Accumulated idle time across all CPUs (for CPU usage graphs).
    pub idle_time_ns: u64,
    /// CPU brand string from CPUID (NUL padded).
    pub cpu_brand: [u8; 48],
}

impl Default for SystemInfo {
    fn default() -> Self {
        SystemInfo {
            version: [0; 32],
            cpu_count: 0,
            page_size: 0,
            total_memory: 0,
            free_memory: 0,
            uptime_ns: 0,
            process_count: 0,
            thread_count: 0,
            idle_time_ns: 0,
            cpu_brand: [0; 48],
        }
    }
}

/// Exit code reported for processes terminated by an unhandled CPU fault.
pub const EXIT_CODE_CRASHED: i64 = -1001;
/// Exit code reported for processes killed with `process_kill`.
pub const EXIT_CODE_KILLED: i64 = -1002;
/// Exit code of a program that panicked (set by the runtime's panic handler).
pub const EXIT_CODE_PANICKED: i64 = -1003;

/// Boot-time information the kernel hands to `init` in a VMO.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelBootInfo {
    pub framebuffer_width: u32,
    pub framebuffer_height: u32,
    /// Bytes per scan line.
    pub framebuffer_pitch: u32,
    /// 1 = BGRX, 2 = RGBX.
    pub framebuffer_format: u32,
    pub framebuffer_size: u64,
    pub initrd_size: u64,
    pub cmdline: [u8; 256],
    pub cmdline_len: u32,
    pub cpu_count: u32,
}
