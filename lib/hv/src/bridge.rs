//! The bridge: Veda's IPC for the programs of a Linux guest.
//!
//! A program in the driver VM uses Veda's objects as a Veda program does:
//! channels that carry messages and handles, events, memory objects
//! (VMOs) it maps, waits on their signals. The objects live in Veda; the
//! guest holds them through handles of its own, numbers that only the
//! monitor (`drivervm`) gives meaning to. So a guest reaches exactly the
//! objects it was given or created, and the protocols of Veda's drivers
//! (`audiodev`, `displaydev`, ...) work across the boundary unchanged.
//!
//! Each operation stands for the system call of its name. A program asks
//! Linux's bridge driver for it (an `ioctl` on `/dev/veda`, with the
//! request below and pointers into the program's memory); the driver asks
//! the monitor (a [`hypercall`] with the same request, its pointers made
//! guest-physical addresses of the driver's copies). Every request starts
//! with the operation's status: [`OK`], a Veda error code
//! (`vabi::Error`), or [`PENDING`] for a wait the monitor finishes later,
//! through the notification ring.
//!
//! A VMO a program maps is mapped by the monitor into a window of
//! guest-physical memory beyond the guest's RAM ([`VmoMap::gpa`]), and by
//! the driver into the program. Its pages are Veda's: what either side
//! writes, the other sees.
//!
//! The definitions here and those of Linux's bridge driver
//! (`include/uapi/linux/veda.h` in the Linux port) are the same.

/// The platform's hypercall for the bridge: `rbx` is the operation, `rcx`
/// the guest-physical address of its request. The result in `rax` is 0
/// when the monitor carried the operation out (its status is in the
/// request), negative when it could not read or write the request.
pub const HYPERCALL: u64 = 5;

/// Operations (`rbx` of the hypercall; also the `ioctl` numbers).
pub mod op {
    /// [`super::Setup`]: the driver's notification ring and vector.
    pub const SETUP: u32 = 1;
    pub const CLOSE: u32 = 2;
    pub const DUPLICATE: u32 = 3;
    pub const OBJECT_INFO: u32 = 4;
    pub const SIGNAL: u32 = 5;
    pub const WAIT: u32 = 6;
    /// Gives up a wait that is [`super::PENDING`] (the driver's).
    pub const CANCEL: u32 = 7;
    pub const CHANNEL_CREATE: u32 = 8;
    pub const CHANNEL_WRITE: u32 = 9;
    pub const CHANNEL_READ: u32 = 10;
    pub const EVENT_CREATE: u32 = 11;
    pub const VMO_CREATE: u32 = 12;
    pub const VMO_SIZE: u32 = 13;
    pub const VMO_READ: u32 = 14;
    pub const VMO_WRITE: u32 = 15;
    pub const VMO_MAP: u32 = 16;
    /// Undoes a [`VMO_MAP`] (the driver's, once no program maps it).
    pub const VMO_UNMAP: u32 = 17;
    pub const BOOTSTRAP: u32 = 18;
    pub const CLOCK: u32 = 19;
    /// [`super::Watch`]: the guest kernel's own, made of [`WAIT`] and
    /// [`CANCEL`]; the monitor never sees it.
    pub const WATCH: u32 = 20;
    /// [`super::VmoDmabuf`]: the guest kernel's own, made of [`VMO_MAP`]
    /// and [`VMO_UNMAP`].
    pub const VMO_DMABUF: u32 = 21;
    /// [`super::LogRead`].
    pub const LOG_READ: u32 = 22;
}

/// The operation was carried out.
pub const OK: u32 = 0;
/// The wait goes on: its completion comes through the ring.
pub const PENDING: u32 = u32::MAX;

/// The most bytes a channel message carries, and handles.
pub const MAX_BYTES: u32 = 64 * 1024;
pub const MAX_HANDLES: u32 = 64;
/// The most items a wait takes.
pub const MAX_WAIT_ITEMS: u32 = 1024;
/// The most bytes one VMO read or write moves.
pub const MAX_COPY: u64 = 64 * 1024;

/// [`op::SETUP`]: where the monitor reports finished waits, and the
/// interrupt it raises (on the guest's first processor) when it has.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Setup {
    pub status: u32,
    pub vector: u32,
    /// The ring: one page of guest memory ([`ring`]).
    pub ring: u64,
}

/// [`op::CLOSE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Close {
    pub status: u32,
    pub handle: u32,
}

/// [`op::DUPLICATE`]: `rights` 0 keeps the handle's.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Duplicate {
    pub status: u32,
    pub handle: u32,
    pub rights: u32,
    pub out: u32,
}

/// [`op::OBJECT_INFO`]: the handle's basic information.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectInfo {
    pub status: u32,
    pub handle: u32,
    pub koid: u64,
    pub object_type: u32,
    pub rights: u32,
}

/// [`op::SIGNAL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Signal {
    pub status: u32,
    pub handle: u32,
    pub clear: u32,
    pub set: u32,
}

/// [`op::WAIT`]: waits until a signal of an item is active, or the
/// deadline (Veda's monotonic time) passes; `items` points at `count`
/// `vabi::WaitItem`s, whose `observed` fields are filled in.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Wait {
    pub status: u32,
    pub count: u32,
    pub items: u64,
    pub deadline: u64,
    /// The driver's name for the wait, which its completion carries.
    pub key: u64,
    /// How many items have an active signal (output).
    pub satisfied: u32,
    pub _reserved: u32,
}

