//! Tests of the API's checks and object model.

use super::*;

const VS: &str = "#version 300 es
in vec4 pos; void main() { gl_Position = pos; }";
const FS: &str = "#version 300 es
precision mediump float; out vec4 color; void main() { color = vec4(1); }";

#[test]
fn errors_are_recorded_once() {
    let mut c = context(4, 4);
    c.enable(0x1234);
    c.viewport(0, 0, -1, 4);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    assert_eq!(c.get_error(), gl::NO_ERROR);
}

#[test]
fn buffer_names_and_lifetimes() {
    let mut c = context(4, 4);
    let b = c.gen_buffer();
    // A generated name names no buffer until it is bound.
    assert!(!c.is_buffer(b));
    c.bind_buffer(gl::ARRAY_BUFFER, b);
    assert!(c.is_buffer(b));
    // Binding an unused name creates a buffer too.
    c.bind_buffer(gl::COPY_READ_BUFFER, 4242);
    assert!(c.is_buffer(4242));
    c.buffer_data(gl::ARRAY_BUFFER, &[1, 2, 3, 4], gl::STATIC_DRAW);
    assert_eq!(c.get_buffer_parameteri(gl::ARRAY_BUFFER, gl::BUFFER_SIZE), 4);
    // A vertex array keeps a deleted buffer alive.
    let vao = c.gen_vertex_array();
    c.bind_vertex_array(vao);
    c.vertex_attrib_pointer(0, 1, gl::UNSIGNED_BYTE, false, 0, 0);
    c.bind_vertex_array(0);
    c.delete_buffers(&[b]);
    assert!(!c.is_buffer(b));
    assert_eq!(c.get_integer(gl::ARRAY_BUFFER_BINDING), 0);
    c.bind_vertex_array(vao);
    let mut v = [7];
    c.get_vertex_attribiv(0, gl::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING, &mut v);
    // The name is gone, the attachment stays.
    assert_eq!(v[0], 0);
    let p = program(
        &mut c,
        "#version 300 es
        in float x; void main() { gl_Position = vec4(x / 4.0 * 2.0 - 1.0, 0, 0, 1); gl_PointSize = 1.0; }",
        FS,
    );
    c.use_program(p);
    c.enable_vertex_attrib_array(0);
    c.bind_attrib_location(p, 0, "x");
    c.link_program(p);
    c.draw_arrays(gl::POINTS, 0, 4);
    no_error(&mut c);
    // Deleting a buffer bound to the current vertex array detaches it.
    let b2 = c.gen_buffer();
    c.bind_buffer(gl::ELEMENT_ARRAY_BUFFER, b2);
    c.delete_buffers(&[b2]);
    assert_eq!(c.get_integer(gl::ELEMENT_ARRAY_BUFFER_BINDING), 0);
    // Out of the binding's reach: errors.
    c.bind_buffer(gl::ARRAY_BUFFER, 0);
    c.buffer_data(gl::ARRAY_BUFFER, &[1], gl::STATIC_DRAW);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.buffer_data(0x9999, &[1], gl::STATIC_DRAW);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    c.bind_buffer(gl::ARRAY_BUFFER, 4242);
    c.buffer_data(gl::ARRAY_BUFFER, &[1], 0x1);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
}

