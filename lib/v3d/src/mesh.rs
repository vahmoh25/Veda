//! Meshes: indexed triangles whose vertices carry a position, a normal,
//! texture coordinates and a colour.
//!
//! [`MeshBuilder`] collects floating-point vertices and triangles (see
//! [`crate::shapes`] for procedural builders); [`MeshBuilder::build`] turns
//! them into a [`Mesh`], which also holds the fixed-point copy the
//! rasterizer core consumes and splits the triangles into chunks with
//! compact vertex ranges (the unit of parallel geometry processing).
//!
//! Front faces are counter-clockwise when seen from outside, as in OpenGL.

use alloc::vec::Vec;

use vmath::{Mat3, Mat4, Vec2, Vec3};

use crate::fixed::{fx14, fx16};
use crate::pipeline::FxVertex;

/// A mesh vertex.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    /// Object-space position.
    pub position: Vec3,
    /// Unit normal (used for lighting).
    pub normal: Vec3,
    /// Texture coordinates; 1.0 is one repeat of the texture.
    pub uv: Vec2,
    /// Straight-alpha `0xAARRGGBB` colour, multiplied with the material
    /// when it uses vertex colours.
    pub color: u32,
}

impl Vertex {
    /// A vertex from its parts.
    pub const fn new(position: Vec3, normal: Vec3, uv: Vec2, color: u32) -> Vertex {
        Vertex { position, normal, uv, color }
    }
}

/// Bounding volumes in object space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    /// Smallest corner of the axis-aligned box.
    pub min: Vec3,
    /// Largest corner of the axis-aligned box.
    pub max: Vec3,
    /// Centre of the bounding sphere.
    pub center: Vec3,
    /// Radius of the bounding sphere.
    pub radius: f32,
}

impl Bounds {
    /// Bounds of nothing (a point at the origin).
    pub const EMPTY: Bounds = Bounds { min: Vec3::ZERO, max: Vec3::ZERO, center: Vec3::ZERO, radius: 0.0 };

    /// Bounds of a set of points.
    pub fn of_points<'a>(points: impl Iterator<Item = &'a Vec3> + Clone) -> Bounds {
        let mut min = Vec3::splat(f32::MAX);
        let mut max = Vec3::splat(f32::MIN);
        let mut any = false;
        for p in points.clone() {
            min = min.min(*p);
            max = max.max(*p);
            any = true;
        }
        if !any {
            return Bounds::EMPTY;
        }
        let center = (min + max) * 0.5;
        let r2 = points.fold(0.0f32, |r, p| r.max(p.distance_squared(center)));
        Bounds { min, max, center, radius: vmath::FloatExt::sqrt(r2) }
    }
}

/// Triangles that are transformed and set up together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    /// First triangle (into `Mesh::local`, three indices per triangle).
    pub tri_start: u32,
    pub tri_count: u32,
    /// Vertex range transformed for this chunk.
    pub vmin: u32,
    pub vcount: u32,
}

/// Most triangles per chunk.
pub(crate) const CHUNK_TRIS: usize = 512;
/// Most vertices transformed per chunk.
pub(crate) const CHUNK_VERTS: usize = 2048;

/// Splits `indices` into chunks; writes chunk-local indices to `local`.
pub(crate) fn build_chunks(indices: &[u32], local: &mut Vec<u32>, chunks: &mut Vec<Chunk>) {
    local.clear();
    chunks.clear();
    let tris = indices.len() / 3;
    let mut start = 0usize;
    while start < tris {
        let (mut lo, mut hi) = (u32::MAX, 0u32);
        let mut end = start;
        while end < tris && end - start < CHUNK_TRIS {
            let t = &indices[end * 3..end * 3 + 3];
            let nlo = lo.min(t[0]).min(t[1]).min(t[2]);
            let nhi = hi.max(t[0]).max(t[1]).max(t[2]);
            if end > start && (nhi - nlo) as usize >= CHUNK_VERTS {
                break;
            }
            lo = nlo;
            hi = nhi;
            end += 1;
        }
        chunks.push(Chunk { tri_start: start as u32, tri_count: (end - start) as u32, vmin: lo, vcount: hi - lo + 1 });
        for &i in &indices[start * 3..end * 3] {
            local.push(i - lo);
        }
        start = end;
    }
}

