//! Tests of the OpenGL ES implementation, with the software renderer.

mod api;
mod bench;
mod fbo;
mod pipeline;
mod render;
mod texture;
mod virgl;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::backend::Present;
use crate::soft::{SoftBackend, Workers};
use crate::{Config, Context, Pixels, PixelsMut, gl};

/// Threads from the standard library, for testing the parallel paths.
pub struct StdWorkers(pub usize);

impl Workers for StdWorkers {
    fn threads(&self) -> usize {
        self.0
    }

    fn run(&self, f: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|s| {
            for i in 1..self.0 {
                s.spawn(move || f(i));
            }
            f(0);
        });
    }
}

/// A context with a `w` x `h` default framebuffer (RGBA8, depth 24,
/// stencil 8), rendering on `threads` threads.
pub fn context_with(w: u32, h: u32, threads: usize, samples: u32) -> Context {
    let config = Config { width: w, height: h, samples, ..Config::default() };
    if virgl_requested() {
        return virgl_context(config).expect("VGL_TEST_BACKEND names a renderer that is not available");
    }
    let workers: Box<dyn Workers> = Box::new(StdWorkers(threads));
    Context::new(Box::new(SoftBackend::new(workers)), config)
}

/// Whether the tests are to run on the virgl renderer: through
/// virglrenderer on the host's GPU (`VGL_TEST_BACKEND=virgl`), or through
/// Veda's renderer on softpipe (`VGL_TEST_BACKEND=gallium`).
pub fn virgl_requested() -> bool {
    std::env::var("VGL_TEST_BACKEND").is_ok_and(|v| v == "virgl" || v == "gallium")
}

/// Whether the tests run on softpipe, through Veda's renderer
/// (`VGL_TEST_BACKEND=gallium`): its points sit an eighth of a pixel off
/// their place, and it filters depth before comparing it rather than the
/// comparisons. GPUs do neither.
pub fn on_softpipe() -> bool {
    std::env::var("VGL_TEST_BACKEND").is_ok_and(|v| v == "gallium")
}

/// Whether the renderer has multisampling (softpipe has none); tests of it
/// skip where it does not.
pub fn multisampling(c: &mut Context) -> bool {
    let n = c.get_integer(gl::MAX_SAMPLES);
    if n < 4 {
        std::println!("skipped: the renderer has no multisampling");
    }
    n >= 4
}

/// A context of the virgl renderer: through Veda's renderer if
/// `VGL_TEST_BACKEND=gallium`, otherwise through virglrenderer on the
/// host's GPU, if it is installed.
pub fn virgl_context(config: Config) -> Option<Context> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        if std::env::var("VGL_TEST_BACKEND").is_ok_and(|v| v == "gallium") {
            return crate::virgl::gallium::context(config);
        }
        crate::virgl::host::context(config)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = config;
        None
    }
}

pub fn context(w: u32, h: u32) -> Context {
    context_with(w, h, 1, 0)
}

/// Compiles and links a program, panicking with the logs on failure.
pub fn program(c: &mut Context, vs: &str, fs: &str) -> u32 {
    let v = c.create_shader(gl::VERTEX_SHADER);
    c.shader_source(v, &[vs]);
    c.compile_shader(v);
    assert_eq!(c.get_shaderiv(v, gl::COMPILE_STATUS), 1, "vertex shader: {}", c.get_shader_info_log(v));
    let f = c.create_shader(gl::FRAGMENT_SHADER);
    c.shader_source(f, &[fs]);
    c.compile_shader(f);
    assert_eq!(c.get_shaderiv(f, gl::COMPILE_STATUS), 1, "fragment shader: {}", c.get_shader_info_log(f));
    let p = c.create_program();
    c.attach_shader(p, v);
    c.attach_shader(p, f);
    c.link_program(p);
    assert_eq!(c.get_programiv(p, gl::LINK_STATUS), 1, "link: {}", c.get_program_info_log(p));
    c.delete_shader(v);
    c.delete_shader(f);
    p
}

/// A buffer holding `data` (bound to `target`).
pub fn buffer_f32(c: &mut Context, target: u32, data: &[f32]) -> u32 {
    let b = c.gen_buffer();
    c.bind_buffer(target, b);
    let bytes: Vec<u8> = data.iter().flat_map(|f| f.to_le_bytes()).collect();
    c.buffer_data(target, &bytes, gl::STATIC_DRAW);
    b
}

/// Reads the whole default framebuffer as RGBA bytes (row 0 at the
/// bottom).
pub fn read_rgba(c: &mut Context, w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    c.read_pixels(0, 0, w as i32, h as i32, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    out
}

/// The pixel at `(x, y)` of an RGBA image `w` wide.
pub fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let o = ((y * w + x) * 4) as usize;
    [img[o], img[o + 1], img[o + 2], img[o + 3]]
}

/// Asserts no error is pending.
#[track_caller]
pub fn no_error(c: &mut Context) {
    assert_eq!(c.get_error(), gl::NO_ERROR);
}

/// How far a color byte may be from the exact result. The software
/// renderer's filtering, blending and conversions are exact; a GPU's may
/// round the other way, by one step, as OpenGL ES allows.
pub fn tolerance() -> u8 {
    if virgl_requested() { 1 } else { 0 }
}

/// Asserts each byte is within [`tolerance`] of the expected one.
#[track_caller]
pub fn assert_close(got: &[u8], want: &[u8]) {
    let t = tolerance();
    assert!(got.len() == want.len() && got.iter().zip(want).all(|(g, w)| g.abs_diff(*w) <= t), "{got:?} != {want:?}");
}
