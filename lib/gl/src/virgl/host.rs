//! The virgl renderer on the host's GPU, for tests (with the `host-virgl`
//! feature): virglrenderer, the library QEMU runs for a virtio-gpu device,
//! called directly, on OpenGL contexts made as QEMU makes them.
//!
//! QEMU's Windows build ships it with ANGLE (OpenGL ES on Direct3D 11);
//! the tests load both from QEMU's directory (`VEDA_QEMU_DIR`, by default
//! `C:\Program Files\qemu`). Headless QEMU renders on ANGLE, and so do the
//! tests; QEMU's window renders on the host's desktop OpenGL instead
//! (WGL), as the tests do with `VGL_TEST_HOST=desktop`.
//!
//! On Linux QEMU runs the system's virglrenderer on desktop OpenGL,
//! through EGL; headless, on the GPU of the first render node, through
//! GBM. So do the tests (`VGL_TEST_RENDERNODE` names another node, such as
//! `/dev/dri/renderD129`), and on OpenGL ES with `VGL_TEST_HOST=gles`, as
//! QEMU renders with `gl=es`.
//!
//! virglrenderer and its contexts belong to one thread, so a thread of
//! their own runs every call, and the transport sends it jobs.

use std::boxed::Box;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
#[cfg(windows)]
use std::format;
use std::ops::Range;
use std::ptr::null_mut;
use std::string::String;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::vec::Vec;
use std::{println, vec};

use crate::backend::OutOfMemory;
use crate::virgl::{Lost, ResourceArgs, Transport};

/// A library loaded into the process.
pub(super) type Module = *mut c_void;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> Module;
    fn GetProcAddress(m: Module, name: *const c_char) -> *mut c_void;
    fn SetDllDirectoryW(path: *const u16) -> i32;
    fn GetModuleHandleW(name: *const u16) -> Module;
}

// The C library's dynamic linking.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> Module;
    fn dlsym(m: Module, name: *const c_char) -> *mut c_void;
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Loads a library: a path, or on Linux a name the dynamic linker looks up
/// (`libEGL.so.1`). Null if it is not there.
pub(super) unsafe fn load(name: &str) -> Module {
    #[cfg(windows)]
    // SAFETY: a NUL-terminated name; the caller trusts the library.
    unsafe {
        LoadLibraryW(wide(name).as_ptr())
    }
    #[cfg(target_os = "linux")]
    {
        let c = CString::new(name).unwrap();
        // SAFETY: as above. RTLD_NOW binds every function the library
        // calls at once: one missing fails here, not at its first call.
        unsafe { dlopen(c.as_ptr(), 2) }
    }
}

/// A function of a loaded library.
pub(super) unsafe fn sym<T: Copy>(m: Module, name: &str) -> T {
    let c = CString::new(name).unwrap();
    // SAFETY: the caller gives the symbol's real type.
    unsafe {
        #[cfg(windows)]
        let p = GetProcAddress(m, c.as_ptr());
        #[cfg(target_os = "linux")]
        let p = dlsym(m, c.as_ptr());
        assert!(!p.is_null(), "{name} is missing");
        core::mem::transmute_copy(&p)
    }
}

type Dpy = *mut c_void;

#[repr(C)]
struct GlCtxParam {
    version: c_int,
    shared: bool,
    major_ver: c_int,
    minor_ver: c_int,
    compat_ctx: c_int,
}

#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: unsafe extern "C" fn(*mut c_void, u32),
    create_gl_context: unsafe extern "C" fn(*mut c_void, c_int, *mut GlCtxParam) -> *mut c_void,
    destroy_gl_context: unsafe extern "C" fn(*mut c_void, *mut c_void),
    make_current: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int,
    get_drm_fd: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    write_context_fence: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u64)>,
    get_server_fd: Option<unsafe extern "C" fn(*mut c_void, u32) -> c_int>,
    get_egl_display: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
}

