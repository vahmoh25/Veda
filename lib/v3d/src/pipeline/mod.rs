//! The fixed-point pipeline behind [`crate::Renderer`]: vertex
//! transformation and lighting ([`transform`]), triangle setup
//! ([`setup`]), tile rasterisation ([`raster`]) and presentation of the
//! rendered image ([`post`]).
//!
//! Everything after the per-draw matrix set-up is integer arithmetic.
//! Vindows usually runs under QEMU's TCG emulator, where scalar integer
//! operations cost ~0.5 ns and an integer division ~1 ns, but every
//! floating-point instruction ~6-30 ns. Fixed-point formats used here:
//!
//! * positions, clip coordinates, texture coordinates: 16.16
//! * unit normals and directions: 2.14 (1.0 = 16384)
//! * light colours and multipliers: 8.8 (1.0 = 256)
//! * additive colours: output units 0..255 scaled by 256
//! * screen coordinates: 28.4 (1/16 pixel)
//! * depth: q = near / w, scaled to 2^30 at the near plane (bigger = closer)
//!
//! Intermediate products that could exceed their type for extreme inputs
//! use wrapping arithmetic, so the results never depend on the build
//! profile.

pub mod post;
pub mod raster;
pub mod setup;
pub mod transform;

pub use post::{upscale, upscale2x, upscale2x_fast, upscale2x_swar};
pub use raster::{clear_rect, raster};
pub use setup::{setup_batch, setup_tri};
pub use transform::{project, transform};

/// A mesh vertex in fixed point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FxVertex {
    /// Object-space position, 16.16.
    pub x: i32,
    /// See `x`.
    pub y: i32,
    /// See `x`.
    pub z: i32,
    /// Unit normal, 2.14.
    pub nx: i16,
    /// See `nx`.
    pub ny: i16,
    /// See `nx`.
    pub nz: i16,
    /// Texture coordinates, 16.16 (1.0 = one repeat).
    pub u: i32,
    /// See `u`.
    pub v: i32,
    /// Straight-alpha 0xAARRGGBB.
    pub color: u32,
}

/// A transformed, lit vertex.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TVert {
    /// Clip-space x, 16.16.
    pub cx: i32,
    /// Clip-space y, 16.16.
    pub cy: i32,
    /// Clip-space w (the view depth), 16.16.
    pub cw: i32,
    /// Screen x, 28.4 (valid without `OC_NEAR`).
    pub sx: i32,
    /// Screen y, 28.4 (valid without `OC_NEAR`).
    pub sy: i32,
    /// Depth: near / w * 2^30 (valid without `OC_NEAR`).
    pub q: i32,
    /// Texture coordinates, 16.16.
    pub u: i32,
    /// See `u`.
    pub v: i32,
    /// Red, green and blue multipliers 8.8; alpha 0..256.
    pub mul: [u16; 4],
    /// Additive red, green and blue in output units * 256.
    pub add: [u16; 3],
    /// `OC_*` bits.
    pub outcode: u16,
}

/// Outcode: behind the near plane.
pub const OC_NEAR: u16 = 1;
/// Outcode: beyond the far plane.
pub const OC_FAR: u16 = 2;
/// Outcode: left of the view.
pub const OC_LEFT: u16 = 4;
/// Outcode: right of the view.
pub const OC_RIGHT: u16 = 8;
/// Outcode: below the view.
pub const OC_BOTTOM: u16 = 16;
/// Outcode: above the view.
pub const OC_TOP: u16 = 32;
/// Outcode: outside the guard band, so a triangle using the vertex needs
/// clipping.
pub const OC_GUARD: u16 = 64;
/// A triangle whose vertices share one of these bits is invisible.
pub const OC_REJECT: u16 = OC_NEAR | OC_FAR | OC_LEFT | OC_RIGHT | OC_BOTTOM | OC_TOP;
/// A triangle with a vertex outside any of these needs clipping.
pub const OC_CLIP: u16 = OC_NEAR | OC_GUARD;

/// A point light in object space.
#[derive(Clone, Copy, Debug, Default)]
pub struct FxPointLight {
    /// Position, 16.16.
    pub pos: [i32; 3],
    /// Radius, 16.16.
    pub radius: i32,
    /// Colour times the material's diffuse colour, 8.8.
    pub rgb: [i32; 3],
}

/// Compute lighting (otherwise the multiplier is the base colour).
pub const XF_LIT: u32 = 1;
/// Multiply by the vertex colour.
pub const XF_VCOLOR: u32 = 2;
/// Apply distance fog.
pub const XF_FOG: u32 = 4;
/// Fog only fades the colour (additive materials).
pub const XF_FOG_FADE: u32 = 8;
/// Blinn-Phong specular highlight.
pub const XF_SPECULAR: u32 = 16;
/// Light both sides (|n.l|).
pub const XF_TWO_SIDED: u32 = 32;

