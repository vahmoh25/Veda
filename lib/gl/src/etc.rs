//! The compressed texture formats of OpenGL ES 3.0: ETC2 and EAC
//! (appendix C of the specification).
//!
//! Compressed images are decompressed when they are specified and stored
//! in an uncompressed format: 8-bit RGBA (linear or sRGB) for the ETC2
//! formats, 32-bit floats for the 11-bit EAC formats, which keeps all of
//! their precision.

use alloc::vec::Vec;

use crate::format::{ComponentType, Format, Internal};
use crate::gl;

/// A compressed format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Compressed {
    pub gl: u32,
    /// What decompressed texels are stored as.
    pub format: Format,
    /// Bytes per 4x4 block.
    pub block_bytes: usize,
    pub kind: Kind,
    /// The base internal format (`RGB`, `RGBA`, `RED`, `RG`).
    pub base: u32,
    pub component: ComponentType,
}

/// How blocks decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Rgb,
    RgbPunchthrough,
    RgbaEac,
    R11 { signed: bool },
    Rg11 { signed: bool },
}

const fn c(gl: u32, format: Format, block_bytes: usize, kind: Kind, base: u32, component: ComponentType) -> Compressed {
    Compressed { gl, format, block_bytes, kind, base, component }
}

/// The compressed formats (`GL_COMPRESSED_TEXTURE_FORMATS`).
pub const FORMATS: &[Compressed] = &[
    c(
        gl::COMPRESSED_R11_EAC,
        Format::R32Float,
        8,
        Kind::R11 { signed: false },
        gl::RED,
        ComponentType::UnsignedNormalized,
    ),
    c(
        gl::COMPRESSED_SIGNED_R11_EAC,
        Format::R32Float,
        8,
        Kind::R11 { signed: true },
        gl::RED,
        ComponentType::SignedNormalized,
    ),
    c(
        gl::COMPRESSED_RG11_EAC,
        Format::Rg32Float,
        16,
        Kind::Rg11 { signed: false },
        gl::RG,
        ComponentType::UnsignedNormalized,
    ),
    c(
        gl::COMPRESSED_SIGNED_RG11_EAC,
        Format::Rg32Float,
        16,
        Kind::Rg11 { signed: true },
        gl::RG,
        ComponentType::SignedNormalized,
    ),
    c(gl::COMPRESSED_RGB8_ETC2, Format::Rgba8Unorm, 8, Kind::Rgb, gl::RGB, ComponentType::UnsignedNormalized),
    c(gl::COMPRESSED_SRGB8_ETC2, Format::Rgba8Srgb, 8, Kind::Rgb, gl::RGB, ComponentType::UnsignedNormalized),
    c(
        gl::COMPRESSED_RGB8_PUNCHTHROUGH_ALPHA1_ETC2,
        Format::Rgba8Unorm,
        8,
        Kind::RgbPunchthrough,
        gl::RGBA,
        ComponentType::UnsignedNormalized,
    ),
    c(
        gl::COMPRESSED_SRGB8_PUNCHTHROUGH_ALPHA1_ETC2,
        Format::Rgba8Srgb,
        8,
        Kind::RgbPunchthrough,
        gl::RGBA,
        ComponentType::UnsignedNormalized,
    ),
    c(
        gl::COMPRESSED_RGBA8_ETC2_EAC,
        Format::Rgba8Unorm,
        16,
        Kind::RgbaEac,
        gl::RGBA,
        ComponentType::UnsignedNormalized,
    ),
    c(
        gl::COMPRESSED_SRGB8_ALPHA8_ETC2_EAC,
        Format::Rgba8Srgb,
        16,
        Kind::RgbaEac,
        gl::RGBA,
        ComponentType::UnsignedNormalized,
    ),
];

/// The compressed format named `gl`.
pub fn compressed(internalformat: u32) -> Option<&'static Compressed> {
    FORMATS.iter().find(|f| f.gl == internalformat)
}

impl Compressed {
    /// Bytes of a `width` x `height` x `depth` image.
    pub fn image_size(&self, width: u32, height: u32, depth: u32) -> Option<usize> {
        let blocks = (width.div_ceil(4) as usize).checked_mul(height.div_ceil(4) as usize)?;
        blocks.checked_mul(depth as usize)?.checked_mul(self.block_bytes)
    }

