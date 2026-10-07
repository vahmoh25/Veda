//! A scene: the draws and clears recorded for one framebuffer, with their
//! primitives set up and sorted into 64x64-pixel tiles, ready for the
//! tiles to be rendered in parallel.
//!
//! Setup clips primitives in clip space (against the near and far planes
//! and a guard band, triangles being bounded by the viewport when they are
//! rasterized), maps them to window coordinates, and builds exact
//! fixed-point edge functions (8 sub-pixel bits) with a top-left fill
//! rule, so that triangles sharing an edge never both cover a sample on
//! it and never leave a gap.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;

use vglsl::interp::Lanes;
use vglsl::link::Interpolation;

use super::program::SoftProgram;
use super::target::RenderTarget;
use super::texture::Sampled;
use crate::backend::{Blend, Cull, DepthStencil, Framebuffer, Raster, ResourceId, Viewport};
use crate::format::Format;
use vmath::f32 as m;

/// Tile size in pixels.
pub const TILE: i32 = 64;

/// Sub-pixel precision of window coordinates.
pub const SUB: i64 = 256;

/// The sample positions of 4x multisampling, in 1/256 pixel (Direct3D's
/// standard pattern), and the centre for single sampling.
pub const SAMPLES_4X: [(i64, i64); 4] = [(96, 32), (224, 96), (32, 160), (160, 224)];
pub const CENTER: [(i64, i64); 1] = [(128, 128)];

/// Window coordinates are kept within this many pixels of the origin.
const GUARD: f32 = 32768.0;

/// A command in a tile's list.
#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    Tri(u32),
    Line(u32),
    Point(u32),
    Clear(u32),
}

/// The fixed-function state the fragment stage of a draw uses.
#[derive(Clone, Copy, Debug)]
pub struct FragState {
    pub depth_stencil: DepthStencil,
    pub blend: Blend,
    pub alpha_to_coverage: bool,
    pub sample_coverage: Option<(f32, bool)>,
    /// Fragment output → color attachment.
    pub draw_buffers: [Option<u8>; 8],
    /// Depth and stencil are tested before the fragment shader runs.
    pub early: bool,
}

/// What a draw's fragments need.
pub struct DrawRecord {
    pub program: Arc<SoftProgram>,
    pub uniforms: Vec<[u32; 4]>,
    /// The fragment shader's prologue registers.
    pub prologue: Vec<Lanes>,
    /// Each uniform block's contents (copied when the draw was recorded).
    pub blocks: Vec<Vec<u8>>,
    pub textures: Vec<Option<Sampled>>,
    pub state: FragState,
    /// The occlusion query counting the draw's samples.
    pub query: Option<Arc<AtomicU64>>,
}

/// A set-up triangle.
#[derive(Clone, Copy, Debug)]
pub struct Tri {
    pub draw: u32,
    /// Pixels it may cover: x0, y0, x1, y1 (exclusive).
    pub bbox: [i32; 4],
    /// Edge functions `a * x + b * y + c` over fixed-point positions,
    /// non-negative inside (bias of the fill rule included).
    pub edges: [[i64; 3]; 3],
    /// The origin of the planes below (window coordinates of vertex 0).
    pub origin: [f32; 2],
    /// Screen-space barycentric weights of vertices 1 and 2: d/dx, d/dy.
    pub l1: [f32; 2],
    pub l2: [f32; 2],
    /// Window depth: d/dx, d/dy, value at the origin (offset included).
    pub z: [f32; 3],
    pub front: bool,
    /// Where its vertices' 1/w and varyings are in the scene's arena.
    pub attrs: u32,
}

/// A set-up line segment.
#[derive(Clone, Copy, Debug)]
pub struct LinePrim {
    pub draw: u32,
    pub bbox: [i32; 4],
    /// Window coordinates of the ends (x, y, z).
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub half_width: f32,
    pub x_major: bool,
    pub attrs: u32,
}

/// A set-up point.
#[derive(Clone, Copy, Debug)]
pub struct PointPrim {
    pub draw: u32,
    pub bbox: [i32; 4],
    pub center: [f32; 2],
    pub z: f32,
    pub inv_w: f32,
    pub size: f32,
    pub attrs: u32,
}

