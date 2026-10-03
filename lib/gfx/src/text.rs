//! Text rendering: a font collection plus a glyph cache, drawing onto a
//! [`Canvas`] with subpixel positioning.

use alloc::string::String;
use alloc::vec::Vec;

use vfont::{Font, FontCollection, GlyphCache, PositionedGlyph, subpixel_position};
use vraster::Point;

use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::Rect;

/// Horizontal alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// Vertical line metrics in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_height: f32,
}

/// Fonts plus a glyph cache.
pub struct Text {
    fonts: FontCollection<'static>,
    cache: GlyphCache,
    scratch: Vec<PositionedGlyph>,
}

impl Default for Text {
    fn default() -> Self {
        Self::new()
    }
}

impl Text {
    pub fn new() -> Text {
        Text { fonts: FontCollection::new(), cache: GlyphCache::new(4 << 20), scratch: Vec::new() }
    }

    /// Adds a font (the data must live for the rest of the program; leak a
    /// `Vec` with `Vec::leak` if needed). Returns its index. Fonts added
    /// later act as fallbacks for characters missing from earlier ones.
    pub fn add_font(&mut self, data: &'static [u8]) -> Option<usize> {
        let font = Font::from_bytes(data).ok()?;
        Some(self.fonts.add(font))
    }

    pub fn font_count(&self) -> usize {
        self.fonts.fonts().len()
    }

    pub fn font(&self, index: usize) -> Option<&Font<'static>> {
        self.fonts.font(index)
    }

    pub fn metrics(&self, font: usize, size: f32) -> LineMetrics {
        let s = self.fonts.scaled(font, size);
        LineMetrics { ascent: s.ascent(), descent: s.descent(), line_height: s.line_height() }
    }

    pub fn measure(&self, font: usize, size: f32, text: &str) -> f32 {
        self.fonts.scaled(font, size).measure(text)
    }

    /// Byte index of the character boundary nearest to `x` (caret placement).
    pub fn index_for_x(&self, font: usize, size: f32, text: &str, x: f32) -> usize {
        self.fonts.scaled(font, size).index_for_x(text, x)
    }

    /// Horizontal position of the caret before byte `index`.
    pub fn x_for_index(&self, font: usize, size: f32, text: &str, index: usize) -> f32 {
        self.fonts.scaled(font, size).x_for_index(text, index)
    }

    /// Line ranges after wrapping `text` to `max_width` pixels.
    pub fn wrap(&self, font: usize, size: f32, text: &str, max_width: f32) -> Vec<core::ops::Range<usize>> {
        self.fonts.scaled(font, size).wrap_lines(text, max_width)
    }

    /// Draws one line with its baseline at `y`, starting at `x`. Returns the
    /// advance width.
    pub fn draw(&mut self, c: &mut Canvas, font: usize, size: f32, x: f32, y: f32, text: &str, color: Color) -> f32 {
        let scaled = self.fonts.scaled(font, size);
        self.scratch.clear();
        let width = scaled.layout_line_into(text, Point::new(x, y), &mut self.scratch);
        let clip = c.clip_rect();
        for g in &self.scratch {
            let Some(f) = self.fonts.font(g.font as usize) else { continue };
            let (ix, bin) = subpixel_position(g.x);
            let iy = g.y as i32;
            // Skip glyphs that are obviously outside the clip.
            if iy - (size as i32 * 2) > clip.bottom() || iy + size as i32 * 2 < clip.y {
                continue;
            }
            let bm = self.cache.get(f, g.glyph, size, bin);
            if bm.width == 0 || bm.height == 0 {
                continue;
            }
            c.fill_mask(ix + bm.left, iy - bm.top, bm.width as i32, bm.height as i32, &bm.data, color);
        }
        width
    }

    /// Truncates `text` with an ellipsis so that it fits `max_width`.
    pub fn ellipsize(&self, font: usize, size: f32, text: &str, max_width: f32) -> String {
        if self.measure(font, size, text) <= max_width {
            return String::from(text);
        }
        let ell = "…";
        let budget = max_width - self.measure(font, size, ell);
        let mut end = self.index_for_x(font, size, text, budget.max(0.0));
        while end > 0 && self.measure(font, size, &text[..end]) > budget {
            end = text[..end].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        }
        let mut s = String::from(text[..end].trim_end());
        s.push_str(ell);
        s
    }

    /// Draws a single line inside `r`, vertically centred, aligned
    /// horizontally, truncated with an ellipsis if necessary.
    pub fn draw_in(&mut self, c: &mut Canvas, font: usize, size: f32, r: Rect, text: &str, color: Color, align: Align) {
        let m = self.metrics(font, size);
        let fitted = self.ellipsize(font, size, text, r.w as f32);
        let w = self.measure(font, size, &fitted);
        let x = match align {
            Align::Left => r.x as f32,
            Align::Center => r.x as f32 + (r.w as f32 - w) / 2.0,
            Align::Right => r.right() as f32 - w,
        };
        // `descent` is positive (distance below the baseline).
        let text_h = m.ascent + m.descent;
        let baseline = r.y as f32 + (r.h as f32 - text_h) / 2.0 + m.ascent;
        c.save();
        c.clip_to(r);
        self.draw(c, font, size, x.round_down(), baseline.round_down(), &fitted, color);
        c.restore();
    }

    /// Draws word-wrapped text from the top of `r`; returns the height used.
    pub fn draw_wrapped(
        &mut self,
        c: &mut Canvas,
        font: usize,
        size: f32,
        r: Rect,
        text: &str,
        color: Color,
        align: Align,
    ) -> i32 {
        let m = self.metrics(font, size);
        let lines = self.wrap(font, size, text, r.w as f32);
        let mut y = r.y as f32 + m.ascent;
        for range in lines {
            let line = text[range].trim_end();
            let w = self.measure(font, size, line);
            let x = match align {
                Align::Left => r.x as f32,
                Align::Center => r.x as f32 + (r.w as f32 - w) / 2.0,
                Align::Right => r.right() as f32 - w,
            };
            self.draw(c, font, size, x, y.round_down(), line, color);
            y += m.line_height;
        }
        (y - m.ascent - r.y as f32) as i32
    }
}

trait RoundDown {
    fn round_down(self) -> f32;
}

impl RoundDown for f32 {
    /// Snaps to whole pixels (keeps glyph stems crisp on baselines).
    fn round_down(self) -> f32 {
        vmath::FloatExt::floor(self + 0.5)
    }
}
