//! The software renderer: OpenGL ES 3.0 on the CPU, on every core.
//!
//! Draws are not rendered when they are made. Each draw's vertices are
//! shaded at once (in parallel for large draws), its primitives clipped,
//! set up and sorted into the tiles of a *scene*; the scene is rendered
//! when its results are needed — the framebuffer changes, its images are
//! read or written, `glFlush`, `glFinish` — with the tiles shared among the
//! workers. Clears are recorded the same way. Shaders run on the SIMD
//! interpreter of [`vglsl::interp`].
//!
//! Everything a recorded draw needs later is kept with it: its uniforms
//! and uniform buffer contents are copied, and textures it samples or
//! renders to are not changed before the scene is rendered (writing one
//! renders the scene first).

mod ops;
pub mod program;
mod raster;
pub mod resource;
mod scene;
mod target;
pub mod texture;
pub(crate) mod vertex;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use vglsl::interp::{Env, Exec, Lanes};

use crate::backend::*;
use program::SoftProgram;
use resource::Resource;
use scene::{ClearCmd, DrawRecord, FragState, Scene, Setup, Targets};
use target::RenderTarget;
use texture::{DrawTextures, Sampled, TexLevel};

/// Runs work on several threads.
pub trait Workers: Send + Sync {
    /// Threads taking part (the caller included).
    fn threads(&self) -> usize;
    /// Runs `f(i)` for every thread `i` in `0..threads()` and waits for all
    /// of them.
    fn run(&self, f: &(dyn Fn(usize) + Sync));
}

/// Everything on the calling thread.
pub struct Serial;

impl Workers for Serial {
    fn threads(&self) -> usize {
        1
    }

    fn run(&self, f: &(dyn Fn(usize) + Sync)) {
        f(0);
    }
}

/// A scene is rendered once it holds this much primitive data.
const SCENE_LIMIT: usize = 64 << 20;

/// Draws with at least this many vertices shade them in parallel.
const PARALLEL_VERTICES: usize = 2048;

/// A raw pointer that may cross threads (the code using it keeps
/// accesses disjoint).
#[derive(Clone, Copy)]
struct SyncPtr<T>(*mut T);
// SAFETY: see the type's documentation.
unsafe impl<T> Send for SyncPtr<T> {}
unsafe impl<T> Sync for SyncPtr<T> {}

impl<T> SyncPtr<T> {
    /// The pointer (a method, so that closures capture the whole wrapper).
    fn get(self) -> *mut T {
        self.0
    }
}

/// The software renderer.
pub struct SoftBackend {
    caps: Caps,
    resources: Vec<Option<Resource>>,
    free: Vec<u32>,
    programs: BTreeMap<ProgramId, Arc<SoftProgram>>,
    next_program: ProgramId,
    queries: BTreeMap<QueryId, Arc<AtomicU64>>,
    next_query: QueryId,
    /// The occlusion query draws count into.
    active_query: Option<Arc<AtomicU64>>,
    scene: Option<Scene>,
    workers: Box<dyn Workers>,
    fences: u64,
    /// The vertex shader's register file.
    exec: Option<Exec>,
}

impl SoftBackend {
    /// A renderer using `workers` for the parallel parts.
    pub fn new(workers: Box<dyn Workers>) -> SoftBackend {
        SoftBackend {
            caps: Caps {
                name: "Veda software renderer",
                max_texture_size: 8192,
                max_3d_texture_size: 2048,
                max_cube_map_size: 8192,
                max_array_layers: 2048,
                max_renderbuffer_size: 8192,
                max_samples: 4,
                max_viewport: 16384,
                color_buffer_float: true,
                float_linear: true,
                line_width: (1.0, 16.0),
                point_size: (1.0, 1024.0),
                max_anisotropy: 1.0,
            },
            resources: Vec::new(),
            free: Vec::new(),
            programs: BTreeMap::new(),
            next_program: 1,
            queries: BTreeMap::new(),
            next_query: 1,
            active_query: None,
            scene: None,
            workers,
            fences: 0,
            exec: None,
        }
    }

