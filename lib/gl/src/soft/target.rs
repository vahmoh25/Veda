//! Render targets: reading and writing color, depth and stencil samples,
//! blending (OpenGL ES 3.0 section 4.1.7) and the stencil and depth tests
//! (sections 4.1.4 and 4.1.5).

use crate::backend::{Blend, BlendEq, BlendFactor, Func, StencilOp};
use crate::format::{Class, Format, Texel, linear_to_srgb, srgb_to_linear};

/// A color, depth or stencil image being rendered to.
#[derive(Clone, Copy, Debug)]
pub struct RenderTarget {
    pub ptr: *mut u8,
    pub width: u32,
    pub height: u32,
    pub row: usize,
    /// Bytes per pixel (all samples).
    pub texel: usize,
    pub format: Format,
    pub samples: u32,
}

// SAFETY: a target points into a resource the renderer keeps alive while
// the scene renders; workers touch disjoint tiles of it.
unsafe impl Send for RenderTarget {}
unsafe impl Sync for RenderTarget {}

impl RenderTarget {
    /// The bytes of sample `s` of pixel `(x, y)`.
    ///
    /// # Safety
    /// `(x, y)` must be inside the target and no other thread may access
    /// the pixel at the same time.
    #[inline(always)]
    pub unsafe fn sample(&self, x: u32, y: u32, s: u32) -> *mut u8 {
        debug_assert!(x < self.width && y < self.height && s < self.samples.max(1));
        let bytes = self.format.bytes();
        // SAFETY: inside the target, as the caller guarantees.
        unsafe { self.ptr.add(y as usize * self.row + x as usize * self.texel + s as usize * bytes) }
    }

