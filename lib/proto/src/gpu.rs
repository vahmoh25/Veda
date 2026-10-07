//! The GPU protocol: 3D rendering on the host's GPU through virtio-gpu.
//!
//! The virtio-gpu driver serves it when the device has 3D ("virgl")
//! support. Each connection gets a virgl context of its own: the client
//! (an OpenGL ES implementation, `vgl`) creates resources (buffers,
//! textures) and submits command streams in virglrenderer's protocol; the
//! host turns them into OpenGL calls. Contexts are isolated from each
//! other: a context names resources by handles the driver assigned to it,
//! and the host resolves only resources attached to that context.
//!
//! [`gpu::open`] gives the client the host's capabilities and a block of
//! memory shared with the driver and the device:
//!
//! * the command area, where [`gpu::submit`] takes commands from;
//! * the shared area, which resources may use as their guest storage (the
//!   client's staging buffer and query results);
//! * the fence word: the number of the last fence that signaled, which the
//!   driver updates (and signals the session's event) as fences signal.
//!   Waiting for a fence needs no call: wait for the event until the word
//!   reaches it.

use alloc::string::String;
use vipc::{Bytes, enumeration, message, protocol};
use vrt::object::{Event, Vmo};

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GpuError {
        /// The device has no 3D support, or failed.
        Unavailable = 1,
        /// Out of memory, or over the connection's limits.
        NoMemory = 2,
        /// A bad argument (an unknown resource, a range outside the shared
        /// memory, an impossible resource description).
        Invalid = 3,
        /// `open` was already called (or not yet, for the other calls).
        State = 4,
    }
}

impl core::fmt::Display for GpuError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            GpuError::Unavailable => "no 3D device",
            GpuError::NoMemory => "out of memory",
            GpuError::Invalid => "invalid request",
            GpuError::State => "not open, or opened twice",
        })
    }
}

message! {
    /// A resource as virglrenderer creates it (Gallium's description).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ResourceSpec {
        pub target: u32,
        pub format: u32,
        pub bind: u32,
        pub width: u32,
        pub height: u32,
        pub depth: u32,
        pub array_size: u32,
        pub last_level: u32,
        pub nr_samples: u32,
        pub flags: u32,
    }
}

message! {
    /// A connection's context, as [`gpu::open`] sets it up.
    pub struct Session {
        /// The host's capability set 2 (`struct virgl_caps_v2`).
        pub caps: Bytes,
        /// The shared memory.
        pub memory: Vmo,
        /// The command area: offset and size in bytes.
        pub command_offset: u64,
        pub command_size: u64,
        /// The area resources may use as storage.
        pub shared_offset: u64,
        pub shared_size: u64,
        /// Where the last signaled fence's number is (a little-endian u64).
        pub fence_offset: u64,
        /// Signaled whenever a fence signals.
        pub fences: Event,
        /// The host's renderer, for display.
        pub renderer: String,
    }
}

protocol! {
    /// 3D rendering contexts on the host's GPU.
    pub mod gpu = "gpu" {
        /// Creates this connection's context, with `shared` bytes of shared
        /// memory (the driver may give less, at least 1 MiB).
        1 => fn open(shared: u64) -> Result<Session, GpuError>;
        /// Creates a resource and attaches it to the context. `backing`
        /// (offset, length) is a range of the shared area for its guest
        /// storage, or (0, 0). Returns its handle.
        2 => fn create(spec: ResourceSpec, backing_offset: u64, backing_len: u64) -> Result<u32, GpuError>;
        3 => fn destroy(resource: u32) -> ();
        /// Runs `words` 32-bit words of commands from the start of the
        /// command area. Replies once the host has executed them, as far as
        /// guest memory goes (the GPU may still be busy with them).
        4 => fn submit(words: u32) -> Result<(), GpuError>;
        /// Queues a fence after everything submitted. Replies at once with
        /// its number; it signals when the GPU has done that work.
        5 => fn fence() -> Result<u64, GpuError>;
    }
}
