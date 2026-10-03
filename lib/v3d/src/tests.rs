//! Host unit tests: clipping, rasterisation coverage and the top-left fill
//! rule, depth ordering, chunking, textures, scaling, and a complete scene
//! rendered to a PNG (written to `$V3D_TEST_OUT` when that is set).

use std::format;
use std::vec;
use std::vec::Vec;

use vmath::{Mat4, Quat, Vec2, Vec3};

use crate::pipeline::{self, Setup, TVert, Target, Tri};
use crate::*;

// -- Helpers -----------------------------------------------------------------

/// A minimal PNG encoder (stored deflate blocks), for inspecting images.
fn png(width: usize, height: usize, argb: &[u32]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let mut raw = Vec::with_capacity(height * (width * 3 + 1));
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            let p = argb[y * width + x];
            raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn save_png(name: &str, width: usize, height: usize, argb: &[u32]) {
    if let Ok(dir) = std::env::var("V3D_TEST_OUT") {
        let _ = std::fs::create_dir_all(&dir);
        let path = std::path::Path::new(&dir).join(name);
        std::fs::write(&path, png(width, height, argb)).expect("writing PNG");
        std::println!("wrote {}", path.display());
    }
}

/// A screen-space vertex for driving the core directly (28.4 coordinates).
/// The colour goes into the additive part so that it is reproduced exactly.
fn sv(x: f32, y: f32, q: i32, color: u32) -> TVert {
    let c = |s: u32| (((color >> s) & 255) * 256) as u16;
    TVert {
        cx: 0,
        cy: 0,
        cw: 65536,
        sx: (x * 16.0) as i32,
        sy: (y * 16.0) as i32,
        q,
        u: 0,
        v: 0,
        mul: [0, 0, 0, 256],
        add: [c(16), c(8), c(0)],
        outcode: 0,
    }
}

struct Canvas {
    w: i32,
    h: i32,
    color: Vec<u32>,
    depth: Vec<i32>,
}

impl Canvas {
    fn new(w: i32, h: i32) -> Canvas {
        Canvas { w, h, color: vec![0xFF00_0000; (w * h) as usize], depth: vec![0; (w * h) as usize] }
    }

    /// Sets up and rasterises one triangle over the whole canvas.
    fn tri(&mut self, a: TVert, b: TVert, c: TVert, mode: u32, cull: u32) -> bool {
        let s = Setup { width: self.w, height: self.h, mode, cull, tex: core::ptr::null(), lod_bias: 0 };
        let mut tris: Vec<Tri> = Vec::new();
        // SAFETY: untextured.
        if !unsafe { pipeline::setup_tri(&s, &a, &b, &c, &mut tris) } {
            return false;
        }
        let target = Target {
            color: self.color.as_mut_ptr(),
            depth: self.depth.as_mut_ptr(),
            stride: self.w,
            width: self.w,
            height: self.h,
        };
        // SAFETY: the target's buffers match its size; untextured.
        unsafe { pipeline::raster(&target, 0, 0, self.w, self.h, &tris, &[0], &mut pipeline::RasterStats::default()) };
        true
    }

    fn at(&self, x: i32, y: i32) -> u32 {
        self.color[(y * self.w + x) as usize] & 0x00FF_FFFF
    }
}

const ADD: u32 = pipeline::M_ADD;
const OPAQUE_Z: u32 = pipeline::M_OPAQUE | pipeline::M_ZTEST | pipeline::M_ZWRITE;

// -- Rasterisation -------------------------------------------------------------

#[test]
fn top_left_rule_square_through_pixel_centres() {
    // A square whose edges pass exactly through pixel centres: left and top
    // edges are included, right and bottom excluded -> exactly 4x4 pixels.
    let mut cv = Canvas::new(10, 10);
    let red = 0x0040_0000;
    let (a, b, c, d) = (sv(2.5, 2.5, 100, red), sv(6.5, 2.5, 100, red), sv(6.5, 6.5, 100, red), sv(2.5, 6.5, 100, red));
    // Counter-clockwise on screen (y down) is clockwise in NDC: draw both
    // windings without culling.
    assert!(cv.tri(a, d, c, ADD, pipeline::CULL_NONE));
    assert!(cv.tri(a, c, b, ADD, pipeline::CULL_NONE));
    for y in 0..10 {
        for x in 0..10 {
            let inside = (2..6).contains(&x) && (2..6).contains(&y);
            assert_eq!(cv.at(x, y), if inside { red } else { 0 }, "pixel {x},{y}");
        }
    }
}

#[test]
fn shared_edges_cover_every_pixel_exactly_once() {
    // A fan of triangles around an off-grid centre with arbitrary subpixel
    // vertices: additive drawing reveals gaps (0) and overlaps (2x).
    let mut cv = Canvas::new(64, 64);
    let one = 0x0000_0020;
    let center = (31.3, 30.7);
    let ring: Vec<(f32, f32)> = (0..17)
        .map(|i| {
            let a = i as f32 / 17.0 * core::f32::consts::TAU;
            (center.0 + 60.0 * vmath::FloatExt::cos(a), center.1 + 60.0 * vmath::FloatExt::sin(a))
        })
        .collect();
    for i in 0..17 {
        let (p, q) = (ring[i], ring[(i + 1) % 17]);
        cv.tri(sv(center.0, center.1, 1, one), sv(p.0, p.1, 1, one), sv(q.0, q.1, 1, one), ADD, pipeline::CULL_NONE);
    }
    for y in 0..64 {
        for x in 0..64 {
            assert_eq!(cv.at(x, y), one, "pixel {x},{y} covered {} times", cv.at(x, y) / one);
        }
    }
}

#[test]
fn back_faces_are_culled() {
    let mut cv = Canvas::new(8, 8);
    let (a, b, c) = (sv(1.0, 1.0, 1, 0xFFFFFF), sv(7.0, 1.0, 1, 0xFFFFFF), sv(1.0, 7.0, 1, 0xFFFFFF));
    // As seen on screen, a -> c -> b runs counter-clockwise: a front face.
    assert!(cv.tri(a, c, b, OPAQUE_Z, pipeline::CULL_BACK));
    assert!(!cv.tri(a, b, c, OPAQUE_Z, pipeline::CULL_BACK));
    assert!(!cv.tri(a, c, b, OPAQUE_Z, pipeline::CULL_FRONT));
    assert!(cv.tri(a, b, c, OPAQUE_Z, pipeline::CULL_FRONT));
}

#[test]
fn depth_test_keeps_the_nearest_surface() {
    for order in [false, true] {
        let mut cv = Canvas::new(16, 16);
        let near = [sv(0.0, 0.0, 2000, 0xFF0000), sv(16.0, 0.0, 2000, 0xFF0000), sv(0.0, 16.0, 2000, 0xFF0000)];
        let far = [sv(0.0, 0.0, 1000, 0x00FF00), sv(16.0, 0.0, 1000, 0x00FF00), sv(0.0, 16.0, 1000, 0x00FF00)];
        let (first, second) = if order { (near, far) } else { (far, near) };
        cv.tri(first[0], first[1], first[2], OPAQUE_Z, pipeline::CULL_NONE);
        cv.tri(second[0], second[1], second[2], OPAQUE_Z, pipeline::CULL_NONE);
        assert_eq!(cv.at(3, 3), 0xFF0000, "order {order}");
    }
}

#[test]
fn depth_interpolates_across_an_intersection() {
    // Two triangles crossing each other in depth: each wins on its side.
    let mut cv = Canvas::new(32, 8);
    let r = |x: f32, y: f32, q: i32| sv(x, y, q, 0xFF0000);
    let g = |x: f32, y: f32, q: i32| sv(x, y, q, 0x00FF00);
    cv.tri(r(0.0, 0.0, 1000), r(32.0, 0.0, 3000), r(0.0, 8.0, 1000), OPAQUE_Z, pipeline::CULL_NONE);
    cv.tri(r(32.0, 0.0, 3000), r(32.0, 8.0, 3000), r(0.0, 8.0, 1000), OPAQUE_Z, pipeline::CULL_NONE);
    cv.tri(g(0.0, 0.0, 3000), g(32.0, 0.0, 1000), g(0.0, 8.0, 3000), OPAQUE_Z, pipeline::CULL_NONE);
    cv.tri(g(32.0, 0.0, 1000), g(32.0, 8.0, 1000), g(0.0, 8.0, 3000), OPAQUE_Z, pipeline::CULL_NONE);
    assert_eq!(cv.at(4, 4), 0x00FF00);
    assert_eq!(cv.at(28, 4), 0xFF0000);
}

#[test]
fn gouraud_colours_interpolate() {
    let mut cv = Canvas::new(64, 4);
    let a = sv(0.0, 0.0, 1, 0x000000);
    let b = sv(64.0, 0.0, 1, 0xFF0000);
    let c = sv(64.0, 4.0, 1, 0xFF0000);
    let d = sv(0.0, 4.0, 1, 0x000000);
    cv.tri(a, b, c, pipeline::M_OPAQUE, pipeline::CULL_NONE);
    cv.tri(a, c, d, pipeline::M_OPAQUE, pipeline::CULL_NONE);
    let mid = (cv.at(32, 2) >> 16) as i32;
    assert!((mid - 128).abs() <= 3, "mid {mid}");
    assert!(cv.at(1, 2) >> 16 <= 8 && cv.at(62, 2) >> 16 >= 247);
}

// -- Clipping ----------------------------------------------------------------

fn test_env() -> Environment {
    Environment {
        background: Background::Solid(0xFF00_0000),
        sun_direction: Vec3::new(0.3, 1.0, 0.2),
        ..Environment::default()
    }
}

#[test]
fn ground_plane_through_the_near_plane_and_guard_band() {
    // A huge plane under the camera reaches behind it and far beyond the
    // guard band: everything below the horizon must be covered, nothing above.
    let mut r = Renderer::new(160, 100, ThreadPool::single());
    let plane = shapes::plane(20000.0, 20000.0, 1, 1, 1.0, 0xFFFFFFFF).build();
    let cam = Camera {
        position: Vec3::new(0.0, 2.0, 0.0),
        forward: Vec3::NEG_Z,
        up: Vec3::Y,
        fov_y: 1.0,
        near: 0.1,
        far: 5000.0,
    };
    let mut f = r.frame(&cam, &test_env());
    f.draw(&plane, &Mat4::IDENTITY, &Material::unlit(0xFF80_8080).without_fog());
    f.finish();
    assert!(r.stats().clipped > 0, "the plane must be clipped");
    let px = r.pixels();
    for y in 0..100 {
        for x in 0..160 {
            let p = px[y * 160 + x] & 0xFFFFFF;
            if y >= 52 {
                assert_eq!(p, 0x808080, "below the horizon at {x},{y}");
            } else if y <= 47 {
                assert_eq!(p, 0, "above the horizon at {x},{y}");
            }
        }
    }
}

#[test]
fn triangle_behind_the_camera_is_invisible() {
    let mut r = Renderer::new(64, 64, ThreadPool::single());
    let quad = shapes::plane(10.0, 10.0, 1, 1, 1.0, 0xFFFFFFFF).build();
    let cam = Camera::look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 0.0, 0.0), Vec3::Y);
    let behind = Mat4::from_rotation_translation(Quat::from_rotation_x(vmath::FRAC_PI_2), Vec3::new(0.0, 0.0, 10.0));
    let mut f = r.frame(&cam, &test_env());
    f.draw(&quad, &behind, &Material::unlit(0xFFFF_FFFF).double_sided().without_fog());
    f.finish();
    assert!(r.pixels().iter().all(|&p| p & 0xFFFFFF == 0));
}

