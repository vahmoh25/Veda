//! Programs, state objects and draws.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use vglsl::tgsi;
use vglsl::types::{Basic, Scalar};

use super::protocol::*;
use super::{VirglBackend, formats};
use crate::backend::*;

/// The vertex attribute data a shader input takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AttribKind {
    Float,
    Int,
    Uint,
}

/// A program: its two shaders and what drawing with it needs.
pub(super) struct Program {
    pub vs: u32,
    pub fs: u32,
    /// The attribute locations the vertex shader reads, by kind.
    attribs: Vec<(u32, AttribKind)>,
    /// Default-block slots.
    slots: u32,
    writes_point_size: bool,
    /// Per stage: the samplers and uniform blocks used.
    samplers: [Vec<u32>; 2],
    blocks: [Vec<u32>; 2],
    /// The program's sampler types (by sampler index).
    sampler_types: Vec<vglsl::types::Sampler>,
    /// The bytes each uniform block needs (by block index).
    block_sizes: Vec<u32>,
    /// Vertex shader variants capturing the second, third... separate
    /// transform feedback buffer (where the host captures one at a time).
    pub passes: Vec<u32>,
    /// The program as linked, for capturing in guest memory where the host
    /// cannot (see `feedback`).
    source: Arc<vglsl::program::Program>,
}

/// State objects made so far, and what is bound.
#[derive(Default)]
pub(super) struct States {
    blend: BTreeMap<[u32; 10], u32>,
    dsa: BTreeMap<[u32; 4], u32>,
    rasterizer: BTreeMap<[u32; 8], u32>,
    samplers: BTreeMap<[u32; 8], u32>,
    elements: BTreeMap<Vec<u32>, u32>,
    pub(super) bound: Bound,
    /// The current values of attributes without arrays, as uploaded.
    current: Vec<[u32; 4]>,
}

/// What the host has bound (empty or zero: unknown).
#[derive(Default)]
pub(super) struct Bound {
    blend: u32,
    dsa: u32,
    rasterizer: u32,
    elements: u32,
    vs: u32,
    fs: u32,
    framebuffer: Vec<u32>,
    viewport: Vec<u32>,
    scissor: Vec<u32>,
    stencil_ref: Option<u32>,
    blend_color: Vec<u32>,
    sample_mask: Option<u32>,
    vertex_buffers: Vec<u32>,
    index_buffer: Vec<u32>,
    views: [Vec<u32>; 2],
    sampler_states: [Vec<u32>; 2],
    constants: [Vec<u32>; 2],
    ubos: [Vec<u32>; 2],
}

impl Bound {
    /// Forgets what constants a stage has (they were set elsewhere).
    pub fn constants_invalidate(&mut self, stage: usize) {
        self.constants[stage].clear();
    }

    pub fn vertex_buffers_set(&mut self, w: Vec<u32>) {
        self.vertex_buffers = w;
    }
}

impl States {
    /// A resource is gone: bindings may name it (or a handle it had).
    pub fn forget_resource(&mut self, _id: u32) {
        self.bound = Bound::default();
    }

    pub fn forget_program(&mut self, _p: &Program) {
        self.bound = Bound::default();
    }
}

/// The stream output description of a vertex shader (the words after
/// `SO_NUM_OUTPUTS`, with it), for transform feedback: of every captured
/// varying, or (`only`) of one, into buffer 0.
fn stream_output(l: &vglsl::link::Linked, vs: &tgsi::Shader, only: Option<usize>) -> Vec<u32> {
    if l.feedback.is_empty() {
        return vec![0];
    }
    let mut strides = [0u32; 4];
    let mut outputs: Vec<(u32, u32, usize, u32)> = Vec::new();
    for (i, f) in l.feedback.iter().enumerate() {
        if only.is_some_and(|o| o != i) {
            continue;
        }
        let buf = if l.feedback_separate && only.is_none() { i.min(3) } else { 0 };
        let mut add = |reg: u32, comps: u32| {
            outputs.push((reg, comps, buf, strides[buf]));
            strides[buf] += comps;
        };
        if f.position {
            add(vs.position.unwrap_or(0), 4);
        } else if f.point_size {
            add(vs.point_size.unwrap_or(0), 1);
        } else {
            let (cols, rows) = match f.ty {
                Basic::Matrix(c, r) => (u32::from(c), u32::from(r)),
                b => (1, b.components()),
            };
            for e in 0..f.size {
                for c in 0..cols {
                    let slot = (f.slot + e * cols + c) as usize;
                    add(vs.varyings.get(slot).copied().unwrap_or(0), rows);
                }
            }
        }
    }
    let mut w = vec![outputs.len() as u32];
    w.extend_from_slice(&strides);
    for (reg, comps, buf, offset) in outputs {
        w.push(reg | (comps << 10) | ((buf as u32) << 13) | (offset << 16));
        w.push(0);
    }
    w
}

