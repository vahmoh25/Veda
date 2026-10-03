//! The renderer: frames and draw lists, the parallel geometry and raster
//! phases, and presentation.
//!
//! A frame goes through these steps:
//!
//! 1. [`Renderer::frame`] starts a [`Frame`]; every [`Frame::draw`] culls the
//!    object against the view frustum and converts its matrices, material
//!    and lights into the fixed-point parameters of the [`pipeline`].
//! 2. When the frame ends, opaque draws are sorted front to back (early
//!    depth rejection) and blended ones back to front. Every draw is split
//!    into chunks of triangles; the chunks are divided into contiguous,
//!    equally heavy ranges, one per thread.
//! 3. Geometry phase (parallel): each thread transforms and lights its
//!    chunks' vertices, sets up the triangles (clipping the few that cross
//!    the near plane or the guard band) and bins them into screen tiles.
//! 4. Raster phase (parallel): threads take tiles, most expensive first,
//!    clear them and rasterise the triangles of every thread's bin in
//!    order, which preserves the submission order within each tile.
//!
//! [`Renderer::present`] then scales the image to the window (also in
//! parallel).

use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

use vmath::{FloatExt, Mat4, Vec2, Vec3};

use crate::camera::{Camera, Frustum};
use crate::clip::clip_triangle;
use crate::env::Environment;
use crate::fixed::{alpha_of, dir14, fx8, fx16, out_rgb, rgb_of};
use crate::material::{Blend, Cull, Material, Shading};
use crate::mesh::{CHUNK_TRIS, Chunk, Mesh, Vertex, build_chunks, pack_vertex};
use crate::pipeline::{self, FxPointLight, FxVertex, MAX_POINT_LIGHTS, Setup, TVert, Target, Tri, Xform};
use crate::pool::{ThreadPool, partition};
use crate::texture::{Texture, TextureId};

/// Tile size in pixels (powers of two): wide tiles keep spans long (less
/// per-span setup), short ones give enough tiles to balance the threads.
pub const TILE_W: i32 = 64;
/// Tile height in pixels (see [`TILE_W`]).
pub const TILE_H: i32 = 32;
const TILE_W_SHIFT: i32 = 6;
const TILE_H_SHIFT: i32 = 5;

/// Largest guard band, in pixels beyond the screen centre.
const GUARD_PIXELS: i64 = 8192;

/// Counters and timings of the last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Draw calls submitted.
    pub draws: u32,
    /// Draw calls culled by the frustum test.
    pub culled: u32,
    /// Triangles of the drawn objects.
    pub triangles: u32,
    /// Triangles set up for rasterisation (visible, front-facing).
    pub rasterized: u32,
    /// Triangles that needed clipping.
    pub clipped: u32,
    /// Rasterised rows (before depth testing).
    pub spans: u64,
    /// Rasterised pixels (before depth testing).
    pub pixels: u64,
    /// Transform, clipping, setup and binning time in nanoseconds (0
    /// without a clock, see [`Renderer::set_clock`]).
    pub geometry_ns: u64,
    /// Rasterisation wall time in nanoseconds.
    pub raster_ns: u64,
    /// Raster time summed over all threads (busy time; compare with
    /// `raster_ns * threads` for the parallel efficiency).
    pub raster_cpu_ns: u64,
    /// Per-thread raster busy time in microseconds (first eight threads;
    /// for profiling).
    pub raster_busy_us: [u32; 8],
    /// Per-thread delay from the start of rasterisation until the thread
    /// began working, in microseconds.
    pub raster_delay_us: [u32; 8],
    /// Duration of the last [`Renderer::present`] in nanoseconds.
    pub present_ns: u64,
}

/// The filter used by [`Renderer::present`] for exact 2x scaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upscale2x {
    /// Bilinear with 3:1 weights (SSE2).
    Smooth,
    /// Bilinear at pixel corners, SSE2.
    FastSimd,
    /// Bilinear at pixel corners, scalar SWAR code.
    FastScalar,
}

/// Blended billboards whose projected size exceeds this fraction of the
/// view height start to fade out (see [`Frame::billboards`]).
pub const BILLBOARD_FADE_START: f32 = 0.45;
/// Blended billboards at least this fraction of the view height are skipped.
pub const BILLBOARD_FADE_END: f32 = 0.9;

/// A camera-facing quad (sprites, particles, glows).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Billboard {
    /// Centre in world space.
    pub position: Vec3,
    /// Width and height in world units.
    pub size: Vec2,
    /// Rotation around the view axis in radians.
    pub rotation: f32,
    /// Straight-alpha tint.
    pub color: u32,
    /// Texture rectangle (u0, v0, u1, v1).
    pub uv: [f32; 4],
}

impl Billboard {
    /// A square billboard showing the whole texture.
    pub fn new(position: Vec3, size: f32, color: u32) -> Billboard {
        Billboard { position, size: Vec2::splat(size), rotation: 0.0, color, uv: [0.0, 0.0, 1.0, 1.0] }
    }
}

/// A camera-facing strip between two points (see [`Frame::beams`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Beam {
    /// One end in world space.
    pub start: Vec3,
    /// The other end in world space.
    pub end: Vec3,
    /// Width in world units.
    pub width: f32,
    /// Straight-alpha tint.
    pub color: u32,
}