/// Viewport and clipping parameters for a `w` x `h` target: near plane at
/// `near`, guard band at 8x the half viewport.
fn clip_xform(w: i32, h: i32, near: f32) -> pipeline::Xform {
    let (half_w, half_h) = (w * 8, h * 8); // 28.4
    pipeline::Xform {
        near_w: crate::fixed::fx16(near),
        far_w: crate::fixed::fx16(1000.0),
        guard: 8 << 16,
        vp_x: half_w,
        vp_y: half_h,
        vp_sx: half_w,
        vp_sy: half_h,
        q_scale: (crate::fixed::fx16(near) as i64) << 30,
        ..pipeline::Xform::default()
    }
}

/// A clip-space vertex (white in the additive colour) with texture `u`,
/// projected to get its outcode and screen position.
fn clip_vertex(xf: &pipeline::Xform, x: f32, y: f32, w: f32, u: f32) -> TVert {
    let f = crate::fixed::fx16;
    let mut v = TVert {
        cx: f(x),
        cy: f(y),
        cw: f(w),
        sx: 0,
        sy: 0,
        q: 0,
        u: f(u),
        v: 0,
        mul: [0, 0, 0, 256],
        add: [0xFF00; 3],
        outcode: 0,
    };
    pipeline::project(xf, &mut v);
    v
}