#[repr(C)]
struct CreateArgs {
    handle: u32,
    a: ResourceArgs,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Iovec {
    base: *mut c_void,
    len: usize,
}

/// EGL, on ANGLE or on Linux, as the callbacks use it (from the GPU thread
/// only).
struct Egl {
    dpy: Dpy,
    config: *mut c_void,
    /// Whether its contexts are OpenGL ES (ANGLE's are), not desktop
    /// OpenGL.
    es: bool,
    create_context: unsafe extern "C" fn(Dpy, *mut c_void, *mut c_void, *const i32) -> *mut c_void,
    destroy_context: unsafe extern "C" fn(Dpy, *mut c_void) -> u32,
    make_current: unsafe extern "C" fn(Dpy, *mut c_void, *mut c_void, *mut c_void) -> u32,
    get_current_context: unsafe extern "C" fn() -> *mut c_void,
}

/// WGL: the host's desktop OpenGL, with the device context of a hidden
/// window (from the GPU thread only).
#[cfg(windows)]
struct Wgl {
    dc: *mut c_void,
    create_context: unsafe extern "system" fn(*mut c_void, *mut c_void, *const i32) -> *mut c_void,
    delete_context: unsafe extern "system" fn(*mut c_void) -> i32,
    make_current: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    get_current_context: unsafe extern "system" fn() -> *mut c_void,
}

/// Where virglrenderer's OpenGL contexts come from.
enum Platform {
    Egl(Egl),
    #[cfg(windows)]
    Wgl(Wgl),
}

// The GPU thread's: set once before virglrenderer starts.
static mut PLATFORM: Option<Platform> = None;
static LAST_FENCE: AtomicU32 = AtomicU32::new(0);

fn platform() -> &'static Platform {
    // SAFETY: set before any callback runs, never changed afterwards.
    unsafe { (*core::ptr::addr_of!(PLATFORM)).as_ref().expect("an OpenGL platform") }
}

unsafe extern "C" fn write_fence(_: *mut c_void, fence: u32) {
    LAST_FENCE.fetch_max(fence, Ordering::SeqCst);
}

unsafe extern "C" fn create_gl_context(_: *mut c_void, _: c_int, p: *mut GlCtxParam) -> *mut c_void {
    // SAFETY: virglrenderer passes a valid parameter block.
    let p = unsafe { &*p };
    // The profile of desktop OpenGL: compatibility or core.
    let profile = if p.compat_ctx != 0 { 2 } else { 1 };
    // SAFETY: calls with the display, configuration or device context set
    // up at start.
    unsafe {
        match platform() {
            Platform::Egl(e) => {
                // EGL_CONTEXT_{MAJOR,MINOR}_VERSION, and for desktop OpenGL
                // the profile (EGL_CONTEXT_OPENGL_PROFILE_MASK).
                let mut att = [0x3098, p.major_ver, 0x30FB, p.minor_ver, 0x3038, 0x3038, 0x3038];
                if !e.es {
                    att[4..6].copy_from_slice(&[0x30FD, profile]);
                }
                let share = if p.shared { (e.get_current_context)() } else { null_mut() };
                (e.create_context)(e.dpy, e.config, share, att.as_ptr())
            }
            #[cfg(windows)]
            Platform::Wgl(w) => {
                // WGL_CONTEXT_{MAJOR,MINOR}_VERSION_ARB, and the profile.
                let att = [0x2091, p.major_ver, 0x2092, p.minor_ver, 0x9126, profile, 0];
                let share = if p.shared { (w.get_current_context)() } else { null_mut() };
                (w.create_context)(w.dc, share, att.as_ptr())
            }
        }
    }
}

unsafe extern "C" fn destroy_gl_context(_: *mut c_void, ctx: *mut c_void) {
    // SAFETY: a context made by `create_gl_context`.
    unsafe {
        match platform() {
            Platform::Egl(e) => {
                (e.destroy_context)(e.dpy, ctx);
            }
            #[cfg(windows)]
            Platform::Wgl(w) => {
                (w.delete_context)(ctx);
            }
        }
    }
}

