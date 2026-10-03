//! TrueType/OpenType (glyf and CFF) font parsing, rasterization, caching and text layout.
//!
//! * [`Font`] is a zero-copy parser over the font file bytes: table directory, metrics
//!   ([`FontMetrics`]), names, `cmap` (formats 4 and 12, plus 0/6/13), kerning (GPOS PairPos for
//!   the `kern` feature, or the legacy `kern` table; cached) and glyph outlines (TrueType quadratic
//!   outlines including composites, and CFF Type 2 charstrings).
//! * [`OutlineSink`] receives outlines in font units; [`PathSink`] / [`Font::glyph_path`] produce
//!   [`vraster::Path`]s in pixels (scaled, y flipped).
//! * [`rasterize_glyph`] / [`GlyphRasterizer`] render anti-aliased [`GlyphBitmap`]s at fractional
//!   x offsets, with optional stem darkening and gamma ([`RasterOptions`]).
//! * [`GlyphCache`] caches bitmaps keyed by font, glyph, size (1/64 px) and subpixel bin under a
//!   memory budget (CLOCK eviction).
//! * [`FontCollection`] (fallback chain) and [`ScaledFont`] (font + pixel size) provide pixel
//!   metrics, measuring, single-line layout, word wrapping and caret helpers.
//!
//! Typical text rendering:
//!
//! ```ignore
//! let mut fonts = FontCollection::new();
//! let ui = fonts.add(Font::from_bytes(INTER)?);
//! fonts.add(Font::from_bytes(LATO)?); // fallback
//! let style = fonts.scaled(ui, 14.0);
//! let mut cache = GlyphCache::new(4 << 20);
//! for g in style.layout_line("Hello", Point::new(10.0, baseline)) {
//!     let (px, bin) = subpixel_position(g.x);
//!     let font = fonts.font(g.font as usize).unwrap();
//!     let bmp = cache.get(font, g.glyph, style.size_px(), bin);
//!     blend(bmp, px + bmp.left, g.y as i32 - bmp.top);
//! }
//! ```
//!
//! Malformed fonts never cause panics or unbounded work: every read is bounds-checked, composite
//! glyph nesting, charstring subroutine depth and operation counts are limited, and errors are
//! reported as [`FontError`].

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod cache;
mod cff;
mod cmap;
mod font;
mod glyf;
mod kern;
mod layout;
mod name;
mod outline;
mod parse;
mod raster;

use core::fmt;

pub use cache::{CacheStats, GlyphCache, GlyphKey, SUBPIXEL_BINS, subpixel_position};
pub use font::{Font, FontMetrics, OutlineFormat, OutlineScratch};
pub use layout::{FontCollection, PositionedGlyph, ScaledFont};
pub use name::name_id;
pub use outline::{BoundsSink, OutlineSink, PathSink};
pub use raster::{GlyphBitmap, GlyphRasterizer, MAX_GLYPH_SIZE, RasterOptions, rasterize_glyph};
pub use vraster::{Path, Point, Rect};

/// A glyph index within a font.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlyphId(pub u16);

impl GlyphId {
    /// The `.notdef` glyph (shown for unsupported characters).
    pub const NOTDEF: GlyphId = GlyphId(0);
}

/// Errors produced while parsing fonts or glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontError {
    /// The data ends before a structure it declares.
    Truncated,
    /// Not a supported font format (unknown signature, CFF2, non-Type 2 charstrings, ...).
    UnsupportedFormat,
    /// The font index is out of range for this (collection) file.
    InvalidFontIndex,
    /// A required table is missing.
    MissingTable([u8; 4]),
    /// A table is malformed or points outside the file.
    MalformedTable([u8; 4]),
    /// The glyph id is out of range.
    InvalidGlyph,
    /// The glyph data is malformed.
    MalformedGlyph,
    /// A safety limit was exceeded (composite nesting, subroutine depth, operation count, ...).
    LimitExceeded,
}

fn tag_str(t: &[u8; 4]) -> [char; 4] {
    t.map(|b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FontError::Truncated => f.write_str("font data is truncated"),
            FontError::UnsupportedFormat => f.write_str("unsupported font format"),
            FontError::InvalidFontIndex => f.write_str("font index out of range"),
            FontError::MissingTable(t) => {
                let [a, b, c, d] = tag_str(t);
                write!(f, "missing table '{a}{b}{c}{d}'")
            }
            FontError::MalformedTable(t) => {
                let [a, b, c, d] = tag_str(t);
                write!(f, "malformed table '{a}{b}{c}{d}'")
            }
            FontError::InvalidGlyph => f.write_str("glyph id out of range"),
            FontError::MalformedGlyph => f.write_str("malformed glyph data"),
            FontError::LimitExceeded => f.write_str("glyph exceeds processing limits"),
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod visual_tests;
