//! Composition on the GPU.
//!
//! Where the display flips (its driver gave the compositor pictures, see
//! `screen`) and the GPU can draw into those pictures (the `gpu` service's
//! renderer on the PC's own GPU), every frame is drawn by the GPU with
//! OpenGL ES (`vgl`), straight into the picture the display shows next, as
//! modern window systems compose:
//!
//! * every window is a texture, brought up to date where its client drew
//!   (the processor copies what changed, nothing more);
//! * shadows, borders, rounded corners and the desktop's gradient are
//!   shaders, as is the startup sequence: its light, ring and gradient;
//! * what is text or a drawing (title bars, cursors, the switcher, the
//!   startup sequence's words) is drawn by the processor into a texture
//!   once, and again only when it changes.
//!
//! The scene is drawn as the processor draws it (`render`, `decor`): the
//! same shapes, colours and coverage, from the same formulas.
//!
//! Setting up (connecting to the service, making the pictures render
//! targets, compiling the programs and drawing with each once, so that the
//! driver has compiled them too) takes a thread of its own, so that the
//! screen goes on moving meanwhile. Before the first frame the compositor
//! checks that what the GPU draws reaches the picture's memory, where the
//! display reads it ([`Gpu::check`]). If any of it fails, or the GPU stops
//! answering later, frames are drawn by the processor, as without a GPU.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vgfx::{Bitmap, Rect};
use vgl::backend::External;
use vgl::veda::{FenceWatch, GpuTransport};
use vgl::virgl::VirglBackend;
use vgl::{Config, Context, gl};
use vmath::FloatExt;
use vproto::gpu::gpu;
use vrt::object::{Event, Vmo};
use vrt::sync::Mutex;
use vrt::time::{Duration, now_ns};

use crate::screen::Screen;

/// How long setting up waits for the `gpu` service to appear (the
/// renderer starts after the GPU's driver has brought the engines up).
const SERVICE_WAIT_NS: u64 = 60_000_000_000;
/// How often it looks.
const SERVICE_POLL: Duration = Duration::from_millis(250);
/// How long a call to the renderer may take before it counts as hung; on
/// softpipe (see `setup`).
const CALL_NS: u64 = 10_000_000_000;
const SOFTPIPE_CALL_NS: u64 = 60_000_000_000;
/// How long the check waits for the GPU to draw.
const CHECK_NS: u64 = 2_000_000_000;

/// Shared by the programs: a quad, `u_dest` in the screen's pixels. The
/// pictures' first row is the screen's top one, and the GPU's too (its
/// window coordinates have row 0 first in memory): the screen's
/// coordinates are the GPU's, unflipped.
const VERTEX: &str = "#version 300 es
in vec2 a_pos;
uniform vec2 u_screen;
uniform vec4 u_dest;
out vec2 v_pos;
void main() {
    v_pos = a_pos;
    vec2 p = u_dest.xy + a_pos * u_dest.zw;
    gl_Position = vec4(p / u_screen * 2.0 - 1.0, 0.0, 1.0);
}
";

/// Shapes: a rounded rectangle `u_rect` filled, outlined `-u_blur`
/// pixels wide inside its edge (`u_blur` < 0), or its shadow, blurred
/// over `u_blur` pixels (as `vgfx::ShadowTemplate` has it); in a colour,
/// or a vertical gradient. Colours are premultiplied.
const SHAPE: &str = "#version 300 es
precision highp float;
uniform vec4 u_rect;
uniform float u_radius;
uniform float u_blur;
uniform vec4 u_color;
uniform vec4 u_color2;
uniform float u_gradient;
out vec4 o;
void main() {
    vec2 p = gl_FragCoord.xy;
    vec2 hs = u_rect.zw * 0.5;
    vec2 q = abs(p - (u_rect.xy + hs)) - (hs - vec2(u_radius));
    float d = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - u_radius;
    float a;
    if (u_blur > 0.0) {
        float t = clamp(0.5 - d / (2.0 * u_blur), 0.0, 1.0);
        a = t * t * (3.0 - 2.0 * t);
    } else if (u_blur < 0.0) {
        a = clamp(min(0.5 - d, d - u_blur + 0.5), 0.0, 1.0);
    } else {
        a = clamp(0.5 - d, 0.0, 1.0);
    }
    vec4 c = u_color;
    if (u_gradient > 0.5) {
        float t = clamp((p.y - 0.5 - u_rect.y) / max(u_rect.w - 1.0, 1.0), 0.0, 1.0);
        c = mix(u_color, u_color2, t);
    }
    o = c * a;
}
";

