//! Rendering tests: exact results where the specification makes them
//! exact, tight bounds where it allows a choice.

use super::*;

const VS_POS: &str = "#version 300 es
in vec4 pos;
void main() { gl_Position = pos; }";

const FS_RED: &str = "#version 300 es
precision mediump float;
out vec4 color;
void main() { color = vec4(1.0, 0.0, 0.0, 1.0); }";

const VS_COLOR: &str = "#version 300 es
in vec4 pos;
in vec4 col;
out vec4 v;
void main() { gl_Position = pos; v = col; }";

const FS_COLOR: &str = "#version 300 es
precision highp float;
in vec4 v;
out vec4 color;
void main() { color = v; }";

/// Sets `name`'s attribute array to `data` (`size` floats per vertex).
fn attrib(c: &mut Context, p: u32, name: &str, size: i32, data: &[f32]) -> u32 {
    let b = buffer_f32(c, gl::ARRAY_BUFFER, data);
    let loc = c.get_attrib_location(p, name);
    assert!(loc >= 0, "no attribute {name}");
    c.vertex_attrib_pointer(loc as u32, size, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(loc as u32);
    b
}

/// A rectangle from `(x0, y0)` to `(x1, y1)` in clip space at depth `z`, as
/// two triangles (6 vertices, xyzw).
fn quad(x0: f32, y0: f32, x1: f32, y1: f32, z: f32) -> Vec<f32> {
    let v = [(x0, y0), (x1, y0), (x0, y1), (x0, y1), (x1, y0), (x1, y1)];
    v.iter().flat_map(|&(x, y)| [x, y, z, 1.0]).collect()
}

fn count(img: &[u8], f: impl Fn([u8; 4]) -> bool) -> usize {
    img.chunks(4).filter(|p| f([p[0], p[1], p[2], p[3]])).count()
}

#[test]
fn clears_the_framebuffer() {
    let mut c = context(8, 8);
    c.clear_color(0.0, 0.5, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    no_error(&mut c);
    let img = read_rgba(&mut c, 8, 8);
    assert_close(&px(&img, 8, 3, 3), &[0, 128, 255, 255]);
    assert_close(&px(&img, 8, 7, 7), &[0, 128, 255, 255]);
}

#[test]
fn draws_a_triangle_with_the_fill_rule() {
    let mut c = context(16, 16);
    let p = program(&mut c, VS_POS, FS_RED);
    c.use_program(p);
    // Covers the lower-left half of the viewport.
    attrib(&mut c, p, "pos", 4, &[-1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 0.0, 1.0, -1.0, 1.0, 0.0, 1.0]);
    c.clear_color(0.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    no_error(&mut c);
    let img = read_rgba(&mut c, 16, 16);
    assert_eq!(px(&img, 16, 2, 2), [255, 0, 0, 255]);
    assert_eq!(px(&img, 16, 13, 13), [0, 0, 0, 255]);
    let first = count(&img, |p| p[0] == 255);
    // The complementary triangle covers exactly the rest.
    c.clear(gl::COLOR_BUFFER_BIT);
    attrib(&mut c, p, "pos", 4, &[1.0, -1.0, 0.0, 1.0, 1.0, 1.0, 0.0, 1.0, -1.0, 1.0, 0.0, 1.0]);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    let img = read_rgba(&mut c, 16, 16);
    assert_eq!(first + count(&img, |p| p[0] == 255), 256);
    // Pixels whose centres are on the diagonal belong to one of the two,
    // which OpenGL leaves to the implementation: here, the other one; GPUs
    // that rasterize with y down (Intel's) give them to this one.
    if virgl_requested() {
        assert!(first == 120 || first == 136, "{first}");
    } else {
        assert_eq!(first, 120);
    }
}

#[test]
fn shared_edges_cover_every_pixel_once() {
    // A fan of thin triangles around the centre, drawn with additive
    // blending: every pixel is hit exactly once.
    let (w, h) = (37, 29);
    let mut c = context(w, h);
    let p = program(
        &mut c,
        VS_POS,
        "#version 300 es
        precision mediump float; out vec4 color;
        void main() { color = vec4(0.0, 0.0, 0.0, 1.0 / 255.0); }",
    );
    c.use_program(p);
    let mut v = vec![0.13f32, -0.07, 0.0, 1.0];
    let n = 23;
    for i in 0..=n {
        let a = i as f32 / n as f32 * core::f32::consts::TAU;
        v.extend_from_slice(&[3.0 * vmath::f32::cos(a), 3.0 * vmath::f32::sin(a), 0.0, 1.0]);
    }
    attrib(&mut c, p, "pos", 4, &v);
    c.clear_color(0.0, 0.0, 0.0, 0.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.enable(gl::BLEND);
    c.blend_func(gl::ONE, gl::ONE);
    c.draw_arrays(gl::TRIANGLE_FAN, 0, n + 2);
    no_error(&mut c);
    let img = read_rgba(&mut c, w, h);
    assert!(img.chunks(4).all(|p| p[3] == 1), "pixels hit other than once");
}

#[test]
fn interpolates_varyings_with_perspective() {
    let mut c = context(64, 1);
    let p = program(&mut c, VS_COLOR, FS_COLOR);
    c.use_program(p);
    // A strip across the row: left end at w = 1, right end at w = 4.
    let (wl, wr) = (1.0f32, 4.0f32);
    let pos = [-wl, -wl, 0.0, wl, wr, -wr, 0.0, wr, -wl, wl, 0.0, wl, wr, wr, 0.0, wr];
    attrib(&mut c, p, "pos", 4, &pos);
    attrib(&mut c, p, "col", 4, &[0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0]);
    c.draw_arrays(gl::TRIANGLE_STRIP, 0, 4);
    let img = read_rgba(&mut c, 64, 1);
    for x in [0u32, 13, 32, 50, 63] {
        // Screen-space t at the pixel centre, then the perspective-correct
        // value t/wr / ((1-t)/wl + t/wr).
        let t = (x as f32 + 0.5) / 64.0;
        let v = (t / wr) / ((1.0 - t) / wl + t / wr);
        let got = px(&img, 64, x, 0)[0] as f32;
        assert!((got - v * 255.0).abs() <= 1.0, "x {x}: {got} vs {}", v * 255.0);
    }
}

#[test]
fn depth_test_keeps_the_nearest() {
    let mut c = context(8, 8);
    let p = program(&mut c, VS_COLOR, FS_COLOR);
    c.use_program(p);
    let mut pos = quad(-1.0, -1.0, 1.0, 1.0, 0.5);
    pos.extend(quad(-1.0, -1.0, 1.0, 1.0, -0.5));
    pos.extend(quad(-1.0, -1.0, 0.0, 1.0, 0.9));
    attrib(&mut c, p, "pos", 4, &pos);
    let mut col = Vec::new();
    for rgba in [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]] {
        for _ in 0..6 {
            col.extend_from_slice(&rgba);
        }
    }
    attrib(&mut c, p, "col", 4, &col);
    c.enable(gl::DEPTH_TEST);
    c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 18);
    let img = read_rgba(&mut c, 8, 8);
    // Green (z = -0.5) is nearest everywhere.
    assert!(img.chunks(4).all(|p| p == [0, 255, 0, 255]));
    // With GREATER, the farthest (blue, z = 0.9 on the left half) wins
    // there, red elsewhere.
    c.clear_depthf(0.0);
    c.depth_func(gl::GREATER);
    c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 18);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(px(&img, 8, 1, 4), [0, 0, 255, 255]);
    assert_eq!(px(&img, 8, 6, 4), [255, 0, 0, 255]);
    // A disabled depth mask leaves the depth buffer alone.
    c.depth_func(gl::LESS);
    c.clear_depthf(1.0);
    c.clear(gl::DEPTH_BUFFER_BIT);
    c.depth_mask(false);
    c.draw_arrays(gl::TRIANGLES, 12, 6);
    c.depth_mask(true);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(px(&img, 8, 1, 4), [255, 0, 0, 255]);
    no_error(&mut c);
}

#[test]
fn stencil_masks_drawing() {
    let mut c = context(8, 8);
    let p = program(&mut c, VS_COLOR, FS_COLOR);
    c.use_program(p);
    let mut pos = quad(-1.0, -1.0, 0.0, 1.0, 0.0);
    pos.extend(quad(-1.0, -1.0, 1.0, 1.0, 0.0));
    attrib(&mut c, p, "pos", 4, &pos);
    attrib(&mut c, p, "col", 4, &[1.0; 48]);
    c.enable(gl::STENCIL_TEST);
    c.clear_stencil(0);
    c.clear(gl::COLOR_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
    // Mark the left half with 5, writing no color.
    c.stencil_func(gl::ALWAYS, 5, 0xFF);
    c.stencil_op(gl::KEEP, gl::KEEP, gl::REPLACE);
    c.color_mask(false, false, false, false);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.color_mask(true, true, true, true);
    // Draw the whole screen where the stencil is 5.
    c.stencil_func(gl::EQUAL, 5, 0xFF);
    c.stencil_op(gl::KEEP, gl::KEEP, gl::INCR);
    c.draw_arrays(gl::TRIANGLES, 6, 6);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(px(&img, 8, 1, 3), [255, 255, 255, 255]);
    assert_eq!(px(&img, 8, 6, 3), [0, 0, 0, 0]);
    // The left half is now 6: drawing where it is 5 does nothing.
    c.clear_color(0.0, 0.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 6, 6);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(px(&img, 8, 1, 3), [0, 0, 255, 255]);
    no_error(&mut c);
}

#[test]
fn blends_exactly() {
    let mut c = context(4, 4);
    let p = program(
        &mut c,
        VS_POS,
        "#version 300 es
        precision mediump float; uniform vec4 u; out vec4 color;
        void main() { color = u; }",
    );
    c.use_program(p);
    attrib(&mut c, p, "pos", 4, &quad(-1.0, -1.0, 1.0, 1.0, 0.0));
    let u = c.get_uniform_location(p, "u");
    c.clear_color(0.2, 0.4, 0.6, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.enable(gl::BLEND);
    c.blend_func(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
    c.uniform4f(u, 1.0, 0.0, 0.0, 0.25);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 4);
    // dst bytes 51, 102, 153, 255; result = 0.25 * src + 0.75 * dst.
    let e = |s: f32, d: u8| ((0.25 * s + 0.75 * (d as f32 / 255.0)) * 255.0 + 0.5) as u8;
    assert_close(&px(&img, 4, 1, 1), &[e(1.0, 51), e(0.0, 102), e(0.0, 153), e(0.25, 255)]);
    // Separate equations: reverse subtract on color, max on alpha.
    c.clear(gl::COLOR_BUFFER_BIT);
    c.blend_equation_separate(gl::FUNC_REVERSE_SUBTRACT, gl::MAX);
    c.blend_func(gl::ONE, gl::ONE);
    c.uniform4f(u, 0.1, 0.1, 0.1, 0.5);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 4);
    let sub = |d: u8| ((d as f32 / 255.0 - 0.1) * 255.0 + 0.5) as u8;
    assert_close(&px(&img, 4, 2, 2), &[sub(51), sub(102), sub(153), 255]);
    no_error(&mut c);
}

#[test]
fn scissor_limits_clears_and_draws() {
    let mut c = context(8, 8);
    c.clear_color(0.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.enable(gl::SCISSOR_TEST);
    c.scissor(2, 3, 3, 2);
    c.clear_color(1.0, 1.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(count(&img, |p| p[0] == 255), 6);
    assert_eq!(px(&img, 8, 2, 3)[0], 255);
    assert_eq!(px(&img, 8, 4, 4)[0], 255);
    assert_eq!(px(&img, 8, 5, 4)[0], 0);
    let p = program(&mut c, VS_POS, FS_RED);
    c.use_program(p);
    attrib(&mut c, p, "pos", 4, &quad(-1.0, -1.0, 1.0, 1.0, 0.0));
    c.scissor(0, 0, 1, 8);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(count(&img, |p| p == [255, 0, 0, 255]), 8);
    no_error(&mut c);
}

#[test]
fn culls_by_winding() {
    let mut c = context(8, 8);
    let p = program(&mut c, VS_POS, FS_RED);
    c.use_program(p);
    // Clockwise in window coordinates.
    attrib(&mut c, p, "pos", 4, &[-1.0, -1.0, 0.0, 1.0, -1.0, 1.0, 0.0, 1.0, 1.0, -1.0, 0.0, 1.0]);
    c.enable(gl::CULL_FACE);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    assert_eq!(count(&read_rgba(&mut c, 8, 8), |p| p[0] == 255), 0);
    c.front_face(gl::CW);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    assert!(count(&read_rgba(&mut c, 8, 8), |p| p[0] == 255) > 0);
    no_error(&mut c);
}

#[test]
fn front_facing_and_frag_coord() {
    let mut c = context(4, 4);
    let p = program(
        &mut c,
        VS_POS,
        "#version 300 es
        precision highp float; out vec4 color;
        void main() { color = vec4(gl_FrontFacing ? 1.0 : 0.0, gl_FragCoord.x / 4.0, gl_FragCoord.y / 4.0, 1.0); }",
    );
    c.use_program(p);
    // Counter-clockwise quad.
    attrib(&mut c, p, "pos", 4, &quad(-1.0, -1.0, 1.0, 1.0, 0.0));
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 4);
    let e = |v: f32| (v * 255.0 + 0.5) as u8;
    assert_eq!(px(&img, 4, 1, 2), [255, e(1.5 / 4.0), e(2.5 / 4.0), 255]);
}

/// Integer arithmetic in a fragment shader is exact, as the window
/// system's splash has it: a gradient ordered-dithered to whole steps (a
/// falling channel among them), from the pixel's position by shifts, masks
/// and divisions, the same as the processor's.
#[test]
fn integer_arithmetic_is_exact() {
    let (w, h) = (64u32, 48u32);
    let mut c = context(w, h);
    let p = program(
        &mut c,
        VS_POS,
        "#version 300 es
        precision highp float; precision highp int;
        uniform ivec3 u_top; uniform ivec3 u_bottom; uniform int u_h;
        out vec4 color;
        void main() {
            int x = int(gl_FragCoord.x);
            int y = int(gl_FragCoord.y);
            int t = 16 * ((2 * (x & 1) + 3 * (y & 1)) & 3)
                + 4 * ((2 * ((x >> 1) & 1) + 3 * ((y >> 1) & 1)) & 3)
                + ((2 * ((x >> 2) & 1) + 3 * ((y >> 2) & 1)) & 3);
            ivec3 v = u_top * (128 * u_h) + (u_bottom - u_top) * ((2 * y + 1) * 64) + ivec3((2 * t + 1) * u_h);
            color = vec4(vec3(v / (128 * u_h)) / 255.0, 1.0);
        }",
    );
    c.use_program(p);
    let (top, bottom) = ([7, 200, 22], [250, 26, 53]);
    let at = |c: &mut Context, name: &str| c.get_uniform_location(p, name);
    let (t, b, hl) = (at(&mut c, "u_top"), at(&mut c, "u_bottom"), at(&mut c, "u_h"));
    c.uniform3i(t, top[0], top[1], top[2]);
    c.uniform3i(b, bottom[0], bottom[1], bottom[2]);
    c.uniform1i(hl, h as i32);
    attrib(&mut c, p, "pos", 4, &quad(-1.0, -1.0, 1.0, 1.0, 0.0));
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, w, h);
    let level = |x: u32, y: u32, s: u32| (2 * ((x >> s) & 1) + 3 * ((y >> s) & 1)) & 3;
    for y in 0..h {
        for x in 0..w {
            let t = (16 * level(x, y, 0) + 4 * level(x, y, 1) + level(x, y, 2)) as i32;
            let (y, hh) = (y as i32, h as i32);
            let want = |i: usize| {
                ((top[i] * 128 * hh + (bottom[i] - top[i]) * (2 * y + 1) * 64 + (2 * t + 1) * hh) / (128 * hh)) as u8
            };
            assert_eq!(px(&img, w, x, y as u32), [want(0), want(1), want(2), 255], "({x}, {y})");
        }
    }
}

#[test]
fn indexed_draws_and_primitive_restart() {
    let mut c = context(8, 8);
    let p = program(&mut c, VS_POS, FS_RED);
    c.use_program(p);
    // Corners of the left and right halves.
    let v = [
        -1.0, -1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 1.0, -1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 1.0, 1.0,
        -1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 1.0, 0.0, 1.0,
    ];
    attrib(&mut c, p, "pos", 4, &v);
    let ib = c.gen_buffer();
    c.bind_buffer(gl::ELEMENT_ARRAY_BUFFER, ib);
    // Two strips of the halves, separated by a restart index.
    let idx: [u16; 9] = [0, 1, 2, 3, 0xFFFF, 4, 5, 6, 7];
    let bytes: Vec<u8> = idx.iter().flat_map(|i| i.to_le_bytes()).collect();
    c.buffer_data(gl::ELEMENT_ARRAY_BUFFER, &bytes, gl::STATIC_DRAW);
    c.enable(gl::PRIMITIVE_RESTART_FIXED_INDEX);
    c.draw_elements(gl::TRIANGLE_STRIP, 9, gl::UNSIGNED_SHORT, 0);
    no_error(&mut c);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(count(&img, |p| p[0] == 255), 64);
    // Without restart, 0xFFFF is an index past the buffer: robust access
    // makes its triangles degenerate, and nothing breaks.
    c.disable(gl::PRIMITIVE_RESTART_FIXED_INDEX);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.draw_elements(gl::TRIANGLE_STRIP, 9, gl::UNSIGNED_SHORT, 0);
    no_error(&mut c);
    // 8-bit and 32-bit indices, and an offset into the element buffer.
    let idx8: [u8; 7] = [9, 0, 1, 2, 2, 1, 3];
    c.buffer_data(gl::ELEMENT_ARRAY_BUFFER, &idx8, gl::STATIC_DRAW);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.draw_elements(gl::TRIANGLES, 6, gl::UNSIGNED_BYTE, 1);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(count(&img, |p| p[0] == 255), 32);
    let idx32: [u32; 6] = [4, 5, 6, 6, 5, 7];
    let bytes: Vec<u8> = idx32.iter().flat_map(|i| i.to_le_bytes()).collect();
    c.buffer_data(gl::ELEMENT_ARRAY_BUFFER, &bytes, gl::STATIC_DRAW);
    c.draw_elements(gl::TRIANGLES, 6, gl::UNSIGNED_INT, 0);
    assert_eq!(count(&read_rgba(&mut c, 8, 8), |p| p[0] == 255), 64);
    no_error(&mut c);
}

#[test]
fn instancing_with_divisors() {
    let mut c = context(8, 2);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 corner; in float column; in vec4 tint;
        out vec4 v;
        void main() {
            // Instance i covers column i of 4 (quarter of the width).
            float x = -1.0 + 0.5 * (float(gl_InstanceID) + corner.x);
            gl_Position = vec4(x, corner.y * 2.0 - 1.0, 0.0, 1.0);
            v = tint * column;
        }",
        FS_COLOR,
    );
    c.use_program(p);
    attrib(&mut c, p, "corner", 2, &[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0]);
    let col = attrib(&mut c, p, "column", 1, &[1.0, 0.5, 0.25, 0.0]);
    let _ = col;
    let loc = c.get_attrib_location(p, "column") as u32;
    c.vertex_attrib_divisor(loc, 1);
    let tint = attrib(&mut c, p, "tint", 4, &[1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
    let _ = tint;
    let loc = c.get_attrib_location(p, "tint") as u32;
    c.vertex_attrib_divisor(loc, 2);
    c.draw_arrays_instanced(gl::TRIANGLES, 0, 6, 4);
    no_error(&mut c);
    let img = read_rgba(&mut c, 8, 2);
    assert_eq!(px(&img, 8, 0, 0), [255, 0, 0, 255]);
    assert_close(&px(&img, 8, 2, 1), &[128, 0, 0, 128]);
    assert_eq!(px(&img, 8, 5, 0), [0, 64, 0, 64]);
    assert_eq!(px(&img, 8, 7, 1), [0, 0, 0, 0]);
}

#[test]
fn glsl_100_programs_and_current_attributes() {
    let mut c = context(4, 4);
    let p = program(
        &mut c,
        "attribute vec4 pos; attribute vec4 tint; varying vec4 v;
        void main() { gl_Position = pos; v = tint; }",
        "precision mediump float; varying vec4 v;
        void main() { gl_FragColor = v; }",
    );
    c.use_program(p);
    attrib(&mut c, p, "pos", 4, &quad(-1.0, -1.0, 1.0, 1.0, 0.0));
    // A disabled array reads the current value.
    let loc = c.get_attrib_location(p, "tint") as u32;
    c.vertex_attrib4f(loc, 0.0, 1.0, 1.0, 1.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 4, 4), 4, 2, 2), [0, 255, 255, 255]);
    no_error(&mut c);
}

#[test]
fn rendering_is_the_same_on_many_threads() {
    let draw = |threads: usize| {
        let mut c = context_with(150, 97, threads, 0);
        let p = program(&mut c, VS_COLOR, FS_COLOR);
        c.use_program(p);
        let mut pos = Vec::new();
        let mut col = Vec::new();
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        for _ in 0..300 * 3 {
            pos.extend_from_slice(&[rnd() * 2.4 - 1.2, rnd() * 2.4 - 1.2, rnd() * 2.0 - 1.0, 1.0]);
            col.extend_from_slice(&[rnd(), rnd(), rnd(), rnd()]);
        }
        attrib(&mut c, p, "pos", 4, &pos);
        attrib(&mut c, p, "col", 4, &col);
        c.enable(gl::DEPTH_TEST);
        c.enable(gl::BLEND);
        c.blend_func(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
        c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
        c.draw_arrays(gl::TRIANGLES, 0, 900);
        read_rgba(&mut c, 150, 97)
    };
    let one = draw(1);
    assert!(one.chunks(4).any(|p| p != [0, 0, 0, 0]));
    assert_eq!(one, draw(4));
}

#[test]
fn presents_top_down_argb() {
    for (threads, samples) in [(1, 0), (3, 0), (2, 4)] {
        let mut c = context_with(3, 2, threads, samples);
        c.clear_color(0.0, 0.0, 1.0, 0.5);
        c.clear(gl::COLOR_BUFFER_BIT);
        // The top row (GL row 1) red.
        c.enable(gl::SCISSOR_TEST);
        c.scissor(0, 1, 3, 1);
        c.clear_color(1.0, 0.0, 0.0, 1.0);
        c.clear(gl::COLOR_BUFFER_BIT);
        let mut out = vec![0u32; 4 * 2];
        let mut dst = Present { pixels: &mut out, stride: 4, width: 3, height: 2, opaque: true };
        c.present_to(&mut dst);
        assert_eq!(&out[0..3], &[0xFFFF_0000; 3], "threads {threads}, samples {samples}");
        assert_eq!(&out[4..7], &[0xFF00_00FF; 3]);
        // Premultiplied: blue at half alpha.
        let mut dst = Present { pixels: &mut out, stride: 4, width: 3, height: 2, opaque: false };
        c.present_to(&mut dst);
        assert_close(&out[4].to_be_bytes(), &0x8000_0080u32.to_be_bytes());
        // Scaled to a window twice as high: the top half red, the bottom
        // blue, a blend between.
        let mut tall = vec![0u32; 3 * 4];
        let mut dst = Present { pixels: &mut tall, stride: 3, width: 3, height: 4, opaque: true };
        c.present_to(&mut dst);
        assert_eq!(tall[0], 0xFFFF_0000);
        assert_eq!(tall[9], 0xFF00_00FF);
        assert!(tall[3] & 0xFF > 0 && tall[3] >> 16 & 0xFF > 0, "{:#x}", tall[3]);
    }
}