/// Twice the signed screen area of a projected triangle.
fn screen_area(t: &[TVert; 3]) -> i64 {
    let (a, b, c) = (&t[0], &t[1], &t[2]);
    (b.sx - a.sx) as i64 * (c.sy - a.sy) as i64 - (c.sx - a.sx) as i64 * (b.sy - a.sy) as i64
}

#[test]
fn near_plane_clipping_keeps_the_visible_quad() {
    let xf = clip_xform(64, 64, 0.5);
    let a = clip_vertex(&xf, -1.0, -1.0, 2.0, 0.0);
    let b = clip_vertex(&xf, 1.0, -1.0, 2.0, 0.0);
    let c = clip_vertex(&xf, 0.0, 1.0, -1.0, 1.0);
    assert!(c.outcode & pipeline::OC_NEAR != 0 && (a.outcode | b.outcode) & pipeline::OC_NEAR == 0);
    let mut out: Vec<[TVert; 3]> = Vec::new();
    crate::clip::clip_triangle(&xf, &a, &b, &c, |p, q, r| out.push([*p, *q, *r]));
    // One vertex behind the near plane leaves a quad: two triangles.
    assert_eq!(out.len(), 2);
    // Homogeneous orientation of the input (positive here); with every w
    // positive and the screen y axis pointing down, the projected pieces
    // must all have negative area (the winding is preserved).
    let det = |p: &TVert, q: &TVert, r: &TVert| {
        let m = |v: &TVert| [v.cx as f64, v.cy as f64, v.cw as f64];
        let (p, q, r) = (m(p), m(q), m(r));
        p[0] * (q[1] * r[2] - q[2] * r[1]) - p[1] * (q[0] * r[2] - q[2] * r[0]) + p[2] * (q[0] * r[1] - q[1] * r[0])
    };
    assert!(det(&a, &b, &c) > 0.0);
    for t in &out {
        assert!(screen_area(t) < 0, "winding flipped");
        for v in t {
            assert!(v.cw >= xf.near_w && v.outcode & pipeline::OC_NEAR == 0);
            assert!(v.q > 0);
        }
    }
    // The new vertices sit on the near plane half-way along b-c and c-a,
    // where u is interpolated to one half.
    let new: Vec<&TVert> = out.iter().flatten().filter(|v| v.cw == xf.near_w).collect();
    assert!(new.len() >= 2);
    for v in new {
        assert!((v.u - 32768).abs() <= 2, "u {}", v.u);
    }
}