fn text_words(b: &[u8]) -> impl Iterator<Item = u32> + '_ {
    b.chunks(4).map(|c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        u32::from_le_bytes(w)
    })
}

fn prim(m: Mode) -> u32 {
    match m {
        Mode::Points => PRIM_POINTS,
        Mode::Lines => PRIM_LINES,
        Mode::LineLoop => PRIM_LINE_LOOP,
        Mode::LineStrip => PRIM_LINE_STRIP,
        Mode::Triangles => PRIM_TRIANGLES,
        Mode::TriangleStrip => PRIM_TRIANGLE_STRIP,
        Mode::TriangleFan => PRIM_TRIANGLE_FAN,
    }
}

fn func(f: Func) -> u32 {
    match f {
        Func::Never => 0,
        Func::Less => 1,
        Func::Equal => 2,
        Func::LessEqual => 3,
        Func::Greater => 4,
        Func::NotEqual => 5,
        Func::GreaterEqual => 6,
        Func::Always => 7,
    }
}

fn stencil_op(o: StencilOp) -> u32 {
    match o {
        StencilOp::Keep => 0,
        StencilOp::Zero => 1,
        StencilOp::Replace => 2,
        StencilOp::Incr => 3,
        StencilOp::Decr => 4,
        StencilOp::IncrWrap => 5,
        StencilOp::DecrWrap => 6,
        StencilOp::Invert => 7,
    }
}

fn blend_eq(e: BlendEq) -> u32 {
    match e {
        BlendEq::Add => BLEND_ADD,
        BlendEq::Subtract => BLEND_SUBTRACT,
        BlendEq::ReverseSubtract => BLEND_REVERSE_SUBTRACT,
        BlendEq::Min => BLEND_MIN,
        BlendEq::Max => BLEND_MAX,
    }
}

fn factor(f: BlendFactor) -> u32 {
    use BlendFactor::*;
    match f {
        Zero => FACTOR_ZERO,
        One => FACTOR_ONE,
        SrcColor => FACTOR_SRC_COLOR,
        OneMinusSrcColor => FACTOR_INV_SRC_COLOR,
        DstColor => FACTOR_DST_COLOR,
        OneMinusDstColor => FACTOR_INV_DST_COLOR,
        SrcAlpha => FACTOR_SRC_ALPHA,
        OneMinusSrcAlpha => FACTOR_INV_SRC_ALPHA,
        DstAlpha => FACTOR_DST_ALPHA,
        OneMinusDstAlpha => FACTOR_INV_DST_ALPHA,
        ConstantColor => FACTOR_CONST_COLOR,
        OneMinusConstantColor => FACTOR_INV_CONST_COLOR,
        ConstantAlpha => FACTOR_CONST_ALPHA,
        OneMinusConstantAlpha => FACTOR_INV_CONST_ALPHA,
        SrcAlphaSaturate => FACTOR_SRC_ALPHA_SATURATE,
    }
}

fn wrap(w: Wrap) -> u32 {
    match w {
        Wrap::Repeat => WRAP_REPEAT,
        Wrap::ClampToEdge => WRAP_CLAMP_TO_EDGE,
        Wrap::MirroredRepeat => WRAP_MIRROR_REPEAT,
    }
}

fn filter(f: Filter) -> u32 {
    match f {
        Filter::Nearest => FILTER_NEAREST,
        Filter::Linear => FILTER_LINEAR,
    }
}

/// The words of a blend state object (after its handle).
pub(super) fn blend_words(b: &Blend, alpha_to_coverage: bool) -> [u32; 10] {
    let mask = b.write_mask.iter().enumerate().fold(0, |m, (i, &on)| m | (u32::from(on) << i));
    let rt = if b.enabled {
        1 | (blend_eq(b.eq_rgb) << 1)
            | (factor(b.src_rgb) << 4)
            | (factor(b.dst_rgb) << 9)
            | (blend_eq(b.eq_alpha) << 14)
            | (factor(b.src_alpha) << 17)
            | (factor(b.dst_alpha) << 22)
            | (mask << 27)
    } else {
        // Disabled blending still keeps the factors at ONE/ZERO.
        (FACTOR_ONE << 4) | (FACTOR_ZERO << 9) | (FACTOR_ONE << 17) | (FACTOR_ZERO << 22) | (mask << 27)
    };
    let s0 = (u32::from(b.dither) << 2) | (u32::from(alpha_to_coverage) << 3);
    [s0, 0, rt, rt, rt, rt, rt, rt, rt, rt]
}

