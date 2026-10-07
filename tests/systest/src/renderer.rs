//! Veda's renderer (`services/renderer`, when the image has it): OpenGL ES
//! through it, on Mesa's softpipe, clears and draws what it should. The
//! test runs a renderer of its own, under a name of its own: `gpu` may be
//! the machine's (virtio-gpu).

use alloc::format;
use alloc::string::{String, ToString};

use vabi::map_flags;
use vabi::startup::role;
use vgl::{Config, Context, gl};
use vrt::object::Process;
use vrt::process::Spawn;
use vrt::vm;

use crate::{TestResult, check, vfs_client};

const PATH: &str = "/system/bin/renderer.exe";
const NAME: &str = "gpu-systest";
/// How long the renderer may take to start and answer (TCG is slow).
const WAIT_NS: u64 = 60_000_000_000;

fn s<E: core::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Starts the program at `path` (which serves something) as `name`, with
/// `args`.
fn start(path: &str, name: &str, args: &[&str]) -> Result<Process, String> {
    let fs = vfs_client()?;
    let (vmo, size) = fs.read_file(path.into()).map_err(s)?.map_err(|e| format!("{path}: {e}"))?;
    let image = vm::Mapping::new(vmo, size as usize, map_flags::READ).map_err(s)?;
    let registry = vproto::with_registry(|r| r.clone_registry())
        .map_err(|e| format!("{e:?}"))?
        .map_err(s)?
        .map_err(|e| format!("{e:?}"))?;
    let mut spawn = Spawn::new(name).path(path).handle(role::REGISTRY, registry.into_handle());
    for a in args {
        spawn = spawn.arg(a);
    }
    // SAFETY: a read-only mapping of the VFS's copy of the file.
    spawn.start(unsafe { image.as_slice() }).map_err(|e| format!("{path}: {e}"))
}

fn exists(path: &str) -> Result<bool, String> {
    Ok(vfs_client()?.stat(path.into()).map_err(s)?.is_ok())
}

pub fn test_renderer() -> TestResult {
    if !exists(PATH)? {
        vrt::println!("renderer: not in this image (Mesa was not built)");
        return Ok(());
    }
    let name = format!("name={NAME}");
    let process = start(PATH, "renderer", &["softpipe", &name])?;
    let result = draw();
    let _ = process.kill();
    result
}

/// The GPU's render node, `/dev/dri/renderD128` (the POSIX layer's i915
/// interface, which Mesa's iris uses): the C checks of `tests/c/drm.c`,
/// against gemsim, a stand-in GPU that runs nothing. It serves `gem`, as
/// intel-gpu does on Intel's GPUs, which QEMU has not.
pub fn test_render_node() -> TestResult {
    const CHECKS: &str = "/system/tests/c/drm";
    if !exists(CHECKS)? {
        vrt::println!("render node: no C checks in this image (no cross toolchain was built)");
        return Ok(());
    }
    let gemsim = start("/system/bin/gemsim.exe", "gemsim", &[])?;
    let result = (|| {
        let (code, out) = crate::posix::run_c(CHECKS, &[], "/tmp")?;
        for line in out.lines().filter(|l| !l.starts_with("ok - ")) {
            vrt::println!("drm: {line}");
        }
        check(
            code == 0 && out.contains("PASS: 0 failed"),
            &format!("the render node's checks failed (exit code {code})"),
        )?;
        on_iris()
    })();
    let _ = gemsim.kill();
    result
}

/// The renderer on iris (on gemsim, which runs nothing): a context's
/// window has every buffer it asks for. Pixels cannot be checked here; the
/// formats the renderer offers can (iris keeps 24-bit depth in the lower
/// bits of each texel, unlike the OpenGL hosts vgl was written against).
fn on_iris() -> TestResult {
    const IRIS: &str = "gpu-systest-iris";
    let name = format!("name={IRIS}");
    let process = start(PATH, "renderer", &["iris", &name])?;
    let result = (|| {
        let config = Config { width: 16, height: 16, ..Config::default() };
        let mut c =
            vgl::veda::gpu_context_on(IRIS, WAIT_NS, config).ok_or("no context through the renderer on iris")?;
        let renderer = c.get_string(gl::RENDERER).unwrap_or_default();
        check(renderer.contains("Intel"), &format!("the renderer is iris ({renderer})"))?;
        let (depth, stencil) = (c.get_integer(gl::DEPTH_BITS), c.get_integer(gl::STENCIL_BITS));
        check((depth, stencil) == (24, 8), &format!("the window's depth and stencil bits ({depth}, {stencil})"))?;
        let missing = formats(&mut c);
        check(missing.is_empty(), &format!("formats iris does not take: {}", missing.join(", ")))?;
        // A frame's commands go through, and come back done.
        c.clear_color(0.25, 0.5, 0.75, 1.0);
        c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
        let mut px = [0u8; 16 * 16 * 4];
        c.read_pixels(0, 0, 16, 16, gl::RGBA, gl::UNSIGNED_BYTE, &mut px[..]);
        check(c.get_error() == gl::NO_ERROR, "no OpenGL error")
    })();
    let _ = process.kill();
    result
}

