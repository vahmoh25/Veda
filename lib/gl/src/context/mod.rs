//! The OpenGL ES 3.0 context: the GL's state and objects, and every entry
//! point, checked against the specification.
//!
//! Entry points are methods named as the C functions in snake case
//! (`glBufferData` is [`Context::buffer_data`]); enumerants are the
//! constants of [`crate::gl`]. Errors follow the GL's model: a call that
//! fails changes nothing and records its error, which
//! [`Context::get_error`] returns (the first one since the last query).
//!
//! The areas are in submodules, following the specification's chapters:
//! buffers, vertex arrays, programs, textures and samplers, framebuffers
//! and renderbuffers, drawing, queries, transform feedback, sync objects
//! and state queries.

mod buffers;
mod draw;
mod feedback;
mod framebuffers;
mod objects;
mod programs;
mod queries;
mod state;
mod textures;
mod vertex;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

pub use objects::*;

use crate::backend::{Backend, BlendEq, BlendFactor, Caps, Cull, Func, StencilOp};
use crate::gl;

/// How a context is set up.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The default framebuffer's size.
    pub width: u32,
    pub height: u32,
    /// Its color format: `gl::RGBA8` (or `gl::RGB8`, `gl::RGB565`).
    pub color: u32,
    /// Its depth and stencil bits (0, 16, 24 / 0, 8).
    pub depth_bits: u8,
    pub stencil_bits: u8,
    /// Samples per pixel (0 or 4).
    pub samples: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config { width: 640, height: 480, color: gl::RGBA8, depth_bits: 24, stencil_bits: 8, samples: 0 }
    }
}

/// Texture units (`GL_MAX_COMBINED_TEXTURE_IMAGE_UNITS`).
pub const TEXTURE_UNITS: usize = 32;
/// Vertex attributes (`GL_MAX_VERTEX_ATTRIBS`).
pub const VERTEX_ATTRIBS: usize = 16;
/// Uniform buffer binding points.
pub const UNIFORM_BUFFER_BINDINGS: usize = 36;
/// Transform feedback buffer binding points.
pub const FEEDBACK_BINDINGS: usize = 4;
/// Color attachments and draw buffers.
pub const DRAW_BUFFERS: usize = 4;

/// Pixel storage modes (`glPixelStorei`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelStore {
    pub alignment: u32,
    pub row_length: u32,
    pub image_height: u32,
    pub skip_pixels: u32,
    pub skip_rows: u32,
    pub skip_images: u32,
}

impl Default for PixelStore {
    fn default() -> PixelStore {
        PixelStore { alignment: 4, row_length: 0, image_height: 0, skip_pixels: 0, skip_rows: 0, skip_images: 0 }
    }
}

/// One face's stencil state as the GL keeps it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StencilFace {
    pub func: Func,
    pub reference: i32,
    pub value_mask: u32,
    pub write_mask: u32,
    pub fail: StencilOp,
    pub depth_fail: StencilOp,
    pub pass: StencilOp,
}

impl Default for StencilFace {
    fn default() -> StencilFace {
        StencilFace {
            func: Func::Always,
            reference: 0,
            value_mask: !0,
            write_mask: !0,
            fail: StencilOp::Keep,
            depth_fail: StencilOp::Keep,
            pass: StencilOp::Keep,
        }
    }
}

/// Fixed-function state (OpenGL ES 3.0 tables 6.6 to 6.15).
#[derive(Clone, Debug)]
pub struct Fixed {
    pub viewport: [i32; 4],
    pub depth_range: [f32; 2],
    pub scissor_test: bool,
    pub scissor: [i32; 4],
    pub cull_face: bool,
    pub cull_mode: Cull,
    pub front_ccw: bool,
    pub line_width: f32,
    pub polygon_offset_fill: bool,
    pub polygon_offset: (f32, f32),
    pub rasterizer_discard: bool,
    pub sample_alpha_to_coverage: bool,
    pub sample_coverage: bool,
    pub sample_coverage_value: f32,
    pub sample_coverage_invert: bool,
    pub stencil_test: bool,
    pub stencil_front: StencilFace,
    pub stencil_back: StencilFace,
    pub depth_test: bool,
    pub depth_func: Func,
    pub depth_write: bool,
    pub blend: bool,
    pub blend_eq: (BlendEq, BlendEq),
    pub blend_src: (BlendFactor, BlendFactor),
    pub blend_dst: (BlendFactor, BlendFactor),
    pub blend_color: [f32; 4],
    pub dither: bool,
    pub color_mask: [bool; 4],
    pub clear_color: [f32; 4],
    pub clear_depth: f32,
    pub clear_stencil: i32,
    pub primitive_restart: bool,
    pub generate_mipmap_hint: u32,
    pub derivative_hint: u32,
    pub pack: PixelStore,
    pub unpack: PixelStore,
}

