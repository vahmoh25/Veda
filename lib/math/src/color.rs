//! Color helpers: sRGB transfer functions, HSV conversion and packed ARGB.
//!
//! Packed colors are `u32` in `0xAARRGGBB` order (Veda's pixel format:
//! bytes B, G, R, A in memory). Float components are in `[0, 1]`; functions
//! say whether they expect sRGB-encoded or linear values. Blending and
//! lighting should happen in linear space: decode with [`srgb_to_linear`]
//! (or the table-driven [`srgb8_to_linear`]) and encode with
//! [`linear_to_srgb`] / [`linear_to_srgb8`].
//!
//! The 8-bit conversion tables are computed at compile time from the exact
//! formulas, so they cost nothing at startup.

use crate::f32 as m;

/// Decodes an sRGB-encoded component to linear light with the exact
/// IEC 61966-2-1 formula (evaluated in double precision, so the result is
/// almost always correctly rounded). Values outside `[0, 1]` follow the
/// linear segment below 0.04045 and the power curve above it.
#[inline]
pub const fn srgb_to_linear(c: f32) -> f32 {
    let x = c as f64;
    if c <= 0.040_45 {
        (x / 12.92) as f32
    } else if c <= 1e30 {
        m::pow_f64((x + 0.055) / 1.055, 2.4) as f32
    } else {
        m::powf(c, 2.4) // +∞ and NaN
    }
}

/// Encodes a linear component to sRGB with the exact formula (inverse of
/// [`srgb_to_linear`]).
#[inline]
pub const fn linear_to_srgb(c: f32) -> f32 {
    let x = c as f64;
    if c <= 0.003_130_8 {
        (x * 12.92) as f32
    } else if c <= 1e30 {
        (1.055 * m::pow_f64(x, 1.0 / 2.4) - 0.055) as f32
    } else {
        m::powf(c, 1.0 / 2.4) // +∞ and NaN
    }
}

/// `srgb_to_linear(i / 255)` for every 8-bit value.
pub const SRGB8_TO_LINEAR: [f32; 256] = {
    let mut t = [0.0; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = srgb_to_linear(i as f32 / 255.0);
        i += 1;
    }
    t
};

/// Decision thresholds for encoding: `T[v] = srgb_to_linear((v - 0.5) / 255)`,
/// the linear value at which the rounded 8-bit encoding switches to `v`.
const LINEAR_TO_SRGB8_THRESHOLDS: [f32; 256] = {
    let mut t = [0.0; 256];
    let mut v = 1;
    while v < 256 {
        t[v] = srgb_to_linear((v as f32 - 0.5) / 255.0);
        v += 1;
    }
    t
};

/// Initial guesses for [`linear_to_srgb8`], indexed by `(c * 1024) as usize`.
const LINEAR_TO_SRGB8_GUESS: [u8; 1025] = {
    let mut t = [0u8; 1025];
    let mut i = 0;
    while i <= 1024 {
        let v = linear_to_srgb(i as f32 / 1024.0) * 255.0 + 0.5;
        t[i] = if v >= 255.0 { 255 } else { v as u8 };
        i += 1;
    }
    t
};

/// Decodes an 8-bit sRGB value to linear light with a table lookup.
#[inline]
pub const fn srgb8_to_linear(c: u8) -> f32 {
    SRGB8_TO_LINEAR[c as usize]
}

/// Encodes a linear value to 8-bit sRGB: `round(linear_to_srgb(c) * 255)` for
/// `c` clamped to `[0, 1]` (NaN maps to 0). A table lookup plus at most a
/// couple of comparisons; no `powf`.
#[inline]
pub fn linear_to_srgb8(c: f32) -> u8 {
    let c = crate::clamp01(c);
    let mut v = LINEAR_TO_SRGB8_GUESS[(c * 1024.0) as usize] as usize;
    while v < 255 && c >= LINEAR_TO_SRGB8_THRESHOLDS[v + 1] {
        v += 1;
    }
    while v > 0 && c < LINEAR_TO_SRGB8_THRESHOLDS[v] {
        v -= 1;
    }
    v as u8
}

/// Converts HSV to RGB. `h` is the hue in turns (`0.0` red, `1/3` green,
/// `2/3` blue; any value wraps around), `s` and `v` are in `[0, 1]`.
/// Returns `(r, g, b)` in `[0, 1]` (in whatever space `v` is in).
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h6 = crate::fract_floor(h) * 6.0;
    let sector = m::floor(h6);
    let f = h6 - sector;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match sector as i32 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Converts RGB to HSV: returns `(h, s, v)` with the hue in turns `[0, 1)`
/// (0 for grays) and saturation and value in `[0, 1]`.
pub fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = m::max(r, m::max(g, b));
    let min = m::min(r, m::min(g, b));
    let delta = max - min;
    let s = if max > 0.0 { delta / max } else { 0.0 };
    if delta <= 0.0 {
        return (0.0, s, max);
    }
    let h = if max == r {
        (g - b) / delta
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    (crate::fract_floor(h * (1.0 / 6.0)), s, max)
}

