//! Pixel formats: what resources store, what OpenGL ES calls them, and
//! conversion to and from the pixel data applications pass.
//!
//! * [`Format`] is a storage layout, the same in every back end: texels are
//!   packed as GL's packed types describe them (`RGB565` is a 16-bit value
//!   with red in the top five bits), other formats are their components in
//!   order, little-endian. RGB formats of 8, 16 and 32-bit components are
//!   stored with a fourth, unused component, as GPUs keep them.
//! * [`Internal`] describes each sized internal format of OpenGL ES 3.0
//!   (tables 3.13 and 3.14), and the unsized and luminance/alpha formats of
//!   OpenGL ES 2.0 with their effective sized formats.
//! * [`tex_format`] checks a `(internalformat, format, type)` combination
//!   against tables 3.2 and 3.3; [`Client`] decodes and encodes pixels in
//!   any of those external layouts through [`Texel`], a neutral form, with
//!   GL's conversion rules (equations 2.1 to 2.4).

use crate::gl;
use vglsl::ops::{f16_to_f32, f32_to_f16};
use vmath::f32 as m;

/// A storage layout.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Format {
    R8Unorm,
    R8Snorm,
    R8Uint,
    R8Sint,
    Rg8Unorm,
    Rg8Snorm,
    Rg8Uint,
    Rg8Sint,
    /// RGB8, with an unused fourth byte.
    Rgbx8Unorm,
    Rgbx8Snorm,
    Rgbx8Srgb,
    Rgbx8Uint,
    Rgbx8Sint,
    Rgba8Unorm,
    Rgba8Snorm,
    Rgba8Srgb,
    Rgba8Uint,
    Rgba8Sint,
    R16Float,
    R16Uint,
    R16Sint,
    Rg16Float,
    Rg16Uint,
    Rg16Sint,
    Rgbx16Float,
    Rgbx16Uint,
    Rgbx16Sint,
    Rgba16Float,
    Rgba16Uint,
    Rgba16Sint,
    R32Float,
    R32Uint,
    R32Sint,
    Rg32Float,
    Rg32Uint,
    Rg32Sint,
    Rgbx32Float,
    Rgbx32Uint,
    Rgbx32Sint,
    Rgba32Float,
    Rgba32Uint,
    Rgba32Sint,
    /// `UNSIGNED_SHORT_5_6_5`: red in bits 11-15.
    B5G6R5Unorm,
    /// `UNSIGNED_SHORT_4_4_4_4`: red in bits 12-15, alpha in 0-3.
    Rgba4Unorm,
    /// `UNSIGNED_SHORT_5_5_5_1`: red in bits 11-15, alpha in bit 0.
    Rgb5A1Unorm,
    /// `UNSIGNED_INT_2_10_10_10_REV`: red in bits 0-9, alpha in 30-31.
    Rgb10A2Unorm,
    Rgb10A2Uint,
    /// `UNSIGNED_INT_10F_11F_11F_REV`.
    R11G11B10Float,
    /// `UNSIGNED_INT_5_9_9_9_REV`.
    Rgb9E5Float,
    /// 16-bit depth.
    D16Unorm,
    /// 24-bit depth in bits 8-31, bits 0-7 unused.
    D24Unorm,
    /// `UNSIGNED_INT_24_8`: depth in bits 8-31, stencil in 0-7.
    D24UnormS8Uint,
    D32Float,
    /// `FLOAT_32_UNSIGNED_INT_24_8_REV`: a float depth, then a word with
    /// the stencil in its low byte.
    D32FloatS8Uint,
    S8Uint,
    /// OpenGL ES 2.0's luminance and alpha formats.
    L8Unorm,
    A8Unorm,
    L8A8Unorm,
    L16Float,
    A16Float,
    L16A16Float,
    L32Float,
    A32Float,
    L32A32Float,
}

/// What a format's components are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Unorm,
    Snorm,
    Float,
    Uint,
    Sint,
    Depth,
    DepthStencil,
    Stencil,
}

impl Format {
    /// Bytes per texel.
    pub fn bytes(self) -> usize {
        use Format::*;
        match self {
            R8Unorm | R8Snorm | R8Uint | R8Sint | S8Uint | L8Unorm | A8Unorm => 1,
            Rg8Unorm | Rg8Snorm | Rg8Uint | Rg8Sint | R16Float | R16Uint | R16Sint | B5G6R5Unorm | Rgba4Unorm
            | Rgb5A1Unorm | D16Unorm | L8A8Unorm | L16Float | A16Float => 2,
            Rgbx8Unorm | Rgbx8Snorm | Rgbx8Srgb | Rgbx8Uint | Rgbx8Sint | Rgba8Unorm | Rgba8Snorm | Rgba8Srgb
            | Rgba8Uint | Rgba8Sint | Rg16Float | Rg16Uint | Rg16Sint | R32Float | R32Uint | R32Sint | Rgb10A2Unorm
            | Rgb10A2Uint | R11G11B10Float | Rgb9E5Float | D24Unorm | D24UnormS8Uint | D32Float | L16A16Float
            | L32Float | A32Float => 4,
            Rgbx16Float | Rgbx16Uint | Rgbx16Sint | Rgba16Float | Rgba16Uint | Rgba16Sint | Rg32Float | Rg32Uint
            | Rg32Sint | D32FloatS8Uint | L32A32Float => 8,
            Rgbx32Float | Rgbx32Uint | Rgbx32Sint | Rgba32Float | Rgba32Uint | Rgba32Sint => 16,
        }
    }

    pub fn class(self) -> Class {
        use Format::*;
        match self {
            R8Snorm | Rg8Snorm | Rgbx8Snorm | Rgba8Snorm => Class::Snorm,
            R8Uint | Rg8Uint | Rgbx8Uint | Rgba8Uint | R16Uint | Rg16Uint | Rgbx16Uint | Rgba16Uint | R32Uint
            | Rg32Uint | Rgbx32Uint | Rgba32Uint | Rgb10A2Uint => Class::Uint,
            R8Sint | Rg8Sint | Rgbx8Sint | Rgba8Sint | R16Sint | Rg16Sint | Rgbx16Sint | Rgba16Sint | R32Sint
            | Rg32Sint | Rgbx32Sint | Rgba32Sint => Class::Sint,
            R16Float | Rg16Float | Rgbx16Float | Rgba16Float | R32Float | Rg32Float | Rgbx32Float | Rgba32Float
            | R11G11B10Float | Rgb9E5Float | L16Float | A16Float | L16A16Float | L32Float | A32Float | L32A32Float => {
                Class::Float
            }
            D16Unorm | D24Unorm | D32Float => Class::Depth,
            D24UnormS8Uint | D32FloatS8Uint => Class::DepthStencil,
            S8Uint => Class::Stencil,
            _ => Class::Unorm,
        }
    }

    pub fn is_srgb(self) -> bool {
        matches!(self, Format::Rgbx8Srgb | Format::Rgba8Srgb)
    }

    pub fn has_depth(self) -> bool {
        matches!(self.class(), Class::Depth | Class::DepthStencil)
    }

    pub fn has_stencil(self) -> bool {
        matches!(self.class(), Class::Stencil | Class::DepthStencil)
    }

    /// Whether texels are integers (sampled without filtering, written
    /// without conversion).
    pub fn is_integer(self) -> bool {
        matches!(self.class(), Class::Uint | Class::Sint)
    }

