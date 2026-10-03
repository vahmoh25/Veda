//! Drawing pictures quickly at any zoom.
//!
//! [`render`] scales (and rotates by quarter turns) a picture with fixed-point arithmetic only,
//! from the mip level closest to the screen resolution, so even bilinear filtering stays cheap.
//! [`ViewCache`] keeps the result: most frames only redraw the chrome around the picture and
//! copy the cached pixels. While the user zooms or pans, a fast nearest-neighbour draft is
//! shown and refined once the view has settled.

use vgfx::color::{lerp_px, over};
use vgfx::{Bitmap, Canvas, Rect};

/// Rendering quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Nearest neighbour (while zooming or panning).
    Draft,
    /// Bilinear (smooth), except at large magnifications where pixels are shown crisp.
    Fine,
}

/// Where a picture appears in the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Top-left corner of the (rotated) picture, in viewport pixels.
    pub x: f32,
    pub y: f32,
    /// Viewport pixels per picture pixel.
    pub scale: f32,
    /// Clockwise quarter turns (0..=3).
    pub rot: u8,
}

impl Placement {
    /// Size of the (rotated) picture on screen for a `w` x `h` picture.
    pub fn size(&self, w: u32, h: u32) -> (f32, f32) {
        let (rw, rh) = rotated(w, h, self.rot);
        (rw as f32 * self.scale, rh as f32 * self.scale)
    }