/// The words of a depth-stencil-alpha state object.
pub(super) fn dsa_words(d: &DepthStencil) -> [u32; 4] {
    let s0 = if d.depth_test { 1 | (u32::from(d.depth_write) << 1) | (func(d.depth_func) << 2) } else { 0 };
    let face = |s: &Stencil| {
        if !d.stencil_test {
            return 0;
        }
        1 | (func(s.func) << 1)
            | (stencil_op(s.fail) << 4)
            | (stencil_op(s.pass) << 7)
            | (stencil_op(s.depth_fail) << 10)
            | ((s.value_mask & 0xFF) << 13)
            | ((s.write_mask & 0xFF) << 21)
    };
    [s0, face(&d.front), face(&d.back), 0]
}

/// The words of a sampler state object.
fn sampler_words(s: &SamplerState) -> [u32; 8] {
    let mip = match s.mip {
        None => MIPFILTER_NONE,
        Some(Filter::Nearest) => MIPFILTER_NEAREST,
        Some(Filter::Linear) => MIPFILTER_LINEAR,
    };
    let aniso = if s.max_anisotropy > 1.0 { (s.max_anisotropy as u32).min(16) } else { 0 };
    let s0 = wrap(s.wrap[0])
        | (wrap(s.wrap[1]) << 3)
        | (wrap(s.wrap[2]) << 6)
        | (filter(s.min) << 9)
        | (mip << 11)
        | (filter(s.mag) << 13)
        | (u32::from(s.compare.is_some()) << 15)
        | (s.compare.map_or(0, func) << 16)
        | (1 << 19)
        | (aniso << 20);
    [s0, 0, s.min_lod.to_bits(), s.max_lod.to_bits(), 0, 0, 0, 0]
}

impl VirglBackend {
    // ---- Programs -------------------------------------------------------------

    pub(super) fn program(&mut self, p: Arc<vglsl::program::Program>) -> ProgramId {
        let vs = tgsi::translate(&p.vertex, &p.linked, &self.tgsi);
        let fs = tgsi::translate(&p.fragment, &p.linked, &self.tgsi);
        // virglrenderer captures several buffers only with
        // `gl_NextBuffer`, which OpenGL ES lacks: there, a separate buffer
        // per varying takes a vertex shader variant (and a pass) each.
        let l = &p.linked;
        let split = l.feedback_separate && l.feedback.len() > 1 && !self.host.has(CAP_TRANSFORM_FEEDBACK3);
        let vh = self.new_handle();
        let mut passes = Vec::new();
        if split {
            self.create_shader(vh, SHADER_VERTEX, &vs, &stream_output(l, &vs, Some(0)));
            for k in 1..l.feedback.len() {
                let h = self.new_handle();
                self.create_shader(h, SHADER_VERTEX, &vs, &stream_output(l, &vs, Some(k)));
                passes.push(h);
            }
        } else {
            self.create_shader(vh, SHADER_VERTEX, &vs, &stream_output(l, &vs, None));
        }
        let fh = self.new_handle();
        self.create_shader(fh, SHADER_FRAGMENT, &fs, &[0]);
        let mut attribs = Vec::new();
        for a in &p.linked.attributes {
            let (kind, n) = match a.ty.as_basic() {
                Some(Basic::Matrix(c, _)) => (AttribKind::Float, u32::from(c)),
                Some(b) => match b.scalar() {
                    Some(Scalar::Int) => (AttribKind::Int, 1),
                    Some(Scalar::Uint) => (AttribKind::Uint, 1),
                    _ => (AttribKind::Float, 1),
                },
                None => (AttribKind::Float, 1),
            };
            for k in 0..n {
                attribs.push((a.location + k, kind));
            }
        }
        attribs.sort_unstable_by_key(|a| a.0);
        attribs.dedup_by_key(|a| a.0);
        let id = self.next_program;
        self.next_program += 1;
        self.programs.insert(
            id,
            Program {
                vs: vh,
                fs: fh,
                attribs,
                slots: p.linked.slots,
                writes_point_size: p.linked.writes_point_size,
                samplers: [vs.samplers.clone(), fs.samplers.clone()],
                blocks: [vs.blocks.clone(), fs.blocks.clone()],
                sampler_types: p.linked.samplers.iter().map(|s| s.sampler).collect(),
                block_sizes: p.linked.blocks.iter().map(|b| b.size).collect(),
                passes,
                source: p,
            },
        );
        id
    }

