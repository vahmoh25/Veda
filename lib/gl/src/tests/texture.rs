//! Texture tests.

use super::*;

/// A full-viewport quad with texture coordinates 0 to 1 (attributes `pos`
/// and `uv`), drawn with a program whose fragment shader is `fs`.
fn textured(c: &mut Context, fs: &str) -> u32 {
    let p = program(
        c,
        "#version 300 es
        in vec2 pos; out vec2 uv;
        void main() { gl_Position = vec4(pos, 0.0, 1.0); uv = pos * 0.5 + 0.5; }",
        fs,
    );
    c.use_program(p);
    buffer_f32(c, gl::ARRAY_BUFFER, &[-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 1.0]);
    let loc = c.get_attrib_location(p, "pos") as u32;
    c.vertex_attrib_pointer(loc, 2, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(loc);
    p
}

const FS_TEX: &str = "#version 300 es
precision highp float; uniform sampler2D t; in vec2 uv; out vec4 color;
void main() { color = texture(t, uv); }";

/// A 2D RGBA8 texture from pixels, bound to unit 0.
fn texture_rgba(c: &mut Context, w: i32, h: i32, pixels: &[u8], filter: u32) -> u32 {
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, w, h, 0, gl::RGBA, gl::UNSIGNED_BYTE, pixels);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, filter as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, filter as i32);
    t
}

#[test]
fn samples_nearest() {
    let mut c = context(4, 4);
    textured(&mut c, FS_TEX);
    let texels = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255];
    texture_rgba(&mut c, 2, 2, &texels, gl::NEAREST);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 4, 4);
    assert_eq!(px(&img, 4, 0, 0), [255, 0, 0, 255]);
    assert_eq!(px(&img, 4, 3, 1), [0, 255, 0, 255]);
    assert_eq!(px(&img, 4, 1, 3), [0, 0, 255, 255]);
    assert_eq!(px(&img, 4, 2, 2), [255, 255, 255, 255]);
}

#[test]
fn samples_linear_with_clamping() {
    let mut c = context(4, 1);
    textured(&mut c, FS_TEX);
    texture_rgba(&mut c, 2, 1, &[0, 0, 0, 255, 255, 255, 255, 255], gl::LINEAR);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 1);
    let r: Vec<u8> = (0..4).map(|x| px(&img, 4, x, 0)[0]).collect();
    assert_close(&r, &[0, 64, 191, 255]);
}

#[test]
fn wraps_repeat_and_mirror() {
    let mut c = context(4, 1);
    textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform sampler2D t; in vec2 uv; out vec4 color;
        void main() { color = texture(t, vec2(uv.x + 1.0, 0.5)); }",
    );
    texture_rgba(&mut c, 4, 1, &[10, 0, 0, 255, 20, 0, 0, 255, 30, 0, 0, 255, 40, 0, 0, 255], gl::NEAREST);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 1);
    assert_eq!((0..4).map(|x| px(&img, 4, x, 0)[0]).collect::<Vec<_>>(), [10, 20, 30, 40]);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::MIRRORED_REPEAT as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 4, 1);
    assert_eq!((0..4).map(|x| px(&img, 4, x, 0)[0]).collect::<Vec<_>>(), [40, 30, 20, 10]);
}

