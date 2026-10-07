//! How the renderer reaches virglrenderer.
//!
//! In Veda, through the `gpu` service (the virtio-gpu driver), which owns
//! the device and gives each client a virgl context of its own; in host
//! tests, by calling the library directly. Either way the renderer sees
//! the same thing: a context to submit command streams to, resources it
//! creates by description, and a block of memory both sides read and write
//! (the staging area transfers go through, and query results).

use core::ops::Range;

use crate::backend::OutOfMemory;

/// A resource as virglrenderer creates it
/// (`virgl_renderer_resource_create_args`, without the handle).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct ResourceArgs {
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

/// The device failed or the context is gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lost;

/// A virgl context.
pub trait Transport {
    /// The host's capability set 2 (`struct virgl_caps_v2`).
    fn caps(&self) -> &[u8];

    /// The memory shared with the host: its address in this process and
    /// its size. The host writes into it while commands run (transfers
    /// from the host) and, for query results, later; read it with care.
    fn shared(&self) -> (*mut u8, usize);

    /// Creates a resource. `backing`, a range of the shared memory, is
    /// what transfers to and from the resource's own storage use (staging
    /// buffers and query results need one; other resources do not).
    /// Returns its handle, which the context's commands name it by.
    fn create_resource(&mut self, args: &ResourceArgs, backing: Option<Range<usize>>) -> Result<u32, OutOfMemory>;

    fn destroy_resource(&mut self, handle: u32);

    /// The most words one submission may have.
    fn max_submit_words(&self) -> usize;

    /// Runs commands. When it returns, the host has executed them as far
    /// as the shared memory goes: it has read what transfers to the host
    /// take from it and written what transfers from the host put there.
    /// (The GPU may still be working on them.)
    fn submit(&mut self, commands: &[u32]) -> Result<(), Lost>;

    /// Queues a fence after the commands submitted so far and returns its
    /// number (fences signal in order). The host also writes the results
    /// of finished queries when fences signal.
    fn fence(&mut self) -> Result<u64, Lost>;

    /// Whether a fence has signaled.
    fn signaled(&mut self, fence: u64) -> bool;

    /// Waits for a fence.
    fn wait(&mut self, fence: u64) -> Result<(), Lost>;
}