/// Where a draw's geometry lives.
#[derive(Clone, Copy)]
enum Source {
    Mesh(*const Mesh),
    /// Chunks `first..first + count` of the frame's dynamic geometry.
    Dynamic {
        first: u32,
        count: u32,
    },
}

/// Drawing passes, in order.
const LAYER_OPAQUE: u8 = 0;
const LAYER_SKY: u8 = 1;
const LAYER_BLENDED: u8 = 2;

struct DrawCmd {
    source: Source,
    xform: Xform,
    setup: Setup,
    /// View depth used for sorting.
    depth: f32,
    layer: u8,
}

// SAFETY: the raw pointers (mesh, texture) refer to data that outlives the
// frame and is only read while the frame renders.
unsafe impl Send for DrawCmd {}
// SAFETY: as above.
unsafe impl Sync for DrawCmd {}

#[derive(Clone, Copy)]
struct Job {
    draw: u32,
    chunk: u32,
}

/// Per-thread geometry output.
#[derive(Default)]
struct Worker {
    tverts: Vec<TVert>,
    tris: Vec<Tri>,
    bins: Vec<Vec<u32>>,
    deferred: Vec<u32>,
    triangles: u32,
    clipped: u32,
}

// SAFETY: `Tri` holds texture pointers to immutable data; each worker is
// mutated by one thread at a time (see `Renderer::render`).
unsafe impl Send for Worker {}
// SAFETY: as above.
unsafe impl Sync for Worker {}

/// A raw pointer that may be shared with the pool's threads.
struct SharedPtr<T>(*mut T);

impl<T> SharedPtr<T> {
    fn get(&self) -> *mut T {
        self.0
    }
}

// SAFETY: users only access disjoint parts through the pointer (documented
// at each use).
unsafe impl<T> Sync for SharedPtr<T> {}
// SAFETY: as above.
unsafe impl<T> Send for SharedPtr<T> {}

/// A multi-threaded software renderer with an internal colour and depth
/// buffer.
pub struct Renderer {
    width: i32,
    height: i32,
    color: Vec<u32>,
    depth: Vec<i32>,
    tiles_x: i32,
    tiles_y: i32,
    textures: Vec<Texture>,
    pool: ThreadPool,
    workers: Vec<Worker>,
    draws: Vec<DrawCmd>,
    order: Vec<u32>,
    jobs: Vec<Job>,
    weights: Vec<u32>,
    ranges: Vec<(usize, usize)>,
    dyn_vertices: Vec<FxVertex>,
    dyn_local: Vec<u32>,
    dyn_chunks: Vec<Chunk>,
    scratch_idx: Vec<u32>,
    scratch_order: Vec<(f32, u32)>,
    row_colors: Vec<u32>,
    tile_order: Vec<u32>,
    tile_cost: Vec<u32>,
    clock: Option<fn() -> u64>,
    stats: Stats,
    upscale2x: Upscale2x,
    // Per-frame state.
    camera: Camera,
    env: Environment,
    view_proj: Mat4,
    frustum: Frustum,
    aspect: f32,
}

impl core::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Renderer({}x{}, {} threads)", self.width, self.height, self.pool.threads())
    }
}

impl Renderer {
    /// A renderer with an internal image of `width` x `height` pixels
    /// (at most 2048 x 2048) using `pool` for parallel work.
    pub fn new(width: usize, height: usize, pool: ThreadPool) -> Renderer {
        let threads = pool.threads();
        let mut r = Renderer {
            width: 0,
            height: 0,
            color: Vec::new(),
            depth: Vec::new(),
            tiles_x: 0,
            tiles_y: 0,
            textures: Vec::new(),
            pool,
            workers: (0..threads).map(|_| Worker::default()).collect(),
            draws: Vec::new(),
            order: Vec::new(),
            jobs: Vec::new(),
            weights: Vec::new(),
            ranges: Vec::new(),
            dyn_vertices: Vec::new(),
            dyn_local: Vec::new(),
            dyn_chunks: Vec::new(),
            scratch_idx: Vec::new(),
            scratch_order: Vec::new(),
            row_colors: Vec::new(),
            tile_order: Vec::new(),
            tile_cost: Vec::new(),
            clock: None,
            stats: Stats::default(),
            upscale2x: Upscale2x::FastScalar,
            camera: Camera::default(),
            env: Environment::default(),
            view_proj: Mat4::IDENTITY,
            frustum: Frustum::from_matrix(&Mat4::IDENTITY),
            aspect: 1.0,
        };
        r.resize(width, height);
        r
    }

    /// Changes the internal resolution.
    pub fn resize(&mut self, width: usize, height: usize) {
        let (w, h) = (width.clamp(1, 2048) as i32, height.clamp(1, 2048) as i32);
        if w == self.width && h == self.height {
            return;
        }
        self.width = w;
        self.height = h;
        self.color = vec![0xFF00_0000; (w * h) as usize];
        self.depth = vec![0; (w * h) as usize];
        self.tiles_x = (w + TILE_W - 1) >> TILE_W_SHIFT;
        self.tiles_y = (h + TILE_H - 1) >> TILE_H_SHIFT;
        let tiles = (self.tiles_x * self.tiles_y) as usize;
        for wk in &mut self.workers {
            wk.bins.clear();
            wk.bins.resize_with(tiles, Vec::new);
        }
        self.row_colors = vec![0; h as usize];
    }