impl Fixed {
    fn new(width: u32, height: u32) -> Fixed {
        Fixed {
            viewport: [0, 0, width as i32, height as i32],
            depth_range: [0.0, 1.0],
            scissor_test: false,
            scissor: [0, 0, width as i32, height as i32],
            cull_face: false,
            cull_mode: Cull::Back,
            front_ccw: true,
            line_width: 1.0,
            polygon_offset_fill: false,
            polygon_offset: (0.0, 0.0),
            rasterizer_discard: false,
            sample_alpha_to_coverage: false,
            sample_coverage: false,
            sample_coverage_value: 1.0,
            sample_coverage_invert: false,
            stencil_test: false,
            stencil_front: StencilFace::default(),
            stencil_back: StencilFace::default(),
            depth_test: false,
            depth_func: Func::Less,
            depth_write: true,
            blend: false,
            blend_eq: (BlendEq::Add, BlendEq::Add),
            blend_src: (BlendFactor::One, BlendFactor::One),
            blend_dst: (BlendFactor::Zero, BlendFactor::Zero),
            blend_color: [0.0; 4],
            dither: true,
            color_mask: [true; 4],
            clear_color: [0.0; 4],
            clear_depth: 1.0,
            clear_stencil: 0,
            primitive_restart: false,
            generate_mipmap_hint: gl::DONT_CARE,
            derivative_hint: gl::DONT_CARE,
            pack: PixelStore::default(),
            unpack: PixelStore::default(),
        }
    }
}

/// A generic vertex attribute's current value and how it was set.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Current {
    Float([f32; 4]),
    Int([i32; 4]),
    Uint([u32; 4]),
}

impl Current {
    pub fn bits(&self) -> [u32; 4] {
        match *self {
            Current::Float(f) => f.map(f32::to_bits),
            Current::Int(i) => i.map(|x| x as u32),
            Current::Uint(u) => u,
        }
    }
}

/// The texture binding targets of a texture unit, in the order units keep
/// them.
pub const TEXTURE_TARGETS: [u32; 4] = [gl::TEXTURE_2D, gl::TEXTURE_CUBE_MAP, gl::TEXTURE_3D, gl::TEXTURE_2D_ARRAY];

/// The index of a texture target in [`TEXTURE_TARGETS`].
pub(crate) fn target_index(target: u32) -> Option<usize> {
    TEXTURE_TARGETS.iter().position(|&t| t == target)
}

/// The buffer binding points of the context (not of vertex arrays or
/// transform feedback objects).
#[derive(Clone, Debug, Default)]
pub(crate) struct BufferBindings {
    pub array: Option<Key>,
    pub copy_read: Option<Key>,
    pub copy_write: Option<Key>,
    pub pixel_pack: Option<Key>,
    pub pixel_unpack: Option<Key>,
    pub uniform: Option<Key>,
    /// `BindBufferRange(UNIFORM_BUFFER, i, ...)`.
    pub uniform_indexed: Vec<IndexedBinding>,
}

/// A uniform buffer binding point.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IndexedBinding {
    pub buffer: Option<Key>,
    pub offset: usize,
    /// 0: the whole buffer (`BindBufferBase`).
    pub size: usize,
}

