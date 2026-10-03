//! Image operations: resampling, thumbnails, cropping, flipping, rotation, EXIF orientation and
//! alpha premultiplication.
//!
//! # Resampling
//!
//! [`resize`] is separable: a horizontal pass into an intermediate buffer, then a vertical pass.
//! Filter weights are computed once per output column/row (in floating point), converted to
//! 14-bit fixed point and normalized so they sum exactly to one; the per-pixel work is integer
//! only. When downscaling, the filter is widened by the scale factor so every source pixel
//! contributes (no aliasing); [`Filter::Box`] computes exact area coverage. Pixels are filtered
//! in premultiplied form with 16 bits per channel, so transparent pixels never bleed their
//! (meaningless) color into visible ones.
//!
//! Cost is proportional to the number of filter taps: per output pixel roughly 2x the scale
//! factor for `Bilinear`, 1x for `Box` and 6x for `Lanczos3` (in each direction). [`thumbnail`]
//! first reduces large factors with a cheap box filter.

use alloc::vec;
use alloc::vec::Vec;

use crate::image::{Image, Orientation};
use crate::mathf;

/// A resampling filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Filter {
    /// Nearest neighbor: fastest, blocky. Exact for integer upscaling of pixel art.
    Nearest,
    /// Triangle filter (bilinear interpolation when enlarging; widened when shrinking).
    Bilinear,
    /// Area averaging: each output pixel is the exact average of the source area it covers.
    /// Fast and alias-free for downscaling (thumbnails).
    Box,
    /// Lanczos windowed sinc with 3 lobes: the sharpest high-quality filter.
    Lanczos3,
}

/// Fixed-point precision of filter weights.
const WEIGHT_BITS: u32 = 14;
const WEIGHT_ONE: i32 = 1 << WEIGHT_BITS;
/// Premultiplied channels are stored as `value * alpha` (and alpha as `alpha * 255`).
const FULL: i32 = 255 * 255;

/// Filter weights for one axis.
struct Weights {
    /// First source index and number of taps for each output index.
    spans: Vec<(usize, usize)>,
    /// `stride` weights per output index (only the first `taps` are used).
    weights: Vec<i32>,
    stride: usize,
}

fn kernel(filter: Filter, x: f64) -> f64 {
    let ax = mathf::abs(x);
    match filter {
        Filter::Bilinear => {
            if ax < 1.0 {
                1.0 - ax
            } else {
                0.0
            }
        }
        Filter::Lanczos3 if ax < 3.0 => mathf::sinc(x) * mathf::sinc(x / 3.0),
        // Outside the filter window (Box and Nearest never evaluate the kernel).
        _ => 0.0,
    }
}

fn compute_weights(src: usize, dst: usize, filter: Filter) -> Weights {
    let scale = src as f64 / dst as f64;
    let fscale = if scale > 1.0 { scale } else { 1.0 };
    let support = match filter {
        Filter::Lanczos3 => 3.0,
        Filter::Bilinear => 1.0,
        _ => 0.5,
    } * fscale;
    let stride = (mathf::ceil(support) as usize) * 2 + 2;
    let mut spans = Vec::with_capacity(dst);
    let mut weights = vec![0i32; dst * stride];
    let mut tmp: Vec<f64> = Vec::with_capacity(stride);
    for i in 0..dst {
        tmp.clear();
        let (lo, hi);
        if filter == Filter::Box {
            // Exact coverage of [i * scale, (i + 1) * scale) by source pixel j.
            let (a, b) = (i as f64 * scale, (i + 1) as f64 * scale);
            lo = (mathf::floor(a) as usize).min(src - 1);
            hi = (mathf::ceil(b) as usize).clamp(lo + 1, src);
            for j in lo..hi {
                let overlap = b.min((j + 1) as f64) - a.max(j as f64);
                tmp.push(if overlap > 0.0 { overlap } else { 0.0 });
            }
        } else {
            let center = (i as f64 + 0.5) * scale;
            lo = (mathf::floor(center - support).max(0.0) as usize).min(src - 1);
            hi = (mathf::ceil(center + support) as usize).clamp(lo + 1, src);
            for j in lo..hi {
                tmp.push(kernel(filter, (j as f64 + 0.5 - center) / fscale));
            }
        }
        let total: f64 = tmp.iter().sum();
        let row = &mut weights[i * stride..(i + 1) * stride];
        let n = tmp.len().min(stride);
        if mathf::abs(total) < 1e-12 {
            // Degenerate (cannot happen for valid filters): fall back to the nearest pixel.
            row[0] = WEIGHT_ONE;
            spans.push(((lo + hi) / 2, 1));
            continue;
        }
        let mut sum = 0;
        let mut best = 0;
        for k in 0..n {
            let w = tmp[k] / total * WEIGHT_ONE as f64;
            row[k] = if w < 0.0 { -((-w + 0.5) as i32) } else { (w + 0.5) as i32 };
            sum += row[k];
            if row[k] > row[best] {
                best = k;
            }
        }
        // Make the weights sum to exactly one.
        row[best] += WEIGHT_ONE - sum;
        // Trim zero taps at both ends.
        let first = row[..n].iter().position(|&w| w != 0).unwrap_or(0);
        let last = row[..n].iter().rposition(|&w| w != 0).unwrap_or(0);
        row.copy_within(first..=last, 0);
        spans.push((lo + first, last - first + 1));
    }
    Weights { spans, weights, stride }
}