/// Point lights per draw.
pub const MAX_POINT_LIGHTS: usize = 4;

/// Per-draw transformation and lighting parameters.
#[derive(Clone, Copy, Debug, Default)]
pub struct Xform {
    /// Object to clip space, row major, 16.16 (row 2 unused).
    pub mvp: [i32; 16],
    /// Towards the sun (object space), 2.14.
    pub light_dir: [i32; 4],
    /// World up (object space), 2.14.
    pub up_dir: [i32; 4],
    /// Blinn-Phong half vector (object space), 2.14.
    pub half_dir: [i32; 4],
    /// Sun colour times the material, 8.8.
    pub sun_rgb: [i32; 4],
    /// Ambient light from above times the material, 8.8.
    pub sky_rgb: [i32; 4],
    /// Ambient light from below times the material, 8.8.
    pub ground_rgb: [i32; 4],
    /// Specular colour, output units * 256.
    pub spec_rgb: [i32; 4],
    /// Emissive colour, output units * 256.
    pub emit_rgb: [i32; 4],
    /// Unlit colour multiplier, 8.8 (alpha 0..256).
    pub base_rgba: [i32; 4],
    /// Fog colour, output units * 256.
    pub fog_rgb: [i32; 4],
    /// View depth where fog starts, 16.16.
    pub fog_start: i32,
    /// fog = (w - start) * fog_mul >> 32 (0..256).
    pub fog_mul: i32,
    /// Maximum fog, 0..256.
    pub fog_max: i32,
    /// Shininess = 2^spec_power.
    pub spec_power: i32,
    /// `XF_*` bits.
    pub flags: u32,
    /// Point lights used.
    pub num_points: i32,
    /// The point lights (the first `num_points`).
    pub points: [FxPointLight; MAX_POINT_LIGHTS],
    /// Near plane, 16.16.
    pub near_w: i32,
    /// Far plane, 16.16.
    pub far_w: i32,
    /// Guard band as a multiple of w, 16.16.
    pub guard: i32,
    /// Viewport centre, 28.4.
    pub vp_x: i32,
    /// See `vp_x`.
    pub vp_y: i32,
    /// Viewport half size, 28.4.
    pub vp_sx: i32,
    /// See `vp_sx`.
    pub vp_sy: i32,
    /// Near plane (16.16) << 30.
    pub q_scale: i64,
}

/// One mip level of a texture.
#[derive(Clone, Copy, Debug)]
pub struct Mip {
    /// Premultiplied 0xAARRGGBB, `1 << wlog2` by `1 << hlog2` texels.
    pub pixels: *const u32,
    /// log2 of the width.
    pub wlog2: i32,
    /// log2 of the height.
    pub hlog2: i32,
}

/// Mip levels per texture (up to 4096 x 4096 texels).
pub const MAX_MIPS: usize = 13;

/// A mip-mapped texture as the rasteriser reads it.
#[derive(Clone, Copy, Debug)]
pub struct FxTexture {
    /// The levels, largest first (the first `count`).
    pub levels: [Mip; MAX_MIPS],
    /// Number of levels.
    pub count: i32,
}

/// Mode: textured (modulate and add).
pub const M_TEX: u32 = 1;
/// Mode: the blend mode bits.
pub const M_BLEND_MASK: u32 = 6;
/// Blend mode: opaque.
pub const M_OPAQUE: u32 = 0;
/// Blend mode: premultiplied source-over.
pub const M_ALPHA: u32 = 2;
/// Blend mode: additive.
pub const M_ADD: u32 = 4;
/// Blend mode: multiply (darken).
pub const M_MUL: u32 = 6;
/// Mode: alpha test (discard alpha < 128).
pub const M_ATEST: u32 = 8;
/// Mode: depth test.
pub const M_ZTEST: u32 = 16;
/// Mode: depth write.
pub const M_ZWRITE: u32 = 32;
/// Mode: affine texture mapping (screen-aligned quads).
pub const M_AFFINE: u32 = 64;
/// Mode: constant colour (untextured; set by the setup).
pub const M_FLAT: u32 = 128;
/// Rasteriser variants: the mode bits below this select one.
pub const M_COUNT: u32 = 256;
/// Mode: bilinear filtering when magnifying (mip level 0).
pub const M_BILINEAR: u32 = 256;

/// Work counters of [`raster`] (for profiling).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RasterStats {
    /// Triangle/tile pairs visited.
    pub triangles: u64,
    /// Non-empty rows.
    pub spans: u64,
    /// Pixels covered (before the depth test).
    pub pixels: u64,
}

/// Culling: none.
pub const CULL_NONE: u32 = 0;
/// Culling: back faces.
pub const CULL_BACK: u32 = 1;
/// Culling: front faces.
pub const CULL_FRONT: u32 = 2;

