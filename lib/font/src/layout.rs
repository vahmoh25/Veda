//! Simple text layout: font fallback, pixel metrics, measuring, line layout, word wrapping and
//! caret hit-testing.
//!
//! Layout is per character (no shaping): every character maps to one glyph through the fallback
//! chain, glyphs advance by their `hmtx` advance plus pair kerning (only between glyphs of the same
//! font), tabs advance to the next multiple of four spaces and control/format characters are
//! invisible with zero width.

use alloc::vec::Vec;
use core::ops::Range;

use vraster::{Point, math};

use crate::GlyphId;
use crate::font::Font;

/// Tab stops are this many space advances apart.
const TAB_SPACES: f32 = 4.0;

/// A list of fonts used as a fallback chain (see [`FontCollection::resolve`]).
#[derive(Clone, Debug, Default)]
pub struct FontCollection<'a> {
    fonts: Vec<Font<'a>>,
}

impl<'a> FontCollection<'a> {
    /// Creates an empty collection.
    pub fn new() -> Self {
        FontCollection { fonts: Vec::new() }
    }

    /// Adds a font and returns its index. The order of addition is the fallback order.
    pub fn add(&mut self, font: Font<'a>) -> usize {
        self.fonts.push(font);
        self.fonts.len() - 1
    }

    /// Number of fonts.
    pub fn len(&self) -> usize {
        self.fonts.len()
    }

    /// Returns `true` if the collection has no fonts.
    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    /// The font at `index`.
    pub fn font(&self, index: usize) -> Option<&Font<'a>> {
        self.fonts.get(index)
    }

    /// All fonts.
    pub fn fonts(&self) -> &[Font<'a>] {
        &self.fonts
    }

    /// Finds a glyph for `c`: first in font `primary`, then in the other fonts in order. Falls back
    /// to `.notdef` (glyph 0) of the primary font. Returns `(font index, glyph)`.
    pub fn resolve(&self, primary: usize, c: char) -> (usize, GlyphId) {
        let primary = if primary < self.fonts.len() { primary } else { 0 };
        if let Some(f) = self.fonts.get(primary)
            && let Some(g) = f.glyph_index(c)
        {
            return (primary, g);
        }
        for (i, f) in self.fonts.iter().enumerate() {
            if i != primary
                && let Some(g) = f.glyph_index(c)
            {
                return (i, g);
            }
        }
        (primary, GlyphId::NOTDEF)
    }

    /// A text style: font `primary` (with the rest of the collection as fallback) at `size_px`
    /// pixels per em.
    pub fn scaled(&self, primary: usize, size_px: f32) -> ScaledFont<'_, 'a> {
        ScaledFont::new(self, primary, size_px)
    }
}

/// A glyph positioned by [`ScaledFont::layout_line`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    /// Glyph id within `font`.
    pub glyph: GlyphId,
    /// Index of the font in the [`FontCollection`].
    pub font: u16,
    /// Horizontal position of the glyph origin in pixels (fractional; see
    /// [`subpixel_position`](crate::subpixel_position)).
    pub x: f32,
    /// Baseline position in pixels.
    pub y: f32,
    /// Byte offset of the source character in the laid-out string.
    pub cluster: u32,
}

/// One step of the layout walk.
#[derive(Clone, Copy)]
struct Step {
    /// Byte index of the character.
    index: usize,
    /// The glyph, or `None` for invisible characters (controls, tabs, whitespace).
    glyph: Option<(usize, GlyphId)>,
    /// Pen position of the character (after kerning).
    x: f32,
    /// Advance.
    advance: f32,
}

/// A [`FontCollection`] font at a pixel size: metrics in pixels, measuring and layout.
#[derive(Clone, Copy, Debug)]
pub struct ScaledFont<'c, 'a> {
    coll: &'c FontCollection<'a>,
    primary: usize,
    size: f32,
    kerning: bool,
}

/// Characters drawn with zero width and no glyph.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\0'..='\u{1F}' | '\u{7F}'..='\u{9F}' | '\u{AD}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}')
        && c != '\t'
}

/// Whitespace at which lines may be broken (no-break spaces excluded).
fn is_break_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{1680}' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200B}' | '\u{205F}' | '\u{3000}')
}

impl<'c, 'a> ScaledFont<'c, 'a> {
    /// Creates a text style. An out-of-range `primary` uses font 0; non-positive or non-finite
    /// sizes are treated as 0.
    pub fn new(collection: &'c FontCollection<'a>, primary: usize, size_px: f32) -> Self {
        let primary = if primary < collection.len() { primary } else { 0 };
        let size = if size_px > 0.0 && size_px.is_finite() { size_px.min(4096.0) } else { 0.0 };
        ScaledFont { coll: collection, primary, size, kerning: true }
    }

    /// The same style at another size.
    pub fn with_size(&self, size_px: f32) -> Self {
        ScaledFont { kerning: self.kerning, ..ScaledFont::new(self.coll, self.primary, size_px) }
    }

