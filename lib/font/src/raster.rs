//! Glyph rasterization: outlines to anti-aliased coverage bitmaps.
//!
//! Glyphs are rendered unhinted with exact-area anti-aliasing (`vraster`, non-zero rule) at
//! fractional horizontal positions. Two optional adjustments improve small sizes:
//!
//! * **Stem darkening** emboldens outlines by a fraction of a pixel at small sizes (fading out
//!   towards larger sizes), which counteracts the washed-out look of unhinted text.
//! * **Gamma** remaps coverage (`c^(1/gamma)`); values above 1 make anti-aliased edges darker.

use alloc::vec::Vec;

use vraster::{Coverage, FillRule, Path, Point, Rasterizer, Transform, math};

use crate::GlyphId;
use crate::font::{Font, OutlineScratch};
use crate::outline::PathSink;

/// Largest supported glyph size in pixels per em.
pub const MAX_GLYPH_SIZE: f32 = 2048.0;

/// Largest bitmap width or height produced (larger outlines yield an empty bitmap).
const MAX_BITMAP_DIM: i32 = 8192;

/// A rasterized glyph: an 8-bit coverage bitmap plus its placement relative to the glyph origin.
///
/// To draw a glyph whose origin (pen position on the baseline) is at integer pixel `(ox, oy)`,
/// copy/blend the bitmap with its top-left corner at `(ox + left, oy - top)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlyphBitmap {
    /// Width in pixels (= row stride of `data`).
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Horizontal offset of the bitmap's left edge from the glyph origin, in pixels.
    pub left: i32,
    /// Distance from the baseline up to the bitmap's top edge, in pixels.
    pub top: i32,
    /// Coverage values (0..=255), `width * height` bytes, top row first.
    pub data: Vec<u8>,
}

impl GlyphBitmap {
    /// Returns `true` if the bitmap has no pixels (e.g. space).
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Coverage at `(x, y)` (0 outside).
    pub fn get(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.data.get((y * self.width + x) as usize).copied().unwrap_or(0)
    }

    /// One row of coverage values.
    pub fn row(&self, y: u32) -> &[u8] {
        let w = self.width as usize;
        let s = y as usize * w;
        if y >= self.height {
            return &[];
        }
        self.data.get(s..s + w).unwrap_or(&[])
    }
}

/// Glyph rendering options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterOptions {
    /// Stem darkening: extra stem thickness in pixels applied at sizes up to
    /// `darkening_full_size` (0 disables darkening).
    pub darkening: f32,
    /// Sizes (pixels per em) at or below which the full `darkening` applies.
    pub darkening_full_size: f32,
    /// Sizes at or above which no darkening applies (linear fade in between).
    pub darkening_zero_size: f32,
    /// Coverage gamma: output = coverage^(1/gamma). 1 = linear coverage.
    pub gamma: f32,
    /// Curve flattening tolerance in pixels.
    pub tolerance: f32,
}

impl RasterOptions {
    /// Plain linear coverage: no darkening, no gamma.
    pub const LINEAR: RasterOptions =
        RasterOptions { darkening: 0.0, darkening_full_size: 0.0, darkening_zero_size: 0.0, gamma: 1.0, tolerance: 0.05 };

    /// The stem darkening amount (pixels) at a given size.
    pub fn darkening_at(&self, size_px: f32) -> f32 {
        if !(self.darkening > 0.0) || !(size_px < self.darkening_zero_size) {
            return 0.0;
        }
        if size_px <= self.darkening_full_size {
            return self.darkening;
        }
        let range = self.darkening_zero_size - self.darkening_full_size;
        if !(range > 0.0) {
            return 0.0;
        }
        self.darkening * (self.darkening_zero_size - size_px) / range
    }
}

impl Default for RasterOptions {
    /// Mild stem darkening for small sizes (0.3 px up to 12 px, fading out at 28 px), linear
    /// coverage.
    fn default() -> Self {
        RasterOptions { darkening: 0.3, darkening_full_size: 12.0, darkening_zero_size: 28.0, gamma: 1.0, tolerance: 0.05 }
    }
}

/// A reusable glyph rasterizer (keeps its path, scratch and rasterizer buffers between calls).
pub struct GlyphRasterizer {
    raster: Rasterizer,
    path: Path,
    scratch: OutlineScratch,
    emb: Vec<Point>,
    options: RasterOptions,
    lut: [u8; 256],
    linear: bool,
}

impl Default for GlyphRasterizer {
    fn default() -> Self {
        GlyphRasterizer::new()
    }
}

impl GlyphRasterizer {
    /// Creates a rasterizer with [`RasterOptions::default`].
    pub fn new() -> Self {
        GlyphRasterizer::with_options(RasterOptions::default())
    }