/// Per-draw triangle setup parameters.
#[derive(Clone, Copy, Debug)]
pub struct Setup {
    /// Render target width in pixels.
    pub width: i32,
    /// Render target height in pixels.
    pub height: i32,
    /// `M_*` bits.
    pub mode: u32,
    /// `CULL_*`.
    pub cull: u32,
    /// The texture when `M_TEX` (must stay valid while triangles use it).
    pub tex: *const FxTexture,
    /// Mip bias in quarter levels.
    pub lod_bias: i32,
}

/// A set-up triangle ready for rasterisation. Its planes are values at the
/// centre of the reference pixel (`rx`, `ry`) plus steps per pixel in x
/// and y.
#[derive(Clone, Copy, Debug)]
pub struct Tri {
    /// Pixel bounding box [x0, x1) x [y0, y1).
    pub x0: i32,
    /// See `x0`.
    pub y0: i32,
    /// See `x0`.
    pub x1: i32,
    /// See `x0`.
    pub y1: i32,
    /// Reference pixel x.
    pub rx: i32,
    /// Reference pixel y.
    pub ry: i32,
    /// `M_*` bits.
    pub mode: u32,
    /// Per-pixel mip level = lod_base - log2(qt), in quarter levels.
    pub lod_base: i32,
    /// Edge functions at the reference pixel (inside where all are >= 0).
    pub e: [i64; 3],
    /// Edge function steps per pixel in x.
    pub ex: [i32; 3],
    /// Edge function steps per pixel in y.
    pub ey: [i32; 3],
    /// Depth q with `Z_FRAC` extra fraction bits.
    pub z: i64,
    /// Depth step in x.
    pub zx: i64,
    /// Depth step in y.
    pub zy: i64,
    /// Colours 8.16: multipliers r, g, b, a (or the final rgba) and the
    /// additive r, g, b.
    pub c: [i32; 7],
    /// Colour steps in x.
    pub cx: [i32; 7],
    /// Colour steps in y.
    pub cy: [i32; 7],
    /// u * qt (u in texels * 256, rebased), with `T_FRAC` extra bits.
    pub uq: i64,
    /// Step of `uq` in x.
    pub uqx: i64,
    /// Step of `uq` in y.
    pub uqy: i64,
    /// v * qt, like `uq`.
    pub vq: i64,
    /// Step of `vq` in x.
    pub vqx: i64,
    /// Step of `vq` in y.
    pub vqy: i64,
    /// Per-triangle normalised q for perspective-correct texturing.
    pub qt: i64,
    /// Step of `qt` in x.
    pub qtx: i64,
    /// Step of `qt` in y.
    pub qty: i64,
    /// The texture (null when untextured).
    pub tex: *const FxTexture,
}

impl Tri {
    /// An all-zero triangle.
    pub const ZERO: Tri = Tri {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
        rx: 0,
        ry: 0,
        mode: 0,
        lod_base: 0,
        e: [0; 3],
        ex: [0; 3],
        ey: [0; 3],
        z: 0,
        zx: 0,
        zy: 0,
        c: [0; 7],
        cx: [0; 7],
        cy: [0; 7],
        uq: 0,
        uqx: 0,
        uqy: 0,
        vq: 0,
        vqx: 0,
        vqy: 0,
        qt: 0,
        qtx: 0,
        qty: 0,
        tex: core::ptr::null(),
    };
}

/// Extra fraction bits of the depth planes.
pub const Z_FRAC: u32 = 8;
/// Extra fraction bits of the texture planes.
pub const T_FRAC: u32 = 4;
/// Extra fraction bits of the colour planes.
pub const C_FRAC: u32 = 8;

/// A render target: colour and depth buffers of the same layout.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// Opaque 0xAARRGGBB pixels.
    pub color: *mut u32,
    /// Depth q of the nearest surface so far (0 = nothing).
    pub depth: *mut i32,
    /// Pixels per row (both buffers).
    pub stride: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

/// Keeps the loop it is called in scalar. LLVM would otherwise vectorise
/// some of the pipeline's loops into SSE integer multiplies, shuffles and
/// spills, which QEMU's TCG emulates with slow helper calls: scalar code
/// is several times faster there. The empty assembly block is opaque to
/// the vectoriser and emits no instructions.
#[inline(always)]
fn scalar_loop() {
    // SAFETY: an empty assembly block has no effect.
    unsafe { core::arch::asm!("", options(nomem, nostack, preserves_flags)) }
}

/// Number of significant bits of `x` (0 for 0).
#[inline(always)]
fn bitlen64(x: u64) -> i32 {
    64 - x.leading_zeros() as i32
}

/// log2(x) in quarter steps: 4 * floor(log2 x) plus the next two mantissa
/// bits as a linear fraction (0 counts as 1).
#[inline(always)]
fn log2q(x: u64) -> i32 {
    let x = x | 1;
    let top = 63 - x.leading_zeros() as i32;
    let frac = if top >= 2 { (x >> (top - 2)) & 3 } else { (x << (2 - top)) & 3 } as i32;
    top * 4 + frac
}