    /// Creates a shader object, in several commands if its text is longer
    /// than one command holds.
    pub(super) fn create_shader(&mut self, handle: u32, stage: u32, s: &tgsi::Shader, so: &[u32]) {
        let mut bytes = Vec::with_capacity(s.text.len() + 1);
        bytes.extend_from_slice(s.text.as_bytes());
        bytes.push(0);
        let total = bytes.len();
        let mut at = 0;
        while at < total {
            let first = at == 0;
            let header = 4 + if first { so.len() } else { 1 };
            let room = (MAX_COMMAND_WORDS - header) * 4;
            let n = (total - at).min(room);
            let offlen = if first { total as u32 } else { at as u32 | (1 << 31) };
            let mut w = Vec::with_capacity(header + n.div_ceil(4));
            w.extend_from_slice(&[handle, stage, offlen, s.tokens]);
            if first {
                w.extend_from_slice(so);
            } else {
                w.push(0);
            }
            w.extend(text_words(&bytes[at..at + n]));
            self.emit(CMD_CREATE_OBJECT, OBJ_SHADER, &w);
            at += n;
        }
    }

    // ---- State objects --------------------------------------------------------

    pub(super) fn bind_blend(&mut self, w: [u32; 10]) {
        let h = match self.state.blend.get(&w) {
            Some(&h) => h,
            None => {
                let h = self.new_handle();
                let mut c = vec![h];
                c.extend_from_slice(&w);
                self.emit(CMD_CREATE_OBJECT, OBJ_BLEND, &c);
                self.state.blend.insert(w, h);
                h
            }
        };
        if self.state.bound.blend != h {
            self.emit(CMD_BIND_OBJECT, OBJ_BLEND, &[h]);
            self.state.bound.blend = h;
        }
    }

    pub(super) fn bind_dsa(&mut self, w: [u32; 4]) {
        let h = match self.state.dsa.get(&w) {
            Some(&h) => h,
            None => {
                let h = self.new_handle();
                self.emit(CMD_CREATE_OBJECT, OBJ_DSA, &[h, w[0], w[1], w[2], w[3]]);
                self.state.dsa.insert(w, h);
                h
            }
        };
        if self.state.bound.dsa != h {
            self.emit(CMD_BIND_OBJECT, OBJ_DSA, &[h]);
            self.state.bound.dsa = h;
        }
    }

    pub(super) fn bind_rasterizer(&mut self, w: [u32; 8]) {
        let h = match self.state.rasterizer.get(&w) {
            Some(&h) => h,
            None => {
                let h = self.new_handle();
                let mut c = vec![h];
                c.extend_from_slice(&w);
                self.emit(CMD_CREATE_OBJECT, OBJ_RASTERIZER, &c);
                self.state.rasterizer.insert(w, h);
                h
            }
        };
        if self.state.bound.rasterizer != h {
            self.emit(CMD_BIND_OBJECT, OBJ_RASTERIZER, &[h]);
            self.state.bound.rasterizer = h;
        }
    }

    pub(super) fn sampler_state(&mut self, s: &SamplerState) -> u32 {
        let w = sampler_words(s);
        if let Some(&h) = self.state.samplers.get(&w) {
            return h;
        }
        let h = self.new_handle();
        let mut c = vec![h];
        c.extend_from_slice(&w);
        self.emit(CMD_CREATE_OBJECT, OBJ_SAMPLER_STATE, &c);
        self.state.samplers.insert(w, h);
        h
    }

    pub(super) fn bind_elements(&mut self, w: Vec<u32>) {
        let h = match self.state.elements.get(&w) {
            Some(&h) => h,
            None => {
                let h = self.new_handle();
                let mut c = vec![h];
                c.extend_from_slice(&w);
                self.emit(CMD_CREATE_OBJECT, OBJ_VERTEX_ELEMENTS, &c);
                self.state.elements.insert(w, h);
                h
            }
        };
        if self.state.bound.elements != h {
            self.emit(CMD_BIND_OBJECT, OBJ_VERTEX_ELEMENTS, &[h]);
            self.state.bound.elements = h;
        }
    }

    pub(super) fn bind_shaders(&mut self, vs: u32, fs: u32) {
        if self.state.bound.vs != vs {
            self.emit(CMD_BIND_SHADER, 0, &[vs, SHADER_VERTEX]);
            self.state.bound.vs = vs;
        }
        if self.state.bound.fs != fs {
            self.emit(CMD_BIND_SHADER, 0, &[fs, SHADER_FRAGMENT]);
            self.state.bound.fs = fs;
        }
    }

    // ---- Framebuffer and fixed state ------------------------------------------

    /// Binds color surfaces (by fragment output) and a depth-stencil one.
    pub(super) fn bind_surfaces(&mut self, colors: &[Option<Surface>], zs: Option<Surface>) {
        let mut w = vec![colors.len() as u32, zs.map_or(0, |s| self.surface(s))];
        for c in colors {
            let h = c.map_or(0, |s| self.surface(s));
            w.push(h);
        }
        if self.state.bound.framebuffer != w {
            self.emit(CMD_SET_FRAMEBUFFER_STATE, 0, &w);
            self.state.bound.framebuffer = w;
        }
    }

