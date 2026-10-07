//! Tests of the rest of the pipeline: uniform blocks, transform feedback,
//! queries, points and lines, clipping, sync objects.

use super::*;

fn floats(c: &mut Context, p: u32, name: &str, size: i32, data: &[f32]) {
    buffer_f32(c, gl::ARRAY_BUFFER, data);
    let loc = c.get_attrib_location(p, name);
    assert!(loc >= 0, "no attribute {name}");
    c.vertex_attrib_pointer(loc as u32, size, gl::FLOAT, false, 0, 0);
    c.enable_vertex_attrib_array(loc as u32);
}

fn screen(c: &mut Context, p: u32) {
    floats(c, p, "pos", 2, &[-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 1.0]);
}

#[test]
fn uniform_blocks_use_std140() {
    let mut c = context(1, 1);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0, 1); }",
        "#version 300 es
        precision highp float;
        layout(std140) uniform Params { float scale; vec3 tint; float extra[2]; mat2 m; };
        out vec4 color;
        void main() { color = vec4(tint * scale, extra[1] + m[1][0]); }",
    );
    c.use_program(p);
    screen(&mut c, p);
    let index = c.get_uniform_block_index(p, "Params");
    assert_ne!(index, gl::INVALID_INDEX);
    let mut size = [0];
    c.get_active_uniform_blockiv(p, index, gl::UNIFORM_BLOCK_DATA_SIZE, &mut size);
    // scale 0, tint 16 (vec3 aligned to 16), extra at 32 with stride 16,
    // m at 64 (columns 16 apart): 96 bytes.
    assert_eq!(size[0], 96);
    let names = ["tint", "extra[0]", "m"];
    let idx = c.get_uniform_indices(p, &names);
    let mut offsets = [0; 3];
    c.get_active_uniformsiv(p, &idx, gl::UNIFORM_OFFSET, &mut offsets);
    assert_eq!(offsets, [16, 32, 64]);
    let mut strides = [0; 3];
    c.get_active_uniformsiv(p, &idx, gl::UNIFORM_ARRAY_STRIDE, &mut strides);
    assert_eq!(strides[1], 16);
    let mut data = [0f32; 24];
    data[0] = 0.5; // scale
    data[4..7].copy_from_slice(&[1.0, 0.5, 0.25]); // tint
    data[12] = 0.125; // extra[1] at 48
    data[20] = 0.25; // m[1][0] at 64 + 16
    let ub = buffer_f32(&mut c, gl::UNIFORM_BUFFER, &data);
    c.uniform_block_binding(p, index, 3);
    c.bind_buffer_base(gl::UNIFORM_BUFFER, 3, ub);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    assert_eq!(px(&read_rgba(&mut c, 1, 1), 1, 0, 0), [128, 64, 32, 96]);
    // A range that is not aligned is refused.
    c.bind_buffer_range(gl::UNIFORM_BUFFER, 0, ub, 4, 16);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
}

#[test]
fn uniforms_are_kept_per_draw() {
    // Uniform changes between recorded draws do not reach earlier ones.
    let mut c = context(2, 1);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0, 1); }",
        "#version 300 es
        precision mediump float; uniform vec4 u; out vec4 color; void main() { color = u; }",
    );
    c.use_program(p);
    screen(&mut c, p);
    let u = c.get_uniform_location(p, "u");
    c.enable(gl::SCISSOR_TEST);
    for x in 0..2 {
        c.scissor(x, 0, 1, 1);
        c.uniform4f(u, x as f32, 1.0 - x as f32, 0.0, 1.0);
        c.draw_arrays(gl::TRIANGLES, 0, 6);
    }
    let img = read_rgba(&mut c, 2, 1);
    assert_eq!(px(&img, 2, 0, 0), [0, 255, 0, 255]);
    assert_eq!(px(&img, 2, 1, 0), [255, 0, 0, 255]);
}