unsafe extern "C" fn make_current(_: *mut c_void, _: c_int, ctx: *mut c_void) -> c_int {
    // SAFETY: surfaceless with EGL, as ANGLE and Mesa allow; on the hidden
    // window's device context with WGL.
    let ok = unsafe {
        match platform() {
            Platform::Egl(e) => (e.make_current)(e.dpy, null_mut(), null_mut(), ctx) != 0,
            #[cfg(windows)]
            Platform::Wgl(w) => (w.make_current)(w.dc, ctx) != 0,
        }
    };
    if ok { 0 } else { -1 }
}

unsafe extern "C" fn get_egl_display(_: *mut c_void) -> *mut c_void {
    match platform() {
        Platform::Egl(e) => e.dpy,
        #[cfg(windows)]
        Platform::Wgl(_) => null_mut(),
    }
}

unsafe extern "C" fn log(level: c_int, msg: *const c_char, _: *mut c_void) {
    // SAFETY: a NUL-terminated message.
    let m = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    // Debug and information messages are noise.
    if level >= 2 {
        std::eprint!("virglrenderer: {m}");
        LOG.lock().unwrap().push_str(&m);
    }
}

static LOG: Mutex<String> = Mutex::new(String::new());

/// The renderer's functions.
struct Virgl {
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    context_create: unsafe extern "C" fn(u32, u32, *const c_char) -> c_int,
    context_destroy: unsafe extern "C" fn(u32),
    resource_create: unsafe extern "C" fn(*mut CreateArgs, *mut Iovec, u32) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    attach_iov: unsafe extern "C" fn(c_int, *mut Iovec, c_int) -> c_int,
    detach_iov: unsafe extern "C" fn(c_int, *mut *mut Iovec, *mut c_int),
    ctx_attach: unsafe extern "C" fn(c_int, c_int),
    ctx_detach: unsafe extern "C" fn(c_int, c_int),
    submit: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
    create_fence: unsafe extern "C" fn(c_int, u32) -> c_int,
    poll: unsafe extern "C" fn(),
    force_ctx_0: unsafe extern "C" fn(),
}

/// What the GPU thread owns.
struct Gpu {
    v: Virgl,
    caps: Vec<u8>,
    next_ctx: u32,
    next_res: u32,
    next_fence: u32,
    /// The iovec arrays of resources with guest storage (virglrenderer
    /// keeps pointers to them).
    iovs: BTreeMap<u32, Box<[Iovec; 1]>>,
}

type Job = Box<dyn FnOnce(&mut Gpu) + Send>;

static GPU: OnceLock<Option<Mutex<Sender<Job>>>> = OnceLock::new();

/// The directory QEMU (and its renderer) is installed in.
#[cfg(windows)]
fn qemu_dir() -> String {
    std::env::var("VEDA_QEMU_DIR").unwrap_or_else(|_| String::from(r"C:\Program Files\qemu"))
}

/// Starts the GPU thread, once; `None` if the renderer cannot run here.
fn gpu() -> Option<&'static Mutex<Sender<Job>>> {
    GPU.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        let (ready_tx, ready_rx) = channel::<bool>();
        std::thread::Builder::new()
            .name(String::from("virgl"))
            .spawn(move || {
                // SAFETY: loading the libraries and calling them as their
                // headers declare.
                let gpu = unsafe { start() };
                let Some(mut gpu) = gpu else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                while let Ok(job) = rx.recv() {
                    // As QEMU does before each command: virglrenderer goes
                    // back to its own context, and to a client's only for
                    // the client's commands. It loses track otherwise:
                    // creating a context leaves the new one's OpenGL
                    // context current, while it takes the one before for
                    // current still.
                    // SAFETY: on virglrenderer's thread.
                    unsafe { (gpu.v.force_ctx_0)() };
                    job(&mut gpu);
                }
            })
            .ok()?;
        ready_rx.recv().ok().filter(|&ok| ok).map(|_| Mutex::new(tx))
    })
    .as_ref()
}

