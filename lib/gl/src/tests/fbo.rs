//! Framebuffer object tests.

use super::*;

const VS: &str = "#version 300 es
in vec2 pos; out vec2 uv;
void main() { gl_Position = vec4(pos, 0.0, 1.0); uv = pos * 0.5 + 0.5; }";

/// A program over a full-viewport quad (attribute `pos`).
fn full(c: &mut Context, fs: &str) -> u32 {
    let p = program(c, VS, fs);
    c.use_program(p);
    buffer_f32(c, gl::ARRAY_BUFFER, &[-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 1.0]);
    let loc = c.get_attrib_location(p, "pos") as u32;
    c.vertex_attrib_pointer(loc, 2, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(loc);
    p
}

/// A framebuffer with a texture of `internalformat` as color attachment 0.
fn fbo_texture(c: &mut Context, internalformat: u32, w: i32, h: i32) -> (u32, u32) {
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_storage_2d(gl::TEXTURE_2D, 1, internalformat, w, h);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, t, 0);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    (f, t)
}

#[test]
fn renders_to_a_texture_and_samples_it() {
    let mut c = context(4, 4);
    let gradient = full(
        &mut c,
        "#version 300 es
        precision highp float; in vec2 uv; out vec4 color;
        void main() { color = vec4(uv, 0.0, 1.0); }",
    );
    let (f, t) = fbo_texture(&mut c, gl::RGBA8, 2, 2);
    c.viewport(0, 0, 2, 2);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let _ = gradient;
    // Sample it on the default framebuffer.
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    c.viewport(0, 0, 4, 4);
    full(
        &mut c,
        "#version 300 es
        precision highp float; uniform sampler2D t; in vec2 uv; out vec4 color;
        void main() { color = texture(t, uv); }",
    );
    c.bind_texture(gl::TEXTURE_2D, t);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 4, 4);
    // Texel (1, 0) of the 2x2 render was uv (0.75, 0.25).
    assert_eq!(px(&img, 4, 3, 0), [191, 64, 0, 255]);
    assert_eq!(px(&img, 4, 0, 3), [64, 191, 0, 255]);
    let _ = f;
}