    /// The color as a shader sees it, from stored bytes: normalised and
    /// float formats as floats, integer formats as integers. Missing
    /// components read as 0, alpha as 1 (luminance as L, L, L, 1; alpha
    /// as 0, 0, 0, A).
    pub fn decode(self, b: &[u8]) -> Texel {
        use Format::*;
        let u8n = |i: usize| f32::from(b[i]) / 255.0;
        let s8n = |i: usize| (f32::from(b[i] as i8) / 127.0).max(-1.0);
        let h = |i: usize| f16_to_f32(u16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
        let f = |i: usize| f32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        let u16x = |i: usize| u32::from(u16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
        let i16x = |i: usize| i32::from(i16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
        let u32x = |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        let word = || u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let half = || u16::from_le_bytes([b[0], b[1]]);
        match self {
            R8Unorm => Texel::Float([u8n(0), 0.0, 0.0, 1.0]),
            R8Snorm => Texel::Float([s8n(0), 0.0, 0.0, 1.0]),
            R8Uint => Texel::Uint([u32::from(b[0]), 0, 0, 1]),
            R8Sint => Texel::Int([i32::from(b[0] as i8), 0, 0, 1]),
            Rg8Unorm => Texel::Float([u8n(0), u8n(1), 0.0, 1.0]),
            Rg8Snorm => Texel::Float([s8n(0), s8n(1), 0.0, 1.0]),
            Rg8Uint => Texel::Uint([u32::from(b[0]), u32::from(b[1]), 0, 1]),
            Rg8Sint => Texel::Int([i32::from(b[0] as i8), i32::from(b[1] as i8), 0, 1]),
            Rgbx8Unorm => Texel::Float([u8n(0), u8n(1), u8n(2), 1.0]),
            Rgbx8Snorm => Texel::Float([s8n(0), s8n(1), s8n(2), 1.0]),
            Rgbx8Srgb => Texel::Float([srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), 1.0]),
            Rgbx8Uint => Texel::Uint([u32::from(b[0]), u32::from(b[1]), u32::from(b[2]), 1]),
            Rgbx8Sint => Texel::Int([i32::from(b[0] as i8), i32::from(b[1] as i8), i32::from(b[2] as i8), 1]),
            Rgba8Unorm => Texel::Float([u8n(0), u8n(1), u8n(2), u8n(3)]),
            Rgba8Snorm => Texel::Float([s8n(0), s8n(1), s8n(2), s8n(3)]),
            Rgba8Srgb => Texel::Float([srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), u8n(3)]),
            Rgba8Uint => Texel::Uint([u32::from(b[0]), u32::from(b[1]), u32::from(b[2]), u32::from(b[3])]),
            Rgba8Sint => {
                Texel::Int([i32::from(b[0] as i8), i32::from(b[1] as i8), i32::from(b[2] as i8), i32::from(b[3] as i8)])
            }
            R16Float => Texel::Float([h(0), 0.0, 0.0, 1.0]),
            R16Uint => Texel::Uint([u16x(0), 0, 0, 1]),
            R16Sint => Texel::Int([i16x(0), 0, 0, 1]),
            Rg16Float => Texel::Float([h(0), h(1), 0.0, 1.0]),
            Rg16Uint => Texel::Uint([u16x(0), u16x(1), 0, 1]),
            Rg16Sint => Texel::Int([i16x(0), i16x(1), 0, 1]),
            Rgbx16Float => Texel::Float([h(0), h(1), h(2), 1.0]),
            Rgbx16Uint => Texel::Uint([u16x(0), u16x(1), u16x(2), 1]),
            Rgbx16Sint => Texel::Int([i16x(0), i16x(1), i16x(2), 1]),
            Rgba16Float => Texel::Float([h(0), h(1), h(2), h(3)]),
            Rgba16Uint => Texel::Uint([u16x(0), u16x(1), u16x(2), u16x(3)]),
            Rgba16Sint => Texel::Int([i16x(0), i16x(1), i16x(2), i16x(3)]),
            R32Float => Texel::Float([f(0), 0.0, 0.0, 1.0]),
            R32Uint => Texel::Uint([u32x(0), 0, 0, 1]),
            R32Sint => Texel::Int([u32x(0) as i32, 0, 0, 1]),
            Rg32Float => Texel::Float([f(0), f(1), 0.0, 1.0]),
            Rg32Uint => Texel::Uint([u32x(0), u32x(1), 0, 1]),
            Rg32Sint => Texel::Int([u32x(0) as i32, u32x(1) as i32, 0, 1]),
            Rgbx32Float => Texel::Float([f(0), f(1), f(2), 1.0]),
            Rgbx32Uint => Texel::Uint([u32x(0), u32x(1), u32x(2), 1]),
            Rgbx32Sint => Texel::Int([u32x(0) as i32, u32x(1) as i32, u32x(2) as i32, 1]),
            Rgba32Float => Texel::Float([f(0), f(1), f(2), f(3)]),
            Rgba32Uint => Texel::Uint([u32x(0), u32x(1), u32x(2), u32x(3)]),
            Rgba32Sint => Texel::Int([u32x(0) as i32, u32x(1) as i32, u32x(2) as i32, u32x(3) as i32]),
            B5G6R5Unorm => {
                let v = half();
                Texel::Float([
                    f32::from((v >> 11) & 31) / 31.0,
                    f32::from((v >> 5) & 63) / 63.0,
                    f32::from(v & 31) / 31.0,
                    1.0,
                ])
            }
            Rgba4Unorm => {
                let v = half();
                Texel::Float([
                    f32::from((v >> 12) & 15) / 15.0,
                    f32::from((v >> 8) & 15) / 15.0,
                    f32::from((v >> 4) & 15) / 15.0,
                    f32::from(v & 15) / 15.0,
                ])
            }
            Rgb5A1Unorm => {
                let v = half();
                Texel::Float([
                    f32::from((v >> 11) & 31) / 31.0,
                    f32::from((v >> 6) & 31) / 31.0,
                    f32::from((v >> 1) & 31) / 31.0,
                    f32::from(v & 1),
                ])
            }
            Rgb10A2Unorm => {
                let v = word();
                Texel::Float([
                    (v & 1023) as f32 / 1023.0,
                    ((v >> 10) & 1023) as f32 / 1023.0,
                    ((v >> 20) & 1023) as f32 / 1023.0,
                    (v >> 30) as f32 / 3.0,
                ])
            }
            Rgb10A2Uint => {
                let v = word();
                Texel::Uint([v & 1023, (v >> 10) & 1023, (v >> 20) & 1023, v >> 30])
            }
            R11G11B10Float => {
                let v = word();
                Texel::Float([uf11_to_f32(v & 0x7FF), uf11_to_f32((v >> 11) & 0x7FF), uf10_to_f32(v >> 22), 1.0])
            }
            Rgb9E5Float => {
                let v = word();
                let e = (v >> 27) as i32 - 15 - 9;
                let scale = m::exp2(e as f32);
                Texel::Float([
                    (v & 511) as f32 * scale,
                    ((v >> 9) & 511) as f32 * scale,
                    ((v >> 18) & 511) as f32 * scale,
                    1.0,
                ])
            }
            D16Unorm => Texel::Depth(f32::from(half()) / 65535.0, 0),
            D24Unorm => Texel::Depth((word() >> 8) as f32 / 16_777_215.0, 0),
            D24UnormS8Uint => {
                let v = word();
                Texel::Depth((v >> 8) as f32 / 16_777_215.0, v as u8)
            }
            D32Float => Texel::Depth(f(0), 0),
            D32FloatS8Uint => Texel::Depth(f(0), b[4]),
            S8Uint => Texel::Depth(0.0, b[0]),
            L8Unorm => Texel::Float([u8n(0), u8n(0), u8n(0), 1.0]),
            A8Unorm => Texel::Float([0.0, 0.0, 0.0, u8n(0)]),
            L8A8Unorm => Texel::Float([u8n(0), u8n(0), u8n(0), u8n(1)]),
            L16Float => Texel::Float([h(0), h(0), h(0), 1.0]),
            A16Float => Texel::Float([0.0, 0.0, 0.0, h(0)]),
            L16A16Float => Texel::Float([h(0), h(0), h(0), h(1)]),
            L32Float => Texel::Float([f(0), f(0), f(0), 1.0]),
            A32Float => Texel::Float([0.0, 0.0, 0.0, f(0)]),
            L32A32Float => Texel::Float([f(0), f(0), f(0), f(1)]),
        }
    }

    /// Stores a texel: floats are clamped and rounded to normalised
    /// formats (GL's equations 2.3 and 2.4), converted to half or
    /// packed floats; integers are clamped to the component's range.
    /// Luminance formats take red as luminance.
    pub fn encode(self, t: &Texel, out: &mut [u8]) {
        use Format::*;
        let c = t.as_float();
        let ci = t.as_int();
        let cu = t.as_uint();
        let unorm = |x: f32, max: f32| -> u32 { (clamp01(x) * max + 0.5) as u32 };
        let snorm = |x: f32, max: f32| -> i32 {
            let v = x.clamp(-1.0, 1.0) * max;
            m::round(if v.is_nan() { 0.0 } else { v }) as i32
        };
        let put_u16 = |out: &mut [u8], i: usize, v: u16| out[2 * i..2 * i + 2].copy_from_slice(&v.to_le_bytes());
        let put_u32 = |out: &mut [u8], i: usize, v: u32| out[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        let put_f32 = |out: &mut [u8], i: usize, v: f32| out[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        let h = |x: f32| f32_to_f16(x);
        let su = |x: u32, max: u32| x.min(max);
        let si = |x: i32, min: i32, max: i32| x.clamp(min, max);
        match self {
            R8Unorm | L8Unorm => out[0] = unorm(c[0], 255.0) as u8,
            A8Unorm => out[0] = unorm(c[3], 255.0) as u8,
            L8A8Unorm => {
                out[0] = unorm(c[0], 255.0) as u8;
                out[1] = unorm(c[3], 255.0) as u8;
            }
            R8Snorm | Rg8Snorm | Rgbx8Snorm | Rgba8Snorm => {
                let n = self.bytes().min(4);
                let k = match self {
                    R8Snorm => 1,
                    Rg8Snorm => 2,
                    Rgbx8Snorm => 3,
                    _ => 4,
                };
                for i in 0..n {
                    out[i] = if i < k { snorm(c[i], 127.0) as i8 as u8 } else { 0 };
                }
            }
            Rg8Unorm | Rgbx8Unorm | Rgba8Unorm => {
                let k = match self {
                    Rg8Unorm => 2,
                    Rgbx8Unorm => 3,
                    _ => 4,
                };
                for i in 0..self.bytes() {
                    out[i] = if i < k { unorm(c[i], 255.0) as u8 } else { 255 };
                }
            }
            Rgbx8Srgb | Rgba8Srgb => {
                for i in 0..3 {
                    out[i] = linear_to_srgb(c[i]);
                }
                out[3] = if self == Rgba8Srgb { unorm(c[3], 255.0) as u8 } else { 255 };
            }
            R8Uint | Rg8Uint | Rgbx8Uint | Rgba8Uint => {
                for i in 0..self.bytes() {
                    out[i] = su(cu[i], 255) as u8;
                }
            }
            R8Sint | Rg8Sint | Rgbx8Sint | Rgba8Sint => {
                for i in 0..self.bytes() {
                    out[i] = si(ci[i], -128, 127) as i8 as u8;
                }
            }
            R16Float | Rg16Float | Rgbx16Float | Rgba16Float | L16Float | L16A16Float | A16Float => {
                let src: [f32; 4] = match self {
                    L16A16Float => [c[0], c[3], 0.0, 0.0],
                    A16Float => [c[3], 0.0, 0.0, 0.0],
                    _ => c,
                };
                for (i, &v) in src.iter().enumerate().take(self.bytes() / 2) {
                    put_u16(out, i, h(v));
                }
            }
            R16Uint | Rg16Uint | Rgbx16Uint | Rgba16Uint => {
                for (i, &v) in cu.iter().enumerate().take(self.bytes() / 2) {
                    put_u16(out, i, su(v, 65535) as u16);
                }
            }
            R16Sint | Rg16Sint | Rgbx16Sint | Rgba16Sint => {
                for (i, &v) in ci.iter().enumerate().take(self.bytes() / 2) {
                    put_u16(out, i, si(v, -32768, 32767) as i16 as u16);
                }
            }
            R32Float | Rg32Float | Rgbx32Float | Rgba32Float | L32Float | L32A32Float | A32Float => {
                let src: [f32; 4] = match self {
                    L32A32Float => [c[0], c[3], 0.0, 0.0],
                    A32Float => [c[3], 0.0, 0.0, 0.0],
                    _ => c,
                };
                for (i, &v) in src.iter().enumerate().take(self.bytes() / 4) {
                    put_f32(out, i, v);
                }
            }
            R32Uint | Rg32Uint | Rgbx32Uint | Rgba32Uint => {
                for (i, &v) in cu.iter().enumerate().take(self.bytes() / 4) {
                    put_u32(out, i, v);
                }
            }
            R32Sint | Rg32Sint | Rgbx32Sint | Rgba32Sint => {
                for (i, &v) in ci.iter().enumerate().take(self.bytes() / 4) {
                    put_u32(out, i, v as u32);
                }
            }
            B5G6R5Unorm => {
                let v = (unorm(c[0], 31.0) << 11) | (unorm(c[1], 63.0) << 5) | unorm(c[2], 31.0);
                put_u16(out, 0, v as u16);
            }
            Rgba4Unorm => {
                let v =
                    (unorm(c[0], 15.0) << 12) | (unorm(c[1], 15.0) << 8) | (unorm(c[2], 15.0) << 4) | unorm(c[3], 15.0);
                put_u16(out, 0, v as u16);
            }
            Rgb5A1Unorm => {
                let v =
                    (unorm(c[0], 31.0) << 11) | (unorm(c[1], 31.0) << 6) | (unorm(c[2], 31.0) << 1) | unorm(c[3], 1.0);
                put_u16(out, 0, v as u16);
            }
            Rgb10A2Unorm => {
                let v = unorm(c[0], 1023.0)
                    | (unorm(c[1], 1023.0) << 10)
                    | (unorm(c[2], 1023.0) << 20)
                    | (unorm(c[3], 3.0) << 30);
                put_u32(out, 0, v);
            }
            Rgb10A2Uint => {
                let v = su(cu[0], 1023) | (su(cu[1], 1023) << 10) | (su(cu[2], 1023) << 20) | (su(cu[3], 3) << 30);
                put_u32(out, 0, v);
            }
            R11G11B10Float => {
                let v = f32_to_uf11(c[0]) | (f32_to_uf11(c[1]) << 11) | (f32_to_uf10(c[2]) << 22);
                put_u32(out, 0, v);
            }
            Rgb9E5Float => put_u32(out, 0, f32_to_rgb9e5(c[0], c[1], c[2])),
            D16Unorm => put_u16(out, 0, unorm(t.depth(), 65535.0) as u16),
            D24Unorm => put_u32(out, 0, unorm(t.depth(), 16_777_215.0) << 8),
            D24UnormS8Uint => put_u32(out, 0, (unorm(t.depth(), 16_777_215.0) << 8) | u32::from(t.stencil())),
            D32Float => put_f32(out, 0, t.depth()),
            D32FloatS8Uint => {
                put_f32(out, 0, t.depth());
                put_u32(out, 1, u32::from(t.stencil()));
            }
            S8Uint => out[0] = t.stencil(),
        }
    }
}

fn clamp01(x: f32) -> f32 {
    if x > 0.0 {
        if x < 1.0 { x } else { 1.0 }
    } else {
        // Negative values and NaN.
        0.0
    }
}

/// A texel in a neutral form.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Texel {
    Float([f32; 4]),
    Int([i32; 4]),
    Uint([u32; 4]),
    /// Depth (as a float in [0, 1] for fixed-point formats) and stencil.
    Depth(f32, u8),
}

impl Texel {
    pub fn as_float(&self) -> [f32; 4] {
        match *self {
            Texel::Float(f) => f,
            Texel::Int(i) => i.map(|x| x as f32),
            Texel::Uint(u) => u.map(|x| x as f32),
            Texel::Depth(d, _) => [d, 0.0, 0.0, 1.0],
        }
    }

    pub fn as_int(&self) -> [i32; 4] {
        match *self {
            Texel::Int(i) => i,
            Texel::Uint(u) => u.map(|x| x as i32),
            Texel::Float(f) => f.map(|x| x as i32),
            Texel::Depth(_, s) => [i32::from(s), 0, 0, 1],
        }
    }

    pub fn as_uint(&self) -> [u32; 4] {
        match *self {
            Texel::Uint(u) => u,
            Texel::Int(i) => i.map(|x| x as u32),
            Texel::Float(f) => f.map(|x| x as u32),
            Texel::Depth(_, s) => [u32::from(s), 0, 0, 1],
        }
    }

    pub fn depth(&self) -> f32 {
        match *self {
            Texel::Depth(d, _) => d,
            Texel::Float(f) => f[0],
            _ => 0.0,
        }
    }

    pub fn stencil(&self) -> u8 {
        match *self {
            Texel::Depth(_, s) => s,
            Texel::Uint(u) => u[0] as u8,
            Texel::Int(i) => i[0] as u8,
            Texel::Float(_) => 0,
        }
    }
}

/// Where the texels of an image are in memory: `width` x `height` x
/// `depth` (slices or layers), rows `row` bytes apart and images `image`
/// bytes apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub row: usize,
    pub image: usize,
}

impl Geometry {
    /// Tightly packed texels of `bytes` each.
    pub fn packed(width: u32, height: u32, depth: u32, bytes: usize) -> Geometry {
        let row = width as usize * bytes;
        Geometry { width, height, depth, row, image: row * height as usize }
    }