/// Converts a vertex to the fixed-point format of the core.
pub(crate) fn pack_vertex(v: &Vertex) -> FxVertex {
    let n = v.normal.normalize_or_zero();
    FxVertex {
        x: fx16(v.position.x),
        y: fx16(v.position.y),
        z: fx16(v.position.z),
        nx: fx14(n.x).clamp(-16384, 16384) as i16,
        ny: fx14(n.y).clamp(-16384, 16384) as i16,
        nz: fx14(n.z).clamp(-16384, 16384) as i16,
        u: fx16(v.uv.x),
        v: fx16(v.uv.y),
        color: v.color,
    }
}

/// A triangle mesh ready for drawing.
#[derive(Clone, Debug)]
pub struct Mesh {
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    pub(crate) packed: Vec<FxVertex>,
    pub(crate) local: Vec<u32>,
    pub(crate) chunks: Vec<Chunk>,
    bounds: Bounds,
}

impl Mesh {
    /// Creates a mesh. Triangles with out-of-range indices are dropped.
    pub fn new(vertices: Vec<Vertex>, indices: Vec<u32>) -> Mesh {
        let n = vertices.len() as u32;
        let indices: Vec<u32> = if indices.iter().all(|&i| i < n) && indices.len().is_multiple_of(3) {
            indices
        } else {
            indices.as_chunks::<3>().0.iter().filter(|t| t.iter().all(|&i| i < n)).flatten().copied().collect()
        };
        let mut m = Mesh {
            vertices,
            indices,
            packed: Vec::new(),
            local: Vec::new(),
            chunks: Vec::new(),
            bounds: Bounds::EMPTY,
        };
        m.refresh();
        m
    }

    /// Recomputes the fixed-point data, chunks and bounds.
    fn refresh(&mut self) {
        self.packed = self.vertices.iter().map(pack_vertex).collect();
        build_chunks(&self.indices, &mut self.local, &mut self.chunks);
        self.bounds = Bounds::of_points(self.vertices.iter().map(|v| &v.position));
    }

    /// The vertices.
    pub fn vertices(&self) -> &[Vertex] {
        &self.vertices
    }

    /// Triangle vertex indices, three per triangle.
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// Number of vertices.
    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// Object-space bounds.
    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Modifies every vertex (e.g. to recolour or animate a mesh) and
    /// refreshes the derived data.
    pub fn update_vertices(&mut self, mut f: impl FnMut(usize, &mut Vertex)) {
        for (i, v) in self.vertices.iter_mut().enumerate() {
            f(i, v);
        }
        self.packed.clear();
        self.packed.extend(self.vertices.iter().map(pack_vertex));
        self.bounds = Bounds::of_points(self.vertices.iter().map(|v| &v.position));
    }
}

/// Collects vertices and triangles for a [`Mesh`].
#[derive(Clone, Debug, Default)]
pub struct MeshBuilder {
    /// The vertices added so far.
    pub vertices: Vec<Vertex>,
    /// Triangle vertex indices, three per triangle.
    pub indices: Vec<u32>,
}

impl MeshBuilder {
    /// An empty builder.
    pub fn new() -> MeshBuilder {
        MeshBuilder::default()
    }

    /// Adds a vertex and returns its index.
    pub fn vertex(&mut self, v: Vertex) -> u32 {
        self.vertices.push(v);
        (self.vertices.len() - 1) as u32
    }

    /// Adds a vertex from its parts.
    pub fn vert(&mut self, position: Vec3, normal: Vec3, uv: Vec2, color: u32) -> u32 {
        self.vertex(Vertex { position, normal, uv, color })
    }

    /// Adds a counter-clockwise triangle.
    pub fn triangle(&mut self, a: u32, b: u32, c: u32) {
        self.indices.extend_from_slice(&[a, b, c]);
    }

    /// Adds a counter-clockwise quad `a b c d` as two triangles.
    pub fn quad(&mut self, a: u32, b: u32, c: u32, d: u32) {
        self.indices.extend_from_slice(&[a, b, c, a, c, d]);
    }