    /// A color sample as floats (linear for sRGB formats), or the bits of
    /// integer components.
    ///
    /// # Safety
    /// As for [`RenderTarget::sample`].
    #[inline]
    pub unsafe fn load(&self, x: u32, y: u32, s: u32) -> [f32; 4] {
        // SAFETY: as the caller guarantees.
        let p = unsafe { self.sample(x, y, s) };
        let n = self.format.bytes();
        // SAFETY: a sample is `n` bytes.
        let b = unsafe { core::slice::from_raw_parts(p, n) };
        match self.format {
            Format::Rgba8Unorm => b4(b),
            Format::Rgbx8Unorm => {
                let v = b4(b);
                [v[0], v[1], v[2], 1.0]
            }
            Format::Rgba8Srgb => {
                [srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), f32::from(b[3]) / 255.0]
            }
            f => match f.decode(b) {
                Texel::Float(v) => v,
                Texel::Int(v) => v.map(|x| f32::from_bits(x as u32)),
                Texel::Uint(v) => v.map(f32::from_bits),
                Texel::Depth(d, _) => [d, 0.0, 0.0, 1.0],
            },
        }
    }

    /// Writes a color sample (floats, or integer bits for integer
    /// formats), only the components `mask` enables.
    ///
    /// # Safety
    /// As for [`RenderTarget::sample`].
    #[inline]
    pub unsafe fn store(&self, x: u32, y: u32, s: u32, v: [f32; 4], mask: [bool; 4]) {
        // SAFETY: as the caller guarantees.
        let p = unsafe { self.sample(x, y, s) };
        let n = self.format.bytes();
        // SAFETY: a sample is `n` bytes, and only this thread touches it.
        let b = unsafe { core::slice::from_raw_parts_mut(p, n) };
        let all = mask == [true; 4];
        match self.format {
            Format::Rgba8Unorm if all => {
                for i in 0..4 {
                    b[i] = unorm8(v[i]);
                }
            }
            Format::Rgba8Srgb if all => {
                for i in 0..3 {
                    b[i] = linear_to_srgb(v[i]);
                }
                b[3] = unorm8(v[3]);
            }
            Format::Rgba8Unorm | Format::Rgbx8Unorm | Format::Rgba8Srgb => {
                let srgb = self.format == Format::Rgba8Srgb;
                for i in 0..4 {
                    if mask[i] {
                        b[i] = if srgb && i < 3 { linear_to_srgb(v[i]) } else { unorm8(v[i]) };
                    }
                }
            }
            f => {
                let texel = match f.class() {
                    Class::Sint => Texel::Int(v.map(|x| x.to_bits() as i32)),
                    Class::Uint => Texel::Uint(v.map(f32::to_bits)),
                    _ => Texel::Float(v),
                };
                if all {
                    f.encode(&texel, b);
                } else {
                    // Merge the enabled components into the stored ones.
                    let old = f.decode(b);
                    let merged = match (old, texel) {
                        (Texel::Float(o), Texel::Float(n)) => Texel::Float(pick(o, n, mask)),
                        (Texel::Int(o), Texel::Int(n)) => Texel::Int(pick(o, n, mask)),
                        (Texel::Uint(o), Texel::Uint(n)) => Texel::Uint(pick(o, n, mask)),
                        (_, n) => n,
                    };
                    f.encode(&merged, b);
                }
            }
        }
    }

    // ---- Depth and stencil ------------------------------------------------------

    /// A depth sample, as the comparison sees it: unorm formats as their
    /// integer value, float formats as the float's bits made orderable.
    ///
    /// # Safety
    /// As for [`RenderTarget::sample`].
    #[inline(always)]
    pub unsafe fn load_depth(&self, x: u32, y: u32, s: u32) -> DepthValue {
        // SAFETY: as the caller guarantees.
        let p = unsafe { self.sample(x, y, s) };
        // SAFETY: depth formats are at least 2 bytes (4 for the u32 reads).
        unsafe {
            match self.format {
                Format::D16Unorm => DepthValue::Fixed(u32::from(u16::from_le(p.cast::<u16>().read_unaligned()))),
                Format::D24Unorm | Format::D24UnormS8Uint => {
                    DepthValue::Fixed(u32::from_le(p.cast::<u32>().read_unaligned()) >> 8)
                }
                _ => DepthValue::Float(f32::from_bits(u32::from_le(p.cast::<u32>().read_unaligned()))),
            }
        }
    }

    /// Writes a depth sample (leaving the stencil bits alone).
    ///
    /// # Safety
    /// As for [`RenderTarget::sample`].
    #[inline(always)]
    pub unsafe fn store_depth(&self, x: u32, y: u32, s: u32, d: DepthValue) {
        // SAFETY: as the caller guarantees.
        let p = unsafe { self.sample(x, y, s) };
        // SAFETY: as for `load_depth`; only this thread touches the sample.
        unsafe {
            match (self.format, d) {
                (Format::D16Unorm, DepthValue::Fixed(v)) => p.cast::<u16>().write_unaligned((v as u16).to_le()),
                (Format::D24Unorm, DepthValue::Fixed(v)) => p.cast::<u32>().write_unaligned((v << 8).to_le()),
                (Format::D24UnormS8Uint, DepthValue::Fixed(v)) => {
                    let old = u32::from_le(p.cast::<u32>().read_unaligned());
                    p.cast::<u32>().write_unaligned(((v << 8) | (old & 0xFF)).to_le());
                }
                (_, DepthValue::Float(f)) => p.cast::<u32>().write_unaligned(f.to_bits().to_le()),
                _ => {}
            }
        }
    }

    /// The depth value a fragment's depth `z` (in [0, 1]) stores as.
    #[inline(always)]
    pub fn quantize(&self, z: f32) -> DepthValue {
        let z = if z > 0.0 { if z < 1.0 { z } else { 1.0 } } else { 0.0 };
        match self.format {
            Format::D16Unorm => DepthValue::Fixed((z * 65535.0 + 0.5) as u32),
            Format::D24Unorm | Format::D24UnormS8Uint => DepthValue::Fixed((f64::from(z) * 16_777_215.0 + 0.5) as u32),
            _ => DepthValue::Float(z),
        }
    }

    /// Where the stencil byte of a sample is.
    #[inline(always)]
    unsafe fn stencil_ptr(&self, x: u32, y: u32, s: u32) -> *mut u8 {
        // SAFETY: as the caller guarantees.
        let p = unsafe { self.sample(x, y, s) };
        match self.format {
            // The low byte of the packed word, or the word after the depth.
            Format::D32FloatS8Uint => unsafe { p.add(4) },
            _ => p,
        }
    }

    /// # Safety
    /// As for [`RenderTarget::sample`].
    #[inline(always)]
    pub unsafe fn load_stencil(&self, x: u32, y: u32, s: u32) -> u8 {
        // SAFETY: as the caller guarantees.
        unsafe { self.stencil_ptr(x, y, s).read() }
    }

    /// # Safety
    /// As for [`RenderTarget::sample`].
    #[inline(always)]
    pub unsafe fn store_stencil(&self, x: u32, y: u32, s: u32, v: u8) {
        // SAFETY: as the caller guarantees.
        unsafe { self.stencil_ptr(x, y, s).write(v) }
    }
}

/// A depth value as stored: an unsigned normalized integer or a float.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum DepthValue {
    Fixed(u32),
    Float(f32),
}

