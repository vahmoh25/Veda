//! The interface between the OpenGL ES front end and a renderer.
//!
//! The front end ([`crate::Context`]) checks every call against the
//! specification, keeps the GL's objects and state, and converts pixel
//! data; a [`Backend`] only renders. Its model is Gallium's (which Mesa's
//! drivers and virglrenderer share): resources with a storage [`Format`],
//! surfaces (a level and layer of a resource), and draws described by
//! complete, resolved state ([`DrawState`]): which buffers feed which
//! attributes, which textures and sampler states feed which samplers,
//! the fixed-function state, the framebuffer.
//!
//! Two back ends implement it: the software renderer ([`crate::soft`]) and
//! the virgl renderer ([`crate::virgl`]), which runs on the GPU.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::format::Format;

/// A resource: a buffer, texture or renderbuffer.
pub type ResourceId = u32;
/// A compiled program.
pub type ProgramId = u32;
/// A query object.
pub type QueryId = u32;

/// The renderer could not allocate memory (`GL_OUT_OF_MEMORY`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutOfMemory;

/// What a resource is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Buffer,
    Texture2D,
    Texture3D,
    /// Six faces as layers (+X, -X, +Y, -Y, +Z, -Z).
    TextureCube,
    Texture2DArray,
    /// A render target that cannot be sampled (it may be multisampled).
    Renderbuffer,
}

/// A resource's description.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResourceDesc {
    pub target: Target,
    pub format: Format,
    /// Texels (bytes for buffers).
    pub width: u32,
    pub height: u32,
    /// 3D depth, array layers, 6 for cube maps, 1 otherwise.
    pub depth: u32,
    pub levels: u32,
    /// Samples per pixel (0 for single-sampled).
    pub samples: u32,
}

/// One image of a resource.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Surface {
    pub resource: ResourceId,
    pub level: u32,
    /// Cube face, array layer or 3D slice.
    pub layer: u32,
}

/// A box of texels (or a byte range of a buffer: `x` and `w`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

impl Region {
    pub fn new(x: u32, y: u32, z: u32, w: u32, h: u32, d: u32) -> Region {
        Region { x, y, z, w, h, d }
    }

    pub fn bytes(offset: usize, size: usize) -> Region {
        Region { x: offset as u32, y: 0, z: 0, w: size as u32, h: 1, d: 1 }
    }
}

/// A screen rectangle (pixels; y from the bottom, as GL counts).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A comparison (depth, stencil, shadow lookups).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Func {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

impl Func {
    /// `a func b`.
    #[inline(always)]
    pub fn test<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            Func::Never => false,
            Func::Less => a < b,
            Func::Equal => a == b,
            Func::LessEqual => a <= b,
            Func::Greater => a > b,
            Func::NotEqual => a != b,
            Func::GreaterEqual => a >= b,
            Func::Always => true,
        }
    }
}

/// A stencil operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StencilOp {
    Keep,
    Zero,
    Replace,
    Incr,
    Decr,
    Invert,
    IncrWrap,
    DecrWrap,
}

/// One face's stencil state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stencil {
    pub func: Func,
    pub reference: i32,
    pub value_mask: u32,
    pub write_mask: u32,
    pub fail: StencilOp,
    pub depth_fail: StencilOp,
    pub pass: StencilOp,
}

/// Depth and stencil testing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DepthStencil {
    pub depth_test: bool,
    pub depth_func: Func,
    pub depth_write: bool,
    pub stencil_test: bool,
    pub front: Stencil,
    pub back: Stencil,
}

/// A blend equation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlendEq {
    Add,
    Subtract,
    ReverseSubtract,
    Min,
    Max,
}

/// A blend factor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlendFactor {
    Zero,
    One,
    SrcColor,
    OneMinusSrcColor,
    DstColor,
    OneMinusDstColor,
    SrcAlpha,
    OneMinusSrcAlpha,
    DstAlpha,
    OneMinusDstAlpha,
    ConstantColor,
    OneMinusConstantColor,
    ConstantAlpha,
    OneMinusConstantAlpha,
    SrcAlphaSaturate,
}

/// Blending and color writes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Blend {
    pub enabled: bool,
    pub eq_rgb: BlendEq,
    pub eq_alpha: BlendEq,
    pub src_rgb: BlendFactor,
    pub dst_rgb: BlendFactor,
    pub src_alpha: BlendFactor,
    pub dst_alpha: BlendFactor,
    pub color: [f32; 4],
    /// Red, green, blue, alpha writes (all draw buffers).
    pub write_mask: [bool; 4],
    pub dither: bool,
}

/// Which faces are culled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cull {
    Front,
    Back,
    Both,
}