    fn at(&self, x: u32, y: u32, z: u32, bytes: usize) -> usize {
        z as usize * self.image + y as usize * self.row + x as usize * bytes
    }
}

/// Fills a mipmap level from the one above it: each texel is the average
/// of the 2x2 texels it covers (2x2x2 in a volume, whose slices halve
/// too; layers and faces do not), or of one along a dimension that did not
/// shrink. Colors are averaged in linear space (sRGB ones decoded first).
pub fn downsample(f: Format, src: &[u8], s: Geometry, dst: &mut [u8], d: Geometry, volume: bool) {
    let tb = f.bytes();
    for z in 0..d.depth {
        let zs = if volume && s.depth > 1 { [2 * z, (2 * z + 1).min(s.depth - 1)] } else { [z, z] };
        for y in 0..d.height {
            let ys = if s.height > 1 { [2 * y, (2 * y + 1).min(s.height - 1)] } else { [0, 0] };
            for x in 0..d.width {
                let xs = if s.width > 1 { [2 * x, (2 * x + 1).min(s.width - 1)] } else { [0, 0] };
                let mut sum = [0.0f32; 4];
                for &k in &zs {
                    for &j in &ys {
                        for &i in &xs {
                            let o = s.at(i, j, k, tb);
                            let v = f.decode(&src[o..o + tb]).as_float();
                            for c in 0..4 {
                                sum[c] += v[c];
                            }
                        }
                    }
                }
                let o = d.at(x, y, z, tb);
                f.encode(&Texel::Float(sum.map(|v| v / 8.0)), &mut dst[o..o + tb]);
            }
        }
    }
}

/// sRGB-encoded bytes as linear floats: the bits of the exact values of the
/// sRGB curve (section 3.8.16), rounded to the nearest float.
#[rustfmt::skip]
static SRGB_TO_LINEAR: [u32; 256] = [
    0x00000000, 0x399F22B4, 0x3A1F22B4, 0x3A6EB40E, 0x3A9F22B4, 0x3AC6EB61, 0x3AEEB40E, 0x3B0B3E5D,
    0x3B1F22B4, 0x3B33070A, 0x3B46EB61, 0x3B5B518E, 0x3B70F18F, 0x3B83E1C6, 0x3B8FE616, 0x3B9C87FD,
    0x3BA9C9B6, 0x3BB7AD6F, 0x3BC6354A, 0x3BD56360, 0x3BE539C1, 0x3BF5BA71, 0x3C0373B6, 0x3C0C6153,
    0x3C15A705, 0x3C1F45BE, 0x3C293E6B, 0x3C3391F7, 0x3C3E4149, 0x3C494D44, 0x3C54B6C9, 0x3C607EB4,
    0x3C6CA5DF, 0x3C792D22, 0x3C830AA9, 0x3C89AF9F, 0x3C9085DC, 0x3C978DC6, 0x3C9EC7C2, 0x3CA63433,
    0x3CADD37D, 0x3CB5A602, 0x3CBDAC21, 0x3CC5E63A, 0x3CCE54AC, 0x3CD6F7D5, 0x3CDFD010, 0x3CE8DDBA,
    0x3CF2212D, 0x3CFB9AC3, 0x3D02A56A, 0x3D0798DD, 0x3D0CA7E6, 0x3D11D2AF, 0x3D171964, 0x3D1C7C30,
    0x3D21FB3C, 0x3D2796B2, 0x3D2D4EBB, 0x3D332381, 0x3D39152B, 0x3D3F23E4, 0x3D454FD2, 0x3D4B991D,
    0x3D51FFEC, 0x3D588468, 0x3D5F26B6, 0x3D65E6FD, 0x3D6CC563, 0x3D73C20E, 0x3D7ADD24, 0x3D810B65,
    0x3D84B793, 0x3D88732E, 0x3D8C3E48, 0x3D9018F4, 0x3D940344, 0x3D97FD49, 0x3D9C0715, 0x3DA020BA,
    0x3DA44A4A, 0x3DA883D6, 0x3DACCD6F, 0x3DB12727, 0x3DB5910F, 0x3DBA0B38, 0x3DBE95B3, 0x3DC33090,
    0x3DC7DBE0, 0x3DCC97B4, 0x3DD1641D, 0x3DD6412B, 0x3DDB2EEE, 0x3DE02D76, 0x3DE53CD4, 0x3DEA5D18,
    0x3DEF8E51, 0x3DF4D090, 0x3DFA23E5, 0x3DFF885E, 0x3E027F06, 0x3E05427F, 0x3E080EA2, 0x3E0AE377,
    0x3E0DC104, 0x3E10A753, 0x3E13966A, 0x3E168E51, 0x3E198F0F, 0x3E1C98AC, 0x3E1FAB30, 0x3E22C6A1,
    0x3E25EB07, 0x3E29186A, 0x3E2C4ED0, 0x3E2F8E42, 0x3E32D6C5, 0x3E362862, 0x3E39831F, 0x3E3CE703,
    0x3E405417, 0x3E43CA60, 0x3E4749E6, 0x3E4AD2AF, 0x3E4E64C3, 0x3E520029, 0x3E55A4E7, 0x3E595305,
    0x3E5D0A89, 0x3E60CB7A, 0x3E6495DF, 0x3E6869BE, 0x3E6C471F, 0x3E702E07, 0x3E741E7E, 0x3E78188B,
    0x3E7C1C33, 0x3E8014BF, 0x3E822039, 0x3E84308B, 0x3E8645B8, 0x3E885FC3, 0x3E8A7EB0, 0x3E8CA281,
    0x3E8ECB3B, 0x3E90F8DF, 0x3E932B72, 0x3E9562F6, 0x3E979F6F, 0x3E99E0E0, 0x3E9C274C, 0x3E9E72B6,
    0x3EA0C321, 0x3EA31890, 0x3EA57307, 0x3EA7D288, 0x3EAA3716, 0x3EACA0B6, 0x3EAF0F68, 0x3EB18332,
    0x3EB3FC15, 0x3EB67A14, 0x3EB8FD34, 0x3EBB8576, 0x3EBE12DE, 0x3EC0A56E, 0x3EC33D2A, 0x3EC5DA14,
    0x3EC87C30, 0x3ECB2380, 0x3ECDD008, 0x3ED081CA, 0x3ED338C9, 0x3ED5F508, 0x3ED8B68A, 0x3EDB7D52,
    0x3EDE4963, 0x3EE11ABF, 0x3EE3F169, 0x3EE6CD65, 0x3EE9AEB5, 0x3EEC955B, 0x3EEF815C, 0x3EF272B8,
    0x3EF56974, 0x3EF86593, 0x3EFB6716, 0x3EFE6E00, 0x3F00BD2B, 0x3F02460C, 0x3F03D1A5, 0x3F055FF7,
    0x3F06F104, 0x3F0884CD, 0x3F0A1B54, 0x3F0BB499, 0x3F0D509F, 0x3F0EEF65, 0x3F1090EF, 0x3F12353D,
    0x3F13DC50, 0x3F15862A, 0x3F1732CC, 0x3F18E237, 0x3F1A946E, 0x3F1C4970, 0x3F1E0140, 0x3F1FBBDE,
    0x3F21794D, 0x3F23398C, 0x3F24FC9F, 0x3F26C285, 0x3F288B41, 0x3F2A56D2, 0x3F2C253C, 0x3F2DF67F,
    0x3F2FCA9C, 0x3F31A194, 0x3F337B6A, 0x3F35581D, 0x3F3737B0, 0x3F391A24, 0x3F3AFF7A, 0x3F3CE7B2,
    0x3F3ED2CF, 0x3F40C0D2, 0x3F42B1BC, 0x3F44A58E, 0x3F469C49, 0x3F4895EF, 0x3F4A9280, 0x3F4C91FF,
    0x3F4E946C, 0x3F5099C9, 0x3F52A216, 0x3F54AD56, 0x3F56BB88, 0x3F58CCAF, 0x3F5AE0CC, 0x3F5CF7DF,
    0x3F5F11EA, 0x3F612EEF, 0x3F634EEE, 0x3F6571E9, 0x3F6797E0, 0x3F69C0D5, 0x3F6BECCA, 0x3F6E1BBF,
    0x3F704DB5, 0x3F7282AE, 0x3F74BAAB, 0x3F76F5AE, 0x3F7933B6, 0x3F7B74C6, 0x3F7DB8DE, 0x3F800000,
];

/// Where linear values start to encode as each sRGB byte above 0: entry
/// `c` is the smallest float whose exact encoding rounds to `c + 1`.
#[rustfmt::skip]
static LINEAR_TO_SRGB: [u32; 255] = [
    0x391F22B4, 0x39EEB40E, 0x3A46EB61, 0x3A8B3E5E, 0x3AB3070B, 0x3ADACFB8, 0x3B014C33, 0x3B153089,
    0x3B2914DF, 0x3B3CF936, 0x3B50F2D1, 0x3B65FB9B, 0x3B7C3403, 0x3B89D060, 0x3B962333, 0x3BA314BD,
    0x3BB0A731, 0x3BBEDCB7, 0x3BCDB76D, 0x3BDD3967, 0x3BED64AF, 0x3BFE3B46, 0x3C07DF91, 0x3C10F91B,
    0x3C1A6B32, 0x3C2436C8, 0x3C2E5CC7, 0x3C38DE1A, 0x3C43BBA4, 0x3C4EF648, 0x3C5A8EE4, 0x3C668654,
    0x3C72DD71, 0x3C7F950F, 0x3C865702, 0x3C8D148F, 0x3C940396, 0x3C9B247C, 0x3CA277A6, 0x3CA9FD78,
    0x3CB1B653, 0x3CB9A298, 0x3CC1C2A9, 0x3CCA16E3, 0x3CD29FA4, 0x3CDB5D4B, 0x3CE45032, 0x3CED78B5,
    0x3CF6D72E, 0x3D0035FC, 0x3D051BB4, 0x3D0A1CEC, 0x3D0F39D0, 0x3D14728A, 0x3D19C745, 0x3D1F382C,
    0x3D24C567, 0x3D2A6F22, 0x3D303584, 0x3D3618B7, 0x3D3C18E4, 0x3D423632, 0x3D4870CA, 0x3D4EC8D2,
    0x3D553E73, 0x3D5BD1D3, 0x3D628318, 0x3D69526A, 0x3D703FEE, 0x3D774BCA, 0x3D7E7624, 0x3D82DF90,
    0x3D869372, 0x3D8A56CB, 0x3D8E29AB, 0x3D920C27, 0x3D95FE4F, 0x3D9A0035, 0x3D9E11EC, 0x3DA23384,
    0x3DA66510, 0x3DAAA6A0, 0x3DAEF847, 0x3DB35A15, 0x3DB7CC1B, 0x3DBC4E6B, 0x3DC0E114, 0x3DC58429,
    0x3DCA37B9, 0x3DCEFBD6, 0x3DD3D08F, 0x3DD8B5F5, 0x3DDDAC19, 0x3DE2B30A, 0x3DE7CAD9, 0x3DECF395,
    0x3DF22D50, 0x3DF77817, 0x3DFCD3FC, 0x3E012087, 0x3E03DFAE, 0x3E06A77B, 0x3E0977F6, 0x3E0C5126,
    0x3E0F3314, 0x3E121DC5, 0x3E151143, 0x3E180D95, 0x3E1B12C2, 0x3E1E20D1, 0x3E2137CB, 0x3E2457B6,
    0x3E278099, 0x3E2AB27D, 0x3E2DED68, 0x3E313161, 0x3E347E70, 0x3E37D49C, 0x3E3B33EC, 0x3E3E9C67,
    0x3E420E15, 0x3E4588FB, 0x3E490D22, 0x3E4C9A90, 0x3E50314C, 0x3E53D15D, 0x3E577ACA, 0x3E5B2D9A,
    0x3E5EE9D4, 0x3E62AF7E, 0x3E667E9F, 0x3E6A573E, 0x3E6E3962, 0x3E722511, 0x3E761A52, 0x3E7A192C,
    0x3E7E21A5, 0x3E8119E2, 0x3E8327C7, 0x3E853A86, 0x3E875222, 0x3E896E9D, 0x3E8B8FFC, 0x3E8DB641,
    0x3E8FE170, 0x3E92118B, 0x3E944696, 0x3E968095, 0x3E98BF89, 0x3E9B0377, 0x3E9D4C62, 0x3E9F9A4C,
    0x3EA1ED38, 0x3EA4452B, 0x3EA6A226, 0x3EA9042E, 0x3EAB6B44, 0x3EADD76D, 0x3EB048AA, 0x3EB2BF00,
    0x3EB53A71, 0x3EB7BB00, 0x3EBA40B1, 0x3EBCCB85, 0x3EBF5B81, 0x3EC1F0A7, 0x3EC48AF9, 0x3EC72A7C,
    0x3EC9CF32, 0x3ECC791E, 0x3ECF2842, 0x3ED1DCA2, 0x3ED49641, 0x3ED75521, 0x3EDA1946, 0x3EDCE2B2,
    0x3EDFB168, 0x3EE2856A, 0x3EE55EBD, 0x3EE83D63, 0x3EEB215D, 0x3EEE0AB1, 0x3EF0F95F, 0x3EF3ED6B,
    0x3EF6E6D8, 0x3EF9E5A8, 0x3EFCE9DE, 0x3EFFF37E, 0x3F018145, 0x3F030B82, 0x3F049877, 0x3F062827,
    0x3F07BA92, 0x3F094FB9, 0x3F0AE79F, 0x3F0C8244, 0x3F0E1FAA, 0x3F0FBFD2, 0x3F1162BE, 0x3F13086E,
    0x3F14B0E4, 0x3F165C22, 0x3F180A29, 0x3F19BAFA, 0x3F1B6E96, 0x3F1D24FF, 0x3F1EDE36, 0x3F209A3C,
    0x3F225913, 0x3F241ABC, 0x3F25DF38, 0x3F27A689, 0x3F2970AF, 0x3F2B3DAD, 0x3F2D0D83, 0x3F2EE032,
    0x3F30B5BD, 0x3F328E24, 0x3F346968, 0x3F36478B, 0x3F38288F, 0x3F3A0C73, 0x3F3BF33A, 0x3F3DDCE5,
    0x3F3FC975, 0x3F41B8EB, 0x3F43AB48, 0x3F45A08F, 0x3F4798BF, 0x3F4993DB, 0x3F4B91E3, 0x3F4D92D8,
    0x3F4F96BD, 0x3F519D92, 0x3F53A758, 0x3F55B411, 0x3F57C3BE, 0x3F59D65F, 0x3F5BEBF7, 0x3F5E0486,
    0x3F60200E, 0x3F623E90, 0x3F64600C, 0x3F668485, 0x3F68ABFB, 0x3F6AD670, 0x3F6D03E5, 0x3F6F345A,
    0x3F7167D2, 0x3F739E4D, 0x3F75D7CC, 0x3F781451, 0x3F7A53DD, 0x3F7C9671, 0x3F7EDC0E,
];

/// sRGB-encoded 8 bits to linear.
#[inline(always)]
pub fn srgb_to_linear(c: u8) -> f32 {
    f32::from_bits(SRGB_TO_LINEAR[c as usize])
}

/// Linear to sRGB-encoded 8 bits, rounded to nearest (exactly, by a search
/// of the rounding boundaries). NaN encodes as 0.
#[inline]
pub fn linear_to_srgb(x: f32) -> u8 {
    if x.is_nan() || x <= 0.0 {
        return 0;
    }
    LINEAR_TO_SRGB.partition_point(|&t| f32::from_bits(t) <= x) as u8
}

/// An unsigned 11-bit float (5-bit exponent, 6-bit mantissa) to f32.
pub fn uf11_to_f32(v: u32) -> f32 {
    small_float(v, 6)
}

/// An unsigned 10-bit float (5-bit exponent, 5-bit mantissa) to f32.
pub fn uf10_to_f32(v: u32) -> f32 {
    small_float(v, 5)
}

fn small_float(v: u32, mbits: u32) -> f32 {
    let e = (v >> mbits) & 31;
    let mant = v & ((1 << mbits) - 1);
    let mf = mant as f32 / (1u32 << mbits) as f32;
    match e {
        0 => mf * m::exp2(-14.0),
        31 => {
            if mant == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + mf) * m::exp2(e as f32 - 15.0),
    }
}

/// f32 to an unsigned 11-bit float (negative values become 0).
pub fn f32_to_uf11(x: f32) -> u32 {
    to_small_float(x, 6)
}

/// f32 to an unsigned 10-bit float.
pub fn f32_to_uf10(x: f32) -> u32 {
    to_small_float(x, 5)
}

fn to_small_float(x: f32, mbits: u32) -> u32 {
    if x.is_nan() {
        return (31 << mbits) | 1;
    }
    if x.partial_cmp(&0.0) != Some(core::cmp::Ordering::Greater) {
        return 0;
    }
    if x.is_infinite() {
        return 31 << mbits;
    }
    // Rounded once, to nearest even, from the float's own bits.
    let max_finite = (30 << mbits) | ((1 << mbits) - 1);
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127;
    if exp > 15 {
        return max_finite;
    }
    let e = exp + 15;
    let mant = (bits & 0x7F_FFFF) | 0x80_0000;
    let shift = 23 - mbits as i32 + if e >= 1 { 0 } else { 1 - e };
    if shift > 24 {
        return 0;
    }
    let shift = shift as u32;
    let half = 1u32 << (shift - 1);
    let rem = mant & ((1 << shift) - 1);
    let mut q = mant >> shift;
    if rem > half || (rem == half && q & 1 == 1) {
        q += 1;
    }
    if e < 1 {
        // Subnormal; rounding up to 2^mbits reads as the smallest normal.
        return q;
    }
    let mut e = e as u32;
    if q >> (mbits + 1) != 0 {
        q >>= 1;
        e += 1;
    }
    if e >= 31 {
        return max_finite;
    }
    (e << mbits) | (q & ((1 << mbits) - 1))
}

/// Packs a color into RGB9_E5 (GL's shared-exponent rules).
pub fn f32_to_rgb9e5(r: f32, g: f32, b: f32) -> u32 {
    const MAX: f32 = 65408.0; // (2^9 - 1) / 2^9 * 2^16
    let c = |x: f32| if x.is_nan() || x <= 0.0 { 0.0 } else { x.min(MAX) };
    let (r, g, b) = (c(r), c(g), c(b));
    let max = r.max(g).max(b);
    if max == 0.0 {
        return 0;
    }
    // floor(log2(max)) from the float's exponent (exact; subnormals
    // fall under the -16 floor).
    let fl = ((max.to_bits() >> 23) & 0xFF) as i32 - 127;
    let mut e = fl.max(-16) + 1 + 15;
    let scale = |e: i32| m::exp2((e - 15 - 9) as f32);
    let maxs = m::floor(max / scale(e) + 0.5) as i32;
    if maxs == 512 {
        e += 1;
    }
    let s = scale(e);
    let q = |x: f32| (m::floor(x / s + 0.5) as u32).min(511);
    q(r) | (q(g) << 9) | (q(b) << 18) | ((e as u32 & 31) << 27)
}

// ---- Internal formats --------------------------------------------------------

/// How a sized internal format's components read in queries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ComponentType {
    UnsignedNormalized,
    SignedNormalized,
    Float,
    Int,
    UnsignedInt,
}