/// Images: texels `u_src` of a `u_size` texture, at `u_opacity`; opaque
/// windows' alpha ignored; the bottom corners of `u_clip` rounded by
/// `u_radius` (a decorated window's client area).
const IMAGE: &str = "#version 300 es
precision highp float;
uniform sampler2D u_tex;
uniform vec4 u_src;
uniform vec2 u_size;
uniform float u_opacity;
uniform float u_opaque;
uniform vec4 u_clip;
uniform float u_radius;
in vec2 v_pos;
out vec4 o;
void main() {
    vec4 c = texture(u_tex, (u_src.xy + v_pos * u_src.zw) / u_size);
    if (u_opaque > 0.5) {
        c.a = 1.0;
    }
    float a = u_opacity;
    if (u_radius > 0.0) {
        vec2 p = gl_FragCoord.xy;
        float bottom = u_clip.y + u_clip.w;
        float left = u_clip.x + u_radius;
        float right = u_clip.x + u_clip.z - u_radius;
        if (p.y > bottom - u_radius && (p.x < left || p.x > right)) {
            vec2 centre = vec2(p.x < left ? left : right, bottom - u_radius);
            a *= clamp(u_radius - length(p - centre) + 0.5, 0.0, 1.0);
        }
    }
    o = c * a;
}
";

/// The startup sequence's gradient alone (`vsplash::background`): row by
/// row in the same integer arithmetic as the boot loader's. At
/// `u_opacity` (as it dissolves into the desktop).
const ROWS: &str = "#version 300 es
precision highp float;
precision highp int;
uniform vec2 u_screen;
uniform ivec3 u_top;
uniform ivec3 u_bottom;
uniform float u_opacity;
out vec4 o;
void main() {
    int y = int(gl_FragCoord.y);
    int t = (y * 256) / int(u_screen.y);
    ivec3 row = (u_top * (256 - t) + u_bottom * t) / 256;
    o = vec4(vec3(row) / 255.0, 1.0) * u_opacity;
}
";

/// The startup sequence's picture (`startup`, `vsplash`) where the light
/// reaches: the gradient as [`ROWS`] has it; the light around the ring,
/// with its glint; the ring. At `u_opacity`.
const SPLASH: &str = "#version 300 es
precision highp float;
precision highp int;
uniform vec2 u_screen;
uniform ivec3 u_top;
uniform ivec3 u_bottom;
uniform vec2 u_centre;
uniform float u_outer;
uniform float u_inner;
uniform float u_ring;
uniform float u_strength;
uniform float u_glint;
uniform vec3 u_glow;
uniform float u_opacity;
out vec4 o;
void main() {
    int y = int(gl_FragCoord.y);
    int t = (y * 256) / int(u_screen.y);
    ivec3 row = (u_top * (256 - t) + u_bottom * t) / 256;
    vec3 c = vec3(row) / 255.0;
    vec2 v = gl_FragCoord.xy - u_centre;
    float d = length(v);
    float f = 1.0;
    if (d >= u_outer) {
        f = 1.0 - (d - u_outer) / (0.95 * u_outer);
    } else if (d <= u_inner) {
        f = 1.0 - (u_inner - d) / (0.55 * u_inner);
    }
    f = max(f, 0.0);
    float away = 6.2831853 * (atan(v.x, -v.y) / 6.2831853 - u_glint);
    float glint = 1.0 + 0.6 * pow((1.0 + cos(away)) * 0.5, 8.0);
    c = mix(c, u_glow, min(f * f * u_strength * glint, 1.0));
    float cover = clamp(u_outer + 0.5 - d, 0.0, 1.0) * clamp(d - u_inner + 0.5, 0.0, 1.0);
    c = mix(c, vec3(1.0), cover * u_ring);
    o = vec4(c, 1.0) * u_opacity;
}
";

/// The part of `r` within `band` pixels of its edge, as rectangles that do
/// not overlap: strips across its top and bottom, and down its sides
/// between them. All of `r` if it is too small to have a middle.
fn border(r: Rect, band: i32) -> Vec<Rect> {
    if r.w <= 2 * band || r.h <= 2 * band {
        return alloc::vec![r];
    }
    alloc::vec![
        Rect::new(r.x, r.y, r.w, band),
        Rect::new(r.x, r.bottom() - band, r.w, band),
        Rect::new(r.x, r.y + band, band, r.h - 2 * band),
        Rect::new(r.right() - band, r.y + band, band, r.h - 2 * band),
    ]
}

/// A premultiplied `0xAARRGGBB` pixel as the GPU's components.
pub(crate) fn rgba(px: u32) -> [f32; 4] {
    let c = |s: u32| ((px >> s) & 0xFF) as f32 / 255.0;
    [c(16), c(8), c(0), c(24)]
}

/// A texture and its size.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Texture {
    id: u32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

struct ShapeProgram {
    id: u32,
    dest: i32,
    rect: i32,
    radius: i32,
    blur: i32,
    color: i32,
    color2: i32,
    gradient: i32,
}

struct ImageProgram {
    id: u32,
    dest: i32,
    src: i32,
    size: i32,
    opacity: i32,
    opaque: i32,
    clip: i32,
    radius: i32,
}