/// Rasterization state.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Raster {
    pub cull: Option<Cull>,
    /// Counter-clockwise triangles face the front.
    pub front_ccw: bool,
    /// Polygon offset (factor, units) for filled triangles.
    pub polygon_offset: Option<(f32, f32)>,
    /// `GL_RASTERIZER_DISCARD`.
    pub discard: bool,
    pub line_width: f32,
    /// `GL_SAMPLE_ALPHA_TO_COVERAGE`.
    pub alpha_to_coverage: bool,
    /// `GL_SAMPLE_COVERAGE` (value, invert), if enabled.
    pub sample_coverage: Option<(f32, bool)>,
}

/// The viewport transform.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub near: f32,
    pub far: f32,
}

/// What is drawn into.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Framebuffer {
    /// Color attachments by attachment number.
    pub colors: [Option<Surface>; 8],
    pub depth: Option<Surface>,
    pub stencil: Option<Surface>,
    pub width: u32,
    pub height: u32,
    pub samples: u32,
    /// Fragment output location → color attachment (`glDrawBuffers`).
    pub draw_buffers: [Option<u8>; 8],
}

/// A texture's filter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    Nearest,
    Linear,
}

/// How texture coordinates outside [0, 1] wrap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrap {
    Repeat,
    ClampToEdge,
    MirroredRepeat,
}

/// Sampler state.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SamplerState {
    pub min: Filter,
    pub mag: Filter,
    /// Filtering between mipmap levels, or none (no mipmapping).
    pub mip: Option<Filter>,
    pub wrap: [Wrap; 3],
    pub min_lod: f32,
    pub max_lod: f32,
    /// Depth comparison (shadow samplers).
    pub compare: Option<Func>,
    pub max_anisotropy: f32,
}

/// A texture as a sampler sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct View {
    pub resource: ResourceId,
    pub target: Target,
    /// The levels the sampler may use.
    pub base_level: u32,
    pub max_level: u32,
    /// For each of red, green, blue and alpha: 0-3 a component, 4 zero,
    /// 5 one (`GL_TEXTURE_SWIZZLE_*`).
    pub swizzle: [u8; 4],
}

/// A sampler's binding: a complete texture and its sampler state, or
/// nothing (an incomplete texture samples as (0, 0, 0, 1)).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TextureBinding {
    pub view: Option<View>,
    pub sampler: SamplerState,
}

/// A vertex attribute's data type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttribType {
    Byte,
    UnsignedByte,
    Short,
    UnsignedShort,
    Int,
    UnsignedInt,
    HalfFloat,
    Float,
    Fixed,
    Int2101010Rev,
    UnsignedInt2101010Rev,
}

impl AttribType {
    /// Bytes per component (the whole value for packed types).
    pub fn bytes(self) -> u32 {
        match self {
            AttribType::Byte | AttribType::UnsignedByte => 1,
            AttribType::Short | AttribType::UnsignedShort | AttribType::HalfFloat => 2,
            _ => 4,
        }
    }

    pub fn packed(self) -> bool {
        matches!(self, AttribType::Int2101010Rev | AttribType::UnsignedInt2101010Rev)
    }
}

/// Where a vertex attribute comes from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Attrib {
    /// The buffer, if the array is enabled; otherwise the current value.
    pub buffer: Option<ResourceId>,
    pub offset: usize,
    /// Bytes from one element to the next (never 0: the front end
    /// computes tightly packed strides).
    pub stride: u32,
    pub size: u8,
    pub ty: AttribType,
    pub normalized: bool,
    /// `VertexAttribIPointer`: integers reach the shader unconverted.
    pub integer: bool,
    /// Instances per element (0: per vertex).
    pub divisor: u32,
    /// The current value's 32-bit components (when no array is enabled).
    pub current: [u32; 4],
}

/// A range of a buffer (uniform blocks, transform feedback).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BufferRange {
    pub buffer: ResourceId,
    pub offset: usize,
    pub size: usize,
}

/// Primitive kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Points,
    Lines,
    LineLoop,
    LineStrip,
    Triangles,
    TriangleStrip,
    TriangleFan,
}

/// Index types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexType {
    U8,
    U16,
    U32,
}

impl IndexType {
    pub fn bytes(self) -> usize {
        match self {
            IndexType::U8 => 1,
            IndexType::U16 => 2,
            IndexType::U32 => 4,
        }
    }

    /// The primitive restart index (`GL_PRIMITIVE_RESTART_FIXED_INDEX`).
    pub fn restart(self) -> u32 {
        match self {
            IndexType::U8 => 0xFF,
            IndexType::U16 => 0xFFFF,
            IndexType::U32 => 0xFFFF_FFFF,
        }
    }
}