#[test]
fn mapping_buffers() {
    let mut c = context(1, 1);
    let b = c.gen_buffer();
    c.bind_buffer(gl::ARRAY_BUFFER, b);
    c.buffer_data(gl::ARRAY_BUFFER, &[0, 1, 2, 3, 4, 5, 6, 7], gl::DYNAMIC_DRAW);
    let m = c.map_buffer_range(gl::ARRAY_BUFFER, 2, 4, gl::MAP_READ_BIT | gl::MAP_WRITE_BIT).unwrap();
    assert_eq!(m, &[2, 3, 4, 5]);
    m[0] = 20;
    // Mapped buffers cannot be changed otherwise or mapped again.
    c.buffer_sub_data(gl::ARRAY_BUFFER, 0, &[9]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    assert!(c.map_buffer_range(gl::ARRAY_BUFFER, 0, 1, gl::MAP_READ_BIT).is_none());
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    assert_eq!(c.get_buffer_parameteri(gl::ARRAY_BUFFER, gl::BUFFER_MAPPED), 1);
    assert!(c.unmap_buffer(gl::ARRAY_BUFFER));
    assert!(!c.unmap_buffer(gl::ARRAY_BUFFER));
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Explicit flushing writes only the flushed ranges.
    let m = c
        .map_buffer_range(
            gl::ARRAY_BUFFER,
            0,
            8,
            gl::MAP_WRITE_BIT | gl::MAP_FLUSH_EXPLICIT_BIT | gl::MAP_INVALIDATE_RANGE_BIT,
        )
        .unwrap();
    m.fill(100);
    c.flush_mapped_buffer_range(gl::ARRAY_BUFFER, 6, 2);
    c.unmap_buffer(gl::ARRAY_BUFFER);
    let m = c.map_buffer_range(gl::ARRAY_BUFFER, 0, 8, gl::MAP_READ_BIT).unwrap().to_vec();
    assert_eq!(m, [0, 1, 20, 3, 4, 5, 100, 100]);
    c.unmap_buffer(gl::ARRAY_BUFFER);
    // Bad maps.
    for (o, l, a, e) in [
        (0, 0, gl::MAP_READ_BIT, gl::INVALID_OPERATION),
        (4, 8, gl::MAP_READ_BIT, gl::INVALID_VALUE),
        (0, 4, gl::MAP_READ_BIT | gl::MAP_INVALIDATE_BUFFER_BIT, gl::INVALID_OPERATION),
        (0, 4, gl::MAP_READ_BIT | gl::MAP_FLUSH_EXPLICIT_BIT, gl::INVALID_OPERATION),
        (0, 4, 0, gl::INVALID_OPERATION),
        (0, 4, 0x100, gl::INVALID_VALUE),
    ] {
        assert!(c.map_buffer_range(gl::ARRAY_BUFFER, o, l, a).is_none());
        assert_eq!(c.get_error(), e, "map {o} {l} {a:#x}");
    }
    // Copies: overlapping ranges of one buffer are refused.
    c.bind_buffer(gl::COPY_WRITE_BUFFER, b);
    c.copy_buffer_sub_data(gl::ARRAY_BUFFER, gl::COPY_WRITE_BUFFER, 0, 2, 4);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.copy_buffer_sub_data(gl::ARRAY_BUFFER, gl::COPY_WRITE_BUFFER, 0, 4, 4);
    no_error(&mut c);
}

#[test]
fn vertex_array_checks() {
    let mut c = context(1, 1);
    // Binding a name not generated is refused.
    c.bind_vertex_array(99);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Offsets need a buffer (there are no client arrays).
    c.vertex_attrib_pointer(0, 4, gl::FLOAT, false, 0, 16);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    let b = c.gen_buffer();
    c.bind_buffer(gl::ARRAY_BUFFER, b);
    c.vertex_attrib_pointer(0, 3, gl::INT_2_10_10_10_REV, true, 0, 0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.vertex_attrib_i_pointer(0, 4, gl::FLOAT, 0, 0);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    c.vertex_attrib_pointer(16, 4, gl::FLOAT, false, 0, 0);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.vertex_attrib_pointer(0, 5, gl::FLOAT, false, 0, 0);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    // Drawing from an enabled array without a buffer is refused.
    let p = program(&mut c, VS, FS);
    c.use_program(p);
    c.bind_buffer(gl::ARRAY_BUFFER, 0);
    c.vertex_attrib_pointer(0, 4, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(0);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Draw arguments.
    c.disable_vertex_attrib_array(0);
    c.draw_arrays(0x77, 0, 3);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    c.draw_arrays(gl::TRIANGLES, 0, -1);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.draw_elements(gl::TRIANGLES, 3, gl::UNSIGNED_SHORT, 0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION, "no element buffer");
    c.draw_elements(gl::TRIANGLES, 3, gl::FLOAT, 0);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    c.draw_range_elements(gl::TRIANGLES, 5, 4, 3, gl::UNSIGNED_SHORT, 0);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    // Without a program, draws do nothing and are not errors.
    c.use_program(0);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    no_error(&mut c);
    // A mapped buffer cannot be drawn from.
    c.use_program(p);
    c.bind_buffer(gl::ARRAY_BUFFER, b);
    c.buffer_data_size(gl::ARRAY_BUFFER, 48, gl::STATIC_DRAW);
    c.vertex_attrib_pointer(0, 4, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(0);
    c.map_buffer_range(gl::ARRAY_BUFFER, 0, 4, gl::MAP_READ_BIT);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

#[test]
fn shader_and_program_lifetimes() {
    let mut c = context(1, 1);
    let v = c.create_shader(gl::VERTEX_SHADER);
    let p = c.create_program();
    // Shaders and programs share a namespace: the wrong kind is an
    // operation error, an unknown name a value error.
    c.compile_shader(p);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.link_program(v);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.compile_shader(9999);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    // A deleted shader lives while attached.
    c.attach_shader(p, v);
    c.attach_shader(p, v);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.delete_shader(v);
    assert!(c.is_shader(v));
    assert_eq!(c.get_shaderiv(v, gl::DELETE_STATUS), 1);
    c.detach_shader(p, v);
    assert!(!c.is_shader(v));
    // A failed compile has a log.
    let f = c.create_shader(gl::FRAGMENT_SHADER);
    c.shader_source(f, &["#version 300 es\nvoid main() { undeclared = 1; }"]);
    c.compile_shader(f);
    assert_eq!(c.get_shaderiv(f, gl::COMPILE_STATUS), 0);
    assert!(c.get_shader_info_log(f).contains("undeclared"));
    assert_eq!(c.get_shaderiv(f, gl::INFO_LOG_LENGTH), c.get_shader_info_log(f).len() as i32 + 1);
    // Linking without both shaders fails with a log.
    c.link_program(p);
    assert_eq!(c.get_programiv(p, gl::LINK_STATUS), 0);
    assert!(!c.get_program_info_log(p).is_empty());
    c.use_program(p);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // A deleted current program lives on until it is not current.
    let q = program(&mut c, VS, FS);
    c.use_program(q);
    c.delete_program(q);
    assert!(c.is_program(q));
    assert_eq!(c.get_programiv(q, gl::DELETE_STATUS), 1);
    c.use_program(0);
    assert!(!c.is_program(q));
    no_error(&mut c);
}

#[test]
fn failed_relink_keeps_the_executable() {
    let mut c = context(1, 1);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0, 1); }",
        "#version 300 es
        precision mediump float; uniform vec4 u; out vec4 color; void main() { color = u; }",
    );
    c.use_program(p);
    let u = c.get_uniform_location(p, "u");
    c.uniform4f(u, 0.0, 1.0, 0.0, 1.0);
    // Replace the fragment shader with one that does not compile; relink.
    for s in c.get_attached_shaders(p) {
        if c.get_shaderiv(s, gl::SHADER_TYPE) == gl::FRAGMENT_SHADER as i32 {
            c.detach_shader(p, s);
        }
    }
    let bad = c.create_shader(gl::FRAGMENT_SHADER);
    c.shader_source(bad, &["#version 300 es\nnope"]);
    c.compile_shader(bad);
    c.attach_shader(p, bad);
    c.link_program(p);
    assert_eq!(c.get_programiv(p, gl::LINK_STATUS), 0);
    // The current executable still draws, with its uniforms.
    buffer_f32(&mut c, gl::ARRAY_BUFFER, &[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0]);
    c.vertex_attrib_pointer(0, 2, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(0);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    no_error(&mut c);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [0, 255, 0, 255]);
    // But queries of the program see the failed link.
    assert_eq!(c.get_uniform_location(p, "u"), -1);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

#[test]
fn uniform_checks_and_values() {
    let mut c = context(1, 1);
    let p = program(
        &mut c,
        VS,
        "#version 300 es
        precision highp float;
        uniform float f; uniform ivec2 i2; uniform uvec3 u3; uniform bvec2 b2;
        uniform mat2x3 m; uniform vec2 arr[4]; uniform sampler2D s;
        uniform struct S { float a; vec2 b[2]; } st;
        out vec4 color;
        void main() {
            color = vec4(f + float(i2.x) + float(u3.y) + float(b2.y) + m[1][2] + arr[3].x + st.a + st.b[1].y)
                  + texture(s, vec2(0.5));
        }",
    );
    c.use_program(p);
    let loc = |c: &mut Context, n: &str| c.get_uniform_location(p, n);
    let (f, i2, u3, b2, m) =
        (loc(&mut c, "f"), loc(&mut c, "i2"), loc(&mut c, "u3"), loc(&mut c, "b2"), loc(&mut c, "m"));
    // Wrong kinds and sizes.
    c.uniform1i(f, 1);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.uniform3f(f, 1.0, 2.0, 3.0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.uniformfv(f, 1, &[1.0, 2.0]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION, "count 2 for a non-array");
    c.uniform2ui(i2, 1, 2);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.uniform_matrix3fv(m, false, &[0.0; 9]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Location -1 is ignored; others than the program's are errors.
    c.uniform1f(-1, 5.0);
    no_error(&mut c);
    c.uniform1f(1000, 5.0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Samplers take units in range, through Uniform1i only.
    let s = loc(&mut c, "s");
    c.uniform1i(s, 32);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.uniform1f(s, 1.0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.uniform1i(s, 3);
    no_error(&mut c);
    // Values round-trip; booleans from any kind.
    c.uniform1f(f, 2.5);
    c.uniform2i(i2, -3, 4);
    c.uniform3ui(u3, 1, 4_000_000_000, 3);
    c.uniform2f(b2, 0.0, 7.0);
    let mut out = [0.0f32; 6];
    c.get_uniformfv(p, f, &mut out);
    assert_eq!(out[0], 2.5);
    let mut oi = [0i32; 2];
    c.get_uniformiv(p, i2, &mut oi);
    assert_eq!(oi, [-3, 4]);
    let mut ou = [0u32; 3];
    c.get_uniformuiv(p, u3, &mut ou);
    assert_eq!(ou, [1, 4_000_000_000, 3]);
    c.get_uniformiv(p, b2, &mut oi);
    assert_eq!(oi, [0, 1]);
    // Matrices, column by column or transposed.
    c.uniform_matrixfv(m, 2, 3, false, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    c.get_uniformfv(p, m, &mut out);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    c.uniform_matrixfv(m, 2, 3, true, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    c.get_uniformfv(p, m, &mut out);
    assert_eq!(out, [1.0, 3.0, 5.0, 2.0, 4.0, 6.0]);
    // Arrays: from an element on, extra values ignored.
    let a2 = loc(&mut c, "arr[2]");
    assert_ne!(a2, -1);
    c.uniformfv(a2, 2, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    no_error(&mut c);
    let a3 = loc(&mut c, "arr[3]");
    c.get_uniformfv(p, a3, &mut out[..2]);
    assert_eq!(&out[..2], &[3.0, 4.0]);
    assert_eq!(loc(&mut c, "arr[4]"), -1);
    assert_eq!(loc(&mut c, "arr"), loc(&mut c, "arr[0]"));
    // Structure members by their full names.
    assert_ne!(loc(&mut c, "st.a"), -1);
    assert_ne!(loc(&mut c, "st.b[1]"), -1);
    // Introspection.
    let n = c.get_programiv(p, gl::ACTIVE_UNIFORMS);
    let mut names = Vec::new();
    for i in 0..n as u32 {
        let (size, ty, name) = c.get_active_uniform(p, i).unwrap();
        names.push((name, size, ty));
    }
    assert!(names.contains(&("arr[0]".into(), 4, gl::FLOAT_VEC2)));
    assert!(names.contains(&("m".into(), 1, gl::FLOAT_MAT2x3)));
    assert!(names.contains(&("st.b[0]".into(), 2, gl::FLOAT_VEC2)));
    assert!(names.contains(&("s".into(), 1, gl::SAMPLER_2D)));
    // No program: an operation error.
    c.use_program(0);
    c.uniform1f(f, 1.0);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

#[test]
fn texture_image_checks() {
    let mut c = context(1, 1);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let px = [0u8; 64];
    // Target, level, internal format, width, height, border, format, type,
    // and the error.
    type Case = (u32, i32, u32, i32, i32, i32, u32, u32, u32);
    let cases: &[Case] = &[
        (gl::TEXTURE_3D, 0, gl::RGBA8, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_ENUM),
        (gl::TEXTURE_2D, -1, gl::RGBA8, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_VALUE),
        (gl::TEXTURE_2D, 0, gl::RGBA8, -1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_VALUE),
        (gl::TEXTURE_2D, 0, gl::RGBA8, 1, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_VALUE),
        (gl::TEXTURE_2D, 0, gl::RGBA8, 1, 1, 0, gl::RGBA, gl::FLOAT, gl::INVALID_OPERATION),
        (gl::TEXTURE_2D, 0, gl::RGBA8, 1, 1, 0, 0x1, gl::UNSIGNED_BYTE, gl::INVALID_ENUM),
        (gl::TEXTURE_2D, 0, 0x1, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_VALUE),
        (gl::TEXTURE_2D, 20, gl::RGBA8, 1, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_VALUE),
        (gl::TEXTURE_CUBE_MAP_POSITIVE_Y, 0, gl::RGBA8, 2, 1, 0, gl::RGBA, gl::UNSIGNED_BYTE, gl::INVALID_VALUE),
    ];
    for &(target, level, ifmt, w, h, border, fmt, ty, err) in cases {
        if target == gl::TEXTURE_CUBE_MAP_POSITIVE_Y {
            let cube = c.gen_texture();
            c.bind_texture(gl::TEXTURE_CUBE_MAP, cube);
        }
        c.tex_image_2d(target, level, ifmt, w, h, border, fmt, ty, &px[..]);
        assert_eq!(c.get_error(), err, "{target:#x} {level} {ifmt:#x} {w}x{h} {fmt:#x} {ty:#x}");
    }
    // A texture bound to one target cannot be bound to another.
    c.bind_texture(gl::TEXTURE_3D, t);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Immutable textures refuse new images and new storage.
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_storage_2d(gl::TEXTURE_2D, 2, gl::RGBA8, 4, 4);
    no_error(&mut c);
    assert_eq!(c.get_tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_IMMUTABLE_FORMAT), 1);
    assert_eq!(c.get_tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_IMMUTABLE_LEVELS), 2);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, 4, 4, 0, gl::RGBA, gl::UNSIGNED_BYTE, &px[..]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.tex_storage_2d(gl::TEXTURE_2D, 1, gl::RGBA8, 4, 4);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Too many levels for the size.
    let t2 = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t2);
    c.tex_storage_2d(gl::TEXTURE_2D, 4, gl::RGBA8, 4, 4);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    // Sub-images: inside the image, in a matching format.
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_sub_image_2d(gl::TEXTURE_2D, 1, 1, 1, 2, 2, gl::RGBA, gl::UNSIGNED_BYTE, &px[..]);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.tex_sub_image_2d(gl::TEXTURE_2D, 0, 0, 0, 2, 2, gl::RGBA, gl::FLOAT, &px[..]);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.tex_sub_image_2d(gl::TEXTURE_2D, 0, 2, 2, 2, 2, gl::RGBA, gl::UNSIGNED_BYTE, &px[..]);
    no_error(&mut c);
    // Parameters.
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR_MIPMAP_LINEAR as i32);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_BASE_LEVEL, -1);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.tex_parameterf(gl::TEXTURE_2D, gl::TEXTURE_MIN_LOD, -2.5);
    assert_eq!(c.get_tex_parameterf(gl::TEXTURE_2D, gl::TEXTURE_MIN_LOD), -2.5);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_SWIZZLE_G, gl::ALPHA as i32);
    assert_eq!(c.get_tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_SWIZZLE_G), gl::ALPHA as i32);
    no_error(&mut c);
}

#[test]
fn framebuffer_completeness() {
    let mut c = context(4, 4);
    let f = c.gen_framebuffer();
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT);
    c.clear(gl::COLOR_BUFFER_BIT);
    assert_eq!(c.get_error(), gl::INVALID_FRAMEBUFFER_OPERATION);
    // A format that is not color-renderable.
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    c.tex_storage_2d(gl::TEXTURE_2D, 1, gl::RGB9_E5, 4, 4);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, t, 0);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT);
    // A missing image.
    let t2 = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t2);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, t2, 0);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, 4, 4, 0, gl::RGBA, gl::UNSIGNED_BYTE, Pixels::None);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    // Mixed sample counts, where the renderer has multisampling.
    let rb = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, rb);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::DEPTH_ATTACHMENT, gl::RENDERBUFFER, rb);
    if multisampling(&mut c) {
        c.renderbuffer_storage_multisample(gl::RENDERBUFFER, 4, gl::DEPTH_COMPONENT16, 4, 4);
        assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_INCOMPLETE_MULTISAMPLE);
    }
    // Depth and stencil from different images.
    c.renderbuffer_storage(gl::RENDERBUFFER, gl::DEPTH_COMPONENT16, 4, 4);
    let rb2 = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, rb2);
    c.renderbuffer_storage(gl::RENDERBUFFER, gl::STENCIL_INDEX8, 4, 4);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::STENCIL_ATTACHMENT, gl::RENDERBUFFER, rb2);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_UNSUPPORTED);
    // One depth-stencil image for both.
    let ds = c.gen_renderbuffer();
    c.bind_renderbuffer(gl::RENDERBUFFER, ds);
    c.renderbuffer_storage(gl::RENDERBUFFER, gl::DEPTH24_STENCIL8, 4, 4);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::DEPTH_STENCIL_ATTACHMENT, gl::RENDERBUFFER, ds);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    assert_eq!(
        c.get_framebuffer_attachment_parameteri(
            gl::FRAMEBUFFER,
            gl::DEPTH_STENCIL_ATTACHMENT,
            gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME
        ),
        ds as i32
    );
    assert_eq!(
        c.get_framebuffer_attachment_parameteri(
            gl::FRAMEBUFFER,
            gl::STENCIL_ATTACHMENT,
            gl::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE
        ),
        8
    );
    // Deleting an attached texture detaches it from the bound framebuffer.
    c.delete_textures(&[t2]);
    assert_eq!(
        c.get_framebuffer_attachment_parameteri(
            gl::FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE
        ),
        gl::NONE as i32
    );
    // ... but not from a framebuffer that is not bound, which keeps it.
    let t3 = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t3);
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, 4, 4, 0, gl::RGBA, gl::UNSIGNED_BYTE, Pixels::None);
    c.framebuffer_texture_2d(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, t3, 0);
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    c.delete_textures(&[t3]);
    c.bind_framebuffer(gl::FRAMEBUFFER, f);
    assert_eq!(c.check_framebuffer_status(gl::FRAMEBUFFER), gl::FRAMEBUFFER_COMPLETE);
    c.clear_color(1.0, 0.0, 0.0, 1.0);
    c.clear(gl::COLOR_BUFFER_BIT);
    let mut out = [0u8; 4];
    c.read_pixels(0, 0, 1, 1, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
    assert_eq!(out, [255, 0, 0, 255]);
    // The default framebuffer cannot take attachments.
    c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    c.framebuffer_renderbuffer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::RENDERBUFFER, ds);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    assert_eq!(
        c.get_framebuffer_attachment_parameteri(gl::FRAMEBUFFER, gl::BACK, gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE),
        gl::FRAMEBUFFER_DEFAULT as i32
    );
    no_error(&mut c);
}

