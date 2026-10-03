//! The C++ rasterizer core (`lib/v3d/cpp`): `#[repr(C)]` mirrors of its
//! structures and the raw `extern "C"` functions.
//!
//! The layouts must match `cpp/v3d_core.h` exactly; both sides assert the
//! structure sizes at compile time. Safe code uses these through
//! [`crate::renderer`].

#![allow(missing_docs)]

/// A mesh vertex in fixed point (positions and UVs 16.16, normal 2.14).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FxVertex {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub nx: i16,
    pub ny: i16,
    pub nz: i16,
    pub pad: i16,
    pub u: i32,
    pub v: i32,
    pub color: u32,
}

/// A transformed, lit vertex (clip space 16.16, screen 28.4).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TVert {
    pub cx: i32,
    pub cy: i32,
    pub cw: i32,
    pub sx: i32,
    pub sy: i32,
    pub q: i32,
    pub u: i32,
    pub v: i32,
    pub mul: [u16; 4],
    pub add: [u16; 3],
    pub outcode: u16,
}

pub const OC_NEAR: u16 = 1;
pub const OC_FAR: u16 = 2;
pub const OC_LEFT: u16 = 4;
pub const OC_RIGHT: u16 = 8;
pub const OC_BOTTOM: u16 = 16;
pub const OC_TOP: u16 = 32;
pub const OC_GUARD: u16 = 64;
pub const OC_REJECT: u16 = OC_NEAR | OC_FAR | OC_LEFT | OC_RIGHT | OC_BOTTOM | OC_TOP;
pub const OC_CLIP: u16 = OC_NEAR | OC_GUARD;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FxPointLight {
    pub pos: [i32; 3],
    pub radius: i32,
    pub rgb: [i32; 3],
    pub pad: i32,
}

pub const XF_LIT: u32 = 1;
pub const XF_VCOLOR: u32 = 2;
pub const XF_FOG: u32 = 4;
pub const XF_FOG_FADE: u32 = 8;
pub const XF_SPECULAR: u32 = 16;
pub const XF_TWO_SIDED: u32 = 32;

pub const MAX_POINT_LIGHTS: usize = 4;

/// Per-draw transformation and lighting parameters.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Xform {
    pub mvp: [i32; 16],
    pub light_dir: [i32; 4],
    pub up_dir: [i32; 4],
    pub half_dir: [i32; 4],
    pub sun_rgb: [i32; 4],
    pub sky_rgb: [i32; 4],
    pub ground_rgb: [i32; 4],
    pub spec_rgb: [i32; 4],
    pub emit_rgb: [i32; 4],
    pub base_rgba: [i32; 4],
    pub fog_rgb: [i32; 4],
    pub fog_start: i32,
    pub fog_mul: i32,
    pub fog_max: i32,
    pub spec_power: i32,
    pub flags: u32,
    pub num_points: i32,
    pub points: [FxPointLight; MAX_POINT_LIGHTS],
    pub near_w: i32,
    pub far_w: i32,
    pub guard: i32,
    pub vp_x: i32,
    pub vp_y: i32,
    pub vp_sx: i32,
    pub vp_sy: i32,
    pub pad: i32,
    pub q_scale: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Mip {
    pub pixels: *const u32,
    pub wlog2: i32,
    pub hlog2: i32,
}

pub const MAX_MIPS: usize = 13;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FxTexture {
    pub levels: [Mip; MAX_MIPS],
    pub count: i32,
    pub flags: u32,
}

pub const M_TEX: u32 = 1;
pub const M_OPAQUE: u32 = 0;
pub const M_ALPHA: u32 = 2;
pub const M_ADD: u32 = 4;
pub const M_MUL: u32 = 6;
pub const M_ATEST: u32 = 8;
pub const M_ZTEST: u32 = 16;
pub const M_ZWRITE: u32 = 32;
pub const M_AFFINE: u32 = 64;
pub const M_FLAT: u32 = 128;
pub const M_BILINEAR: u32 = 256;