/// Everything a draw needs.
#[derive(Clone, Copy)]
pub struct DrawState<'a> {
    pub framebuffer: &'a Framebuffer,
    pub viewport: Viewport,
    pub scissor: Option<Rect>,
    pub raster: Raster,
    pub depth_stencil: DepthStencil,
    pub blend: Blend,
    pub program: ProgramId,
    /// Default-block uniform storage.
    pub uniforms: &'a [[u32; 4]],
    /// The buffer range of each uniform block (by program block index).
    pub blocks: &'a [Option<BufferRange>],
    /// The texture of each sampler (by sampler index).
    pub textures: &'a [TextureBinding],
    /// Attributes by location.
    pub attribs: &'a [Attrib],
    /// The index buffer, for indexed draws.
    pub index: Option<(ResourceId, IndexType)>,
    pub primitive_restart: bool,
    /// Transform feedback buffers, if feedback is active.
    pub feedback: Option<&'a [BufferRange]>,
}

/// One draw call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DrawInfo {
    pub mode: Mode,
    /// First vertex (non-indexed) or byte offset into the index buffer.
    pub first: usize,
    pub count: u32,
    pub indexed: bool,
    pub instances: u32,
}

/// What `glClear`/`glClearBuffer*` clear.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Clear {
    /// Color attachments to clear, by attachment number, with their value
    /// (32-bit components: floats, or integers for integer formats).
    pub colors: [Option<[u32; 4]>; 8],
    pub depth: Option<f32>,
    pub stencil: Option<i32>,
    pub scissor: Option<Rect>,
    pub color_mask: [bool; 4],
    pub stencil_mask: u32,
}

/// A framebuffer blit.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Blit {
    pub src: Framebuffer,
    /// The read buffer (color attachment number).
    pub src_color: Option<u8>,
    pub dst: Framebuffer,
    /// Source and destination rectangles (x0, y0, x1, y1): mirrored when
    /// x1 < x0.
    pub src_rect: [i32; 4],
    pub dst_rect: [i32; 4],
    pub color: bool,
    pub depth: bool,
    pub stencil: bool,
    pub filter: Filter,
    pub scissor: Option<Rect>,
}

/// A query's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryKind {
    AnySamplesPassed,
    AnySamplesPassedConservative,
    PrimitivesWritten,
}

/// Memory from outside that a resource is made of: a display's picture,
/// which the GPU renders into in place. Rows of 32-bit pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct External {
    /// The memory object (a Veda handle the renderer duplicates, keeping
    /// its own), or 0 ...
    pub handle: u32,
    /// ... and its address in this process (host tests).
    pub address: usize,
    /// Its bytes.
    pub size: usize,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// Blue in the low byte of each pixel (`0xXXRRGGBB`); otherwise red.
    pub bgr: bool,
}

/// What a renderer can do.
#[derive(Clone, Debug)]
pub struct Caps {
    /// `GL_RENDERER`.
    pub name: &'static str,
    pub max_texture_size: u32,
    pub max_3d_texture_size: u32,
    pub max_cube_map_size: u32,
    pub max_array_layers: u32,
    pub max_renderbuffer_size: u32,
    pub max_samples: u32,
    pub max_viewport: u32,
    /// `EXT_color_buffer_float`.
    pub color_buffer_float: bool,
    /// `OES_texture_float_linear`.
    pub float_linear: bool,
    /// `GL_ALIASED_LINE_WIDTH_RANGE` (min, max).
    pub line_width: (f32, f32),
    /// `GL_ALIASED_POINT_SIZE_RANGE` (min, max).
    pub point_size: (f32, f32),
    pub max_anisotropy: f32,
}

/// A renderer.
pub trait Backend {
    fn caps(&self) -> &Caps;

    fn create_resource(&mut self, desc: &ResourceDesc) -> Result<ResourceId, OutOfMemory>;
    fn destroy_resource(&mut self, id: ResourceId);
    /// A render target `desc` (a renderbuffer of 8-bit RGB, single
    /// sampled) whose storage is `memory`: the GPU renders into the memory
    /// itself, in its pixels' order. Renderers that cannot refuse.
    fn import_resource(&mut self, desc: &ResourceDesc, memory: &External) -> Result<ResourceId, OutOfMemory> {
        let _ = (desc, memory);
        Err(OutOfMemory)
    }

