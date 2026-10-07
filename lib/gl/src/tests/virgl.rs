//! The virgl renderer on the host's GPU, where virglrenderer is installed
//! (the whole suite runs on it with `VGL_TEST_BACKEND=virgl`; these always
//! do, and are skipped without it).

use std::println;
use std::vec;
use std::vec::Vec;

use super::*;
use crate::backend::Present;

fn gpu(w: u32, h: u32, samples: u32) -> Option<Context> {
    let c = virgl_context(Config { width: w, height: h, samples, ..Config::default() });
    if c.is_none() {
        println!("skipped: virglrenderer is not installed");
    }
    c
}

#[test]
fn clears_and_reads_back() {
    let Some(mut c) = gpu(8, 4, 0) else { return };
    c.clear_color(0.25, 0.5, 0.75, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    let img = read_rgba(&mut c, 8, 4);
    assert_eq!(px(&img, 8, 0, 0), [64, 128, 191, 255]);
    assert_eq!(px(&img, 8, 7, 3), [64, 128, 191, 255]);
    no_error(&mut c);
}

#[test]
fn draws_a_triangle() {
    let Some(mut c) = gpu(16, 16, 0) else { return };
    let p = program(
        &mut c,
        "#version 300 es\nin vec2 pos; out vec2 v; void main() { v = pos; gl_Position = vec4(pos, 0.0, 1.0); }",
        "#version 300 es\nprecision highp float; in vec2 v; uniform vec4 tint; out vec4 o;\n\
         void main() { o = vec4(v * 0.5 + 0.5, 0.0, 1.0) * tint; }",
    );
    c.use_program(p);
    let tint = c.get_uniform_location(p, "tint");
    c.uniform4f(tint, 1.0, 1.0, 1.0, 1.0);
    let b = buffer_f32(&mut c, gl::ARRAY_BUFFER, &[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0]);
    let _ = b;
    let loc = c.get_attrib_location(p, "pos") as u32;
    c.enable_vertex_attrib_array(loc);
    c.vertex_attrib_pointer(loc, 2, gl::FLOAT, false, 0, 0);
    c.clear_color(0.0, 0.0, 0.0, 0.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    let img = read_rgba(&mut c, 16, 16);
    // The pixel at (x, y) sees v = ((x + 0.5) / 8 - 1, (y + 0.5) / 8 - 1).
    let expect = |x: u32, y: u32| {
        let f = |p: u32| (((p as f32 + 0.5) / 8.0 - 1.0) * 0.5 + 0.5) * 255.0;
        [f(x), f(y)]
    };
    for (x, y) in [(0, 0), (15, 0), (0, 15), (7, 9), (15, 15)] {
        let got = px(&img, 16, x, y);
        let [r, g] = expect(x, y);
        assert!((got[0] as f32 - r).abs() <= 1.0 && (got[1] as f32 - g).abs() <= 1.0, "({x}, {y}): {got:?}");
        assert_eq!(got[3], 255);
    }
    no_error(&mut c);
}

#[test]
fn presents_flipped_and_scaled() {
    let Some(mut c) = gpu(4, 4, 4) else { return };
    // Bottom half red, top half blue (GL rows count from the bottom).
    c.enable(gl::SCISSOR_TEST);
    c.scissor(0, 0, 4, 2);
    c.clear_color(1.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.scissor(0, 2, 4, 2);
    c.clear_color(0.0, 0.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    let mut pixels = vec![0u32; 8 * 8];
    let mut dst = Present { pixels: &mut pixels, stride: 8, width: 8, height: 8, opaque: true };
    c.present_to(&mut dst);
    // The window's rows count from the top: blue first.
    assert_eq!(pixels[0], 0xFF0000FF);
    assert_eq!(pixels[7 * 8 + 7], 0xFFFF0000);
    let rows: Vec<u32> = (0..8).map(|y| pixels[y * 8 + 3]).collect();
    println!("{rows:08x?}");
}

#[test]
fn presents_translucent_windows_premultiplied() {
    let Some(mut c) = gpu(2, 2, 0) else { return };
    c.clear_color(1.0, 0.0, 0.0, 0.5);
    c.clear(gl::COLOR_BUFFER_BIT);
    let mut pixels = vec![0u32; 4];
    let mut dst = Present { pixels: &mut pixels, stride: 2, width: 2, height: 2, opaque: false };
    c.present_to(&mut dst);
    // Alpha 128 (0.5 rounded); red 255 times 128/255.
    assert!(pixels.iter().all(|&p| p == 0x8080_0000), "{pixels:08x?}");
}