struct RowsProgram {
    id: u32,
    dest: i32,
    top: i32,
    bottom: i32,
    opacity: i32,
}

struct SplashProgram {
    id: u32,
    dest: i32,
    top: i32,
    bottom: i32,
    centre: i32,
    outer: i32,
    inner: i32,
    ring: i32,
    strength: i32,
    glint: i32,
    glow: i32,
    opacity: i32,
}

/// The startup sequence's picture at one moment (see `startup`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Splash {
    /// The gradient's colours at the top and the bottom (`0xRRGGBB`).
    pub(crate) top: u32,
    pub(crate) bottom: u32,
    /// The ring's centre and radii as it is now (pixels), and its opacity.
    pub(crate) centre: (f32, f32),
    pub(crate) outer: f32,
    pub(crate) inner: f32,
    pub(crate) ring: f32,
    /// The light's strength at the ring (0 to 1), its colour (`0xRRGGBB`),
    /// and where its glint is (turns clockwise from the top).
    pub(crate) strength: f32,
    pub(crate) glow: u32,
    pub(crate) glint: f32,
    /// Over what is under it (as it dissolves).
    pub(crate) opacity: f32,
}

/// One of the display's pictures, as the GPU draws into it.
struct Target {
    framebuffer: u32,
    _renderbuffer: u32,
}

/// Textures the compositor keeps by name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Slot {
    /// A window's contents, by its id.
    Window(u32),
    /// A decorated window's title bar, by its id.
    Title(u32),
    /// A cursor, by its shape's number.
    Cursor(u32),
    Switcher,
    /// The startup sequence's lines of words.
    Words(u8),
}

/// The display's pictures, which the GPU is to draw into.
pub(crate) struct Pictures {
    pub(crate) memory: Vec<Vmo>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    /// Bytes from one row to the next.
    pub(crate) stride: u32,
    /// Red in the low byte of each pixel (otherwise blue).
    pub(crate) rgb: bool,
}

/// The GPU, set up for composition.
pub(crate) struct Gpu {
    gl: Context,
    watch: FenceWatch,
    /// The renderer's name, for the log.
    pub(crate) renderer: String,
    width: i32,
    height: i32,
    targets: Vec<Target>,
    quad: u32,
    shape: ShapeProgram,
    image: ImageProgram,
    rows: RowsProgram,
    splash: SplashProgram,
    /// The program in use.
    current: u32,
    /// The textures, each with what it was made from, as its owner keys
    /// it.
    textures: BTreeMap<Slot, (Texture, u64)>,
    /// The GPU stopped answering.
    lost: bool,
}

/// A setup under way, in a thread of its own.
pub(crate) struct Pending {
    done: Event,
    result: Arc<Mutex<Option<Result<Ready, String>>>>,
}

/// A GPU set up, handed from the setup thread to the compositor's.
struct Ready(Gpu);

// SAFETY: the context is used by one thread at a time: made and tried by
// the setup thread, then handed over whole. What it holds are kernel
// handles and memory, which any thread may use.
unsafe impl Send for Ready {}

impl Pending {
    /// Sets the GPU up for `pictures` in a thread of its own, once the GPU
    /// service is there.
    pub(crate) fn start(pictures: Pictures) -> Option<Pending> {
        let done = Event::create().ok()?;
        let theirs = Event::from_handle(done.0.duplicate(None).ok()?);
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let body = move || {
            let r = setup(pictures).map(Ready);
            *slot.lock() = Some(r);
            let _ = theirs.signal();
        };
        vrt::thread::Builder::new().name("gpu-setup").spawn(body).ok()?;
        Some(Pending { done, result })
    }

    /// Signaled once the setup is over.
    pub(crate) fn event(&self) -> vabi::RawHandle {
        self.done.raw()
    }

    /// The GPU, or why it cannot compose, once the setup is over.
    pub(crate) fn take(&self) -> Option<Result<Gpu, String>> {
        self.result.lock().take().map(|r| r.map(|Ready(g)| g))
    }
}

/// Whether the `gpu` service is registered.
fn service_registered() -> bool {
    vproto::with_registry(|r| r.list()).ok().and_then(|l| l.ok()).is_some_and(|l| l.iter().any(|s| s == gpu::NAME))
}

fn shader(gl: &mut Context, kind: u32, src: &str) -> Result<u32, String> {
    let s = gl.create_shader(kind);
    gl.shader_source(s, &[src]);
    gl.compile_shader(s);
    if gl.get_shaderiv(s, gl::COMPILE_STATUS) == 0 {
        return Err(format!("a shader does not compile: {}", gl.get_shader_info_log(s)));
    }
    Ok(s)
}