/// Runs `f` on the GPU thread and waits for its result.
fn call<R: Send + 'static>(f: impl FnOnce(&mut Gpu) -> R + Send + 'static) -> R {
    let (tx, rx) = channel();
    let job: Job = Box::new(move |g| {
        let _ = tx.send(f(g));
    });
    gpu().expect("virglrenderer").lock().unwrap().send(job).expect("the GPU thread");
    rx.recv().expect("the GPU thread")
}

/// Sets up EGL on the display `dpy` of the library `lib`, with a current
/// context, as QEMU does: for OpenGL ES if `es`, otherwise for desktop
/// OpenGL.
unsafe fn start_egl_on(lib: Module, dpy: Dpy, es: bool) -> Option<Egl> {
    unsafe {
        let initialize: unsafe extern "C" fn(Dpy, *mut i32, *mut i32) -> u32 = sym(lib, "eglInitialize");
        let bind_api: unsafe extern "C" fn(u32) -> u32 = sym(lib, "eglBindAPI");
        let choose: unsafe extern "C" fn(Dpy, *const i32, *mut *mut c_void, i32, *mut i32) -> u32 =
            sym(lib, "eglChooseConfig");
        let (mut major, mut minor) = (0, 0);
        if dpy.is_null() || initialize(dpy, &mut major, &mut minor) == 0 {
            return None;
        }
        // EGL_OPENGL_ES_API or EGL_OPENGL_API, and a window-capable config
        // of it (EGL_OPENGL_ES2_BIT or EGL_OPENGL_BIT), as QEMU chooses.
        bind_api(if es { 0x30A0 } else { 0x30A2 });
        let renderable = if es { 0x4 } else { 0x8 };
        let attribs = [0x3033, 0x4, 0x3040, renderable, 0x3024, 5, 0x3023, 5, 0x3022, 5, 0x3021, 0, 0x3038];
        let mut config = null_mut();
        let mut n = 0;
        if choose(dpy, attribs.as_ptr(), &mut config, 1, &mut n) == 0 || n != 1 {
            return None;
        }
        let e = Egl {
            dpy,
            config,
            es,
            create_context: sym(lib, "eglCreateContext"),
            destroy_context: sym(lib, "eglDestroyContext"),
            make_current: sym(lib, "eglMakeCurrent"),
            get_current_context: sym(lib, "eglGetCurrentContext"),
        };
        // virglrenderer shares its contexts with the current one: OpenGL
        // ES 2, or desktop OpenGL in the core profile.
        let att = if es { [0x3098, 2, 0x3038] } else { [0x30FD, 1, 0x3038] };
        let ctx0 = (e.create_context)(dpy, config, null_mut(), att.as_ptr());
        if ctx0.is_null() || (e.make_current)(dpy, null_mut(), null_mut(), ctx0) == 0 {
            return None;
        }
        Some(e)
    }
}

/// Sets up EGL on ANGLE, with a current context.
#[cfg(windows)]
unsafe fn start_egl(dir: &str) -> Option<Egl> {
    unsafe {
        let egl_lib = load(&format!(r"{dir}\libEGL.dll"));
        if egl_lib.is_null() {
            return None;
        }
        let get_display: unsafe extern "C" fn(u32, *mut c_void, *const i32) -> Dpy =
            sym(egl_lib, "eglGetPlatformDisplayEXT");
        // EGL_PLATFORM_ANGLE_ANGLE, the default (Direct3D 11) device.
        let dpy = get_display(0x3202, null_mut(), core::ptr::null());
        start_egl_on(egl_lib, dpy, true)
    }
}

/// Opens the render node of the GPU to render on: `VGL_TEST_RENDERNODE`,
/// or the first that opens, as QEMU's headless display chooses.
#[cfg(target_os = "linux")]
fn render_node() -> Option<std::fs::File> {
    let open = |path: &std::path::Path| std::fs::File::options().read(true).write(true).open(path);
    if let Some(path) = std::env::var_os("VGL_TEST_RENDERNODE") {
        return open(path.as_ref()).map_err(|e| println!("{}: {e}", path.display())).ok();
    }
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b"renderD")))
        .collect();
    nodes.sort();
    let node = nodes.iter().find_map(|p| open(p).ok());
    if node.is_none() {
        println!("no GPU: no render node in /dev/dri can be opened");
    }
    node
}