    /// Internal image width in pixels.
    pub fn width(&self) -> usize {
        self.width as usize
    }

    /// Internal image height in pixels.
    pub fn height(&self) -> usize {
        self.height as usize
    }

    /// The thread pool (also usable for game work).
    pub fn pool(&self) -> &ThreadPool {
        &self.pool
    }

    /// Sets the clock used for the phase timings in [`Stats`]
    /// (e.g. `vrt::time::now_ns`).
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = Some(clock);
    }

    fn now(&self) -> u64 {
        self.clock.map(|c| c()).unwrap_or(0)
    }

    /// Statistics of the last frame.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Selects the 2x scaling filter.
    pub fn set_upscale_2x(&mut self, f: Upscale2x) {
        self.upscale2x = f;
    }

    /// The 2x scaling filter in use.
    pub fn upscale_2x(&self) -> Upscale2x {
        self.upscale2x
    }

    /// Times the SIMD and the scalar 2x scaler on this machine (they rank
    /// differently on real CPUs and under CPU emulation) and keeps the
    /// faster one. Needs a clock ([`Renderer::set_clock`]); returns the
    /// choice.
    pub fn calibrate(&mut self) -> Upscale2x {
        let Some(clock) = self.clock else { return self.upscale2x };
        let (w, h) = (self.width, self.height);
        let mut dst = vec![0u32; (w * h * 4) as usize];
        let mut best = (u64::MAX, Upscale2x::FastScalar);
        for round in 0..2 {
            for f in [Upscale2x::FastSimd, Upscale2x::FastScalar] {
                let t0 = clock();
                // SAFETY: the buffers hold w*h and (2w)*(2h) pixels.
                unsafe {
                    let func =
                        if f == Upscale2x::FastSimd { pipeline::upscale2x_fast } else { pipeline::upscale2x_swar };
                    func(self.color.as_ptr(), w, h, w, dst.as_mut_ptr(), 2 * w, 0, 2 * h);
                }
                let dt = clock() - t0;
                if round == 1 && dt < best.0 {
                    best = (dt, f);
                }
            }
        }
        self.upscale2x = best.1;
        best.1
    }

    /// Registers a texture for use in materials.
    pub fn add_texture(&mut self, texture: Texture) -> TextureId {
        self.textures.push(texture);
        TextureId((self.textures.len() - 1) as u32)
    }

    /// A registered texture.
    pub fn texture(&self, id: TextureId) -> Option<&Texture> {
        self.textures.get(id.0 as usize)
    }

    /// The rendered image (opaque `0xFFRRGGBB`, `width()` pixels per row).
    pub fn pixels(&self) -> &[u32] {
        &self.color
    }

    /// The depth buffer (larger = closer, 0 = nothing drawn).
    pub fn depth(&self) -> &[i32] {
        &self.depth
    }

    /// Starts a frame seen through `camera` and lit by `env`.
    pub fn frame<'a>(&'a mut self, camera: &Camera, env: &Environment) -> Frame<'a> {
        self.camera = *camera;
        self.env = env.clone();
        self.aspect = self.width as f32 / self.height as f32;
        self.view_proj = camera.projection(self.aspect) * camera.view();
        self.frustum = Frustum::from_matrix(&self.view_proj);
        self.draws.clear();
        self.dyn_vertices.clear();
        self.dyn_local.clear();
        self.dyn_chunks.clear();
        self.stats = Stats::default();
        // Background colours per row (assumes little camera roll).
        let h = self.height as usize;
        for y in 0..h {
            let ndc_y = 1.0 - 2.0 * (y as f32 + 0.5) / h as f32;
            let ray = camera.ray(0.0, ndc_y, self.aspect);
            self.row_colors[y] = self.env.background_at(ray.y) | 0xFF00_0000;
        }
        Frame { r: self, _meshes: PhantomData }
    }

    /// Builds the fixed-point transform and lighting parameters of a draw.
    fn make_xform(&self, model: &Mat4, mat: &Material, center_world: Vec3, radius_world: f32) -> Xform {
        let mut xf = Xform::default();
        let mvp = self.view_proj * *model;
        for r in 0..4 {
            for c in 0..4 {
                xf.mvp[r * 4 + c] = fx16(mvp.col(c)[r]);
            }
        }
        let env = &self.env;
        let inv = model.try_inverse().unwrap_or(Mat4::IDENTITY);
        let to_obj = |d: Vec3| inv.transform_vector3(d).normalize_or_zero();
        let sun = env.sun_direction.normalize_or(Vec3::Y);
        let base = rgb_of(mat.color);
        let alpha = alpha_of(mat.color);
        const MAXC: i32 = 4096;
        let rgb8 = |c: Vec3| [fx8(c.x, MAXC), fx8(c.y, MAXC), fx8(c.z, MAXC), 0];
        let mut flags = 0;
        match mat.shading {
            Shading::Unlit => {}
            Shading::Lambert | Shading::Phong { .. } => {
                flags |= pipeline::XF_LIT;
                xf.light_dir = dir14(to_obj(sun));
                xf.up_dir = dir14(to_obj(Vec3::Y));
                xf.sun_rgb = rgb8(env.sun_color * base);
                xf.sky_rgb = rgb8(env.sky_ambient * base);
                xf.ground_rgb = rgb8(env.ground_ambient * base);
                if let Shading::Phong { shininess, specular } = mat.shading {
                    flags |= pipeline::XF_SPECULAR;
                    let view_dir = (self.camera.position - center_world).normalize_or(Vec3::Y);
                    xf.half_dir = dir14(to_obj((sun + view_dir).normalize_or(sun)));
                    xf.spec_rgb = out_rgb(specular * env.sun_color);
                    xf.spec_power = (shininess.max(2.0).log2() + 0.5).clamp(1.0, 8.0) as i32;
                }
                // Point lights reaching the object, nearest first.
                let mut lights: [(f32, usize); 16] = [(0.0, 0); 16];
                let mut n = 0;
                for (i, l) in env.point_lights.iter().enumerate() {
                    let d = l.position.distance(center_world);
                    if d < l.radius + radius_world && n < lights.len() {
                        lights[n] = (d, i);
                        n += 1;
                    }
                }
                lights[..n].sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
                let scale = model.x_axis.xyz().length().max(1e-6);
                for (k, &(_, i)) in lights[..n.min(MAX_POINT_LIGHTS)].iter().enumerate() {
                    let l = &env.point_lights[i];
                    let p = inv.transform_point3(l.position);
                    let c = rgb8(l.color * base);
                    xf.points[k] = FxPointLight {
                        pos: [fx16(p.x), fx16(p.y), fx16(p.z)],
                        radius: fx16(l.radius / scale),
                        rgb: [c[0], c[1], c[2]],
                    };
                    xf.num_points = k as i32 + 1;
                }
            }
        }
        let b = rgb8(base);
        xf.base_rgba = [b[0], b[1], b[2], (alpha * 256.0 + 0.5) as i32];
        xf.emit_rgb = out_rgb(mat.emissive);
        if mat.vertex_colors {
            flags |= pipeline::XF_VCOLOR;
        }
        if mat.two_sided_lighting {
            flags |= pipeline::XF_TWO_SIDED;
        }
        if let (Some(fog), true) = (env.fog, mat.fog) {
            flags |= pipeline::XF_FOG;
            if matches!(mat.blend, Blend::Additive | Blend::Alpha) {
                flags |= pipeline::XF_FOG_FADE;
            }
            xf.fog_rgb = out_rgb(fog.color);
            xf.fog_start = fx16(fog.start);
            let range = (fog.end - fog.start).max(0.01);
            xf.fog_mul = ((256.0 * 65536.0) / range).min(i32::MAX as f32) as i32;
            xf.fog_max = (fog.max.clamp(0.0, 1.0) * 256.0) as i32;
        }
        xf.flags = flags;
        xf.near_w = fx16(self.camera.near.max(1e-4));
        xf.far_w = fx16(self.camera.far.min(30000.0));
        let half_w = self.width as i64 * 8; // half width in 28.4
        let half_h = self.height as i64 * 8;
        xf.vp_x = half_w as i32;
        xf.vp_y = half_h as i32;
        xf.vp_sx = half_w as i32;
        xf.vp_sy = half_h as i32;
        xf.guard = (((GUARD_PIXELS * 16) << 16) / half_w.max(half_h).max(1)) as i32;
        xf.q_scale = (xf.near_w.max(1) as i64) << 30;
        xf
    }

    fn make_setup(&self, mat: &Material) -> Setup {
        let tex = mat.texture.and_then(|t| self.textures.get(t.0 as usize));
        let mut mode = match mat.blend {
            Blend::Opaque => pipeline::M_OPAQUE,
            Blend::Alpha => pipeline::M_ALPHA,
            Blend::Additive => pipeline::M_ADD,
            Blend::Multiply => pipeline::M_MUL,
        };
        if tex.is_some() {
            mode |= pipeline::M_TEX;
            if mat.alpha_test {
                mode |= pipeline::M_ATEST;
            }
            if mat.bilinear {
                mode |= pipeline::M_BILINEAR;
            }
            if mat.affine {
                mode |= pipeline::M_AFFINE;
            }
        }
        if mat.depth_test {
            mode |= pipeline::M_ZTEST;
        }
        if mat.depth_write {
            mode |= pipeline::M_ZWRITE;
        }
        Setup {
            width: self.width,
            height: self.height,
            mode,
            cull: match mat.cull {
                Cull::Back => pipeline::CULL_BACK,
                Cull::Front => pipeline::CULL_FRONT,
                Cull::None => pipeline::CULL_NONE,
            },
            tex: tex.map(|t| &t.fx as *const pipeline::FxTexture).unwrap_or(core::ptr::null()),
            lod_bias: mat.lod_bias,
        }
    }

    /// Records a draw of `source` with world-space bounding sphere
    /// (`center`, `radius`).
    fn push_draw(&mut self, source: Source, model: &Mat4, mat: &Material, center: Vec3, radius: f32, sky: bool) {
        self.stats.draws += 1;
        if !sky && !self.frustum.intersects_sphere(center, radius) {
            self.stats.culled += 1;
            return;
        }
        let depth = (center - self.camera.position).dot(self.camera.forward) + mat.sort_offset;
        let xform = self.make_xform(model, mat, center, radius);
        let mut setup = self.make_setup(mat);
        let layer = if sky {
            // Only where no geometry was drawn: test depth, never write it.
            setup.mode = (setup.mode | pipeline::M_ZTEST) & !pipeline::M_ZWRITE;
            LAYER_SKY
        } else if mat.is_blended() {
            LAYER_BLENDED
        } else {
            LAYER_OPAQUE
        };
        self.draws.push(DrawCmd { source, xform, setup, depth, layer });
    }

    /// Renders the recorded draws into the internal image.
    fn render(&mut self) {
        let t0 = self.now();
        // Opaque front to back, the sky, then blended back to front.
        self.scratch_order.clear();
        for (i, d) in self.draws.iter().enumerate() {
            let key = if d.layer == LAYER_BLENDED { -d.depth } else { d.depth };
            self.scratch_order.push((key, i as u32));
        }
        let draws = &self.draws;
        self.scratch_order.sort_by(|a, b| {
            let (la, lb) = (draws[a.1 as usize].layer, draws[b.1 as usize].layer);
            la.cmp(&lb).then(a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal)).then(a.1.cmp(&b.1))
        });
        self.order.clear();
        self.order.extend(self.scratch_order.iter().map(|o| o.1));

        // Jobs: (draw, chunk) in drawing order, weighted by triangles.
        self.jobs.clear();
        self.weights.clear();
        let mut triangles = 0u32;
        for &di in &self.order {
            let d = &self.draws[di as usize];
            let chunks: &[Chunk] = match d.source {
                // SAFETY: the frame borrows the mesh for its whole lifetime.
                Source::Mesh(m) => unsafe { &(*m).chunks },
                Source::Dynamic { first, count } => &self.dyn_chunks[first as usize..(first + count) as usize],
            };
            for (ci, c) in chunks.iter().enumerate() {
                self.jobs.push(Job { draw: di, chunk: ci as u32 });
                self.weights.push(c.tri_count + c.vcount / 2 + 8);
                triangles += c.tri_count;
            }
        }
        self.stats.triangles = triangles;
        let threads = self.pool.threads();
        partition(&self.weights, threads, &mut self.ranges);
        for w in &mut self.workers {
            w.tris.clear();
            for b in &mut w.bins {
                b.clear();
            }
            w.triangles = 0;
            w.clipped = 0;
        }

        // Geometry phase.
        {
            let ctx = GeomCtx {
                draws: &self.draws,
                jobs: &self.jobs,
                dyn_vertices: &self.dyn_vertices,
                dyn_local: &self.dyn_local,
                dyn_chunks: &self.dyn_chunks,
                tiles_x: self.tiles_x,
            };
            let ranges = &self.ranges;
            let workers = SharedPtr(self.workers.as_mut_ptr());
            let nworkers = self.workers.len();
            self.pool.run(&|t| {
                if t >= nworkers || t >= ranges.len() {
                    return;
                }
                // SAFETY: every participant index is distinct, so worker `t`
                // is used by exactly one thread during this phase.
                let w = unsafe { &mut *workers.get().add(t) };
                let (a, b) = ranges[t];
                for j in a..b {
                    ctx.process(w, j);
                }
            });
        }
        let t1 = self.now();

        // Raster phase: tiles, most expensive first.
        let tiles = (self.tiles_x * self.tiles_y) as usize;
        self.tile_cost.clear();
        self.tile_cost.resize(tiles, 0);
        for w in &self.workers {
            for (i, b) in w.bins.iter().enumerate() {
                self.tile_cost[i] += b.len() as u32;
            }
            self.stats.rasterized += w.tris.len() as u32;
            self.stats.clipped += w.clipped;
        }
        self.tile_order.clear();
        self.tile_order.extend(0..tiles as u32);
        let cost = &self.tile_cost;
        self.tile_order.sort_unstable_by(|a, b| cost[*b as usize].cmp(&cost[*a as usize]));
        {
            let target = Target {
                color: self.color.as_mut_ptr(),
                depth: self.depth.as_mut_ptr(),
                stride: self.width,
                width: self.width,
                height: self.height,
            };
            let target = SharedPtr(&target as *const Target as *mut Target);
            let workers = &self.workers;
            let order = &self.tile_order;
            let rows = &self.row_colors;
            let (tx, w, h) = (self.tiles_x, self.width, self.height);
            let next = AtomicUsize::new(0);
            let mut counters = [pipeline::RasterStats::default(); 16];
            let counters_ptr = SharedPtr(counters.as_mut_ptr());
            let mut busy = [0u64; 16];
            let busy_ptr = SharedPtr(busy.as_mut_ptr());
            let mut delay = [0u64; 16];
            let delay_ptr = SharedPtr(delay.as_mut_ptr());
            let clock = self.clock;
            let phase_start = clock.map(|c| c()).unwrap_or(0);
            self.pool.run(&|p| {
                // SAFETY: each participant uses its own counter slot.
                let st = unsafe { &mut *counters_ptr.get().add(p.min(15)) };
                let start = clock.map(|c| c()).unwrap_or(0);
                // SAFETY: as above.
                unsafe { *delay_ptr.get().add(p.min(15)) = start.saturating_sub(phase_start) };
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= order.len() {
                        break;
                    }
                    let tile = order[i] as i32;
                    let (x0, y0) = ((tile % tx) * TILE_W, (tile / tx) * TILE_H);
                    let (x1, y1) = ((x0 + TILE_W).min(w), (y0 + TILE_H).min(h));
                    let t = target.get() as *const Target;
                    // SAFETY: tiles are disjoint, so each pixel of the buffers
                    // is written by one thread; the triangle and bin arrays
                    // are only read during this phase.
                    unsafe {
                        pipeline::clear_rect(&*t, x0, y0, x1, y1, rows);
                        for wk in workers {
                            let bin = &wk.bins[tile as usize];
                            if !bin.is_empty() {
                                pipeline::raster(&*t, x0, y0, x1, y1, &wk.tris, bin, st);
                            }
                        }
                    }
                }
                if let Some(c) = clock {
                    // SAFETY: each participant uses its own slot.
                    unsafe { *busy_ptr.get().add(p.min(15)) = c().saturating_sub(start) };
                }
            });
            for c in &counters {
                self.stats.spans += c.spans;
                self.stats.pixels += c.pixels;
            }
            self.stats.raster_cpu_ns = busy.iter().sum();
            for i in 0..8 {
                self.stats.raster_busy_us[i] = (busy[i] / 1000) as u32;
                self.stats.raster_delay_us[i] = (delay[i] / 1000) as u32;
            }
        }
        let t2 = self.now();
        self.stats.geometry_ns = t1.saturating_sub(t0);
        self.stats.raster_ns = t2.saturating_sub(t1);
    }

    /// Darkens the rendered image towards black by `amount` (0 = unchanged,
    /// 255 = black) before it is presented: a backdrop for menus that is
    /// much cheaper than dimming the scaled-up window image.
    pub fn darken(&mut self, amount: u8) {
        if amount == 0 {
            return;
        }
        let a = amount as u32;
        let k = 256 - (a + (a >> 7)); // 255 -> 0
        let (w, h) = (self.width as usize, self.height as usize);
        let pixels = SharedPtr(self.color.as_mut_ptr());
        let bands = (self.pool.threads() * 2).max(1);
        self.pool.for_each(bands, &|i| {
            let (y0, y1) = (h * i / bands, h * (i + 1) / bands);
            // SAFETY: bands cover disjoint rows of the w*h colour buffer.
            let rows = unsafe { core::slice::from_raw_parts_mut(pixels.get().add(y0 * w), (y1 - y0) * w) };
            for p in rows {
                let v = *p;
                *p = ((((v & 0x00FF_00FF) * k) >> 8) & 0x00FF_00FF)
                    | ((((v & 0x0000_FF00) * k) >> 8) & 0x0000_FF00)
                    | (v & 0xFF00_0000);
            }
        });
    }

    /// Scales the image into `dst` (`dst_w` x `dst_h` pixels, `dst_stride`
    /// pixels per row) with bilinear filtering; exactly 2x uses a faster
    /// path.
    pub fn present(&mut self, dst: &mut [u32], dst_stride: usize, dst_w: usize, dst_h: usize) {
        let t0 = self.now();
        if dst_w == 0 || dst_h == 0 || dst.len() < dst_stride * (dst_h - 1) + dst_w {
            return;
        }
        let (sw, sh) = (self.width, self.height);
        let src = SharedPtr(self.color.as_ptr() as *mut u32);
        let out = SharedPtr(dst.as_mut_ptr());
        let threads = self.pool.threads();
        let bands = (threads * 2).max(1);
        let two = dst_w == 2 * sw as usize && dst_h == 2 * sh as usize;
        let same = dst_w == sw as usize && dst_h == sh as usize;
        let up2: unsafe fn(*const u32, i32, i32, i32, *mut u32, i32, i32, i32) = match self.upscale2x {
            Upscale2x::Smooth => pipeline::upscale2x,
            Upscale2x::FastSimd => pipeline::upscale2x_fast,
            Upscale2x::FastScalar => pipeline::upscale2x_swar,
        };
        self.pool.for_each(bands, &|i| {
            let mut y0 = dst_h * i / bands;
            let mut y1 = dst_h * (i + 1) / bands;
            if two {
                y0 &= !1;
                y1 = if i + 1 == bands { dst_h } else { y1 & !1 };
            }
            let (y0, y1) = (y0 as i32, y1 as i32);
            let src = src.get() as *const u32;
            // SAFETY: bands cover disjoint destination rows; the source is
            // only read.
            unsafe {
                if two {
                    up2(src, sw, sh, sw, out.get(), dst_stride as i32, y0, y1);
                } else if same {
                    for y in y0..y1 {
                        core::ptr::copy_nonoverlapping(
                            src.add((y * sw) as usize),
                            out.get().add(y as usize * dst_stride),
                            sw as usize,
                        );
                    }
                } else {
                    let src = core::slice::from_raw_parts(src, (sw * sh) as usize);
                    pipeline::upscale(
                        src,
                        sw,
                        sh,
                        sw,
                        out.get(),
                        dst_w as i32,
                        dst_h as i32,
                        dst_stride as i32,
                        y0,
                        y1,
                    );
                }
            }
        });
        self.stats.present_ns = self.now().saturating_sub(t0);
    }
}

