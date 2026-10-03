//! Procedural mesh builders: boxes, spheres, cylinders and cones, planes,
//! height-map terrain, lathes (surfaces of revolution), extrusions and tori.
//!
//! Every function returns a [`MeshBuilder`] so shapes can be combined,
//! transformed, recoloured or flat-shaded before building the [`Mesh`]
//! (`crate::Mesh`). Shapes are centred on the origin unless noted; front
//! faces point outwards.

use alloc::vec::Vec;

#[allow(unused_imports)] // host tests link std, whose inherent float methods win
use vmath::{FloatExt, Vec2, Vec3};

use crate::mesh::MeshBuilder;

/// An axis-aligned box of the given size, centred on the origin, with one
/// texture repeat per face.
pub fn cuboid(size: Vec3, color: u32) -> MeshBuilder {
    let h = size * 0.5;
    let mut b = MeshBuilder::new();
    let p = |x: f32, y: f32, z: f32| Vec3::new(x * h.x, y * h.y, z * h.z);
    // +X, -X, +Y, -Y, +Z, -Z (counter-clockwise seen from outside).
    b.face([p(1., -1., 1.), p(1., -1., -1.), p(1., 1., -1.), p(1., 1., 1.)], color);
    b.face([p(-1., -1., -1.), p(-1., -1., 1.), p(-1., 1., 1.), p(-1., 1., -1.)], color);
    b.face([p(-1., 1., 1.), p(1., 1., 1.), p(1., 1., -1.), p(-1., 1., -1.)], color);
    b.face([p(-1., -1., -1.), p(1., -1., -1.), p(1., -1., 1.), p(-1., -1., 1.)], color);
    b.face([p(-1., -1., 1.), p(1., -1., 1.), p(1., 1., 1.), p(-1., 1., 1.)], color);
    b.face([p(1., -1., -1.), p(-1., -1., -1.), p(-1., 1., -1.), p(1., 1., -1.)], color);
    b
}

/// A UV sphere with smooth normals (`segments` around, `rings` from pole to
/// pole).
pub fn sphere(radius: f32, segments: u32, rings: u32, color: u32) -> MeshBuilder {
    let (segs, rings) = (segments.max(3), rings.max(2));
    let mut b = MeshBuilder::new();
    for r in 0..=rings {
        let v = r as f32 / rings as f32;
        let phi = v * vmath::PI;
        let (sp, cp) = phi.sin_cos();
        for s in 0..=segs {
            let u = s as f32 / segs as f32;
            let theta = u * vmath::TAU;
            let (st, ct) = theta.sin_cos();
            let n = Vec3::new(sp * ct, cp, -sp * st);
            b.vert(n * radius, n, Vec2::new(u, v), color);
        }
    }
    let row = segs + 1;
    for r in 0..rings {
        for s in 0..segs {
            let (a, c) = (r * row + s, (r + 1) * row + s);
            if r != 0 {
                b.triangle(a, c, a + 1);
            }
            if r != rings - 1 {
                b.triangle(a + 1, c, c + 1);
            }
        }
    }
    b
}