    /// The same style with pair kerning enabled or disabled (enabled by default).
    pub fn with_kerning(&self, enabled: bool) -> Self {
        ScaledFont { kerning: enabled, ..*self }
    }

    /// Returns `true` if pair kerning is applied.
    pub fn kerning_enabled(&self) -> bool {
        self.kerning
    }

    /// The font collection.
    pub fn collection(&self) -> &'c FontCollection<'a> {
        self.coll
    }

    /// Index of the primary font.
    pub fn primary(&self) -> usize {
        self.primary
    }

    /// The primary font (`None` only for an empty collection).
    pub fn font(&self) -> Option<&'c Font<'a>> {
        self.coll.font(self.primary)
    }

    /// The size in pixels per em.
    pub fn size_px(&self) -> f32 {
        self.size
    }

    /// Pixels per font unit of the primary font.
    fn scale(&self) -> f32 {
        self.font().map_or(0.0, |f| self.size * f.inv_upem())
    }

    fn metric(&self, f: impl Fn(&Font<'a>) -> i16) -> f32 {
        self.font().map_or(0.0, |font| f(font) as f32) * self.scale()
    }

    /// Distance from the baseline to the top of the line box (positive, pixels).
    pub fn ascent(&self) -> f32 {
        self.metric(|f| f.metrics().ascender)
    }

    /// Distance from the baseline to the bottom of the line box (positive, pixels).
    pub fn descent(&self) -> f32 {
        -self.metric(|f| f.metrics().descender)
    }

    /// Recommended extra space between lines (pixels).
    pub fn line_gap(&self) -> f32 {
        self.metric(|f| f.metrics().line_gap).max(0.0)
    }

    /// Baseline-to-baseline distance: `ascent + descent + line_gap` (pixels).
    pub fn line_height(&self) -> f32 {
        self.ascent() + self.descent() + self.line_gap()
    }

    /// Height of lowercase letters (pixels).
    pub fn x_height(&self) -> f32 {
        self.metric(|f| f.metrics().x_height)
    }

    /// Height of capital letters (pixels).
    pub fn cap_height(&self) -> f32 {
        self.metric(|f| f.metrics().cap_height)
    }

    /// Underline position (top edge, pixels below the baseline) and thickness (pixels).
    pub fn underline(&self) -> (f32, f32) {
        (-self.metric(|f| f.metrics().underline_position), self.metric(|f| f.metrics().underline_thickness).max(1.0))
    }

    /// The font index and glyph used for `c`.
    pub fn glyph(&self, c: char) -> (usize, GlyphId) {
        self.coll.resolve(self.primary, c)
    }

    /// Width of the space character (pixels).
    pub fn space_advance(&self) -> f32 {
        let (fi, g) = self.glyph(' ');
        self.glyph_advance(fi, g)
    }

    #[inline]
    fn glyph_advance(&self, font: usize, g: GlyphId) -> f32 {
        match self.coll.fonts.get(font) {
            Some(f) => f.advance_width(g) as f32 * self.size * f.inv_upem(),
            None => 0.0,
        }
    }

    /// The advance of a single character (pixels; no kerning). Tabs count as four spaces.
    pub fn advance(&self, c: char) -> f32 {
        if c == '\t' {
            return self.space_advance() * TAB_SPACES;
        }
        if is_invisible(c) {
            return 0.0;
        }
        let (fi, g) = self.glyph(c);
        self.glyph_advance(fi, g)
    }

    /// Computes the step for character `c` at byte `index` given the pen position and the
    /// previous glyph (for kerning). Updates `prev`.
    #[inline]
    fn step(&self, index: usize, c: char, x: f32, prev: &mut Option<(usize, GlyphId)>, tab: f32) -> Step {
        if c == '\t' {
            *prev = None;
            let next = if tab > 0.0 { (math::floor(x / tab + 1e-4) + 1.0) * tab } else { x };
            return Step { index, glyph: None, x, advance: next - x };
        }
        if is_invisible(c) {
            *prev = None;
            return Step { index, glyph: None, x, advance: 0.0 };
        }
        let (fi, g) = self.coll.resolve(self.primary, c);
        let Some(font) = self.coll.fonts.get(fi) else {
            return Step { index, glyph: None, x, advance: 0.0 };
        };
        let scale = self.size * font.inv_upem();
        let mut x = x;
        if let Some((pf, pg)) = *prev
            && pf == fi
            && self.kerning
        {
            x += font.kerning(pg, g) as f32 * scale;
        }
        *prev = Some((fi, g));
        let glyph = if c.is_whitespace() { None } else { Some((fi, g)) };
        Step { index, glyph, x, advance: font.advance_width(g) as f32 * scale }
    }

    /// Walks the characters of `text`, calling `f` for every step; returns the total width.
    fn walk(&self, text: &str, mut f: impl FnMut(&Step)) -> f32 {
        let tab = self.space_advance() * TAB_SPACES;
        let mut x = 0.0f32;
        let mut prev = None;
        for (i, c) in text.char_indices() {
            let s = self.step(i, c, x, &mut prev, tab);
            f(&s);
            x = s.x + s.advance;
        }
        x
    }

    /// The width of `text` in pixels, including kerning (no line breaking; `\n` has zero width).
    pub fn measure(&self, text: &str) -> f32 {
        self.walk(text, |_| {})
    }

    /// Lays out `text` on a single line starting at `origin` (pen position on the baseline).
    /// Invisible characters and whitespace produce no glyphs but still advance the pen.
    pub fn layout_line(&self, text: &str, origin: Point) -> Vec<PositionedGlyph> {
        let mut out = Vec::with_capacity(text.len());
        self.layout_line_into(text, origin, &mut out);
        out
    }

    /// Like [`ScaledFont::layout_line`], appending to `out`; returns the end pen x position.
    pub fn layout_line_into(&self, text: &str, origin: Point, out: &mut Vec<PositionedGlyph>) -> f32 {
        let w = self.walk(text, |s| {
            if let Some((fi, g)) = s.glyph {
                out.push(PositionedGlyph {
                    glyph: g,
                    font: fi as u16,
                    x: origin.x + s.x,
                    y: origin.y,
                    cluster: s.index as u32,
                });
            }
        });
        origin.x + w
    }

    /// Breaks `text` into lines no wider than `max_width` pixels.
    ///
    /// Lines break after whitespace; words wider than `max_width` are broken between characters;
    /// `\n` (and `\r\n`) always ends a line. Each returned byte range covers one line's text
    /// including the whitespace at which it was wrapped (which may exceed `max_width`; it is not
    /// drawn) and excluding the line terminator. Every line has at least one character unless the
    /// paragraph is empty, so the result always makes progress.
    pub fn wrap_lines(&self, text: &str, max_width: f32) -> Vec<Range<usize>> {
        let mut lines = Vec::new();
        let mut start = 0usize;
        loop {
            let end = text[start..].find('\n').map_or(text.len(), |p| start + p);
            let content_end = if end > start && text.as_bytes()[end - 1] == b'\r' { end - 1 } else { end };
            self.wrap_paragraph(text, start, content_end, max_width, &mut lines);
            if end >= text.len() {
                break;
            }
            start = end + 1;
        }
        lines
    }

    fn wrap_paragraph(&self, text: &str, start: usize, end: usize, max_width: f32, out: &mut Vec<Range<usize>>) {
        if start >= end {
            out.push(start..start);
            return;
        }
        let tab = self.space_advance() * TAB_SPACES;
        let mut line_start = start;
        let mut i = start;
        let mut x = 0.0f32;
        let mut prev = None;
        let mut has_content = false;
        let mut after_space = false;
        let mut break_at: Option<usize> = None;
        while i < end {
            let Some(c) = text[i..end].chars().next() else { break };
            let len = c.len_utf8();
            if is_break_space(c) {
                let s = self.step(i, c, x, &mut prev, tab);
                x = s.x + s.advance;
                if has_content {
                    after_space = true;
                }
                i += len;
                continue;
            }
            if after_space {
                break_at = Some(i);
                after_space = false;
            }
            let mut p = prev;
            let s = self.step(i, c, x, &mut p, tab);
            let right = s.x + s.advance;
            if has_content && right > max_width {
                match break_at {
                    Some(b) if b > line_start => {
                        out.push(line_start..b);
                        line_start = b;
                        i = b;
                    }
                    _ => {
                        out.push(line_start..i);
                        line_start = i;
                    }
                }
                x = 0.0;
                prev = None;
                has_content = false;
                after_space = false;
                break_at = None;
                continue;
            }
            prev = p;
            x = right;
            if s.advance > 0.0 || s.glyph.is_some() {
                has_content = true;
            }
            i += len;
        }
        out.push(line_start..end);
    }

    /// The caret x position (pixels from the line start) before the character at `byte_index`
    /// (rounded down to a character boundary; indices past the end give the line width).
    pub fn x_for_index(&self, text: &str, byte_index: usize) -> f32 {
        let mut target = byte_index.min(text.len());
        while !text.is_char_boundary(target) {
            target -= 1;
        }
        let mut result = None;
        let total = self.walk(text, |s| {
            if result.is_none() && s.index >= target {
                result = Some(s.x);
            }
        });
        result.unwrap_or(total)
    }

    /// The byte index of the character boundary nearest to `x` (pixels from the line start):
    /// clicking on the left half of a character places the caret before it, on the right half
    /// after it.
    pub fn index_for_x(&self, text: &str, x: f32) -> usize {
        let mut found = None;
        self.walk(text, |s| {
            if found.is_none() && x < s.x + s.advance * 0.5 {
                found = Some(s.index);
            }
        });
        found.unwrap_or(text.len())
    }
}