#[test]
fn transform_feedback_records_vertices() {
    let mut c = context(1, 1);
    let v = c.create_shader(gl::VERTEX_SHADER);
    c.shader_source(
        v,
        &["#version 300 es
        in float x; out float doubled; out vec2 pair; flat out int id;
        void main() { gl_Position = vec4(0); doubled = 2.0 * x; pair = vec2(x, -x); id = gl_VertexID; }"],
    );
    c.compile_shader(v);
    let f = c.create_shader(gl::FRAGMENT_SHADER);
    c.shader_source(
        f,
        &["#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }"],
    );
    c.compile_shader(f);
    let p = c.create_program();
    c.attach_shader(p, v);
    c.attach_shader(p, f);
    c.transform_feedback_varyings(p, &["pair", "doubled", "id"], gl::INTERLEAVED_ATTRIBS);
    c.link_program(p);
    assert_eq!(c.get_programiv(p, gl::LINK_STATUS), 1, "{}", c.get_program_info_log(p));
    assert_eq!(c.get_programiv(p, gl::TRANSFORM_FEEDBACK_VARYINGS), 3);
    c.use_program(p);
    floats(&mut c, p, "x", 1, &[1.0, 2.0, 3.0, 4.0, 5.0]);
    let out = c.gen_buffer();
    c.bind_buffer_base(gl::TRANSFORM_FEEDBACK_BUFFER, 0, out);
    c.buffer_data_size(gl::TRANSFORM_FEEDBACK_BUFFER, 4 * 4 * 6, gl::STREAM_READ);
    let q = c.gen_query();
    c.begin_query(gl::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, q);
    c.enable(gl::RASTERIZER_DISCARD);
    c.begin_transform_feedback(gl::POINTS);
    // Only POINTS may be drawn, and no indexed draws.
    c.draw_arrays(gl::LINES, 0, 2);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.draw_arrays(gl::POINTS, 1, 4);
    no_error(&mut c);
    // Overflow: 4 recorded of 6, 3 more do not fit.
    c.draw_arrays(gl::POINTS, 0, 3);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.end_transform_feedback();
    c.end_query(gl::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN);
    c.disable(gl::RASTERIZER_DISCARD);
    no_error(&mut c);
    assert_eq!(c.get_query_objectuiv(q, gl::QUERY_RESULT), 4);
    let bytes = c.map_buffer_range(gl::TRANSFORM_FEEDBACK_BUFFER, 0, 4 * 4 * 4, gl::MAP_READ_BIT).unwrap().to_vec();
    c.unmap_buffer(gl::TRANSFORM_FEEDBACK_BUFFER);
    let w = |i: usize| u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]);
    let f = |i: usize| f32::from_bits(w(i));
    // Each vertex: pair.x, pair.y, doubled, id.
    for (k, x) in [2.0f32, 3.0, 4.0, 5.0].iter().enumerate() {
        assert_eq!([f(4 * k), f(4 * k + 1), f(4 * k + 2)], [*x, -*x, 2.0 * x]);
        assert_eq!(w(4 * k + 3), k as u32 + 1);
    }
}

#[test]
fn separate_feedback_buffers() {
    let mut c = context(1, 1);
    let v = c.create_shader(gl::VERTEX_SHADER);
    c.shader_source(
        v,
        &["#version 300 es
        in vec2 p; out float a; out float b;
        void main() { gl_Position = vec4(p, 0, 1); a = p.x; b = p.y; }"],
    );
    c.compile_shader(v);
    let f = c.create_shader(gl::FRAGMENT_SHADER);
    c.shader_source(
        f,
        &["#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }"],
    );
    c.compile_shader(f);
    let p = c.create_program();
    c.attach_shader(p, v);
    c.attach_shader(p, f);
    c.transform_feedback_varyings(p, &["b", "a"], gl::SEPARATE_ATTRIBS);
    c.link_program(p);
    c.use_program(p);
    floats(&mut c, p, "p", 2, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    let (ba, bb) = (c.gen_buffer(), c.gen_buffer());
    for (i, b) in [ba, bb].into_iter().enumerate() {
        c.bind_buffer_base(gl::TRANSFORM_FEEDBACK_BUFFER, i as u32, b);
        c.buffer_data_size(gl::TRANSFORM_FEEDBACK_BUFFER, 12, gl::STREAM_READ);
    }
    // The triangle is still rasterized (the 1x1 framebuffer is unaffected).
    c.begin_transform_feedback(gl::TRIANGLES);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    c.end_transform_feedback();
    no_error(&mut c);
    let read = |c: &mut Context, b: u32| {
        c.bind_buffer(gl::COPY_READ_BUFFER, b);
        let v = c.map_buffer_range(gl::COPY_READ_BUFFER, 0, 12, gl::MAP_READ_BIT).unwrap().to_vec();
        c.unmap_buffer(gl::COPY_READ_BUFFER);
        v.chunks(4).map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]])).collect::<Vec<_>>()
    };
    assert_eq!(read(&mut c, ba), [0.2, 0.4, 0.6]);
    assert_eq!(read(&mut c, bb), [0.1, 0.3, 0.5]);
}