/// Sets up EGL on a GPU, with a current context, as QEMU's headless
/// display does: on its render node, through GBM.
#[cfg(target_os = "linux")]
unsafe fn start_egl(es: bool) -> Option<Egl> {
    use std::os::fd::IntoRawFd;
    let node = render_node()?;
    unsafe {
        let gbm_lib = load("libgbm.so.1");
        let egl_lib = load("libEGL.so.1");
        if gbm_lib.is_null() || egl_lib.is_null() {
            println!("EGL (libEGL.so.1) or GBM (libgbm.so.1) is not installed");
            return None;
        }
        let create_device: unsafe extern "C" fn(c_int) -> *mut c_void = sym(gbm_lib, "gbm_create_device");
        let get_display: unsafe extern "C" fn(u32, *mut c_void, *const isize) -> Dpy =
            sym(egl_lib, "eglGetPlatformDisplay");
        // The device keeps the node, for as long as the process runs.
        let gbm = create_device(node.into_raw_fd());
        if gbm.is_null() {
            return None;
        }
        // EGL_PLATFORM_GBM_KHR.
        start_egl_on(egl_lib, get_display(0x31D7, gbm, core::ptr::null()), es)
    }
}

/// `WNDCLASSW`.
#[cfg(windows)]
#[repr(C)]
struct WndClass {
    style: u32,
    proc: unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize,
    cls_extra: i32,
    wnd_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
}

/// `PIXELFORMATDESCRIPTOR`, the fields the tests set.
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct PixelFormat {
    size: u16,
    version: u16,
    flags: u32,
    pixel_type: u8,
    color_bits: u8,
    rgba_bits_shifts: [u8; 8],
    accum: [u8; 5],
    depth_bits: u8,
    stencil_bits: u8,
    aux_buffers: u8,
    layer_type: u8,
    reserved: u8,
    masks: [u32; 3],
}