    pub(super) fn bind_framebuffer(&mut self, fb: &Framebuffer) {
        let n = fb.draw_buffers.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
        let colors: Vec<Option<Surface>> =
            (0..n).map(|l| fb.draw_buffers[l].and_then(|a| fb.colors[a as usize])).collect();
        self.bind_surfaces(&colors, fb.depth.or(fb.stencil));
    }

    pub(super) fn set_viewport(&mut self, v: &Viewport) {
        let (n, f) = (v.near.clamp(0.0, 1.0), v.far.clamp(0.0, 1.0));
        let w = [
            0,
            (v.w * 0.5).to_bits(),
            (v.h * 0.5).to_bits(),
            ((f - n) * 0.5).to_bits(),
            (v.x + v.w * 0.5).to_bits(),
            (v.y + v.h * 0.5).to_bits(),
            ((f + n) * 0.5).to_bits(),
        ];
        if self.state.bound.viewport != w {
            self.emit(CMD_SET_VIEWPORT_STATE, 0, &w);
            self.state.bound.viewport = w.to_vec();
        }
    }

    pub(super) fn set_scissor(&mut self, r: Rect) {
        let c = |v: i32| v.clamp(0, 0xFFFF) as u32;
        let w = [0, c(r.x) | (c(r.y) << 16), c(r.x.saturating_add(r.w)) | (c(r.y.saturating_add(r.h)) << 16)];
        if self.state.bound.scissor != w {
            self.emit(CMD_SET_SCISSOR_STATE, 0, &w);
            self.state.bound.scissor = w.to_vec();
        }
    }

    pub(super) fn set_stencil_ref(&mut self, front: i32, back: i32) {
        let v = (front.clamp(0, 255) as u32) | ((back.clamp(0, 255) as u32) << 8);
        if self.state.bound.stencil_ref != Some(v) {
            self.emit(CMD_SET_STENCIL_REF, 0, &[v]);
            self.state.bound.stencil_ref = Some(v);
        }
    }

    pub(super) fn set_blend_color(&mut self, c: [f32; 4]) {
        let w = c.map(f32::to_bits);
        if self.state.bound.blend_color != w {
            self.emit(CMD_SET_BLEND_COLOR, 0, &w);
            self.state.bound.blend_color = w.to_vec();
        }
    }

    pub(super) fn set_sample_mask(&mut self, m: u32) {
        if self.state.bound.sample_mask != Some(m) {
            self.emit(CMD_SET_SAMPLE_MASK, 0, &[m]);
            self.state.bound.sample_mask = Some(m);
        }
    }

    /// Rasterizer words for a draw.
    pub(super) fn rasterizer_words(&self, r: &Raster, scissor: bool, point_size: bool, multisample: bool) -> [u32; 8] {
        let cull = match r.cull {
            None => FACE_NONE,
            Some(Cull::Front) => FACE_FRONT,
            Some(Cull::Back) => FACE_BACK,
            Some(Cull::Both) => FACE_FRONT_AND_BACK,
        };
        // Gallium counts windings, and places point sprite coordinates,
        // with y down (row 0 at the top), the opposite of OpenGL's window
        // coordinates, in which these images are drawn: counter-clockwise is
        // clockwise to it, and OpenGL ES's upper left origin of
        // `gl_PointCoord` is its lower left (which hosts on OpenGL ES ignore,
        // placing the origin themselves).
        let s0 = (1 << 1) // depth clip
            | (u32::from(r.discard) << 3)
            | (1 << 6) // point sprite origin: lower left
            | (1 << 7) // point sprites
            | (cull << 8)
            | (u32::from(scissor) << 14)
            | (u32::from(!r.front_ccw) << 15)
            | (u32::from(r.polygon_offset.is_some()) << 20)
            | (u32::from(point_size) << 24)
            | (u32::from(multisample) << 25)
            | (1 << 29); // pixel centers at half-integers
        let (scale, units) = r.polygon_offset.unwrap_or((0.0, 0.0));
        [s0, 1.0f32.to_bits(), 0, 0, r.line_width.to_bits(), units.to_bits(), scale.to_bits(), 0]
    }

    // ---- Draws ----------------------------------------------------------------

    pub(super) fn draw_call(&mut self, s: &DrawState<'_>, info: &DrawInfo) {
        if info.count == 0 || info.instances == 0 || !self.programs.contains_key(&s.program) {
            return;
        }
        // Whether the host records transform feedback, found out before
        // this draw's state is bound (finding out draws too).
        if s.feedback.is_some() {
            self.feedback_works();
        }
        // Into 2D copies of the 3D slices the host cannot bind, if any.
        let flat = self.unqueried(|b| b.flatten(s.framebuffer));
        let state = match &flat {
            Some((fb, _)) => DrawState { framebuffer: fb, ..*s },
            None => *s,
        };
        self.run_query();
        // The program, out of the table while the draw uses it.
        if let Some(p) = self.programs.remove(&s.program) {
            self.draw_with(&p, &state, info);
            self.programs.insert(s.program, p);
        }
        if let Some((_, proxies)) = flat {
            self.unqueried(|b| b.unflatten(proxies));
        }
    }