impl DepthValue {
    /// `self func stored`.
    #[inline(always)]
    pub fn test(self, func: Func, stored: DepthValue) -> bool {
        match (self, stored) {
            (DepthValue::Fixed(a), DepthValue::Fixed(b)) => func.test(a, b),
            (DepthValue::Float(a), DepthValue::Float(b)) => func.test(a, b),
            _ => true,
        }
    }
}

#[inline(always)]
fn b4(b: &[u8]) -> [f32; 4] {
    [f32::from(b[0]), f32::from(b[1]), f32::from(b[2]), f32::from(b[3])].map(|x| x * (1.0 / 255.0))
}

/// A float to an 8-bit unsigned normalized value (clamped, rounded).
#[inline(always)]
pub fn unorm8(x: f32) -> u8 {
    let x = if x > 0.0 { if x < 1.0 { x } else { 1.0 } } else { 0.0 };
    (x * 255.0 + 0.5) as u8
}

fn pick<T: Copy>(old: [T; 4], new: [T; 4], mask: [bool; 4]) -> [T; 4] {
    [0, 1, 2, 3].map(|i| if mask[i] { new[i] } else { old[i] })
}

/// A stencil operation's result.
#[inline(always)]
pub fn stencil_op(op: StencilOp, s: u8, reference: u8) -> u8 {
    match op {
        StencilOp::Keep => s,
        StencilOp::Zero => 0,
        StencilOp::Replace => reference,
        StencilOp::Incr => s.saturating_add(1),
        StencilOp::Decr => s.saturating_sub(1),
        StencilOp::Invert => !s,
        StencilOp::IncrWrap => s.wrapping_add(1),
        StencilOp::DecrWrap => s.wrapping_sub(1),
    }
}

/// Blends a source color with a destination (section 4.1.7). For fixed-
/// point targets the inputs are clamped to [0, 1] first (`clamp`).
#[inline]
pub fn blend(b: &Blend, src: [f32; 4], dst: [f32; 4], clamp: bool) -> [f32; 4] {
    let c01 = |v: [f32; 4]| v.map(|x| if x > 0.0 { if x < 1.0 { x } else { 1.0 } } else { 0.0 });
    let (s, d, k) = if clamp { (c01(src), c01(dst), c01(b.color)) } else { (src, dst, b.color) };
    let factor = |f: BlendFactor, alpha: bool| -> [f32; 4] {
        let rgb = |v: [f32; 3], a: f32| [v[0], v[1], v[2], a];
        match f {
            BlendFactor::Zero => [0.0; 4],
            BlendFactor::One => [1.0; 4],
            BlendFactor::SrcColor => s,
            BlendFactor::OneMinusSrcColor => s.map(|x| 1.0 - x),
            BlendFactor::DstColor => d,
            BlendFactor::OneMinusDstColor => d.map(|x| 1.0 - x),
            BlendFactor::SrcAlpha => [s[3]; 4],
            BlendFactor::OneMinusSrcAlpha => [1.0 - s[3]; 4],
            BlendFactor::DstAlpha => [d[3]; 4],
            BlendFactor::OneMinusDstAlpha => [1.0 - d[3]; 4],
            BlendFactor::ConstantColor => k,
            BlendFactor::OneMinusConstantColor => k.map(|x| 1.0 - x),
            BlendFactor::ConstantAlpha => [k[3]; 4],
            BlendFactor::OneMinusConstantAlpha => [1.0 - k[3]; 4],
            BlendFactor::SrcAlphaSaturate => {
                let f = s[3].min(1.0 - d[3]);
                if alpha { [1.0; 4] } else { rgb([f, f, f], 1.0) }
            }
        }
    };
    let (sf_rgb, df_rgb) = (factor(b.src_rgb, false), factor(b.dst_rgb, false));
    let (sf_a, df_a) = (factor(b.src_alpha, true), factor(b.dst_alpha, true));
    let eq = |e: BlendEq, s: f32, d: f32, sf: f32, df: f32| match e {
        BlendEq::Add => s * sf + d * df,
        BlendEq::Subtract => s * sf - d * df,
        BlendEq::ReverseSubtract => d * df - s * sf,
        BlendEq::Min => s.min(d),
        BlendEq::Max => s.max(d),
    };
    [
        eq(b.eq_rgb, s[0], d[0], sf_rgb[0], df_rgb[0]),
        eq(b.eq_rgb, s[1], d[1], sf_rgb[1], df_rgb[1]),
        eq(b.eq_rgb, s[2], d[2], sf_rgb[2], df_rgb[2]),
        eq(b.eq_alpha, s[3], d[3], sf_a[3], df_a[3]),
    ]
}
