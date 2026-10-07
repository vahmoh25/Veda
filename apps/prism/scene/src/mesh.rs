//! Procedural meshes: interleaved positions and normals (six floats per
//! vertex) and 16-bit triangle indices.

use alloc::vec::Vec;

use vmath::Vec3;
use vmath::f32 as m;

/// A triangle mesh.
pub struct Mesh {
    /// x, y, z, nx, ny, nz per vertex.
    pub vertices: Vec<f32>,
    pub indices: Vec<u16>,
}

impl Mesh {
    fn push(&mut self, p: Vec3, n: Vec3) {
        self.vertices.extend_from_slice(&[p.x, p.y, p.z, n.x, n.y, n.z]);
    }

    fn count(&self) -> u16 {
        (self.vertices.len() / 6) as u16
    }
}

/// A (p, q) torus knot: a tube of radius `tube` around the curve that winds
/// `p` times around the axis and `q` times through the hole.
pub fn torus_knot(p: f32, q: f32, radius: f32, tube: f32, segments: usize, sides: usize) -> Mesh {
    let curve = |t: f32| {
        let r = radius * (2.0 + m::cos(q * t)) / 3.0;
        Vec3::new(r * m::cos(p * t), radius * m::sin(q * t) / 3.0, r * m::sin(p * t))
    };
    let mut m = Mesh { vertices: Vec::new(), indices: Vec::new() };
    let tau = core::f32::consts::TAU;
    for i in 0..=segments {
        let t = i as f32 / segments as f32 * tau;
        let dt = 1e-3;
        let c = curve(t);
        let tangent = (curve(t + dt) - curve(t - dt)).normalize();
        // A frame around the tangent, from the direction to the next point
        // of the curve's bend.
        let bend = curve(t + dt) + curve(t - dt) - c * 2.0;
        let normal = (bend - tangent * bend.dot(tangent)).normalize();
        let binormal = tangent.cross(normal);
        for j in 0..=sides {
            let a = j as f32 / sides as f32 * tau;
            let n = normal * m::cos(a) + binormal * m::sin(a);
            m.push(c + n * tube, n);
        }
    }
    let ring = sides as u16 + 1;
    for i in 0..segments as u16 {
        for j in 0..sides as u16 {
            let (a, b) = (i * ring + j, (i + 1) * ring + j);
            m.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    m
}

/// An octahedron with flat faces (each face its own three vertices), stretched
/// to look like a crystal.
pub fn crystal() -> Mesh {
    let tip = Vec3::new(0.0, 1.6, 0.0);
    let bottom = Vec3::new(0.0, -1.0, 0.0);
    let ring =
        [Vec3::new(0.6, 0.0, 0.0), Vec3::new(0.0, 0.0, 0.6), Vec3::new(-0.6, 0.0, 0.0), Vec3::new(0.0, 0.0, -0.6)];
    let mut m = Mesh { vertices: Vec::new(), indices: Vec::new() };
    for k in 0..4 {
        let (a, b) = (ring[k], ring[(k + 1) % 4]);
        for (p0, p1, p2) in [(tip, b, a), (bottom, a, b)] {
            let n = (p1 - p0).cross(p2 - p0).normalize();
            let base = m.count();
            m.push(p0, n);
            m.push(p1, n);
            m.push(p2, n);
            m.indices.extend_from_slice(&[base, base + 1, base + 2]);
        }
    }
    m
}

/// A flat disc on y = 0 facing up.
pub fn disc(radius: f32, segments: usize) -> Mesh {
    let up = Vec3::new(0.0, 1.0, 0.0);
    let mut m = Mesh { vertices: Vec::new(), indices: Vec::new() };
    m.push(Vec3::ZERO, up);
    for i in 0..=segments {
        let a = i as f32 / segments as f32 * core::f32::consts::TAU;
        m.push(Vec3::new(radius * m::cos(a), 0.0, -radius * m::sin(a)), up);
    }
    for i in 1..=segments as u16 {
        m.indices.extend_from_slice(&[0, i, i + 1]);
    }
    m
}