/// A program of [`VERTEX`] and `fragment`, its screen's size set.
fn program(gl: &mut Context, fragment: &str, size: (i32, i32)) -> Result<u32, String> {
    let v = shader(gl, gl::VERTEX_SHADER, VERTEX)?;
    let f = shader(gl, gl::FRAGMENT_SHADER, fragment)?;
    let id = gl.create_program();
    gl.attach_shader(id, v);
    gl.attach_shader(id, f);
    gl.bind_attrib_location(id, 0, "a_pos");
    gl.link_program(id);
    gl.delete_shader(v);
    gl.delete_shader(f);
    if gl.get_programiv(id, gl::LINK_STATUS) == 0 {
        return Err(format!("a program does not link: {}", gl.get_program_info_log(id)));
    }
    gl.use_program(id);
    let at = gl.get_uniform_location(id, "u_screen");
    gl.uniform2f(at, size.0 as f32, size.1 as f32);
    Ok(id)
}

/// Sets the GPU up for `p` (see the module's comment).
fn setup(p: Pictures) -> Result<Gpu, String> {
    let until = now_ns() + SERVICE_WAIT_NS;
    while !service_registered() {
        if now_ns() >= until {
            return Err(String::from("there is no GPU service"));
        }
        vrt::time::sleep(SERVICE_POLL);
    }
    let t = GpuTransport::connect_to(gpu::NAME, CALL_NS).ok_or("the GPU service did not answer")?;
    let renderer = t.renderer().to_string();
    if renderer == "softpipe" {
        // Mesa's reference rasterizer, the tests' stand-in for a GPU: under
        // emulation it takes seconds to draw a whole screen.
        t.set_call_timeout(SOFTPIPE_CALL_NS);
    }
    let watch = t.watch();
    let backend = VirglBackend::new(Box::new(t)).map_err(|e| format!("{} cannot be used: {:?}", renderer, e))?;
    // No default framebuffer to speak of: frames go into the pictures.
    let config = Config { width: 1, height: 1, color: gl::RGBA8, depth_bits: 0, stencil_bits: 0, samples: 0 };
    let mut gl = Context::new(Box::new(backend), config);
    let (w, h) = (p.width, p.height);
    let mut targets = Vec::new();
    for vmo in &p.memory {
        let size = vmo.size().map_err(|_| "a picture is gone")?;
        let memory = External { handle: vmo.0.raw(), address: 0, size, stride: p.stride, bgr: !p.rgb };
        let rb = gl.gen_renderbuffer();
        gl.bind_renderbuffer(gl::RENDERBUFFER, rb);
        gl.renderbuffer_storage_external(gl::RENDERBUFFER, gl::RGB8, w, h, &memory);
        if gl.get_error() != gl::NO_ERROR {
            return Err(format!("{} cannot draw into the display's pictures", renderer));
        }
        let fb = gl.gen_framebuffer();
        gl.bind_framebuffer(gl::FRAMEBUFFER, fb);
        gl.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, rb);
        if gl.check_framebuffer_status(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
            return Err(format!("{} cannot draw into the display's pictures", renderer));
        }
        targets.push(Target { framebuffer: fb, _renderbuffer: rb });
    }
    let size = (w, h);
    let s = program(&mut gl, SHAPE, size)?;
    let shape = ShapeProgram {
        id: s,
        dest: gl.get_uniform_location(s, "u_dest"),
        rect: gl.get_uniform_location(s, "u_rect"),
        radius: gl.get_uniform_location(s, "u_radius"),
        blur: gl.get_uniform_location(s, "u_blur"),
        color: gl.get_uniform_location(s, "u_color"),
        color2: gl.get_uniform_location(s, "u_color2"),
        gradient: gl.get_uniform_location(s, "u_gradient"),
    };
    let i = program(&mut gl, IMAGE, size)?;
    let at = gl.get_uniform_location(i, "u_tex");
    gl.uniform1i(at, 0);
    let image = ImageProgram {
        id: i,
        dest: gl.get_uniform_location(i, "u_dest"),
        src: gl.get_uniform_location(i, "u_src"),
        size: gl.get_uniform_location(i, "u_size"),
        opacity: gl.get_uniform_location(i, "u_opacity"),
        opaque: gl.get_uniform_location(i, "u_opaque"),
        clip: gl.get_uniform_location(i, "u_clip"),
        radius: gl.get_uniform_location(i, "u_radius"),
    };
    let r = program(&mut gl, ROWS, size)?;
    let rows = RowsProgram {
        id: r,
        dest: gl.get_uniform_location(r, "u_dest"),
        top: gl.get_uniform_location(r, "u_top"),
        bottom: gl.get_uniform_location(r, "u_bottom"),
        opacity: gl.get_uniform_location(r, "u_opacity"),
    };
    let sp = program(&mut gl, SPLASH, size)?;
    let splash = SplashProgram {
        id: sp,
        dest: gl.get_uniform_location(sp, "u_dest"),
        top: gl.get_uniform_location(sp, "u_top"),
        bottom: gl.get_uniform_location(sp, "u_bottom"),
        centre: gl.get_uniform_location(sp, "u_centre"),
        outer: gl.get_uniform_location(sp, "u_outer"),
        inner: gl.get_uniform_location(sp, "u_inner"),
        ring: gl.get_uniform_location(sp, "u_ring"),
        strength: gl.get_uniform_location(sp, "u_strength"),
        glint: gl.get_uniform_location(sp, "u_glint"),
        glow: gl.get_uniform_location(sp, "u_glow"),
        opacity: gl.get_uniform_location(sp, "u_opacity"),
    };
    // The quad every draw is: two triangles, corner to corner.
    let quad = gl.gen_vertex_array();
    gl.bind_vertex_array(quad);
    let vbo = gl.gen_buffer();
    gl.bind_buffer(gl::ARRAY_BUFFER, vbo);
    let corners: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
    let bytes: Vec<u8> = corners.iter().flat_map(|f| f.to_le_bytes()).collect();
    gl.buffer_data(gl::ARRAY_BUFFER, &bytes, gl::STATIC_DRAW);
    gl.vertex_attrib_pointer(0, 2, gl::FLOAT, false, 8, 0);
    gl.enable_vertex_attrib_array(0);
    let mut g = Gpu {
        gl,
        watch,
        renderer,
        width: w,
        height: h,
        targets,
        quad,
        shape,
        image,
        rows,
        splash,
        current: 0,
        textures: BTreeMap::new(),
        lost: false,
    };
    g.warm_up()?;
    Ok(g)
}