#[test]
fn state_queries() {
    let mut c = context(16, 8);
    let mut v = [0i32; 4];
    c.get_integerv(gl::VIEWPORT, &mut v);
    assert_eq!(v, [0, 0, 16, 8]);
    c.blend_func_separate(gl::SRC_ALPHA, gl::ONE, gl::ZERO, gl::DST_COLOR);
    assert_eq!(c.get_integer(gl::BLEND_DST_ALPHA), gl::DST_COLOR as i32);
    c.clear_color(1.0, 0.5, 0.0, -1.0);
    let mut f = [0.0f32; 4];
    c.get_floatv(gl::COLOR_CLEAR_VALUE, &mut f);
    assert_eq!(f, [1.0, 0.5, 0.0, -1.0]);
    // Normalized values become the full integer range.
    c.get_integerv(gl::COLOR_CLEAR_VALUE, &mut v);
    assert_eq!(v[0], i32::MAX);
    assert_eq!(v[3], i32::MIN);
    assert!(c.get_boolean(gl::DITHER));
    c.disable(gl::DITHER);
    assert!(!c.is_enabled(gl::DITHER));
    assert_eq!(c.get_integer(gl::MAJOR_VERSION), 3);
    assert_eq!(c.get_integer(gl::MAX_DRAW_BUFFERS), 4);
    assert!(c.get_integer(gl::MAX_TEXTURE_SIZE) >= 2048);
    assert!(c.get_integer(gl::MAX_UNIFORM_BLOCK_SIZE) >= 16384);
    assert_eq!(c.get_integer(gl::IMPLEMENTATION_COLOR_READ_FORMAT), gl::RGBA as i32);
    assert_eq!(c.get_integer(gl::RED_BITS), 8);
    assert_eq!(c.get_integer(gl::DEPTH_BITS), 24);
    assert_eq!(c.get_integer(gl::STENCIL_BITS), 8);
    assert!(c.get_string(gl::VERSION).unwrap().starts_with("OpenGL ES 3.0"));
    let n = c.get_integer(gl::NUM_EXTENSIONS);
    assert!(n > 0);
    assert_eq!(c.get_stringi(gl::EXTENSIONS, 0), Some("GL_OES_element_index_uint"));
    assert_eq!(c.get_stringi(gl::EXTENSIONS, n as u32), None);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    let mut formats = [0i32; 16];
    c.get_integerv(gl::COMPRESSED_TEXTURE_FORMATS, &mut formats);
    assert!(formats.contains(&(gl::COMPRESSED_RGBA8_ETC2_EAC as i32)));
    c.get_integerv(0xFFFF, &mut v);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    no_error(&mut c);
}