    fn draw_with(&mut self, p: &Program, s: &DrawState<'_>, info: &DrawInfo) {
        let (vs, fs, slots, point_size) = (p.vs, p.fs, p.slots, p.writes_point_size);
        let (attribs, stage_samplers, stage_blocks) = (&p.attribs, &p.samplers, &p.blocks);
        let (sampler_types, passes) = (&p.sampler_types, &p.passes);
        if !self.in_bounds(s, info, attribs, stage_blocks, &p.block_sizes) {
            // What reading past a buffer draws is undefined; the host
            // would refuse it and lose the context, so draw nothing.
            return;
        }
        let fb = s.framebuffer;
        let multisample = fb.samples > 0;

        self.bind_framebuffer(fb);
        self.set_viewport(&s.viewport);
        if let Some(r) = s.scissor {
            self.set_scissor(r);
        }
        let rs = self.rasterizer_words(&s.raster, s.scissor.is_some(), point_size, multisample);
        self.bind_rasterizer(rs);
        // A test without its buffer passes, and writes nothing (OpenGL ES
        // 3.0 sections 4.1.4 and 4.1.5); the host may still have that
        // buffer, part of a depth-stencil image bound for the other test.
        let mut ds = s.depth_stencil;
        ds.depth_test &= fb.depth.is_some();
        ds.stencil_test &= fb.stencil.is_some();
        self.bind_dsa(dsa_words(&ds));
        self.bind_blend(blend_words(&s.blend, s.raster.alpha_to_coverage && multisample));
        if ds.stencil_test {
            self.set_stencil_ref(ds.front.reference, ds.back.reference);
        }
        if s.blend.enabled {
            self.set_blend_color(s.blend.color);
        }
        let mask = match s.raster.sample_coverage {
            Some((value, invert)) if multisample => {
                let bits = (value.clamp(0.0, 1.0) * fb.samples as f32 + 0.5) as u32;
                let m = if bits >= 32 { !0 } else { (1u32 << bits) - 1 };
                if invert { !m } else { m }
            }
            _ => !0,
        };
        self.set_sample_mask(mask);
        self.bind_shaders(vs, fs);

        // Uniforms, blocks and textures, per stage.
        for (stage, shader) in [(0usize, SHADER_VERTEX), (1, SHADER_FRAGMENT)] {
            {
                // The program's uniforms, then the always-zero slot the
                // shaders build constants from (see `vglsl::tgsi`).
                let n = slots as usize;
                let bound = &self.state.bound.constants[stage];
                let unchanged = bound.len() == 6 + n * 4
                    && bound[0] == shader
                    && bound[2..2 + n * 4].as_chunks::<4>().0.iter().zip(s.uniforms).all(|(a, b)| a == b)
                    && s.uniforms.len() >= n;
                if !unchanged {
                    let mut w = Vec::with_capacity(6 + n * 4);
                    w.extend_from_slice(&[shader, 0]);
                    for v in s.uniforms.iter().take(n) {
                        w.extend_from_slice(v);
                    }
                    w.resize(2 + n * 4, 0);
                    w.extend_from_slice(&[0; 4]);
                    self.emit(CMD_SET_CONSTANT_BUFFER, 0, &w);
                    self.state.bound.constants[stage] = w;
                }
            }
            let mut ubos = Vec::new();
            for &b in &stage_blocks[stage] {
                let r = s.blocks.get(b as usize).copied().flatten();
                let w = match r {
                    Some(r) => [shader, b + 1, r.offset as u32, r.size as u32, r.buffer],
                    None => [shader, b + 1, 0, 0, 0],
                };
                ubos.extend_from_slice(&w);
            }
            if !ubos.is_empty() && self.state.bound.ubos[stage] != ubos {
                for w in ubos.as_chunks::<5>().0 {
                    self.emit(CMD_SET_UNIFORM_BUFFER, 0, w);
                }
                self.state.bound.ubos[stage] = ubos;
            }
            if let Some(&last) = stage_samplers[stage].last() {
                let n = last as usize + 1;
                let mut views = vec![shader, 0];
                let mut states = vec![shader, 0];
                for i in 0..n as u32 {
                    if !stage_samplers[stage].contains(&i) {
                        views.push(0);
                        states.push(0);
                        continue;
                    }
                    let binding = s.textures.get(i as usize);
                    let ty = sampler_types.get(i as usize).copied();
                    let (v, st) = match (binding, ty) {
                        (Some(TextureBinding { view: Some(view), sampler }), _) => {
                            (self.view(view), self.sampler_state(sampler))
                        }
                        (_, Some(ty)) => self.missing_texture(ty),
                        _ => (0, 0),
                    };
                    views.push(v);
                    states.push(st);
                }
                if self.state.bound.views[stage] != views {
                    self.emit(CMD_SET_SAMPLER_VIEWS, 0, &views);
                    self.state.bound.views[stage] = views;
                }
                if self.state.bound.sampler_states[stage] != states {
                    self.emit(CMD_BIND_SAMPLER_STATES, 0, &states);
                    self.state.bound.sampler_states[stage] = states;
                }
            }
        }

        // Vertex inputs: one element and buffer per attribute location.
        self.vertex_inputs(attribs, s.attribs);
        let mut first = info.first;
        if let Some((buffer, ty)) = s.index {
            // The indices' offset goes with the buffer; virglrenderer's
            // draws take a start index only for arrays.
            let w = vec![buffer, ty.bytes() as u32, first as u32];
            first = 0;
            if self.state.bound.index_buffer != w {
                self.emit(CMD_SET_INDEX_BUFFER, 0, &w);
                self.state.bound.index_buffer = w;
            }
        }

        let (start, restart) = match s.index {
            Some((_, ty)) => ((first / ty.bytes()) as u32, ty.restart()),
            None => (first as u32, 0),
        };
        let w = [
            start,
            info.count,
            prim(info.mode),
            u32::from(info.indexed),
            info.instances,
            0,
            0,
            u32::from(info.indexed && s.primitive_restart),
            restart,
            0,
            !0,
            0,
        ];
        let Some(ranges) = s.feedback else {
            self.emit(CMD_DRAW_VBO, 0, &w);
            return;
        };
        if self.feedback_on_host == Some(false) {
            // The host draws what is seen; the captured vertices are
            // shaded in guest memory.
            if !s.raster.discard {
                self.emit(CMD_DRAW_VBO, 0, &w);
            }
            return self.feedback_in_guest(&p.source, &stage_samplers[0], s, info, ranges);
        }
        for r in ranges {
            if let Some(sh) = self.resources.get_mut(&r.buffer).and_then(|b| b.shadow.as_mut()) {
                sh.valid = false;
                sh.ranges.clear();
            }
        }
        if passes.is_empty() {
            self.capture(ranges, &w);
            return;
        }
        // A buffer per varying, on a host that captures into one buffer at
        // a time: a pass per buffer, the later ones not rasterized.
        self.capture(&ranges[..1], &w);
        let quiet = Raster { discard: true, ..s.raster };
        let rs = self.rasterizer_words(&quiet, s.scissor.is_some(), point_size, multisample);
        for (k, &variant) in passes.iter().enumerate() {
            let Some(r) = ranges.get(k + 1) else { break };
            self.bind_shaders(variant, fs);
            self.bind_rasterizer(rs);
            self.capture(core::slice::from_ref(r), &w);
        }
    }