#[test]
fn float_and_integer_targets() {
    let mut c = context(1, 1);
    full(
        &mut c,
        "#version 300 es
        precision highp float; out vec4 color;
        void main() { color = vec4(-2.5, 1000.3, 0.125, 3.0); }",
    );
    fbo_texture(&mut c, gl::RGBA32F, 1, 1);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let mut out = [0u8; 16];
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::FLOAT, &mut out[..]);
    no_error(&mut c);
    let v: Vec<f32> = out.chunks(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    assert_eq!(v, [-2.5, 1000.3, 0.125, 3.0]);
    // Half floats round to nearest (steps of 0.5 at 1000); a GPU may
    // round toward zero instead.
    fbo_texture(&mut c, gl::RGBA16F, 1, 1);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::FLOAT, &mut out[..]);
    let v: Vec<f32> = out.chunks(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let near = if tolerance() == 0 { v[1] == 1000.5 } else { v[1] == 1000.5 || v[1] == 1000.0 };
    assert!(near && [v[0], v[2], v[3]] == [-2.5, 0.125, 3.0], "{v:?}");
    // Integer buffers: written by integer outputs, cleared with
    // ClearBufferiv, read as integers.
    let mut c = context(2, 1);
    full(
        &mut c,
        "#version 300 es
        precision highp float; out ivec4 color;
        void main() { color = ivec4(-7, 70000, int(gl_FragCoord.x), 1); }",
    );
    fbo_texture(&mut c, gl::RGBA32I, 2, 1);
    c.clear_bufferiv(gl::COLOR, 0, &[1, 2, 3, 4]);
    let mut out = [0u8; 32];
    c.read_pixels(0, 0, 2, 1, gl::RGBA_INTEGER, gl::INT, &mut out[..]);
    assert_eq!(i32::from_le_bytes([out[16], out[17], out[18], out[19]]), 1);
    c.enable(gl::SCISSOR_TEST);
    c.scissor(1, 0, 1, 1);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.read_pixels(0, 0, 2, 1, gl::RGBA_INTEGER, gl::INT, &mut out[..]);
    no_error(&mut c);
    let v: Vec<i32> = out.chunks(4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    assert_eq!(v, [1, 2, 3, 4, -7, 70000, 1, 1]);
    // The wrong read format for an integer buffer is refused.
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

#[test]
fn multiple_render_targets() {
    let mut c = context(1, 1);
    full(
        &mut c,
        "#version 300 es
        precision highp float;
        layout(location = 0) out vec4 a;
        layout(location = 1) out vec4 b;
        void main() { a = vec4(1, 0, 0, 1); b = vec4(0, 0, 1, 1); }",
    );
    let (f, _) = fbo_texture(&mut c, gl::RGBA8, 1, 1);
    let t2 = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t2);
    c.tex_storage_2d(gl::TEXTURE_2D, 1, gl::RGBA8, 1, 1);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT1, gl::TEXTURE_2D, t2, 0);
    c.draw_buffers(&[gl::COLOR_ATTACHMENT0, gl::COLOR_ATTACHMENT1]);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let mut out = [0u8; 4];
    c.read_buffer(gl::COLOR_ATTACHMENT1);
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    assert_eq!(out, [0, 0, 255, 255]);
    c.read_buffer(gl::COLOR_ATTACHMENT0);
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    assert_eq!(out, [255, 0, 0, 255]);
    // Draw buffer 1 must be COLOR_ATTACHMENT1 or NONE.
    c.draw_buffers(&[gl::COLOR_ATTACHMENT0, gl::COLOR_ATTACHMENT0]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    let _ = f;
}

#[test]
fn depth_textures_render_and_sample() {
    let mut c = context(4, 1);
    let p = program(
        &mut c,
        "#version 300 es
        in vec3 pos; void main() { gl_Position = vec4(pos, 1.0); }",
        "#version 300 es
        precision highp float; out vec4 color; void main() { color = vec4(1); }",
    );
    c.use_program(p);
    // A ramp in depth across the row.
    buffer_f32(
        &mut c,
        gl::ARRAY_BUFFER,
        &[-1.0, -1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, 1.0, 1.0, 1.0],
    );
    let loc = c.get_attrib_location(p, "pos") as u32;
    c.vertex_attrib_pointer(loc, 3, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(loc);
    let d = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, d);
    c.tex_image_2d(
        gl::TEXTURE_2D,
        0,
        gl::DEPTH_COMPONENT24,
        4,
        1,
        0,
        gl::DEPTH_COMPONENT,
        gl::UNSIGNED_INT,
        Pixels::None,
    );
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::DEPTH_ATTACHMENT, gl::TEXTURE_2D, d, 0);
    // Depth only: no color attachment is needed (draw buffer NONE).
    c.draw_buffers(&[gl::NONE]);
    c.read_buffer(gl::NONE);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    c.enable(gl::DEPTH_TEST);
    c.clear(gl::DEPTH_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    // Read the depths back through a shader.
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    c.disable(gl::DEPTH_TEST);
    full(
        &mut c,
        "#version 300 es
        precision highp float; uniform highp sampler2D t; in vec2 uv; out vec4 color;
        void main() { color = vec4(texture(t, uv).r); }",
    );
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 1);
    // Window depth at pixel centres: (x + 0.5) / 4.
    for x in 0..4 {
        let want = ((x as f32 + 0.5) / 4.0 * 255.0 + 0.5) as i32;
        assert!((px(&img, 4, x, 0)[0] as i32 - want).abs() <= 1);
    }
}

#[test]
fn multisampling_resolves_edges() {
    let mut c = context(8, 8);
    if !multisampling(&mut c) {
        return;
    }
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0.0, 1.0); }",
        "#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }",
    );
    c.use_program(p);
    // A triangle with a diagonal edge.
    buffer_f32(&mut c, gl::ARRAY_BUFFER, &[-1.0, -1.0, 1.0, -1.0, -1.0, 1.0]);
    c.vertex_attrib_pointer(0, 2, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(0);
    let rb = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, rb);
    c.renderbuffer_storage_multisample(gl::RENDERBUFFER, 4, gl::RGBA8, 8, 8);
    assert_eq!(c.get_renderbuffer_parameteri(gl::RENDERBUFFER, gl::RENDERBUFFER_SAMPLES), 4);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, rb);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    // Reading a multisampled framebuffer object is refused; blit it.
    let mut out = [0u8; 4];
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.bind_framebuffer(gl::DRAW_FRAMEBUFFER, 0);
    c.blit_framebuffer(0, 0, 8, 8, 0, 0, 8, 8, gl::COLOR_BUFFER_BIT, gl::NEAREST);
    no_error(&mut c);
    c.bind_framebuffer(gl::READ_FRAMEBUFFER, 0);
    let img = read_rgba(&mut c, 8, 8);
    // Inside: all samples; outside: none; on the diagonal: some.
    assert_eq!(px(&img, 8, 1, 1)[0], 255);
    assert_eq!(px(&img, 8, 6, 6)[0], 0);
    let partial = img.chunks(4).filter(|p| p[0] > 0 && p[0] < 255).count();
    assert!(partial >= 8, "{partial} partially covered pixels");
}