impl Gpu {
    /// Draws with every program once, off the screen, so that the driver
    /// compiles them now rather than in the middle of the first frames.
    fn warm_up(&mut self) -> Result<(), String> {
        let gl = &mut self.gl;
        let rb = gl.gen_renderbuffer();
        gl.bind_renderbuffer(gl::RENDERBUFFER, rb);
        gl.renderbuffer_storage(gl::RENDERBUFFER, gl::RGB8, 16, 16);
        let fb = gl.gen_framebuffer();
        gl.bind_framebuffer(gl::FRAMEBUFFER, fb);
        gl.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, rb);
        if gl.check_framebuffer_status(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
            return Err(format!("{} cannot draw off the screen", self.renderer));
        }
        let pixel = Bitmap::new(1, 1);
        let t = self.upload(Slot::Words(u8::MAX), 0, &pixel);
        self.prepare(fb, Rect::new(0, 0, 16, 16));
        let r = Rect::new(0, 0, 16, 16);
        self.fill(r, 2.0, [0.5; 4]);
        self.image(t, Rect::new(0, 0, 1, 1), r, 1.0, false, None);
        let splash = Splash {
            top: 0,
            bottom: 0,
            centre: (8.0, 8.0),
            outer: 6.0,
            inner: 4.0,
            ring: 1.0,
            strength: 0.3,
            glow: 0,
            glint: 0.0,
            opacity: 1.0,
        };
        self.splash_rows(r, &splash);
        self.splash(r, &splash);
        self.forget(Slot::Words(u8::MAX));
        let gl = &mut self.gl;
        gl.finish();
        gl.delete_framebuffers(&[fb]);
        gl.delete_renderbuffers(&[rb]);
        if self.gl.get_error() != gl::NO_ERROR {
            return Err(format!("{} failed to draw", self.renderer));
        }
        Ok(())
    }

    /// Checks, before the first frame, that what the GPU draws into
    /// picture `target` (one the screen does not show) reaches the
    /// picture's memory, where the display reads it: the processor clears
    /// a few pixels, the GPU draws them, the processor reads them back.
    pub(crate) fn check(&mut self, screen: &mut Screen, target: usize) -> Result<(), String> {
        for x in 0..4 {
            screen.poke(target, x, 0);
        }
        let gl = &mut self.gl;
        gl.bind_framebuffer(gl::FRAMEBUFFER, self.targets[target].framebuffer);
        gl.viewport(0, 0, self.width, self.height);
        gl.enable(gl::SCISSOR_TEST);
        gl.scissor(0, 0, 4, 1);
        gl.clear_color(0x12 as f32 / 255.0, 0x34 as f32 / 255.0, 0x56 as f32 / 255.0, 1.0);
        gl.clear(gl::COLOR_BUFFER_BIT);
        let fence = gl.backend().fence();
        if fence == 0 || !gl.backend().wait_fence(fence, CHECK_NS) || self.watch.signaled() < fence {
            return Err(format!("{} did not draw", self.renderer));
        }
        let want = if screen.rgb() { 0x56_3412 } else { 0x12_3456 };
        for x in 0..4 {
            let got = screen.peek(target, x) & 0xFF_FFFF;
            if got != want {
                return Err(format!(
                    "what {} draws does not reach the display's memory (read {:#08x} where {:#08x} was drawn)",
                    self.renderer, got, want
                ));
            }
        }
        Ok(())
    }

    /// The fences' event, for the compositor's wait.
    pub(crate) fn fence_event(&self) -> vabi::RawHandle {
        self.watch.event()
    }

    /// Whether `fence` has signaled (the event is cleared first).
    pub(crate) fn signaled(&self, fence: u64) -> bool {
        self.watch.clear();
        self.watch.signaled() >= fence
    }

    /// Whether the GPU has stopped answering.
    pub(crate) fn lost(&self) -> bool {
        self.lost
    }

    /// Binds framebuffer `fb` and the drawing state: only `clip` is drawn
    /// in, blended over what is there (premultiplied).
    fn prepare(&mut self, fb: u32, clip: Rect) {
        let gl = &mut self.gl;
        gl.bind_framebuffer(gl::FRAMEBUFFER, fb);
        gl.viewport(0, 0, self.width, self.height);
        gl.enable(gl::SCISSOR_TEST);
        gl.scissor(clip.x, clip.y, clip.w.max(0), clip.h.max(0));
        gl.enable(gl::BLEND);
        gl.blend_func(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
        gl.bind_vertex_array(self.quad);
        gl.active_texture(gl::TEXTURE0);
    }

    /// Starts a frame into picture `target`, drawn only within `clip`.
    pub(crate) fn begin(&mut self, target: usize, clip: Rect) {
        let fb = self.targets[target].framebuffer;
        self.prepare(fb, clip);
    }

    /// Copies `rects` of picture `from` into picture `to`: what `to` lacks
    /// of the screen, from the picture that shows it (cheaper than drawing
    /// it again).
    pub(crate) fn copy(&mut self, from: usize, to: usize, rects: &[Rect]) {
        let gl = &mut self.gl;
        gl.bind_framebuffer(gl::READ_FRAMEBUFFER, self.targets[from].framebuffer);
        gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, self.targets[to].framebuffer);
        gl.disable(gl::SCISSOR_TEST);
        for r in rects {
            let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
            gl.blit_framebuffer(x0, y0, x1, y1, x0, y0, x1, y1, gl::COLOR_BUFFER_BIT, gl::NEAREST);
        }
    }

    /// Ends the frame: the GPU starts on it. The fence that signals once
    /// it is in the picture, or `None` if the GPU stopped answering.
    pub(crate) fn end(&mut self) -> Option<u64> {
        let fence = self.gl.backend().fence();
        if fence == 0 {
            self.lost = true;
            return None;
        }
        Some(fence)
    }

    fn use_program(&mut self, id: u32) {
        if self.current != id {
            self.gl.use_program(id);
            self.current = id;
        }
    }

    fn draw(&mut self) {
        self.gl.draw_arrays(gl::TRIANGLE_STRIP, 0, 4);
    }

    fn dest(&mut self, at: i32, r: Rect) {
        self.gl.uniform4f(at, r.x as f32, r.y as f32, r.w as f32, r.h as f32);
    }

    /// The shape program over `quad`, for the rounded rectangle `r`.
    #[allow(clippy::too_many_arguments)]
    fn shape(&mut self, quad: Rect, r: Rect, radius: f32, blur: f32, color: [f32; 4], bottom: Option<[f32; 4]>) {
        if quad.is_empty() {
            return;
        }
        self.use_program(self.shape.id);
        let s = &self.shape;
        let (dest, rect, rad, bl, col, col2, grad) = (s.dest, s.rect, s.radius, s.blur, s.color, s.color2, s.gradient);
        self.dest(dest, quad);
        let gl = &mut self.gl;
        gl.uniform4f(rect, r.x as f32, r.y as f32, r.w as f32, r.h as f32);
        gl.uniform1f(rad, radius.min(r.w as f32 / 2.0).min(r.h as f32 / 2.0).max(0.0));
        gl.uniform1f(bl, blur);
        gl.uniform4f(col, color[0], color[1], color[2], color[3]);
        let c2 = bottom.unwrap_or(color);
        gl.uniform4f(col2, c2[0], c2[1], c2[2], c2[3]);
        gl.uniform1f(grad, if bottom.is_some() { 1.0 } else { 0.0 });
        self.draw();
    }

    /// Fills the rectangle `r`, its corners rounded by `radius`.
    pub(crate) fn fill(&mut self, r: Rect, radius: f32, color: [f32; 4]) {
        self.shape(r, r, radius, 0.0, color, None);
    }

    /// Outlines `r` with a line `width` pixels wide inside its edge: drawn
    /// only along the edge, where its corners and the line are.
    pub(crate) fn stroke(&mut self, r: Rect, radius: f32, width: f32, color: [f32; 4]) {
        let band = radius.max(width).ceil() as i32 + 1;
        for q in border(r, band) {
            self.shape(q, r, radius, -width, color, None);
        }
    }

    /// The shadow of the rounded rectangle `r`, blurred over `blur`
    /// pixels around its edge. `hollow`: what is well inside `r` (fully
    /// dark, and under the opaque window that casts the shadow) is left
    /// out, as `vgfx::ShadowTemplate` leaves it.
    pub(crate) fn shadow(&mut self, r: Rect, radius: f32, blur: i32, color: [f32; 4], hollow: bool) {
        let outer = r.inflate(blur);
        if !hollow {
            return self.shape(outer, r, radius, blur as f32, color, None);
        }
        for q in border(outer, radius.ceil() as i32 + 2 * blur) {
            self.shape(q, r, radius, blur as f32, color, None);
        }
    }

    /// A vertical gradient over `r`, from `top` to `bottom`.
    pub(crate) fn gradient(&mut self, r: Rect, top: [f32; 4], bottom: [f32; 4]) {
        self.shape(r, r, 0.0, 0.0, top, Some(bottom));
    }

    /// Texels `src` of `t` at `dest` (the same size), at `opacity`; the
    /// texels' alpha ignored if `opaque`; `round`: the bottom corners of a
    /// rectangle rounded (a decorated window's client area).
    pub(crate) fn image(
        &mut self,
        t: Texture,
        src: Rect,
        dest: Rect,
        opacity: f32,
        opaque: bool,
        round: Option<(Rect, f32)>,
    ) {
        if dest.is_empty() || opacity <= 0.0 {
            return;
        }
        self.use_program(self.image.id);
        let p = &self.image;
        let (d, s, sz, op, oq, cl, rad) = (p.dest, p.src, p.size, p.opacity, p.opaque, p.clip, p.radius);
        self.dest(d, dest);
        let gl = &mut self.gl;
        gl.bind_texture(gl::TEXTURE_2D, t.id);
        gl.uniform4f(s, src.x as f32, src.y as f32, src.w as f32, src.h as f32);
        gl.uniform2f(sz, t.w as f32, t.h as f32);
        gl.uniform1f(op, opacity.min(1.0));
        gl.uniform1f(oq, if opaque { 1.0 } else { 0.0 });
        let (clip, radius) = round.unwrap_or((dest, 0.0));
        gl.uniform4f(cl, clip.x as f32, clip.y as f32, clip.w as f32, clip.h as f32);
        gl.uniform1f(rad, radius);
        self.draw();
    }

    /// The startup sequence's gradient alone over `r` (where the light does
    /// not reach).
    pub(crate) fn splash_rows(&mut self, r: Rect, s: &Splash) {
        if r.is_empty() || s.opacity <= 0.0 {
            return;
        }
        self.use_program(self.rows.id);
        let p = &self.rows;
        let (d, top, bottom, opacity) = (p.dest, p.top, p.bottom, p.opacity);
        self.dest(d, r);
        let gl = &mut self.gl;
        let channels = |c: u32| (((c >> 16) & 0xFF) as i32, ((c >> 8) & 0xFF) as i32, (c & 0xFF) as i32);
        let (t, b) = (channels(s.top), channels(s.bottom));
        gl.uniform3i(top, t.0, t.1, t.2);
        gl.uniform3i(bottom, b.0, b.1, b.2);
        gl.uniform1f(opacity, s.opacity.min(1.0));
        self.draw();
    }

    /// The startup sequence's picture over `r` (as far as the light
    /// reaches).
    pub(crate) fn splash(&mut self, r: Rect, s: &Splash) {
        if r.is_empty() || s.opacity <= 0.0 {
            return;
        }
        self.use_program(self.splash.id);
        let p = &self.splash;
        let at = [p.dest, p.top, p.bottom, p.centre, p.outer, p.inner, p.ring, p.strength, p.glint, p.glow, p.opacity];
        self.dest(at[0], r);
        let gl = &mut self.gl;
        let channels = |c: u32| (((c >> 16) & 0xFF) as i32, ((c >> 8) & 0xFF) as i32, (c & 0xFF) as i32);
        let (t, b) = (channels(s.top), channels(s.bottom));
        gl.uniform3i(at[1], t.0, t.1, t.2);
        gl.uniform3i(at[2], b.0, b.1, b.2);
        gl.uniform2f(at[3], s.centre.0, s.centre.1);
        gl.uniform1f(at[4], s.outer);
        gl.uniform1f(at[5], s.inner);
        gl.uniform1f(at[6], s.ring);
        gl.uniform1f(at[7], s.strength);
        gl.uniform1f(at[8], s.glint);
        let glow = rgba(0xFF00_0000 | s.glow);
        gl.uniform3f(at[9], glow[0], glow[1], glow[2]);
        gl.uniform1f(at[10], s.opacity.min(1.0));
        self.draw();
    }

    // ---- textures -----------------------------------------------------------

    /// A texture of `w` x `h` for 32-bit pixels as surfaces keep them
    /// (`0xAARRGGBB`, premultiplied: bytes B, G, R, A), read with red and
    /// blue swapped back.
    fn new_texture(&mut self, w: i32, h: i32) -> Texture {
        let gl = &mut self.gl;
        let id = gl.gen_texture();
        gl.bind_texture(gl::TEXTURE_2D, id);
        gl.tex_storage_2d(gl::TEXTURE_2D, 1, gl::RGBA8, w, h);
        for (p, v) in [
            (gl::TEXTURE_MIN_FILTER, gl::NEAREST),
            (gl::TEXTURE_MAG_FILTER, gl::NEAREST),
            (gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE),
            (gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE),
            (gl::TEXTURE_SWIZZLE_R, gl::BLUE),
            (gl::TEXTURE_SWIZZLE_B, gl::RED),
        ] {
            gl.tex_parameteri(gl::TEXTURE_2D, p, v as i32);
        }
        Texture { id, w, h }
    }

    /// Writes `r` of `pixels` (rows `stride` pixels apart) into `t`, at
    /// `r`'s place.
    fn write(&mut self, t: Texture, pixels: &[u32], stride: i32, r: Rect) {
        let r = r.intersect(&Rect::new(0, 0, t.w, t.h));
        if r.is_empty() {
            return;
        }
        let from = (r.y * stride + r.x) as usize;
        let to = ((r.bottom() - 1) * stride + r.right()) as usize;
        let Some(part) = pixels.get(from..to) else { return };
        // SAFETY: the same memory as bytes (no padding, any alignment).
        let bytes = unsafe { core::slice::from_raw_parts(part.as_ptr() as *const u8, part.len() * 4) };
        let gl = &mut self.gl;
        gl.bind_texture(gl::TEXTURE_2D, t.id);
        gl.pixel_storei(gl::UNPACK_ROW_LENGTH, stride);
        gl.tex_sub_image_2d(gl::TEXTURE_2D, 0, r.x, r.y, r.w, r.h, gl::RGBA, gl::UNSIGNED_BYTE, bytes);
        gl.pixel_storei(gl::UNPACK_ROW_LENGTH, 0);
    }

    /// The texture in `slot`, made of what `key` says (its owner's key for
    /// what it shows), if it is current.
    pub(crate) fn cached(&self, slot: Slot, key: u64) -> Option<Texture> {
        self.textures.get(&slot).filter(|(_, k)| *k == key).map(|(t, _)| *t)
    }

    /// Puts `b` into the texture in `slot` (made anew if its size
    /// changed), keyed `key`.
    pub(crate) fn upload(&mut self, slot: Slot, key: u64, b: &Bitmap) -> Texture {
        let t = self.sized(slot, b.width.max(1), b.height.max(1));
        self.write(t, &b.pixels, b.width, Rect::new(0, 0, b.width, b.height));
        self.textures.insert(slot, (t, key));
        t
    }

    /// The texture in `slot`, `w` x `h`: the same as before, or a new one
    /// (its contents undefined).
    fn sized(&mut self, slot: Slot, w: i32, h: i32) -> Texture {
        match self.textures.get(&slot) {
            Some(&(t, _)) if (t.w, t.h) == (w, h) => t,
            _ => {
                self.forget(slot);
                let t = self.new_texture(w, h);
                self.textures.insert(slot, (t, 0));
                t
            }
        }
    }

    /// A window's contents: its texture brought up to date from `pixels`
    /// (`w` x `h`, rows `stride` apart) where `changed` says (all of it if
    /// the texture is new or `all`).
    pub(crate) fn window(
        &mut self,
        id: u32,
        pixels: &[u32],
        w: i32,
        h: i32,
        stride: i32,
        all: bool,
        changed: &[Rect],
    ) -> Texture {
        let fresh = !matches!(self.textures.get(&Slot::Window(id)), Some(&(t, _)) if (t.w, t.h) == (w, h));
        let t = self.sized(Slot::Window(id), w, h);
        if fresh || all {
            self.write(t, pixels, stride, Rect::new(0, 0, w, h));
        } else {
            for &r in changed {
                self.write(t, pixels, stride, r);
            }
        }
        t
    }

    /// Whether `slot` has a texture.
    pub(crate) fn has(&self, slot: Slot) -> bool {
        self.textures.contains_key(&slot)
    }

    /// Deletes the texture in `slot`.
    pub(crate) fn forget(&mut self, slot: Slot) {
        if let Some((t, _)) = self.textures.remove(&slot) {
            self.gl.delete_textures(&[t.id]);
        }
    }

    /// Deletes every texture of window `id`.
    pub(crate) fn forget_window(&mut self, id: u32) {
        self.forget(Slot::Window(id));
        self.forget(Slot::Title(id));
    }
}