#[test]
fn guard_band_clipping_bounds_huge_triangles() {
    let xf = clip_xform(64, 64, 0.1);
    // A sliver reaching 5000 viewports to each side.
    let a = clip_vertex(&xf, -5000.0, -1.0, 1.0, 0.0);
    let b = clip_vertex(&xf, 5000.0, -1.0, 1.0, 0.0);
    let c = clip_vertex(&xf, 0.0, 1.0, 1.0, 0.0);
    assert!(a.outcode & pipeline::OC_GUARD != 0 && b.outcode & pipeline::OC_GUARD != 0);
    let mut out: Vec<[TVert; 3]> = Vec::new();
    crate::clip::clip_triangle(&xf, &a, &b, &c, |p, q, r| out.push([*p, *q, *r]));
    assert!(!out.is_empty());
    // Every vertex is inside the guard band (8 half-viewports = 256 px).
    let limit = 256 * 16 + 16;
    for v in out.iter().flatten() {
        assert!((v.sx - xf.vp_x).abs() <= limit && (v.sy - xf.vp_y).abs() <= limit, "{} {}", v.sx, v.sy);
    }
    // ... and the pieces still cover the whole viewport, without gaps.
    let mut cv = Canvas::new(64, 64);
    for t in &out {
        cv.tri(t[0], t[1], t[2], ADD, pipeline::CULL_NONE);
    }
    for y in 0..64 {
        for x in 0..64 {
            assert_eq!(cv.at(x, y), 0xFF_FFFF, "pixel {x},{y}");
        }
    }
}

#[test]
fn triangles_fully_behind_the_near_plane_vanish() {
    let xf = clip_xform(64, 64, 0.5);
    let a = clip_vertex(&xf, -1.0, -1.0, 0.25, 0.0);
    let b = clip_vertex(&xf, 1.0, -1.0, -2.0, 0.0);
    let c = clip_vertex(&xf, 0.0, 1.0, 0.4, 0.0);
    let mut n = 0;
    crate::clip::clip_triangle(&xf, &a, &b, &c, |_, _, _| n += 1);
    assert_eq!(n, 0);
}

// -- Scene -------------------------------------------------------------------

#[test]
fn cube_faces_lit_and_ordered() {
    let mut r = Renderer::new(128, 128, ThreadPool::single());
    let cube = shapes::cuboid(Vec3::splat(2.0), 0xFFFFFFFF).build();
    let cam = Camera::look_at(Vec3::new(0.0, 0.0, 6.0), Vec3::ZERO, Vec3::Y);
    let mut env = test_env();
    env.sun_direction = Vec3::Z;
    env.sky_ambient = Vec3::ZERO;
    env.ground_ambient = Vec3::ZERO;
    env.sun_color = Vec3::ONE;
    let mut f = r.frame(&cam, &env);
    f.draw(&cube, &Mat4::IDENTITY, &Material::lambert(0xFFFF_FFFF).without_fog());
    f.finish();
    let p = r.pixels()[64 * 128 + 64] & 0xFFFFFF;
    assert!(p >= 0xF0F0F0, "front face fully lit: {p:06x}");
    assert_eq!(r.pixels()[2 * 128 + 2] & 0xFFFFFF, 0, "background");
    assert_eq!(r.stats().draws, 1);
    assert!(r.stats().rasterized >= 2 && r.stats().rasterized <= 6);
}