#[test]
fn occlusion_queries_count() {
    let mut c = context(4, 4);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; uniform float z; void main() { gl_Position = vec4(pos, z, 1); }",
        "#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }",
    );
    c.use_program(p);
    screen(&mut c, p);
    let z = c.get_uniform_location(p, "z");
    c.enable(gl::DEPTH_TEST);
    c.clear(gl::DEPTH_BUFFER_BIT);
    c.uniform1f(z, 0.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let q = c.gen_query();
    // Behind: nothing passes.
    c.begin_query(gl::ANY_SAMPLES_PASSED, q);
    c.uniform1f(z, 0.5);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.end_query(gl::ANY_SAMPLES_PASSED);
    // The result becomes available (at once with the software renderer,
    // once the GPU is done otherwise).
    let mut tries = 0;
    while c.get_query_objectuiv(q, gl::QUERY_RESULT_AVAILABLE) == 0 {
        tries += 1;
        assert!(tries < 10_000, "the query never finished");
        c.flush();
        std::thread::yield_now();
    }
    assert_eq!(c.get_query_objectuiv(q, gl::QUERY_RESULT), 0);
    // In front: some pass.
    c.begin_query(gl::ANY_SAMPLES_PASSED_CONSERVATIVE, q);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION, "the query's target cannot change");
    let q2 = c.gen_query();
    c.begin_query(gl::ANY_SAMPLES_PASSED, q2);
    c.uniform1f(z, -0.5);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    // Only one occlusion query at a time.
    c.begin_query(gl::ANY_SAMPLES_PASSED_CONSERVATIVE, q);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
    c.end_query(gl::ANY_SAMPLES_PASSED);
    assert_eq!(c.get_query_objectuiv(q2, gl::QUERY_RESULT), 1);
    // Names that were never generated are refused.
    c.begin_query(gl::ANY_SAMPLES_PASSED, 777);
    assert_eq!(c.get_error(), gl::INVALID_OPERATION);
}

/// The result of an occlusion query, once it is available.
fn occlusion_result(c: &mut Context, q: u32) -> u32 {
    let mut tries = 0;
    while c.get_query_objectuiv(q, gl::QUERY_RESULT_AVAILABLE) == 0 {
        tries += 1;
        assert!(tries < 10_000, "the query never finished");
        c.flush();
        std::thread::yield_now();
    }
    c.get_query_objectuiv(q, gl::QUERY_RESULT)
}

