//! The GPU protocol: 3D rendering on the GPU.
//!
//! Veda's renderer serves it, in the driver VM (`guest/renderer`), on
//! Mesa's Gallium driver of the GPU over the GPU's Linux driver. Each
//! connection gets a context of its own: the client (an OpenGL ES
//! implementation, `vgl`) creates resources (buffers, textures) and submits
//! command streams in virglrenderer's protocol, which the renderer carries
//! out on Gallium. Contexts are isolated from each other: a context names
//! resources by handles the renderer assigned to it, and resolves only its
//! own.
//!
//! [`gpu::open`] gives the client the renderer's capabilities and a block of
//! memory shared with it:
//!
//! * the command area, where [`gpu::submit`] takes commands from;
//! * the shared area, which resources may use as their storage (the
//!   client's staging buffer and query results);
//! * the fence word: the number of the last fence that signaled, which the
//!   renderer updates (and signals the session's event) as fences signal.
//!   Waiting for a fence needs no call: wait for the event until the word
//!   reaches it.
//!
//! The renderer can render into memory it is given ([`gpu::import`]): the
//! display's pictures, into which the compositor draws its frames.

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
        /// The renderer's capability set 2 (`struct virgl_caps_v2`).
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
        /// The renderer's name (its GPU's), for display.
        pub renderer: String,
    }
}

protocol! {
    /// 3D rendering contexts on the GPU.
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
        /// command area. Replies once the renderer has executed them, as far
        /// as the shared memory goes (the GPU may still be busy with them).
        4 => fn submit(words: u32) -> Result<(), GpuError>;
        /// Queues a fence after everything submitted. Replies at once with
        /// its number; it signals when the GPU has done that work.
        5 => fn fence() -> Result<u64, GpuError>;
        /// Creates a 2D render target (`spec`: one level, single sampled,
        /// a format of 32-bit pixels) whose storage is `memory`, rows
        /// `stride` bytes apart: a display's picture, which the GPU renders
        /// into in place (the compositor draws its frames so). Once a fence
        /// after the drawing has signaled, the memory holds what was drawn.
        /// Returns its handle; `Unavailable` if the GPU cannot.
        6 => fn import(spec: ResourceSpec, stride: u32, memory: Vmo) -> Result<u32, GpuError>;
    }
}