/// Converts a straight-alpha pixel to premultiplied 16-bit channels `[b, g, r, a]`.
#[inline(always)]
fn to_premul(p: u32) -> [u16; 4] {
    let a = p >> 24;
    [((p & 0xFF) * a) as u16, (((p >> 8) & 0xFF) * a) as u16, (((p >> 16) & 0xFF) * a) as u16, (a * 255) as u16]
}

/// Converts filtered premultiplied channels back to a straight-alpha pixel.
#[inline(always)]
fn from_premul(c: [i32; 4]) -> u32 {
    let a16 = c[3].clamp(0, FULL) as u32;
    let a = (a16 * 257 + 32768) >> 16;
    if a == 0 {
        return 0;
    }
    let inv = (255 << 16) / a16;
    let ch = |v: i32| -> u32 { ((v.clamp(0, a16 as i32) as u32 * inv + 32768) >> 16).min(255) };
    a << 24 | ch(c[2]) << 16 | ch(c[1]) << 8 | ch(c[0])
}

/// Resizes `image` to `width x height` with `filter`. Works for any combination of up- and
/// downscaling; a zero target dimension yields an empty image.
pub fn resize(image: &Image, width: u32, height: u32, filter: Filter) -> Image {
    if width == 0 || height == 0 {
        return Image { width, height, pixels: Vec::new() };
    }
    if image.is_empty() {
        return Image::new(width, height);
    }
    if (width, height) == (image.width, image.height) {
        return image.clone();
    }
    let (sw, sh) = (image.width as usize, image.height as usize);
    let (dw, dh) = (width as usize, height as usize);
    if filter == Filter::Nearest {
        let xs: Vec<usize> = (0..dw).map(|x| ((2 * x + 1) * sw / (2 * dw)).min(sw - 1)).collect();
        let mut pixels = Vec::with_capacity(dw * dh);
        for y in 0..dh {
            let sy = ((2 * y + 1) * sh / (2 * dh)).min(sh - 1);
            let row = &image.pixels[sy * sw..(sy + 1) * sw];
            pixels.extend(xs.iter().map(|&x| row[x]));
        }
        return Image { width, height, pixels };
    }

    // Horizontal pass: source rows -> `mid` (dw x sh), premultiplied 16-bit.
    let wx = compute_weights(sw, dw, filter);
    let mut mid: Vec<[u16; 4]> = vec![[0; 4]; dw * sh];
    let mut row_buf: Vec<[u16; 4]> = vec![[0; 4]; sw];
    for (src_row, mid_row) in image.pixels.chunks_exact(sw).zip(mid.chunks_exact_mut(dw)) {
        for (d, &p) in row_buf.iter_mut().zip(src_row) {
            *d = to_premul(p);
        }
        for (x, out) in mid_row.iter_mut().enumerate() {
            let (start, taps) = wx.spans[x];
            let ws = &wx.weights[x * wx.stride..x * wx.stride + taps];
            let mut acc = [1i32 << (WEIGHT_BITS - 1); 4];
            for (p, &w) in row_buf[start..start + taps].iter().zip(ws) {
                for c in 0..4 {
                    acc[c] += p[c] as i32 * w;
                }
            }
            let a = (acc[3] >> WEIGHT_BITS).clamp(0, FULL);
            *out = [
                (acc[0] >> WEIGHT_BITS).clamp(0, a) as u16,
                (acc[1] >> WEIGHT_BITS).clamp(0, a) as u16,
                (acc[2] >> WEIGHT_BITS).clamp(0, a) as u16,
                a as u16,
            ];
        }
    }
    drop(row_buf);

    // Vertical pass: weighted sums of whole `mid` rows.
    let wy = compute_weights(sh, dh, filter);
    let mut pixels = Vec::with_capacity(dw * dh);
    let mut acc: Vec<[i32; 4]> = vec![[0; 4]; dw];
    for y in 0..dh {
        let (start, taps) = wy.spans[y];
        let ws = &wy.weights[y * wy.stride..y * wy.stride + taps];
        acc.fill([1 << (WEIGHT_BITS - 1); 4]);
        for (k, &w) in ws.iter().enumerate() {
            let src = &mid[(start + k) * dw..(start + k + 1) * dw];
            for (a, p) in acc.iter_mut().zip(src) {
                for c in 0..4 {
                    a[c] += p[c] as i32 * w;
                }
            }
        }
        pixels.extend(acc.iter().map(|a| from_premul(a.map(|v| v >> WEIGHT_BITS))));
    }
    Image { width, height, pixels }
}