#[test]
fn occlusion_queries_count_only_draws() {
    // Clears, blits and reads generate no fragments; a renderer that does
    // them by drawing must not count those draws.
    let mut c = context(4, 4);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; uniform float z; void main() { gl_Position = vec4(pos, z, 1); }",
        "#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }",
    );
    c.use_program(p);
    screen(&mut c, p);
    let z = c.get_uniform_location(p, "z");
    c.uniform1f(z, 2.0);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_3D, t);
    c.tex_storage_3d(gl::TEXTURE_3D, 1, gl::RGBA8, 4, 4, 2);
    let f = c.gen_framebuffer();
    let others = |c: &mut Context| {
        // A scissored clear and a masked one.
        c.enable(gl::SCISSOR_TEST);
        c.scissor(1, 1, 2, 2);
        c.clear(gl::COLOR_BUFFER_BIT);
        c.disable(gl::SCISSOR_TEST);
        c.color_mask(true, false, true, true);
        c.clear(gl::COLOR_BUFFER_BIT);
        c.color_mask(true, true, true, true);
        // A slice of a 3D texture: read, cleared, blitted into.
        c.bind_framebuffer(gl::FRAMEBUFFER, f);
        c.framebuffer_texture_layer(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, t, 0, 1);
        let mut out = [0u8; 64];
        c.read_pixels(0, 0, 4, 4, gl::RGBA, gl::UNSIGNED_BYTE, &mut out[..]);
        c.clear(gl::COLOR_BUFFER_BIT);
        c.bind_framebuffer(gl::READ_FRAMEBUFFER, 0);
        c.blit_framebuffer(0, 0, 4, 4, 0, 0, 2, 2, gl::COLOR_BUFFER_BIT, gl::LINEAR);
        c.bind_framebuffer(gl::FRAMEBUFFER, 0);
    };
    let q = c.gen_query();
    // Outside the clip volume (z = 2): no draw passes.
    c.begin_query(gl::ANY_SAMPLES_PASSED, q);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    others(&mut c);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.end_query(gl::ANY_SAMPLES_PASSED);
    no_error(&mut c);
    assert_eq!(occlusion_result(&mut c, q), 0);
    // A draw that passes, after the others, still counts.
    c.begin_query(gl::ANY_SAMPLES_PASSED, q);
    others(&mut c);
    c.uniform1f(z, 0.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    c.end_query(gl::ANY_SAMPLES_PASSED);
    assert_eq!(occlusion_result(&mut c, q), 1);
}

#[test]
fn points_have_size_and_coordinates() {
    let mut c = context(8, 8);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0, 1); gl_PointSize = 4.0; }",
        "#version 300 es
        precision highp float; out vec4 color; void main() { color = vec4(gl_PointCoord, 0, 1); }",
    );
    c.use_program(p);
    // Centre at window (4, 4): covers pixels 2..5.
    floats(&mut c, p, "pos", 2, &[0.0, 0.0]);
    c.draw_arrays(gl::POINTS, 0, 1);
    no_error(&mut c);
    let img = read_rgba(&mut c, 8, 8);
    assert_eq!(img.chunks(4).filter(|p| p[3] == 255).count(), 16);
    // gl_PointCoord: s from the left, t from the top: pixel (2, 5) is the
    // top-left one, coordinates (1/8, 1/8).
    assert_eq!(px(&img, 8, 2, 5), [32, 32, 0, 255]);
    assert_eq!(px(&img, 8, 5, 2), [223, 223, 0, 255]);
}

#[test]
fn lines_cover_one_pixel_per_column() {
    let mut c = context(8, 8);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; void main() { gl_Position = vec4(pos, 0, 1); }",
        "#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }",
    );
    c.use_program(p);
    // A gentle slope across the framebuffer.
    floats(&mut c, p, "pos", 2, &[-1.0, -0.9, 1.0, 0.3]);
    c.draw_arrays(gl::LINES, 0, 2);
    no_error(&mut c);
    let img = read_rgba(&mut c, 8, 8);
    for x in 0..8 {
        let n = (0..8).filter(|&y| px(&img, 8, x, y)[0] == 255).count();
        assert_eq!(n, 1, "column {x}");
    }
    // A line loop closes; a strip does not.
    c.clear(gl::COLOR_BUFFER_BIT);
    floats(&mut c, p, "pos", 2, &[-0.75, -0.75, 0.75, -0.75, 0.75, 0.75]);
    c.draw_arrays(gl::LINE_LOOP, 0, 3);
    let img = read_rgba(&mut c, 8, 8);
    // The closing diagonal passes through the middle.
    assert_eq!(px(&img, 8, 4, 4)[0], 255);
}