/// A clear, as recorded.
#[derive(Clone, Copy, Debug)]
pub struct ClearCmd {
    /// Each color attachment's value (32-bit components).
    pub colors: [Option<[u32; 4]>; 8],
    pub depth: Option<f32>,
    pub stencil: Option<i32>,
    pub stencil_mask: u32,
    pub color_mask: [bool; 4],
    pub rect: [i32; 4],
}

/// The images of the framebuffer being rendered.
#[derive(Clone, Copy, Debug, Default)]
pub struct Targets {
    pub colors: [Option<RenderTarget>; 8],
    pub depth: Option<RenderTarget>,
    pub stencil: Option<RenderTarget>,
    pub width: u32,
    pub height: u32,
    pub samples: u32,
}

/// Recorded work for one framebuffer.
pub struct Scene {
    pub framebuffer: Framebuffer,
    pub targets: Targets,
    pub tiles_x: u32,
    pub bins: Vec<Vec<Cmd>>,
    pub draws: Vec<DrawRecord>,
    pub tris: Vec<Tri>,
    pub lines: Vec<LinePrim>,
    pub points: Vec<PointPrim>,
    pub clears: Vec<ClearCmd>,
    /// Per-primitive interpolation data.
    pub attrs: Vec<u32>,
    /// Resources the scene reads or writes.
    pub uses: Vec<ResourceId>,
}

impl Scene {
    pub fn new(framebuffer: Framebuffer, targets: Targets) -> Scene {
        let tiles_x = targets.width.div_ceil(TILE as u32).max(1);
        let tiles_y = targets.height.div_ceil(TILE as u32).max(1);
        Scene {
            framebuffer,
            targets,
            tiles_x,
            bins: vec![Vec::new(); (tiles_x * tiles_y) as usize],
            draws: Vec::new(),
            tris: Vec::new(),
            lines: Vec::new(),
            points: Vec::new(),
            clears: Vec::new(),
            attrs: Vec::new(),
            uses: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.draws.is_empty() && self.clears.is_empty()
    }

    pub fn uses(&self, r: ResourceId) -> bool {
        self.uses.contains(&r)
    }

    pub fn add_use(&mut self, r: ResourceId) {
        if !self.uses.contains(&r) {
            self.uses.push(r);
        }
    }

    /// Bytes of primitive data recorded (to bound the scene's memory).
    pub fn size(&self) -> usize {
        self.attrs.len() * 4
            + self.tris.len() * core::mem::size_of::<Tri>()
            + self.bins.iter().map(Vec::len).sum::<usize>() * 8
    }

    /// Records a clear of the pixels in `rect`.
    pub fn clear(&mut self, c: ClearCmd) {
        let index = self.clears.len() as u32;
        self.clears.push(c);
        self.bin(c.rect, Cmd::Clear(index), None);
    }

    /// Adds a command to every tile `rect` touches (and, for a triangle,
    /// that its edges do not exclude).
    fn bin(&mut self, rect: [i32; 4], cmd: Cmd, edges: Option<&[[i64; 3]; 3]>) {
        if rect[0] >= rect[2] || rect[1] >= rect[3] {
            return;
        }
        let (tx0, ty0) = (rect[0] / TILE, rect[1] / TILE);
        let (tx1, ty1) = ((rect[2] - 1) / TILE, (rect[3] - 1) / TILE);
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                if let (Some(e), false) = (edges, tx0 == tx1 && ty0 == ty1) {
                    // Skip tiles entirely outside an edge.
                    let (x0, y0) = (i64::from(tx * TILE) * SUB, i64::from(ty * TILE) * SUB);
                    let (x1, y1) = (x0 + i64::from(TILE) * SUB, y0 + i64::from(TILE) * SUB);
                    let outside = e.iter().any(|&[a, b, c]| {
                        let x = if a > 0 { x1 } else { x0 };
                        let y = if b > 0 { y1 } else { y0 };
                        a * x + b * y + c < 0
                    });
                    if outside {
                        continue;
                    }
                }
                self.bins[(ty as u32 * self.tiles_x + tx as u32) as usize].push(cmd);
            }
        }
    }
}

/// A vertex being clipped: clip position and the varyings that
/// interpolate.
#[derive(Clone, Copy)]
struct ClipVertex {
    pos: [f32; 4],
    vars: [f32; MAX_VARYINGS],
}

/// The most varying components (16 vec4s).
pub const MAX_VARYINGS: usize = 64;