    fn res(&self, id: ResourceId) -> Option<&Resource> {
        self.resources.get(id.checked_sub(1)? as usize)?.as_ref()
    }

    fn res_mut(&mut self, id: ResourceId) -> Option<&mut Resource> {
        self.resources.get_mut(id.checked_sub(1)? as usize)?.as_mut()
    }

    /// Renders the scene if it uses `id`.
    fn flush_if_uses(&mut self, id: ResourceId) {
        if self.scene.as_ref().is_some_and(|s| s.uses(id)) {
            self.render();
        }
    }

    /// A render target for a surface.
    fn target(&mut self, s: Surface) -> Option<RenderTarget> {
        let r = self.res_mut(s.resource)?;
        let lv = *r.level(s.level)?;
        if s.layer >= lv.layers {
            return None;
        }
        let offset = lv.offset + s.layer as usize * lv.image;
        let ptr = r.data.as_mut_ptr().wrapping_add(offset);
        Some(RenderTarget {
            ptr,
            width: lv.width,
            height: lv.height,
            row: lv.row,
            texel: lv.texel,
            format: r.desc.format,
            samples: r.samples,
        })
    }

    /// The scene for a framebuffer, rendering the current one first if it
    /// draws elsewhere.
    fn scene_for(&mut self, fb: &Framebuffer) -> Option<&mut Scene> {
        if self.scene.as_ref().is_some_and(|s| s.framebuffer != *fb) {
            self.render();
        }
        if self.scene.is_none() {
            let mut t = Targets { width: fb.width, height: fb.height, samples: fb.samples, ..Default::default() };
            let mut uses = Vec::new();
            for (i, c) in fb.colors.iter().enumerate() {
                if let Some(s) = c {
                    t.colors[i] = self.target(*s);
                    uses.push(s.resource);
                }
            }
            if let Some(s) = fb.depth {
                t.depth = self.target(s);
                uses.push(s.resource);
            }
            if let Some(s) = fb.stencil {
                t.stencil = self.target(s);
                uses.push(s.resource);
            }
            // Never touch pixels outside every attachment.
            let mut w = fb.width;
            let mut h = fb.height;
            for x in t.colors.iter().flatten().chain(t.depth.iter()).chain(t.stencil.iter()) {
                w = w.min(x.width);
                h = h.min(x.height);
            }
            t.width = w;
            t.height = h;
            let mut scene = Scene::new(*fb, t);
            for u in uses {
                scene.add_use(u);
            }
            self.scene = Some(scene);
        }
        self.scene.as_mut()
    }

    /// Renders the recorded scene.
    fn render(&mut self) {
        let Some(scene) = self.scene.take() else { return };
        if scene.is_empty() {
            return;
        }
        let envs: Vec<raster::DrawEnv<'_>> = scene
            .draws
            .iter()
            .map(|d| raster::DrawEnv {
                blocks: d.blocks.iter().map(Vec::as_slice).collect(),
                textures: DrawTextures { bound: &d.textures },
            })
            .collect();
        let registers = scene.draws.iter().map(|d| d.program.registers).max().unwrap_or(1);
        let tiles = scene.bins.len();
        let next = AtomicUsize::new(0);
        let job = |_: usize| {
            let mut w = raster::Worker::new(registers);
            loop {
                let t = next.fetch_add(1, Ordering::Relaxed);
                if t >= tiles {
                    break;
                }
                w.tile(&scene, &envs, t);
            }
        };
        self.workers.run(&job);
    }