/// An OpenGL ES 3.0 context.
pub struct Context {
    pub(crate) backend: Box<dyn Backend>,
    pub(crate) error: u32,
    pub(crate) config: Config,
    pub(crate) state: Fixed,
    pub(crate) shading: vglsl::Options,
    // Objects.
    pub(crate) buffers: Store<Buffer>,
    pub(crate) textures: Store<Texture>,
    pub(crate) renderbuffers: Store<Renderbuffer>,
    pub(crate) framebuffers: Store<FramebufferObject>,
    pub(crate) vertex_arrays: Store<VertexArray>,
    pub(crate) samplers: Store<Sampler>,
    pub(crate) queries: Store<Query>,
    pub(crate) feedbacks: Store<TransformFeedback>,
    pub(crate) programs: programs::Programs,
    pub(crate) syncs: BTreeMap<u32, Sync>,
    pub(crate) next_sync: u32,
    // Bindings.
    pub(crate) bound: BufferBindings,
    pub(crate) active_texture: usize,
    /// Per unit, the texture bound to each of [`TEXTURE_TARGETS`].
    pub(crate) texture_units: Vec<[Key; 4]>,
    /// The default textures (name 0) of each target.
    pub(crate) default_textures: [Key; 4],
    pub(crate) sampler_units: Vec<Option<Key>>,
    pub(crate) draw_framebuffer: Option<Key>,
    pub(crate) read_framebuffer: Option<Key>,
    pub(crate) renderbuffer: Option<Key>,
    /// The vertex array object (`None`: the default one).
    pub(crate) vertex_array: Option<Key>,
    pub(crate) default_vao: VertexArray,
    pub(crate) current_attribs: [Current; VERTEX_ATTRIBS],
    /// The transform feedback object (`None`: the default one).
    pub(crate) feedback: Option<Key>,
    pub(crate) default_feedback: TransformFeedback,
    /// The active query of each target: `ANY_SAMPLES_PASSED`,
    /// `ANY_SAMPLES_PASSED_CONSERVATIVE`,
    /// `TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN`.
    pub(crate) active_queries: [Option<Key>; 3],
    /// The default framebuffer.
    pub(crate) default_fb: DefaultFramebuffer,
    /// Samples per pixel of the default framebuffer (0 or 4).
    pub(crate) default_samples: u32,
    /// Primitives transform feedback has written (for queries).
    pub(crate) primitives_written: u64,
    /// Per-draw scratch space, kept to avoid allocating on every draw.
    pub(crate) scratch: draw::Scratch,
}

impl Context {
    /// A context drawing with `backend`, with a default framebuffer as
    /// `config` describes.
    pub fn new(backend: Box<dyn Backend>, config: Config) -> Context {
        let mut textures = Store::default();
        let default_textures = TEXTURE_TARGETS.map(|t| textures.create_unnamed(Texture::new(t)));
        let mut c = Context {
            backend,
            error: gl::NO_ERROR,
            config,
            state: Fixed::new(config.width, config.height),
            shading: vglsl::Options::default(),
            buffers: Store::default(),
            textures,
            renderbuffers: Store::default(),
            framebuffers: Store::default(),
            vertex_arrays: Store::default(),
            samplers: Store::default(),
            queries: Store::default(),
            feedbacks: Store::default(),
            programs: programs::Programs::default(),
            syncs: BTreeMap::new(),
            next_sync: 1,
            bound: BufferBindings {
                uniform_indexed: vec![IndexedBinding::default(); UNIFORM_BUFFER_BINDINGS],
                ..BufferBindings::default()
            },
            active_texture: 0,
            texture_units: vec![default_textures; TEXTURE_UNITS],
            default_textures,
            sampler_units: vec![None; TEXTURE_UNITS],
            draw_framebuffer: None,
            read_framebuffer: None,
            renderbuffer: None,
            vertex_array: None,
            default_vao: VertexArray::default(),
            current_attribs: [Current::Float([0.0, 0.0, 0.0, 1.0]); VERTEX_ATTRIBS],
            feedback: None,
            default_feedback: TransformFeedback::default(),
            active_queries: [None; 3],
            default_fb: DefaultFramebuffer::default(),
            default_samples: 0,
            primitives_written: 0,
            scratch: draw::Scratch::default(),
        };
        c.shading.limits.max_draw_buffers = DRAW_BUFFERS as u32;
        c.shading.limits.max_combined_texture_image_units = TEXTURE_UNITS as u32;
        c.shading.limits.max_vertex_attribs = VERTEX_ATTRIBS as u32;
        c.state.line_width = 1.0;
        let (w, h) = (config.width, config.height);
        c.create_default_framebuffer(w, h);
        c
    }

    /// The renderer.
    pub fn backend(&mut self) -> &mut dyn Backend {
        &mut *self.backend
    }