/// Packs 8-bit channels into `0xAARRGGBB`.
#[inline]
pub const fn pack_argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Unpacks `0xAARRGGBB` into `(a, r, g, b)`.
#[inline]
pub const fn unpack_argb(c: u32) -> (u8, u8, u8, u8) {
    ((c >> 24) as u8, (c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Converts a float component in `[0, 1]` to 8 bits (clamped, rounded; NaN -> 0).
#[inline]
pub const fn unorm8(x: f32) -> u8 {
    (crate::clamp01(x) * 255.0 + 0.5) as u8
}

/// Packs float channels in `[0, 1]` (clamped, no color-space conversion) into `0xAARRGGBB`.
#[inline]
pub const fn pack_argb_f32(a: f32, r: f32, g: f32, b: f32) -> u32 {
    pack_argb(unorm8(a), unorm8(r), unorm8(g), unorm8(b))
}

/// Interpolates two `0xAARRGGBB` colors channel by channel (all four
/// channels, in the encoded space). `t` is clamped to `[0, 1]`; `t = 0`
/// gives exactly `a` and `t = 1` exactly `b`.
#[inline]
pub fn lerp_argb(a: u32, b: u32, t: f32) -> u32 {
    let w = (crate::clamp01(t) * 256.0 + 0.5) as u32; // 0..=256
    let iw = 256 - w;
    // Two channels per multiply: 0x00RR00BB and 0x00AA00GG.
    let rb = ((a & 0x00ff_00ff) * iw + (b & 0x00ff_00ff) * w + 0x0080_0080) >> 8;
    let ag = ((a >> 8) & 0x00ff_00ff) * iw + ((b >> 8) & 0x00ff_00ff) * w + 0x0080_0080;
    (rb & 0x00ff_00ff) | (ag & 0xff00_ff00)
}

/// Converts a straight-alpha `0xAARRGGBB` color to premultiplied alpha
/// (rounded: `c * a / 255`).
#[inline]
pub const fn premultiply_argb(c: u32) -> u32 {
    let a = c >> 24;
    // x * a / 255 rounded, via (t + 128 + ((t + 128) >> 8)) >> 8, two channels at a time.
    let rb = (c & 0x00ff_00ff) * a + 0x0080_0080;
    let rb = ((rb + ((rb >> 8) & 0x00ff_00ff)) >> 8) & 0x00ff_00ff;
    let g = ((c >> 8) & 0xff) * a + 0x80;
    let g = ((g + (g >> 8)) >> 8) & 0xff;
    (a << 24) | rb | (g << 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_srgb_to_linear(c: f64) -> f64 {
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }

    fn exact_linear_to_srgb(c: f64) -> f64 {
        if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    }

    #[test]
    fn transfer_functions() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert_eq!(srgb_to_linear(1.0), 1.0);
        assert_eq!(linear_to_srgb(0.0), 0.0);
        assert!((linear_to_srgb(1.0) - 1.0).abs() <= f32::EPSILON);
        let ulps = |a: f32, b: f64| crate::libm_tests::ulps32(a, b as f32);
        let mut rng = crate::Rng::new(3);
        for i in 0..=200_000 {
            let c = if i <= 100_000 { i as f32 / 100_000.0 } else { rng.range_f32(-0.5, 2.0) };
            let lin = srgb_to_linear(c);
            assert!(ulps(lin, exact_srgb_to_linear(c as f64)) <= 1, "{c}");
            assert!(ulps(linear_to_srgb(c), exact_linear_to_srgb(c as f64)) <= 1, "{c}");
            if (0.0..=1.0).contains(&c) {
                let back = linear_to_srgb(lin);
                assert!((back - c).abs() < 1e-6, "{c} -> {lin} -> {back}");
            }
        }
        // Mid gray.
        assert!((srgb_to_linear(0.5) - 0.214_041_14).abs() < 1e-7);
        assert_eq!(srgb_to_linear(f32::INFINITY), f32::INFINITY);
        assert!(srgb_to_linear(f32::NAN).is_nan());
        assert!(linear_to_srgb(f32::NAN).is_nan());
        assert!((srgb_to_linear(-0.5) - -0.5 / 12.92).abs() < 1e-8);
    }

    #[test]
    fn tables() {
        for i in 0..=255u8 {
            assert_eq!(srgb8_to_linear(i), srgb_to_linear(i as f32 / 255.0));
            assert_eq!(linear_to_srgb8(srgb8_to_linear(i)), i, "round trip of {i}");
        }
        // linear_to_srgb8 equals the exactly rounded encoding (except within an
        // ulp of a decision threshold).
        let mut rng = crate::Rng::new(4);
        for k in 0..200_000 {
            let c = if k < 100_000 { k as f32 / 100_000.0 } else { rng.next_f32() * rng.next_f32() };
            let exact = exact_linear_to_srgb(c as f64) * 255.0 + 0.5;
            let got = linear_to_srgb8(c) as f64;
            let near_threshold = (exact - exact.round()).abs() < 1e-4;
            assert!(got == exact.floor() || near_threshold, "c = {c}: got {got}, exact {exact}");
        }
        assert_eq!(linear_to_srgb8(-1.0), 0);
        assert_eq!(linear_to_srgb8(2.0), 255);
        assert_eq!(linear_to_srgb8(f32::NAN), 0);
        assert_eq!(linear_to_srgb8(1.0), 255);
    }

    #[test]
    fn hsv() {
        let close = |a: (f32, f32, f32), b: (f32, f32, f32)| {
            (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5 && (a.2 - b.2).abs() < 1e-5
        };
        assert!(close(hsv_to_rgb(0.0, 1.0, 1.0), (1.0, 0.0, 0.0)));
        assert!(close(hsv_to_rgb(1.0 / 3.0, 1.0, 1.0), (0.0, 1.0, 0.0)));
        assert!(close(hsv_to_rgb(2.0 / 3.0, 1.0, 1.0), (0.0, 0.0, 1.0)));
        assert!(close(hsv_to_rgb(1.0 / 6.0, 1.0, 1.0), (1.0, 1.0, 0.0)));
        assert!(close(hsv_to_rgb(-1.0 / 6.0, 1.0, 1.0), (1.0, 0.0, 1.0))); // wraps
        assert!(close(hsv_to_rgb(0.3, 0.0, 0.5), (0.5, 0.5, 0.5)));
        assert!(close(rgb_to_hsv(1.0, 0.0, 0.0), (0.0, 1.0, 1.0)));
        assert!(close(rgb_to_hsv(0.0, 0.0, 1.0), (2.0 / 3.0, 1.0, 1.0)));
        assert!(close(rgb_to_hsv(0.5, 0.5, 0.5), (0.0, 0.0, 0.5)));
        assert!(close(rgb_to_hsv(0.0, 0.0, 0.0), (0.0, 0.0, 0.0)));
        let mut rng = crate::Rng::new(8);
        for _ in 0..10_000 {
            let rgb = (rng.next_f32(), rng.next_f32(), rng.next_f32());
            let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
            assert!((0.0..1.0).contains(&h) && (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&v));
            assert!(close(hsv_to_rgb(h, s, v), rgb), "{rgb:?}");
        }
    }

    #[test]
    fn packed() {
        let c = pack_argb(0x12, 0x34, 0x56, 0x78);
        assert_eq!(c, 0x1234_5678);
        assert_eq!(unpack_argb(c), (0x12, 0x34, 0x56, 0x78));
        assert_eq!(pack_argb_f32(1.0, 1.0, 0.5, 0.0), 0xffff_8000);
        assert_eq!(pack_argb_f32(2.0, -1.0, f32::NAN, 0.25), 0xff00_0040);
        let (a, b) = (0xff00_80ffu32, 0x0080_ff00u32);
        assert_eq!(lerp_argb(a, b, 0.0), a);
        assert_eq!(lerp_argb(a, b, 1.0), b);
        assert_eq!(lerp_argb(a, b, -3.0), a);
        assert_eq!(lerp_argb(a, b, 7.0), b);
        assert_eq!(lerp_argb(a, b, f32::NAN), a);
        assert_eq!(lerp_argb(0x0000_0000, 0xffff_ffff, 0.5), 0x8080_8080);
        // Each channel matches a rounded scalar lerp.
        let mut rng = crate::Rng::new(9);
        for _ in 0..10_000 {
            let (a, b, t) = (rng.next_u32(), rng.next_u32(), rng.next_f32());
            let r = lerp_argb(a, b, t);
            let w = (t * 256.0 + 0.5) as u32;
            for shift in [0, 8, 16, 24] {
                let (ca, cb) = ((a >> shift) & 0xff, (b >> shift) & 0xff);
                let expected = (ca * (256 - w) + cb * w + 128) >> 8;
                assert_eq!((r >> shift) & 0xff, expected);
            }
        }
        assert_eq!(premultiply_argb(0xff12_3456), 0xff12_3456);
        assert_eq!(premultiply_argb(0x0012_3456), 0x0000_0000);
        assert_eq!(premultiply_argb(0x80ff_8040), 0x8080_4020);
        for a in 0..=255u32 {
            for x in [0u32, 1, 17, 128, 200, 255] {
                let c = (a << 24) | (x << 16) | (x << 8) | x;
                let p = premultiply_argb(c);
                let expected = (x * a + 127) / 255; // round(x * a / 255)
                assert_eq!(p >> 24, a);
                assert_eq!((p >> 16) & 0xff, expected, "a={a} x={x}");
                assert_eq!((p >> 8) & 0xff, expected);
                assert_eq!(p & 0xff, expected);
            }
        }
    }
}