/// Sets up a draw's primitives into a scene.
pub struct Setup<'a> {
    pub scene: &'a mut Scene,
    pub draw: u32,
    pub prog: &'a SoftProgram,
    pub viewport: Viewport,
    pub raster: Raster,
    /// Framebuffer ∩ scissor (points and lines).
    pub clip_rect: [i32; 4],
    /// Framebuffer ∩ scissor ∩ viewport (triangles).
    pub tri_rect: [i32; 4],
    pub depth_format: Option<Format>,
    pub point_size: (f32, f32),
    pub line_width: (f32, f32),
}

/// Words a vertex stores before its varyings: x, y, z, w, point size.
const HEAD: usize = 5;

impl Setup<'_> {
    /// The clip planes as `(a, b, c, d)`: inside when
    /// `a x + b y + c z + d w >= 0`. The x and y planes are the guard band.
    fn planes(&self) -> [[f32; 4]; 7] {
        let vp = &self.viewport;
        // Window coordinates within ±GUARD (which holds every framebuffer):
        // x_ndc in [lo, hi]. Everything outside is off the framebuffer, and
        // fixed-point arithmetic on what is inside cannot overflow.
        let lo = |o: f32, size: f32| (-GUARD - o) * 2.0 / size.max(1.0) - 1.0;
        let hi = |o: f32, size: f32| (GUARD - o) * 2.0 / size.max(1.0) - 1.0;
        let (lx, hx) = (lo(vp.x, vp.w), hi(vp.x, vp.w));
        let (ly, hy) = (lo(vp.y, vp.h), hi(vp.y, vp.h));
        [
            [1.0, 0.0, 0.0, -lx],
            [-1.0, 0.0, 0.0, hx],
            [0.0, 1.0, 0.0, -ly],
            [0.0, -1.0, 0.0, hy],
            [0.0, 0.0, 1.0, 1.0],
            [0.0, 0.0, -1.0, 1.0],
            // w > 0 (a tiny positive bound).
            [0.0, 0.0, 0.0, 1.0],
        ]
    }

    fn outcode(pos: &[f32; 4], planes: &[[f32; 4]; 7]) -> u32 {
        let mut code = 0;
        for (i, p) in planes.iter().enumerate() {
            let d = if i == 6 { pos[3] - 1e-30 } else { p[0] * pos[0] + p[1] * pos[1] + p[2] * pos[2] + p[3] * pos[3] };
            if d.is_nan() || d < 0.0 {
                code |= 1 << i;
            }
        }
        code
    }

    fn clip_vertex(&self, v: &[u32]) -> ClipVertex {
        let mut c = ClipVertex { pos: [0.0; 4], vars: [0.0; MAX_VARYINGS] };
        for (p, &w) in c.pos.iter_mut().zip(v) {
            *p = f32::from_bits(w);
        }
        for (k, out) in c.vars.iter_mut().enumerate().take(self.prog.varyings.len().min(MAX_VARYINGS)) {
            *out = f32::from_bits(v[HEAD + k]);
        }
        c
    }

    /// Window coordinates of a clip-space position: x, y, z, 1/w.
    fn window(&self, p: &[f32; 4]) -> [f32; 4] {
        let vp = &self.viewport;
        let iw = 1.0 / p[3];
        let (x, y, z) = (p[0] * iw, p[1] * iw, p[2] * iw);
        [
            vp.x + (x + 1.0) * vp.w * 0.5,
            vp.y + (y + 1.0) * vp.h * 0.5,
            z * (vp.far - vp.near) * 0.5 + (vp.near + vp.far) * 0.5,
            iw,
        ]
    }

    // ---- Triangles --------------------------------------------------------------

    /// A triangle of post-transform vertices (the last is the provoking
    /// one).
    pub fn triangle(&mut self, v: [&[u32]; 3]) {
        let planes = self.planes();
        let p = v.map(|x| [0, 1, 2, 3].map(|i| f32::from_bits(x[i])));
        let codes = p.map(|x| Self::outcode(&x, &planes));
        if codes[0] & codes[1] & codes[2] != 0 {
            return;
        }
        let provoking = v[2];
        if codes[0] | codes[1] | codes[2] == 0 {
            let c = v.map(|x| self.clip_vertex(x));
            self.emit_triangle(&c[0], &c[1], &c[2], provoking);
            return;
        }
        // Sutherland-Hodgman against the planes the vertices cross.
        let mut poly: [ClipVertex; 12] = [self.clip_vertex(v[0]); 12];
        poly[1] = self.clip_vertex(v[1]);
        poly[2] = self.clip_vertex(v[2]);
        let mut n = 3;
        let nv = self.prog.varyings.len().min(MAX_VARYINGS);
        let mut tmp = poly;
        for (i, pl) in planes.iter().enumerate() {
            if (codes[0] | codes[1] | codes[2]) & (1 << i) == 0 {
                continue;
            }
            let dist = |c: &ClipVertex| {
                if i == 6 {
                    c.pos[3] - 1e-30
                } else {
                    pl[0] * c.pos[0] + pl[1] * c.pos[1] + pl[2] * c.pos[2] + pl[3] * c.pos[3]
                }
            };
            let mut m = 0;
            for k in 0..n {
                let a = &poly[k];
                let b = &poly[(k + 1) % n];
                let (da, db) = (dist(a), dist(b));
                let (ina, inb) = (da >= 0.0, db >= 0.0);
                if ina && m < tmp.len() {
                    tmp[m] = *a;
                    m += 1;
                }
                if ina != inb && m < tmp.len() {
                    let t = da / (da - db);
                    let mut c = *a;
                    for j in 0..4 {
                        c.pos[j] = a.pos[j] + (b.pos[j] - a.pos[j]) * t;
                    }
                    for j in 0..nv {
                        c.vars[j] = a.vars[j] + (b.vars[j] - a.vars[j]) * t;
                    }
                    tmp[m] = c;
                    m += 1;
                }
            }
            poly[..m].copy_from_slice(&tmp[..m]);
            n = m;
            if n < 3 {
                return;
            }
        }
        for k in 1..n - 1 {
            self.emit_triangle(&poly[0], &poly[k], &poly[k + 1], provoking);
        }
    }

    fn emit_triangle(&mut self, v0: &ClipVertex, v1: &ClipVertex, v2: &ClipVertex, provoking: &[u32]) {
        let w = [self.window(&v0.pos), self.window(&v1.pos), self.window(&v2.pos)];
        let fix = |x: f32| m::round_ties_even(x * SUB as f32) as i64;
        let mut xy = w.map(|p| (fix(p[0]), fix(p[1])));
        let area = (xy[1].0 - xy[0].0) * (xy[2].1 - xy[0].1) - (xy[2].0 - xy[0].0) * (xy[1].1 - xy[0].1);
        if area == 0 {
            return;
        }
        // Counter-clockwise in window coordinates (y up) has positive area.
        let ccw = area > 0;
        let front = ccw == self.raster.front_ccw;
        let culled = match self.raster.cull {
            None => false,
            Some(Cull::Front) => front,
            Some(Cull::Back) => !front,
            Some(Cull::Both) => true,
        };
        if culled || self.raster.discard {
            return;
        }
        // Order the vertices counter-clockwise.
        let mut order = [0usize, 1, 2];
        if !ccw {
            order = [0, 2, 1];
            xy = [xy[0], xy[2], xy[1]];
        }
        let area = area.abs();
        let verts = [v0, v1, v2];
        let win = order.map(|i| w[i]);
        // Edge k is opposite vertex k: from vertex k+1 to k+2.
        let mut edges = [[0i64; 3]; 3];
        for (k, e) in edges.iter_mut().enumerate() {
            let (i, j) = ((k + 1) % 3, (k + 2) % 3);
            let (dx, dy) = (xy[j].0 - xy[i].0, xy[j].1 - xy[i].1);
            let (a, b) = (-dy, dx);
            let c = -(a * xy[i].0 + b * xy[i].1);
            // Top-left rule (y up, counter-clockwise): left edges go down,
            // top edges go left.
            let top_left = dy < 0 || (dy == 0 && dx < 0);
            *e = [a, b, if top_left { c } else { c - 1 }];
        }
        // Bounding box, conservative by a pixel for the sample positions.
        let minx = xy.iter().map(|p| p.0).min().unwrap();
        let maxx = xy.iter().map(|p| p.0).max().unwrap();
        let miny = xy.iter().map(|p| p.1).min().unwrap();
        let maxy = xy.iter().map(|p| p.1).max().unwrap();
        let r = self.tri_rect;
        let bbox = [
            ((minx >> 8) as i32).max(r[0]),
            ((miny >> 8) as i32).max(r[1]),
            (((maxx >> 8) + 1) as i32).min(r[2]),
            (((maxy >> 8) + 1) as i32).min(r[3]),
        ];
        if bbox[0] >= bbox[2] || bbox[1] >= bbox[3] {
            return;
        }
        // Barycentric planes (per pixel) from the edge functions.
        let s = SUB as f64 / area as f64;
        let l1 = [(edges[1][0] as f64 * s) as f32, (edges[1][1] as f64 * s) as f32];
        let l2 = [(edges[2][0] as f64 * s) as f32, (edges[2][1] as f64 * s) as f32];
        let origin = [xy[0].0 as f32 / SUB as f32, xy[0].1 as f32 / SUB as f32];
        let (z0, z1, z2) = (win[0][2], win[1][2], win[2][2]);
        let dzdx = (z1 - z0) * l1[0] + (z2 - z0) * l2[0];
        let dzdy = (z1 - z0) * l1[1] + (z2 - z0) * l2[1];
        let mut zc = z0;
        if let Some((factor, units)) = self.raster.polygon_offset {
            let max_slope = dzdx.abs().max(dzdy.abs());
            let r = match self.depth_format {
                Some(Format::D16Unorm) => 1.0 / 65536.0,
                Some(Format::D24Unorm | Format::D24UnormS8Uint) => 1.0 / 16_777_216.0,
                Some(Format::D32Float | Format::D32FloatS8Uint) => {
                    // Float depth: one unit in the last place of the largest
                    // depth.
                    let zmax = z0.abs().max(z1.abs()).max(z2.abs()).max(f32::MIN_POSITIVE);
                    let e = ((zmax.to_bits() >> 23) & 0xFF) as i32 - 127;
                    f32::from_bits(((e - 23 + 127).max(1) as u32) << 23)
                }
                _ => 0.0,
            };
            let o = factor * max_slope + units * r;
            if o.is_finite() {
                zc += o;
            }
        }
        let attrs = self.scene.attrs.len() as u32;
        let prog = self.prog;
        let a = &mut self.scene.attrs;
        for w in &win {
            a.push(w[3].to_bits());
        }
        for k in 0..prog.varyings.len() {
            if prog.varyings[k].interpolation == Interpolation::Flat || k >= MAX_VARYINGS {
                a.push(provoking.get(HEAD + k).copied().unwrap_or(0));
            } else {
                for &i in &order {
                    a.push((verts[i].vars[k] * w[i][3]).to_bits());
                }
            }
        }
        let tri = Tri { draw: self.draw, bbox, edges, origin, l1, l2, z: [dzdx, dzdy, zc], front, attrs };
        let index = self.scene.tris.len() as u32;
        self.scene.tris.push(tri);
        let e = self.scene.tris[index as usize].edges;
        self.scene.bin(bbox, Cmd::Tri(index), Some(&e));
    }

    // ---- Lines ----------------------------------------------------------------------

    /// A line segment (`provoking` is the vertex whose flat varyings it
    /// takes).
    pub fn line(&mut self, a: &[u32], b: &[u32], provoking: &[u32]) {
        if self.raster.discard {
            return;
        }
        let pa = [0, 1, 2, 3].map(|i| f32::from_bits(a[i]));
        let pb = [0, 1, 2, 3].map(|i| f32::from_bits(b[i]));
        // Clip against the view volume (Liang-Barsky in clip space).
        let (mut t0, mut t1) = (0.0f32, 1.0f32);
        let planes: [[f32; 4]; 7] = [
            [1.0, 0.0, 0.0, 1.0],
            [-1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, -1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
            [0.0, 0.0, -1.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        for (i, p) in planes.iter().enumerate() {
            let d = |q: &[f32; 4]| {
                if i == 6 { q[3] - 1e-30 } else { p[0] * q[0] + p[1] * q[1] + p[2] * q[2] + p[3] * q[3] }
            };
            let (da, db) = (d(&pa), d(&pb));
            if da.is_nan() || db.is_nan() || (da < 0.0 && db < 0.0) {
                return;
            }
            if da < 0.0 {
                t0 = t0.max(da / (da - db));
            } else if db < 0.0 {
                t1 = t1.min(da / (da - db));
            }
            if t0 > t1 {
                return;
            }
        }
        let lerp = |t: f32| [0, 1, 2, 3].map(|i| pa[i] + (pb[i] - pa[i]) * t);
        let (ca, cb) = (lerp(t0), lerp(t1));
        let (wa, wb) = (self.window(&ca), self.window(&cb));
        let (dx, dy) = (wb[0] - wa[0], wb[1] - wa[1]);
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let width = self.raster.line_width.clamp(self.line_width.0, self.line_width.1).max(1.0);
        let hw = width * 0.5;
        let r = self.clip_rect;
        let bbox = [
            (m::floor(wa[0].min(wb[0]) - hw) as i32 - 1).max(r[0]),
            (m::floor(wa[1].min(wb[1]) - hw) as i32 - 1).max(r[1]),
            (m::ceil(wa[0].max(wb[0]) + hw) as i32 + 1).min(r[2]),
            (m::ceil(wa[1].max(wb[1]) + hw) as i32 + 1).min(r[3]),
        ];
        if bbox[0] >= bbox[2] || bbox[1] >= bbox[3] {
            return;
        }
        let attrs = self.scene.attrs.len() as u32;
        let a_att = &mut self.scene.attrs;
        a_att.push(wa[3].to_bits());
        a_att.push(wb[3].to_bits());
        let at = |v: &[u32], k: usize| f32::from_bits(v.get(HEAD + k).copied().unwrap_or(0));
        for k in 0..self.prog.varyings.len() {
            if self.prog.varyings[k].interpolation == Interpolation::Flat {
                a_att.push(provoking.get(HEAD + k).copied().unwrap_or(0));
            } else {
                let (va, vb) = (at(a, k), at(b, k));
                let (xa, xb) = (va + (vb - va) * t0, va + (vb - va) * t1);
                a_att.push((xa * wa[3]).to_bits());
                a_att.push((xb * wb[3]).to_bits());
            }
        }
        let prim = LinePrim {
            draw: self.draw,
            bbox,
            a: [wa[0], wa[1], wa[2]],
            b: [wb[0], wb[1], wb[2]],
            half_width: hw,
            x_major: dx.abs() >= dy.abs(),
            attrs,
        };
        let index = self.scene.lines.len() as u32;
        self.scene.lines.push(prim);
        self.scene.bin(bbox, Cmd::Line(index), None);
    }

    // ---- Points -----------------------------------------------------------------------

    pub fn point(&mut self, v: &[u32]) {
        if self.raster.discard {
            return;
        }
        let p = [0, 1, 2, 3].map(|i| f32::from_bits(v[i]));
        // A point is drawn only if its vertex is inside the view volume.
        let w = p[3];
        if w.is_nan() || w <= 0.0 || p[0].abs() > w || p[1].abs() > w || p[2].abs() > w {
            return;
        }
        let win = self.window(&p);
        let ps = f32::from_bits(v[4]);
        let size = if ps.is_nan() { 1.0 } else { ps.clamp(self.point_size.0, self.point_size.1) };
        let h = size * 0.5;
        let r = self.clip_rect;
        let bbox = [
            (m::floor(win[0] - h) as i32).max(r[0]),
            (m::floor(win[1] - h) as i32).max(r[1]),
            (m::ceil(win[0] + h) as i32 + 1).min(r[2]),
            (m::ceil(win[1] + h) as i32 + 1).min(r[3]),
        ];
        if bbox[0] >= bbox[2] || bbox[1] >= bbox[3] {
            return;
        }
        let attrs = self.scene.attrs.len() as u32;
        for k in 0..self.prog.varyings.len() {
            self.scene.attrs.push(v.get(HEAD + k).copied().unwrap_or(0));
        }
        let prim = PointPrim { draw: self.draw, bbox, center: [win[0], win[1]], z: win[2], inv_w: win[3], size, attrs };
        let index = self.scene.points.len() as u32;
        self.scene.points.push(prim);
        self.scene.bin(bbox, Cmd::Point(index), None);
    }
}

/// The sample positions of a sample count.
pub fn sample_positions(samples: u32) -> &'static [(i64, i64)] {
    if samples > 1 { &SAMPLES_4X } else { &CENTER }
}