/// OpenGL ES 3.0's sized formats, which every implementation has: colors
/// it renders to (with `EXT_color_buffer_float`'s), colors it only samples,
/// and depth and stencil with where they attach.
const RENDERABLE: &[u32] = &[
    gl::R8,
    gl::RG8,
    gl::RGB8,
    gl::RGB565,
    gl::RGBA4,
    gl::RGB5_A1,
    gl::RGBA8,
    gl::RGB10_A2,
    gl::RGB10_A2UI,
    gl::SRGB8_ALPHA8,
    gl::R8I,
    gl::R8UI,
    gl::R16I,
    gl::R16UI,
    gl::R32I,
    gl::R32UI,
    gl::RG8I,
    gl::RG8UI,
    gl::RG16I,
    gl::RG16UI,
    gl::RG32I,
    gl::RG32UI,
    gl::RGBA8I,
    gl::RGBA8UI,
    gl::RGBA16I,
    gl::RGBA16UI,
    gl::RGBA32I,
    gl::RGBA32UI,
    gl::R16F,
    gl::RG16F,
    gl::RGBA16F,
    gl::R32F,
    gl::RG32F,
    gl::RGBA32F,
    gl::R11F_G11F_B10F,
];
const SAMPLED: &[u32] = &[
    gl::R8_SNORM,
    gl::RG8_SNORM,
    gl::RGB8_SNORM,
    gl::RGBA8_SNORM,
    gl::SRGB8,
    gl::RGB9_E5,
    gl::RGB16F,
    gl::RGB32F,
    gl::RGB8UI,
    gl::RGB8I,
    gl::RGB16UI,
    gl::RGB16I,
    gl::RGB32UI,
    gl::RGB32I,
];
const DEPTH_STENCIL: &[(u32, u32)] = &[
    (gl::DEPTH_COMPONENT16, gl::DEPTH_ATTACHMENT),
    (gl::DEPTH_COMPONENT24, gl::DEPTH_ATTACHMENT),
    (gl::DEPTH_COMPONENT32F, gl::DEPTH_ATTACHMENT),
    (gl::DEPTH24_STENCIL8, gl::DEPTH_STENCIL_ATTACHMENT),
    (gl::DEPTH32F_STENCIL8, gl::DEPTH_STENCIL_ATTACHMENT),
];

/// Every sized format as a texture, and those it renders to as the one
/// attachment of a framebuffer (and 8-bit stencil as a renderbuffer): the
/// ones that fail.
fn formats(c: &mut Context) -> alloc::vec::Vec<String> {
    let mut missing = alloc::vec::Vec::new();
    let fbo = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, fbo);
    let attach = RENDERABLE.iter().map(|&f| (f, Some(gl::COLOR_ATTACHMENT0)));
    let attach = attach.chain(SAMPLED.iter().map(|&f| (f, None)));
    for (f, at) in attach.chain(DEPTH_STENCIL.iter().map(|&(f, a)| (f, Some(a)))) {
        let t = c.gen_texture();
        c.bind_texture(gl::TEXTURE_2D, t);
        c.tex_storage_2d(gl::TEXTURE_2D, 1, f, 4, 4);
        let e = c.get_error();
        if e != gl::NO_ERROR {
            missing.push(format!("{f:#x} (error {e:#x})"));
        } else if let Some(at) = at {
            c.framebuffer_texture_2d(gl::FRAMEBUFFER, at, gl::TEXTURE_2D, t, 0);
            let status = c.check_framebuffer_status(gl::FRAMEBUFFER);
            if status != gl::FRAMEBUFFER_COMPLETE {
                missing.push(format!("{f:#x} (framebuffer {status:#x})"));
            }
            c.framebuffer_texture_2d(gl::FRAMEBUFFER, at, gl::TEXTURE_2D, 0, 0);
        }
        c.delete_textures(&[t]);
    }
    let r = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, r);
    c.renderbuffer_storage(gl::RENDERBUFFER, gl::STENCIL_INDEX8, 4, 4);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::STENCIL_ATTACHMENT, gl::RENDERBUFFER, r);
    let status = c.check_framebuffer_status(gl::FRAMEBUFFER);
    if c.get_error() != gl::NO_ERROR || status != gl::FRAMEBUFFER_COMPLETE {
        missing.push(format!("STENCIL_INDEX8 (framebuffer {status:#x})"));
    }
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    c.delete_framebuffers(&[fbo]);
    c.delete_renderbuffers(&[r]);
    missing
}

