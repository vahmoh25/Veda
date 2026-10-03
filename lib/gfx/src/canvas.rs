//! [`Canvas`]: drawing operations on a borrowed pixel buffer.
//!
//! All coordinates are relative to the current origin ([`Canvas::translate`])
//! and clipped to the current clip rectangle ([`Canvas::clip_to`]); `save` /
//! `restore` bracket temporary changes.

use alloc::vec::Vec;

use vmath::FloatExt;
use vraster::{FillRule, Path, Rasterizer, Span, StrokeStyle, Transform};

use crate::bitmap::Bitmap;
use crate::color::{Color, over, scale};
use crate::geom::Rect;

/// Image sampling filter for scaled drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Nearest,
    Bilinear,
}

pub struct Canvas<'a> {
    pixels: &'a mut [u32],
    width: i32,
    height: i32,
    stride: i32,
    /// Clip in device coordinates.
    clip: Rect,
    /// Origin in device coordinates.
    ox: i32,
    oy: i32,
    stack: Vec<(Rect, i32, i32)>,
    raster: Option<Rasterizer>,
}

#[inline(always)]
fn blend_cov(dst: &mut u32, px: u32, cov: u32) {
    if cov >= 255 {
        *dst = over(px, *dst);
    } else if cov > 0 {
        *dst = over(scale(px, cov), *dst);
    }
}

/// Fills a run of pixels with a premultiplied color.
#[inline]
fn fill_run(row: &mut [u32], px: u32) {
    if px >> 24 == 255 {
        row.fill(px);
    } else if px != 0 {
        for d in row {
            *d = over(px, *d);
        }
    }
}

