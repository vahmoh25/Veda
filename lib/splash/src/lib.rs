//! `vsplash` — the boot splash's picture: a dark gradient with the Veda
//! logo (OS1's white ring from "Her").
//!
//! The boot loader paints it the moment it has a screen, and the window
//! system takes it over when it starts: it draws the same picture, pixel
//! for pixel, before it brings it to life and dissolves it into the
//! desktop. Both draw through this crate, so the handover never shows.
//! Everything is integer arithmetic on `0xRRGGBB` colours (the loader has
//! no floating point).
//!
//! The gradient is dark, and each channel steps only a dozen to a few
//! dozen times from the top to the bottom: drawn in whole steps, it would
//! show as bands as tall as a tenth of the screen. So it is dithered: each
//! pixel takes the step above or the one below by an 8x8 ordered (Bayer)
//! pattern, in the proportion the gradient falls between them, which the
//! eye sees as the colour between.

#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

/// The gradient's colours at the top and the bottom of the screen.
pub const TOP: u32 = 0x07_0A16;
pub const BOTTOM: u32 = 0x12_1A35;
/// The ring's colour.
pub const RING: u32 = 0xFF_FFFF;

/// `a` mixed with `b`, `t` parts of 256 of `b`.
pub fn mix(a: u32, b: u32, t: u32) -> u32 {
    let channel = |s: u32| {
        let (x, y) = ((a >> s) & 0xFF, (b >> s) & 0xFF);
        ((x * (256 - t) + y * t) >> 8) << s
    };
    channel(16) | channel(8) | channel(0)
}

/// The integer square root of `n`.
pub fn isqrt(n: u32) -> u32 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// The ordered-dither (8x8 Bayer) pattern at pixel (`x`, `y`), 0 to 63:
/// the thresholds of every 2x2, 4x4 and 8x8 square of pixels are spread
/// evenly over the range.
pub fn bayer(x: u32, y: u32) -> u32 {
    let level = |s: u32| (2 * ((x >> s) & 1) + 3 * ((y >> s) & 1)) & 3;
    16 * level(0) + 4 * level(1) + level(2)
}

/// The background of row `y` of a screen `h` pixels high, from pixel `x`
/// on, into `out` (`out.len()` pixels): the gradient from [`TOP`] to
/// [`BOTTOM`] through the middle of the row, dithered. Each channel is
/// `floor(v + (bayer + 1/2) / 64)` of its value `v` there: in integers,
/// `v` in 128ths of a step and `h`ths, as the window system's shaders
/// have it too.
pub fn background(y: u32, h: u32, x: u32, out: &mut [u32]) {
    let h = h.max(1) as i64;
    let unit = 128 * h;
    // A channel's whole steps along the row, and the pattern's threshold
    // from which a pixel takes the next: where (2 bayer + 1) h, added to
    // what is left over, makes another step.
    let channel = |s: u32| {
        let (top, bottom) = (((TOP >> s) & 0xFF) as i64, ((BOTTOM >> s) & 0xFF) as i64);
        let v = top * unit + (bottom - top) * (2 * y as i64 + 1) * 64;
        let (whole, part) = (v / unit, v % unit);
        let next = (unit - part + h - 1) / h;
        (whole as u32, (next / 2) as u32)
    };
    let [r, g, b] = [channel(16), channel(8), channel(0)];
    for (i, px) in out.iter_mut().enumerate() {
        let t = bayer(x + i as u32, y);
        let step = |(whole, from): (u32, u32)| whole + u32::from(t >= from);
        *px = step(r) << 16 | step(g) << 8 | step(b);
    }
}

/// The ring: its centre and radii, in sixteenths of a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ring {
    pub cx: i32,
    pub cy: i32,
    pub outer: i32,
    pub inner: i32,
}

impl Ring {
    /// Where the splash has it on a `w` x `h` screen: centred across, a
    /// sixteenth of the height above the middle; its outer radius a twelfth
    /// of the height (28 to 96 pixels), the band a quarter of the inner
    /// radius wide.
    pub fn place(w: u32, h: u32) -> Ring {
        let outer = (h / 12).clamp(28, 96) as i32 * 16;
        Ring { cx: w as i32 * 8, cy: (h as i32 / 2 - h as i32 / 16) * 16, outer, inner: outer * 7 / 9 }
    }

    /// The same ring scaled by `num / den` about its centre.
    pub fn scaled(self, num: i32, den: i32) -> Ring {
        Ring { outer: self.outer * num / den, inner: self.inner * num / den, ..self }
    }

    /// How much of pixel (`x`, `y`) the ring covers, 0 to 256: across a
    /// one-pixel band at each edge.
    pub fn coverage(&self, x: i32, y: i32) -> u32 {
        let (dx, dy) = (x * 16 + 8 - self.cx, y * 16 + 8 - self.cy);
        if dx.abs() > self.outer + 16 || dy.abs() > self.outer + 16 {
            return 0;
        }
        let d = isqrt((dx * dx + dy * dy) as u32) as i32;
        ((self.outer + 8 - d).clamp(0, 16) * (d - self.inner + 8).clamp(0, 16)) as u32
    }
}

/// Row `y` of the splash on a `w` x `h` screen into `out` (`0xRRGGBB`
/// each, `out.len()` pixels from the left): the gradient and, over it,
/// the ring.
pub fn row(w: u32, h: u32, y: u32, out: &mut [u32]) {
    let ring = Ring::place(w, h);
    background(y, h, 0, out);
    // Only the ring's rows have more than the gradient, and only across it.
    let reach = ring.outer / 16 + 2;
    let (cx, cy) = (ring.cx / 16, ring.cy / 16);
    if (y as i32 - cy).abs() > reach {
        return;
    }
    let from = (cx - reach).max(0) as usize;
    let to = ((cx + reach + 1).max(0) as usize).min(out.len());
    for (x, px) in out.iter_mut().enumerate().take(to).skip(from) {
        let cover = ring.coverage(x as i32, y as i32);
        if cover > 0 {
            *px = mix(*px, RING, cover);
        }
    }
}