    /// The pixels covered on screen, clipped to `viewport` (`0, 0, vw, vh`).
    pub fn visible(&self, w: u32, h: u32, vw: i32, vh: i32) -> Rect {
        let (sw, sh) = self.size(w, h);
        let x0 = floor(self.x).max(0);
        let y0 = floor(self.y).max(0);
        let x1 = ceil(self.x + sw).min(vw);
        let y1 = ceil(self.y + sh).min(vh);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// Size of a `w` x `h` picture after `rot` quarter turns.
pub fn rotated(w: u32, h: u32, rot: u8) -> (u32, u32) {
    if rot & 1 == 1 { (h, w) } else { (w, h) }
}

fn floor(v: f32) -> i32 {
    let i = v as i32;
    if (i as f32) > v { i - 1 } else { i }
}

fn ceil(v: f32) -> i32 {
    let i = v as i32;
    if (i as f32) < v { i + 1 } else { i }
}

/// Checkerboard behind transparent pictures (screen-aligned 8 px squares).
const CHECK_LIGHT: u32 = 0xFF3C_3C44;
const CHECK_DARK: u32 = 0xFF2E_2E35;

/// Renders the part `dst` (viewport pixels) of a picture into `out` (`dst.w * dst.h` pixels).
///
/// `levels` is a mip chain (largest first) of a picture that is `w` x `h` pixels at full size;
/// the levels may be smaller than that (e.g. a preview alone). Transparent pixels are composited
/// over a checkerboard when `checker` is set.
pub fn render(
    levels: &[Bitmap],
    w: u32,
    h: u32,
    p: &Placement,
    dst: Rect,
    quality: Quality,
    checker: bool,
    out: &mut [u32],
) {
    if levels.is_empty() || dst.is_empty() || w == 0 || h == 0 {
        return;
    }
    // The smallest level that still has at least one pixel per screen pixel.
    let mut level = &levels[0];
    for l in levels.iter().rev() {
        if l.width > 0 && p.scale * (w as f32 / l.width as f32) <= 1.001 {
            level = l;
            break;
        }
    }
    let (lw, lh) = (level.width, level.height);
    if lw == 0 || lh == 0 {
        return;
    }
    let kx = lw as f32 / w as f32;
    let ky = lh as f32 / h as f32;
    let inv = 1.0 / p.scale;
    // Screen pixels per level pixel: smooth when shrinking or enlarging moderately, crisp pixels
    // when zoomed far in.
    let magnification = p.scale / kx;
    let bilinear = quality == Quality::Fine && magnification < 4.0;
    let (wf, hf) = (w as f32, h as f32);
    let src = &level.pixels;
    let (lw1, lh1) = (lw - 1, lh - 1);
    for row in 0..dst.h {
        let y = dst.y + row;
        let u0 = (dst.x as f32 + 0.5 - p.x) * inv;
        let v = (y as f32 + 0.5 - p.y) * inv;
        // Picture coordinates (full size, unrotated) of the row's first pixel, and their change
        // per screen pixel to the right.
        let (sx, sy, dsx, dsy) = match p.rot & 3 {
            0 => (u0, v, inv, 0.0),
            1 => (v, hf - u0, 0.0, -inv),
            2 => (wf - u0, hf - v, -inv, 0.0),
            _ => (wf - v, u0, 0.0, inv),
        };
        // 16.16 fixed point in level coordinates.
        let mut fx = (sx * kx * 65536.0) as i32;
        let mut fy = (sy * ky * 65536.0) as i32;
        let dfx = (dsx * kx * 65536.0) as i32;
        let dfy = (dsy * ky * 65536.0) as i32;
        let line = &mut out[(row * dst.w) as usize..((row + 1) * dst.w) as usize];
        if bilinear {
            for o in line.iter_mut() {
                let (bx, by) = (fx - 0x8000, fy - 0x8000);
                let (x0, y0) = (bx >> 16, by >> 16);
                let (tx, ty) = (((bx >> 8) & 0xFF) as u32, ((by >> 8) & 0xFF) as u32);
                let (xa, xb) = (x0.clamp(0, lw1) as usize, (x0 + 1).clamp(0, lw1) as usize);
                let (ra, rb) = (y0.clamp(0, lh1) as usize * lw as usize, (y0 + 1).clamp(0, lh1) as usize * lw as usize);
                let top = lerp_px(src[ra + xa], src[ra + xb], tx);
                let bottom = lerp_px(src[rb + xa], src[rb + xb], tx);
                *o = lerp_px(top, bottom, ty);
                fx += dfx;
                fy += dfy;
            }
        } else {
            for o in line.iter_mut() {
                let x = (fx >> 16).clamp(0, lw1) as usize;
                let y = (fy >> 16).clamp(0, lh1) as usize;
                *o = src[y * lw as usize + x];
                fx += dfx;
                fy += dfy;
            }
        }
        if checker {
            for (i, o) in line.iter_mut().enumerate() {
                if *o >> 24 != 0xFF {
                    let x = dst.x + i as i32;
                    let bg = if ((x >> 3) ^ (y >> 3)) & 1 == 0 { CHECK_LIGHT } else { CHECK_DARK };
                    *o = over(*o, bg);
                }
            }
        }
    }
}

/// Draws a thumbnail that is opaque except for its rounded corners: the `radius` rows at the top
/// and bottom are blended, the rows between are copied.
pub fn draw_rounded(canvas: &mut Canvas, b: &Bitmap, x: i32, y: i32, radius: i32, opacity: u8) {
    if opacity != 255 {
        canvas.draw_bitmap(b, x, y, opacity);
        return;
    }
    let r = radius.clamp(0, b.height / 2);
    canvas.draw_bitmap_region(b, Rect::new(0, 0, b.width, r), x, y, 255);
    canvas.blit(b, Rect::new(0, r, b.width, b.height - 2 * r), x, y + r);
    canvas.draw_bitmap_region(b, Rect::new(0, b.height - r, b.width, r), x, y + b.height - r, 255);
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Key {
    picture: usize,
    levels: usize,
    x: i32,
    y: i32,
    scale: u32,
    rot: u8,
    dst: Rect,
}

/// The scaled view of the picture on screen.
pub struct ViewCache {
    key: Option<Key>,
    quality: Quality,
    rect: Rect,
    bitmap: Bitmap,
}

impl ViewCache {
    pub fn new() -> ViewCache {
        ViewCache { key: None, quality: Quality::Draft, rect: Rect::default(), bitmap: Bitmap::new(0, 0) }
    }

    /// Makes the cache show `dst` (viewport pixels) of a picture placed by `p` in at least
    /// `quality`. `picture` identifies the pixels (it changes when they do). Returns `true` if it
    /// had to render.
    pub fn update(
        &mut self,
        picture: usize,
        levels: &[Bitmap],
        (w, h): (u32, u32),
        p: &Placement,
        dst: Rect,
        quality: Quality,
        checker: bool,
    ) -> bool {
        let key = Key {
            picture,
            levels: levels.len(),
            x: (p.x * 16.0) as i32,
            y: (p.y * 16.0) as i32,
            scale: p.scale.to_bits(),
            rot: p.rot,
            dst,
        };
        if self.key == Some(key) && (self.quality == Quality::Fine || quality == Quality::Draft) {
            return false;
        }
        if self.bitmap.width != dst.w || self.bitmap.height != dst.h {
            self.bitmap = Bitmap::new(dst.w, dst.h);
        }
        render(levels, w, h, p, dst, quality, checker, &mut self.bitmap.pixels);
        self.key = Some(key);
        self.quality = quality;
        self.rect = dst;
        true
    }

    /// The cached view is a draft that should be refined.
    pub fn is_draft(&self) -> bool {
        self.key.is_some() && self.quality == Quality::Draft
    }

    /// Copies the cached view to the canvas; the viewport's top-left is at `origin`.
    pub fn draw(&self, canvas: &mut Canvas, origin: (i32, i32)) {
        if self.key.is_some() && !self.rect.is_empty() {
            canvas.blit(&self.bitmap, self.bitmap.rect(), origin.0 + self.rect.x, origin.1 + self.rect.y);
        }
    }

    /// Forgets the cached view (and its memory).
    pub fn clear(&mut self) {
        self.key = None;
        self.bitmap = Bitmap::new(0, 0);
    }
}