#[test]
fn frustum_culling_skips_objects_outside() {
    let mut r = Renderer::new(64, 64, ThreadPool::single());
    let s = shapes::sphere(1.0, 8, 6, 0xFFFFFFFF).build();
    let cam = Camera::look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
    let mut f = r.frame(&cam, &test_env());
    f.draw(&s, &Mat4::from_translation(Vec3::new(0.0, 0.0, 20.0)), &Material::default());
    f.draw(&s, &Mat4::from_translation(Vec3::new(100.0, 0.0, 0.0)), &Material::default());
    f.draw(&s, &Mat4::IDENTITY, &Material::default());
    f.finish();
    assert_eq!(r.stats().draws, 3);
    assert_eq!(r.stats().culled, 2);
}

#[test]
fn chunks_cover_all_triangles() {
    let b = shapes::heightmap(40, 40, Vec2::ONE, 4.0, |x, z| x * 0.1 + z * 0.05, |_, _| 0xFFFFFFFF);
    let m = b.clone().build();
    let total: u32 = m.chunks.iter().map(|c| c.tri_count).sum();
    assert_eq!(total as usize, m.triangle_count());
    for c in &m.chunks {
        assert!(c.vcount as usize <= crate::mesh::CHUNK_VERTS);
        for t in c.tri_start..c.tri_start + c.tri_count {
            for k in 0..3 {
                let local = m.local[t as usize * 3 + k];
                assert!(local < c.vcount);
                assert_eq!(local + c.vmin, m.indices()[t as usize * 3 + k]);
            }
        }
    }
}

#[test]
fn textures_have_full_mip_chains() {
    let t = Texture::checker(64, 8, 0xFFFFFFFF, 0xFF000000);
    assert_eq!(t.mip_count(), 7);
    assert_eq!(t.texel(6, 0, 0) & 0xFF, 0x80);
    let odd = Texture::new(3, 5, &[0xFF11_2233; 15]);
    assert_eq!((odd.width(), odd.height()), (4, 8));
    let glow = Texture::radial(32, 0xFFFFFFFF, 0.0);
    assert!(glow.texel(0, 16, 16) >> 24 > 200 && glow.texel(0, 0, 0) >> 24 == 0);
}

#[test]
fn upscaling_preserves_flat_colours() {
    let mut r = Renderer::new(32, 16, ThreadPool::single());
    let cam = Camera::default();
    let mut env = test_env();
    env.background = Background::Solid(0xFF33_6699);
    r.frame(&cam, &env).finish();
    for (w, h) in [(64, 32), (50, 27), (32, 16)] {
        let mut dst = vec![0u32; w * h];
        r.present(&mut dst, w, w, h);
        assert!(dst.iter().all(|&p| p == 0xFF33_6699), "{w}x{h}");
    }
}

#[test]
fn darkening_scales_colours_and_keeps_alpha() {
    let mut r = Renderer::new(16, 8, ThreadPool::single());
    let mut env = test_env();
    env.background = Background::Solid(0xFF80_C040);
    for (amount, want) in [(0u8, 0xFF80_C040u32), (0x80, 0xFF40_6020), (0xFF, 0xFF00_0000)] {
        r.frame(&Camera::default(), &env).finish();
        r.darken(amount);
        for &p in r.pixels() {
            assert_eq!(p >> 24, 0xFF, "alpha kept");
            for s in [0, 8, 16] {
                let (g, w) = ((p >> s) & 0xFF, (want >> s) & 0xFF);
                assert!(g.abs_diff(w) <= 1, "darken {amount:#x}: {p:08x} vs {want:08x}");
            }
        }
    }
}

/// Deterministic pseudo-random opaque pixels.
fn noise_image(w: usize, h: usize, seed: u32) -> Vec<u32> {
    let mut s = seed | 1;
    (0..w * h)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s | 0xFF00_0000
        })
        .collect()
}

/// Reference bilinear sample (pixel centres aligned, edges clamped) of the
/// `dw` x `dh` scaling of `src`, per channel (r, g, b).
fn bilinear_reference(src: &[u32], sw: usize, sh: usize, dw: usize, dh: usize, x: usize, y: usize) -> [f32; 3] {
    let fx = ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
    let fy = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let ch = |p: u32, k: u32| ((p >> k) & 255) as f32;
    let mut out = [0.0; 3];
    for (i, k) in [16, 8, 0].into_iter().enumerate() {
        let top = ch(src[y0 * sw + x0], k) * (1.0 - tx) + ch(src[y0 * sw + x1], k) * tx;
        let bottom = ch(src[y1 * sw + x0], k) * (1.0 - tx) + ch(src[y1 * sw + x1], k) * tx;
        out[i] = top * (1.0 - ty) + bottom * ty;
    }
    out
}