/// Read-only data shared by the geometry threads.
struct GeomCtx<'a> {
    draws: &'a [DrawCmd],
    jobs: &'a [Job],
    dyn_vertices: &'a [FxVertex],
    dyn_local: &'a [u32],
    dyn_chunks: &'a [Chunk],
    tiles_x: i32,
}

impl GeomCtx<'_> {
    /// Transforms, sets up and bins one chunk.
    fn process(&self, w: &mut Worker, job: usize) {
        let Job { draw, chunk } = self.jobs[job];
        let d = &self.draws[draw as usize];
        let (vertices, local, c): (&[FxVertex], &[u32], Chunk) = match d.source {
            Source::Mesh(m) => {
                // SAFETY: the frame borrows the mesh for its whole lifetime.
                let m = unsafe { &*m };
                (&m.packed, &m.local, m.chunks[chunk as usize])
            }
            Source::Dynamic { first, .. } => {
                (self.dyn_vertices, self.dyn_local, self.dyn_chunks[(first + chunk) as usize])
            }
        };
        let vs = &vertices[c.vmin as usize..(c.vmin + c.vcount) as usize];
        let idx = &local[c.tri_start as usize * 3..(c.tri_start + c.tri_count) as usize * 3];
        let n = c.tri_count as usize;
        w.tris.reserve(n);
        w.deferred.clear();
        let base = w.tris.len();
        pipeline::transform(&d.xform, vs, &mut w.tverts);
        // SAFETY: the draw's texture outlives the frame.
        unsafe { pipeline::setup_batch(&d.setup, &w.tverts, idx, &mut w.tris, &mut w.deferred) };
        w.triangles += c.tri_count;
        // Triangles crossing the near plane or the guard band.
        for k in 0..w.deferred.len() {
            let t = w.deferred[k] as usize;
            let (a, b, cc) =
                (w.tverts[idx[t * 3] as usize], w.tverts[idx[t * 3 + 1] as usize], w.tverts[idx[t * 3 + 2] as usize]);
            w.clipped += 1;
            let tris = &mut w.tris;
            clip_triangle(&d.xform, &a, &b, &cc, |p, q, r| {
                // SAFETY: the draw's texture outlives the frame.
                unsafe { pipeline::setup_tri(&d.setup, p, q, r, tris) };
            });
        }
        // Bin the new triangles.
        let tx = self.tiles_x;
        for k in base..w.tris.len() {
            let t = &w.tris[k];
            let (x0, x1) = (t.x0 >> TILE_W_SHIFT, (t.x1 - 1) >> TILE_W_SHIFT);
            let (y0, y1) = (t.y0 >> TILE_H_SHIFT, (t.y1 - 1) >> TILE_H_SHIFT);
            for ty in y0..=y1 {
                for txi in x0..=x1 {
                    w.bins[(ty * tx + txi) as usize].push(k as u32);
                }
            }
        }
    }
}

