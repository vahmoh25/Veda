//! The low-poly car model.
//!
//! Car space: +Z forward, +Y up, +X to the left; the origin is on the ground
//! between the axles.

use alloc::vec::Vec;

use v3d::{Mesh, MeshBuilder, shapes};
use vmath::{Mat4, Vec2, Vec3};

pub const WHEEL_RADIUS: f32 = 0.34;
/// Wheel positions (left/right, front/back) in car space.
pub const WHEELS: [Vec3; 4] = [
    Vec3::new(0.82, WHEEL_RADIUS, 1.33),
    Vec3::new(-0.82, WHEEL_RADIUS, 1.33),
    Vec3::new(0.82, WHEEL_RADIUS, -1.33),
    Vec3::new(-0.82, WHEEL_RADIUS, -1.33),
];

/// Meshes of one car model.
pub struct CarModel {
    pub body: Mesh,
    pub wheel: Mesh,
    pub brake_lights: Mesh,
}

/// Extrudes a side profile given as (z forward, y up) points to `width`
/// across X, centred on x = `x0`.
fn profile(points: &[(f32, f32)], width: f32, x0: f32, color: u32) -> MeshBuilder {
    // Build in XY with x = -z, so that a +90 degree turn about Y maps the
    // extrusion axis to the car's X and the profile to its Z.
    let mut poly: Vec<Vec2> = points.iter().map(|&(z, y)| Vec2::new(-z, y)).collect();
    let area: f32 = (0..poly.len()).map(|i| poly[i].perp_dot(poly[(i + 1) % poly.len()])).sum();
    if area < 0.0 {
        poly.reverse();
    }
    let mut b = shapes::extrude(&poly, width, color);
    b.transform(&(Mat4::from_translation(Vec3::new(x0, 0.0, 0.0)) * Mat4::from_rotation_y(vmath::FRAC_PI_2)));
    b
}

fn quad(b: &mut MeshBuilder, p: [Vec3; 4], color: u32) {
    b.face(p, color);
}

impl CarModel {
    /// A sports car painted `paint` with a `stripe` accent colour.
    pub fn new(paint: u32, stripe: u32) -> CarModel {
        let mut body = MeshBuilder::new();
        // Lower body with wheel arches.
        let lower = [
            (2.15, 0.24),
            (2.2, 0.47),
            (2.02, 0.62),
            (0.78, 0.78),
            (-1.52, 0.84),
            (-2.06, 0.82),
            (-2.18, 0.72),
            (-2.14, 0.26),
            (-1.8, 0.26),
            (-1.68, 0.56),
            (-1.33, 0.7),
            (-0.98, 0.56),
            (-0.88, 0.26),
            (0.88, 0.26),
            (0.98, 0.56),
            (1.33, 0.7),
            (1.68, 0.56),
            (1.8, 0.26),
        ];
        body.append(&profile(&lower, 1.86, 0.0, paint));
        // Cabin.
        let cabin = [(0.8, 0.77), (0.02, 1.16), (-0.86, 1.18), (-1.58, 0.83), (-1.58, 0.77)];
        body.append(&profile(&cabin, 1.52, 0.0, paint));
        // Glass panels, slightly proud of the cabin.
        let glass = 0xFF1E2A36;
        let wz = |z: f32, y: f32, x: f32| Vec3::new(x, y, z);
        // Windshield (normal up and forward).
        quad(
            &mut body,
            [wz(0.74, 0.8, -0.68), wz(0.74, 0.8, 0.68), wz(0.06, 1.15, 0.64), wz(0.06, 1.15, -0.64)],
            glass,
        );
        // Rear window.
        quad(
            &mut body,
            [wz(-1.52, 0.86, 0.66), wz(-1.52, 0.86, -0.66), wz(-0.9, 1.16, -0.62), wz(-0.9, 1.16, 0.62)],
            glass,
        );
        // Side windows.
        for side in [-1.0f32, 1.0] {
            let x = side * 0.77;
            let p = [wz(0.62, 0.83, x), wz(-1.4, 0.86, x), wz(-0.84, 1.13, x), wz(0.02, 1.12, x)];
            if side > 0.0 {
                quad(&mut body, p, glass);
            } else {
                quad(&mut body, [p[1], p[0], p[3], p[2]], glass);
            }
        }
        // Racing stripes over hood, roof and trunk.
        for x in [-0.22f32, 0.22] {
            let s = shapes::cuboid(Vec3::new(0.2, 0.02, 1.2), stripe);
            body.append_transformed(
                &s,
                &(Mat4::from_translation(Vec3::new(x, 0.71, 1.4)) * Mat4::from_rotation_x(0.115)),
            );
            let r = shapes::cuboid(Vec3::new(0.2, 0.02, 0.85), stripe);
            body.append_transformed(&r, &Mat4::from_translation(Vec3::new(x, 1.18, -0.42)));
        }
        // Spoiler.
        let wing = shapes::cuboid(Vec3::new(1.7, 0.05, 0.36), stripe);
        body.append_transformed(&wing, &Mat4::from_translation(Vec3::new(0.0, 1.06, -1.96)));
        for x in [-0.55f32, 0.55] {
            let strut = shapes::cuboid(Vec3::new(0.06, 0.24, 0.16), 0xFF202226);
            body.append_transformed(&strut, &Mat4::from_translation(Vec3::new(x, 0.93, -1.92)));
        }
        // Lights, bumpers and grille.
        for x in [-0.62f32, 0.62] {
            let head = shapes::cuboid(Vec3::new(0.38, 0.09, 0.06), 0xFFFFF6D8);
            body.append_transformed(&head, &Mat4::from_translation(Vec3::new(x, 0.56, 2.17)));
            let tail = shapes::cuboid(Vec3::new(0.42, 0.09, 0.06), 0xFF8A1212);
            body.append_transformed(&tail, &Mat4::from_translation(Vec3::new(x, 0.64, -2.17)));
        }
        let grille = shapes::cuboid(Vec3::new(0.7, 0.12, 0.05), 0xFF18191C);
        body.append_transformed(&grille, &Mat4::from_translation(Vec3::new(0.0, 0.36, 2.19)));
        let diffuser = shapes::cuboid(Vec3::new(1.5, 0.12, 0.06), 0xFF18191C);
        body.append_transformed(&diffuser, &Mat4::from_translation(Vec3::new(0.0, 0.3, -2.15)));
        let skirt = shapes::cuboid(Vec3::new(1.9, 0.1, 1.6), 0xFF202226);
        body.append_transformed(&skirt, &Mat4::from_translation(Vec3::new(0.0, 0.28, 0.0)));

        // Wheel: tyre plus hub, axle along X.
        let mut wheel = shapes::cylinder(WHEEL_RADIUS, WHEEL_RADIUS, 0.28, 10, true, 0xFF1C1C1E);
        let hub = shapes::cylinder(0.2, 0.2, 0.3, 6, true, 0xFFB8BEC8);
        wheel.append(&hub);
        wheel
            .transform(&(Mat4::from_rotation_z(vmath::FRAC_PI_2) * Mat4::from_translation(Vec3::new(0.0, -0.14, 0.0))));
        let wheel = wheel.flat_shaded();

        let mut brake = MeshBuilder::new();
        for x in [-0.62f32, 0.62] {
            let tail = shapes::cuboid(Vec3::new(0.44, 0.11, 0.06), 0xFFFF3030);
            brake.append_transformed(&tail, &Mat4::from_translation(Vec3::new(x, 0.64, -2.18)));
        }
        CarModel { body: body.build(), wheel: wheel.build(), brake_lights: brake.build() }
    }
}