    /// The textures a draw samples, with the scene marked as using them.
    fn sampled(&mut self, bindings: &[TextureBinding]) -> Vec<Option<Sampled>> {
        let mut out = Vec::with_capacity(bindings.len());
        for b in bindings {
            let s = b.view.and_then(|v| {
                let r = self.res(v.resource)?;
                let dim = match v.target {
                    Target::Texture3D => vglsl::types::Dim::D3,
                    Target::TextureCube => vglsl::types::Dim::Cube,
                    Target::Texture2DArray => vglsl::types::Dim::D2Array,
                    _ => vglsl::types::Dim::D2,
                };
                let levels: Vec<TexLevel> = (v.base_level..=v.max_level)
                    .filter_map(|l| r.level(l))
                    .map(|lv| TexLevel {
                        ptr: r.data.as_ptr().wrapping_add(lv.offset),
                        width: lv.width,
                        height: lv.height,
                        depth: lv.layers,
                        row: lv.row,
                        image: lv.image,
                    })
                    .collect();
                if levels.is_empty() || r.samples > 1 {
                    return None;
                }
                Some(Sampled {
                    levels,
                    dim,
                    format: r.desc.format,
                    texel: r.desc.format.bytes(),
                    kind: Sampled::kind_of(r.desc.format),
                    swizzle: v.swizzle,
                    state: b.sampler,
                })
            });
            if let (Some(v), Some(scene)) = (b.view, self.scene.as_mut())
                && s.is_some()
            {
                scene.add_use(v.resource);
            }
            out.push(s);
        }
        out
    }

    /// Copies each uniform block's buffer range (zero-filled to the block's
    /// size).
    fn block_contents(&self, program: &SoftProgram, ranges: &[Option<BufferRange>]) -> Vec<Vec<u8>> {
        let infos = &program.program.linked.blocks;
        infos
            .iter()
            .enumerate()
            .map(|(i, info)| {
                let mut v = vec![0u8; info.size as usize];
                if let Some(Some(r)) = ranges.get(i)
                    && let Some(res) = self.res(r.buffer)
                {
                    let start = r.offset.min(res.data.len());
                    let end = start.saturating_add(r.size.min(info.size as usize)).min(res.data.len());
                    v[..end - start].copy_from_slice(&res.data[start..end]);
                }
                v
            })
            .collect()
    }
}