/// One frame being recorded; rendering happens when it is finished (or
/// dropped).
pub struct Frame<'a> {
    r: &'a mut Renderer,
    _meshes: PhantomData<&'a Mesh>,
}

impl<'a> Frame<'a> {
    /// The frame's camera.
    pub fn camera(&self) -> &Camera {
        &self.r.camera
    }

    /// Internal image size.
    pub fn size(&self) -> (usize, usize) {
        (self.r.width as usize, self.r.height as usize)
    }

    /// Draws `mesh` transformed by `model`.
    pub fn draw(&mut self, mesh: &'a Mesh, model: &Mat4, material: &Material) {
        if mesh.triangle_count() == 0 {
            return;
        }
        let b = mesh.bounds();
        let center = model.transform_point3(b.center);
        let scale = model.x_axis.xyz().length().max(model.y_axis.xyz().length()).max(model.z_axis.xyz().length());
        self.r.push_draw(Source::Mesh(mesh as *const Mesh), model, material, center, b.radius * scale, false);
    }

    /// Draws a background mesh (sky dome, nebula, star sphere) after all
    /// opaque geometry, only where nothing else was drawn. It must lie
    /// inside the far plane; usually it is centred on the camera.
    pub fn draw_sky(&mut self, mesh: &'a Mesh, model: &Mat4, material: &Material) {
        if mesh.triangle_count() == 0 {
            return;
        }
        let b = mesh.bounds();
        let center = model.transform_point3(b.center);
        self.r.push_draw(Source::Mesh(mesh as *const Mesh), model, material, center, b.radius, true);
    }