    /// The internal format images of this format have.
    pub fn internal(&self) -> Internal {
        let bits = match self.kind {
            Kind::Rgb => [8, 8, 8, 0, 0, 0],
            Kind::RgbPunchthrough => [8, 8, 8, 1, 0, 0],
            Kind::RgbaEac => [8, 8, 8, 8, 0, 0],
            Kind::R11 { .. } => [11, 0, 0, 0, 0, 0],
            Kind::Rg11 { .. } => [11, 11, 0, 0, 0, 0],
        };
        Internal {
            gl: self.gl,
            format: self.format,
            base: self.base,
            bits,
            component: self.component,
            renderable: false,
            float_renderable: false,
            filterable: true,
        }
    }

    /// Decompresses a `width` x `height` x `depth` image (`data` holds
    /// [`Compressed::image_size`] bytes) to tightly packed texels.
    pub fn decode(&self, data: &[u8], width: u32, height: u32, depth: u32) -> Option<Vec<u8>> {
        let tb = self.format.bytes();
        let (w, h, d) = (width as usize, height as usize, depth as usize);
        let mut out = crate::pixels::try_zeroed(w.checked_mul(h)?.checked_mul(d)?.checked_mul(tb)?)?;
        let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
        let mut texels = [[0u8; 16]; 16];
        for z in 0..d {
            for by in 0..bh {
                for bx in 0..bw {
                    let i = (z * bh + by) * bw + bx;
                    let block = &data[i * self.block_bytes..(i + 1) * self.block_bytes];
                    self.decode_block(block, &mut texels);
                    for y in 0..4 {
                        for x in 0..4 {
                            let (px, py) = (bx * 4 + x, by * 4 + y);
                            if px < w && py < h {
                                let o = ((z * h + py) * w + px) * tb;
                                out[o..o + tb].copy_from_slice(&texels[y * 4 + x][..tb]);
                            }
                        }
                    }
                }
            }
        }
        Some(out)
    }

    /// One block's 16 texels (row by row), each in `self.format`'s bytes.
    fn decode_block(&self, block: &[u8], out: &mut [[u8; 16]; 16]) {
        let word = |b: &[u8]| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        match self.kind {
            Kind::Rgb | Kind::RgbPunchthrough => {
                let mut rgba = [[0u8; 4]; 16];
                etc2_rgb(word(block), self.kind == Kind::RgbPunchthrough, &mut rgba);
                for (o, t) in out.iter_mut().zip(rgba) {
                    o[..4].copy_from_slice(&t);
                }
            }
            Kind::RgbaEac => {
                let mut rgba = [[0u8; 4]; 16];
                etc2_rgb(word(&block[8..]), false, &mut rgba);
                let alpha = eac(word(block));
                for i in 0..16 {
                    rgba[i][3] = eac_alpha(&alpha, i);
                    out[i][..4].copy_from_slice(&rgba[i]);
                }
            }
            Kind::R11 { signed } => {
                let r = eac(word(block));
                for (i, o) in out.iter_mut().enumerate() {
                    o[..4].copy_from_slice(&eac11(&r, i, signed).to_le_bytes());
                }
            }
            Kind::Rg11 { signed } => {
                let (r, g) = (eac(word(block)), eac(word(&block[8..])));
                for (i, o) in out.iter_mut().enumerate() {
                    o[..4].copy_from_slice(&eac11(&r, i, signed).to_le_bytes());
                    o[4..8].copy_from_slice(&eac11(&g, i, signed).to_le_bytes());
                }
            }
        }
    }
}

// ---- ETC2 colors -------------------------------------------------------------

const MODIFIERS: [[i32; 4]; 8] = [
    [2, 8, -2, -8],
    [5, 17, -5, -17],
    [9, 29, -9, -29],
    [13, 42, -13, -42],
    [18, 60, -18, -60],
    [24, 80, -24, -80],
    [33, 106, -33, -106],
    [47, 183, -47, -183],
];

const DISTANCES: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

fn bits(v: u64, hi: u32, lo: u32) -> u32 {
    ((v >> lo) & ((1u64 << (hi - lo + 1)) - 1)) as u32
}

fn ext4(x: u32) -> i32 {
    ((x << 4) | x) as i32
}