    /// Adds a flat quad from four corner positions (counter-clockwise,
    /// starting at the bottom left as seen from the front) with its face
    /// normal; the texture appears upright (UVs (0,1) (1,1) (1,0) (0,0)).
    pub fn face(&mut self, p: [Vec3; 4], color: u32) {
        let n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        let uv = [Vec2::new(0.0, 1.0), Vec2::new(1.0, 1.0), Vec2::new(1.0, 0.0), Vec2::new(0.0, 0.0)];
        let base = self.vertices.len() as u32;
        for i in 0..4 {
            self.vert(p[i], n, uv[i], color);
        }
        self.quad(base, base + 1, base + 2, base + 3);
    }

    /// Appends another builder's geometry.
    pub fn append(&mut self, other: &MeshBuilder) {
        let base = self.vertices.len() as u32;
        self.vertices.extend_from_slice(&other.vertices);
        self.indices.extend(other.indices.iter().map(|i| i + base));
    }

    /// Appends another builder's geometry transformed by `m`.
    pub fn append_transformed(&mut self, other: &MeshBuilder, m: &Mat4) {
        let start = self.vertices.len();
        self.append(other);
        transform_vertices(&mut self.vertices[start..], m);
    }

    /// Transforms positions and normals.
    pub fn transform(&mut self, m: &Mat4) {
        transform_vertices(&mut self.vertices, m);
    }

    /// Sets the colour of every vertex.
    pub fn set_color(&mut self, color: u32) {
        for v in &mut self.vertices {
            v.color = color;
        }
    }

    /// Scales texture coordinates.
    pub fn scale_uv(&mut self, s: Vec2) {
        for v in &mut self.vertices {
            v.uv *= s;
        }
    }

    /// Recomputes smooth (area-weighted) vertex normals.
    pub fn compute_normals(&mut self) {
        let mut acc = alloc::vec![Vec3::ZERO; self.vertices.len()];
        for t in self.indices.as_chunks::<3>().0 {
            let (a, b, c) = (
                self.vertices[t[0] as usize].position,
                self.vertices[t[1] as usize].position,
                self.vertices[t[2] as usize].position,
            );
            let n = (b - a).cross(c - a);
            for &i in t {
                acc[i as usize] += n;
            }
        }
        for (v, n) in self.vertices.iter_mut().zip(acc) {
            v.normal = n.normalize_or(Vec3::Y);
        }
    }

    /// A copy where every triangle has its own vertices with the face
    /// normal: the faceted "low-poly" look.
    pub fn flat_shaded(&self) -> MeshBuilder {
        let mut out = MeshBuilder::new();
        out.vertices.reserve(self.indices.len());
        for t in self.indices.as_chunks::<3>().0 {
            let (a, b, c) = (self.vertices[t[0] as usize], self.vertices[t[1] as usize], self.vertices[t[2] as usize]);
            let n = (b.position - a.position).cross(c.position - a.position).normalize_or(a.normal);
            let base = out.vertices.len() as u32;
            for mut v in [a, b, c] {
                v.normal = n;
                out.vertices.push(v);
            }
            out.triangle(base, base + 1, base + 2);
        }
        out
    }

    /// Reverses the winding of every triangle (and flips the normals).
    pub fn flip(&mut self) {
        for t in self.indices.as_chunks_mut::<3>().0 {
            t.swap(1, 2);
        }
        for v in &mut self.vertices {
            v.normal = -v.normal;
        }
    }

    /// Creates the mesh (triangles with invalid indices are dropped).
    pub fn build(self) -> Mesh {
        Mesh::new(self.vertices, self.indices)
    }
}

fn transform_vertices(vertices: &mut [Vertex], m: &Mat4) {
    // Normals use the inverse transpose (correct under non-uniform scale).
    let nm = Mat3::from_mat4(m).try_inverse().map(|i| i.transpose()).unwrap_or(Mat3::IDENTITY);
    for v in vertices {
        v.position = m.transform_point3(v.position);
        v.normal = (nm * v.normal).normalize_or(v.normal);
    }
}