#[test]
fn selects_mipmap_levels() {
    let mut c = context(2, 2);
    textured(&mut c, FS_TEX);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let level = |n: usize, rgba: [u8; 4]| rgba.repeat(n * n);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, 4, 4, 0, gl::RGBA, gl::UNSIGNED_BYTE, &level(4, [255, 0, 0, 255])[..]);
    c.tex_image_2d(gl::TEXTURE_2D, 1, gl::RGBA8, 2, 2, 0, gl::RGBA, gl::UNSIGNED_BYTE, &level(2, [0, 255, 0, 255])[..]);
    c.tex_image_2d(gl::TEXTURE_2D, 2, gl::RGBA8, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, &level(1, [0, 0, 255, 255])[..]);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST_MIPMAP_NEAREST as i32);
    // Two pixels across the texture: two texels per pixel, level 1.
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    assert_eq!(px(&read_rgba(&mut c, 2, 2), 2, 0, 0), [0, 255, 0, 255]);
    // Base level 2 makes the 1x1 level the only one.
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_BASE_LEVEL, 2);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 2, 2), 2, 1, 1), [0, 0, 255, 255]);
    // A missing level makes the texture incomplete: (0, 0, 0, 1).
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_BASE_LEVEL, 0);
    c.tex_image_2d(gl::TEXTURE_2D, 1, gl::RGBA8, 3, 3, 0, gl::RGBA, gl::UNSIGNED_BYTE, &level(3, [9, 9, 9, 9])[..]);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 2, 2), 2, 0, 1), [0, 0, 0, 255]);
    // Respecifying the level as it should be completes it again, with the
    // other levels' contents intact.
    c.tex_image_2d(gl::TEXTURE_2D, 1, gl::RGBA8, 2, 2, 0, gl::RGBA, gl::UNSIGNED_BYTE, &level(2, [0, 255, 0, 255])[..]);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAX_LEVEL, 0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 2, 2), 2, 0, 1), [255, 0, 0, 255]);
    no_error(&mut c);
}

#[test]
fn explicit_lod_and_generated_mipmaps() {
    let mut c = context(1, 1);
    textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform sampler2D t; uniform float lod; in vec2 uv; out vec4 color;
        void main() { color = textureLod(t, vec2(0.5), lod); }",
    );
    let px4: Vec<u8> = [[0u8, 0, 0, 255], [100, 0, 0, 255], [200, 0, 0, 255], [40, 0, 0, 255]].concat();
    texture_rgba(&mut c, 2, 2, &px4, gl::NEAREST);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST_MIPMAP_NEAREST as i32);
    c.generate_mipmap(gl::TEXTURE_2D);
    no_error(&mut c);
    let p = c.get_integer(gl::CURRENT_PROGRAM) as u32;
    let lod = c.get_uniform_location(p, "lod");
    c.uniform1f(lod, 1.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    // The 1x1 level is the average: (0 + 100 + 200 + 40) / 4 = 85.
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [85, 0, 0, 255]);
    c.uniform1f(lod, 0.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0)[0], 40);
}

#[test]
fn generates_mipmaps_of_3d_textures() {
    let mut c = context(1, 1);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_3D, t);
    // 4x4x4: red grows with x, green with y, blue with z.
    let mut vox = Vec::new();
    for z in 0..4u8 {
        for y in 0..4u8 {
            for x in 0..4u8 {
                vox.extend_from_slice(&[8 * x, 8 * y, 32 * z, 255]);
            }
        }
    }
    c.tex_image_3d(gl::TEXTURE_3D, 0, gl::RGBA8, 4, 4, 4, 0, gl::RGBA, gl::UNSIGNED_BYTE, &vox[..]);
    c.generate_mipmap(gl::TEXTURE_3D);
    no_error(&mut c);
    let fb = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, fb);
    // Level 1 is 2x2x2, each texel the average of a 2x2x2 block, slices
    // included: (16x + 4, 16y + 4, 64z + 16).
    for z in 0..2u8 {
        c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 1, i32::from(z));
        let want: Vec<u8> =
            (0..2u8).flat_map(|y| (0..2u8).flat_map(move |x| [16 * x + 4, 16 * y + 4, 64 * z + 16, 255])).collect();
        assert_close(&read_rgba(&mut c, 2, 2), &want);
    }
    // Level 2, 1x1x1: the average of them all.
    c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 2, 0);
    assert_close(&read_rgba(&mut c, 1, 1), &[12, 12, 48, 255]);
    no_error(&mut c);
}

#[test]
fn fetches_texels() {
    let mut c = context(1, 1);
    textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform sampler2D t; out vec4 color;
        void main() { color = texelFetch(t, ivec2(2, 1), 0) + texelFetchOffset(t, ivec2(0, 0), 0, ivec2(1, 1)); }",
    );
    let mut texels = vec![0u8; 3 * 2 * 4];
    texels[(3 + 2) * 4] = 100; // (2, 1) red
    texels[(3 + 1) * 4 + 1] = 50; // (1, 1) green
    texture_rgba(&mut c, 3, 2, &texels, gl::NEAREST);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [100, 50, 0, 0]);
}