fn assert_close(got: u32, want: [f32; 3], tolerance: f32, what: &str) {
    assert_eq!(got >> 24, 255, "{what}: opaque");
    for (i, k) in [16, 8, 0].into_iter().enumerate() {
        let g = ((got >> k) & 255) as f32;
        assert!((g - want[i]).abs() <= tolerance, "{what}: {got:08x} vs {want:?}");
    }
}

#[test]
fn general_upscaler_matches_reference_bilinear() {
    let (sw, sh) = (13, 9);
    let src = noise_image(sw, sh, 0x1234_5678);
    for (dw, dh) in [(29, 17), (21, 20), (13, 9), (40, 31), (16, 9)] {
        let mut dst = vec![0u32; dw * dh];
        // In two bands, as presenting splits the work between threads.
        let split = dh as i32 / 2;
        for (y0, y1) in [(0, split), (split, dh as i32)] {
            // SAFETY: the destination holds dw*dh pixels.
            unsafe {
                pipeline::upscale(
                    &src,
                    sw as i32,
                    sh as i32,
                    sw as i32,
                    dst.as_mut_ptr(),
                    dw as i32,
                    dh as i32,
                    dw as i32,
                    y0,
                    y1,
                )
            };
        }
        for y in 0..dh {
            for x in 0..dw {
                let want = bilinear_reference(&src, sw, sh, dw, dh, x, y);
                assert_close(dst[y * dw + x], want, 4.0, &format!("{dw}x{dh} at {x},{y}"));
            }
        }
    }
}

type Upscale2xFn = unsafe fn(*const u32, i32, i32, i32, *mut u32, i32, i32, i32);

#[test]
fn fast_2x_filters_agree_and_respect_bands() {
    // An odd width exercises the SIMD loop's scalar tail.
    let (sw, sh) = (37, 11);
    let (dw, dh) = (2 * sw, 2 * sh);
    let src = noise_image(sw, sh, 99);
    let run = |f: Upscale2xFn, bands: &[(i32, i32)]| {
        let mut dst = vec![0u32; dw * dh];
        for &(y0, y1) in bands {
            // SAFETY: the buffers hold sw*sh and (2sw)*(2sh) pixels.
            unsafe { f(src.as_ptr(), sw as i32, sh as i32, sw as i32, dst.as_mut_ptr(), dw as i32, y0, y1) };
        }
        dst
    };
    let whole = [(0, dh as i32)];
    let split = [(0, 5), (5, 6), (6, 13), (13, dh as i32)];
    let simd = run(pipeline::upscale2x_fast, &whole);
    let swar = run(pipeline::upscale2x_swar, &whole);
    assert!(simd == swar, "SSE2 and scalar fast filters differ");
    assert!(run(pipeline::upscale2x_fast, &split) == simd);
    assert!(run(pipeline::upscale2x_swar, &split) == swar);
    // Source pixels land on the even output pixels; the others average
    // their neighbours (sampling at the source pixel corners).
    for y in 0..sh {
        for x in 0..sw {
            assert_eq!(swar[2 * y * dw + 2 * x], src[y * sw + x]);
        }
    }
    // The smooth filter is pixel-centre aligned bilinear, like the general
    // scaler at exactly 2x.
    let smooth = run(pipeline::upscale2x, &split);
    assert!(run(pipeline::upscale2x, &whole) == smooth);
    for y in 0..dh {
        for x in 0..dw {
            let want = bilinear_reference(&src, sw, sh, dw, dh, x, y);
            assert_close(smooth[y * dw + x], want, 3.0, &format!("smooth at {x},{y}"));
        }
    }
}

