//! The [`Font`] type: a zero-copy view of an OpenType/TrueType font file.

use alloc::string::String;
use core::sync::atomic::{AtomicU32, Ordering};

use vraster::{Path, Point, Rect};

use crate::cff::Cff;
use crate::cmap::Cmap;
use crate::glyf::{Glyf, GlyfScratch};
use crate::kern::{KernCache, Kerning};
use crate::name::{self, name_id};
use crate::outline::{BoundsSink, OutlineSink, PathSink};
use crate::parse::{i16_at, tag_at, u16_at, u32_at};
use crate::{FontError, GlyphId};

/// Source of unique font ids (see [`Font::id`]).
static NEXT_FONT_ID: AtomicU32 = AtomicU32::new(1);

/// The outline format of a font.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutlineFormat {
    /// Quadratic TrueType outlines (`glyf`/`loca`).
    TrueType,
    /// Cubic PostScript outlines (`CFF `).
    Cff,
}

/// Font-wide metrics in font units (y up: ascenders positive, descenders negative).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FontMetrics {
    /// Font units per em.
    pub units_per_em: u16,
    /// Ascender used for layout: the OS/2 typo ascender if the font sets `USE_TYPO_METRICS`,
    /// otherwise the `hhea` ascender.
    pub ascender: i16,
    /// Descender used for layout (negative), chosen like `ascender`.
    pub descender: i16,
    /// Line gap used for layout, chosen like `ascender`.
    pub line_gap: i16,
    /// `hhea` ascender.
    pub hhea_ascender: i16,
    /// `hhea` descender.
    pub hhea_descender: i16,
    /// `hhea` line gap.
    pub hhea_line_gap: i16,
    /// OS/2 typographic ascender (0 without OS/2).
    pub typo_ascender: i16,
    /// OS/2 typographic descender (0 without OS/2).
    pub typo_descender: i16,
    /// OS/2 typographic line gap (0 without OS/2).
    pub typo_line_gap: i16,
    /// OS/2 Windows ascent (0 without OS/2).
    pub win_ascent: u16,
    /// OS/2 Windows descent, positive (0 without OS/2).
    pub win_descent: u16,
    /// Height of lowercase letters (OS/2 v2+, else measured from 'x', else estimated).
    pub x_height: i16,
    /// Height of capital letters (OS/2 v2+, else measured from 'H', else estimated).
    pub cap_height: i16,
    /// Union of all glyph bounding boxes (`head`): minimum x.
    pub x_min: i16,
    /// Minimum y of all glyphs.
    pub y_min: i16,
    /// Maximum x of all glyphs.
    pub x_max: i16,
    /// Maximum y of all glyphs.
    pub y_max: i16,
    /// Weight class (400 regular, 700 bold; 400 without OS/2).
    pub weight_class: u16,
    /// Underline position (top of the underline, usually negative; from `post`).
    pub underline_position: i16,
    /// Underline thickness (from `post`).
    pub underline_thickness: i16,
    /// Strikeout position (from OS/2).
    pub strikeout_position: i16,
    /// Strikeout thickness (from OS/2).
    pub strikeout_size: i16,
    /// The font declares itself monospaced (`post.isFixedPitch`).
    pub is_monospace: bool,
}

/// Tables used by the engine.
#[derive(Clone, Copy, Default)]
struct Tables<'a> {
    head: &'a [u8],
    hhea: &'a [u8],
    maxp: &'a [u8],
    hmtx: &'a [u8],
    cmap: &'a [u8],
    os2: &'a [u8],
    post: &'a [u8],
    name: &'a [u8],
    glyf: &'a [u8],
    loca: &'a [u8],
    cff: &'a [u8],
    cff2: &'a [u8],
    kern: &'a [u8],
    gpos: &'a [u8],
}