#[test]
fn blits_scale_and_mirror() {
    let mut c = context(4, 4);
    // A 2x2 source texture in a framebuffer.
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let texels = [10u8, 0, 0, 255, 20, 0, 0, 255, 30, 0, 0, 255, 40, 0, 0, 255];
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, 2, 2, 0, gl::RGBA, gl::UNSIGNED_BYTE, &texels[..]);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::READ_FRAMEBUFFER, f);
    c.framebuffer_texture_2d(gl::READ_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, t, 0);
    // Upscale with x mirrored onto the default framebuffer.
    c.blit_framebuffer(0, 0, 2, 2, 4, 0, 0, 4, gl::COLOR_BUFFER_BIT, gl::NEAREST);
    no_error(&mut c);
    c.bind_framebuffer(gl::READ_FRAMEBUFFER, 0);
    let img = read_rgba(&mut c, 4, 4);
    assert_eq!(
        [px(&img, 4, 0, 0)[0], px(&img, 4, 3, 0)[0], px(&img, 4, 0, 3)[0], px(&img, 4, 3, 3)[0]],
        [20, 10, 40, 30]
    );
    // Linear filtering is refused for depth.
    c.blit_framebuffer(0, 0, 1, 1, 0, 0, 1, 1, gl::DEPTH_BUFFER_BIT, gl::LINEAR);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