fn ext5(x: u32) -> i32 {
    ((x << 3) | (x >> 2)) as i32
}

fn ext6(x: u32) -> i32 {
    ((x << 2) | (x >> 4)) as i32
}

fn ext7(x: u32) -> i32 {
    ((x << 1) | (x >> 6)) as i32
}

fn clamp255(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// The 2-bit index of texel `(x, y)`: the most significant bit in bits
/// 16-31, the least in 0-15, texels counted down columns.
fn index2(v: u64, x: usize, y: usize) -> usize {
    let i = x * 4 + y;
    ((((v >> (16 + i)) & 1) << 1) | ((v >> i) & 1)) as usize
}

/// Decodes an ETC2 RGB block (with punch-through alpha if `punch`) into
/// texels in row order.
fn etc2_rgb(v: u64, punch: bool, out: &mut [[u8; 4]; 16]) {
    let d_bit = bits(v, 33, 33) == 1;
    // With punch-through alpha the bit says the block is opaque; there is
    // no individual mode.
    let opaque = !punch || d_bit;
    let differential = punch || d_bit;
    if !differential {
        let base = [
            [ext4(bits(v, 63, 60)), ext4(bits(v, 55, 52)), ext4(bits(v, 47, 44))],
            [ext4(bits(v, 59, 56)), ext4(bits(v, 51, 48)), ext4(bits(v, 43, 40))],
        ];
        subblocks(v, base, true, out);
        return;
    }
    let (r, g, b) = (bits(v, 63, 59) as i32, bits(v, 55, 51) as i32, bits(v, 47, 43) as i32);
    let signed3 = |x: u32| ((x as i32) << 29) >> 29;
    let (dr, dg, db) = (signed3(bits(v, 58, 56)), signed3(bits(v, 50, 48)), signed3(bits(v, 42, 40)));
    if !(0..=31).contains(&(r + dr)) {
        // T mode.
        let r1 = (bits(v, 60, 59) << 2) | bits(v, 57, 56);
        let c1 = [ext4(r1), ext4(bits(v, 55, 52)), ext4(bits(v, 51, 48))];
        let c2 = [ext4(bits(v, 47, 44)), ext4(bits(v, 43, 40)), ext4(bits(v, 39, 36))];
        let d = DISTANCES[((bits(v, 35, 34) << 1) | bits(v, 32, 32)) as usize];
        let paint = [c1, add(c2, d), c2, add(c2, -d)];
        painted(v, paint, opaque, out);
    } else if !(0..=31).contains(&(g + dg)) {
        // H mode.
        let g1 = (bits(v, 58, 56) << 1) | bits(v, 52, 52);
        let b1 = (bits(v, 51, 51) << 3) | bits(v, 49, 47);
        let c1 = [ext4(bits(v, 62, 59)), ext4(g1), ext4(b1)];
        let c2 = [ext4(bits(v, 46, 43)), ext4(bits(v, 42, 39)), ext4(bits(v, 38, 35))];
        let value = |c: [i32; 3]| (c[0] << 16) | (c[1] << 8) | c[2];
        let index = (bits(v, 34, 34) << 2) | (bits(v, 32, 32) << 1) | u32::from(value(c1) >= value(c2));
        let d = DISTANCES[index as usize];
        let paint = [add(c1, d), add(c1, -d), add(c2, d), add(c2, -d)];
        painted(v, paint, opaque, out);
    } else if !(0..=31).contains(&(b + db)) {
        // Planar mode (always opaque).
        let ro = ext6(bits(v, 62, 57));
        let go = ext7((bits(v, 56, 56) << 6) | bits(v, 54, 49));
        let bo = ext6((bits(v, 48, 48) << 5) | (bits(v, 44, 43) << 3) | bits(v, 41, 39));
        let rh = ext6((bits(v, 38, 34) << 1) | bits(v, 32, 32));
        let gh = ext7(bits(v, 31, 25));
        let bh = ext6(bits(v, 24, 19));
        let rv = ext6(bits(v, 18, 13));
        let gv = ext7(bits(v, 12, 6));
        let bv = ext6(bits(v, 5, 0));
        for y in 0..4i32 {
            for x in 0..4i32 {
                let f = |o: i32, h: i32, v: i32| clamp255((x * (h - o) + y * (v - o) + 4 * o + 2) >> 2);
                out[(y * 4 + x) as usize] = [f(ro, rh, rv), f(go, gh, gv), f(bo, bh, bv), 255];
            }
        }
    } else {
        // Differential mode.
        let base = [
            [ext5(r as u32), ext5(g as u32), ext5(b as u32)],
            [ext5((r + dr) as u32), ext5((g + dg) as u32), ext5((b + db) as u32)],
        ];
        subblocks(v, base, opaque, out);
    }
}

fn add(c: [i32; 3], d: i32) -> [i32; 3] {
    [c[0] + d, c[1] + d, c[2] + d]
}

/// Individual and differential modes: two subblocks, each a base color and
/// a modifier table.
fn subblocks(v: u64, base: [[i32; 3]; 2], opaque: bool, out: &mut [[u8; 4]; 16]) {
    let flip = bits(v, 32, 32) == 1;
    let tables = [bits(v, 39, 37) as usize, bits(v, 36, 34) as usize];
    for y in 0..4 {
        for x in 0..4 {
            let sub = usize::from(if flip { y >= 2 } else { x >= 2 });
            let index = index2(v, x, y);
            let texel = &mut out[y * 4 + x];
            if !opaque && index == 2 {
                *texel = [0; 4];
                continue;
            }
            let m = if !opaque && index == 0 { 0 } else { MODIFIERS[tables[sub]][index] };
            let c = base[sub];
            *texel = [clamp255(c[0] + m), clamp255(c[1] + m), clamp255(c[2] + m), 255];
        }
    }
}

/// T and H modes: each texel picks one of four paint colors.
fn painted(v: u64, paint: [[i32; 3]; 4], opaque: bool, out: &mut [[u8; 4]; 16]) {
    for y in 0..4 {
        for x in 0..4 {
            let index = index2(v, x, y);
            out[y * 4 + x] = if !opaque && index == 2 {
                [0; 4]
            } else {
                let c = paint[index];
                [clamp255(c[0]), clamp255(c[1]), clamp255(c[2]), 255]
            };
        }
    }
}

// ---- EAC ---------------------------------------------------------------------------

const EAC_MODIFIERS: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14],
    [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12],
    [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11],
    [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10],
    [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9],
    [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9],
    [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9],
    [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8],
    [-3, -5, -7, -9, 2, 4, 6, 8],
];