/// [`op::WATCH`]: a Linux file descriptor (`fd`, out) that is readable
/// while one of `signals` of `handle` is active, so that a program polls
/// Veda's objects with its other files. Reading it gives the signals active
/// then (a `u32`); a handle that is gone makes it an error.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Watch {
    pub status: u32,
    pub handle: u32,
    pub signals: u32,
    pub fd: i32,
}

/// [`op::VMO_DMABUF`]: a Linux dma-buf (`fd`, out) of `len` bytes of the
/// VMO `handle` from `offset`, which the monitor maps as for
/// [`op::VMO_MAP`] (`flags` too): Linux's drivers reach Veda's memory
/// through it (a display scans a picture out, a GPU renders into it). The
/// pages stay mapped while the dma-buf lives.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VmoDmabuf {
    pub status: u32,
    pub handle: u32,
    pub offset: u64,
    pub len: u64,
    pub flags: u32,
    pub fd: i32,
}

/// [`op::CANCEL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Cancel {
    pub status: u32,
    pub _reserved: u32,
    pub key: u64,
}

/// [`op::CHANNEL_CREATE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelCreate {
    pub status: u32,
    pub _reserved: u32,
    pub out: [u32; 2],
}

/// [`op::CHANNEL_WRITE`]: the handles go with the message (`consumed`:
/// they are gone, even if the write failed).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelWrite {
    pub status: u32,
    pub handle: u32,
    pub bytes: u64,
    pub handles: u64,
    pub bytes_len: u32,
    pub handles_len: u32,
    pub consumed: u32,
    pub _reserved: u32,
}

/// [`op::CHANNEL_READ`]: reads the next message into buffers of the given
/// capacities; the lengths say what it held (also when the buffers were
/// too small, `vabi::Error::BufferTooSmall`, and it stays queued).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelRead {
    pub status: u32,
    pub handle: u32,
    pub bytes: u64,
    pub handles: u64,
    pub bytes_capacity: u32,
    pub handles_capacity: u32,
    pub bytes_len: u32,
    pub handles_len: u32,
}

/// [`op::EVENT_CREATE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EventCreate {
    pub status: u32,
    pub out: u32,
}

/// [`op::VMO_CREATE`] (`flags`: `vabi::vmo_flags`) and [`op::VMO_SIZE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Vmo {
    pub status: u32,
    pub handle: u32,
    pub size: u64,
    pub flags: u32,
    pub _reserved: u32,
}

/// [`op::VMO_READ`] and [`op::VMO_WRITE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VmoCopy {
    pub status: u32,
    pub handle: u32,
    pub offset: u64,
    pub buffer: u64,
    pub len: u64,
}

/// [`op::VMO_MAP`] and [`op::VMO_UNMAP`]: `flags` are `vabi::map_flags`
/// (read, write). For a program, `gpa` is where to `mmap` `/dev/veda`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VmoMap {
    pub status: u32,
    pub handle: u32,
    pub offset: u64,
    pub len: u64,
    pub gpa: u64,
    pub flags: u32,
    pub _reserved: u32,
}

/// [`op::BOOTSTRAP`]: the handle the guest starts with in `role`
/// (`vabi::startup::role`: the registry). Each request makes a new one.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Bootstrap {
    pub status: u32,
    pub role: u32,
    pub out: u32,
    pub _reserved: u32,
}

/// [`op::CLOCK`]: how Veda's clocks follow the TSC, which the guest reads
/// as Veda does (`vabi::ClockInfo`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Clock {
    pub status: u32,
    pub _reserved: u32,
    pub info: vabi::ClockInfo,
}

/// [`op::LOG_READ`]: up to `len` bytes of Veda's log (every program's
/// lines, Linux's console among them) from `offset` (counted from the
/// first byte ever logged) into `buffer`, at most [`MAX_COPY`]. Out: `len`
/// is what was read, `next` the offset after it. The log keeps its latest
/// part only: what was read starts later than `offset` (at `next - len`)
/// if that is gone.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct LogRead {
    pub status: u32,
    pub _reserved: u32,
    pub offset: u64,
    pub buffer: u64,
    pub len: u64,
    pub next: u64,
}

/// The notification ring: a page the monitor writes finished waits into.
/// The monitor advances `HEAD`, the guest `TAIL`; each entry is a
/// [`Completion`].
pub mod ring {
    pub const HEAD: usize = 0;
    pub const TAIL: usize = 64;
    pub const ENTRIES: usize = 128;
    pub const ENTRY_SIZE: usize = 16;
    pub const CAPACITY: u32 = ((4096 - ENTRIES) / ENTRY_SIZE) as u32;
}

/// A finished wait, in the ring.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Completion {
    pub key: u64,
    pub status: u32,
    pub satisfied: u32,
}

const _: () = {
    assert!(core::mem::size_of::<Setup>() == 16);
    assert!(core::mem::size_of::<Wait>() == 40);
    assert!(core::mem::size_of::<ChannelWrite>() == 40);
    assert!(core::mem::size_of::<ChannelRead>() == 40);
    assert!(core::mem::size_of::<VmoMap>() == 40);
    assert!(core::mem::size_of::<Clock>() == 48);
    assert!(core::mem::size_of::<LogRead>() == 40);
    assert!(core::mem::size_of::<Completion>() == ring::ENTRY_SIZE);
};
