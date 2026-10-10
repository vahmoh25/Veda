//! Running in Veda (feature `veda`): contexts on the GPU through the `gpu`
//! service (Veda's renderer, in the driver VM), or with the software
//! renderer on every CPU.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use vproto::gpu::{ResourceSpec, gpu};
use vrt::object::{Event, Vmo};
use vrt::pool::ThreadPool;
use vrt::vm::Mapping;

use crate::backend::{External, OutOfMemory};
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

/// A context on the GPU if there is one (the `gpu` service), with the
/// software renderer otherwise.
pub fn context(config: Config) -> Context {
    gpu_context(config).unwrap_or_else(|| software_context(config))
}

/// A context on the GPU, if there is one.
pub fn gpu_context(config: Config) -> Option<Context> {
    backend_context(GpuTransport::connect()?, config)
}

/// A context through the renderer serving the `gpu` protocol under
/// `service` (tests start one of their own), waiting up to `wait_ns` for it
/// to register.
pub fn gpu_context_on(service: &str, wait_ns: u64, config: Config) -> Option<Context> {
    backend_context(GpuTransport::connect_to(service, wait_ns)?, config)
}

fn backend_context(t: GpuTransport, config: Config) -> Option<Context> {
    let renderer = String::from(t.renderer());
    match VirglBackend::new(Box::new(t)) {
        Ok(b) => {
            let mut c = Context::new(Box::new(b), config);
            // A framebuffer without the buffers asked for draws, but wrongly
            // (with no depth buffer every depth test passes): say so.
            for (bits, pname, what) in [
                (config.depth_bits, crate::gl::DEPTH_BITS, "depth"),
                (config.stencil_bits, crate::gl::STENCIL_BITS, "stencil"),
            ] {
                if bits > 0 && c.get_integer(pname) == 0 {
                    vrt::println!("{renderer}: the window has no {what} buffer: the renderer could not make one");
                }
            }
            Some(c)
        }
        Err(e) => {
            vrt::println!("cannot render on the GPU: {:?}", e);
            None
        }
    }
}

/// Shared memory to ask for: staging for a full-HD frame and then some,
/// and the query results.
const SHARED: u64 = 16 * 1024 * 1024 + 16 * 1024;
/// Programs started with the system may connect before the renderer has
/// registered: this long after boot, they wait for it a little.
const BOOT_NS: u64 = 60_000_000_000;
const BOOT_WAIT_NS: u64 = 5_000_000_000;
/// How long a call to the renderer may take before it counts as hung.
const CALL_NS: u64 = 10_000_000_000;

/// A virgl context through the `gpu` service.
pub struct GpuTransport {
    client: gpu::Client,
    caps: Vec<u8>,
    map: Arc<Mapping>,
    command: Range<usize>,
    shared: Range<usize>,
    fence_word: usize,
    fences: Arc<Event>,
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
        GpuTransport::connect_to(gpu::NAME, if registered { CALL_NS } else { BOOT_WAIT_NS })
    }

    /// Opens a context through the service `name`, giving it `wait_ns` to
    /// answer (connections to a service not registered yet wait for it).
    pub fn connect_to(name: &str, wait_ns: u64) -> Option<GpuTransport> {
        let client = gpu::Client::new(vproto::connect(name).ok()?);
        client.set_timeout(wait_ns);
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
            map: Arc::new(map),
            command: co..co + session.command_size as usize,
            shared: so..so + session.shared_size as usize,
            fence_word: session.fence_offset as usize,
            fences: Arc::new(session.fences),
            renderer: session.renderer,
        })
    }

    /// The host's renderer.
    pub fn renderer(&self) -> &str {
        &self.renderer
    }

    /// How long a call may take before the context counts as lost (10 s
    /// unless set).
    pub fn set_call_timeout(&self, ns: u64) {
        self.client.set_timeout(ns);
    }

    /// A watch over this context's fences, for a program that waits for
    /// them among other things (it shares the context's event and memory).
    pub fn watch(&self) -> FenceWatch {
        FenceWatch { event: self.fences.clone(), map: self.map.clone(), word: self.fence_word }
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

    fn import_resource(&mut self, args: &ResourceArgs, memory: &External) -> Result<u32, OutOfMemory> {
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
        // The caller keeps its handle: the renderer gets one of its own.
        let theirs = {
            // SAFETY: borrowed for the duplication, never closed here.
            let h = core::mem::ManuallyDrop::new(unsafe { vrt::object::Handle::from_raw(memory.handle) });
            Vmo::from_handle(h.duplicate(None).map_err(|_| OutOfMemory)?)
        };
        match self.client.import(spec, memory.stride, theirs) {
            Ok(Ok(h)) => Ok(h),
            Ok(Err(e)) => {
                vrt::println!("{}: cannot render into memory it is given: {}", self.renderer, e);
                Err(OutOfMemory)
            }
            Err(_) => Err(OutOfMemory),
        }
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
            Ok(Err(e)) => lost(&self.renderer, format_args!("refused commands ({e})")),
            Err(e) => lost(&self.renderer, format_args!("did not answer ({e:?})")),
        }
    }

    fn fence(&mut self) -> Result<u64, Lost> {
        match self.client.fence() {
            Ok(Ok(f)) => Ok(f),
            Ok(Err(e)) => lost(&self.renderer, format_args!("refused a fence ({e})")),
            Err(e) => lost(&self.renderer, format_args!("did not answer ({e:?})")),
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
                return lost(&self.renderer, format_args!("left fence {fence} unsignaled"));
            }
        }
    }
}

/// A context's fences, as a program that waits for several things at once
/// watches them: the event the GPU's driver signals as fences signal (for
/// its wait set), and the number of the last that did.
pub struct FenceWatch {
    event: Arc<Event>,
    map: Arc<Mapping>,
    word: usize,
}

impl FenceWatch {
    /// The event: signaled whenever a fence signals.
    pub fn event(&self) -> vabi::RawHandle {
        self.event.raw()
    }

    /// Clears the event; look at [`signaled`](Self::signaled) after.
    pub fn clear(&self) {
        let _ = self.event.clear();
    }

    /// The last fence that signaled.
    pub fn signaled(&self) -> u64 {
        // SAFETY: the fence word, inside the mapping, aligned; the driver
        // writes it at any time.
        unsafe { core::ptr::read_volatile(self.map.as_ptr().add(self.word) as *const u64) }
    }
}

/// Says why the GPU's context is lost (the renderer draws nothing from
/// then on).
fn lost<T>(renderer: &str, why: core::fmt::Arguments) -> Result<T, Lost> {
    vrt::println!("the GPU service ({renderer}) {why}: rendering stops");
    Err(Lost)
}