    /// The renderer's description.
    pub fn caps(&self) -> &Caps {
        self.backend.caps()
    }

    /// Records an error (only the first since the last `get_error`).
    pub(crate) fn err(&mut self, e: u32) {
        if self.error == gl::NO_ERROR {
            self.error = e;
        }
    }

    /// `glGetError`.
    pub fn get_error(&mut self) -> u32 {
        core::mem::replace(&mut self.error, gl::NO_ERROR)
    }

    /// The current vertex array object.
    pub(crate) fn vao(&self) -> &VertexArray {
        match self.vertex_array {
            Some(k) => self.vertex_arrays.get(k),
            None => &self.default_vao,
        }
    }

    pub(crate) fn vao_mut(&mut self) -> &mut VertexArray {
        match self.vertex_array {
            Some(k) => self.vertex_arrays.get_mut(k),
            None => &mut self.default_vao,
        }
    }

    /// The current transform feedback object.
    pub(crate) fn tf(&self) -> &TransformFeedback {
        match self.feedback {
            Some(k) => self.feedbacks.get(k),
            None => &self.default_feedback,
        }
    }

    pub(crate) fn tf_mut(&mut self) -> &mut TransformFeedback {
        match self.feedback {
            Some(k) => self.feedbacks.get_mut(k),
            None => &mut self.default_feedback,
        }
    }

    /// Transform feedback is active and not paused.
    pub(crate) fn feedback_running(&self) -> bool {
        let t = self.tf();
        t.active && !t.paused
    }

    // ---- Enable / disable ------------------------------------------------

    fn capability(&mut self, cap: u32) -> Option<&mut bool> {
        let s = &mut self.state;
        Some(match cap {
            gl::BLEND => &mut s.blend,
            gl::CULL_FACE => &mut s.cull_face,
            gl::DEPTH_TEST => &mut s.depth_test,
            gl::DITHER => &mut s.dither,
            gl::POLYGON_OFFSET_FILL => &mut s.polygon_offset_fill,
            gl::PRIMITIVE_RESTART_FIXED_INDEX => &mut s.primitive_restart,
            gl::RASTERIZER_DISCARD => &mut s.rasterizer_discard,
            gl::SAMPLE_ALPHA_TO_COVERAGE => &mut s.sample_alpha_to_coverage,
            gl::SAMPLE_COVERAGE => &mut s.sample_coverage,
            gl::SCISSOR_TEST => &mut s.scissor_test,
            gl::STENCIL_TEST => &mut s.stencil_test,
            _ => return None,
        })
    }