    /// Creates a rasterizer with the given options.
    pub fn with_options(options: RasterOptions) -> Self {
        let mut r = GlyphRasterizer {
            raster: Rasterizer::new(),
            path: Path::new(),
            scratch: OutlineScratch::default(),
            emb: Vec::new(),
            options,
            lut: [0; 256],
            linear: true,
        };
        r.set_options(options);
        r
    }

    /// The current options.
    pub fn options(&self) -> &RasterOptions {
        &self.options
    }

    /// Changes the options.
    pub fn set_options(&mut self, options: RasterOptions) {
        self.options = options;
        let g = if options.gamma > 0.0 && options.gamma.is_finite() { options.gamma } else { 1.0 };
        self.linear = (g - 1.0).abs() < 1e-4;
        for (i, v) in self.lut.iter_mut().enumerate() {
            *v = if self.linear {
                i as u8
            } else {
                math::round(255.0 * math::powf(i as f32 / 255.0, 1.0 / g)).clamp(0.0, 255.0) as u8
            };
        }
        let tol = if options.tolerance > 0.0 { options.tolerance } else { 0.05 };
        self.raster.set_tolerance(tol);
    }

    /// Rasterizes `glyph` at `size_px` pixels per em, shifted right by `subpixel_x` pixels
    /// (normally in `[0, 1)`). Empty or malformed glyphs give an empty bitmap.
    pub fn rasterize(&mut self, font: &Font<'_>, glyph: GlyphId, size_px: f32, subpixel_x: f32) -> GlyphBitmap {
        let mut bitmap = GlyphBitmap::default();
        self.rasterize_into(font, glyph, size_px, subpixel_x, &mut bitmap);
        bitmap
    }

    /// Like [`GlyphRasterizer::rasterize`], writing into `out` (its buffer is reused).
    pub fn rasterize_into(
        &mut self,
        font: &Font<'_>,
        glyph: GlyphId,
        size_px: f32,
        subpixel_x: f32,
        out: &mut GlyphBitmap,
    ) {
        out.width = 0;
        out.height = 0;
        out.left = 0;
        out.top = 0;
        out.data.clear();
        if !(size_px > 0.0) {
            return;
        }
        let size = size_px.min(MAX_GLYPH_SIZE);
        let scale = size * font.inv_upem();
        let dx = if subpixel_x.is_finite() { subpixel_x.clamp(-4.0, 4.0) } else { 0.0 };
        self.path.clear();
        let mut sink = PathSink::new(&mut self.path, scale, Point::new(dx, 0.0));
        if font.outline_with(glyph, &mut sink, &mut self.scratch).is_err() {
            self.path.clear();
            return;
        }
        let dark = self.options.darkening_at(size);
        if dark > 0.0 {
            self.path.embolden_with(dark, dark, &mut self.emb);
        }
        let Some(b) = self.path.bounds() else { return };
        if !(b.x0.is_finite() && b.y0.is_finite() && b.x1.is_finite() && b.y1.is_finite()) {
            return;
        }
        let x0 = math::floor(b.x0).max(-1e6) as i32;
        let y0 = math::floor(b.y0).max(-1e6) as i32;
        let x1 = math::ceil(b.x1).min(1e6) as i32;
        let y1 = math::ceil(b.y1).min(1e6) as i32;
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0 || h <= 0 || w > MAX_BITMAP_DIM || h > MAX_BITMAP_DIM {
            return;
        }
        let stride = w as usize;
        out.data.resize(stride * h as usize, 0);
        let data = &mut out.data;
        let t = Transform::translate(-x0 as f32, -y0 as f32);
        self.raster.fill(&self.path, &t, FillRule::NonZero, (w as u32, h as u32), |span| {
            let start = span.y as usize * stride + span.x as usize;
            match span.coverage {
                Coverage::Solid(v) => {
                    if let Some(s) = data.get_mut(start..start + span.len as usize) {
                        s.fill(v);
                    }
                }
                Coverage::Mask(m) => {
                    if let Some(s) = data.get_mut(start..start + m.len()) {
                        s.copy_from_slice(m);
                    }
                }
            }
        });
        if !self.linear {
            for v in out.data.iter_mut() {
                *v = self.lut[*v as usize];
            }
        }
        out.width = w as u32;
        out.height = h as u32;
        out.left = x0;
        out.top = -y0;
    }
}

/// Rasterizes `glyph` of `font` at `size_px` pixels per em with a horizontal subpixel offset,
/// using [`RasterOptions::default`]. Creates a temporary [`GlyphRasterizer`]; reuse one (or a
/// [`GlyphCache`](crate::GlyphCache)) when rendering many glyphs.
pub fn rasterize_glyph(font: &Font<'_>, glyph: GlyphId, size_px: f32, subpixel_x: f32) -> GlyphBitmap {
    GlyphRasterizer::new().rasterize(font, glyph, size_px, subpixel_x)
}