#[test]
fn copies_from_the_framebuffer() {
    let mut c = context(4, 4);
    c.clear_color(0.0, 1.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.enable(gl::SCISSOR_TEST);
    c.scissor(1, 1, 1, 1);
    c.clear_color(1.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.disable(gl::SCISSOR_TEST);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.copy_tex_image_2d(gl::TEXTURE_2D, 0, gl::RGB8, 1, 1, 2, 2, 0);
    no_error(&mut c);
    // Read the copy back through a framebuffer.
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, t, 0);
    let mut out = [0u8; 16];
    c.read_pixels(0, 0, 2, 2, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    assert_eq!(&out[..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
    // Copying into an RGBA texture from an RGB one would add alpha.
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    let t2 = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t2);
    c.copy_tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA, 0, 0, 1, 1, 0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // A sub-image copy, partly outside the source.
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    c.bind_texture(gl::TEXTURE_2D, t);
    c.copy_tex_sub_image_2d(gl::TEXTURE_2D, 0, 0, 0, 3, 3, 2, 2);
    no_error(&mut c);
}

#[test]
fn reads_into_pack_buffers_with_alignment() {
    let mut c = context(3, 2);
    c.clear_color(1.0, 0.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    let b = c.gen_buffer();
    c.bind_buffer(gl::PIXEL_PACK_BUFFER, b);
    c.buffer_data(gl::PIXEL_PACK_BUFFER, &[0xEE; 64], gl::STREAM_READ);
    c.pixel_storei(gl::PACK_ALIGNMENT, 8);
    // RGB rows of 9 bytes padded to 16; the padding keeps its bytes.
    c.read_pixels(0, 0, 3, 2, gl::RGB, gl::UNSIGNED_BYTE, PixelsMut::Offset(4));
    // RGB is not a format the default framebuffer reads: refused.
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.read_pixels(0, 0, 3, 2, gl::RGBA, gl::UNSIGNED_BYTE, PixelsMut::Offset(4));
    no_error(&mut c);
    let mapped = c.map_buffer_range(gl::PIXEL_PACK_BUFFER, 0, 64, gl::MAP_READ_BIT).unwrap().to_vec();
    assert_eq!(&mapped[..4], &[0xEE; 4]);
    assert_eq!(&mapped[4..8], &[255, 0, 255, 255]);
    // Rows of 12 bytes padded to 16: bytes 16..20 are padding.
    assert_eq!(&mapped[16..20], &[0xEE; 4]);
    assert_eq!(&mapped[20..24], &[255, 0, 255, 255]);
    assert!(c.unmap_buffer(gl::PIXEL_PACK_BUFFER));
    // Past the buffer's end: refused.
    c.read_pixels(0, 0, 3, 2, gl::RGBA, gl::UNSIGNED_BYTE, PixelsMut::Offset(40));
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

/// A 4x4x3 RGBA8 texture whose slice `z` is all `(10z, 20, 30, 255)`.
fn volume(c: &mut Context) -> u32 {
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_3D, t);
    let vox: Vec<u8> = (0..3u8).flat_map(|z| [[10 * z, 20, 30, 255]; 16].concat()).collect();
    c.tex_image_3d(gl::TEXTURE_3D, 0, gl::RGBA8, 4, 4, 3, 0, gl::RGBA, gl::UNSIGNED_BYTE, &vox[..]);
    c.tex_parameteri(gl::TEXTURE_3D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_3D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    t
}

#[test]
fn renders_into_slices_of_3d_textures() {
    let mut c = context(4, 4);
    let t = volume(&mut c);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 0, 1);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    // A scissored clear of the left half, and a draw over the top half.
    c.enable(gl::SCISSOR_TEST);
    c.scissor(0, 0, 2, 4);
    c.clear_color(1.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.disable(gl::SCISSOR_TEST);
    full(
        &mut c,
        "#version 300 es
        precision highp float; in vec2 uv; out vec4 color;
        void main() { if (uv.y < 0.5) discard; color = vec4(0.0, 1.0, 0.0, 1.0); }",
    );
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 4, 4);
    assert_eq!(px(&img, 4, 0, 0), [255, 0, 0, 255]);
    assert_eq!(px(&img, 4, 3, 1), [10, 20, 30, 255]);
    assert_eq!(px(&img, 4, 0, 2), [0, 255, 0, 255]);
    assert_eq!(px(&img, 4, 3, 3), [0, 255, 0, 255]);
    // The other slices are as they were.
    for z in [0, 2] {
        c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 0, z);
        let img = read_rgba(&mut c, 4, 4);
        assert!(img.chunks(4).all(|p| p == [10 * z as u8, 20, 30, 255]), "slice {z}: {img:?}");
    }
    // Samplers see the drawing.
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    full(
        &mut c,
        "#version 300 es
        precision highp float; uniform highp sampler3D t; in vec2 uv; out vec4 color;
        void main() { color = texture(t, vec3(uv, 0.5)); }",
    );
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 4);
    assert_eq!(px(&img, 4, 1, 0), [255, 0, 0, 255]);
    assert_eq!(px(&img, 4, 2, 1), [10, 20, 30, 255]);
    assert_eq!(px(&img, 4, 2, 3), [0, 255, 0, 255]);
    no_error(&mut c);
}

#[test]
fn copies_and_blits_slices_of_3d_textures() {
    let mut c = context(4, 4);
    // The default framebuffer: left half blue, right half white.
    c.clear_color(0.0, 0.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.enable(gl::SCISSOR_TEST);
    c.scissor(2, 0, 2, 4);
    c.clear_color(1.0, 1.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.disable(gl::SCISSOR_TEST);
    let t = volume(&mut c);
    // Its box (1, 0)-(3, 2) into slice 1 at (0, 1).
    c.copy_tex_sub_image_3d(gl::TEXTURE_3D, 0, 0, 1, 1, 1, 0, 2, 2);
    no_error(&mut c);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 0, 1);
    let img = read_rgba(&mut c, 4, 4);
    let (blue, white, old) = ([0, 0, 255, 255], [255, 255, 255, 255], [10, 20, 30, 255]);
    assert_eq!(
        [px(&img, 4, 0, 1), px(&img, 4, 1, 1), px(&img, 4, 0, 2), px(&img, 4, 1, 2)],
        [blue, white, blue, white]
    );
    assert_eq!([px(&img, 4, 0, 0), px(&img, 4, 2, 1), px(&img, 4, 3, 3)], [old; 3]);
    // Slice 1 blitted to slice 2, mirrored, and slice 2 to a renderbuffer.
    let g = c.gen_framebuffer();
    c.bind_framebuffer(gl::DRAW_FRAMEBUFFER, g);
    c.framebuffer_texture_layer(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 0, 2);
    c.blit_framebuffer(0, 0, 4, 4, 4, 0, 0, 4, gl::COLOR_BUFFER_BIT, gl::NEAREST);
    no_error(&mut c);
    let rb = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, rb);
    c.renderbuffer_storage(gl::RENDERBUFFER, gl::RGBA8, 4, 4);
    let h = c.gen_framebuffer();
    c.bind_framebuffer(gl::READ_FRAMEBUFFER, g);
    c.bind_framebuffer(gl::DRAW_FRAMEBUFFER, h);
    c.framebuffer_renderbuffer(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, rb);
    c.blit_framebuffer(0, 0, 4, 4, 0, 0, 4, 4, gl::COLOR_BUFFER_BIT, gl::NEAREST);
    no_error(&mut c);
    c.bind_framebuffer(gl::READ_FRAMEBUFFER, h);
    let img = read_rgba(&mut c, 4, 4);
    assert_eq!(
        [px(&img, 4, 3, 1), px(&img, 4, 2, 1), px(&img, 4, 3, 2), px(&img, 4, 2, 2)],
        [blue, white, blue, white]
    );
    assert_eq!([px(&img, 4, 3, 0), px(&img, 4, 0, 1)], [old; 2]);
}

#[test]
fn integer_3d_textures_read_back() {
    let mut c = context(1, 1);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_3D, t);
    c.tex_storage_3d(gl::TEXTURE_3D, 1, gl::RGBA32I, 2, 1, 2);
    let values: Vec<u8> = [-7i32, 70000, 3, -1, 1, 2, 3, 4, 5, 6, 7, 8, i32::MIN, i32::MAX, 0, 9]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    c.tex_sub_image_3d(gl::TEXTURE_3D, 0, 0, 0, 0, 2, 1, 2, gl::RGBA_INTEGER, gl::INT, &values[..]);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 0, 1);
    let mut out = [0u8; 32];
    c.read_pixels(0, 0, 2, 1, gl::RGBA_INTEGER, gl::INT, &mut out[..]);
    no_error(&mut c);
    assert_eq!(&out[..], &values[32..]);
}