impl<'a> Canvas<'a> {
    /// Wraps a pixel buffer of `width` x `height` with `stride` pixels per row.
    pub fn new(pixels: &'a mut [u32], width: i32, height: i32, stride: i32) -> Canvas<'a> {
        assert!(stride >= width && pixels.len() >= (stride * height.max(0)) as usize);
        Canvas { pixels, width, height, stride, clip: Rect::new(0, 0, width, height), ox: 0, oy: 0, stack: Vec::new(), raster: None }
    }

    pub fn for_bitmap(b: &'a mut Bitmap) -> Canvas<'a> {
        let (w, h) = (b.width, b.height);
        Canvas::new(&mut b.pixels, w, h, w)
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// The full canvas in local coordinates.
    pub fn bounds(&self) -> Rect {
        Rect::new(-self.ox, -self.oy, self.width, self.height)
    }

    /// The current clip in local coordinates.
    pub fn clip_rect(&self) -> Rect {
        self.clip.translate(-self.ox, -self.oy)
    }

    pub fn save(&mut self) {
        self.stack.push((self.clip, self.ox, self.oy));
    }

    pub fn restore(&mut self) {
        if let Some((c, x, y)) = self.stack.pop() {
            self.clip = c;
            self.ox = x;
            self.oy = y;
        }
    }

    pub fn translate(&mut self, dx: i32, dy: i32) {
        self.ox += dx;
        self.oy += dy;
    }

    /// Restricts drawing to `r` (local coordinates) within the current clip.
    pub fn clip_to(&mut self, r: Rect) {
        self.clip = self.clip.intersect(&r.translate(self.ox, self.oy));
    }

    /// Device-space rectangle clipped to the current clip.
    fn device(&self, r: Rect) -> Rect {
        r.translate(self.ox, self.oy).intersect(&self.clip)
    }

    #[inline]
    fn row_mut(&mut self, y: i32) -> &mut [u32] {
        let start = (y * self.stride) as usize;
        &mut self.pixels[start..start + self.width as usize]
    }

    /// Raw access to the pixels (premultiplied), e.g. for custom effects.
    pub fn pixels_mut(&mut self) -> (&mut [u32], i32) {
        (self.pixels, self.stride)
    }

    pub fn clear(&mut self, color: Color) {
        let px = color.premul();
        let c = self.clip;
        for y in c.y..c.bottom() {
            self.row_mut(y)[c.x as usize..c.right() as usize].fill(px);
        }
    }

    /// Fills a rectangle (alpha-blended).
    pub fn fill_rect(&mut self, r: Rect, color: Color) {
        self.fill_rect_px(r, color.premul());
    }

    pub fn fill_rect_px(&mut self, r: Rect, px: u32) {
        let d = self.device(r);
        for y in d.y..d.bottom() {
            fill_run(&mut self.row_mut(y)[d.x as usize..d.right() as usize], px);
        }
    }

    /// Replaces pixels (no blending), e.g. to punch transparent holes.
    pub fn copy_rect(&mut self, r: Rect, color: Color) {
        let px = color.premul();
        let d = self.device(r);
        for y in d.y..d.bottom() {
            self.row_mut(y)[d.x as usize..d.right() as usize].fill(px);
        }
    }

    pub fn fill_vertical_gradient(&mut self, r: Rect, top: Color, bottom: Color) {
        let d = self.device(r);
        let ry = r.y + self.oy;
        for y in d.y..d.bottom() {
            let t = if r.h > 1 { (y - ry) as f32 / (r.h - 1) as f32 } else { 0.0 };
            let px = top.lerp(bottom, t).premul();
            fill_run(&mut self.row_mut(y)[d.x as usize..d.right() as usize], px);
        }
    }

    pub fn fill_horizontal_gradient(&mut self, r: Rect, left: Color, right: Color) {
        let d = self.device(r);
        if d.is_empty() {
            return;
        }
        let rx = r.x + self.ox;
        let cols: Vec<u32> = (d.x..d.right())
            .map(|x| {
                let t = if r.w > 1 { (x - rx) as f32 / (r.w - 1) as f32 } else { 0.0 };
                left.lerp(right, t).premul()
            })
            .collect();
        for y in d.y..d.bottom() {
            let row = &mut self.row_mut(y)[d.x as usize..d.right() as usize];
            for (dst, &px) in row.iter_mut().zip(&cols) {
                *dst = over(px, *dst);
            }
        }
    }

    /// Fills a rounded rectangle with exact anti-aliased corners, taking the
    /// color of each row from `row_color` (premultiplied).
    fn rounded_rect_rows(&mut self, r: Rect, radius: f32, mut row_color: impl FnMut(i32) -> u32) {
        let d = self.device(r);
        if d.is_empty() {
            return;
        }
        let rad = radius.min(r.w as f32 / 2.0).min(r.h as f32 / 2.0).max(0.0);
        let band = rad.ceil() as i32;
        let (rx0, ry0) = ((r.x + self.ox) as f32, (r.y + self.oy) as f32);
        let (rx1, ry1) = (rx0 + r.w as f32, ry0 + r.h as f32);
        let ry_i = r.y + self.oy;
        for y in d.y..d.bottom() {
            let px = row_color(y - ry_i);
            if px == 0 {
                continue;
            }
            let ly = y - ry_i;
            let in_band = ly < band || ly >= r.h - band;
            if !in_band || rad <= 0.0 {
                fill_run(&mut self.row_mut(y)[d.x as usize..d.right() as usize], px);
                continue;
            }
            let cy = if ly < band { ry0 + rad } else { ry1 - rad };
            let fy = y as f32 + 0.5 - cy;
            let x_left_end = (r.x + self.ox + band).min(d.right());
            let x_right_start = (r.right() + self.ox - band).max(d.x);
            let width = self.width;
            let row_start = (y * self.stride) as usize;
            let row = &mut self.pixels[row_start..row_start + width as usize];
            let cov_at = |x: i32, cx: f32| -> u32 {
                let fx = x as f32 + 0.5 - cx;
                let dist = (fx * fx + fy * fy).sqrt();
                ((rad - dist + 0.5).clamp(0.0, 1.0) * 255.0) as u32
            };
            for x in d.x..x_left_end {
                blend_cov(&mut row[x as usize], px, cov_at(x, rx0 + rad));
            }
            let mid0 = x_left_end.max(d.x);
            let mid1 = x_right_start.min(d.right());
            if mid1 > mid0 {
                fill_run(&mut row[mid0 as usize..mid1 as usize], px);
            }
            for x in x_right_start.max(x_left_end)..d.right() {
                blend_cov(&mut row[x as usize], px, cov_at(x, rx1 - rad));
            }
        }
    }

    pub fn fill_rounded_rect(&mut self, r: Rect, radius: f32, color: Color) {
        let px = color.premul();
        self.rounded_rect_rows(r, radius, |_| px);
    }

    pub fn fill_rounded_rect_gradient(&mut self, r: Rect, radius: f32, top: Color, bottom: Color) {
        let h = (r.h - 1).max(1) as f32;
        self.rounded_rect_rows(r, radius, |ly| top.lerp(bottom, ly as f32 / h).premul());
    }

    /// Outlines a rounded rectangle with a `width`-pixel line inside `r`.
    pub fn stroke_rounded_rect(&mut self, r: Rect, radius: f32, width: f32, color: Color) {
        let mut path = Path::new();
        let hw = width / 2.0;
        let rad = (radius - hw).max(0.0);
        path.rounded_rect(r.x as f32 + hw, r.y as f32 + hw, r.w as f32 - width, r.h as f32 - width, [rad; 4]);
        self.stroke_path(&path, &StrokeStyle::new(width), color);
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        let r = radius.ceil() as i32;
        let (icx, icy) = (cx.floor() as i32, cy.floor() as i32);
        let px = color.premul();
        let d = self.device(Rect::new(icx - r - 1, icy - r - 1, 2 * r + 3, 2 * r + 3));
        let (dcx, dcy) = (cx + self.ox as f32, cy + self.oy as f32);
        for y in d.y..d.bottom() {
            let fy = y as f32 + 0.5 - dcy;
            let start = (y * self.stride) as usize;
            for x in d.x..d.right() {
                let fx = x as f32 + 0.5 - dcx;
                let dist = (fx * fx + fy * fy).sqrt();
                let cov = ((radius - dist + 0.5).clamp(0.0, 1.0) * 255.0) as u32;
                blend_cov(&mut self.pixels[start + x as usize], px, cov);
            }
        }
    }

    /// Composites rasterizer spans with a solid color.
    fn blend_spans(pixels: &mut [u32], stride: i32, clip: Rect, ox: i32, oy: i32, px: u32, span: Span<'_>) {
        let y = span.y as i32 - oy;
        let _ = (ox, y);
        let dy = span.y as i32;
        if dy < clip.y || dy >= clip.bottom() {
            return;
        }
        let row = (dy * stride) as usize;
        let x0 = span.x as i32;
        for i in 0..span.len as i32 {
            let x = x0 + i;
            if x < clip.x || x >= clip.right() {
                continue;
            }
            let cov = span.coverage_at(i as u32) as u32;
            blend_cov(&mut pixels[row + x as usize], px, cov);
        }
    }

    /// Fills a vector path (local coordinates) with a solid color.
    pub fn fill_path(&mut self, path: &Path, color: Color, rule: FillRule) {
        self.fill_path_transformed(path, &Transform::IDENTITY, color, rule);
    }

    pub fn fill_path_transformed(&mut self, path: &Path, t: &Transform, color: Color, rule: FillRule) {
        let px = color.premul();
        let t = t.then_translate(self.ox as f32, self.oy as f32);
        let mut raster = self.raster.take().unwrap_or_default();
        let (pixels, stride, clip, ox, oy) = (&mut *self.pixels, self.stride, self.clip, self.ox, self.oy);
        raster.fill(path, &t, rule, (self.width as u32, self.height as u32), |span| {
            Self::blend_spans(pixels, stride, clip, ox, oy, px, span)
        });
        self.raster = Some(raster);
    }

    pub fn stroke_path(&mut self, path: &Path, style: &StrokeStyle, color: Color) {
        self.stroke_path_transformed(path, style, &Transform::IDENTITY, color);
    }

    pub fn stroke_path_transformed(&mut self, path: &Path, style: &StrokeStyle, t: &Transform, color: Color) {
        let px = color.premul();
        let t = t.then_translate(self.ox as f32, self.oy as f32);
        let mut raster = self.raster.take().unwrap_or_default();
        let (pixels, stride, clip, ox, oy) = (&mut *self.pixels, self.stride, self.clip, self.ox, self.oy);
        raster.stroke(path, style, &t, (self.width as u32, self.height as u32), |span| {
            Self::blend_spans(pixels, stride, clip, ox, oy, px, span)
        });
        self.raster = Some(raster);
    }

    pub fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Color) {
        let mut p = Path::new();
        p.move_to(x0, y0);
        p.line_to(x1, y1);
        self.stroke_path(&p, &StrokeStyle::new(width), color);
    }

    /// Blends an 8-bit coverage mask (`w` x `h`, row stride `w`) at (x, y)
    /// tinted with `color` (used for glyphs and icons).
    pub fn fill_mask(&mut self, x: i32, y: i32, w: i32, h: i32, mask: &[u8], color: Color) {
        let px = color.premul();
        let d = self.device(Rect::new(x, y, w, h));
        let (mx, my) = (x + self.ox, y + self.oy);
        for dy in d.y..d.bottom() {
            let mrow = ((dy - my) * w) as usize;
            let start = (dy * self.stride) as usize;
            for dx in d.x..d.right() {
                let cov = mask[mrow + (dx - mx) as usize] as u32;
                if cov != 0 {
                    blend_cov(&mut self.pixels[start + dx as usize], px, cov);
                }
            }
        }
    }

    /// Draws a bitmap at (x, y) with `opacity` (0..=255).
    pub fn draw_bitmap(&mut self, b: &Bitmap, x: i32, y: i32, opacity: u8) {
        self.draw_bitmap_region(b, b.rect(), x, y, opacity);
    }

    /// Draws the `src` part of a bitmap with its top-left at (x, y).
    pub fn draw_bitmap_region(&mut self, b: &Bitmap, src: Rect, x: i32, y: i32, opacity: u8) {
        let src = src.intersect(&b.rect());
        let d = self.device(Rect::new(x, y, src.w, src.h));
        let (dx0, dy0) = (x + self.ox, y + self.oy);
        let op = opacity as u32;
        for dy in d.y..d.bottom() {
            let sy = src.y + dy - dy0;
            let srow = (sy * b.width + src.x - dx0) as isize;
            let start = (dy * self.stride) as usize;
            let dst = &mut self.pixels[start + d.x as usize..start + d.right() as usize];
            let srcrow = &b.pixels[(srow + d.x as isize) as usize..(srow + d.right() as isize) as usize];
            if op == 255 {
                for (o, &s) in dst.iter_mut().zip(srcrow) {
                    *o = over(s, *o);
                }
            } else {
                for (o, &s) in dst.iter_mut().zip(srcrow) {
                    *o = over(scale(s, op), *o);
                }
            }
        }
    }

    /// Copies a bitmap region without blending (fast path for opaque images).
    pub fn blit(&mut self, b: &Bitmap, src: Rect, x: i32, y: i32) {
        let src = src.intersect(&b.rect());
        let d = self.device(Rect::new(x, y, src.w, src.h));
        let (dx0, dy0) = (x + self.ox, y + self.oy);
        for dy in d.y..d.bottom() {
            let sy = src.y + dy - dy0;
            let sx = src.x + d.x - dx0;
            let s = (sy * b.width + sx) as usize;
            let start = (dy * self.stride) as usize;
            self.pixels[start + d.x as usize..start + d.right() as usize]
                .copy_from_slice(&b.pixels[s..s + d.w as usize]);
        }
    }

    /// Draws a bitmap scaled into `dst`.
    pub fn draw_bitmap_scaled(&mut self, b: &Bitmap, dst: Rect, filter: Filter, opacity: u8) {
        if b.width == 0 || b.height == 0 || dst.is_empty() {
            return;
        }
        let d = self.device(dst);
        let (dx0, dy0) = (dst.x + self.ox, dst.y + self.oy);
        let sx = b.width as f32 / dst.w as f32;
        let sy = b.height as f32 / dst.h as f32;
        let op = opacity as u32;
        for y in d.y..d.bottom() {
            let fy = (y - dy0) as f32 + 0.5;
            let start = (y * self.stride) as usize;
            for x in d.x..d.right() {
                let fx = (x - dx0) as f32 + 0.5;
                let s = match filter {
                    Filter::Nearest => b.get((fx * sx) as i32, (fy * sy) as i32),
                    Filter::Bilinear => b.sample_bilinear(fx * sx, fy * sy),
                };
                let s = if op == 255 { s } else { scale(s, op) };
                let p = &mut self.pixels[start + x as usize];
                *p = over(s, *p);
            }
        }
    }

    /// Multiplies the alpha of everything inside `r` by `opacity`.
    pub fn fade_rect(&mut self, r: Rect, opacity: u8) {
        let d = self.device(r);
        for y in d.y..d.bottom() {
            for p in &mut self.row_mut(y)[d.x as usize..d.right() as usize] {
                *p = scale(*p, opacity as u32);
            }
        }
    }
}