    /// Draws world-space triangles given as vertices and indices
    /// (temporary geometry: shadows, trails, decals).
    pub fn draw_triangles(&mut self, vertices: &[Vertex], indices: &[u32], material: &Material) {
        if vertices.is_empty() || indices.len() < 3 {
            return;
        }
        let r = &mut *self.r;
        let base = r.dyn_vertices.len() as u32;
        r.dyn_vertices.extend(vertices.iter().map(pack_vertex));
        r.scratch_idx.clear();
        let n = vertices.len() as u32;
        for t in indices.as_chunks::<3>().0 {
            if t.iter().all(|&i| i < n) {
                r.scratch_idx.extend(t.iter().map(|&i| i + base));
            }
        }
        let first_chunk = r.dyn_chunks.len();
        let mut chunks = Vec::new();
        let mut local = Vec::new();
        build_chunks(&r.scratch_idx, &mut local, &mut chunks);
        let tri_base = (r.dyn_local.len() / 3) as u32;
        r.dyn_local.extend_from_slice(&local);
        for mut c in chunks {
            c.tri_start += tri_base;
            r.dyn_chunks.push(c);
        }
        let count = (r.dyn_chunks.len() - first_chunk) as u32;
        if count == 0 {
            return;
        }
        let bounds = crate::mesh::Bounds::of_points(vertices.iter().map(|v| &v.position));
        r.push_draw(
            Source::Dynamic { first: first_chunk as u32, count },
            &Mat4::IDENTITY,
            material,
            bounds.center,
            bounds.radius,
            false,
        );
    }