    /// Writes data laid out in the resource's format (rows `row_pitch`
    /// bytes apart, slices `image_pitch`) into a region of a level.
    fn write(&mut self, id: ResourceId, level: u32, region: Region, data: &[u8], row_pitch: usize, image_pitch: usize);
    /// Reads a region of a level, laid out as for [`Backend::write`].
    fn read(
        &mut self,
        id: ResourceId,
        level: u32,
        region: Region,
        out: &mut [u8],
        row_pitch: usize,
        image_pitch: usize,
    );
    /// Copies bytes between buffers (or within one).
    fn copy_buffer(&mut self, src: ResourceId, src_offset: usize, dst: ResourceId, dst_offset: usize, size: usize);
    /// Copies a box of texels (`region` of level `src_level`) to level
    /// `dst_level` of another resource of the same format, at `(x, y, z)`.
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
    );
    /// Copies a framebuffer rectangle into a texture image
    /// (`CopyTexSubImage`), converting formats.
    fn copy_to_texture(&mut self, src: &Framebuffer, read: u8, rect: Rect, dst: Surface, x: u32, y: u32);
    fn blit(&mut self, blit: &Blit);
    /// Fills levels `base + 1..=last` from `base` (box filtered).
    fn generate_mipmap(&mut self, id: ResourceId, base: u32, last: u32);

    /// Compiles a linked program for this renderer.
    fn create_program(&mut self, program: Arc<vglsl::program::Program>) -> ProgramId;
    fn destroy_program(&mut self, id: ProgramId);

    fn draw(&mut self, state: &DrawState<'_>, info: &DrawInfo);
    fn clear(&mut self, framebuffer: &Framebuffer, clear: &Clear);

    fn begin_query(&mut self, kind: QueryKind) -> QueryId;
    fn end_query(&mut self, id: QueryId);
    fn destroy_query(&mut self, id: QueryId);
    /// A query's result, if available (waiting for it with `wait`).
    fn query_result(&mut self, id: QueryId, wait: bool) -> Option<u64>;

    /// Starts the work queued so far.
    fn flush(&mut self);
    /// Waits for all work to be done.
    fn finish(&mut self);
    /// Shows a color buffer (a 2D image `width` x `height`, resolved if
    /// multisampled) in a window's pixels, scaled to them (bilinearly) if
    /// their size differs.
    fn present(&mut self, color: ResourceId, width: u32, height: u32, dst: &mut Present<'_>) {
        present_by_reading(self, color, width, height, dst);
    }
    /// The format of a resource.
    fn format_of(&self, id: ResourceId) -> Option<Format>;
    /// A fence that signals when the work queued so far is done.
    fn fence(&mut self) -> u64;
    /// Whether `fence` has signaled, waiting for it up to `timeout_ns`.
    fn wait_fence(&mut self, fence: u64, timeout_ns: u64) -> bool;
}

/// Several resources' worth of data, for tests and reads.
pub type Bytes = Vec<u8>;

/// A window's pixels, as [`Backend::present`] fills them.
pub struct Present<'a> {
    /// 0xAARRGGBB words, rows from the top down.
    pub pixels: &'a mut [u32],
    /// Words from one row to the next.
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    /// Alpha is 255; otherwise colors are premultiplied by it.
    pub opaque: bool,
}

/// A color as a 0xAARRGGBB word: opaque, or premultiplied by its alpha.
#[inline]
pub fn to_argb(c: [f32; 4], opaque: bool) -> u32 {
    let u = |x: f32| (if x > 0.0 { if x < 1.0 { x } else { 1.0 } } else { 0.0 } * 255.0 + 0.5) as u32;
    let a = if opaque { 1.0 } else { c[3] };
    let k = if opaque { 1.0 } else { a };
    (u(a) << 24) | (u(c[0] * k) << 16) | (u(c[1] * k) << 8) | u(c[2] * k)
}

/// Presents by reading the color buffer back row by row and converting
/// and scaling on the CPU (the default [`Backend::present`]).
pub fn present_by_reading<B: Backend + ?Sized>(
    b: &mut B,
    color: ResourceId,
    width: u32,
    height: u32,
    dst: &mut Present<'_>,
) {
    let Some(format) = b.format_of(color) else { return };
    let tb = format.bytes();
    let row = width as usize * tb;
    let mut texels = alloc::vec![0u8; row];
    let mut image = alloc::vec![0u32; width as usize * height as usize];
    for y in 0..height as usize {
        // Row 0 of a GL image is the bottom one.
        let src = height as usize - 1 - y;
        b.read(color, 0, Region::new(0, src as u32, 0, width, 1, 1), &mut texels, row, row);
        let line = &mut image[y * width as usize..(y + 1) * width as usize];
        for (d, t) in line.iter_mut().zip(texels.chunks_exact(tb)) {
            *d = to_argb(crate::pixels::decode_raw(format, t).as_float(), dst.opaque);
        }
    }
    let rows = 0..dst.height as usize;
    crate::present::scale(&image, width as usize, height as usize, dst, rows);
}