#[derive(Clone)]
enum Outlines<'a> {
    Glyf(Glyf<'a>),
    Cff(Cff<'a>),
}

/// Reusable scratch buffers for outline extraction (avoids allocations in hot paths).
#[derive(Default)]
pub struct OutlineScratch {
    glyf: GlyfScratch,
}

impl OutlineScratch {
    /// Creates empty scratch buffers.
    pub fn new() -> Self {
        OutlineScratch::default()
    }
}

/// A parsed font. Parsing is zero-copy: the font borrows the file data for `'a`.
///
/// Creating a `Font` validates the table directory and the tables needed for metrics, character
/// mapping and outlines; glyph data is decoded lazily and every access is bounds-checked, so
/// malformed fonts produce errors (or empty glyphs) but never panics.
///
/// `Font` uses interior mutability for its kerning cache and is meant for single-threaded use.
#[derive(Clone)]
pub struct Font<'a> {
    data: &'a [u8],
    id: u32,
    tables: Tables<'a>,
    metrics: FontMetrics,
    num_glyphs: u16,
    num_h_metrics: u16,
    inv_upem: f32,
    cmap: Option<Cmap<'a>>,
    outlines: Outlines<'a>,
    kerning: Kerning<'a>,
    kern_cache: Option<KernCache>,
    ascii: [u16; 128],
}

impl core::fmt::Debug for Font<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Font")
            .field("id", &self.id)
            .field("num_glyphs", &self.num_glyphs)
            .field("units_per_em", &self.metrics.units_per_em)
            .field("outline_format", &self.outline_format())
            .finish()
    }
}