#[test]
fn samplers_override_texture_parameters() {
    let mut c = context(1, 1);
    let s = c.gen_sampler();
    assert!(c.is_sampler(s));
    c.sampler_parameteri(s, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
    assert_eq!(c.get_sampler_parameteri(s, gl::TEXTURE_MIN_FILTER), gl::LINEAR as i32);
    c.sampler_parameteri(s, gl::TEXTURE_BASE_LEVEL, 1);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
    c.bind_sampler(0, 1234);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.bind_sampler(40, s);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    // A texture without mipmaps is complete when the sampler does not
    // ask for them.
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0, 1); }",
        "#version 300 es
        precision mediump float; uniform sampler2D t; out vec4 color; void main() { color = texture(t, vec2(0.5)); }",
    );
    c.use_program(p);
    buffer_f32(&mut c, gl::ARRAY_BUFFER, &[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0]);
    c.vertex_attrib_pointer(0, 2, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(0);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    // (A 1x1 texture is its whole mipmap chain: 2x2 needs level 1.)
    c.tex_image_2d(
        gl::TEXTURE_2D,
        0,
        gl::RGBA8,
        2,
        2,
        0,
        gl::RGBA,
        gl::UNSIGNED_BYTE,
        &[0u8, 200, 0, 255].repeat(4)[..],
    );
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [0, 0, 0, 255], "incomplete without the sampler");
    c.bind_sampler(0, s);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [0, 200, 0, 255]);
    c.delete_samplers(&[s]);
    assert_eq!(c.get_integer(gl::SAMPLER_BINDING), 0);
    no_error(&mut c);
}