#[test]
fn samples_cube_maps() {
    let mut c = context(6, 1);
    textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform samplerCube t; in vec2 uv; out vec4 color;
        void main() {
            int i = int(uv.x * 6.0);
            vec3 d = i == 0 ? vec3(1, 0.1, 0.2) : i == 1 ? vec3(-1, 0.3, -0.1) : i == 2 ? vec3(0.2, 1, 0.1)
                   : i == 3 ? vec3(0.1, -1, 0.3) : i == 4 ? vec3(0.3, 0.2, 1) : vec3(-0.2, 0.1, -1);
            color = texture(t, d);
        }",
    );
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_CUBE_MAP, t);
    for f in 0..6u32 {
        let texel = [(f * 40) as u8, 0, 0, 255].repeat(4);
        c.tex_image_2d(
            gl::TEXTURE_CUBE_MAP_POSITIVE_X + f,
            0,
            gl::RGBA8,
            2,
            2,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            &texel[..],
        );
    }
    c.tex_parameteri(gl::TEXTURE_CUBE_MAP, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 6, 1);
    assert_eq!((0..6).map(|x| px(&img, 6, x, 0)[0]).collect::<Vec<_>>(), [0, 40, 80, 120, 160, 200]);
}

#[test]
fn samples_3d_and_array_textures() {
    let mut c = context(2, 1);
    textured(&mut c, "#version 300 es
        precision highp float; uniform highp sampler3D v; uniform highp sampler2DArray a; in vec2 uv; out vec4 color;
        void main() { color = vec4(texture(v, vec3(0.25, 0.25, uv.x)).r, texture(a, vec3(0.5, 0.5, 2.0 * uv.x + 0.6)).g, 0, 1); }");
    let p = c.get_integer(gl::CURRENT_PROGRAM) as u32;
    let (lv, la) = (c.get_uniform_location(p, "v"), c.get_uniform_location(p, "a"));
    c.uniform1i(lv, 0);
    c.uniform1i(la, 1);
    let t3 = c.gen_texture();
    c.bind_texture(gl::TEXTURE_3D, t3);
    // 2x2x2: slice 0 red 10, slice 1 red 20.
    let mut vox = [0u8; 2 * 2 * 2 * 4];
    for (i, v) in vox.chunks_mut(4).enumerate() {
        v[0] = if i < 4 { 10 } else { 20 };
    }
    c.tex_image_3d(gl::TEXTURE_3D, 0, gl::RGBA8, 2, 2, 2, 0, gl::RGBA, gl::UNSIGNED_BYTE, &vox[..]);
    c.tex_parameteri(gl::TEXTURE_3D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_3D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    c.active_texture(gl::TEXTURE1);
    let ta = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D_ARRAY, ta);
    c.tex_storage_3d(gl::TEXTURE_2D_ARRAY, 1, gl::RGBA8, 1, 1, 3);
    for layer in 0..3 {
        c.tex_sub_image_3d(
            gl::TEXTURE_2D_ARRAY,
            0,
            0,
            0,
            layer,
            1,
            1,
            1,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            &[0u8, 30 * layer as u8 + 1, 0, 255][..],
        );
    }
    c.tex_parameteri(gl::TEXTURE_2D_ARRAY, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 2, 1);
    // Pixel 0: slice 0, layer round(0.5 + 0.6) = 1; pixel 1: slice 1,
    // layer round(1.5 + 0.6) = 2.
    assert_eq!(px(&img, 2, 0, 0), [10, 31, 0, 255]);
    assert_eq!(px(&img, 2, 1, 0), [20, 61, 0, 255]);
}

#[test]
fn compares_depth_textures() {
    let mut c = context(4, 1);
    textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform highp sampler2DShadow t; in vec2 uv; out vec4 color;
        void main() { color = vec4(texture(t, vec3(uv.x + 0.125, 0.5, 0.5))); }",
    );
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let depths: [f32; 4] = [0.25, 0.75, 0.25, 0.75];
    let bytes: Vec<u8> = depths.iter().flat_map(|d| d.to_le_bytes()).collect();
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::DEPTH_COMPONENT32F, 4, 1, 0, gl::DEPTH_COMPONENT, gl::FLOAT, &bytes[..]);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_COMPARE_MODE, gl::COMPARE_REF_TO_TEXTURE as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_COMPARE_FUNC, gl::LESS as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 4, 1);
    // Pixel x samples texel x + 1 (repeating): 0.5 < 0.25 fails, 0.5 < 0.75
    // passes.
    assert_eq!((0..4).map(|x| px(&img, 4, x, 0)[0]).collect::<Vec<_>>(), [255, 0, 255, 0]);
    if on_softpipe() {
        return;
    }
    // Linear filtering averages the comparisons: pixel 1 is halfway
    // between texels 1 (pass) and 2 (fail).
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let v = px(&read_rgba(&mut c, 4, 1), 4, 1, 0)[0];
    assert!((100..=155).contains(&v), "{v}");
}

