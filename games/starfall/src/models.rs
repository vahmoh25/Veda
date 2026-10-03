//! Procedural models and textures: the player's fighter, three enemy
//! types, asteroids, pickups, the nebula sky and a planet.
//!
//! Ships fly towards -Z: model space has the nose at -Z, +Y up.

use alloc::vec::Vec;

use v3d::{Mesh, MeshBuilder, Texture, shapes};
use vmath::{FloatExt, Mat4, Noise, Quat, Rng, Vec2, Vec3};

/// Rotates a builder made along +Y (lathe/cylinder axis) to point along -Z.
fn y_to_neg_z(b: &mut MeshBuilder) {
    b.transform(&Mat4::from_rotation_x(-vmath::FRAC_PI_2));
}

/// A flat wing outline (in the XZ plane, given as (x, z) points) with
/// some thickness.
fn wing(points: &[(f32, f32)], thickness: f32, color: u32) -> MeshBuilder {
    // Extrude in XY (x, y = -z) along Z, then lay it flat: Y -> -Z, Z -> Y.
    let poly: Vec<Vec2> = points.iter().map(|&(x, z)| Vec2::new(x, -z)).collect();
    let mut b = shapes::extrude(&poly, thickness, color);
    b.transform(&Mat4::from_rotation_x(-vmath::FRAC_PI_2));
    b
}

/// The player's fighter.
pub fn player_ship() -> Mesh {
    let mut b = MeshBuilder::new();
    let hull = 0xFFE4E8EE;
    let accent = 0xFF2E7CF0;
    let dark = 0xFF2A2E36;
    let mut body = shapes::lathe(
        &[
            Vec2::new(0.0, -2.0),
            Vec2::new(0.42, -1.9),
            Vec2::new(0.62, -1.2),
            Vec2::new(0.66, 0.4),
            Vec2::new(0.42, 1.9),
            Vec2::new(0.12, 3.1),
            Vec2::new(0.0, 3.3),
        ],
        8,
        hull,
    );
    y_to_neg_z(&mut body);
    b.append(&body);
    // Cockpit canopy.
    let mut canopy = shapes::icosphere(0.5, 1, 0xFF1A2A40);
    canopy.transform(&Mat4::from_scale_rotation_translation(
        Vec3::new(0.75, 0.6, 1.5),
        Quat::IDENTITY,
        Vec3::new(0.0, 0.42, -1.0),
    ));
    b.append(&canopy);
    // Swept wings with accent tips.
    for side in [-1.0f32, 1.0] {
        let pts: Vec<(f32, f32)> =
            [(0.4, -0.6), (3.4, 1.0), (3.4, 1.6), (0.4, 1.8)].iter().map(|&(x, z)| (x * side, z)).collect();
        let pts = if side < 0.0 { pts.into_iter().rev().collect::<Vec<_>>() } else { pts };
        let mut w = wing(&pts, 0.12, hull);
        w.transform(&Mat4::from_translation(Vec3::new(0.0, -0.1, 0.0)));
        b.append(&w);
        let tip = shapes::cuboid(Vec3::new(0.18, 0.2, 1.3), accent);
        b.append_transformed(&tip, &Mat4::from_translation(Vec3::new(3.45 * side, -0.05, 1.2)));
        // Engine nacelle.
        let mut pod = shapes::cylinder(0.3, 0.36, 2.2, 6, true, dark);
        pod.transform(
            &(Mat4::from_translation(Vec3::new(1.2 * side, -0.05, 3.4)) * Mat4::from_rotation_x(-vmath::FRAC_PI_2)),
        );
        b.append(&pod);
        // Fin.
        let mut fin = wing(&[(0.0, 0.6), (0.0, 1.9), (0.9, 2.0)], 0.08, accent);
        fin.transform(
            &(Mat4::from_translation(Vec3::new(0.45 * side, 0.35, 0.0))
                * Mat4::from_rotation_z(vmath::FRAC_PI_2 - 0.35 * side)),
        );
        b.append(&fin);
    }
    // Accent stripe on the nose.
    let stripe = shapes::cuboid(Vec3::new(0.3, 0.05, 1.4), accent);
    b.append_transformed(&stripe, &Mat4::from_translation(Vec3::new(0.0, 0.55, 0.6)));
    b.flat_shaded().build()
}

/// Engine exhaust positions of the player's ship (model space).
pub const PLAYER_ENGINES: [Vec3; 2] = [Vec3::new(-1.2, -0.05, 3.45), Vec3::new(1.2, -0.05, 3.45)];