#[test]
fn new_render_targets_are_zero() {
    // Never what memory held before: the default framebuffer, and a
    // renderbuffer made after one of its size was drawn into and deleted
    // (whose memory it may take).
    let mut c = context(4, 4);
    let img = read_rgba(&mut c, 4, 4);
    assert!(img.iter().all(|&b| b == 0), "{img:?}");
    for round in 0..3 {
        let rb = c.gen_renderbuffer();
        c.bind_renderbuffer(gl::RENDERBUFFER, rb);
        c.renderbuffer_storage(gl::RENDERBUFFER, gl::RGBA8, 4, 4);
        let f = c.gen_framebuffer();
        c.bind_framebuffer(gl::FRAMEBUFFER, f);
        c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, rb);
        let img = read_rgba(&mut c, 4, 4);
        assert!(img.iter().all(|&b| b == 0), "round {round}: {img:?}");
        c.clear_color(1.0, 0.5, 0.25, 1.0);
        c.clear(gl::COLOR_BUFFER_BIT);
        c.bind_framebuffer(gl::FRAMEBUFFER, 0);
        c.delete_framebuffers(&[f]);
        c.delete_renderbuffers(&[rb]);
    }
    no_error(&mut c);
}

#[test]
fn stencil_only_framebuffers() {
    // A color texture and a stencil-only renderbuffer: the stencil masks a
    // draw, and a depth test without a depth buffer passes (even NEVER).
    let mut c = context(4, 4);
    let p = full(
        &mut c,
        "#version 300 es
        precision mediump float; uniform vec4 tint; out vec4 color;
        void main() { color = tint; }",
    );
    let tint = c.get_uniform_location(p, "tint");
    fbo_texture(&mut c, gl::RGBA8, 4, 4);
    let rb = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, rb);
    c.renderbuffer_storage(gl::RENDERBUFFER, gl::STENCIL_INDEX8, 4, 4);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::STENCIL_ATTACHMENT, gl::RENDERBUFFER, rb);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    c.viewport(0, 0, 4, 4);
    c.clear_color(0.0, 0.0, 0.0, 1.0);
    c.clear_stencil(0);
    c.clear(gl::COLOR_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
    // The left half marked in the stencil buffer only.
    c.enable(gl::STENCIL_TEST);
    c.stencil_func(gl::ALWAYS, 1, 0xFF);
    c.stencil_op(gl::KEEP, gl::KEEP, gl::REPLACE);
    c.enable(gl::SCISSOR_TEST);
    c.scissor(0, 0, 2, 4);
    c.color_mask(false, false, false, false);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.color_mask(true, true, true, true);
    c.disable(gl::SCISSOR_TEST);
    // Green where marked.
    c.stencil_func(gl::EQUAL, 1, 0xFF);
    c.stencil_op(gl::KEEP, gl::KEEP, gl::KEEP);
    c.enable(gl::DEPTH_TEST);
    c.depth_func(gl::NEVER);
    c.uniform4f(tint, 0.0, 1.0, 0.0, 1.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 4, 4);
    let (green, black) = ([0, 255, 0, 255], [0, 0, 0, 255]);
    assert_eq!(
        [px(&img, 4, 0, 0), px(&img, 4, 1, 3), px(&img, 4, 2, 0), px(&img, 4, 3, 3)],
        [green, green, black, black]
    );
}

