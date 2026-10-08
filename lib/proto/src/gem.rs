//! The GEM protocol: an Intel GPU's buffers, address spaces, contexts and
//! submissions, as Mesa's iris driver uses them (Linux's i915 calls this
//! the Graphics Execution Manager).
//!
//! The renderer reaches it through the POSIX layer's DRM device
//! (`/dev/dri/renderD128`, `lib/posix/src/drm.rs`), which carries out
//! i915's ioctls with it: what i915 keeps per open file (its handles,
//! syncobjs, the buffers' last uses) the layer keeps, and this protocol
//! carries only what the GPU's driver must do. `intel-gpu` serves it for
//! the GPU; `gemsim`, a stand-in that executes nothing, serves it in QEMU.
//!
//! * Buffers are memory objects the client maps; the GPU sees them at the
//!   addresses each submission gives (iris chooses them: "softpin"), in
//!   the address space of the submission's context.
//! * Submissions on an engine complete in order. Each gets the next
//!   sequence number of its engine (numbers count from 1, device-wide);
//!   the session's fence page holds the last completed number of every
//!   engine, and its event is signaled as they move. Waiting needs no call.
//!   A submission the GPU loses (to a reset after a hang) completes too:
//!   `reset_stats` tells its context.

use vipc::{Bytes, enumeration, message, protocol};
use vrt::object::{Event, Vmo};

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GemError {
        /// A bad argument: an unknown handle, address space or context, a
        /// buffer at an address another holds, a batch outside its buffer.
        Invalid = 1,
        NoMemory = 2,
        /// The GPU failed (the session's work since is lost).
        Lost = 3,
        /// Something the GPU or its driver does not do.
        Unsupported = 4,
    }
}

impl core::fmt::Display for GemError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            GemError::Invalid => "invalid request",
            GemError::NoMemory => "out of memory",
            GemError::Lost => "the GPU failed",
            GemError::Unsupported => "not supported",
        })
    }
}

/// Engine classes (i915's `drm_i915_gem_engine_class`).
pub mod class {
    pub const RENDER: u16 = 0;
    pub const COPY: u16 = 1;
    pub const VIDEO: u16 = 2;
    pub const VIDEO_ENHANCE: u16 = 3;
    pub const COMPUTE: u16 = 4;
}

message! {
    /// An engine: its class and instance.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Engine {
        pub class: u16,
        pub instance: u16,
    }
}

message! {
    /// What the GPU is, as Mesa asks i915 (the POSIX layer answers i915's
    /// parameters and queries from it).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct GemDevice {
        /// The PCI identity and location.
        pub vendor: u16,
        pub device: u16,
        pub subvendor: u16,
        pub subdevice: u16,
        pub revision: u8,
        pub domain: u16,
        pub bus: u8,
        pub dev: u8,
        pub func: u8,
        /// The execution units present: slices, the subslices of each
        /// slice (dual subslices on Gfx12), and the units of each subslice
        /// (the same in all), as bits.
        pub slice_mask: u32,
        pub subslice_masks: alloc::vec::Vec<u32>,
        pub eu_mask: u32,
        /// The command streamers' timestamp clock.
        pub timestamp_hz: u64,
        /// The bytes of each address space.
        pub vm_size: u64,
        /// The memory buffers may take (system memory: integrated GPUs).
        pub memory: u64,
        /// The engines, in the order of the fence page's numbers.
        pub engines: alloc::vec::Vec<Engine>,
        /// The driver's name for people ("Intel Alder Lake-P").
        pub name: alloc::string::String,
    }
}

message! {
    /// A connection's session.
    pub struct GemSession {
        pub device: GemDevice,
        /// The last completed sequence number of every engine: a
        /// little-endian u64 each, from offset 0, read-only.
        pub fences: Vmo,
        /// Signaled whenever a number moves.
        pub progress: Event,
    }
}

message! {
    /// A buffer a submission uses, at its address in the context's address
    /// space, and whether the GPU writes it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ExecBuffer {
        pub handle: u32,
        pub address: u64,
        pub write: bool,
    }
}

message! {
    /// A sequence number of an engine (by its index in the device's
    /// engines).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Point {
        pub engine: u32,
        pub seqno: u64,
    }
}

message! {
    /// A submission: a batch buffer of commands, run on one of the
    /// context's engines once the points it waits for have passed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Exec {
        pub context: u32,
        /// The engine, by its index in the context's engines.
        pub engine: u32,
        /// The buffers the batch uses; the first is the batch.
        pub buffers: alloc::vec::Vec<ExecBuffer>,
        pub batch_start: u32,
        pub batch_len: u32,
        pub waits: alloc::vec::Vec<Point>,
    }
}

protocol! {
    /// An Intel GPU's buffers, address spaces, contexts and submissions.
    pub mod gem = "gem" {
        /// Starts the session.
        1 => fn open() -> Result<GemSession, GemError>;
        /// A new buffer of `size` bytes, zeroed: its handle and memory.
        /// Mapped write-combining by the client if `flags` has
        /// [`buffer::WRITE_COMBINING`], cached otherwise.
        2 => fn create(size: u64, flags: u32) -> Result<(u32, Vmo), GemError>;
        /// Forgets a buffer; the GPU keeps it until its last use is done.
        3 => fn close(handle: u32) -> ();
        /// A new address space, and forgetting one (once no context is in
        /// it; the GPU keeps it while it still works in it).
        4 => fn vm_create() -> Result<u32, GemError>;
        5 => fn vm_destroy(vm: u32) -> ();
        /// A context in an address space, with its engines (by class and
        /// instance), and forgetting one.
        6 => fn context_create(vm: u32, engines: alloc::vec::Vec<Engine>) -> Result<u32, GemError>;
        7 => fn context_destroy(context: u32) -> ();
        /// Queues a submission; its sequence number on its engine (by the
        /// device's index of the engine).
        8 => fn execute(exec: Exec) -> Result<Point, GemError>;
        /// The render engine's timestamp counter.
        9 => fn timestamp() -> Result<u64, GemError>;
        /// How many of a context's submissions were lost to a reset of the
        /// GPU: while running, and while waiting.
        10 => fn reset_stats(context: u32) -> Result<(u32, u32), GemError>;
        /// The GPU's own description of itself (i915's hardware
        /// configuration table), if it has one.
        11 => fn hwconfig() -> Result<Bytes, GemError>;
        /// Makes a buffer of memory the client holds: a display's picture,
        /// which the GPU then renders into while the display engine reads
        /// it. The memory is the processor's write-combining memory, and
        /// the GPU reaches it uncached too, so that what it writes is in
        /// memory, where the display engine reads, once it is done. Its
        /// handle and size.
        12 => fn import(memory: Vmo) -> Result<(u32, u64), GemError>;
    }
}

/// [`gem::create`]'s flags.
pub mod buffer {
    pub const WRITE_COMBINING: u32 = 1;
}