    /// Draws beams (lasers, trails): quads from `start` to `end` that turn
    /// around their axis to face the camera.
    pub fn beams(&mut self, material: &Material, items: &[Beam]) {
        if items.is_empty() {
            return;
        }
        let cam = self.r.camera;
        let mut verts = Vec::with_capacity(items.len() * 4);
        let mut idx = Vec::with_capacity(items.len() * 6);
        for b in items {
            let axis = b.end - b.start;
            let mid = (b.start + b.end) * 0.5;
            let to_cam = cam.position - mid;
            let side = axis.cross(to_cam).normalize_or(cam.right()) * (b.width * 0.5);
            let n = to_cam.normalize_or(-cam.forward);
            let base = verts.len() as u32;
            verts.push(Vertex::new(b.start - side, n, Vec2::new(0.0, 1.0), b.color));
            verts.push(Vertex::new(b.end - side, n, Vec2::new(0.0, 0.0), b.color));
            verts.push(Vertex::new(b.end + side, n, Vec2::new(1.0, 0.0), b.color));
            verts.push(Vertex::new(b.start + side, n, Vec2::new(1.0, 1.0), b.color));
            idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        let mut m = *material;
        m.cull = Cull::None;
        self.draw_triangles(&verts, &idx, &m);
    }

    /// Draws camera-facing quads. Alpha-blended billboards are sorted back
    /// to front.
    ///
    /// Blended billboards fade out as they grow to fill the view (from
    /// [`BILLBOARD_FADE_START`] of the view height) and are skipped beyond
    /// [`BILLBOARD_FADE_END`]: such quads cost a full-screen blend each but
    /// add little but a tint, and a burst of them next to the camera would
    /// otherwise stall the frame.
    pub fn billboards(&mut self, material: &Material, items: &[Billboard]) {
        if items.is_empty() {
            return;
        }
        let cam = self.r.camera;
        let (right, up) = (cam.right(), cam.true_up());
        let normal = -cam.forward;
        let fade = !matches!(material.blend, Blend::Opaque);
        // Projected height / view height = size * inv_view / depth.
        let inv_view = 0.5 / FloatExt::tan(cam.fov_y * 0.5).max(1e-3);
        let mut order: Vec<(f32, usize)> =
            items.iter().enumerate().map(|(i, b)| ((b.position - cam.position).dot(cam.forward), i)).collect();
        if matches!(material.blend, Blend::Alpha) {
            order.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(core::cmp::Ordering::Equal));
        }
        // Build in pieces of CHUNK_TRIS / 2 quads (one chunk each).
        for piece in order.chunks(CHUNK_TRIS / 2) {
            let mut verts = Vec::with_capacity(piece.len() * 4);
            let mut idx = Vec::with_capacity(piece.len() * 6);
            for &(depth, i) in piece {
                if depth < cam.near {
                    continue;
                }
                let b = &items[i];
                let mut color = b.color;
                if fade {
                    let cover = b.size.x.max(b.size.y) * inv_view / depth;
                    if cover >= BILLBOARD_FADE_END {
                        continue;
                    }
                    if cover > BILLBOARD_FADE_START {
                        let k = (BILLBOARD_FADE_END - cover) / (BILLBOARD_FADE_END - BILLBOARD_FADE_START);
                        let a = ((color >> 24) as f32 * k) as u32;
                        if a == 0 {
                            continue;
                        }
                        color = (color & 0x00FF_FFFF) | (a << 24);
                    }
                }
                let (s, c) = FloatExt::sin_cos(b.rotation);
                let hx = b.size.x * 0.5;
                let hy = b.size.y * 0.5;
                let ax = right * (c * hx) + up * (s * hx);
                let ay = up * (c * hy) - right * (s * hy);
                let base = verts.len() as u32;
                let [u0, v0, u1, v1] = b.uv;
                verts.push(Vertex::new(b.position - ax - ay, normal, Vec2::new(u0, v1), color));
                verts.push(Vertex::new(b.position + ax - ay, normal, Vec2::new(u1, v1), color));
                verts.push(Vertex::new(b.position + ax + ay, normal, Vec2::new(u1, v0), color));
                verts.push(Vertex::new(b.position - ax + ay, normal, Vec2::new(u0, v0), color));
                idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
            let mut m = *material;
            m.cull = Cull::None;
            self.draw_triangles(&verts, &idx, &m);
        }
    }

    /// Renders the frame now (same as dropping it).
    pub fn finish(self) {}
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        self.r.render();
    }
}