/// Computes the size of a thumbnail that fits in `max_width x max_height` while preserving the
/// aspect ratio of a `width x height` image (never larger than the image itself).
pub fn fit_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let (w, h, mw, mh) = (width as u64, height as u64, max_width as u64, max_height as u64);
    if w * mh > h * mw {
        (max_width, ((h * mw + w / 2) / w).max(1) as u32)
    } else {
        (((w * mh + h / 2) / h).max(1) as u32, max_height)
    }
}

/// Scales `image` down (never up) to fit in `max_width x max_height`, preserving the aspect
/// ratio. Large reductions are first done with a box filter, then finished with `filter`.
pub fn thumbnail(image: &Image, max_width: u32, max_height: u32, filter: Filter) -> Image {
    if max_width == 0 || max_height == 0 {
        return Image { width: 0, height: 0, pixels: Vec::new() };
    }
    let (tw, th) = fit_size(image.width, image.height, max_width, max_height);
    if (tw, th) == (image.width, image.height) || image.is_empty() {
        return image.clone();
    }
    // Leave at most a factor of ~2-3 for the (more expensive) final filter.
    let factor = (image.width / tw).min(image.height / th) / 2;
    if filter != Filter::Nearest && filter != Filter::Box && factor >= 2 {
        let mid = resize(image, image.width.div_ceil(factor), image.height.div_ceil(factor), Filter::Box);
        return resize(&mid, tw, th, filter);
    }
    resize(image, tw, th, filter)
}

/// Returns the part of `image` inside the rectangle at `(x, y)` of size `width x height`,
/// clipped to the image (the result may be smaller, or empty).
pub fn crop(image: &Image, x: u32, y: u32, width: u32, height: u32) -> Image {
    let x0 = x.min(image.width);
    let y0 = y.min(image.height);
    let x1 = x.saturating_add(width).min(image.width);
    let y1 = y.saturating_add(height).min(image.height);
    let (w, h) = (x1 - x0, y1 - y0);
    let mut pixels = Vec::with_capacity(w as usize * h as usize);
    if w > 0 {
        for row in image.pixels.chunks_exact(image.width as usize).skip(y0 as usize).take(h as usize) {
            pixels.extend_from_slice(&row[x0 as usize..x1 as usize]);
        }
    }
    Image { width: w, height: h, pixels }
}