#[test]
fn bilinear_magnification_is_smooth() {
    // A 2x2 texture (black column, white column) magnified across a quad:
    // nearest sampling gives two flat halves, bilinear a ramp between the
    // texel centres.
    let centre_row = |bilinear: bool| -> Vec<u32> {
        let mut r = Renderer::new(64, 64, ThreadPool::single());
        let tex = r.add_texture(Texture::new(2, 2, &[0xFF00_0000, 0xFFFF_FFFF, 0xFF00_0000, 0xFFFF_FFFF]));
        let cam = Camera::look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        let n = Vec3::Z;
        let verts = [
            Vertex::new(Vec3::new(-2.0, -2.0, 0.0), n, Vec2::new(0.0, 1.0), 0xFFFF_FFFF),
            Vertex::new(Vec3::new(2.0, -2.0, 0.0), n, Vec2::new(1.0, 1.0), 0xFFFF_FFFF),
            Vertex::new(Vec3::new(2.0, 2.0, 0.0), n, Vec2::new(1.0, 0.0), 0xFFFF_FFFF),
            Vertex::new(Vec3::new(-2.0, 2.0, 0.0), n, Vec2::new(0.0, 0.0), 0xFFFF_FFFF),
        ];
        let mut m = Material::unlit(0xFFFF_FFFF).with_texture(tex).without_fog();
        if bilinear {
            m = m.with_bilinear();
        }
        let mut f = r.frame(&cam, &test_env());
        f.draw_triangles(&verts, &[0, 1, 2, 0, 2, 3], &m);
        f.finish();
        r.pixels()[32 * 64..33 * 64].iter().map(|p| p & 0xFF).collect()
    };
    // The quad spans x = 8.6 .. 55.4; the texel centres (u = 1/4 and 3/4)
    // fall at x = 20.3 and 43.7.
    let nearest = centre_row(false);
    assert!(nearest[10..54].iter().all(|&v| v <= 2 || v >= 253), "{nearest:?}");
    let smooth = centre_row(true);
    let ramp = &smooth[21..44];
    assert!(ramp.windows(2).all(|w| w[1] + 1 >= w[0]), "not monotonic: {ramp:?}");
    let mid: Vec<u32> = ramp.iter().copied().filter(|&v| v > 8 && v < 247).collect();
    assert!(mid.len() >= 15, "{smooth:?}");
    assert!(ramp[0] < 40 && ramp[ramp.len() - 1] > 215, "{ramp:?}");
}

#[test]
fn billboards_filling_the_view_fade_out() {
    // Additive billboards at depth 5 (the view is 2 * 5 * tan(0.5) = 5.46
    // units tall): small ones draw, ones taller than the view are skipped,
    // and ones in between are dimmed.
    let draw = |size: f32| -> u32 {
        let mut r = Renderer::new(64, 64, ThreadPool::single());
        let cam = Camera::look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        let mut f = r.frame(&cam, &test_env());
        let m = Material::glow(0xFFFF_FFFF).without_fog();
        f.billboards(&m, &[Billboard::new(Vec3::ZERO, size, 0xFF80_8080)]);
        f.finish();
        r.pixels()[32 * 64 + 32] & 0xFF
    };
    let view = 2.0 * 5.0 * vmath::FloatExt::tan(0.5f32);
    let small = draw(0.2 * view);
    assert!((small as i32 - 0x80).abs() <= 2, "small {small}");
    assert_eq!(draw(BILLBOARD_FADE_END * view * 1.05), 0);
    let mid = draw((BILLBOARD_FADE_START + BILLBOARD_FADE_END) * 0.5 * view);
    assert!(mid > 0x20 && mid < 0x60, "half faded {mid}");
}

#[test]
fn partition_balances_weights() {
    let mut out = Vec::new();
    crate::pool::partition(&[10, 10, 10, 10, 10, 10, 10, 10], 4, &mut out);
    assert_eq!(out, vec![(0, 2), (2, 4), (4, 6), (6, 8)]);
    crate::pool::partition(&[100, 1, 1, 1], 2, &mut out);
    assert_eq!(out.last().unwrap().1, 4);
    crate::pool::partition(&[], 3, &mut out);
    assert!(out.iter().all(|&(a, b)| a == b));
}

