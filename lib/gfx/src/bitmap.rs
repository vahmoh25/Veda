//! Owned pixel images.

use alloc::vec;
use alloc::vec::Vec;

use crate::color::{lerp_px, premultiply};
use crate::geom::Rect;

/// An owned image of premultiplied `0xAARRGGBB` pixels.
#[derive(Clone, Default)]
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u32>,
}

impl core::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Bitmap({}x{})", self.width, self.height)
    }
}

impl Bitmap {
    pub fn new(width: i32, height: i32) -> Bitmap {
        let (w, h) = (width.max(0), height.max(0));
        Bitmap { width: w, height: h, pixels: vec![0; (w * h) as usize] }
    }

    pub fn filled(width: i32, height: i32, px: u32) -> Bitmap {
        let mut b = Bitmap::new(width, height);
        b.pixels.fill(px);
        b
    }

    /// Wraps straight-alpha pixels (as produced by image decoders).
    pub fn from_straight(width: i32, height: i32, mut pixels: Vec<u32>) -> Bitmap {
        for p in pixels.iter_mut() {
            *p = premultiply(*p);
        }
        Bitmap { width, height, pixels }
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return 0;
        }
        self.pixels[(y * self.width + x) as usize]
    }

    /// Bilinear sample at a pixel-space position (pixel centres at +0.5).
    pub fn sample_bilinear(&self, fx: f32, fy: f32) -> u32 {
        let x = fx - 0.5;
        let y = fy - 0.5;
        let x0 = vmath::FloatExt::floor(x);
        let y0 = vmath::FloatExt::floor(y);
        let tx = ((x - x0) * 256.0) as u32;
        let ty = ((y - y0) * 256.0) as u32;
        let (ix, iy) = (x0 as i32, y0 as i32);
        let clamp = |v: i32, max: i32| v.clamp(0, max - 1);
        let (x0c, x1c) = (clamp(ix, self.width), clamp(ix + 1, self.width));
        let (y0c, y1c) = (clamp(iy, self.height), clamp(iy + 1, self.height));
        let w = self.width as usize;
        let p = |xx: i32, yy: i32| self.pixels[yy as usize * w + xx as usize];
        let top = lerp_px(p(x0c, y0c), p(x1c, y0c), tx);
        let bottom = lerp_px(p(x0c, y1c), p(x1c, y1c), tx);
        lerp_px(top, bottom, ty)
    }

    /// High-quality resize (area averaging when shrinking, bilinear when
    /// growing).
    pub fn resized(&self, w: i32, h: i32) -> Bitmap {
        let mut out = Bitmap::new(w, h);
        if self.width == 0 || self.height == 0 || w == 0 || h == 0 {
            return out;
        }
        if w <= self.width && h <= self.height {
            // Box filter: average all source pixels covering each target pixel.
            for ty in 0..h {
                let sy0 = (ty as i64 * self.height as i64 / h as i64) as i32;
                let sy1 = (((ty + 1) as i64 * self.height as i64 / h as i64) as i32).max(sy0 + 1);
                for tx in 0..w {
                    let sx0 = (tx as i64 * self.width as i64 / w as i64) as i32;
                    let sx1 = (((tx + 1) as i64 * self.width as i64 / w as i64) as i32).max(sx0 + 1);
                    let mut acc = [0u32; 4];
                    for sy in sy0..sy1 {
                        let row = &self.pixels[(sy * self.width) as usize..];
                        for sx in sx0..sx1 {
                            let p = row[sx as usize];
                            acc[0] += p >> 24;
                            acc[1] += (p >> 16) & 0xFF;
                            acc[2] += (p >> 8) & 0xFF;
                            acc[3] += p & 0xFF;
                        }
                    }
                    let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
                    out.pixels[(ty * w + tx) as usize] =
                        (acc[0] / n) << 24 | (acc[1] / n) << 16 | (acc[2] / n) << 8 | (acc[3] / n);
                }
            }
        } else {
            let sx = self.width as f32 / w as f32;
            let sy = self.height as f32 / h as f32;
            for ty in 0..h {
                for tx in 0..w {
                    out.pixels[(ty * w + tx) as usize] =
                        self.sample_bilinear((tx as f32 + 0.5) * sx, (ty as f32 + 0.5) * sy);
                }
            }
        }
        out
    }

    /// Scales to cover `w` x `h` (cropping the overflow, keeping the aspect).
    pub fn cover(&self, w: i32, h: i32) -> Bitmap {
        if self.width == 0 || self.height == 0 {
            return Bitmap::new(w, h);
        }
        let scale = (w as f32 / self.width as f32).max(h as f32 / self.height as f32);
        let sw = ((self.width as f32 * scale) as i32).max(w);
        let sh = ((self.height as f32 * scale) as i32).max(h);
        let scaled = self.resized(sw, sh);
        scaled.crop(Rect::new((sw - w) / 2, (sh - h) / 2, w, h))
    }

    pub fn crop(&self, r: Rect) -> Bitmap {
        let r = r.intersect(&self.rect());
        let mut out = Bitmap::new(r.w, r.h);
        for y in 0..r.h {
            let src = ((r.y + y) * self.width + r.x) as usize;
            out.pixels[(y * r.w) as usize..((y + 1) * r.w) as usize]
                .copy_from_slice(&self.pixels[src..src + r.w as usize]);
        }
        out
    }

    /// Separable box blur, repeated `passes` times (3 passes ≈ Gaussian).
    pub fn blur(&mut self, radius: i32, passes: u32) {
        if radius <= 0 || self.width == 0 || self.height == 0 {
            return;
        }
        let mut tmp = vec![0u32; self.pixels.len()];
        for _ in 0..passes {
            box_pass(&self.pixels, &mut tmp, self.width, self.height, radius, true);
            box_pass(&tmp, &mut self.pixels, self.width, self.height, radius, false);
        }
    }
}

/// One horizontal or vertical box-blur pass with edge clamping.
fn box_pass(src: &[u32], dst: &mut [u32], w: i32, h: i32, r: i32, horizontal: bool) {
    let (len, lines) = if horizontal { (w, h) } else { (h, w) };
    let idx =
        |line: i32, i: i32| -> usize { if horizontal { (line * w + i) as usize } else { (i * w + line) as usize } };
    let div = (2 * r + 1) as u32;
    for line in 0..lines {
        let mut acc = [0u32; 4];
        let at = |i: i32| src[idx(line, i.clamp(0, len - 1))];
        for i in -r..=r {
            let p = at(i);
            acc[0] += p >> 24;
            acc[1] += (p >> 16) & 0xFF;
            acc[2] += (p >> 8) & 0xFF;
            acc[3] += p & 0xFF;
        }
        for i in 0..len {
            dst[idx(line, i)] = (acc[0] / div) << 24 | (acc[1] / div) << 16 | (acc[2] / div) << 8 | (acc[3] / div);
            let (add, sub) = (at(i + r + 1), at(i - r));
            acc[0] = acc[0] + (add >> 24) - (sub >> 24);
            acc[1] = acc[1] + ((add >> 16) & 0xFF) - ((sub >> 16) & 0xFF);
            acc[2] = acc[2] + ((add >> 8) & 0xFF) - ((sub >> 8) & 0xFF);
            acc[3] = acc[3] + (add & 0xFF) - (sub & 0xFF);
        }
    }
}
