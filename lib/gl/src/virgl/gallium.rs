//! Veda's renderer on the host, for tests (`VGL_TEST_BACKEND=gallium`):
//! the decoder that carries out virgl command streams on a Gallium driver
//! (`services/renderer`), built with Mesa as `vgallium.dll` (on Linux
//! `vgallium.so`) around softpipe, Mesa's reference rasterizer. In Veda
//! the same decoder runs the PC's GPU for the `gpu` service's clients;
//! here the tests call it directly, a device and context of its own for
//! each of theirs.
//!
//! `VGL_GALLIUM_DLL` names the library; by default it is where Mesa's build
//! for this machine leaves it (`cargo xtask toolchain` configures that
//! build, `cargo xtask test` brings the library up to date).

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

/// The library's functions (`services/renderer/src/renderer.h`).
struct Lib {
    device_create: unsafe extern "C" fn() -> Device,
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
        #[cfg(windows)]
        let built = concat!(
            env!("CARGO_MANIFEST_DIR"),
            r"\..\..\target\toolchain\build\mesa-host\src\gallium\targets\veda\vgallium.dll"
        );
        #[cfg(not(windows))]
        let built = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/toolchain/build/mesa-host/src/gallium/targets/veda/vgallium.so"
        );
        String::from(built)
    })
}

/// The library, loaded once; `None` if it is not there.
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
            Some(Lib {
                device_create: sym(m, "vr_device_create_softpipe"),
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

/// A context of Veda's renderer, on a softpipe device of its own.
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
            let dev = (lib.device_create)();
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