    /// Draws (`DRAW_VBO` words `w`) capturing into buffer ranges.
    fn capture(&mut self, ranges: &[BufferRange], w: &[u32]) {
        let mut targets = vec![0];
        for r in ranges {
            let h = self.new_handle();
            self.emit(CMD_CREATE_OBJECT, OBJ_STREAMOUT_TARGET, &[h, r.buffer, r.offset as u32, r.size as u32]);
            targets.push(h);
        }
        self.emit(CMD_SET_STREAMOUT_TARGETS, 0, &targets);
        self.emit(CMD_DRAW_VBO, 0, w);
        self.emit(CMD_SET_STREAMOUT_TARGETS, 0, &[0]);
        for &h in &targets[1..] {
            self.destroy_object(h);
        }
    }

    /// Vertex elements and buffers for the attribute locations a program
    /// reads. Attributes without an array read their current value from a
    /// small buffer, as an instanced attribute that never advances.
    fn vertex_inputs(&mut self, used: &[(u32, AttribKind)], attribs: &[Attrib]) {
        let Some(&(last, _)) = used.last() else {
            self.bind_elements(Vec::new());
            return;
        };
        let n = last as usize + 1;
        let current = self.current_buffer();
        // Upload changed current values.
        let mut values = vec![[0u32; 4]; n];
        for &(loc, _) in used {
            if let Some(a) = attribs.get(loc as usize)
                && a.buffer.is_none()
            {
                values[loc as usize] = a.current;
            }
        }
        if self.state.current.len() < n || self.state.current[..n] != values[..] {
            let bytes: Vec<u8> = values.iter().flat_map(|v| v.iter().flat_map(|w| w.to_le_bytes())).collect();
            self.write_buffer(current, 0, &bytes);
            self.state.current = values;
        }
        let mut elements = Vec::with_capacity(n * 4);
        let mut buffers = Vec::with_capacity(n * 3);
        for loc in 0..n {
            let kind = used.iter().find(|u| u.0 as usize == loc).map(|u| u.1);
            let a = attribs.get(loc).filter(|_| kind.is_some());
            match a {
                Some(a) if a.buffer.is_some() => {
                    let fmt = formats::vertex_format(a);
                    elements.extend_from_slice(&[0, a.divisor, loc as u32, fmt]);
                    buffers.extend_from_slice(&[a.stride, a.offset as u32, a.buffer.unwrap_or(0)]);
                }
                _ => {
                    let fmt = match kind {
                        Some(AttribKind::Int) => format::R32G32B32A32_SINT,
                        Some(AttribKind::Uint) => format::R32G32B32A32_UINT,
                        _ => format::R32G32B32A32_FLOAT,
                    };
                    elements.extend_from_slice(&[0, !0, loc as u32, fmt]);
                    buffers.extend_from_slice(&[16, (loc * 16) as u32, current]);
                }
            }
        }
        self.bind_elements(elements);
        if self.state.bound.vertex_buffers != buffers {
            self.emit(CMD_SET_VERTEX_BUFFERS, 0, &buffers);
            self.state.bound.vertex_buffers = buffers;
        }
    }
}