impl ComponentType {
    /// The `GL_*` value `GetFramebufferAttachmentParameteriv` reports.
    pub fn gl(self) -> u32 {
        match self {
            ComponentType::UnsignedNormalized => gl::UNSIGNED_NORMALIZED,
            ComponentType::SignedNormalized => gl::SIGNED_NORMALIZED,
            ComponentType::Float => gl::FLOAT,
            ComponentType::Int => gl::INT,
            ComponentType::UnsignedInt => gl::UNSIGNED_INT,
        }
    }
}

/// A sized internal format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Internal {
    /// The `GL_*` sized internal format.
    pub gl: u32,
    pub format: Format,
    /// The base internal format (`GL_RGBA`, `GL_DEPTH_COMPONENT`...).
    pub base: u32,
    /// Red, green, blue, alpha, depth and stencil bits.
    pub bits: [u8; 6],
    pub component: ComponentType,
    /// Color-renderable (core OpenGL ES 3.0).
    pub renderable: bool,
    /// Color-renderable with `EXT_color_buffer_float`.
    pub float_renderable: bool,
    /// Texture-filterable (32-bit floats need `OES_texture_float_linear`).
    pub filterable: bool,
}

const fn fmt(
    gl: u32,
    format: Format,
    base: u32,
    bits: [u8; 6],
    component: ComponentType,
    renderable: bool,
    filterable: bool,
) -> Internal {
    Internal { gl, format, base, bits, component, renderable, float_renderable: false, filterable }
}