/// Mirrors `image` left to right.
pub fn flip_horizontal(image: &Image) -> Image {
    let mut out = image.clone();
    if image.width > 0 {
        for row in out.pixels.chunks_exact_mut(image.width as usize) {
            row.reverse();
        }
    }
    out
}

/// Mirrors `image` top to bottom.
pub fn flip_vertical(image: &Image) -> Image {
    let mut pixels = Vec::with_capacity(image.pixels.len());
    if image.width > 0 {
        for row in image.pixels.chunks_exact(image.width as usize).rev() {
            pixels.extend_from_slice(row);
        }
    }
    Image { width: image.width, height: image.height, pixels }
}

/// Rotates `image` by 180 degrees.
pub fn rotate180(image: &Image) -> Image {
    let mut out = image.clone();
    out.pixels.reverse();
    out
}

/// Rotates `image` by 90 degrees clockwise.
pub fn rotate90(image: &Image) -> Image {
    transpose_flip(image, true, false)
}

/// Rotates `image` by 270 degrees clockwise (90 degrees counter-clockwise).
pub fn rotate270(image: &Image) -> Image {
    transpose_flip(image, false, true)
}

/// Transposes `image` (output `(x, y)` = input `(y, x)`), then optionally mirrors the result
/// horizontally and/or vertically. Works in 32x32 tiles to stay cache friendly.
fn transpose_flip(image: &Image, flip_h: bool, flip_v: bool) -> Image {
    let (w, h) = (image.width as usize, image.height as usize);
    let (ow, oh) = (h, w);
    let mut pixels = vec![0u32; w * h];
    const TILE: usize = 32;
    for ty in (0..oh).step_by(TILE) {
        for tx in (0..ow).step_by(TILE) {
            for y in ty..(ty + TILE).min(oh) {
                let sx = if flip_v { w - 1 - y } else { y };
                let out_row = &mut pixels[y * ow..(y + 1) * ow];
                for (x, o) in out_row.iter_mut().enumerate().take((tx + TILE).min(ow)).skip(tx) {
                    let sy = if flip_h { h - 1 - x } else { x };
                    *o = image.pixels[sy * w + sx];
                }
            }
        }
    }
    Image { width: h as u32, height: w as u32, pixels }
}

/// Transforms stored pixels to the upright orientation described by an EXIF tag.
pub fn apply_orientation(image: &Image, orientation: Orientation) -> Image {
    match orientation {
        Orientation::Normal => image.clone(),
        Orientation::FlipHorizontal => flip_horizontal(image),
        Orientation::Rotate180 => rotate180(image),
        Orientation::FlipVertical => flip_vertical(image),
        Orientation::Transpose => transpose_flip(image, false, false),
        Orientation::Rotate90 => transpose_flip(image, true, false),
        Orientation::Transverse => transpose_flip(image, true, true),
        Orientation::Rotate270 => transpose_flip(image, false, true),
    }
}

/// Converts a straight-alpha pixel to premultiplied alpha (rounded).
#[inline]
pub fn premultiply_pixel(p: u32) -> u32 {
    let a = p >> 24;
    match a {
        255 => p,
        0 => 0,
        _ => {
            let m = |c: u32| {
                let t = c * a + 128;
                (t + (t >> 8)) >> 8
            };
            a << 24 | m((p >> 16) & 0xFF) << 16 | m((p >> 8) & 0xFF) << 8 | m(p & 0xFF)
        }
    }
}

/// Converts a premultiplied pixel back to straight alpha (rounded, clamped).
#[inline]
pub fn unpremultiply_pixel(p: u32) -> u32 {
    let a = p >> 24;
    match a {
        255 => p,
        0 => 0,
        _ => {
            let inv = ((255 << 16) + a / 2) / a;
            let u = |c: u32| ((c * inv + 32768) >> 16).min(255);
            a << 24 | u((p >> 16) & 0xFF) << 16 | u((p >> 8) & 0xFF) << 8 | u(p & 0xFF)
        }
    }
}