#[test]
fn transform_feedback_object_rules() {
    let mut c = context(1, 1);
    let v = c.create_shader(gl::VERTEX_SHADER);
    c.shader_source(v, &["#version 300 es\nout float o; void main() { gl_Position = vec4(0); o = 1.0; }"]);
    c.compile_shader(v);
    let f = c.create_shader(gl::FRAGMENT_SHADER);
    c.shader_source(f, &["#version 300 es\nprecision mediump float; out vec4 c; void main() { c = vec4(1); }"]);
    c.compile_shader(f);
    let p = c.create_program();
    c.attach_shader(p, v);
    c.attach_shader(p, f);
    c.transform_feedback_varyings(p, &["o"], gl::INTERLEAVED_ATTRIBS);
    c.link_program(p);
    c.use_program(p);
    let tf = c.gen_transform_feedback();
    c.bind_transform_feedback(gl::TRANSFORM_FEEDBACK, 5000);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.bind_transform_feedback(gl::TRANSFORM_FEEDBACK, tf);
    assert!(c.is_transform_feedback(tf));
    // No buffer bound: beginning fails.
    c.begin_transform_feedback(gl::POINTS);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    let b = c.gen_buffer();
    c.bind_buffer_base(gl::TRANSFORM_FEEDBACK_BUFFER, 0, b);
    c.buffer_data_size(gl::TRANSFORM_FEEDBACK_BUFFER, 64, gl::STREAM_READ);
    c.begin_transform_feedback(gl::POINTS);
    no_error(&mut c);
    for check in 0..5 {
        match check {
            0 => c.bind_transform_feedback(gl::TRANSFORM_FEEDBACK, 0),
            1 => c.delete_transform_feedbacks(&[tf]),
            2 => c.use_program(0),
            3 => c.link_program(p),
            _ => c.bind_buffer_base(gl::TRANSFORM_FEEDBACK_BUFFER, 0, 0),
        }
        assert_eq!(c.get_error(), gl::INVALID_OPERATION, "check {check}");
    }
    // Paused: another object may be bound; resume needs the program.
    c.pause_transform_feedback();
    c.bind_transform_feedback(gl::TRANSFORM_FEEDBACK, 0);
    no_error(&mut c);
    c.bind_transform_feedback(gl::TRANSFORM_FEEDBACK, tf);
    c.resume_transform_feedback();
    c.end_transform_feedback();
    c.end_transform_feedback();
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.delete_transform_feedbacks(&[tf]);
    assert!(!c.is_transform_feedback(tf));
    no_error(&mut c);
}