/// A geodesic sphere: a subdivided icosahedron (even triangle sizes; good
/// for rocks and asteroids after displacing the vertices).
pub fn icosphere(radius: f32, subdivisions: u32, color: u32) -> MeshBuilder {
    let t = (1.0 + 5.0f32.sqrt()) / 2.0;
    let mut pos: Vec<Vec3> = [
        (-1.0, t, 0.0),
        (1.0, t, 0.0),
        (-1.0, -t, 0.0),
        (1.0, -t, 0.0),
        (0.0, -1.0, t),
        (0.0, 1.0, t),
        (0.0, -1.0, -t),
        (0.0, 1.0, -t),
        (t, 0.0, -1.0),
        (t, 0.0, 1.0),
        (-t, 0.0, -1.0),
        (-t, 0.0, 1.0),
    ]
    .iter()
    .map(|&(x, y, z)| Vec3::new(x, y, z).normalize())
    .collect();
    let mut faces: Vec<[u32; 3]> = alloc::vec![
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    for _ in 0..subdivisions.min(5) {
        let mut cache: alloc::collections::BTreeMap<(u32, u32), u32> = alloc::collections::BTreeMap::new();
        let mut mid = |a: u32, b: u32, pos: &mut Vec<Vec3>| -> u32 {
            let key = (a.min(b), a.max(b));
            *cache.entry(key).or_insert_with(|| {
                pos.push(((pos[a as usize] + pos[b as usize]) * 0.5).normalize());
                (pos.len() - 1) as u32
            })
        };
        let mut next = Vec::with_capacity(faces.len() * 4);
        for f in &faces {
            let ab = mid(f[0], f[1], &mut pos);
            let bc = mid(f[1], f[2], &mut pos);
            let ca = mid(f[2], f[0], &mut pos);
            next.push([f[0], ab, ca]);
            next.push([f[1], bc, ab]);
            next.push([f[2], ca, bc]);
            next.push([ab, bc, ca]);
        }
        faces = next;
    }
    let mut b = MeshBuilder::new();
    for p in &pos {
        let uv = Vec2::new(0.5 + p.z.atan2(p.x) / vmath::TAU, 0.5 - p.y.asin() / vmath::PI);
        b.vert(*p * radius, *p, uv, color);
    }
    for f in faces {
        b.triangle(f[0], f[1], f[2]);
    }
    b
}

/// A cylinder or truncated cone along +Y from `y = 0` to `y = height`
/// (`top_radius = 0` gives a cone). `caps` closes the ends.
pub fn cylinder(
    bottom_radius: f32,
    top_radius: f32,
    height: f32,
    segments: u32,
    caps: bool,
    color: u32,
) -> MeshBuilder {
    let segs = segments.max(3);
    let mut b = MeshBuilder::new();
    // Side normals tilt with the slope.
    let slope = (bottom_radius - top_radius) / height.max(1e-6);
    for s in 0..=segs {
        let u = s as f32 / segs as f32;
        let (st, ct) = (u * vmath::TAU).sin_cos();
        let dir = Vec3::new(ct, 0.0, -st);
        let n = (dir + Vec3::new(0.0, slope, 0.0)).normalize();
        b.vert(dir * bottom_radius, n, Vec2::new(u, 1.0), color);
        b.vert(dir * top_radius + Vec3::new(0.0, height, 0.0), n, Vec2::new(u, 0.0), color);
    }
    for s in 0..segs {
        let i = s * 2;
        if top_radius <= 0.0 {
            b.triangle(i, i + 2, i + 1); // cone: the top edge is a point
        } else if bottom_radius <= 0.0 {
            b.triangle(i, i + 3, i + 1);
        } else {
            b.quad(i, i + 2, i + 3, i + 1);
        }
    }
    if caps {
        for (y, r, up) in [(0.0, bottom_radius, false), (height, top_radius, true)] {
            if r <= 0.0 {
                continue;
            }
            let n = if up { Vec3::Y } else { Vec3::NEG_Y };
            let center = b.vert(Vec3::new(0.0, y, 0.0), n, Vec2::splat(0.5), color);
            let first = b.vertices.len() as u32;
            for s in 0..segs {
                let (st, ct) = (s as f32 / segs as f32 * vmath::TAU).sin_cos();
                b.vert(Vec3::new(ct * r, y, -st * r), n, Vec2::new(0.5 + ct * 0.5, 0.5 + st * 0.5), color);
            }
            for s in 0..segs {
                let (a, c) = (first + s, first + (s + 1) % segs);
                if up {
                    b.triangle(center, a, c);
                } else {
                    b.triangle(center, c, a);
                }
            }
        }
    }
    b
}

/// A flat grid in the XZ plane facing +Y, `width` x `depth` with
/// `nx` x `nz` cells; UVs repeat once per `uv_size` world units.
pub fn plane(width: f32, depth: f32, nx: u32, nz: u32, uv_size: f32, color: u32) -> MeshBuilder {
    heightmap(nx, nz, Vec2::new(width / nx.max(1) as f32, depth / nz.max(1) as f32), uv_size, |_, _| 0.0, |_, _| color)
}

/// Terrain: a grid of `nx` x `nz` cells of `cell` size centred on the
/// origin, with heights from `height(x, z)` and colours from
/// `color(position, normal)`. UVs repeat every `uv_size` units.
pub fn heightmap(
    nx: u32,
    nz: u32,
    cell: Vec2,
    uv_size: f32,
    height: impl Fn(f32, f32) -> f32,
    color: impl Fn(Vec3, Vec3) -> u32,
) -> MeshBuilder {
    let (nx, nz) = (nx.max(1), nz.max(1));
    let (x0, z0) = (-(nx as f32) * cell.x * 0.5, -(nz as f32) * cell.y * 0.5);
    let mut b = MeshBuilder::new();
    let h = |i: i32, j: i32| height(x0 + i as f32 * cell.x, z0 + j as f32 * cell.y);
    for j in 0..=nz as i32 {
        for i in 0..=nx as i32 {
            let (x, z) = (x0 + i as f32 * cell.x, z0 + j as f32 * cell.y);
            let y = h(i, j);
            // Central differences for the normal.
            let dx = (h(i + 1, j) - h(i - 1, j)) / (2.0 * cell.x);
            let dz = (h(i, j + 1) - h(i, j - 1)) / (2.0 * cell.y);
            let n = Vec3::new(-dx, 1.0, -dz).normalize();
            let p = Vec3::new(x, y, z);
            let uv = Vec2::new(x / uv_size, z / uv_size);
            b.vert(p, n, uv, color(p, n));
        }
    }
    let row = nx + 1;
    for j in 0..nz {
        for i in 0..nx {
            let a = j * row + i;
            // Counter-clockwise seen from above (+Y): a, a+row, a+row+1, a+1.
            b.quad(a, a + row, a + row + 1, a + 1);
        }
    }
    b
}

/// A surface of revolution around +Y: `profile` points are (radius, y),
/// from bottom to top.
pub fn lathe(profile: &[Vec2], segments: u32, color: u32) -> MeshBuilder {
    let segs = segments.max(3);
    let mut b = MeshBuilder::new();
    if profile.len() < 2 {
        return b;
    }
    let n = profile.len();
    let mut len = 0.0;
    let mut vs = Vec::with_capacity(n);
    for i in 0..n {
        if i > 0 {
            len += profile[i].distance(profile[i - 1]);
        }
        vs.push(len);
    }
    for s in 0..=segs {
        let u = s as f32 / segs as f32;
        let (st, ct) = (u * vmath::TAU).sin_cos();
        let dir = Vec3::new(ct, 0.0, -st);
        for i in 0..n {
            let p = profile[i];
            // Profile tangent -> outward normal in the (radius, y) plane.
            let (a, c) = (profile[i.saturating_sub(1)], profile[(i + 1).min(n - 1)]);
            let t = (c - a).normalize_or(Vec2::Y);
            let n2 = Vec2::new(t.y, -t.x);
            let normal = (dir * n2.x + Vec3::new(0.0, n2.y, 0.0)).normalize_or(dir);
            b.vert(dir * p.x + Vec3::new(0.0, p.y, 0.0), normal, Vec2::new(u, vs[i] / len.max(1e-6)), color);
        }
    }
    let row = n as u32;
    for s in 0..segs {
        for i in 0..row - 1 {
            let a = s * row + i;
            b.quad(a, a + row, a + row + 1, a + 1);
        }
    }
    b
}

/// Extrudes a simple polygon in the XY plane along Z, from `-depth / 2` to
/// `depth / 2`, with flat sides and capped ends (either winding works).
pub fn extrude(polygon: &[Vec2], depth: f32, color: u32) -> MeshBuilder {
    let mut b = MeshBuilder::new();
    let n = polygon.len();
    if n < 3 {
        return b;
    }
    let area: f32 = (0..n).map(|i| polygon[i].perp_dot(polygon[(i + 1) % n])).sum();
    let reversed: Vec<Vec2>;
    let polygon = if area < 0.0 {
        reversed = polygon.iter().rev().copied().collect();
        &reversed[..]
    } else {
        polygon
    };
    let (zf, zb) = (depth * 0.5, -depth * 0.5);
    // Sides.
    for i in 0..n {
        let (p, q) = (polygon[i], polygon[(i + 1) % n]);
        b.face(
            [Vec3::new(p.x, p.y, zb), Vec3::new(q.x, q.y, zb), Vec3::new(q.x, q.y, zf), Vec3::new(p.x, p.y, zf)],
            color,
        );
    }
    // Caps (ear clipping, so concave outlines work).
    let tris = triangulate(polygon);
    for (z, front) in [(zf, true), (zb, false)] {
        let normal = if front { Vec3::Z } else { Vec3::NEG_Z };
        let base = b.vertices.len() as u32;
        for p in polygon {
            b.vert(Vec3::new(p.x, p.y, z), normal, *p, color);
        }
        for t in &tris {
            if front {
                b.triangle(base + t[0], base + t[1], base + t[2]);
            } else {
                b.triangle(base + t[0], base + t[2], base + t[1]);
            }
        }
    }
    b
}

/// A torus around +Y.
pub fn torus(radius: f32, tube: f32, segments: u32, sides: u32, color: u32) -> MeshBuilder {
    let (segs, sides) = (segments.max(3), sides.max(3));
    let mut b = MeshBuilder::new();
    for s in 0..=segs {
        let u = s as f32 / segs as f32;
        let (st, ct) = (u * vmath::TAU).sin_cos();
        let dir = Vec3::new(ct, 0.0, -st);
        for k in 0..=sides {
            let v = k as f32 / sides as f32;
            let (sp, cp) = (v * vmath::TAU).sin_cos();
            let n = dir * cp + Vec3::new(0.0, sp, 0.0);
            b.vert(dir * radius + n * tube, n, Vec2::new(u, v), color);
        }
    }
    let row = sides + 1;
    for s in 0..segs {
        for k in 0..sides {
            let a = s * row + k;
            b.quad(a, a + row, a + row + 1, a + 1);
        }
    }
    b
}

/// Ear-clipping triangulation of a simple counter-clockwise polygon.
pub fn triangulate(poly: &[Vec2]) -> Vec<[u32; 3]> {
    let n = poly.len();
    let mut out = Vec::new();
    if n < 3 {
        return out;
    }
    // Ensure counter-clockwise order.
    let area: f32 = (0..n).map(|i| poly[i].perp_dot(poly[(i + 1) % n])).sum();
    let mut idx: Vec<u32> = if area >= 0.0 { (0..n as u32).collect() } else { (0..n as u32).rev().collect() };
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (poly[ia as usize], poly[ib as usize], poly[ic as usize]);
            if (b - a).perp_dot(c - b) <= 0.0 {
                continue; // reflex corner
            }
            let inside = idx.iter().any(|&j| {
                if j == ia || j == ib || j == ic {
                    return false;
                }
                let p = poly[j as usize];
                (b - a).perp_dot(p - a) > 0.0 && (c - b).perp_dot(p - b) > 0.0 && (a - c).perp_dot(p - c) > 0.0
            });
            if inside {
                continue;
            }
            out.push([ia, ib, ic]);
            idx.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            break;
        }
    }
    if idx.len() == 3 {
        out.push([idx[0], idx[1], idx[2]]);
    } else {
        // Degenerate input: fall back to a fan.
        for i in 1..idx.len().saturating_sub(1) {
            out.push([idx[0], idx[i], idx[i + 1]]);
        }
    }
    out
}