/// Converts every pixel of `image` to premultiplied alpha in place.
pub fn premultiply(image: &mut Image) {
    for p in &mut image.pixels {
        *p = premultiply_pixel(*p);
    }
}

/// Converts every pixel of `image` from premultiplied to straight alpha in place.
pub fn unpremultiply(image: &mut Image) {
    for p in &mut image.pixels {
        *p = unpremultiply_pixel(*p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_image(w: u32, h: u32) -> Image {
        Image::from_fn(w, h, |x, y| 0xFF00_0000 | (x * 255 / w.max(2)) << 16 | (y * 255 / h.max(2)) << 8 | 77)
    }

    #[test]
    fn weights_sum_to_one() {
        for filter in [Filter::Bilinear, Filter::Box, Filter::Lanczos3] {
            for (src, dst) in [(1, 7), (7, 1), (10, 3), (3, 10), (100, 37), (37, 100), (5, 5), (1000, 999)] {
                let w = compute_weights(src, dst, filter);
                for i in 0..dst {
                    let (start, taps) = w.spans[i];
                    assert!(start + taps <= src && taps >= 1);
                    let sum: i32 = w.weights[i * w.stride..i * w.stride + taps].iter().sum();
                    assert_eq!(sum, WEIGHT_ONE, "{filter:?} {src}->{dst} at {i}");
                }
            }
        }
    }

    #[test]
    fn constant_images_stay_constant() {
        for &color in &[0xFF12_3456u32, 0x8040_80C0, 0x0000_0000, 0x01FF_FFFF] {
            let img = Image::filled(13, 9, color);
            for filter in [Filter::Nearest, Filter::Bilinear, Filter::Box, Filter::Lanczos3] {
                for (w, h) in [(5, 4), (26, 18), (13, 30), (1, 1), (40, 3)] {
                    let out = resize(&img, w, h, filter);
                    assert_eq!((out.width, out.height), (w, h));
                    let want = if color >> 24 == 0 { 0 } else { color };
                    assert!(
                        out.pixels.iter().all(|&p| p == want),
                        "{filter:?} {color:#x} -> {w}x{h}: {:#x}",
                        out.pixels[0]
                    );
                }
            }
        }
    }

    #[test]
    fn transparent_pixels_do_not_bleed() {
        // Opaque red next to fully transparent green: no green fringe after scaling.
        let img = Image::from_fn(8, 8, |x, _| if x < 4 { 0xFFFF_0000 } else { 0x0000_FF00 });
        for filter in [Filter::Bilinear, Filter::Box, Filter::Lanczos3] {
            let out = resize(&img, 5, 5, filter);
            for &p in &out.pixels {
                if p >> 24 != 0 {
                    assert_eq!(p & 0x00FF_FFFF, 0x00FF_0000, "{filter:?}: {p:#x}");
                }
            }
        }
    }

    #[test]
    fn box_downscale_is_exact_average() {
        let img = Image::from_fn(4, 2, |x, y| 0xFF00_0000 | (x * 40 + y * 100));
        let out = resize(&img, 2, 1, Filter::Box);
        // Averages of 2x2 blocks: (0 + 40 + 100 + 140) / 4 = 70, (80 + 120 + 180 + 220) / 4 = 150.
        assert_eq!(out.pixels, [0xFF00_0046, 0xFF00_0096]);
        // Bilinear upscaling of a 2-pixel ramp interpolates.
        let ramp = Image::from_fn(2, 1, |x, _| 0xFF00_0000 | (x * 200));
        let up = resize(&ramp, 4, 1, Filter::Bilinear);
        assert_eq!(up.pixels.iter().map(|p| p & 0xFF).collect::<Vec<_>>(), [0, 50, 150, 200]);
    }

    #[test]
    fn up_and_down_preserve_content() {
        let img = test_image(64, 48);
        for filter in [Filter::Bilinear, Filter::Box, Filter::Lanczos3] {
            let small = resize(&img, 32, 24, filter);
            let big = resize(&small, 64, 48, filter);
            let mut max_err = 0;
            for (a, b) in img.pixels.iter().zip(&big.pixels) {
                for s in [0, 8, 16] {
                    max_err = max_err.max((((a >> s) & 0xFF) as i32 - ((b >> s) & 0xFF) as i32).abs());
                }
            }
            assert!(max_err <= 12, "{filter:?}: {max_err}");
        }
    }

    #[test]
    fn thumbnails() {
        assert_eq!(fit_size(4000, 3000, 256, 256), (256, 192));
        assert_eq!(fit_size(3000, 4000, 256, 256), (192, 256));
        assert_eq!(fit_size(100, 50, 256, 256), (100, 50));
        assert_eq!(fit_size(10000, 1, 100, 100), (100, 1));
        let img = test_image(1000, 600);
        let t = thumbnail(&img, 100, 100, Filter::Lanczos3);
        assert_eq!((t.width, t.height), (100, 60));
        let direct = resize(&img, 100, 60, Filter::Lanczos3);
        for (a, b) in t.pixels.iter().zip(&direct.pixels) {
            for s in [0, 8, 16] {
                assert!((((a >> s) & 0xFF) as i32 - ((b >> s) & 0xFF) as i32).abs() <= 3);
            }
        }
        assert_eq!(thumbnail(&img, 2000, 2000, Filter::Box), img);
    }

    #[test]
    fn geometry() {
        let img = Image::from_fn(3, 2, |x, y| y * 3 + x);
        assert_eq!(flip_horizontal(&img).pixels, [2, 1, 0, 5, 4, 3]);
        assert_eq!(flip_vertical(&img).pixels, [3, 4, 5, 0, 1, 2]);
        assert_eq!(rotate180(&img).pixels, [5, 4, 3, 2, 1, 0]);
        let r90 = rotate90(&img);
        assert_eq!((r90.width, r90.height), (2, 3));
        assert_eq!(r90.pixels, [3, 0, 4, 1, 5, 2]);
        let r270 = rotate270(&img);
        assert_eq!(r270.pixels, [2, 5, 1, 4, 0, 3]);
        assert_eq!(rotate90(&rotate90(&img)), rotate180(&img));
        assert_eq!(rotate270(&rotate90(&img)), img);
        assert_eq!(apply_orientation(&img, Orientation::Transpose).pixels, [0, 3, 1, 4, 2, 5]);
        assert_eq!(apply_orientation(&img, Orientation::Transverse).pixels, [5, 2, 4, 1, 3, 0]);
        // Large images exercise the tiling.
        let big = Image::from_fn(70, 45, |x, y| y * 1000 + x);
        let r = rotate90(&big);
        for y in 0..r.height {
            for x in 0..r.width {
                assert_eq!(r.get(x, y), big.get(y, big.height - 1 - x));
            }
        }
        assert_eq!(crop(&img, 1, 0, 5, 1).pixels, [1, 2]);
        assert_eq!(crop(&img, 1, 1, 1, 1).pixels, [4]);
        assert!(crop(&img, 3, 0, 2, 2).is_empty());
    }

    #[test]
    fn premultiplication() {
        assert_eq!(premultiply_pixel(0x80FF_8000), 0x8080_4000);
        assert_eq!(premultiply_pixel(0x00FF_FFFF), 0);
        assert_eq!(premultiply_pixel(0xFF12_3456), 0xFF12_3456);
        assert_eq!(unpremultiply_pixel(0x8080_4000), 0x80FF_8000);
        for a in 1..=255u32 {
            for c in [0u32, 1, 77, 128, 200, 255] {
                let p = a << 24 | c << 16 | c << 8 | c;
                let back = unpremultiply_pixel(premultiply_pixel(p));
                let err = ((back & 0xFF) as i32 - c as i32).abs();
                // Premultiplication loses precision at low alpha (only about `a` levels remain).
                assert!(err as u32 <= 255 / (2 * a) + 1, "a {a} c {c}: {back:#x}");
            }
        }
        let mut img = Image::filled(2, 2, 0x8040_2010);
        premultiply(&mut img);
        unpremultiply(&mut img);
        assert!(img.pixels.iter().all(|&p| p == 0x8040_2010));
    }
}
