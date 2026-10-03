//! Soft drop shadows, rendered as cached nine-slice images.
//!
//! A shadow is the blurred silhouette of a rounded rectangle. Its corners
//! are rendered once per (radius, blur) pair into a small alpha mask; along
//! the edges the shadow is constant, so drawing a shadow of any size costs
//! four corner blits plus a few row/column fills — cheap enough to redo
//! every frame.

use alloc::vec::Vec;

use vmath::FloatExt;

use crate::bitmap::Bitmap;
use crate::canvas::Canvas;
use crate::color::{Color, scale};
use crate::geom::Rect;

/// Smooth falloff approximating a Gaussian-blurred edge: 1 well inside,
/// 0.5 at the edge, 0 at `blur` pixels outside.
fn falloff(signed_distance: f32, blur: f32) -> f32 {
    if blur <= 0.0 {
        return if signed_distance <= 0.0 { 1.0 } else { 0.0 };
    }
    let t = (0.5 - signed_distance / (2.0 * blur)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Pre-rendered shadow pieces for one (corner radius, blur) pair.
pub struct ShadowTemplate {
    radius: i32,
    blur: i32,
    /// `size` x `size` alpha mask of the top-left corner of the shadow.
    corner: Vec<u8>,
    /// Alpha across an edge, from the outer border inwards (`size` values).
    edge: Vec<u8>,
    size: i32,
}

impl ShadowTemplate {
    pub fn new(radius: i32, blur: i32) -> ShadowTemplate {
        let (radius, blur) = (radius.max(0), blur.max(0));
        // The template reaches `blur` pixels inside the shape, where the
        // shadow becomes fully opaque.
        let size = (radius + 2 * blur).max(1);
        let (r, b) = (radius as f32, blur as f32);
        let c = (blur + radius) as f32; // corner circle centre, both axes
        let mut corner = alloc::vec![0u8; (size * size) as usize];
        for y in 0..size {
            for x in 0..size {
                // Rounded-rectangle signed distance in the corner quadrant.
                let qx = c - (x as f32 + 0.5);
                let qy = c - (y as f32 + 0.5);
                let (mx, my) = (qx.max(0.0), qy.max(0.0));
                let sd = (mx * mx + my * my).sqrt() + qx.max(qy).min(0.0) - r;
                corner[(y * size + x) as usize] = (falloff(sd, b) * 255.0 + 0.5) as u8;
            }
        }
        let edge = (0..size).map(|i| (falloff(b - (i as f32 + 0.5), b) * 255.0 + 0.5) as u8).collect();
        ShadowTemplate { radius, blur, corner, edge, size }
    }

    pub fn radius(&self) -> i32 {
        self.radius
    }

    pub fn blur(&self) -> i32 {
        self.blur
    }

    /// Draws the shadow of the rounded rectangle `r` (it extends `blur`
    /// pixels beyond `r`). With `fill_interior` false the part well inside
    /// `r` is skipped, which is correct under an opaque window.
    pub fn draw(&self, c: &mut Canvas, r: Rect, color: Color, fill_interior: bool) {
        let px = color.premul();
        let outer = r.inflate(self.blur);
        let s = self.size.min(outer.w / 2).min(outer.h / 2);
        if s <= 0 {
            return;
        }
        let (x0, y0, x1, y1) = (outer.x, outer.y, outer.right(), outer.bottom());
        let stride = self.size as usize;
        // Corners (mirrored).
        for y in 0..s {
            for x in 0..s {
                let a = self.corner[y as usize * stride + x as usize] as u32;
                if a == 0 {
                    continue;
                }
                c.blend_pixel(x0 + x, y0 + y, px, a);
                c.blend_pixel(x1 - 1 - x, y0 + y, px, a);
                c.blend_pixel(x0 + x, y1 - 1 - y, px, a);
                c.blend_pixel(x1 - 1 - x, y1 - 1 - y, px, a);
            }
        }
        // Edges between the corners.
        let (inner_w, inner_h) = (outer.w - 2 * s, outer.h - 2 * s);
        for i in 0..s {
            let a = self.edge[i as usize] as u32;
            if a == 0 {
                continue;
            }
            let p = scale(px, a);
            c.fill_rect_px(Rect::new(x0 + s, y0 + i, inner_w, 1), p);
            c.fill_rect_px(Rect::new(x0 + s, y1 - 1 - i, inner_w, 1), p);
            c.fill_rect_px(Rect::new(x0 + i, y0 + s, 1, inner_h), p);
            c.fill_rect_px(Rect::new(x1 - 1 - i, y0 + s, 1, inner_h), p);
        }
        if fill_interior {
            c.fill_rect_px(Rect::new(x0 + s, y0 + s, inner_w, inner_h), px);
        }
    }
}

impl Canvas<'_> {
    /// Blends one premultiplied pixel with coverage `cov` at local (x, y).
    #[inline]
    pub fn blend_pixel(&mut self, x: i32, y: i32, px: u32, cov: u32) {
        let b = self.bounds();
        if !self.clip_rect().contains(x, y) {
            return;
        }
        let (dx, dy) = (x - b.x, y - b.y);
        let (pixels, stride) = self.pixels_mut();
        let p = &mut pixels[(dy * stride + dx) as usize];
        *p = crate::color::over(if cov >= 255 { px } else { scale(px, cov) }, *p);
    }

    /// Draws a soft shadow, building the template on the fly (cache a
    /// [`ShadowTemplate`] when drawing many shadows).
    pub fn draw_shadow(&mut self, r: Rect, radius: i32, blur: i32, color: Color) {
        ShadowTemplate::new(radius, blur).draw(self, r, color, true);
    }
}

/// A blurred alpha silhouette of `src` (for shadows under icons and text).
pub fn silhouette_shadow(src: &Bitmap, blur: i32) -> Bitmap {
    let pad = blur * 2;
    let mut b = Bitmap::new(src.width + 2 * pad, src.height + 2 * pad);
    for y in 0..src.height {
        for x in 0..src.width {
            let a = src.get(x, y) >> 24;
            b.pixels[((y + pad) * b.width + x + pad) as usize] = a << 24;
        }
    }
    b.blur(blur.max(1) / 2 + 1, 3);
    b
}