#[test]
fn samples_depth_stencil_textures() {
    // Depth in the upper 24 bits of each texel, stencil in the lower 8, as
    // `UNSIGNED_INT_24_8` packs them; renderers that keep depth in the
    // lower bits (Veda's on iris) turn the texels round on the way in.
    let mut c = context(4, 1);
    textured(&mut c, FS_TEX);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let depths: [u32; 4] = [0x40_0000, 0xC0_0000, 0x80_0000, 0xFF_FFFF];
    let texels: Vec<u8> =
        depths.iter().zip([7u32, 0x55, 0xAA, 0xFF]).flat_map(|(d, s)| (d << 8 | s).to_le_bytes()).collect();
    c.tex_image_2d(
        gl::TEXTURE_2D,
        0,
        gl::DEPTH24_STENCIL8,
        4,
        1,
        0,
        gl::DEPTH_STENCIL,
        gl::UNSIGNED_INT_24_8,
        &texels[..],
    );
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 4, 1);
    // The depths, a quarter, three quarters, a half and one, as red.
    assert_eq!((0..4).map(|x| px(&img, 4, x, 0)[0]).collect::<Vec<_>>(), [64, 191, 128, 255]);
}

#[test]
fn swizzles_and_integer_and_srgb_textures() {
    let mut c = context(1, 1);
    textured(&mut c, FS_TEX);
    texture_rgba(&mut c, 1, 1, &[10, 20, 30, 40], gl::NEAREST);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_SWIZZLE_R, gl::BLUE as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_SWIZZLE_G, gl::ONE as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_SWIZZLE_A, gl::ZERO as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [30, 255, 30, 0]);
    // sRGB texels are decoded: 128 is 0.2158605 linear.
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_image_2d(
        gl::TEXTURE_2D,
        0,
        gl::SRGB8_ALPHA8,
        1,
        1,
        0,
        gl::RGBA,
        gl::UNSIGNED_BYTE,
        &[128u8, 255, 0, 255][..],
    );
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [55, 255, 0, 255]);
    // An integer texture through a usampler.
    let mut c = context(1, 1);
    textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform highp usampler2D t; out vec4 color;
        void main() { uvec4 v = texture(t, vec2(0.5)); color = vec4(v) / 1000.0; }",
    );
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let v: Vec<u8> = [500u16, 1000, 250, 0].iter().flat_map(|x| x.to_le_bytes()).collect();
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA16UI, 1, 1, 0, gl::RGBA_INTEGER, gl::UNSIGNED_SHORT, &v[..]);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [128, 255, 64, 0]);
}

#[test]
fn compressed_textures_decode() {
    let mut c = context(4, 4);
    textured(&mut c, FS_TEX);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    // An ETC2 block in individual mode: both subblocks base (238, 51,
    // 136), table 0, every texel +2.
    let block = ((14u64 << 60) | (14 << 56) | (3 << 52) | (3 << 48) | (8 << 44) | (8 << 40)).to_be_bytes();
    c.compressed_tex_image_2d(gl::TEXTURE_2D, 0, gl::COMPRESSED_RGB8_ETC2, 4, 4, 0, 8, &block[..]);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    no_error(&mut c);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 4, 4), 4, 2, 1), [240, 53, 138, 255]);
    // The wrong size is refused.
    c.compressed_tex_image_2d(gl::TEXTURE_2D, 0, gl::COMPRESSED_RGB8_ETC2, 4, 4, 0, 7, &block[..7]);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
}

