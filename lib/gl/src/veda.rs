//! Running in Veda (feature `veda`): contexts on the host's GPU through the
//! `gpu` service (the virtio-gpu driver), or with the software renderer on
//! every CPU.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use vproto::gpu::{ResourceSpec, gpu};
use vrt::object::Event;
use vrt::pool::ThreadPool;
use vrt::vm::Mapping;

use crate::backend::OutOfMemory;
use crate::soft::{SoftBackend, Workers};
use crate::virgl::{Lost, ResourceArgs, Transport, VirglBackend};
use crate::{Config, Context};

impl Workers for ThreadPool {
    fn threads(&self) -> usize {
        ThreadPool::threads(self)
    }

    fn run(&self, f: &(dyn Fn(usize) + Sync)) {
        ThreadPool::run(self, f);
    }
}

/// A context rendering with the software renderer on a worker per CPU
/// (just below normal priority, so that the window system stays
/// responsive).
pub fn software_context(config: Config) -> Context {
    let pool = ThreadPool::named("gl-worker", vrt::pool::cpus().min(64), Some(vabi::priority::NORMAL - 2));
    Context::new(Box::new(SoftBackend::new(Box::new(pool))), config)
}

/// A context on the host's GPU if there is one (a virtio-gpu device with
/// 3D support), with the software renderer otherwise.
pub fn context(config: Config) -> Context {
    gpu_context(config).unwrap_or_else(|| software_context(config))
}

/// A context on the host's GPU, if there is one.
pub fn gpu_context(config: Config) -> Option<Context> {
    let t = GpuTransport::connect()?;
    match VirglBackend::new(Box::new(t)) {
        Ok(b) => Some(Context::new(Box::new(b), config)),
        Err(e) => {
            vrt::println!("cannot render on the GPU: {:?}", e);
            None
        }
    }
}

/// Shared memory to ask for: staging for a full-HD frame and then some,
/// and the query results.
const SHARED: u64 = 16 * 1024 * 1024 + 16 * 1024;
/// Programs started with the system may connect before the driver has
/// registered: this long after boot, they wait for it a little.
const BOOT_NS: u64 = 60_000_000_000;
const BOOT_WAIT_NS: u64 = 5_000_000_000;
/// How long a call to the driver may take before it counts as hung.
const CALL_NS: u64 = 10_000_000_000;

/// A virgl context through the `gpu` service.
pub struct GpuTransport {
    client: gpu::Client,
    caps: Vec<u8>,
    map: Mapping,
    command: Range<usize>,
    shared: Range<usize>,
    fence_word: usize,
    fences: Event,
    renderer: String,
}

impl GpuTransport {
    /// Opens a context, if the GPU service exists.
    pub fn connect() -> Option<GpuTransport> {
        let registered = vproto::with_registry(|r| r.list()).ok()?.ok()?.iter().any(|s| s == gpu::NAME);
        let booting = vrt::time::now_ns() < BOOT_NS;
        if !registered && !booting {
            return None;
        }
        // Connections to a service not yet registered wait for it.
        let client = gpu::Client::new(vproto::connect(gpu::NAME).ok()?);
        client.set_timeout(if registered { CALL_NS } else { BOOT_WAIT_NS });
        let session = match client.open(SHARED) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                vrt::println!("the GPU refused a context: {}", e);
                return None;
            }
            Err(_) => return None,
        };
        client.set_timeout(CALL_NS);
        let size = (session.shared_offset + session.shared_size) as usize;
        let map = Mapping::new(session.memory, size, vabi::map_flags::READ | vabi::map_flags::WRITE).ok()?;
        let co = session.command_offset as usize;
        let so = session.shared_offset as usize;
        Some(GpuTransport {
            client,
            caps: session.caps.0,
            map,
            command: co..co + session.command_size as usize,
            shared: so..so + session.shared_size as usize,
            fence_word: session.fence_offset as usize,
            fences: session.fences,
            renderer: session.renderer,
        })
    }

    /// The host's renderer.
    pub fn renderer(&self) -> &str {
        &self.renderer
    }

    /// The last fence that signaled.
    fn last_fence(&self) -> u64 {
        // SAFETY: the fence word, inside the mapping, aligned; the driver
        // writes it at any time.
        unsafe { core::ptr::read_volatile(self.map.as_ptr().add(self.fence_word) as *const u64) }
    }
}

impl Transport for GpuTransport {
    fn caps(&self) -> &[u8] {
        &self.caps
    }

    fn shared(&self) -> (*mut u8, usize) {
        // SAFETY: the shared area lies inside the mapping.
        (unsafe { self.map.as_ptr().add(self.shared.start) }, self.shared.len())
    }

    fn create_resource(&mut self, args: &ResourceArgs, backing: Option<Range<usize>>) -> Result<u32, OutOfMemory> {
        let a = args;
        let spec = ResourceSpec {
            target: a.target,
            format: a.format,
            bind: a.bind,
            width: a.width,
            height: a.height,
            depth: a.depth,
            array_size: a.array_size,
            last_level: a.last_level,
            nr_samples: a.nr_samples,
            flags: a.flags,
        };
        let (off, len) = backing.map_or((0, 0), |r| (r.start as u64, r.len() as u64));
        match self.client.create(spec, off, len) {
            Ok(Ok(h)) => Ok(h),
            _ => Err(OutOfMemory),
        }
    }

    fn destroy_resource(&mut self, handle: u32) {
        let _ = self.client.destroy(handle);
    }

    fn max_submit_words(&self) -> usize {
        self.command.len() / 4
    }

    fn submit(&mut self, commands: &[u32]) -> Result<(), Lost> {
        let bytes = commands.len() * 4;
        if bytes > self.command.len() {
            return Err(Lost);
        }
        // SAFETY: the command area, inside the mapping; the driver reads it
        // only during the call.
        unsafe {
            core::ptr::copy_nonoverlapping(
                commands.as_ptr() as *const u8,
                self.map.as_ptr().add(self.command.start),
                bytes,
            );
        }
        match self.client.submit(commands.len() as u32) {
            Ok(Ok(())) => Ok(()),
            _ => Err(Lost),
        }
    }

    fn fence(&mut self) -> Result<u64, Lost> {
        match self.client.fence() {
            Ok(Ok(f)) => Ok(f),
            _ => Err(Lost),
        }
    }

    fn signaled(&mut self, fence: u64) -> bool {
        self.last_fence() >= fence
    }

    fn wait(&mut self, fence: u64) -> Result<(), Lost> {
        loop {
            // Clear, then look: a fence signaling in between leaves the
            // event signaled again.
            let _ = self.fences.clear();
            if self.last_fence() >= fence {
                return Ok(());
            }
            if self.fences.wait(vabi::signals::SIGNALED, vrt::time::now_ns() + CALL_NS).is_err() {
                return Err(Lost);
            }
        }
    }
}