/// Indices of a draw, read from its element buffer (as far as the buffer
/// goes).
pub(crate) fn read_indices(data: &[u8], offset: usize, count: u32, ty: IndexType) -> Vec<u32> {
    let size = ty.bytes();
    let available = data.len().saturating_sub(offset) / size;
    let n = (count as usize).min(available);
    let mut out = Vec::new();
    if out.try_reserve_exact(n).is_err() {
        return out;
    }
    let b = &data[offset..offset + n * size];
    match ty {
        IndexType::U8 => out.extend(b.iter().map(|&x| u32::from(x))),
        IndexType::U16 => out.extend(b.as_chunks::<2>().0.iter().map(|c| u32::from(u16::from_le_bytes([c[0], c[1]])))),
        IndexType::U32 => out.extend(b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
    }
    out
}

/// A primitive of the vertex stream: vertex positions, the provoking
/// vertex last for triangles; lines carry their provoking vertex.
#[derive(Clone, Copy)]
enum Prim {
    Tri(u32, u32, u32),
    Line(u32, u32, u32),
    Point(u32),
}

/// Calls `f` with each primitive of a vertex stream (section 2.8.1),
/// splitting it at restart indices. Triangles keep the stream's winding.
fn assemble(mode: Mode, stream: &[Option<u32>], mut f: impl FnMut(Prim)) {
    for part in stream.split(Option::is_none) {
        let v: Vec<u32> = part.iter().map(|x| x.unwrap_or(0)).collect();
        let n = v.len();
        match mode {
            Mode::Points => v.iter().for_each(|&a| f(Prim::Point(a))),
            Mode::Lines => {
                for k in 0..n / 2 {
                    f(Prim::Line(v[2 * k], v[2 * k + 1], v[2 * k + 1]));
                }
            }
            Mode::LineStrip | Mode::LineLoop => {
                for k in 1..n {
                    f(Prim::Line(v[k - 1], v[k], v[k]));
                }
                if mode == Mode::LineLoop && n >= 2 {
                    // The closing segment's provoking vertex is the first.
                    f(Prim::Line(v[n - 1], v[0], v[0]));
                }
            }
            Mode::Triangles => {
                for k in 0..n / 3 {
                    f(Prim::Tri(v[3 * k], v[3 * k + 1], v[3 * k + 2]));
                }
            }
            Mode::TriangleStrip => {
                for k in 2..n {
                    if k % 2 == 0 {
                        f(Prim::Tri(v[k - 2], v[k - 1], v[k]));
                    } else {
                        f(Prim::Tri(v[k - 1], v[k - 2], v[k]));
                    }
                }
            }
            Mode::TriangleFan => {
                for k in 2..n {
                    f(Prim::Tri(v[0], v[k - 1], v[k]));
                }
            }
        }
    }
}

impl Backend for SoftBackend {
    fn caps(&self) -> &Caps {
        &self.caps
    }

    fn create_resource(&mut self, desc: &ResourceDesc) -> Result<ResourceId, OutOfMemory> {
        let r = Resource::new(desc)?;
        match self.free.pop() {
            Some(i) => {
                self.resources[i as usize] = Some(r);
                Ok(i + 1)
            }
            None => {
                self.resources.try_reserve(1).map_err(|_| OutOfMemory)?;
                self.resources.push(Some(r));
                Ok(self.resources.len() as u32)
            }
        }
    }

    fn destroy_resource(&mut self, id: ResourceId) {
        self.flush_if_uses(id);
        if let Some(slot) = id.checked_sub(1).and_then(|i| self.resources.get_mut(i as usize))
            && slot.take().is_some()
        {
            self.free.push(id - 1);
        }
    }

    fn write(&mut self, id: ResourceId, level: u32, region: Region, data: &[u8], row_pitch: usize, image_pitch: usize) {
        self.flush_if_uses(id);
        if let Some(r) = self.res_mut(id) {
            r.write(level, region, data, row_pitch, image_pitch);
        }
    }

    fn read(
        &mut self,
        id: ResourceId,
        level: u32,
        region: Region,
        out: &mut [u8],
        row_pitch: usize,
        image_pitch: usize,
    ) {
        self.flush_if_uses(id);
        if let Some(r) = self.res(id) {
            r.read(level, region, out, row_pitch, image_pitch);
        }
    }

    fn copy_buffer(&mut self, src: ResourceId, src_offset: usize, dst: ResourceId, dst_offset: usize, size: usize) {
        self.flush_if_uses(dst);
        let Some(bytes) =
            self.res(src).and_then(|r| r.data.get(src_offset..src_offset.checked_add(size)?)).map(<[u8]>::to_vec)
        else {
            return;
        };
        if let Some(d) = self.res_mut(dst).and_then(|r| r.data.get_mut(dst_offset..dst_offset.checked_add(size)?)) {
            d.copy_from_slice(&bytes);
        }
    }

    fn copy_region(
        &mut self,
        src: ResourceId,
        src_level: u32,
        region: Region,
        dst: ResourceId,
        dst_level: u32,
        x: u32,
        y: u32,
        z: u32,
    ) {
        self.flush_if_uses(src);
        self.flush_if_uses(dst);
        ops::copy_region(self, src, src_level, region, dst, dst_level, [x, y, z]);
    }

    fn copy_to_texture(&mut self, src: &Framebuffer, read: u8, rect: Rect, dst: Surface, x: u32, y: u32) {
        self.render();
        ops::copy_to_texture(self, src, read, rect, dst, x, y);
    }

    fn blit(&mut self, blit: &Blit) {
        self.render();
        ops::blit(self, blit);
    }

    fn generate_mipmap(&mut self, id: ResourceId, base: u32, last: u32) {
        self.flush_if_uses(id);
        ops::generate_mipmap(self, id, base, last);
    }

    fn create_program(&mut self, program: Arc<vglsl::program::Program>) -> ProgramId {
        let id = self.next_program;
        self.next_program = self.next_program.wrapping_add(1).max(1);
        self.programs.insert(id, Arc::new(SoftProgram::new(program)));
        id
    }

    fn destroy_program(&mut self, id: ProgramId) {
        // Recorded draws hold their own reference.
        self.programs.remove(&id);
    }

    fn draw(&mut self, state: &DrawState<'_>, info: &DrawInfo) {
        let Some(prog) = self.programs.get(&state.program).cloned() else { return };
        // Uniform blocks and textures as this draw sees them.
        let blocks = self.block_contents(&prog, state.blocks);
        let fb = *state.framebuffer;
        if self.scene_for(&fb).is_none() {
            return;
        }
        let textures = self.sampled(state.textures);
        let block_refs: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
        let tex = DrawTextures { bound: &textures };
        let env = Env { uniforms: state.uniforms, blocks: &block_refs, textures: &tex };
        let mut exec = self.exec.take().unwrap_or_else(|| Exec::new(&prog.vs));
        if exec.regs.len() < prog.registers {
            exec.regs.resize(prog.registers, Lanes::ZERO);
        }
        // The fragment shader's per-draw values.
        exec.prologue(&prog.fs, &env);
        let prologue = exec.regs[..prog.fs.uniform_registers].to_vec();
        exec.prologue(&prog.vs, &env);
        let vs_prologue = exec.regs[..prog.vs.uniform_registers].to_vec();
        // Vertex inputs.
        let resources = &self.resources;
        let get = |id: ResourceId| resources.get(id.checked_sub(1)? as usize)?.as_ref();
        let fetch: Vec<vertex::Fetch<'_>> = state
            .attribs
            .iter()
            .map(|a| vertex::Fetch {
                data: a.buffer.and_then(get).map(|r| r.data.as_slice()),
                offset: a.offset,
                stride: a.stride as usize,
                size: a.size,
                ty: a.ty,
                normalized: a.normalized,
                integer: a.integer,
                divisor: a.divisor,
                current: a.current,
            })
            .collect();
        // The vertex stream: indices, or consecutive vertices.
        let stream: Vec<Option<u32>> = match state.index {
            Some((buffer, ty)) => {
                let data = get(buffer).map_or(&[][..], |r| r.data.as_slice());
                let restart = ty.restart();
                read_indices(data, info.first, info.count, ty)
                    .into_iter()
                    .map(|i| if state.primitive_restart && i == restart { None } else { Some(i) })
                    .collect()
            }
            None => (0..info.count).map(|k| Some((info.first as u32).wrapping_add(k))).collect(),
        };
        // Each vertex is shaded once: the distinct indices, in order.
        let mut ids: Vec<u32> = stream.iter().flatten().copied().collect();
        if state.index.is_some() {
            ids.sort_unstable();
            ids.dedup();
        }
        let words = vertex::vertex_words(&prog);
        let feedback = state.feedback.filter(|_| !prog.feedback.is_empty());
        let ncap = prog.feedback.len();
        let mut out: Vec<u32> = Vec::new();
        let mut captured: Vec<u32> = Vec::new();
        let mut recorded: Vec<u32> = Vec::new();
        if out.try_reserve_exact(ids.len() * words).is_err()
            || (feedback.is_some() && captured.try_reserve_exact(ids.len() * ncap).is_err())
        {
            self.exec = Some(exec);
            return;
        }
        out.resize(ids.len() * words, 0);
        if feedback.is_some() {
            captured.resize(ids.len() * ncap, 0);
        }
        let draw_state = FragState {
            depth_stencil: state.depth_stencil,
            blend: state.blend,
            alpha_to_coverage: state.raster.alpha_to_coverage,
            sample_coverage: state.raster.sample_coverage,
            draw_buffers: fb.draw_buffers,
            early: !prog.discards
                && prog.frag_depth.is_none()
                && !(fb.samples > 1 && (state.raster.alpha_to_coverage || state.raster.sample_coverage.is_some())),
        };
        let scene = self.scene.as_mut().unwrap();
        let draw_index = scene.draws.len() as u32;
        scene.draws.push(DrawRecord {
            program: prog.clone(),
            uniforms: state.uniforms.to_vec(),
            prologue,
            blocks: blocks.clone(),
            textures: textures.clone(),
            state: draw_state,
            query: self.active_query.clone(),
        });
        // Rasterization bounds.
        let fbw = scene.targets.width as i32;
        let fbh = scene.targets.height as i32;
        let mut clip_rect = [0, 0, fbw, fbh];
        if let Some(s) = state.scissor {
            let x1 = s.x.saturating_add(s.w);
            let y1 = s.y.saturating_add(s.h);
            clip_rect = [clip_rect[0].max(s.x), clip_rect[1].max(s.y), clip_rect[2].min(x1), clip_rect[3].min(y1)];
        }
        let vp = state.viewport;
        let vpr = [
            vmath::f32::floor(vp.x) as i32,
            vmath::f32::floor(vp.y) as i32,
            vmath::f32::ceil(vp.x + vp.w) as i32,
            vmath::f32::ceil(vp.y + vp.h) as i32,
        ];
        let tri_rect =
            [clip_rect[0].max(vpr[0]), clip_rect[1].max(vpr[1]), clip_rect[2].min(vpr[2]), clip_rect[3].min(vpr[3])];
        let depth_format =
            fb.depth.and_then(|s| self.resources.get(s.resource as usize - 1)?.as_ref().map(|r| r.desc.format));
        let (point_size, line_width) = (self.caps.point_size, self.caps.line_width);
        let threads = self.workers.threads();
        for instance in 0..info.instances {
            // Shade the vertices.
            let parallel = threads > 1 && ids.len() >= PARALLEL_VERTICES;
            if parallel {
                let chunk = 1024usize;
                let chunks = ids.len().div_ceil(chunk);
                let next = AtomicUsize::new(0);
                let out_ptr = SyncPtr(out.as_mut_ptr());
                let cap_ptr = SyncPtr(captured.as_mut_ptr());
                let has_cap = feedback.is_some();
                let (prog_ref, fetch_ref, ids_ref, vsp) = (&prog, &fetch, &ids, &vs_prologue);
                // The environment is rebuilt in each worker from its
                // thread-safe parts.
                let (uniforms, blocks_ref, tex_ref) = (state.uniforms, &block_refs, &tex);
                let registers = prog.registers;
                let job = move |_: usize| {
                    let env = Env { uniforms, blocks: blocks_ref, textures: tex_ref };
                    let mut ex = Exec::new(&prog_ref.vs);
                    if ex.regs.len() < registers {
                        ex.regs.resize(registers, Lanes::ZERO);
                    }
                    ex.regs[..vsp.len()].copy_from_slice(vsp);
                    loop {
                        let c = next.fetch_add(1, Ordering::Relaxed);
                        if c >= chunks {
                            break;
                        }
                        let (a, b) = (c * chunk, ((c + 1) * chunk).min(ids_ref.len()));
                        // SAFETY: chunks are disjoint ranges of `out` and
                        // `captured`, each handed to one worker.
                        let o =
                            unsafe { core::slice::from_raw_parts_mut(out_ptr.get().add(a * words), (b - a) * words) };
                        let cap = if has_cap {
                            Some(unsafe {
                                core::slice::from_raw_parts_mut(cap_ptr.get().add(a * ncap), (b - a) * ncap)
                            })
                        } else {
                            None
                        };
                        vertex::shade(prog_ref, fetch_ref, &env, &mut ex, &ids_ref[a..b], instance, o, cap);
                    }
                };
                self.workers.run(&job);
            } else {
                exec.regs[..vs_prologue.len()].copy_from_slice(&vs_prologue);
                let cap = if feedback.is_some() { Some(&mut captured[..]) } else { None };
                vertex::shade(&prog, &fetch, &env, &mut exec, &ids, instance, &mut out, cap);
            }
            // Where each stream entry's vertex is.
            let slot = |i: u32| -> u32 {
                if state.index.is_some() {
                    ids.binary_search(&i).map_or(0, |s| s as u32)
                } else {
                    i.wrapping_sub(info.first as u32)
                }
            };
            let positions: Vec<Option<u32>> = stream.iter().map(|x| x.map(slot)).collect();
            // Transform feedback records whole primitives in order.
            if feedback.is_some() {
                let per = match info.mode {
                    Mode::Points => 1,
                    Mode::Lines => 2,
                    _ => 3,
                };
                let whole = positions.len() / per * per;
                for &p in positions.iter().take(whole).flatten() {
                    let v = p as usize * ncap;
                    recorded.extend_from_slice(&captured[v..v + ncap]);
                }
            }
            let scene = self.scene.as_mut().unwrap();
            let mut setup = Setup {
                scene,
                draw: draw_index,
                prog: &prog,
                viewport: vp,
                raster: state.raster,
                clip_rect,
                tri_rect,
                depth_format,
                point_size,
                line_width,
            };
            let vtx = |p: u32| &out[p as usize * words..(p as usize + 1) * words];
            if !state.raster.discard {
                assemble(info.mode, &positions, |p| match p {
                    Prim::Tri(a, b, c) => setup.triangle([vtx(a), vtx(b), vtx(c)]),
                    Prim::Line(a, b, pv) => setup.line(vtx(a), vtx(b), vtx(pv)),
                    Prim::Point(a) => setup.point(vtx(a)),
                });
            }
        }
        self.exec = Some(exec);
        drop(fetch);
        // Write what transform feedback recorded.
        if let Some(ranges) = feedback {
            ops::write_feedback(self, &prog, ranges, &recorded);
        }
        if self.scene.as_ref().is_some_and(|s| s.size() > SCENE_LIMIT) {
            self.render();
        }
    }

    fn clear(&mut self, framebuffer: &Framebuffer, clear: &Clear) {
        let Some(scene) = self.scene_for(framebuffer) else { return };
        let (w, h) = (scene.targets.width as i32, scene.targets.height as i32);
        let mut rect = [0, 0, w, h];
        if let Some(s) = clear.scissor {
            rect = [
                rect[0].max(s.x),
                rect[1].max(s.y),
                rect[2].min(s.x.saturating_add(s.w)),
                rect[3].min(s.y.saturating_add(s.h)),
            ];
        }
        scene.clear(ClearCmd {
            colors: clear.colors,
            depth: clear.depth,
            stencil: clear.stencil,
            stencil_mask: clear.stencil_mask,
            color_mask: clear.color_mask,
            rect,
        });
    }

    fn begin_query(&mut self, _kind: QueryKind) -> QueryId {
        let id = self.next_query;
        self.next_query = self.next_query.wrapping_add(1).max(1);
        let counter = Arc::new(AtomicU64::new(0));
        self.queries.insert(id, counter.clone());
        self.active_query = Some(counter);
        id
    }

    fn end_query(&mut self, _id: QueryId) {
        self.active_query = None;
    }

    fn destroy_query(&mut self, id: QueryId) {
        self.queries.remove(&id);
    }

    fn query_result(&mut self, id: QueryId, _wait: bool) -> Option<u64> {
        // The scene may count into the query: render it first.
        self.render();
        self.queries.get(&id).map(|c| c.load(Ordering::Relaxed))
    }

    fn flush(&mut self) {
        self.render();
    }

    fn finish(&mut self) {
        self.render();
    }

    fn format_of(&self, id: ResourceId) -> Option<crate::format::Format> {
        self.res(id).map(|r| r.desc.format)
    }

    fn present(&mut self, color: ResourceId, width: u32, height: u32, dst: &mut Present<'_>) {
        self.flush_if_uses(color);
        let Some(r) = self.res(color) else { return };
        let Some(lv) = r.level(0).copied() else { return };
        let (w, h) = (width.min(lv.width) as usize, height.min(lv.height) as usize);
        let (dw, dh, opaque) = (dst.width as usize, dst.height as usize, dst.opaque);
        if h == 0 || w == 0 || dw == 0 || dh == 0 || dst.pixels.len() < (dh - 1) * dst.stride + dw {
            return;
        }
        // The same size: converted straight into the window. Otherwise into
        // an image first, then scaled.
        let direct = (w, h) == (dw, dh);
        let mut image = Vec::new();
        let (out, stride) = if direct {
            (dst.pixels.as_mut_ptr(), dst.stride)
        } else {
            if image.try_reserve_exact(w * h).is_err() {
                return;
            }
            image.resize(w * h, 0u32);
            (image.as_mut_ptr(), w)
        };
        let format = r.desc.format;
        let samples = r.samples as usize;
        let tb = format.bytes();
        let data = &r.data;
        let out = SyncPtr(out);
        let next = AtomicUsize::new(0);
        let rows = 16usize;
        let job = |_: usize| {
            loop {
                let first = next.fetch_add(rows, Ordering::Relaxed);
                if first >= h {
                    break;
                }
                for y in first..(first + rows).min(h) {
                    // Row 0 of a GL image is the bottom one.
                    let src = &data[lv.offset + (lv.height as usize - 1 - y) * lv.row..][..w * lv.texel];
                    // SAFETY: each row of `out` is written by one worker only,
                    // and the length was checked above.
                    let row = unsafe { core::slice::from_raw_parts_mut(out.get().add(y * stride), w) };
                    match (format, samples, opaque) {
                        (crate::format::Format::Rgba8Unorm | crate::format::Format::Rgbx8Unorm, 1, true) => {
                            for (d, s) in row.iter_mut().zip(src.as_chunks::<4>().0) {
                                *d = 0xFF00_0000 | u32::from(s[0]) << 16 | u32::from(s[1]) << 8 | u32::from(s[2]);
                            }
                        }
                        _ => {
                            let mut one = [0u8; 16];
                            for (d, s) in row.iter_mut().zip(src.chunks_exact(lv.texel)) {
                                resource::resolve(format, s, &mut one[..tb]);
                                *d = to_argb(crate::pixels::decode_raw(format, &one[..tb]).as_float(), opaque);
                            }
                        }
                    }
                }
            }
        };
        self.workers.run(&job);
        if !direct {
            let next = AtomicUsize::new(0);
            let (pixels, dstride) = (SyncPtr(dst.pixels.as_mut_ptr()), dst.stride);
            let image = &image;
            let job = |_: usize| {
                loop {
                    let first = next.fetch_add(rows, Ordering::Relaxed);
                    if first >= dh {
                        break;
                    }
                    let last = (first + rows).min(dh);
                    let len = (last - first - 1) * dstride + dw;
                    // SAFETY: rows `first..last` of the window are written
                    // by this worker only; the length was checked above.
                    let part = unsafe { core::slice::from_raw_parts_mut(pixels.get().add(first * dstride), len) };
                    crate::present::scale_rows(image, w, h, dw, dh, part, dstride, first..last);
                }
            };
            self.workers.run(&job);
        }
    }

    fn fence(&mut self) -> u64 {
        self.render();
        self.fences += 1;
        self.fences
    }

    fn wait_fence(&mut self, _fence: u64, _timeout_ns: u64) -> bool {
        // Rendering is done when a fence is made.
        true
    }
}

impl Drop for SoftBackend {
    fn drop(&mut self) {
        // Recorded work refers to resources: drop it first.
        self.scene = None;
    }
}