#[test]
fn render_test_scene() {
    let (w, h) = (480, 270);
    let mut r = Renderer::new(w, h, ThreadPool::single());
    let checker = r.add_texture(Texture::checker(256, 8, 0xFFE8E8E8, 0xFF4A6A8A));
    let noise = r.add_texture(Texture::noise(128, 7, 8, 4, 0xFF5A8A3A, 0xFF2A4A1A));
    let glow = r.add_texture(Texture::radial(64, 0xFFFFFFFF, 0.2));
    let ground = shapes::plane(200.0, 200.0, 20, 20, 4.0, 0xFFFFFFFF).build();
    let cube = shapes::cuboid(Vec3::splat(2.0), 0xFFFFFFFF).build();
    let sphere = shapes::sphere(1.2, 24, 16, 0xFFFFFFFF).build();
    let cone = shapes::cylinder(1.0, 0.0, 2.5, 16, true, 0xFFFFFFFF).flat_shaded().build();
    let rock = {
        let mut b = shapes::icosphere(1.0, 2, 0xFF8A7A6A);
        for v in &mut b.vertices {
            let n = vmath::Noise::new(3).fbm3(v.position.x * 2.0, v.position.y * 2.0, v.position.z * 2.0, 3, 2.0, 0.5);
            v.position *= 1.0 + 0.25 * n;
        }
        b.flat_shaded().build()
    };
    let torus = shapes::torus(1.0, 0.3, 24, 12, 0xFFFFFFFF).build();
    let mut env = Environment {
        sun_direction: Vec3::new(-0.5, 0.8, 0.4),
        sun_color: Vec3::new(1.0, 0.95, 0.85),
        sky_ambient: Vec3::new(0.35, 0.42, 0.55),
        ground_ambient: Vec3::new(0.18, 0.16, 0.12),
        point_lights: vec![PointLight {
            position: Vec3::new(3.0, 1.0, 2.0),
            color: Vec3::new(1.5, 0.6, 0.2),
            radius: 5.0,
        }],
        fog: Some(Fog { color: Vec3::new(0.62, 0.72, 0.85), start: 10.0, end: 90.0, max: 1.0 }),
        background: Background::Sky(vec![(-1.0, 0xFF9DB7D5), (0.0, 0xFF9DB7D5), (0.3, 0xFF5A8AC8), (1.0, 0xFF2A5AA8)]),
    };
    env.sun_direction = env.sun_direction.normalize();
    let cam = Camera {
        position: Vec3::new(0.0, 3.0, 9.0),
        forward: Vec3::new(0.0, -0.25, -1.0).normalize(),
        fov_y: 1.0,
        ..Camera::default()
    };
    let mut f = r.frame(&cam, &env);
    f.draw(&ground, &Mat4::IDENTITY, &Material::lambert(0xFFFFFFFF).with_texture(checker));
    f.draw(
        &cube,
        &Mat4::from_rotation_translation(Quat::from_rotation_y(0.6), Vec3::new(-3.0, 1.0, 0.0)),
        &Material::lambert(0xFFE05030),
    );
    f.draw(
        &sphere,
        &Mat4::from_translation(Vec3::new(0.0, 1.2, -1.0)),
        &Material::phong(0xFF3070E0, 32.0, Vec3::splat(0.8)),
    );
    f.draw(&cone, &Mat4::from_translation(Vec3::new(3.0, 0.0, -1.5)), &Material::lambert(0xFF40C060));
    f.draw(
        &rock,
        &Mat4::from_scale_rotation_translation(Vec3::splat(1.4), Quat::IDENTITY, Vec3::new(-1.0, 0.8, -8.0)),
        &Material::lambert(0xFFFFFFFF),
    );
    f.draw(
        &torus,
        &Mat4::from_rotation_translation(Quat::from_rotation_x(1.0), Vec3::new(5.5, 1.5, -6.0)),
        &Material::lambert(0xFFFFFFFF).with_texture(noise),
    );
    for i in 0..12 {
        let z = -10.0 - i as f32 * 12.0;
        f.draw(&cube, &Mat4::from_translation(Vec3::new(-6.0, 1.0, z)), &Material::lambert(0xFFC0A060));
        f.draw(&cube, &Mat4::from_translation(Vec3::new(6.0, 1.0, z)), &Material::lambert(0xFF60A0C0));
    }
    let glows: Vec<Billboard> =
        (0..5).map(|i| Billboard::new(Vec3::new(-2.0 + i as f32, 3.2, 1.0), 0.9, 0xFFFFA040)).collect();
    f.billboards(&Material::glow(0xFFFFFFFF).with_texture(glow), &glows);
    let smoke: Vec<Billboard> =
        (0..4).map(|i| Billboard::new(Vec3::new(2.0, 1.0 + i as f32 * 0.5, 2.0), 1.4, 0x80808080)).collect();
    f.billboards(&Material::unlit(0xFFFFFFFF).with_texture(glow).with_blend(Blend::Alpha), &smoke);
    f.finish();
    let st = r.stats();
    assert!(st.rasterized > 100, "{st:?}");
    let px = r.pixels();
    // The sky is at the top, the sphere in the middle is blue-ish.
    let top = px[2 * w + w / 2];
    assert!(top & 0xFF > (top >> 16) & 0xFF, "sky {top:08x}");
    save_png("v3d-scene.png", w, h, px);
    let scene_hash = fnv1a(px);
    let mut big = vec![0u32; w * 2 * h * 2];
    r.present(&mut big, w * 2, w * 2, h * 2);
    save_png("v3d-scene-2x.png", w * 2, h * 2, &big);
    // The exact image. Rendering is deterministic (one thread), so any change
    // to these hashes is a change to the renderer's output: check the PNGs
    // ($V3D_TEST_OUT) and update the hashes if it is intended.
    assert_eq!((scene_hash, fnv1a(&big)), (0x62b4_ca3c_6147_8e8e, 0x0ba1_525e_6c4c_d760), "rendered image changed");
}

/// FNV-1a hash of an image.
fn fnv1a(pixels: &[u32]) -> u64 {
    pixels
        .iter()
        .flat_map(|p| p.to_le_bytes())
        .fold(0xCBF2_9CE4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3))
}
