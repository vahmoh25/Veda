//! Colors and pixel arithmetic.
//!
//! API colors ([`Color`]) use straight alpha. Pixels in surfaces are
//! premultiplied `0xAARRGGBB`; [`Color::premul`] converts between the two.

/// A straight-alpha `0xAARRGGBB` color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Color(pub u32);

impl Color {
    pub const TRANSPARENT: Color = Color(0);
    pub const BLACK: Color = Color(0xFF00_0000);
    pub const WHITE: Color = Color(0xFFFF_FFFF);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(0xFF00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color((a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }

    /// `0xRRGGBB` with full opacity.
    pub const fn hex(rgb: u32) -> Color {
        Color(0xFF00_0000 | (rgb & 0xFF_FFFF))
    }

    pub const fn a(self) -> u8 {
        (self.0 >> 24) as u8
    }
    pub const fn r(self) -> u8 {
        (self.0 >> 16) as u8
    }
    pub const fn g(self) -> u8 {
        (self.0 >> 8) as u8
    }
    pub const fn b(self) -> u8 {
        self.0 as u8
    }

    /// The same color with alpha `a`.
    pub const fn with_alpha(self, a: u8) -> Color {
        Color((self.0 & 0x00FF_FFFF) | (a as u32) << 24)
    }

    /// Multiplies the alpha by `f` (0.0 ..= 1.0).
    pub fn fade(self, f: f32) -> Color {
        let a = (self.a() as f32 * f.clamp(0.0, 1.0) + 0.5) as u8;
        self.with_alpha(a)
    }

    /// Linear interpolation between two colors (in sRGB space).
    pub fn lerp(self, other: Color, t: f32) -> Color {
        let t = (t.clamp(0.0, 1.0) * 256.0) as u32;
        let mix = |s: u32| -> u32 {
            let (x, y) = ((self.0 >> s) & 0xFF, (other.0 >> s) & 0xFF);
            ((x * (256 - t) + y * t) >> 8) << s
        };
        Color(mix(24) | mix(16) | mix(8) | mix(0))
    }

    /// Lighter (`amount` > 0) or darker (`amount` < 0) version.
    pub fn shade(self, amount: f32) -> Color {
        if amount >= 0.0 {
            self.lerp(Color::WHITE.with_alpha(self.a()), amount)
        } else {
            self.lerp(Color::BLACK.with_alpha(self.a()), -amount)
        }
    }

    /// Premultiplied pixel value.
    pub fn premul(self) -> u32 {
        premultiply(self.0)
    }
}

/// `x * a / 255` for the two 8-bit channels at bits 0-7 and 16-23 of `x`.
#[inline(always)]
fn mul_div255_rb(x: u32, a: u32) -> u32 {
    let t = (x & 0x00FF_00FF) * a + 0x0080_0080;
    ((t + ((t >> 8) & 0x00FF_00FF)) >> 8) & 0x00FF_00FF
}

/// Scales all four channels of a pixel by `a / 255`.
#[inline(always)]
pub fn scale(px: u32, a: u32) -> u32 {
    mul_div255_rb(px, a) | (mul_div255_rb(px >> 8, a) << 8)
}

/// Source-over compositing of premultiplied pixels.
#[inline(always)]
pub fn over(src: u32, dst: u32) -> u32 {
    let sa = src >> 24;
    if sa == 255 {
        return src;
    }
    if sa == 0 && src == 0 {
        return dst;
    }
    src.wrapping_add(scale(dst, 255 - sa))
}

/// Straight → premultiplied alpha.
#[inline]
pub fn premultiply(px: u32) -> u32 {
    let a = px >> 24;
    match a {
        255 => px,
        0 => 0,
        _ => (scale(px, a) & 0x00FF_FFFF) | (a << 24),
    }
}

/// Premultiplied → straight alpha.
#[inline]
pub fn unpremultiply(px: u32) -> u32 {
    let a = px >> 24;
    match a {
        255 => px,
        0 => 0,
        _ => {
            let ch = |s: u32| (((px >> s) & 0xFF) * 255 + a / 2) / a;
            (a << 24) | ch(16).min(255) << 16 | ch(8).min(255) << 8 | ch(0).min(255)
        }
    }
}

/// Linear interpolation between two premultiplied pixels (`t` in 0..=256).
#[inline(always)]
pub fn lerp_px(a: u32, b: u32, t: u32) -> u32 {
    let it = 256 - t;
    let rb = ((a & 0x00FF_00FF) * it + (b & 0x00FF_00FF) * t) >> 8 & 0x00FF_00FF;
    let ag = (((a >> 8) & 0x00FF_00FF) * it + ((b >> 8) & 0x00FF_00FF) * t) & 0xFF00_FF00;
    rb | ag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_is_exact_at_extremes() {
        assert_eq!(over(0xFF11_2233, 0xFF44_5566), 0xFF11_2233);
        assert_eq!(over(0, 0xFF44_5566), 0xFF44_5566);
        // 50% black over white is mid grey.
        let half_black = premultiply(0x8000_0000);
        let r = over(half_black, 0xFFFF_FFFF);
        assert_eq!(r >> 24, 0xFF);
        assert!(((r >> 16) & 0xFF).abs_diff(127) <= 1);
    }

    #[test]
    fn premultiply_roundtrip() {
        for px in [0xFF12_3456u32, 0x80FF_8000, 0x40_204060, 0x01FF_FFFF] {
            let back = unpremultiply(premultiply(px));
            for s in [0, 8, 16] {
                let (x, y) = ((px >> s) & 0xFF, (back >> s) & 0xFF);
                let tol = 255 / (px >> 24).max(1) + 1;
                assert!(x.abs_diff(y) <= tol, "{px:08x} -> {back:08x}");
            }
        }
    }

    #[test]
    fn scale_matches_reference() {
        for a in [0u32, 1, 127, 128, 254, 255] {
            for c in [0u32, 1, 100, 200, 255] {
                let px = c << 16 | c << 8 | c | c << 24;
                let got = scale(px, a) & 0xFF;
                let want = (c * a + 127) / 255;
                assert!(got.abs_diff(want) <= 1, "c={c} a={a} got={got} want={want}");
            }
        }
    }
}