    /// `glEnable`.
    pub fn enable(&mut self, cap: u32) {
        match self.capability(cap) {
            Some(v) => *v = true,
            None => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glDisable`.
    pub fn disable(&mut self, cap: u32) {
        match self.capability(cap) {
            Some(v) => *v = false,
            None => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glIsEnabled`.
    pub fn is_enabled(&mut self, cap: u32) -> bool {
        match self.capability(cap) {
            Some(v) => *v,
            None => {
                self.err(gl::INVALID_ENUM);
                false
            }
        }
    }

    // ---- Fixed-function state -------------------------------------------

    /// `glViewport`.
    pub fn viewport(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if w < 0 || h < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let max = self.backend.caps().max_viewport as i32;
        self.state.viewport = [x, y, w.min(max), h.min(max)];
    }

    /// `glDepthRangef`.
    pub fn depth_rangef(&mut self, near: f32, far: f32) {
        self.state.depth_range = [clamp01(near), clamp01(far)];
    }

    /// `glScissor`.
    pub fn scissor(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if w < 0 || h < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        self.state.scissor = [x, y, w, h];
    }

    /// `glCullFace`.
    pub fn cull_face(&mut self, mode: u32) {
        self.state.cull_mode = match mode {
            gl::FRONT => Cull::Front,
            gl::BACK => Cull::Back,
            gl::FRONT_AND_BACK => Cull::Both,
            _ => return self.err(gl::INVALID_ENUM),
        };
    }

    /// `glFrontFace`.
    pub fn front_face(&mut self, mode: u32) {
        self.state.front_ccw = match mode {
            gl::CCW => true,
            gl::CW => false,
            _ => return self.err(gl::INVALID_ENUM),
        };
    }

    /// `glLineWidth`.
    pub fn line_width(&mut self, width: f32) {
        if width.is_nan() || width <= 0.0 {
            return self.err(gl::INVALID_VALUE);
        }
        self.state.line_width = width;
    }

    /// `glPolygonOffset`.
    pub fn polygon_offset(&mut self, factor: f32, units: f32) {
        self.state.polygon_offset = (factor, units);
    }

    /// `glSampleCoverage`.
    pub fn sample_coverage(&mut self, value: f32, invert: bool) {
        self.state.sample_coverage_value = clamp01(value);
        self.state.sample_coverage_invert = invert;
    }

    /// `glStencilFunc`.
    pub fn stencil_func(&mut self, func: u32, reference: i32, mask: u32) {
        self.stencil_func_separate(gl::FRONT_AND_BACK, func, reference, mask);
    }

    /// `glStencilFuncSeparate`.
    pub fn stencil_func_separate(&mut self, face: u32, func: u32, reference: i32, mask: u32) {
        let Some(f) = compare_func(func) else { return self.err(gl::INVALID_ENUM) };
        let Some((front, back)) = faces(face) else { return self.err(gl::INVALID_ENUM) };
        for (on, s) in [(front, &mut self.state.stencil_front), (back, &mut self.state.stencil_back)] {
            if on {
                s.func = f;
                s.reference = reference;
                s.value_mask = mask;
            }
        }
    }

    /// `glStencilMask`.
    pub fn stencil_mask(&mut self, mask: u32) {
        self.stencil_mask_separate(gl::FRONT_AND_BACK, mask);
    }

    /// `glStencilMaskSeparate`.
    pub fn stencil_mask_separate(&mut self, face: u32, mask: u32) {
        let Some((front, back)) = faces(face) else { return self.err(gl::INVALID_ENUM) };
        if front {
            self.state.stencil_front.write_mask = mask;
        }
        if back {
            self.state.stencil_back.write_mask = mask;
        }
    }

    /// `glStencilOp`.
    pub fn stencil_op(&mut self, fail: u32, depth_fail: u32, pass: u32) {
        self.stencil_op_separate(gl::FRONT_AND_BACK, fail, depth_fail, pass);
    }

    /// `glStencilOpSeparate`.
    pub fn stencil_op_separate(&mut self, face: u32, fail: u32, depth_fail: u32, pass: u32) {
        let (Some(a), Some(b), Some(c)) = (stencil_op(fail), stencil_op(depth_fail), stencil_op(pass)) else {
            return self.err(gl::INVALID_ENUM);
        };
        let Some((front, back)) = faces(face) else { return self.err(gl::INVALID_ENUM) };
        for (on, s) in [(front, &mut self.state.stencil_front), (back, &mut self.state.stencil_back)] {
            if on {
                s.fail = a;
                s.depth_fail = b;
                s.pass = c;
            }
        }
    }

    /// `glDepthFunc`.
    pub fn depth_func(&mut self, func: u32) {
        match compare_func(func) {
            Some(f) => self.state.depth_func = f,
            None => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glDepthMask`.
    pub fn depth_mask(&mut self, flag: bool) {
        self.state.depth_write = flag;
    }

    /// `glBlendEquation`.
    pub fn blend_equation(&mut self, mode: u32) {
        self.blend_equation_separate(mode, mode);
    }

    /// `glBlendEquationSeparate`.
    pub fn blend_equation_separate(&mut self, rgb: u32, alpha: u32) {
        match (blend_eq(rgb), blend_eq(alpha)) {
            (Some(a), Some(b)) => self.state.blend_eq = (a, b),
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glBlendFunc`.
    pub fn blend_func(&mut self, src: u32, dst: u32) {
        self.blend_func_separate(src, dst, src, dst);
    }

    /// `glBlendFuncSeparate`.
    pub fn blend_func_separate(&mut self, src_rgb: u32, dst_rgb: u32, src_alpha: u32, dst_alpha: u32) {
        match (blend_factor(src_rgb), blend_factor(dst_rgb), blend_factor(src_alpha), blend_factor(dst_alpha)) {
            (Some(a), Some(b), Some(c), Some(d)) => {
                self.state.blend_src = (a, c);
                self.state.blend_dst = (b, d);
            }
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glBlendColor`.
    pub fn blend_color(&mut self, r: f32, g: f32, b: f32, a: f32) {
        self.state.blend_color = [clamp01(r), clamp01(g), clamp01(b), clamp01(a)];
    }

    /// `glColorMask`.
    pub fn color_mask(&mut self, r: bool, g: bool, b: bool, a: bool) {
        self.state.color_mask = [r, g, b, a];
    }

    /// `glClearColor`.
    pub fn clear_color(&mut self, r: f32, g: f32, b: f32, a: f32) {
        self.state.clear_color = [r, g, b, a];
    }

    /// `glClearDepthf`.
    pub fn clear_depthf(&mut self, d: f32) {
        self.state.clear_depth = clamp01(d);
    }

    /// `glClearStencil`.
    pub fn clear_stencil(&mut self, s: i32) {
        self.state.clear_stencil = s;
    }

    /// `glHint`.
    pub fn hint(&mut self, target: u32, mode: u32) {
        if !matches!(mode, gl::FASTEST | gl::NICEST | gl::DONT_CARE) {
            return self.err(gl::INVALID_ENUM);
        }
        match target {
            gl::GENERATE_MIPMAP_HINT => self.state.generate_mipmap_hint = mode,
            gl::FRAGMENT_SHADER_DERIVATIVE_HINT => self.state.derivative_hint = mode,
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glPixelStorei`.
    pub fn pixel_storei(&mut self, pname: u32, param: i32) {
        let alignment = matches!(pname, gl::PACK_ALIGNMENT | gl::UNPACK_ALIGNMENT);
        if alignment && !matches!(param, 1 | 2 | 4 | 8) || param < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let v = param as u32;
        let s = &mut self.state;
        match pname {
            gl::PACK_ALIGNMENT => s.pack.alignment = v,
            gl::PACK_ROW_LENGTH => s.pack.row_length = v,
            gl::PACK_SKIP_PIXELS => s.pack.skip_pixels = v,
            gl::PACK_SKIP_ROWS => s.pack.skip_rows = v,
            gl::UNPACK_ALIGNMENT => s.unpack.alignment = v,
            gl::UNPACK_ROW_LENGTH => s.unpack.row_length = v,
            gl::UNPACK_IMAGE_HEIGHT => s.unpack.image_height = v,
            gl::UNPACK_SKIP_PIXELS => s.unpack.skip_pixels = v,
            gl::UNPACK_SKIP_ROWS => s.unpack.skip_rows = v,
            gl::UNPACK_SKIP_IMAGES => s.unpack.skip_images = v,
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glFlush`.
    pub fn flush(&mut self) {
        self.backend.flush();
    }

    /// `glFinish`.
    pub fn finish(&mut self) {
        self.backend.finish();
    }
}

pub(crate) fn clamp01(x: f32) -> f32 {
    if x > 0.0 { if x < 1.0 { x } else { 1.0 } } else { 0.0 }
}

/// `GL_NEVER`... as a comparison.
pub(crate) fn compare_func(f: u32) -> Option<Func> {
    Some(match f {
        gl::NEVER => Func::Never,
        gl::LESS => Func::Less,
        gl::EQUAL => Func::Equal,
        gl::LEQUAL => Func::LessEqual,
        gl::GREATER => Func::Greater,
        gl::NOTEQUAL => Func::NotEqual,
        gl::GEQUAL => Func::GreaterEqual,
        gl::ALWAYS => Func::Always,
        _ => return None,
    })
}

pub(crate) fn func_gl(f: Func) -> u32 {
    match f {
        Func::Never => gl::NEVER,
        Func::Less => gl::LESS,
        Func::Equal => gl::EQUAL,
        Func::LessEqual => gl::LEQUAL,
        Func::Greater => gl::GREATER,
        Func::NotEqual => gl::NOTEQUAL,
        Func::GreaterEqual => gl::GEQUAL,
        Func::Always => gl::ALWAYS,
    }
}

fn faces(face: u32) -> Option<(bool, bool)> {
    match face {
        gl::FRONT => Some((true, false)),
        gl::BACK => Some((false, true)),
        gl::FRONT_AND_BACK => Some((true, true)),
        _ => None,
    }
}

fn stencil_op(op: u32) -> Option<StencilOp> {
    Some(match op {
        gl::KEEP => StencilOp::Keep,
        gl::ZERO => StencilOp::Zero,
        gl::REPLACE => StencilOp::Replace,
        gl::INCR => StencilOp::Incr,
        gl::DECR => StencilOp::Decr,
        gl::INVERT => StencilOp::Invert,
        gl::INCR_WRAP => StencilOp::IncrWrap,
        gl::DECR_WRAP => StencilOp::DecrWrap,
        _ => return None,
    })
}

pub(crate) fn stencil_op_gl(op: StencilOp) -> u32 {
    match op {
        StencilOp::Keep => gl::KEEP,
        StencilOp::Zero => gl::ZERO,
        StencilOp::Replace => gl::REPLACE,
        StencilOp::Incr => gl::INCR,
        StencilOp::Decr => gl::DECR,
        StencilOp::Invert => gl::INVERT,
        StencilOp::IncrWrap => gl::INCR_WRAP,
        StencilOp::DecrWrap => gl::DECR_WRAP,
    }
}

fn blend_eq(e: u32) -> Option<BlendEq> {
    Some(match e {
        gl::FUNC_ADD => BlendEq::Add,
        gl::FUNC_SUBTRACT => BlendEq::Subtract,
        gl::FUNC_REVERSE_SUBTRACT => BlendEq::ReverseSubtract,
        gl::MIN => BlendEq::Min,
        gl::MAX => BlendEq::Max,
        _ => return None,
    })
}

pub(crate) fn blend_eq_gl(e: BlendEq) -> u32 {
    match e {
        BlendEq::Add => gl::FUNC_ADD,
        BlendEq::Subtract => gl::FUNC_SUBTRACT,
        BlendEq::ReverseSubtract => gl::FUNC_REVERSE_SUBTRACT,
        BlendEq::Min => gl::MIN,
        BlendEq::Max => gl::MAX,
    }
}

fn blend_factor(f: u32) -> Option<BlendFactor> {
    use BlendFactor::*;
    Some(match f {
        gl::ZERO => Zero,
        gl::ONE => One,
        gl::SRC_COLOR => SrcColor,
        gl::ONE_MINUS_SRC_COLOR => OneMinusSrcColor,
        gl::DST_COLOR => DstColor,
        gl::ONE_MINUS_DST_COLOR => OneMinusDstColor,
        gl::SRC_ALPHA => SrcAlpha,
        gl::ONE_MINUS_SRC_ALPHA => OneMinusSrcAlpha,
        gl::DST_ALPHA => DstAlpha,
        gl::ONE_MINUS_DST_ALPHA => OneMinusDstAlpha,
        gl::CONSTANT_COLOR => ConstantColor,
        gl::ONE_MINUS_CONSTANT_COLOR => OneMinusConstantColor,
        gl::CONSTANT_ALPHA => ConstantAlpha,
        gl::ONE_MINUS_CONSTANT_ALPHA => OneMinusConstantAlpha,
        gl::SRC_ALPHA_SATURATE => SrcAlphaSaturate,
        _ => return None,
    })
}

pub(crate) fn blend_factor_gl(f: BlendFactor) -> u32 {
    use BlendFactor::*;
    match f {
        Zero => gl::ZERO,
        One => gl::ONE,
        SrcColor => gl::SRC_COLOR,
        OneMinusSrcColor => gl::ONE_MINUS_SRC_COLOR,
        DstColor => gl::DST_COLOR,
        OneMinusDstColor => gl::ONE_MINUS_DST_COLOR,
        SrcAlpha => gl::SRC_ALPHA,
        OneMinusSrcAlpha => gl::ONE_MINUS_SRC_ALPHA,
        DstAlpha => gl::DST_ALPHA,
        OneMinusDstAlpha => gl::ONE_MINUS_DST_ALPHA,
        ConstantColor => gl::CONSTANT_COLOR,
        OneMinusConstantColor => gl::ONE_MINUS_CONSTANT_COLOR,
        ConstantAlpha => gl::CONSTANT_ALPHA,
        OneMinusConstantAlpha => gl::ONE_MINUS_CONSTANT_ALPHA,
        SrcAlphaSaturate => gl::SRC_ALPHA_SATURATE,
    }
}