#[test]
fn program_introspection() {
    let mut c = context(1, 1);
    let p = program(
        &mut c,
        "#version 300 es
        layout(location = 3) in vec4 a; in mat3 m; in float b;
        void main() { gl_Position = a + vec4(m[0] * b, 0); }",
        "#version 300 es
        precision mediump float;
        layout(location = 1) out vec4 second;
        void main() { second = vec4(1); }",
    );
    assert_eq!(c.get_attrib_location(p, "a"), 3);
    let (lm, lb) = (c.get_attrib_location(p, "m"), c.get_attrib_location(p, "b"));
    // The matrix takes three consecutive locations, none of them 3.
    assert!(lm >= 0 && lb >= 0 && !(lm..lm + 3).contains(&3) && !(lm..lm + 3).contains(&lb));
    assert_eq!(c.get_programiv(p, gl::ACTIVE_ATTRIBUTES), 3);
    let (size, ty, _) = c.get_active_attrib(p, 0).unwrap();
    assert_eq!(size, 1);
    assert!([gl::FLOAT_VEC4, gl::FLOAT_MAT3, gl::FLOAT].contains(&ty));
    assert_eq!(c.get_frag_data_location(p, "second"), 1);
    assert_eq!(c.get_frag_data_location(p, "nothing"), -1);
    assert_eq!(c.get_programiv(p, gl::ACTIVE_ATTRIBUTE_MAX_LENGTH), 2);
    no_error(&mut c);
}