const fn ffmt(gl: u32, format: Format, base: u32, bits: [u8; 6], filterable: bool) -> Internal {
    Internal {
        gl,
        format,
        base,
        bits,
        component: ComponentType::Float,
        renderable: false,
        float_renderable: true,
        filterable,
    }
}

use ComponentType as C;
use Format as F;

/// OpenGL ES 3.0's sized internal formats (tables 3.13 and 3.14).
pub const SIZED: &[Internal] = &[
    fmt(gl::R8, F::R8Unorm, gl::RED, [8, 0, 0, 0, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::R8_SNORM, F::R8Snorm, gl::RED, [8, 0, 0, 0, 0, 0], C::SignedNormalized, false, true),
    fmt(gl::RG8, F::Rg8Unorm, gl::RG, [8, 8, 0, 0, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RG8_SNORM, F::Rg8Snorm, gl::RG, [8, 8, 0, 0, 0, 0], C::SignedNormalized, false, true),
    fmt(gl::RGB8, F::Rgbx8Unorm, gl::RGB, [8, 8, 8, 0, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RGB8_SNORM, F::Rgbx8Snorm, gl::RGB, [8, 8, 8, 0, 0, 0], C::SignedNormalized, false, true),
    fmt(gl::RGB565, F::B5G6R5Unorm, gl::RGB, [5, 6, 5, 0, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RGBA4, F::Rgba4Unorm, gl::RGBA, [4, 4, 4, 4, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RGB5_A1, F::Rgb5A1Unorm, gl::RGBA, [5, 5, 5, 1, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RGBA8, F::Rgba8Unorm, gl::RGBA, [8, 8, 8, 8, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RGBA8_SNORM, F::Rgba8Snorm, gl::RGBA, [8, 8, 8, 8, 0, 0], C::SignedNormalized, false, true),
    fmt(gl::RGB10_A2, F::Rgb10A2Unorm, gl::RGBA, [10, 10, 10, 2, 0, 0], C::UnsignedNormalized, true, true),
    fmt(gl::RGB10_A2UI, F::Rgb10A2Uint, gl::RGBA, [10, 10, 10, 2, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::SRGB8, F::Rgbx8Srgb, gl::RGB, [8, 8, 8, 0, 0, 0], C::UnsignedNormalized, false, true),
    fmt(gl::SRGB8_ALPHA8, F::Rgba8Srgb, gl::RGBA, [8, 8, 8, 8, 0, 0], C::UnsignedNormalized, true, true),
    ffmt(gl::R16F, F::R16Float, gl::RED, [16, 0, 0, 0, 0, 0], true),
    ffmt(gl::RG16F, F::Rg16Float, gl::RG, [16, 16, 0, 0, 0, 0], true),
    fmt(gl::RGB16F, F::Rgbx16Float, gl::RGB, [16, 16, 16, 0, 0, 0], C::Float, false, true),
    ffmt(gl::RGBA16F, F::Rgba16Float, gl::RGBA, [16, 16, 16, 16, 0, 0], true),
    ffmt(gl::R32F, F::R32Float, gl::RED, [32, 0, 0, 0, 0, 0], false),
    ffmt(gl::RG32F, F::Rg32Float, gl::RG, [32, 32, 0, 0, 0, 0], false),
    fmt(gl::RGB32F, F::Rgbx32Float, gl::RGB, [32, 32, 32, 0, 0, 0], C::Float, false, false),
    ffmt(gl::RGBA32F, F::Rgba32Float, gl::RGBA, [32, 32, 32, 32, 0, 0], false),
    ffmt(gl::R11F_G11F_B10F, F::R11G11B10Float, gl::RGB, [11, 11, 10, 0, 0, 0], true),
    fmt(gl::RGB9_E5, F::Rgb9E5Float, gl::RGB, [9, 9, 9, 0, 0, 0], C::Float, false, true),
    fmt(gl::R8I, F::R8Sint, gl::RED, [8, 0, 0, 0, 0, 0], C::Int, true, false),
    fmt(gl::R8UI, F::R8Uint, gl::RED, [8, 0, 0, 0, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::R16I, F::R16Sint, gl::RED, [16, 0, 0, 0, 0, 0], C::Int, true, false),
    fmt(gl::R16UI, F::R16Uint, gl::RED, [16, 0, 0, 0, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::R32I, F::R32Sint, gl::RED, [32, 0, 0, 0, 0, 0], C::Int, true, false),
    fmt(gl::R32UI, F::R32Uint, gl::RED, [32, 0, 0, 0, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::RG8I, F::Rg8Sint, gl::RG, [8, 8, 0, 0, 0, 0], C::Int, true, false),
    fmt(gl::RG8UI, F::Rg8Uint, gl::RG, [8, 8, 0, 0, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::RG16I, F::Rg16Sint, gl::RG, [16, 16, 0, 0, 0, 0], C::Int, true, false),
    fmt(gl::RG16UI, F::Rg16Uint, gl::RG, [16, 16, 0, 0, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::RG32I, F::Rg32Sint, gl::RG, [32, 32, 0, 0, 0, 0], C::Int, true, false),
    fmt(gl::RG32UI, F::Rg32Uint, gl::RG, [32, 32, 0, 0, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::RGB8I, F::Rgbx8Sint, gl::RGB, [8, 8, 8, 0, 0, 0], C::Int, false, false),
    fmt(gl::RGB8UI, F::Rgbx8Uint, gl::RGB, [8, 8, 8, 0, 0, 0], C::UnsignedInt, false, false),
    fmt(gl::RGB16I, F::Rgbx16Sint, gl::RGB, [16, 16, 16, 0, 0, 0], C::Int, false, false),
    fmt(gl::RGB16UI, F::Rgbx16Uint, gl::RGB, [16, 16, 16, 0, 0, 0], C::UnsignedInt, false, false),
    fmt(gl::RGB32I, F::Rgbx32Sint, gl::RGB, [32, 32, 32, 0, 0, 0], C::Int, false, false),
    fmt(gl::RGB32UI, F::Rgbx32Uint, gl::RGB, [32, 32, 32, 0, 0, 0], C::UnsignedInt, false, false),
    fmt(gl::RGBA8I, F::Rgba8Sint, gl::RGBA, [8, 8, 8, 8, 0, 0], C::Int, true, false),
    fmt(gl::RGBA8UI, F::Rgba8Uint, gl::RGBA, [8, 8, 8, 8, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::RGBA16I, F::Rgba16Sint, gl::RGBA, [16, 16, 16, 16, 0, 0], C::Int, true, false),
    fmt(gl::RGBA16UI, F::Rgba16Uint, gl::RGBA, [16, 16, 16, 16, 0, 0], C::UnsignedInt, true, false),
    fmt(gl::RGBA32I, F::Rgba32Sint, gl::RGBA, [32, 32, 32, 32, 0, 0], C::Int, true, false),
    fmt(gl::RGBA32UI, F::Rgba32Uint, gl::RGBA, [32, 32, 32, 32, 0, 0], C::UnsignedInt, true, false),
    fmt(
        gl::DEPTH_COMPONENT16,
        F::D16Unorm,
        gl::DEPTH_COMPONENT,
        [0, 0, 0, 0, 16, 0],
        C::UnsignedNormalized,
        false,
        false,
    ),
    fmt(
        gl::DEPTH_COMPONENT24,
        F::D24Unorm,
        gl::DEPTH_COMPONENT,
        [0, 0, 0, 0, 24, 0],
        C::UnsignedNormalized,
        false,
        false,
    ),
    fmt(gl::DEPTH_COMPONENT32F, F::D32Float, gl::DEPTH_COMPONENT, [0, 0, 0, 0, 32, 0], C::Float, false, false),
    fmt(
        gl::DEPTH24_STENCIL8,
        F::D24UnormS8Uint,
        gl::DEPTH_STENCIL,
        [0, 0, 0, 0, 24, 8],
        C::UnsignedNormalized,
        false,
        false,
    ),
    fmt(gl::DEPTH32F_STENCIL8, F::D32FloatS8Uint, gl::DEPTH_STENCIL, [0, 0, 0, 0, 32, 8], C::Float, false, false),
    fmt(gl::STENCIL_INDEX8, F::S8Uint, gl::STENCIL_INDEX8, [0, 0, 0, 0, 0, 8], C::UnsignedInt, false, false),
];

/// OpenGL ES 2.0's luminance and alpha formats as internal formats (their
/// "sized" names are the unsized ones; they are filterable, not
/// renderable).
const LEGACY: &[Internal] = &[
    fmt(gl::LUMINANCE, F::L8Unorm, gl::LUMINANCE, [0, 0, 0, 0, 0, 0], C::UnsignedNormalized, false, true),
    fmt(gl::ALPHA, F::A8Unorm, gl::ALPHA, [0, 0, 0, 8, 0, 0], C::UnsignedNormalized, false, true),
    fmt(gl::LUMINANCE_ALPHA, F::L8A8Unorm, gl::LUMINANCE_ALPHA, [0, 0, 0, 8, 0, 0], C::UnsignedNormalized, false, true),
];

/// Luminance/alpha formats with half-float and float data
/// (`OES_texture_half_float`, `OES_texture_float`).
const LEGACY_FLOAT: &[(u32, u32, Format)] = &[
    (gl::LUMINANCE, gl::HALF_FLOAT, F::L16Float),
    (gl::ALPHA, gl::HALF_FLOAT, F::A16Float),
    (gl::LUMINANCE_ALPHA, gl::HALF_FLOAT, F::L16A16Float),
    (gl::LUMINANCE, gl::FLOAT, F::L32Float),
    (gl::ALPHA, gl::FLOAT, F::A32Float),
    (gl::LUMINANCE_ALPHA, gl::FLOAT, F::L32A32Float),
];

/// The sized internal format named `gl`.
pub fn sized(internalformat: u32) -> Option<&'static Internal> {
    SIZED.iter().find(|f| f.gl == internalformat)
}

/// The internal format description of a storage format (for queries).
pub fn of_format(f: Format) -> Option<&'static Internal> {
    SIZED.iter().chain(LEGACY).find(|i| i.format == f)
}

/// Why a texture format combination is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatError {
    /// An unknown token: `GL_INVALID_ENUM`.
    Enum,
    /// Known tokens that do not combine: `GL_INVALID_OPERATION`.
    Operation,
    /// A bad value (`GL_INVALID_VALUE`, e.g. an internal format that is
    /// not a format at all in `TexImage`).
    Value,
}

/// The internal format `TexImage*` creates for `(internalformat, format,
/// type)`, checked against tables 3.2 and 3.3 (and OpenGL ES 2.0's float
/// texture extensions for unsized formats).
pub fn tex_format(internalformat: u32, format: u32, ty: u32) -> Result<Internal, FormatError> {
    if !is_type(ty) {
        return Err(FormatError::Enum);
    }
    if !is_format(format) {
        return Err(FormatError::Enum);
    }
    // Unsized internal formats (table 3.3) plus extensions.
    let from_unsized = match (internalformat, format, ty) {
        (gl::RGBA, gl::RGBA, gl::UNSIGNED_BYTE) => Some(gl::RGBA8),
        (gl::RGBA, gl::RGBA, gl::UNSIGNED_SHORT_4_4_4_4) => Some(gl::RGBA4),
        (gl::RGBA, gl::RGBA, gl::UNSIGNED_SHORT_5_5_5_1) => Some(gl::RGB5_A1),
        (gl::RGB, gl::RGB, gl::UNSIGNED_BYTE) => Some(gl::RGB8),
        (gl::RGB, gl::RGB, gl::UNSIGNED_SHORT_5_6_5) => Some(gl::RGB565),
        (gl::RGBA, gl::RGBA, gl::HALF_FLOAT | HALF_FLOAT_OES) => Some(gl::RGBA16F),
        (gl::RGBA, gl::RGBA, gl::FLOAT) => Some(gl::RGBA32F),
        (gl::RGB, gl::RGB, gl::HALF_FLOAT | HALF_FLOAT_OES) => Some(gl::RGB16F),
        (gl::RGB, gl::RGB, gl::FLOAT) => Some(gl::RGB32F),
        (gl::DEPTH_COMPONENT, gl::DEPTH_COMPONENT, gl::UNSIGNED_SHORT) => Some(gl::DEPTH_COMPONENT16),
        (gl::DEPTH_COMPONENT, gl::DEPTH_COMPONENT, gl::UNSIGNED_INT) => Some(gl::DEPTH_COMPONENT24),
        (gl::DEPTH_STENCIL, gl::DEPTH_STENCIL, gl::UNSIGNED_INT_24_8) => Some(gl::DEPTH24_STENCIL8),
        _ => None,
    };
    if let Some(s) = from_unsized {
        return sized(s).copied().ok_or(FormatError::Operation);
    }
    let legacy = |i: u32| LEGACY.iter().find(|x| x.gl == i).copied();
    match (internalformat, format, ty) {
        (gl::LUMINANCE | gl::ALPHA | gl::LUMINANCE_ALPHA, f, gl::UNSIGNED_BYTE) if f == internalformat => {
            return legacy(internalformat).ok_or(FormatError::Operation);
        }
        (gl::LUMINANCE | gl::ALPHA | gl::LUMINANCE_ALPHA, f, t) if f == internalformat => {
            let t = if t == HALF_FLOAT_OES { gl::HALF_FLOAT } else { t };
            return match LEGACY_FLOAT.iter().find(|x| x.0 == f && x.1 == t) {
                Some(&(_, _, format)) => {
                    let mut i = legacy(internalformat).ok_or(FormatError::Operation)?;
                    i.format = format;
                    i.component = ComponentType::Float;
                    Ok(i)
                }
                None => Err(FormatError::Operation),
            };
        }
        _ => {}
    }
    let Some(info) = sized(internalformat) else {
        return Err(if is_format(internalformat) { FormatError::Operation } else { FormatError::Value });
    };
    if internalformat == gl::STENCIL_INDEX8 {
        return Err(FormatError::Operation);
    }
    use gl::*;
    let ok = match (format, ty) {
        (RGBA, UNSIGNED_BYTE) => matches!(internalformat, RGBA8 | RGB5_A1 | RGBA4 | SRGB8_ALPHA8),
        (RGBA, BYTE) => internalformat == RGBA8_SNORM,
        (RGBA, UNSIGNED_SHORT_4_4_4_4) => internalformat == RGBA4,
        (RGBA, UNSIGNED_SHORT_5_5_5_1) => internalformat == RGB5_A1,
        (RGBA, UNSIGNED_INT_2_10_10_10_REV) => matches!(internalformat, RGB10_A2 | RGB5_A1),
        (RGBA, HALF_FLOAT) => internalformat == RGBA16F,
        (RGBA, FLOAT) => matches!(internalformat, RGBA32F | RGBA16F),
        (RGBA_INTEGER, UNSIGNED_BYTE) => internalformat == RGBA8UI,
        (RGBA_INTEGER, BYTE) => internalformat == RGBA8I,
        (RGBA_INTEGER, UNSIGNED_SHORT) => internalformat == RGBA16UI,
        (RGBA_INTEGER, SHORT) => internalformat == RGBA16I,
        (RGBA_INTEGER, UNSIGNED_INT) => internalformat == RGBA32UI,
        (RGBA_INTEGER, INT) => internalformat == RGBA32I,
        (RGBA_INTEGER, UNSIGNED_INT_2_10_10_10_REV) => internalformat == RGB10_A2UI,
        (RGB, UNSIGNED_BYTE) => matches!(internalformat, RGB8 | RGB565 | SRGB8),
        (RGB, BYTE) => internalformat == RGB8_SNORM,
        (RGB, UNSIGNED_SHORT_5_6_5) => internalformat == RGB565,
        (RGB, UNSIGNED_INT_10F_11F_11F_REV) => internalformat == R11F_G11F_B10F,
        (RGB, UNSIGNED_INT_5_9_9_9_REV) => internalformat == RGB9_E5,
        (RGB, HALF_FLOAT) => matches!(internalformat, RGB16F | R11F_G11F_B10F | RGB9_E5),
        (RGB, FLOAT) => matches!(internalformat, RGB32F | RGB16F | R11F_G11F_B10F | RGB9_E5),
        (RGB_INTEGER, UNSIGNED_BYTE) => internalformat == RGB8UI,
        (RGB_INTEGER, BYTE) => internalformat == RGB8I,
        (RGB_INTEGER, UNSIGNED_SHORT) => internalformat == RGB16UI,
        (RGB_INTEGER, SHORT) => internalformat == RGB16I,
        (RGB_INTEGER, UNSIGNED_INT) => internalformat == RGB32UI,
        (RGB_INTEGER, INT) => internalformat == RGB32I,
        (RG, UNSIGNED_BYTE) => internalformat == RG8,
        (RG, BYTE) => internalformat == RG8_SNORM,
        (RG, HALF_FLOAT) => internalformat == RG16F,
        (RG, FLOAT) => matches!(internalformat, RG32F | RG16F),
        (RG_INTEGER, UNSIGNED_BYTE) => internalformat == RG8UI,
        (RG_INTEGER, BYTE) => internalformat == RG8I,
        (RG_INTEGER, UNSIGNED_SHORT) => internalformat == RG16UI,
        (RG_INTEGER, SHORT) => internalformat == RG16I,
        (RG_INTEGER, UNSIGNED_INT) => internalformat == RG32UI,
        (RG_INTEGER, INT) => internalformat == RG32I,
        (RED, UNSIGNED_BYTE) => internalformat == R8,
        (RED, BYTE) => internalformat == R8_SNORM,
        (RED, HALF_FLOAT) => internalformat == R16F,
        (RED, FLOAT) => matches!(internalformat, R32F | R16F),
        (RED_INTEGER, UNSIGNED_BYTE) => internalformat == R8UI,
        (RED_INTEGER, BYTE) => internalformat == R8I,
        (RED_INTEGER, UNSIGNED_SHORT) => internalformat == R16UI,
        (RED_INTEGER, SHORT) => internalformat == R16I,
        (RED_INTEGER, UNSIGNED_INT) => internalformat == R32UI,
        (RED_INTEGER, INT) => internalformat == R32I,
        (DEPTH_COMPONENT, UNSIGNED_SHORT) => internalformat == DEPTH_COMPONENT16,
        (DEPTH_COMPONENT, UNSIGNED_INT) => matches!(internalformat, DEPTH_COMPONENT24 | DEPTH_COMPONENT16),
        (DEPTH_COMPONENT, FLOAT) => internalformat == DEPTH_COMPONENT32F,
        (DEPTH_STENCIL, UNSIGNED_INT_24_8) => internalformat == DEPTH24_STENCIL8,
        (DEPTH_STENCIL, FLOAT_32_UNSIGNED_INT_24_8_REV) => internalformat == DEPTH32F_STENCIL8,
        _ => false,
    };
    if ok { Ok(*info) } else { Err(FormatError::Operation) }
}

/// `GL_HALF_FLOAT_OES` (OpenGL ES 2.0's half float type, a different value
/// from OpenGL ES 3.0's `GL_HALF_FLOAT`).
pub const HALF_FLOAT_OES: u32 = 0x8D61;

/// Whether `t` is a pixel data type.
pub fn is_type(t: u32) -> bool {
    use gl::*;
    matches!(
        t,
        UNSIGNED_BYTE
            | BYTE
            | UNSIGNED_SHORT
            | SHORT
            | UNSIGNED_INT
            | INT
            | HALF_FLOAT
            | HALF_FLOAT_OES
            | FLOAT
            | UNSIGNED_SHORT_5_6_5
            | UNSIGNED_SHORT_4_4_4_4
            | UNSIGNED_SHORT_5_5_5_1
            | UNSIGNED_INT_2_10_10_10_REV
            | UNSIGNED_INT_10F_11F_11F_REV
            | UNSIGNED_INT_5_9_9_9_REV
            | UNSIGNED_INT_24_8
            | FLOAT_32_UNSIGNED_INT_24_8_REV
    )
}

/// Whether `f` is a pixel data format.
pub fn is_format(f: u32) -> bool {
    use gl::*;
    matches!(
        f,
        RED | RED_INTEGER
            | RG
            | RG_INTEGER
            | RGB
            | RGB_INTEGER
            | RGBA
            | RGBA_INTEGER
            | DEPTH_COMPONENT
            | DEPTH_STENCIL
            | LUMINANCE_ALPHA
            | LUMINANCE
            | ALPHA
    )
}

// ---- Client pixel data --------------------------------------------------------

/// A pixel layout in application memory: a `(format, type)` pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Client {
    pub format: u32,
    pub ty: u32,
}

impl Client {
    pub fn new(format: u32, ty: u32) -> Client {
        let ty = if ty == HALF_FLOAT_OES { gl::HALF_FLOAT } else { ty };
        Client { format, ty }
    }

    /// Components per pixel (packed types count their whole value as one
    /// group of all components).
    pub fn components(&self) -> usize {
        use gl::*;
        match self.format {
            RED | RED_INTEGER | DEPTH_COMPONENT | LUMINANCE | ALPHA => 1,
            RG | RG_INTEGER | LUMINANCE_ALPHA | DEPTH_STENCIL => 2,
            RGB | RGB_INTEGER => 3,
            _ => 4,
        }
    }

    /// Whether the type packs all components in one value.
    pub fn packed(&self) -> bool {
        use gl::*;
        matches!(
            self.ty,
            UNSIGNED_SHORT_5_6_5
                | UNSIGNED_SHORT_4_4_4_4
                | UNSIGNED_SHORT_5_5_5_1
                | UNSIGNED_INT_2_10_10_10_REV
                | UNSIGNED_INT_10F_11F_11F_REV
                | UNSIGNED_INT_5_9_9_9_REV
                | UNSIGNED_INT_24_8
                | FLOAT_32_UNSIGNED_INT_24_8_REV
        )
    }

    /// Bytes of one component (or of the packed value).
    pub fn type_bytes(&self) -> usize {
        use gl::*;
        match self.ty {
            UNSIGNED_BYTE | BYTE => 1,
            UNSIGNED_SHORT
            | SHORT
            | HALF_FLOAT
            | UNSIGNED_SHORT_5_6_5
            | UNSIGNED_SHORT_4_4_4_4
            | UNSIGNED_SHORT_5_5_5_1 => 2,
            FLOAT_32_UNSIGNED_INT_24_8_REV => 8,
            _ => 4,
        }
    }

    /// Bytes per pixel.
    pub fn bytes_per_pixel(&self) -> usize {
        if self.packed() { self.type_bytes() } else { self.type_bytes() * self.components() }
    }

    fn integer(&self) -> bool {
        use gl::*;
        matches!(self.format, RED_INTEGER | RG_INTEGER | RGB_INTEGER | RGBA_INTEGER)
    }

    /// Decodes one pixel (GL's unpacking: normalised types to [0, 1] or
    /// [-1, 1], missing components 0, alpha 1).
    pub fn decode(&self, b: &[u8]) -> Texel {
        use gl::*;
        let n = self.components();
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let half = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        match (self.format, self.ty) {
            (DEPTH_COMPONENT, UNSIGNED_SHORT) => return Texel::Depth(f32::from(half(0)) / 65535.0, 0),
            (DEPTH_COMPONENT, UNSIGNED_INT) => return Texel::Depth((f64::from(word(0)) / 4_294_967_295.0) as f32, 0),
            (DEPTH_COMPONENT, FLOAT) => return Texel::Depth(f32::from_bits(word(0)), 0),
            (DEPTH_STENCIL, UNSIGNED_INT_24_8) => {
                let v = word(0);
                return Texel::Depth((v >> 8) as f32 / 16_777_215.0, v as u8);
            }
            (DEPTH_STENCIL, FLOAT_32_UNSIGNED_INT_24_8_REV) => {
                return Texel::Depth(f32::from_bits(word(0)), b[4]);
            }
            _ => {}
        }
        if self.packed() {
            let f = match self.ty {
                UNSIGNED_SHORT_5_6_5 => F::B5G6R5Unorm,
                UNSIGNED_SHORT_4_4_4_4 => F::Rgba4Unorm,
                UNSIGNED_SHORT_5_5_5_1 => F::Rgb5A1Unorm,
                UNSIGNED_INT_2_10_10_10_REV if self.integer() => F::Rgb10A2Uint,
                UNSIGNED_INT_2_10_10_10_REV => F::Rgb10A2Unorm,
                UNSIGNED_INT_10F_11F_11F_REV => F::R11G11B10Float,
                _ => F::Rgb9E5Float,
            };
            return f.decode(b);
        }
        let int = self.integer();
        let mut fv = [0.0f32, 0.0, 0.0, 1.0];
        let mut iv = [0i32, 0, 0, 1];
        let mut uv = [0u32, 0, 0, 1];
        let ts = self.type_bytes();
        for i in 0..n {
            let o = i * ts;
            match self.ty {
                UNSIGNED_BYTE => {
                    uv[i] = u32::from(b[o]);
                    fv[i] = f32::from(b[o]) / 255.0;
                }
                BYTE => {
                    iv[i] = i32::from(b[o] as i8);
                    fv[i] = (f32::from(b[o] as i8) / 127.0).max(-1.0);
                }
                UNSIGNED_SHORT => {
                    uv[i] = u32::from(half(o));
                    fv[i] = f32::from(half(o)) / 65535.0;
                }
                SHORT => {
                    iv[i] = i32::from(half(o) as i16);
                    fv[i] = (f32::from(half(o) as i16) / 32767.0).max(-1.0);
                }
                UNSIGNED_INT => {
                    uv[i] = word(o);
                    fv[i] = (f64::from(word(o)) / 4_294_967_295.0) as f32;
                }
                INT => {
                    iv[i] = word(o) as i32;
                    fv[i] = ((word(o) as i32) as f64 / 2_147_483_647.0).max(-1.0) as f32;
                }
                HALF_FLOAT => fv[i] = f16_to_f32(half(o)),
                FLOAT => fv[i] = f32::from_bits(word(o)),
                _ => {}
            }
        }
        if int {
            // The signedness comes from the type.
            return match self.ty {
                BYTE | SHORT | INT => Texel::Int(iv),
                _ => Texel::Uint(uv),
            };
        }
        // Spread luminance and alpha.
        match self.format {
            LUMINANCE => Texel::Float([fv[0], fv[0], fv[0], 1.0]),
            ALPHA => Texel::Float([0.0, 0.0, 0.0, fv[0]]),
            LUMINANCE_ALPHA => Texel::Float([fv[0], fv[0], fv[0], fv[1]]),
            _ => Texel::Float(fv),
        }
    }

    /// Encodes one pixel (`ReadPixels`): floats are clamped to [0, 1] for
    /// unsigned normalised types and rounded.
    pub fn encode(&self, t: &Texel, out: &mut [u8]) {
        use gl::*;
        let c = t.as_float();
        match (self.format, self.ty) {
            (_, UNSIGNED_INT_2_10_10_10_REV) => {
                let f = if self.integer() { F::Rgb10A2Uint } else { F::Rgb10A2Unorm };
                f.encode(t, out);
                return;
            }
            (_, UNSIGNED_SHORT_5_6_5) => return F::B5G6R5Unorm.encode(t, out),
            (_, UNSIGNED_SHORT_4_4_4_4) => return F::Rgba4Unorm.encode(t, out),
            (_, UNSIGNED_SHORT_5_5_5_1) => return F::Rgb5A1Unorm.encode(t, out),
            (_, UNSIGNED_INT_10F_11F_11F_REV) => return F::R11G11B10Float.encode(t, out),
            (_, UNSIGNED_INT_5_9_9_9_REV) => return F::Rgb9E5Float.encode(t, out),
            _ => {}
        }
        let n = self.components();
        let ts = self.type_bytes();
        let src: [f32; 4] = match self.format {
            LUMINANCE => [c[0], 0.0, 0.0, 0.0],
            ALPHA => [c[3], 0.0, 0.0, 0.0],
            LUMINANCE_ALPHA => [c[0], c[3], 0.0, 0.0],
            _ => c,
        };
        let iv = t.as_int();
        let uv = t.as_uint();
        for i in 0..n {
            let o = i * ts;
            match self.ty {
                UNSIGNED_BYTE if self.integer() => out[o] = uv[i].min(255) as u8,
                BYTE if self.integer() => out[o] = iv[i].clamp(-128, 127) as i8 as u8,
                UNSIGNED_BYTE => out[o] = (clamp01(src[i]) * 255.0 + 0.5) as u8,
                BYTE => out[o] = m::round(src[i].clamp(-1.0, 1.0) * 127.0) as i8 as u8,
                UNSIGNED_SHORT => out[o..o + 2].copy_from_slice(&(uv[i].min(65535) as u16).to_le_bytes()),
                SHORT => out[o..o + 2].copy_from_slice(&(iv[i].clamp(-32768, 32767) as i16).to_le_bytes()),
                UNSIGNED_INT => out[o..o + 4].copy_from_slice(&uv[i].to_le_bytes()),
                INT => out[o..o + 4].copy_from_slice(&iv[i].to_le_bytes()),
                HALF_FLOAT => out[o..o + 2].copy_from_slice(&f32_to_f16(src[i]).to_le_bytes()),
                FLOAT => out[o..o + 4].copy_from_slice(&src[i].to_le_bytes()),
                _ => {}
            }
        }
    }
}