#[test]
fn clips_against_near_and_far() {
    let mut c = context(8, 8);
    let p = program(
        &mut c,
        "#version 300 es
        in vec4 pos; void main() { gl_Position = pos; }",
        "#version 300 es
        precision mediump float; out vec4 color; void main() { color = vec4(1); }",
    );
    c.use_program(p);
    // A quad whose left half is behind the near plane (z < -w).
    let v = [
        -1.0f32, -1.0, -3.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, -3.0, 1.0, -1.0, 1.0, -3.0, 1.0, 1.0, -1.0, 1.0, 1.0,
        1.0, 1.0, 1.0, 1.0,
    ];
    floats(&mut c, p, "pos", 4, &v);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    no_error(&mut c);
    let img = read_rgba(&mut c, 8, 8);
    // z_ndc = -3 + 4 t: inside from t = 0.5, i.e. the right half.
    for y in 0..8 {
        let n = (0..8).filter(|&x| px(&img, 8, x, y)[0] == 255).count();
        assert_eq!(n, 4, "row {y}");
    }
    // A huge triangle (far beyond the guard band) still covers everything.
    c.clear(gl::COLOR_BUFFER_BIT);
    let big = [-1e7f32, -1e7, 0.0, 1.0, 1e7, -1e7, 0.0, 1.0, 0.0, 1e7, 0.0, 1.0];
    floats(&mut c, p, "pos", 4, &big);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    let img = read_rgba(&mut c, 8, 8);
    assert!(img.chunks(4).all(|p| p[0] == 255));
    // Vertices behind the eye (w < 0) are clipped away.
    c.clear(gl::COLOR_BUFFER_BIT);
    let behind = [-1.0f32, -1.0, 0.0, -1.0, 1.0, -1.0, 0.0, -1.0, 0.0, 1.0, 0.0, -1.0];
    floats(&mut c, p, "pos", 4, &behind);
    c.draw_arrays(gl::TRIANGLES, 0, 3);
    let img = read_rgba(&mut c, 8, 8);
    assert!(img.chunks(4).all(|p| p[0] == 0));
}

#[test]
fn polygon_offset_and_discard() {
    let mut c = context(2, 1);
    let p = program(
        &mut c,
        "#version 300 es
        in vec2 pos; uniform vec4 col; void main() { gl_Position = vec4(pos, 0, 1); }",
        "#version 300 es
        precision mediump float; uniform vec4 col; out vec4 color;
        void main() { if (gl_FragCoord.x > 1.0 && col.b > 0.5) discard; color = col; }",
    );
    c.use_program(p);
    screen(&mut c, p);
    let col = c.get_uniform_location(p, "col");
    c.enable(gl::DEPTH_TEST);
    c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
    c.uniform4f(col, 1.0, 0.0, 0.0, 1.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    // The same depth fails LESS unless offset towards the viewer.
    c.uniform4f(col, 0.0, 1.0, 0.0, 1.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 2, 1), 2, 0, 0), [255, 0, 0, 255]);
    c.enable(gl::POLYGON_OFFSET_FILL);
    c.polygon_offset(0.0, -2.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    assert_eq!(px(&read_rgba(&mut c, 2, 1), 2, 0, 0), [0, 255, 0, 255]);
    // Discarded fragments write nothing (pixel 1), the others write.
    c.polygon_offset(0.0, -4.0);
    c.uniform4f(col, 0.0, 0.0, 1.0, 1.0);
    c.draw_arrays(gl::TRIANGLES, 0, 6);
    let img = read_rgba(&mut c, 2, 1);
    assert_eq!(px(&img, 2, 0, 0), [0, 0, 255, 255]);
    assert_eq!(px(&img, 2, 1, 0), [0, 255, 0, 255]);
}

#[test]
fn sync_objects() {
    let mut c = context(1, 1);
    let s = c.fence_sync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0);
    assert!(c.is_sync(s));
    assert_eq!(c.client_wait_sync(s, gl::SYNC_FLUSH_COMMANDS_BIT, 1_000_000), gl::CONDITION_SATISFIED);
    assert_eq!(c.client_wait_sync(s, 0, 0), gl::ALREADY_SIGNALED);
    assert_eq!(c.get_synciv(s, gl::SYNC_STATUS), gl::SIGNALED as i32);
    c.wait_sync(s, 0, gl::TIMEOUT_IGNORED);
    no_error(&mut c);
    c.wait_sync(s, 0, 5);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    c.delete_sync(s);
    assert!(!c.is_sync(s));
    c.delete_sync(s);
    assert_eq!(c.get_error(), gl::INVALID_VALUE);
    assert_eq!(c.fence_sync(0x1234, 0), 0);
    assert_eq!(c.get_error(), gl::INVALID_ENUM);
}
