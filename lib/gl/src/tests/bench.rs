//! Rendering speed (run with `cargo test --release -p vgl bench -- --ignored
//! --nocapture`).

use super::*;

fn scene(c: &mut Context, w: u32, h: u32) {
    let p = program(
        c,
        "#version 300 es
        in vec3 pos; in vec2 uv0; in vec3 col; out vec2 uv; out vec3 tint;
        uniform mat4 mvp;
        void main() { gl_Position = mvp * vec4(pos, 1.0); uv = uv0; tint = col; }",
        "#version 300 es
        precision mediump float; uniform sampler2D t; in vec2 uv; in vec3 tint; out vec4 color;
        void main() { color = texture(t, uv) * vec4(tint, 1.0); }",
    );
    c.use_program(p);
    let mut pos = Vec::new();
    let mut uv = Vec::new();
    let mut col = Vec::new();
    let mut seed = 7u32;
    let mut rnd = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    // 400 triangles of about a ninetieth of the screen each: ~4x overdraw.
    for _ in 0..400 {
        let (cx, cy, z) = (rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0, rnd());
        for k in 0..3 {
            let a = k as f32 * 2.1 + rnd();
            pos.extend_from_slice(&[cx + 0.25 * vmath::f32::cos(a), cy + 0.25 * vmath::f32::sin(a), z]);
            uv.extend_from_slice(&[rnd(), rnd()]);
            col.extend_from_slice(&[rnd(), rnd(), rnd()]);
        }
    }
    for (name, size, data) in [("pos", 3, &pos), ("uv0", 2, &uv), ("col", 3, &col)] {
        buffer_f32(c, gl::ARRAY_BUFFER, data);
        let l = c.get_attrib_location(p, name) as u32;
        c.vertex_attrib_pointer(l, size, gl::FLOAT, false, 0, 0);
        c.enable_vertex_attrib_array(l);
    }
    let m = c.get_uniform_location(p, "mvp");
    c.uniform_matrix4fv(m, false, &[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
    let t = c.gen_texture();
    c.bind_texture(gl::TEXTURE_2D, t);
    let texels: Vec<u8> = (0..256 * 256 * 4).map(|i| (i * 7 % 251) as u8).collect();
    c.tex_image_2d(gl::TEXTURE_2D, 0, gl::RGBA8, 256, 256, 0, gl::RGBA, gl::UNSIGNED_BYTE, &texels[..]);
    c.generate_mipmap(gl::TEXTURE_2D);
    c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR_MIPMAP_LINEAR as i32);
    c.enable(gl::DEPTH_TEST);
    c.viewport(0, 0, w as i32, h as i32);
}

#[test]
#[ignore]
fn bench() {
    let (w, h) = (1280, 720);
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    for threads in [1, cpus] {
        let mut c = context_with(w, h, threads, 0);
        scene(&mut c, w, h);
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
            c.draw_arrays(gl::TRIANGLES, 0, 1200);
            c.finish();
            best = best.min(t0.elapsed().as_secs_f64());
        }
        std::println!("{threads:2} threads: {:.1} ms per frame", best * 1000.0);
    }
}

#[test]
#[ignore]
fn bench_parts() {
    let (w, h) = (1280, 720);
    let variants = [
        (
            "constant",
            "#version 300 es
        precision mediump float; in vec2 uv; in vec3 tint; out vec4 color;
        void main() { color = vec4(1.0, 0.5, 0.25, 1.0); }",
        ),
        (
            "varyings",
            "#version 300 es
        precision mediump float; in vec2 uv; in vec3 tint; out vec4 color;
        void main() { color = vec4(tint * uv.x, uv.y); }",
        ),
        (
            "texture",
            "#version 300 es
        precision mediump float; uniform sampler2D t; in vec2 uv; in vec3 tint; out vec4 color;
        void main() { color = texture(t, uv) * vec4(tint, 1.0); }",
        ),
    ];
    for (name, fs) in variants {
        for filter in [gl::NEAREST, gl::LINEAR_MIPMAP_LINEAR] {
            let mut c = context_with(w, h, 1, 0);
            scene(&mut c, w, h);
            c.tex_parameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, filter as i32);
            c.tex_parameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_MAG_FILTER,
                if filter == gl::NEAREST { gl::NEAREST } else { gl::LINEAR } as i32,
            );
            let p = program(
                &mut c,
                "#version 300 es
                in vec3 pos; in vec2 uv0; in vec3 col; out vec2 uv; out vec3 tint;
                uniform mat4 mvp;
                void main() { gl_Position = mvp * vec4(pos, 1.0); uv = uv0; tint = col; }",
                fs,
            );
            for (i, n) in ["pos", "uv0", "col"].iter().enumerate() {
                c.bind_attrib_location(p, i as u32, n);
            }
            c.link_program(p);
            c.use_program(p);
            let m = c.get_uniform_location(p, "mvp");
            c.uniform_matrix4fv(
                m,
                false,
                &[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            );
            let (mut best, mut rec) = (f64::MAX, f64::MAX);
            for _ in 0..3 {
                let t0 = std::time::Instant::now();
                c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
                c.draw_arrays(gl::TRIANGLES, 0, 1200);
                let t1 = std::time::Instant::now();
                c.finish();
                best = best.min(t0.elapsed().as_secs_f64());
                rec = rec.min((t1 - t0).as_secs_f64());
            }
            std::println!("{name:10} {filter:#x}: {:.1} ms ({:.2} ms recording)", best * 1000.0, rec * 1000.0);
            if name != "texture" {
                break;
            }
        }
    }
    // Clears alone.
    let mut c = context_with(w, h, 1, 0);
    let t0 = std::time::Instant::now();
    for _ in 0..10 {
        c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
        c.finish();
    }
    std::println!("clear: {:.2} ms", t0.elapsed().as_secs_f64() * 100.0);
}

#[test]
#[ignore]
fn bench_depth() {
    let (w, h) = (1280, 720);
    for depth in [true, false] {
        let mut c = context_with(w, h, 1, 0);
        scene(&mut c, w, h);
        let p = program(
            &mut c,
            "#version 300 es
            in vec3 pos; in vec2 uv0; in vec3 col; out vec2 uv; out vec3 tint;
            uniform mat4 mvp;
            void main() { gl_Position = mvp * vec4(pos, 1.0); uv = uv0; tint = col; }",
            "#version 300 es
            precision mediump float; out vec4 color;
            void main() { color = vec4(1.0, 0.5, 0.25, 1.0); }",
        );
        for (i, n) in ["pos", "uv0", "col"].iter().enumerate() {
            c.bind_attrib_location(p, i as u32, n);
        }
        c.link_program(p);
        c.use_program(p);
        let m = c.get_uniform_location(p, "mvp");
        c.uniform_matrix4fv(
            m,
            false,
            &[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
        );
        if !depth {
            c.disable(gl::DEPTH_TEST);
        }
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            c.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
            c.draw_arrays(gl::TRIANGLES, 0, 1200);
            c.finish();
            best = best.min(t0.elapsed().as_secs_f64());
        }
        let img = read_rgba(&mut c, w, h);
        let covered = img.chunks(4).filter(|p| p[0] == 255).count();
        std::println!("depth {depth}: {:.1} ms, {covered} pixels covered", best * 1000.0);
    }
}