/// Small, fast enemy fighter (red).
pub fn dart() -> Mesh {
    let mut b = MeshBuilder::new();
    let red = 0xFFD8342E;
    let dark = 0xFF3A2024;
    let mut body = shapes::cylinder(0.55, 0.0, 3.0, 5, true, red);
    y_to_neg_z(&mut body);
    body.transform(&Mat4::from_translation(Vec3::new(0.0, 0.0, 1.0)));
    b.append(&body);
    for side in [-1.0f32, 1.0] {
        let pts: Vec<(f32, f32)> =
            [(0.2, -0.4), (2.2, 1.4), (2.0, 1.8), (0.2, 1.2)].iter().map(|&(x, z)| (x * side, z)).collect();
        let pts = if side < 0.0 { pts.into_iter().rev().collect::<Vec<_>>() } else { pts };
        b.append(&wing(&pts, 0.1, dark));
    }
    let mut cockpit = shapes::icosphere(0.3, 0, 0xFFFFB040);
    cockpit.transform(&Mat4::from_translation(Vec3::new(0.0, 0.3, -0.2)));
    b.append(&cockpit);
    b.transform(&Mat4::from_scale(Vec3::splat(1.5)));
    b.flat_shaded().build()
}

/// Hovering saucer (purple with a glowing dome).
pub fn saucer() -> Mesh {
    let mut b = shapes::lathe(
        &[
            Vec2::new(0.0, -0.5),
            Vec2::new(1.2, -0.45),
            Vec2::new(2.4, -0.1),
            Vec2::new(2.5, 0.05),
            Vec2::new(1.6, 0.4),
            Vec2::new(0.0, 0.5),
        ],
        12,
        0xFF7A4AC0,
    );
    let mut dome = shapes::icosphere(0.9, 1, 0xFF60F0FF);
    dome.transform(&Mat4::from_scale_rotation_translation(
        Vec3::new(1.0, 0.7, 1.0),
        Quat::IDENTITY,
        Vec3::new(0.0, 0.45, 0.0),
    ));
    b.append(&dome);
    let ring = shapes::torus(2.45, 0.08, 16, 4, 0xFFFFE070);
    b.append(&ring);
    b.transform(&Mat4::from_scale(Vec3::splat(1.3)));
    b.flat_shaded().build()
}

/// Heavy gunship (orange and grey).
pub fn heavy() -> Mesh {
    let mut b = MeshBuilder::new();
    let grey = 0xFF8A9098;
    let orange = 0xFFE87A2A;
    b.append_transformed(&shapes::cuboid(Vec3::new(2.4, 1.2, 5.0), grey), &Mat4::IDENTITY);
    b.append_transformed(
        &shapes::cuboid(Vec3::new(1.6, 0.8, 1.8), orange),
        &Mat4::from_translation(Vec3::new(0.0, 0.8, -0.6)),
    );
    let mut nose = shapes::cylinder(0.9, 0.2, 1.8, 6, false, orange);
    y_to_neg_z(&mut nose);
    nose.transform(&Mat4::from_translation(Vec3::new(0.0, 0.0, -2.5)));
    b.append(&nose);
    for side in [-1.0f32, 1.0] {
        b.append_transformed(
            &shapes::cuboid(Vec3::new(1.0, 0.9, 3.6), orange),
            &Mat4::from_translation(Vec3::new(2.0 * side, -0.1, 0.4)),
        );
        let mut gun = shapes::cylinder(0.15, 0.15, 1.6, 5, true, 0xFF30343A);
        y_to_neg_z(&mut gun);
        gun.transform(&Mat4::from_translation(Vec3::new(2.0 * side, -0.1, -1.4)));
        b.append(&gun);
        let pts: Vec<(f32, f32)> =
            [(1.2, -0.5), (3.6, 1.2), (3.6, 2.2), (1.2, 2.2)].iter().map(|&(x, z)| (x * side, z)).collect();
        let pts = if side < 0.0 { pts.into_iter().rev().collect::<Vec<_>>() } else { pts };
        b.append(&wing(&pts, 0.15, grey));
    }
    b.transform(&Mat4::from_scale(Vec3::splat(1.35)));
    b.flat_shaded().build()
}

/// A lumpy asteroid with vertex-colour variation.
pub fn asteroid(seed: u64) -> Mesh {
    let mut b = shapes::icosphere(1.0, 1, 0xFFFFFFFF);
    let n = Noise::new(seed);
    let mut rng = Rng::new(seed);
    let stretch = Vec3::new(rng.range_f32(0.8, 1.2), rng.range_f32(0.7, 1.0), rng.range_f32(0.8, 1.25));
    for v in &mut b.vertices {
        let p = v.position;
        let k = n.fbm3(p.x * 1.6, p.y * 1.6, p.z * 1.6, 3, 2.0, 0.5);
        v.position = p * (1.0 + 0.32 * k) * stretch;
        let shade = (0.5 + 0.5 * n.perlin3(p.x * 3.0 + 9.0, p.y * 3.0, p.z * 3.0)).clamp(0.0, 1.0);
        v.color = v3d::lerp_color(0xFF5E5650, 0xFFA89C8E, (shade * 256.0) as u32);
    }
    b.flat_shaded().build()
}