fn draw() -> TestResult {
    let config = Config { width: 16, height: 16, ..Config::default() };
    let mut c = vgl::veda::gpu_context_on(NAME, WAIT_NS, config).ok_or("no context through the renderer")?;
    check(c.get_string(gl::RENDERER).unwrap_or_default().contains("softpipe"), "the renderer is softpipe")?;

    // A clear.
    c.clear_color(0.25, 0.5, 0.75, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    let mut px = [0u8; 16 * 16 * 4];
    c.read_pixels(0, 0, 16, 16, gl::RGBA, gl::UNSIGNED_BYTE, &mut px[..]);
    check(px[..4] == [64, 128, 191, 255], &format!("the clear color reads back ({:?})", &px[..4]))?;

    // A triangle over the lower left half, in a uniform's color.
    let p = program(
        &mut c,
        "#version 300 es\nin vec2 pos; void main() { gl_Position = vec4(pos, 0.0, 1.0); }",
        "#version 300 es\nprecision mediump float; uniform vec4 tint; out vec4 o; void main() { o = tint; }",
    )?;
    c.use_program(p);
    let tint = c.get_uniform_location(p, "tint");
    c.uniform4f(tint, 1.0, 0.0, 0.0, 1.0);
    let b = c.gen_buffer();
    c.bind_buffer(gl::ARRAY_BUFFER, b);
    let corners: [f32; 6] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0];
    let bytes: alloc::vec::Vec<u8> = corners.iter().flat_map(|f| f.to_le_bytes()).collect();
    c.buffer_data(gl::ARRAY_BUFFER, &bytes, gl::STATIC_DRAW);
    let pos = c.get_attrib_location(p, "pos") as u32;
    c.enable_vertex_attrib_array(pos);
    c.vertex_attrib_pointer(pos, 2, gl::FLOAT, false, 0, 0);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    c.read_pixels(0, 0, 16, 16, gl::RGBA, gl::UNSIGNED_BYTE, &mut px[..]);
    let at = |x: usize, y: usize| [px[(y * 16 + x) * 4], px[(y * 16 + x) * 4 + 1], px[(y * 16 + x) * 4 + 2]];
    check(at(2, 2) == [255, 0, 0], &format!("inside the triangle ({:?})", at(2, 2)))?;
    check(at(13, 13) == [64, 128, 191], &format!("outside it ({:?})", at(13, 13)))?;
    check(c.get_error() == gl::NO_ERROR, "no OpenGL error")
}

fn program(c: &mut Context, vs: &str, fs: &str) -> Result<u32, String> {
    let shader = |c: &mut Context, kind: u32, src: &str| -> Result<u32, String> {
        let s = c.create_shader(kind);
        c.shader_source(s, &[src]);
        c.compile_shader(s);
        if c.get_shaderiv(s, gl::COMPILE_STATUS) != 1 {
            return Err(format!("shader: {}", c.get_shader_info_log(s)));
        }
        Ok(s)
    };
    let v = shader(c, gl::VERTEX_SHADER, vs)?;
    let f = shader(c, gl::FRAGMENT_SHADER, fs)?;
    let p = c.create_program();
    c.attach_shader(p, v);
    c.attach_shader(p, f);
    c.link_program(p);
    if c.get_programiv(p, gl::LINK_STATUS) != 1 {
        return Err(format!("link: {}", c.get_program_info_log(p)));
    }
    Ok(p)
}
