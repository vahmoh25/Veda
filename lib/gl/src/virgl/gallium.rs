//! Veda's renderer on the host, for tests (`VGL_TEST_BACKEND=gallium`):
//! the decoder that carries out virgl command streams on a Gallium driver
//! (`guest/renderer`), built with Mesa as `vgallium.so` around softpipe,
//! Mesa's reference rasterizer. In the driver VM the same decoder runs the
//! GPU for the `gpu` service's clients; here the tests call it directly, a
//! device and context of its own for each of theirs.
//!
//! `VGL_GALLIUM_DLL` names the library; by default it is where Mesa's build
//! for this machine leaves it (`cargo xtask linux` configures that build,
//! `cargo xtask test` brings the library up to date). With
//! `VGL_GALLIUM_DEVICE=virgl` the decoder renders on virgl, over
//! virglrenderer's test server (`virgl_test_server`, at `VTEST_SOCKET_NAME`),
//! as the driver VM's renderer does under QEMU, and with
//! `VGL_GALLIUM_DEVICE=iris` on this machine's Intel GPU (iris, through its
//! render node), as the renderer does on a PC's; on softpipe otherwise.

use std::boxed::Box;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::ops::Range;
use std::string::String;
use std::sync::OnceLock;
use std::vec;
use std::vec::Vec;

use super::host::{load, sym};
use crate::backend::{External, OutOfMemory};
use crate::virgl::{Lost, ResourceArgs, Transport};

type Device = *mut c_void;
type Ctx = *mut c_void;

/// How the library makes a device: on its own (softpipe, virgl over
/// vtest), or on the GPU of a render node it is given (an open one).
#[derive(Clone, Copy)]
enum Make {
    Plain(unsafe extern "C" fn() -> Device),
    Drm(unsafe extern "C" fn(c_int) -> Device, c_int),
}

/// The library's functions (`guest/renderer/decoder/renderer.h`).
struct Lib {
    make: Make,
    device_destroy: unsafe extern "C" fn(Device),
    device_caps: unsafe extern "C" fn(Device, *mut c_void, usize) -> usize,
    context_create: unsafe extern "C" fn(Device, *mut u8, usize) -> Ctx,
    context_destroy: unsafe extern "C" fn(Ctx),
    context_error: unsafe extern "C" fn(Ctx) -> *const c_char,
    resource_create: unsafe extern "C" fn(Ctx, *const ResourceArgs, u64, u64, *mut u32) -> c_int,
    resource_import: unsafe extern "C" fn(Ctx, *const ResourceArgs, c_int, *mut c_void, u32, *mut u32) -> c_int,
    resource_destroy: unsafe extern "C" fn(Ctx, u32),
    submit: unsafe extern "C" fn(Ctx, *const u32, usize) -> c_int,
    fence: unsafe extern "C" fn(Ctx, *mut u64) -> c_int,
    fence_signaled: unsafe extern "C" fn(Ctx) -> u64,
    fence_wait: unsafe extern "C" fn(Ctx, u64, u64) -> c_int,
}

// SAFETY: plain function pointers.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

fn dll_path() -> String {
    std::env::var("VGL_GALLIUM_DLL").unwrap_or_else(|_| {
        String::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/linux/build/mesa-host/src/gallium/targets/veda/vgallium.so"
        ))
    })
}

/// This machine's Intel GPU's render node, open (for as long as the tests
/// run): the node whose Linux driver is i915 or xe.
fn intel_render_node() -> Option<c_int> {
    let mut nodes: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    let node = nodes.into_iter().find(|n| {
        std::fs::read_link(std::format!("/sys/class/drm/{n}/device/driver"))
            .is_ok_and(|d| d.file_name().is_some_and(|f| f == "i915" || f == "xe"))
    })?;
    let file = std::fs::OpenOptions::new().read(true).write(true).open(std::format!("/dev/dri/{node}")).ok()?;
    Some(std::os::fd::IntoRawFd::into_raw_fd(file))
}

/// The library, loaded once; `None` if it is not there (or the device the
/// tests ask for is not).
fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: loading the library and taking its functions as
        // renderer.h declares them.
        unsafe {
            let m = load(&dll_path());
            if m.is_null() {
                return None;
            }
            let make = match std::env::var("VGL_GALLIUM_DEVICE").as_deref() {
                Ok("virgl") => Make::Plain(sym(m, "vr_device_create_vtest")),
                Ok("iris") => Make::Drm(sym(m, "vr_device_create_drm"), intel_render_node()?),
                _ => Make::Plain(sym(m, "vr_device_create_softpipe")),
            };
            Some(Lib {
                make,
                device_destroy: sym(m, "vr_device_destroy"),
                device_caps: sym(m, "vr_device_caps"),
                context_create: sym(m, "vr_context_create"),
                context_destroy: sym(m, "vr_context_destroy"),
                context_error: sym(m, "vr_context_error"),
                resource_create: sym(m, "vr_resource_create"),
                resource_import: sym(m, "vr_resource_import"),
                resource_destroy: sym(m, "vr_resource_destroy"),
                submit: sym(m, "vr_submit"),
                fence: sym(m, "vr_fence"),
                fence_signaled: sym(m, "vr_fence_signaled"),
                fence_wait: sym(m, "vr_fence_wait"),
            })
        }
    })
    .as_ref()
}