/// An EAC block's fields: base codeword, multiplier, and each texel's
/// modifier (in row order).
struct Eac {
    base: u8,
    multiplier: i32,
    modifiers: [i32; 16],
}

fn eac(v: u64) -> Eac {
    let table = &EAC_MODIFIERS[bits(v, 51, 48) as usize];
    let mut modifiers = [0; 16];
    for y in 0..4 {
        for x in 0..4 {
            // Texels down columns, three bits each from bit 47.
            let i = x * 4 + y;
            let index = (v >> (45 - 3 * i)) & 7;
            modifiers[y * 4 + x] = table[index as usize];
        }
    }
    Eac { base: bits(v, 63, 56) as u8, multiplier: bits(v, 55, 52) as i32, modifiers }
}

/// The 8-bit alpha of `RGBA8_ETC2_EAC` (equation C.1).
fn eac_alpha(e: &Eac, i: usize) -> u8 {
    clamp255(i32::from(e.base) + e.modifiers[i] * e.multiplier)
}

/// An 11-bit EAC value as a float (equations C.4, C.5, C.8 and C.9).
fn eac11(e: &Eac, i: usize, signed: bool) -> f32 {
    let m = e.modifiers[i];
    let term = if e.multiplier == 0 { m } else { m * e.multiplier * 8 };
    if signed {
        let base = i32::from(e.base as i8).max(-127);
        (base * 8 + term).clamp(-1023, 1023) as f32 / 1023.0
    } else {
        (i32::from(e.base) * 8 + 4 + term).clamp(0, 2047) as f32 / 2047.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn block(v: u64) -> [u8; 8] {
        v.to_be_bytes()
    }

    #[test]
    fn individual_mode_matches_the_specification_example() {
        // R1 = 14, G1 = 3, B1 = 8: base color (238, 51, 136); table 0;
        // every texel index 0 (+2).
        let v: u64 = (14 << 60) | (3 << 52) | (8 << 44);
        let f = compressed(gl::COMPRESSED_RGB8_ETC2).unwrap();
        let out = f.decode(&block(v), 4, 4, 1).unwrap();
        assert_eq!(&out[..4], &[240, 53, 138, 255]);
    }

    #[test]
    fn differential_mode_extends_five_bits() {
        // R = 28 (231), dR = -4 → 24 (198); G = 4 (33), dG = 2 → 6 (49);
        // B = 3 (24), dB = 0. Flip: subblock 2 is the bottom half. Table 7,
        // index 1 (+183) for every texel.
        let mut v: u64 = (28 << 59) | (0b100 << 56) | (4 << 51) | (2 << 48) | (3 << 43);
        v |= 1 << 33; // differential
        v |= 1 << 32; // flip
        v |= (7 << 37) | (7 << 34);
        v |= 0xFFFF; // LSBs: index 1
        let f = compressed(gl::COMPRESSED_RGB8_ETC2).unwrap();
        let out = f.decode(&block(v), 4, 4, 1).unwrap();
        // Top-left texel: subblock 1.
        assert_eq!(&out[..4], &[255, 216, 207, 255]);
        // Bottom-left: subblock 2.
        let o = (3 * 4) * 4;
        assert_eq!(&out[o..o + 4], &[255, 232, 207, 255]);
    }

    #[test]
    fn eac_matches_the_specification_examples() {
        // Base 103, table 13, multiplier 2, texel b (x 0, y 1) index 3:
        // R11 gives 668; alpha 83.
        let index_b = 3u64 << 42;
        let v = (103u64 << 56) | (2 << 52) | (13 << 48) | index_b;
        let e = eac(v);
        assert_eq!(eac11(&e, 4, false), 668.0 / 2047.0);
        assert_eq!(eac_alpha(&eac(v), 4), 83);
        // Multiplier 0: 818.
        let v = (103u64 << 56) | (13 << 48) | index_b;
        assert_eq!(eac11(&eac(v), 4, false), 818.0 / 2047.0);
        // Signed: base 60, multiplier 2 → 320; base -128 counts as -127.
        let v = (60u64 << 56) | (2 << 52) | (13 << 48) | index_b;
        assert_eq!(eac11(&eac(v), 4, true), 320.0 / 1023.0);
        let v = (0x80u64 << 56) | (13 << 48) | (4u64 << 45);
        assert_eq!(eac11(&eac(v), 0, true), (-127.0 * 8.0) / 1023.0);
    }

    #[test]
    fn punchthrough_index_2_is_transparent_black() {
        // Differential (opaque bit clear), texel a index 2 (MSB set).
        let v: u64 = (16 << 59) | (16 << 51) | (16 << 43) | (1 << 16);
        let f = compressed(gl::COMPRESSED_RGB8_PUNCHTHROUGH_ALPHA1_ETC2).unwrap();
        let out = f.decode(&block(v), 4, 4, 1).unwrap();
        assert_eq!(&out[..4], &[0, 0, 0, 0]);
        // Index 0 is unmodified and opaque.
        assert_eq!(&out[4..8], &[132, 132, 132, 255]);
    }

    #[test]
    fn planar_mode_interpolates() {
        // R + dR overflows... no: B + dB overflows (B = 31, dB = +1) with R
        // and G in range selects planar. O = (0, 0, 0) at the corner.
        let mut v: u64 = (31u64 << 43) | (1 << 40);
        v |= 1 << 33;
        let f = compressed(gl::COMPRESSED_RGB8_ETC2).unwrap();
        let out = f.decode(&block(v), 4, 4, 1).unwrap();
        assert_eq!(out.len(), 64);
        // All texels opaque.
        assert!(out.chunks(4).all(|t| t[3] == 255));
    }

    #[test]
    fn partial_blocks_and_sizes() {
        let f = compressed(gl::COMPRESSED_RG11_EAC).unwrap();
        assert_eq!(f.image_size(5, 5, 1), Some(4 * 16));
        let data = vec![0u8; 64];
        let out = f.decode(&data, 5, 5, 1).unwrap();
        assert_eq!(out.len(), 5 * 5 * 8);
    }
}