#[test]
fn texture_updates_between_recorded_draws() {
    // Draw, change the texture, draw elsewhere: the first draw keeps the
    // old contents although neither has been rendered yet.
    let mut c = context(2, 1);
    let p = textured(
        &mut c,
        "#version 300 es
        precision highp float; uniform sampler2D t; uniform float u; in vec2 uv; out vec4 color;
        void main() { color = texture(t, vec2(0.5)) + vec4(0, u, 0, 0); }",
    );
    texture_rgba(&mut c, 1, 1, &[100, 0, 0, 255], gl::NEAREST);
    let u = c.get_uniform_location(p, "u");
    c.enable(gl::SCISSOR_TEST);
    c.scissor(0, 0, 1, 1);
    c.uniform1f(u, 0.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.tex_sub_image_2d(gl::TEXTURE_2D, 0, 0, 0, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, &[200u8, 0, 0, 255][..]);
    c.uniform1f(u, 1.0);
    c.scissor(1, 0, 1, 1);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 2, 1);
    assert_eq!(px(&img, 2, 0, 0), [100, 0, 0, 255]);
    assert_eq!(px(&img, 2, 1, 0), [200, 255, 0, 255]);
}

#[test]
fn unpack_parameters_and_buffers() {
    let mut c = context(2, 2);
    textured(&mut c, FS_TEX);
    // A 2x2 image inside a 3-pixel-wide RGB array, skipping one row and
    // one pixel, rows aligned to 4.
    let mut src = vec![0u8; 4 * 12];
    let put = |s: &mut Vec<u8>, x: usize, y: usize, v: u8| s[y * 12 + x * 3] = v;
    put(&mut src, 1, 1, 11);
    put(&mut src, 2, 1, 22);
    put(&mut src, 1, 2, 33);
    put(&mut src, 2, 2, 44);
    c.pixel_storei(gl::UNPACK_ROW_LENGTH, 3);
    c.pixel_storei(gl::UNPACK_SKIP_ROWS, 1);
    c.pixel_storei(gl::UNPACK_SKIP_PIXELS, 1);
    // Through a pixel unpack buffer.
    let b = c.gen_buffer();
    c.bind_buffer(gl::PIXEL_UNPACK_BUFFER, b);
    c.buffer_data(gl::PIXEL_UNPACK_BUFFER, &src, gl::STATIC_DRAW);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGB8, 2, 2, 0, gl::RGB, gl::UNSIGNED_BYTE, Pixels::Offset(0));
    no_error(&mut c);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 2, 2);
    assert_eq!([0, 1, 2, 3].map(|i| img[i * 4]), [11, 22, 33, 44]);
    // A source that is too short is refused.
    c.bind_buffer(gl::PIXEL_UNPACK_BUFFER, 0);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGB8, 2, 2, 0, gl::RGB, gl::UNSIGNED_BYTE, &src[..20]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

/// The color of `expr` (over a sampler `t` of type `sampler`) in a 1x1
/// framebuffer.
fn shade1(c: &mut Context, sampler: &str, expr: &str) -> [u8; 4] {
    textured(
        c,
        &std::format!(
            "#version 300 es
            precision highp float; uniform highp {sampler} t; in vec2 uv; out vec4 color;
            void main() {{ color = {expr}; }}"
        ),
    );
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(c);
    px(&read_rgba(c, 1, 1), 1, 0, 0)
}