/// Shared memory: 16 MiB of staging and the query area, as virtio-gpu's.
const SHARED_BYTES: usize = 16 * 1024 * 1024 + 16 * 1024;

/// A context of Veda's renderer, on a device of its own.
pub struct GalliumTransport {
    lib: &'static Lib,
    dev: Device,
    ctx: Ctx,
    caps: Vec<u8>,
    shared: Box<[u128]>,
}

impl GalliumTransport {
    pub fn new() -> Option<GalliumTransport> {
        let lib = lib()?;
        // SAFETY: calls as renderer.h describes; the shared memory lives as
        // long as the context (it is dropped after it).
        unsafe {
            let dev = match lib.make {
                Make::Plain(make) => make(),
                Make::Drm(make, fd) => make(fd),
            };
            if dev.is_null() {
                return None;
            }
            let mut caps = vec![0u8; 4096];
            let n = (lib.device_caps)(dev, caps.as_mut_ptr() as *mut c_void, caps.len());
            let mut shared = vec![0u128; SHARED_BYTES / 16].into_boxed_slice();
            let ctx = if n > 0 {
                (lib.context_create)(dev, shared.as_mut_ptr() as *mut u8, SHARED_BYTES)
            } else {
                core::ptr::null_mut()
            };
            if ctx.is_null() {
                (lib.device_destroy)(dev);
                return None;
            }
            caps.truncate(n);
            Some(GalliumTransport { lib, dev, ctx, caps, shared })
        }
    }

    /// Why the context's last call failed.
    fn error(&self) -> String {
        // SAFETY: a NUL-terminated message the context keeps.
        unsafe { CStr::from_ptr((self.lib.context_error)(self.ctx)) }.to_string_lossy().into_owned()
    }
}

impl Transport for GalliumTransport {
    fn caps(&self) -> &[u8] {
        &self.caps
    }

    fn shared(&self) -> (*mut u8, usize) {
        (self.shared.as_ptr() as *mut u8, SHARED_BYTES)
    }

    fn create_resource(&mut self, args: &ResourceArgs, backing: Option<Range<usize>>) -> Result<u32, OutOfMemory> {
        let (at, len) = backing.map_or((0, 0), |r| (r.start as u64, r.len() as u64));
        let mut id = 0;
        // SAFETY: a context of this transport; the renderer checks the rest.
        let r = unsafe { (self.lib.resource_create)(self.ctx, args, at, len, &mut id) };
        if r != 0 {
            std::eprintln!("vgallium: {} ({args:?})", self.error());
            return Err(OutOfMemory);
        }
        Ok(id)
    }

    fn destroy_resource(&mut self, handle: u32) {
        // SAFETY: as above.
        unsafe { (self.lib.resource_destroy)(self.ctx, handle) }
    }

    fn import_resource(&mut self, args: &ResourceArgs, memory: &External) -> Result<u32, OutOfMemory> {
        if memory.address == 0 {
            return Err(OutOfMemory);
        }
        let mut id = 0;
        // SAFETY: as above; the caller keeps the memory for as long as the
        // context lives (softpipe draws into it).
        let r = unsafe {
            (self.lib.resource_import)(self.ctx, args, -1, memory.address as *mut c_void, memory.stride, &mut id)
        };
        if r != 0 {
            std::eprintln!("vgallium: {} ({args:?})", self.error());
            return Err(OutOfMemory);
        }
        Ok(id)
    }

    fn max_submit_words(&self) -> usize {
        256 * 1024
    }

    fn submit(&mut self, commands: &[u32]) -> Result<(), Lost> {
        // SAFETY: as above.
        let r = unsafe { (self.lib.submit)(self.ctx, commands.as_ptr(), commands.len()) };
        if r != 0 {
            panic!("the renderer rejected the command stream ({r}): {}", self.error());
        }
        Ok(())
    }

    fn fence(&mut self) -> Result<u64, Lost> {
        let mut seq = 0;
        // SAFETY: as above.
        match unsafe { (self.lib.fence)(self.ctx, &mut seq) } {
            0 => Ok(seq),
            _ => Err(Lost),
        }
    }

    fn signaled(&mut self, fence: u64) -> bool {
        // SAFETY: as above.
        unsafe { (self.lib.fence_signaled)(self.ctx) >= fence }
    }

    fn wait(&mut self, fence: u64) -> Result<(), Lost> {
        // SAFETY: as above.
        match unsafe { (self.lib.fence_wait)(self.ctx, fence, u64::MAX) } {
            0 => Ok(()),
            _ => Err(Lost),
        }
    }
}

impl Drop for GalliumTransport {
    fn drop(&mut self) {
        // SAFETY: the context and device made in `new`, the context first.
        unsafe {
            (self.lib.context_destroy)(self.ctx);
            (self.lib.device_destroy)(self.dev);
        }
    }
}

/// A context rendering through Veda's renderer on softpipe, if the library
/// has been built.
pub fn context(config: crate::Config) -> Option<crate::Context> {
    let t = GalliumTransport::new()?;
    let b = super::VirglBackend::new(Box::new(t)).ok()?;
    Some(crate::Context::new(Box::new(b), config))
}
