//! The scene renders on the host: with the software renderer, and on the
//! host's GPU through virglrenderer where QEMU (with it) is installed.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vgl::backend::Present;
use vgl::soft::{Serial, SoftBackend};
use vgl::{Config, Context, gl};

use crate::scene::{Controls, Scene};

/// The frame size (`PRISM_SIZE=WxH`, 160x100 by default).
fn size() -> (u32, u32) {
    let parse = |s: String| {
        let (a, b) = s.split_once('x')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    };
    std::env::var("PRISM_SIZE").ok().and_then(parse).unwrap_or((160, 100))
}

/// Renders `PRISM_FRAMES` frames (3 by default) and returns the last, as
/// a window would show it.
fn render(c: &mut Context, w: u32, h: u32) -> Vec<u32> {
    let mut scene = match Scene::new(c) {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    };
    let t0 = std::time::Instant::now();
    let frames = std::env::var("PRISM_FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    for _ in 0..frames {
        scene.update(1.0 / 30.0, Controls::default());
        scene.render(c, w as i32, h as i32, 1.0 / 30.0);
    }
    assert_eq!(c.get_error(), gl::NO_ERROR);
    let mut px = vec![0u32; (w * h) as usize];
    c.present_to(&mut Present { pixels: &mut px, stride: w as usize, width: w, height: h, opaque: true });
    std::println!("{frames} frames in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
    px
}

/// Sky at the top, something brighter or darker than it in the middle
/// (the knot), every pixel painted, and plenty of colors.
fn looks_like_the_scene(px: &[u32], w: u32, h: u32) {
    let top = px[(w * 2 + w / 2) as usize];
    let middle = px[(w * (h / 2) + w / 2) as usize];
    assert_ne!(top, 0);
    assert_ne!(top, middle);
    assert!(px.iter().all(|&p| p >> 24 == 0xFF));
    let mut v = px.to_vec();
    v.sort_unstable();
    v.dedup();
    assert!(v.len() > 500, "{} distinct colours", v.len());
}

/// Saves a frame as PNG if the variable `var` names a path.
fn save(var: &str, px: &[u32], w: u32, h: u32) {
    if let Ok(path) = std::env::var(var) {
        let image = vimage::Image { width: w, height: h, pixels: px.to_vec() };
        std::fs::write(path, vimage::png::encode(&image, 6).unwrap()).unwrap();
    }
}

#[test]
fn renders_the_scene() {
    let (w, h) = size();
    let config = Config { width: w, height: h, ..Config::default() };
    let mut c = Context::new(Box::new(SoftBackend::new(Box::new(Serial))), config);
    let px = render(&mut c, w, h);
    looks_like_the_scene(&px, w, h);
    // PRISM_SHOT=path saves the frame for a look.
    save("PRISM_SHOT", &px, w, h);
}

/// The same frame on the GPU looks the same: the shaders, translated to
/// TGSI, compute what the software renderer computes, to within the GPU's
/// precision (and its slightly different rasterization of edges).
#[test]
fn renders_the_same_on_the_gpu() {
    let (w, h) = size();
    let config = Config { width: w, height: h, ..Config::default() };
    let Some(mut gpu) = vgl::virgl::host::context(config) else {
        std::println!("skipped: virglrenderer is not installed");
        return;
    };
    let px = render(&mut gpu, w, h);
    save("PRISM_GPU_SHOT", &px, w, h);
    same_as_software(&px, w, h);
}

/// And through Veda's renderer, on softpipe (`vgallium.dll`), as Veda
/// renders without virtio-gpu.
#[test]
fn renders_the_same_through_the_renderer() {
    let (w, h) = size();
    let config = Config { width: w, height: h, ..Config::default() };
    let Some(mut gpu) = vgl::virgl::gallium::context(config) else {
        std::println!("skipped: the renderer (vgallium.dll) is not built");
        return;
    };
    let px = render(&mut gpu, w, h);
    save("PRISM_RENDERER_SHOT", &px, w, h);
    same_as_software(&px, w, h);
}

/// A frame looks like the scene, and like the software renderer's.
fn same_as_software(px: &[u32], w: u32, h: u32) {
    looks_like_the_scene(px, w, h);
    let config = Config { width: w, height: h, ..Config::default() };
    let mut soft = Context::new(Box::new(SoftBackend::new(Box::new(Serial))), config);
    let reference = render(&mut soft, w, h);
    // Per channel: the mean difference, and how many pixels differ much.
    let channels = |p: u32| [(p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF];
    let mut total = 0u64;
    let mut far = 0usize;
    for (&a, &b) in px.iter().zip(&reference) {
        let d = channels(a).iter().zip(channels(b)).map(|(x, y)| x.abs_diff(y)).max().unwrap_or(0);
        total += u64::from(d);
        if d > 48 {
            far += 1;
        }
    }
    let mean = total as f64 / px.len() as f64;
    std::println!("mean difference {mean:.2}, {far} pixels far apart");
    assert!(mean < 3.0, "mean difference {mean}");
    assert!(far * 100 < px.len(), "{far} pixels far apart");
}