/// Work counters of `v3d_raster`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RasterStats {
    pub triangles: u64,
    pub spans: u64,
    pub pixels: u64,
}

pub const CULL_NONE: u32 = 0;
pub const CULL_BACK: u32 = 1;
pub const CULL_FRONT: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Setup {
    pub width: i32,
    pub height: i32,
    pub mode: u32,
    pub cull: u32,
    pub tex: *const FxTexture,
    pub lod_bias: i32,
    pub pad: i32,
}

/// A set-up triangle (see `V3dTri` in `v3d_core.h`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Tri {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub rx: i32,
    pub ry: i32,
    pub mode: u32,
    pub lod_base: i32,
    pub e: [i64; 3],
    pub ex: [i32; 3],
    pub ey: [i32; 3],
    pub z: i64,
    pub zx: i64,
    pub zy: i64,
    pub c: [i32; 7],
    pub cx: [i32; 7],
    pub cy: [i32; 7],
    pub pad: i32,
    pub uq: i64,
    pub uqx: i64,
    pub uqy: i64,
    pub vq: i64,
    pub vqx: i64,
    pub vqy: i64,
    pub qt: i64,
    pub qtx: i64,
    pub qty: i64,
    pub tex: *const FxTexture,
}

impl Tri {
    /// An all-zero triangle (used to reserve space before the C++ setup
    /// writes it).
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
        pad: 0,
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

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub color: *mut u32,
    pub depth: *mut i32,
    pub stride: i32,
    pub width: i32,
    pub height: i32,
}

const _: () = assert!(core::mem::size_of::<FxVertex>() == 32);
const _: () = assert!(core::mem::size_of::<TVert>() == 48);
const _: () = assert!(core::mem::size_of::<FxPointLight>() == 32);
const _: () = assert!(core::mem::size_of::<Xform>() == 416);
const _: () = assert!(core::mem::size_of::<FxTexture>() == 13 * 16 + 8);
const _: () = assert!(core::mem::size_of::<Setup>() == 32);
const _: () = assert!(core::mem::size_of::<Tri>() == 272);

unsafe extern "C" {
    pub fn v3d_transform(xf: *const Xform, input: *const FxVertex, count: i32, out: *mut TVert);
    pub fn v3d_project(xf: *const Xform, v: *mut TVert);
    pub fn v3d_setup_batch(
        s: *const Setup,
        verts: *const TVert,
        indices: *const u32,
        count: i32,
        out: *mut Tri,
        deferred: *mut u32,
        deferred_count: *mut i32,
    ) -> i32;
    pub fn v3d_setup_tri(s: *const Setup, a: *const TVert, b: *const TVert, c: *const TVert, out: *mut Tri) -> i32;
    pub fn v3d_clear_rect(t: *const Target, x0: i32, y0: i32, x1: i32, y1: i32, row_colors: *const u32);
    pub fn v3d_raster(
        t: *const Target,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        tris: *const Tri,
        indices: *const u32,
        count: i32,
        stats: *mut RasterStats,
    );
    pub fn v3d_upscale(
        src: *const u32,
        sw: i32,
        sh: i32,
        sstride: i32,
        dst: *mut u32,
        dw: i32,
        dh: i32,
        dstride: i32,
        y0: i32,
        y1: i32,
    );
    pub fn v3d_upscale2x(
        src: *const u32,
        sw: i32,
        sh: i32,
        sstride: i32,
        dst: *mut u32,
        dstride: i32,
        y0: i32,
        y1: i32,
    );
    pub fn v3d_upscale2x_fast(
        src: *const u32,
        sw: i32,
        sh: i32,
        sstride: i32,
        dst: *mut u32,
        dstride: i32,
        y0: i32,
        y1: i32,
    );
    pub fn v3d_upscale2x_swar(
        src: *const u32,
        sw: i32,
        sh: i32,
        sstride: i32,
        dst: *mut u32,
        dstride: i32,
        y0: i32,
        y1: i32,
    );
}