/// Pickup crystal.
pub fn crystal() -> Mesh {
    let mut b = shapes::cylinder(0.8, 0.0, 1.1, 6, false, 0xFFFFFFFF);
    let mut bottom = shapes::cylinder(0.8, 0.0, 1.1, 6, false, 0xFFFFFFFF);
    bottom.transform(&Mat4::from_rotation_x(vmath::PI));
    b.append(&bottom);
    b.flat_shaded().build()
}

/// A sphere seen from inside (for the sky).
pub fn sky_sphere(radius: f32) -> Mesh {
    let mut b = shapes::sphere(radius, 24, 14, 0xFFFFFFFF);
    b.flip();
    b.build()
}

/// The nebula: dark space with coloured gas clouds and faint stars,
/// tileable in u (longitude).
pub fn nebula_texture(seed: u32) -> Texture {
    let (w, h) = (512u32, 256u32);
    Texture::from_fn(w, h, |x, y| {
        let gas = v3d::texture::fbm_tiled(x, y * 2, w, 4, 5, seed) as f32 / 256.0;
        let gas2 = v3d::texture::fbm_tiled(x + 37, y * 2 + 11, w, 8, 4, seed + 7) as f32 / 256.0;
        // Fade towards the poles.
        let lat = (y as f32 / h as f32 - 0.5) * 2.0;
        let band = (1.0 - lat * lat * 1.4).clamp(0.0, 1.0);
        let a = ((gas - 0.42) * 2.6).clamp(0.0, 1.0) * band;
        let b = ((gas2 - 0.5) * 3.0).clamp(0.0, 1.0) * band;
        let base = (0.03, 0.035, 0.07);
        let magenta = (0.55, 0.16, 0.5);
        let cyan = (0.1, 0.4, 0.6);
        let r = base.0 + magenta.0 * a * 0.6 + cyan.0 * b * 0.5;
        let g = base.1 + magenta.1 * a * 0.6 + cyan.1 * b * 0.5;
        let bl = base.2 + magenta.2 * a * 0.6 + cyan.2 * b * 0.5;
        let mut c = v3d::pack_rgb(Vec3::new(r, g, bl));
        // Sprinkle faint stars.
        let hsh = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed.wrapping_mul(83_492_791)) % 1000;
        if hsh < 6 {
            let k = 120 + hsh * 20;
            c = 0xFF00_0000 | k << 16 | k << 8 | (k + 20).min(255);
        }
        c
    })
}

/// A gas-giant texture with soft bands.
pub fn planet_texture() -> Texture {
    Texture::from_fn(128, 64, |x, y| {
        let n = v3d::texture::fbm_tiled(x, y, 128, 8, 3, 21) as f32 / 256.0;
        let band = ((y as f32 / 64.0 * 9.0 + n * 3.0).sin() * 0.5 + 0.5).clamp(0.0, 1.0);
        let c1 = Vec3::new(0.85, 0.6, 0.38);
        let c2 = Vec3::new(0.55, 0.32, 0.22);
        v3d::pack_rgb(c1.lerp(c2, band))
    })
}

/// A thin ring sprite for shockwaves.
pub fn ring_texture() -> Texture {
    Texture::from_fn(64, 64, |x, y| {
        let dx = (x as f32 + 0.5) / 32.0 - 1.0;
        let dy = (y as f32 + 0.5) / 32.0 - 1.0;
        let r = (dx * dx + dy * dy).sqrt();
        let a = (1.0 - ((r - 0.8) / 0.18).abs()).clamp(0.0, 1.0);
        ((a * a * 255.0) as u32) << 24 | 0x00FF_FFFF
    })
}

/// A soft streak for laser bolts (bright core along the length).
pub fn bolt_texture() -> Texture {
    Texture::from_fn(16, 32, |x, y| {
        let dx = ((x as f32 + 0.5) / 8.0 - 1.0).abs();
        let dy = ((y as f32 + 0.5) / 16.0 - 1.0).abs();
        let a = ((1.0 - dx) * (1.0 - dy * dy)).clamp(0.0, 1.0);
        let core = (1.0 - dx * 3.0).clamp(0.0, 1.0);
        let k = (255.0 * core) as u32;
        ((a * 255.0) as u32) << 24 | (k.max(160)) << 16 | (k.max(160)) << 8 | k.max(160)
    })
}