impl VirglBackend {
    /// Whether a draw reads only inside its buffers: every vertex and
    /// instance it fetches, its indices, and its uniform blocks.
    fn in_bounds(
        &mut self,
        s: &DrawState<'_>,
        info: &DrawInfo,
        used: &[(u32, AttribKind)],
        blocks: &[Vec<u32>; 2],
        block_sizes: &[u32],
    ) -> bool {
        let size = |b: &Self, id: u32| b.resources.get(&id).map_or(0, |r| u64::from(r.desc.width));
        // The highest vertex index.
        let last_vertex = match s.index {
            Some((ib, ty)) => {
                let end = info.first as u64 + u64::from(info.count) * ty.bytes() as u64;
                if end > size(self, ib) {
                    return false;
                }
                self.max_index(ib, info.first, info.count, ty, s.primitive_restart).map(u64::from)
            }
            None => Some(info.first as u64 + u64::from(info.count) - 1),
        };
        let last_instance = u64::from(info.instances.max(1) - 1);
        for &(loc, _) in used {
            let Some(a) = s.attribs.get(loc as usize) else { continue };
            let Some(b) = a.buffer else { continue };
            let element = if a.ty.packed() { 4 } else { u64::from(a.size) * u64::from(a.ty.bytes()) };
            let last = if a.divisor == 0 {
                match last_vertex {
                    Some(v) => v,
                    None => continue,
                }
            } else {
                last_instance / u64::from(a.divisor)
            };
            if a.offset as u64 + last * u64::from(a.stride) + element > size(self, b) {
                return false;
            }
        }
        for stage in blocks {
            for &b in stage {
                let need = block_sizes.get(b as usize).copied().unwrap_or(0) as usize;
                match s.blocks.get(b as usize).copied().flatten() {
                    Some(r) if r.size >= need => {}
                    _ => return false,
                }
            }
        }
        true
    }

    /// The highest index a draw reads (`None` if all are restarts), from
    /// the buffer's contents (read back if the GPU wrote them).
    fn max_index(&mut self, ib: u32, offset: usize, count: u32, ty: IndexType, restart: bool) -> Option<u32> {
        let bytes = ty.bytes();
        let key = (offset, count, bytes as u8, restart);
        let valid = self.resources.get(&ib).and_then(|r| r.shadow.as_ref()).map(|s| s.valid)?;
        if !valid {
            let n = self.resources.get(&ib).map_or(0, |r| r.desc.width as usize);
            let mut data = alloc::vec![0u8; n];
            self.read_buffer(ib, 0, &mut data);
            let s = self.resources.get_mut(&ib)?.shadow.as_mut()?;
            s.data = data;
            s.valid = true;
            s.ranges.clear();
        }
        let s = self.resources.get_mut(&ib)?.shadow.as_mut()?;
        if let Some(&m) = s.ranges.get(&key) {
            return m;
        }
        let r = ty.restart();
        let data = s.data.get(offset..offset + count as usize * bytes)?;
        let max = data
            .chunks_exact(bytes)
            .map(|c| match bytes {
                1 => u32::from(c[0]),
                2 => u32::from(u16::from_le_bytes([c[0], c[1]])),
                _ => u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
            })
            .filter(|&i| !(restart && i == r))
            .max();
        s.ranges.insert(key, max);
        max
    }
}