impl<'a> Font<'a> {
    /// Parses a font file (`.ttf`/`.otf`; for collections the first font).
    pub fn from_bytes(data: &'a [u8]) -> Result<Font<'a>, FontError> {
        Font::from_collection(data, 0)
    }

    /// The number of fonts in `data`: the font count of a collection (`.ttc`), 1 for a plain font
    /// file and 0 if `data` is not a font.
    pub fn count(data: &[u8]) -> u32 {
        match tag_at(data, 0) {
            Some(t) if &t == b"ttcf" => u32_at(data, 8).unwrap_or(0),
            Some(t) if matches!(&t, b"\0\x01\0\0" | b"OTTO" | b"true") => 1,
            _ => 0,
        }
    }

    /// Parses font number `index` of a collection (index 0 for plain font files).
    pub fn from_collection(data: &'a [u8], index: u32) -> Result<Font<'a>, FontError> {
        let tag = tag_at(data, 0).ok_or(FontError::Truncated)?;
        let dir = if &tag == b"ttcf" {
            let n = u32_at(data, 8).ok_or(FontError::Truncated)?;
            if index >= n {
                return Err(FontError::InvalidFontIndex);
            }
            u32_at(data, 12 + index as usize * 4).ok_or(FontError::Truncated)? as usize
        } else if index != 0 {
            return Err(FontError::InvalidFontIndex);
        } else {
            0
        };
        match u32_at(data, dir).ok_or(FontError::Truncated)? {
            0x0001_0000 | 0x4F54_544F | 0x7472_7565 => {}
            _ => return Err(FontError::UnsupportedFormat),
        }
        let num_tables = u16_at(data, dir + 4).ok_or(FontError::Truncated)? as usize;
        let mut t = Tables::default();
        for i in 0..num_tables {
            let rec = dir + 12 + i * 16;
            let (Some(tag), Some(off), Some(len)) = (tag_at(data, rec), u32_at(data, rec + 8), u32_at(data, rec + 12))
            else {
                return Err(FontError::Truncated);
            };
            let slot = match &tag {
                b"head" => &mut t.head,
                b"hhea" => &mut t.hhea,
                b"maxp" => &mut t.maxp,
                b"hmtx" => &mut t.hmtx,
                b"cmap" => &mut t.cmap,
                b"OS/2" => &mut t.os2,
                b"post" => &mut t.post,
                b"name" => &mut t.name,
                b"glyf" => &mut t.glyf,
                b"loca" => &mut t.loca,
                b"CFF " => &mut t.cff,
                b"CFF2" => &mut t.cff2,
                b"kern" => &mut t.kern,
                b"GPOS" => &mut t.gpos,
                _ => continue,
            };
            let (off, len) = (off as usize, len as usize);
            match off.checked_add(len).and_then(|end| data.get(off..end)) {
                Some(s) => *slot = s,
                None => return Err(FontError::MalformedTable(tag)),
            }
        }
        for (table, tag) in [(t.head, *b"head"), (t.hhea, *b"hhea"), (t.maxp, *b"maxp"), (t.hmtx, *b"hmtx")] {
            if table.is_empty() {
                return Err(FontError::MissingTable(tag));
            }
        }

        // head
        let bad_head = FontError::MalformedTable(*b"head");
        let upem = u16_at(t.head, 18).ok_or(bad_head)?;
        if !(16..=16384).contains(&upem) {
            return Err(bad_head);
        }
        let mut m = FontMetrics {
            units_per_em: upem,
            x_min: i16_at(t.head, 36).ok_or(bad_head)?,
            y_min: i16_at(t.head, 38).ok_or(bad_head)?,
            x_max: i16_at(t.head, 40).ok_or(bad_head)?,
            y_max: i16_at(t.head, 42).ok_or(bad_head)?,
            weight_class: 400,
            ..FontMetrics::default()
        };
        let loc_long = i16_at(t.head, 50).ok_or(bad_head)? != 0;

        // maxp, hhea
        let num_glyphs = u16_at(t.maxp, 4).ok_or(FontError::MalformedTable(*b"maxp"))?;
        let bad_hhea = FontError::MalformedTable(*b"hhea");
        m.hhea_ascender = i16_at(t.hhea, 4).ok_or(bad_hhea)?;
        m.hhea_descender = i16_at(t.hhea, 6).ok_or(bad_hhea)?;
        m.hhea_line_gap = i16_at(t.hhea, 8).ok_or(bad_hhea)?;
        let num_h_metrics = u16_at(t.hhea, 34).ok_or(bad_hhea)?.min(num_glyphs).min((t.hmtx.len() / 4) as u16);
        if num_h_metrics == 0 && num_glyphs > 0 {
            return Err(FontError::MalformedTable(*b"hmtx"));
        }

        // OS/2
        let os2 = t.os2;
        let mut use_typo = false;
        if let (Some(version), Some(asc), Some(desc), Some(gap)) =
            (u16_at(os2, 0), i16_at(os2, 68), i16_at(os2, 70), i16_at(os2, 72))
        {
            m.weight_class = u16_at(os2, 4).unwrap_or(400);
            m.strikeout_size = i16_at(os2, 26).unwrap_or(0);
            m.strikeout_position = i16_at(os2, 28).unwrap_or(0);
            m.typo_ascender = asc;
            m.typo_descender = desc;
            m.typo_line_gap = gap;
            m.win_ascent = u16_at(os2, 74).unwrap_or(0);
            m.win_descent = u16_at(os2, 76).unwrap_or(0);
            use_typo = u16_at(os2, 62).unwrap_or(0) & 0x80 != 0;
            if version >= 2 {
                m.x_height = i16_at(os2, 86).unwrap_or(0);
                m.cap_height = i16_at(os2, 88).unwrap_or(0);
            }
        }
        if (use_typo && m.typo_ascender - m.typo_descender > 0) || (m.hhea_ascender == 0 && m.hhea_descender == 0) {
            m.ascender = m.typo_ascender;
            m.descender = m.typo_descender;
            m.line_gap = m.typo_line_gap;
        } else {
            m.ascender = m.hhea_ascender;
            m.descender = m.hhea_descender;
            m.line_gap = m.hhea_line_gap;
        }
        if m.ascender == 0 && m.descender == 0 {
            m.ascender = m.win_ascent.min(i16::MAX as u16) as i16;
            m.descender = -(m.win_descent.min(i16::MAX as u16) as i16);
        }
        if m.ascender == 0 && m.descender == 0 {
            m.ascender = (upem as i32 * 8 / 10) as i16;
            m.descender = -((upem as i32 * 2 / 10) as i16);
        }

        // post
        m.underline_position = i16_at(t.post, 8).unwrap_or(-(upem as i32 / 10) as i16);
        m.underline_thickness = i16_at(t.post, 10).unwrap_or((upem as i32 / 20) as i16);
        m.is_monospace = u32_at(t.post, 12).unwrap_or(0) != 0;
        if m.strikeout_size == 0 {
            m.strikeout_size = m.underline_thickness;
        }

        // Outlines
        let outlines = if !t.glyf.is_empty() && !t.loca.is_empty() {
            Outlines::Glyf(Glyf { loca: t.loca, glyf: t.glyf, long: loc_long, num_glyphs })
        } else if !t.cff.is_empty() {
            let cff = Cff::parse(t.cff)?;
            if cff.num_glyphs() < num_glyphs as u32 {
                return Err(FontError::MalformedTable(*b"CFF "));
            }
            Outlines::Cff(cff)
        } else if !t.cff2.is_empty() {
            return Err(FontError::UnsupportedFormat);
        } else {
            return Err(FontError::MissingTable(*b"glyf"));
        };

        let cmap = if t.cmap.is_empty() { None } else { Cmap::parse(t.cmap) };
        let kerning = Kerning::new(t.gpos, t.kern);
        let kern_cache = if kerning.is_some() { Some(KernCache::new()) } else { None };
        let mut font = Font {
            data,
            id: NEXT_FONT_ID.fetch_add(1, Ordering::Relaxed),
            tables: t,
            metrics: m,
            num_glyphs,
            num_h_metrics,
            inv_upem: 1.0 / upem as f32,
            cmap,
            outlines,
            kerning,
            kern_cache,
            ascii: [0; 128],
        };
        if let Some(cmap) = &font.cmap {
            for c in 0..128u32 {
                font.ascii[c as usize] = cmap.lookup(c).filter(|&g| g < num_glyphs).unwrap_or(0);
            }
        }
        // Measure x-height and cap height when OS/2 does not provide them.
        if font.metrics.x_height <= 0 {
            font.metrics.x_height = font.measure_top('x').unwrap_or((upem as i32 / 2) as i16);
        }
        if font.metrics.cap_height <= 0 {
            font.metrics.cap_height = font.measure_top('H').unwrap_or((upem as i32 * 7 / 10) as i16);
        }
        Ok(font)
    }

    /// The top of the outline of `c` (font units), if the font has a non-empty glyph for it.
    fn measure_top(&self, c: char) -> Option<i16> {
        let g = self.glyph_index(c)?;
        let b = self.glyph_bounds(g).ok()??;
        Some(b.y1.clamp(i16::MIN as f32, i16::MAX as f32) as i16)
    }

    /// The font file data.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// A process-unique identifier of this font (clones share it). Used as the font part of
    /// glyph cache keys.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Raw access to a table by tag (only for the tables the engine uses: `head`, `hhea`, `maxp`,
    /// `hmtx`, `cmap`, `OS/2`, `post`, `name`, `glyf`, `loca`, `CFF `, `kern`, `GPOS`).
    pub fn table(&self, tag: [u8; 4]) -> Option<&'a [u8]> {
        let t = &self.tables;
        let s = match &tag {
            b"head" => t.head,
            b"hhea" => t.hhea,
            b"maxp" => t.maxp,
            b"hmtx" => t.hmtx,
            b"cmap" => t.cmap,
            b"OS/2" => t.os2,
            b"post" => t.post,
            b"name" => t.name,
            b"glyf" => t.glyf,
            b"loca" => t.loca,
            b"CFF " => t.cff,
            b"kern" => t.kern,
            b"GPOS" => t.gpos,
            _ => return None,
        };
        if s.is_empty() { None } else { Some(s) }
    }

    /// Font units per em.
    #[inline]
    pub fn units_per_em(&self) -> u16 {
        self.metrics.units_per_em
    }

    /// `1 / units_per_em`.
    #[inline]
    pub(crate) fn inv_upem(&self) -> f32 {
        self.inv_upem
    }

    /// Number of glyphs (`maxp`).
    #[inline]
    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// The outline format.
    pub fn outline_format(&self) -> OutlineFormat {
        match self.outlines {
            Outlines::Glyf(_) => OutlineFormat::TrueType,
            Outlines::Cff(_) => OutlineFormat::Cff,
        }
    }

    /// Font-wide metrics in font units.
    #[inline]
    pub fn metrics(&self) -> &FontMetrics {
        &self.metrics
    }

    /// Maps a character to a glyph (`None` if the font has no glyph for it).
    #[inline]
    pub fn glyph_index(&self, c: char) -> Option<GlyphId> {
        let cp = c as u32;
        if cp < 128 {
            let g = self.ascii[cp as usize];
            return if g != 0 { Some(GlyphId(g)) } else { None };
        }
        self.cmap.as_ref()?.lookup(cp).filter(|&g| g < self.num_glyphs).map(GlyphId)
    }

    /// Returns `true` if the font has a glyph for `c`.
    #[inline]
    pub fn has_glyph(&self, c: char) -> bool {
        self.glyph_index(c).is_some()
    }

    /// The advance width of a glyph in font units (0 for invalid glyph ids).
    #[inline]
    pub fn advance_width(&self, glyph: GlyphId) -> u16 {
        if glyph.0 >= self.num_glyphs || self.num_h_metrics == 0 {
            return 0;
        }
        let i = (glyph.0 as usize).min(self.num_h_metrics as usize - 1);
        u16_at(self.tables.hmtx, i * 4).unwrap_or(0)
    }

    /// The left side bearing of a glyph in font units.
    pub fn left_side_bearing(&self, glyph: GlyphId) -> i16 {
        let n = self.num_h_metrics as usize;
        let i = glyph.0 as usize;
        if glyph.0 >= self.num_glyphs {
            return 0;
        }
        let off = if i < n { i * 4 + 2 } else { n * 4 + (i - n) * 2 };
        i16_at(self.tables.hmtx, off).unwrap_or(0)
    }

    /// Returns `true` if the font has pair kerning (GPOS `kern` feature or `kern` table).
    pub fn has_kerning(&self) -> bool {
        self.kerning.is_some()
    }

    /// The kerning adjustment between two glyphs in font units (negative = closer). Cached.
    #[inline]
    pub fn kerning(&self, left: GlyphId, right: GlyphId) -> i16 {
        let Some(cache) = &self.kern_cache else { return 0 };
        let key = ((left.0 as u32) << 16) | right.0 as u32;
        if let Some(v) = cache.get(key) {
            return v;
        }
        let v = self.kerning.lookup(left.0, right.0);
        cache.put(key, v);
        v
    }

    /// Kerning without the cache (mainly for tests and diagnostics).
    pub fn kerning_uncached(&self, left: GlyphId, right: GlyphId) -> i16 {
        self.kerning.lookup(left.0, right.0)
    }

    /// Emits the outline of `glyph` in font units (y up) to `sink`.
    ///
    /// Glyphs without contours (e.g. space) emit nothing. On error the sink may have received a
    /// partial outline and should be discarded.
    pub fn outline<S: OutlineSink + ?Sized>(&self, glyph: GlyphId, sink: &mut S) -> Result<(), FontError> {
        let mut scratch = OutlineScratch::default();
        self.outline_with(glyph, sink, &mut scratch)
    }

    /// Like [`Font::outline`], reusing `scratch` buffers.
    pub fn outline_with<S: OutlineSink + ?Sized>(
        &self,
        glyph: GlyphId,
        sink: &mut S,
        scratch: &mut OutlineScratch,
    ) -> Result<(), FontError> {
        if glyph.0 >= self.num_glyphs {
            return Err(FontError::InvalidGlyph);
        }
        match &self.outlines {
            Outlines::Glyf(g) => g.outline(glyph.0, sink, &mut scratch.glyf),
            Outlines::Cff(c) => c.outline(glyph.0, sink),
        }
    }

    /// The control box of a glyph's outline in font units (`None` for empty glyphs). For
    /// TrueType glyphs this equals the bounding box of all points.
    pub fn glyph_bounds(&self, glyph: GlyphId) -> Result<Option<Rect>, FontError> {
        let mut b = BoundsSink::default();
        self.outline(glyph, &mut b)?;
        Ok(b.bounds)
    }

    /// The bounding box stored in a TrueType glyph header (`[x_min, y_min, x_max, y_max]`);
    /// `None` for CFF fonts and empty glyphs.
    pub fn glyph_header_bbox(&self, glyph: GlyphId) -> Option<[i16; 4]> {
        match &self.outlines {
            Outlines::Glyf(g) => g.header_bbox(glyph.0),
            Outlines::Cff(_) => None,
        }
    }

    /// The outline of `glyph` as a path in pixels for a top-down raster: scaled to `size_px`
    /// (pixels per em), y flipped, with the glyph origin (pen position on the baseline) at
    /// `(0, 0)`.
    pub fn glyph_path(&self, glyph: GlyphId, size_px: f32) -> Result<Path, FontError> {
        let mut path = Path::new();
        self.append_glyph_path(glyph, size_px, Point::ZERO, &mut path)?;
        Ok(path)
    }

    /// Appends the outline of `glyph` scaled to `size_px` with its origin at `origin` (pixels,
    /// y down) to `path`.
    pub fn append_glyph_path(&self, glyph: GlyphId, size_px: f32, origin: Point, path: &mut Path) -> Result<(), FontError> {
        let scale = size_px * self.inv_upem;
        let mut sink = PathSink::new(path, scale, origin);
        self.outline(glyph, &mut sink)
    }

    /// Looks up a `name` table string by name id (see [`crate::name_id`]).
    pub fn name(&self, id: u16) -> Option<String> {
        name::lookup(self.tables.name, id)
    }

    /// The family name (typographic family if present, else the legacy family).
    pub fn family_name(&self) -> Option<String> {
        self.name(name_id::TYPOGRAPHIC_FAMILY).or_else(|| self.name(name_id::FAMILY))
    }

    /// The style name (typographic subfamily if present, else the legacy subfamily).
    pub fn subfamily_name(&self) -> Option<String> {
        self.name(name_id::TYPOGRAPHIC_SUBFAMILY).or_else(|| self.name(name_id::SUBFAMILY))
    }

    /// The full font name.
    pub fn full_name(&self) -> Option<String> {
        self.name(name_id::FULL_NAME)
    }

    /// The PostScript name.
    pub fn postscript_name(&self) -> Option<String> {
        self.name(name_id::POSTSCRIPT_NAME)
    }

    /// The version string.
    pub fn version(&self) -> Option<String> {
        self.name(name_id::VERSION)
    }

    /// The copyright notice (name id 0).
    pub fn copyright(&self) -> Option<String> {
        self.name(name_id::COPYRIGHT)
    }

    /// The license description (name id 13).
    pub fn license(&self) -> Option<String> {
        self.name(name_id::LICENSE)
    }

    /// The license URL (name id 14).
    pub fn license_url(&self) -> Option<String> {
        self.name(name_id::LICENSE_URL)
    }
}