#[test]
fn rebuilt_storage_keeps_every_image() {
    // Each texture's level 0 is defined while its filter needs no mipmaps,
    // so its storage has one level; level 1 then needs new storage, which
    // the next draw fills by copying level 0 (on the GPU, by copies the
    // host cannot always make itself: 3D slices, formats it cannot render
    // to, depth, every layer of an array).
    let mut c = context(1, 1);
    let mipmapped = |c: &mut Context, target: u32| {
        c.tex_parameteri(target, gl::TEXTURE_MIN_FILTER, gl::NEAREST_MIPMAP_NEAREST as i32);
        c.tex_parameteri(target, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
    };
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_3D, t);
    c.tex_parameteri(gl::TEXTURE_3D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    let vox: Vec<u8> = (0..8u8).flat_map(|i| [10 * i, 0, 0, 255]).collect();
    c.tex_image_3d(gl::TEXTURE_3D, 0, gl::RGBA8, 2, 2, 2, 0, gl::RGBA, gl::UNSIGNED_BYTE, &vox[..]);
    c.tex_image_3d(gl::TEXTURE_3D, 1, gl::RGBA8, 1, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, &[200u8, 0, 0, 255][..]);
    mipmapped(&mut c, gl::TEXTURE_3D);
    assert_eq!(shade1(&mut c, "sampler3D", "texelFetch(t, ivec3(1, 0, 1), 0)"), [50, 0, 0, 255]);
    assert_eq!(shade1(&mut c, "sampler3D", "texelFetch(t, ivec3(0), 1)"), [200, 0, 0, 255]);

    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    // (64, -64, 0, 127) / 127, then zero.
    let snorm = [64u8, 192, 0, 127, 0, 0, 0, 0];
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8_SNORM, 2, 1, 0, gl::RGBA, gl::BYTE, &snorm[..]);
    c.tex_image_2d(gl::TEXTURE_2D, 1, gl::RGBA8_SNORM, 1, 1, 0, gl::RGBA, gl::BYTE, &[127u8, 129, 0, 127][..]);
    mipmapped(&mut c, gl::TEXTURE_2D);
    assert_close(&shade1(&mut c, "sampler2D", "texelFetch(t, ivec2(0), 0) * 0.5 + 0.5"), &[192, 63, 128, 255]);
    assert_close(&shade1(&mut c, "sampler2D", "texelFetch(t, ivec2(0), 1) * 0.5 + 0.5"), &[255, 0, 128, 255]);

    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    // Depths 0x4000 and 0xC000 (of 0xFFFF), then 0x8000.
    let depths = [0x00u8, 0x40, 0x00, 0xC0];
    c.tex_image_2d(
        gl::TEXTURE_2D,
        0,
        gl::DEPTH_COMPONENT16,
        2,
        1,
        0,
        gl::DEPTH_COMPONENT,
        gl::UNSIGNED_SHORT,
        &depths[..],
    );
    let half = [0x00u8, 0x80];
    c.tex_image_2d(
        gl::TEXTURE_2D,
        1,
        gl::DEPTH_COMPONENT16,
        1,
        1,
        0,
        gl::DEPTH_COMPONENT,
        gl::UNSIGNED_SHORT,
        &half[..],
    );
    mipmapped(&mut c, gl::TEXTURE_2D);
    let fetch =
        "vec4(texelFetch(t, ivec2(1, 0), 0).r, texelFetch(t, ivec2(0), 0).r, texelFetch(t, ivec2(0), 1).r, 1.0)";
    assert_close(&shade1(&mut c, "sampler2D", fetch), &[191, 64, 128, 255]);

    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D_ARRAY, t);
    c.tex_parameteri(gl::TEXTURE_2D_ARRAY, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
    let texels: Vec<u8> = (0..6u8).flat_map(|i| [0, 40 * i, 0, 255]).collect();
    c.tex_image_3d(gl::TEXTURE_2D_ARRAY, 0, gl::RGBA8, 2, 1, 3, 0, gl::RGBA, gl::UNSIGNED_BYTE, &texels[..]);
    let level1 = [[0u8, 0, 7, 255]; 3].concat();
    c.tex_image_3d(gl::TEXTURE_2D_ARRAY, 1, gl::RGBA8, 1, 1, 3, 0, gl::RGBA, gl::UNSIGNED_BYTE, &level1[..]);
    mipmapped(&mut c, gl::TEXTURE_2D_ARRAY);
    assert_eq!(shade1(&mut c, "sampler2DArray", "texelFetch(t, ivec3(1, 0, 2), 0)"), [0, 200, 0, 255]);
    assert_eq!(shade1(&mut c, "sampler2DArray", "texelFetch(t, ivec3(0, 0, 1), 1)"), [0, 0, 7, 255]);
}