/// Sets up WGL on a hidden window, with a current context, as QEMU's
/// window (GTK) has.
#[cfg(windows)]
unsafe fn start_wgl() -> Option<Wgl> {
    unsafe {
        let user = load("user32.dll");
        let gdi = load("gdi32.dll");
        let gl = load("opengl32.dll");
        if user.is_null() || gdi.is_null() || gl.is_null() {
            return None;
        }
        let register: unsafe extern "system" fn(*const WndClass) -> u16 = sym(user, "RegisterClassW");
        #[allow(clippy::type_complexity)]
        let create_window: unsafe extern "system" fn(
            u32,
            *const u16,
            *const u16,
            u32,
            i32,
            i32,
            i32,
            i32,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> *mut c_void = sym(user, "CreateWindowExW");
        let get_dc: unsafe extern "system" fn(*mut c_void) -> *mut c_void = sym(user, "GetDC");
        let choose: unsafe extern "system" fn(*mut c_void, *const PixelFormat) -> i32 = sym(gdi, "ChoosePixelFormat");
        let set: unsafe extern "system" fn(*mut c_void, i32, *const PixelFormat) -> i32 = sym(gdi, "SetPixelFormat");
        let create_legacy: unsafe extern "system" fn(*mut c_void) -> *mut c_void = sym(gl, "wglCreateContext");
        let get_proc: unsafe extern "system" fn(*const c_char) -> *mut c_void = sym(gl, "wglGetProcAddress");
        let make_current: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32 = sym(gl, "wglMakeCurrent");
        let name = wide("vgl-test");
        let instance = GetModuleHandleW(core::ptr::null());
        let class = WndClass {
            // CS_OWNDC: the window keeps its device context.
            style: 0x20,
            proc: sym(user, "DefWindowProcW"),
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: null_mut(),
            cursor: null_mut(),
            background: null_mut(),
            menu_name: core::ptr::null(),
            class_name: name.as_ptr(),
        };
        if register(&class) == 0 {
            return None;
        }
        // WS_POPUP, never shown.
        let window = create_window(
            0,
            name.as_ptr(),
            name.as_ptr(),
            0x8000_0000,
            0,
            0,
            16,
            16,
            null_mut(),
            null_mut(),
            instance,
            null_mut(),
        );
        if window.is_null() {
            return None;
        }
        let dc = get_dc(window);
        // PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER, RGBA.
        let pfd = PixelFormat {
            size: core::mem::size_of::<PixelFormat>() as u16,
            version: 1,
            flags: 0x25,
            color_bits: 32,
            depth_bits: 24,
            stencil_bits: 8,
            ..Default::default()
        };
        let format = choose(dc, &pfd);
        if format == 0 || set(dc, format, &pfd) == 0 {
            return None;
        }
        // wglCreateContextAttribsARB is found through a current context;
        // virglrenderer then shares its contexts with that one.
        let ctx0 = create_legacy(dc);
        if ctx0.is_null() || make_current(dc, ctx0) == 0 {
            return None;
        }
        let create = get_proc(c"wglCreateContextAttribsARB".as_ptr());
        if create.is_null() {
            return None;
        }
        Some(Wgl {
            dc,
            create_context: core::mem::transmute::<
                *mut c_void,
                unsafe extern "system" fn(*mut c_void, *mut c_void, *const i32) -> *mut c_void,
            >(create),
            delete_context: sym(gl, "wglDeleteContext"),
            make_current,
            get_current_context: sym(gl, "wglGetCurrentContext"),
        })
    }
}

/// Whether the tests run on the host's desktop OpenGL rather than ANGLE.
#[cfg(windows)]
fn desktop_gl() -> bool {
    std::env::var("VGL_TEST_HOST").is_ok_and(|v| v == "desktop")
}

/// Loads virglrenderer and sets up the OpenGL it renders on, with the
/// flags QEMU starts it with there.
#[cfg(windows)]
unsafe fn open_renderer() -> Option<(Module, Platform, c_int)> {
    let dir = qemu_dir();
    unsafe {
        SetDllDirectoryW(wide(&dir).as_ptr());
        let vr = load(&format!(r"{dir}\libvirglrenderer-1.dll"));
        if vr.is_null() {
            println!("virglrenderer not found in {dir}");
            return None;
        }
        if desktop_gl() {
            Some((vr, Platform::Wgl(start_wgl()?), 0))
        } else {
            // VIRGL_RENDERER_D3D11_SHARE_TEXTURE, as QEMU passes with ANGLE.
            Some((vr, Platform::Egl(start_egl(&dir)?), 1 << 12))
        }
    }
}

/// Loads virglrenderer and sets up the OpenGL it renders on, with the
/// flags QEMU starts it with there.
#[cfg(target_os = "linux")]
unsafe fn open_renderer() -> Option<(Module, Platform, c_int)> {
    unsafe {
        let vr = load("libvirglrenderer.so.1");
        if vr.is_null() {
            println!("virglrenderer (libvirglrenderer.so.1) is not installed");
            return None;
        }
        let es = std::env::var("VGL_TEST_HOST").is_ok_and(|v| v == "gles");
        Some((vr, Platform::Egl(start_egl(es)?), 0))
    }
}

unsafe fn start() -> Option<Gpu> {
    unsafe {
        let (vr, p, flags) = open_renderer()?;
        let egl = matches!(p, Platform::Egl(_));
        PLATFORM = Some(p);
        let set_log: unsafe extern "C" fn(
            unsafe extern "C" fn(c_int, *const c_char, *mut c_void),
            *mut c_void,
            *mut c_void,
        ) = sym(vr, "virgl_set_log_callback");
        set_log(log, null_mut(), null_mut());
        let init: unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int = sym(vr, "virgl_renderer_init");
        let cbs = Box::leak(Box::new(Callbacks {
            version: 4,
            write_fence,
            create_gl_context,
            destroy_gl_context,
            make_current,
            get_drm_fd: None,
            write_context_fence: None,
            get_server_fd: None,
            get_egl_display: egl.then_some(get_egl_display as unsafe extern "C" fn(*mut c_void) -> *mut c_void),
        }));
        static mut COOKIE: u32 = 0;
        if init(core::ptr::addr_of_mut!(COOKIE) as *mut c_void, flags, cbs) != 0 {
            return None;
        }
        let v = Virgl {
            get_cap_set: sym(vr, "virgl_renderer_get_cap_set"),
            fill_caps: sym(vr, "virgl_renderer_fill_caps"),
            context_create: sym(vr, "virgl_renderer_context_create"),
            context_destroy: sym(vr, "virgl_renderer_context_destroy"),
            resource_create: sym(vr, "virgl_renderer_resource_create"),
            resource_unref: sym(vr, "virgl_renderer_resource_unref"),
            attach_iov: sym(vr, "virgl_renderer_resource_attach_iov"),
            detach_iov: sym(vr, "virgl_renderer_resource_detach_iov"),
            ctx_attach: sym(vr, "virgl_renderer_ctx_attach_resource"),
            ctx_detach: sym(vr, "virgl_renderer_ctx_detach_resource"),
            submit: sym(vr, "virgl_renderer_submit_cmd"),
            create_fence: sym(vr, "virgl_renderer_create_fence"),
            poll: sym(vr, "virgl_renderer_poll"),
            force_ctx_0: sym(vr, "virgl_renderer_force_ctx_0"),
        };
        let (mut ver, mut size) = (0, 0);
        (v.get_cap_set)(2, &mut ver, &mut size);
        if ver < 2 || size == 0 {
            return None;
        }
        let mut caps = vec![0u8; size as usize];
        (v.fill_caps)(2, ver, caps.as_mut_ptr() as *mut c_void);
        Some(Gpu { v, caps, next_ctx: 1, next_res: 1, next_fence: 0, iovs: BTreeMap::new() })
    }
}

/// A pointer the GPU thread may use.
#[derive(Clone, Copy)]
struct SendPtr(*mut u8);
// SAFETY: the transport keeps the memory alive while the GPU thread uses
// it, and waits for every job.
unsafe impl Send for SendPtr {}

/// A virgl context on the host renderer.
pub struct HostTransport {
    ctx: u32,
    caps: Vec<u8>,
    shared: Box<[u128]>,
    resources: Vec<u32>,
}

/// Shared memory: 16 MiB of staging and the query area.
const SHARED_BYTES: usize = 16 * 1024 * 1024 + 16 * 1024;

impl HostTransport {
    pub fn new() -> Option<HostTransport> {
        gpu()?;
        let (ctx, caps) = call(|g| {
            let ctx = g.next_ctx;
            g.next_ctx += 1;
            let name = CString::new("vgl-test").unwrap();
            // SAFETY: a new context id.
            let r = unsafe { (g.v.context_create)(ctx, 8, name.as_ptr()) };
            (if r == 0 { ctx } else { 0 }, g.caps.clone())
        });
        if ctx == 0 {
            return None;
        }
        Some(HostTransport {
            ctx,
            caps,
            shared: vec![0u128; SHARED_BYTES / 16].into_boxed_slice(),
            resources: Vec::new(),
        })
    }

    fn base(&self) -> SendPtr {
        SendPtr(self.shared.as_ptr() as *mut u8)
    }
}

impl Transport for HostTransport {
    fn caps(&self) -> &[u8] {
        &self.caps
    }

    fn shared(&self) -> (*mut u8, usize) {
        (self.shared.as_ptr() as *mut u8, SHARED_BYTES)
    }

    fn create_resource(&mut self, args: &ResourceArgs, backing: Option<Range<usize>>) -> Result<u32, OutOfMemory> {
        let a = *args;
        let ctx = self.ctx;
        let base = self.base();
        if let Some(r) = &backing {
            assert!(r.end <= SHARED_BYTES);
        }
        let handle = call(move |g| {
            let handle = g.next_res;
            g.next_res += 1;
            let mut ca = CreateArgs { handle, a };
            // SAFETY: valid arguments; the backing lies in the shared
            // memory, which outlives the resource.
            unsafe {
                if (g.v.resource_create)(&mut ca, null_mut(), 0) != 0 {
                    return 0;
                }
                if let Some(r) = backing {
                    let base = base;
                    let mut iov = Box::new([Iovec { base: base.0.add(r.start) as *mut c_void, len: r.len() }]);
                    (g.v.attach_iov)(handle as c_int, iov.as_mut_ptr(), 1);
                    g.iovs.insert(handle, iov);
                }
                (g.v.ctx_attach)(ctx as c_int, handle as c_int);
            }
            handle
        });
        if handle == 0 {
            return Err(OutOfMemory);
        }
        self.resources.push(handle);
        Ok(handle)
    }

    fn destroy_resource(&mut self, handle: u32) {
        self.resources.retain(|&r| r != handle);
        let ctx = self.ctx;
        call(move |g| unref(g, ctx, handle));
    }

    fn max_submit_words(&self) -> usize {
        256 * 1024
    }

    fn submit(&mut self, commands: &[u32]) -> Result<(), Lost> {
        let mut cmds = commands.to_vec();
        let ctx = self.ctx;
        let r = call(move |g| {
            // SAFETY: a word-aligned command buffer of the given length.
            unsafe { (g.v.submit)(cmds.as_mut_ptr() as *mut c_void, ctx as c_int, cmds.len() as c_int) }
        });
        if r != 0 {
            let log = LOG.lock().unwrap().clone();
            panic!("virglrenderer rejected the command stream ({r}):\n{log}");
        }
        Ok(())
    }

    fn fence(&mut self) -> Result<u64, Lost> {
        let ctx = self.ctx;
        Ok(call(move |g| {
            g.next_fence += 1;
            let f = g.next_fence;
            // An empty fenced submission, as Veda's driver makes and QEMU
            // carries out: the fence follows the context's commands in its
            // own OpenGL context, which the submission switches to.
            let mut none = [0u32];
            // SAFETY: a fence after everything submitted so far.
            unsafe {
                (g.v.submit)(none.as_mut_ptr() as *mut c_void, ctx as c_int, 0);
                (g.v.create_fence)(f as c_int, ctx);
            }
            u64::from(f)
        }))
    }

    fn signaled(&mut self, fence: u64) -> bool {
        let done = || u64::from(LAST_FENCE.load(Ordering::SeqCst)) >= fence;
        if done() {
            return true;
        }
        call(|g| {
            // SAFETY: polling retires fences.
            unsafe { (g.v.poll)() }
        });
        done()
    }

    fn wait(&mut self, fence: u64) -> Result<(), Lost> {
        while !self.signaled(fence) {
            std::thread::yield_now();
        }
        Ok(())
    }
}

fn unref(g: &mut Gpu, ctx: u32, handle: u32) {
    // SAFETY: a resource of this context.
    unsafe {
        (g.v.ctx_detach)(ctx as c_int, handle as c_int);
        if let Some(array) = g.iovs.remove(&handle) {
            let mut iov = null_mut();
            let mut n = 0;
            (g.v.detach_iov)(handle as c_int, &mut iov, &mut n);
            // Detaching reads the array (resources kept in host memory
            // copy their contents out of the backing): freed only now.
            drop(array);
        }
        (g.v.resource_unref)(handle);
    }
}

impl Drop for HostTransport {
    fn drop(&mut self) {
        let ctx = self.ctx;
        let resources = core::mem::take(&mut self.resources);
        call(move |g| {
            for r in resources {
                unref(g, ctx, r);
            }
            // SAFETY: the context made in `new`.
            unsafe { (g.v.context_destroy)(ctx) };
        });
    }
}

/// A context rendering on the host's GPU, if virglrenderer is installed.
pub fn context(config: crate::Config) -> Option<crate::Context> {
    let t = HostTransport::new()?;
    let b = super::VirglBackend::new(Box::new(t)).ok()?;
    Some(crate::Context::new(Box::new(b), config))
}