#[test]
fn renders_into_memory_it_is_given() {
    // Rows of 8 pixels, 12 apart: the memory outlives the contexts.
    let (w, h, stride) = (8i32, 4i32, 12usize);
    let mut picture = vec![0xDEAD_BEEFu32; stride * h as usize];
    let memory = crate::backend::External {
        handle: 0,
        address: picture.as_mut_ptr() as usize,
        size: picture.len() * 4,
        stride: stride as u32 * 4,
        bgr: true,
    };
    // A renderer that cannot (the software one) says so.
    let mut soft = Context::new(Box::new(SoftBackend::new(Box::new(StdWorkers(1)))), Config::default());
    let rb = soft.gen_renderbuffer();
    soft.bind_renderbuffer(gl::RENDERBUFFER, rb);
    soft.renderbuffer_storage_external(gl::RENDERBUFFER, gl::RGB8, w, h, &memory);
    assert_eq!(soft.get_error(), gl::INVALID_OPERATION);
    // Veda's renderer draws into it (a display's picture), on softpipe here
    // (virgl's host takes no memory from outside).
    if !on_softpipe() {
        std::println!("skipped: only Veda's renderer on softpipe draws into memory it is given here");
        return;
    }
    let mut c = virgl_context(Config { width: 1, height: 1, ..Config::default() }).unwrap();
    let rb = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, rb);
    c.renderbuffer_storage_external(gl::RENDERBUFFER, gl::RGB8, w, h, &memory);
    no_error(&mut c);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, rb);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    // Blue everywhere, red in the window's row 0, green drawn over the
    // right half.
    c.viewport(0, 0, w, h);
    c.clear_color(0.0, 0.0, 1.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.enable(gl::SCISSOR_TEST);
    c.scissor(0, 0, w, 1);
    c.clear_color(1.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    c.disable(gl::SCISSOR_TEST);
    full(&mut c, "#version 300 es\nprecision mediump float; out vec4 o; void main() { o = vec4(0.0, 1.0, 0.0, 1.0); }");
    c.viewport(w / 2, 0, w / 2, h);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.finish();
    no_error(&mut c);
    drop(c);
    // In the memory, as a display reads it (0xXXRRGGBB): the window's row 0
    // is the first row, and nothing is written beyond the rows' pixels.
    for y in 0..h as usize {
        for x in 0..stride {
            let px = picture[y * stride + x];
            if x >= w as usize {
                assert_eq!(px, 0xDEAD_BEEF, "({x}, {y}) is past the row");
                continue;
            }
            let want = if x >= w as usize / 2 {
                0x00FF00
            } else if y == 0 {
                0xFF0000
            } else {
                0x0000FF
            };
            assert_eq!(px & 0xFF_FFFF, want, "({x}, {y}): {px:#010x}");
        }
    }
}
