//! TrueType outlines (`glyf` + `loca`), including composite glyphs.
//!
//! Glyphs are decoded into a flat point list (on/off-curve flags plus contour ends) so that
//! composite components can be transformed and aligned by point matching, then converted into
//! quadratic path commands (inserting the implied on-curve points between consecutive off-curve
//! points). Hinting instructions are ignored.

use alloc::vec::Vec;

use crate::FontError;
use crate::outline::OutlineSink;
use crate::parse::{Reader, i16_at, u16_at, u32_at};

/// Maximum nesting of composite glyphs.
const MAX_DEPTH: u32 = 8;
/// Maximum number of points of one (possibly composite) glyph.
const MAX_POINTS: usize = 1 << 17;
/// Maximum number of components loaded for one glyph (all nesting levels).
const MAX_COMPONENTS: u32 = 1024;

// Simple glyph flags.
const ON_CURVE: u8 = 0x01;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const REPEAT: u8 = 0x08;
const X_SAME_OR_POSITIVE: u8 = 0x10;
const Y_SAME_OR_POSITIVE: u8 = 0x20;

// Composite glyph flags.
const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const WE_HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
const SCALED_COMPONENT_OFFSET: u16 = 0x0800;
const UNSCALED_COMPONENT_OFFSET: u16 = 0x1000;

/// A decoded outline point (font units).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GPoint {
    x: f32,
    y: f32,
    on: bool,
}

/// Reusable buffers for decoding TrueType glyphs.
#[derive(Default)]
pub(crate) struct GlyfScratch {
    points: Vec<GPoint>,
    /// Exclusive end index (into `points`) of every contour.
    ends: Vec<usize>,
    flags: Vec<u8>,
    components: u32,
}

/// The `glyf` and `loca` tables.
#[derive(Clone, Copy)]
pub(crate) struct Glyf<'a> {
    pub(crate) loca: &'a [u8],
    pub(crate) glyf: &'a [u8],
    pub(crate) long: bool,
    pub(crate) num_glyphs: u16,
}

impl<'a> Glyf<'a> {
    /// The raw data of glyph `gid` (empty for glyphs without outline).
    pub(crate) fn glyph_data(&self, gid: u16) -> Result<&'a [u8], FontError> {
        if gid >= self.num_glyphs {
            return Err(FontError::InvalidGlyph);
        }
        let i = gid as usize;
        let (start, end) = if self.long {
            (u32_at(self.loca, i * 4), u32_at(self.loca, i * 4 + 4))
        } else {
            (u16_at(self.loca, i * 2).map(|v| v as u32 * 2), u16_at(self.loca, i * 2 + 2).map(|v| v as u32 * 2))
        };
        let (Some(start), Some(end)) = (start, end) else {
            return Err(FontError::MalformedTable(*b"loca"));
        };
        let (start, end) = (start as usize, end as usize);
        if end <= start || start >= self.glyf.len() {
            return Ok(&[]);
        }
        // Some fonts have a final loca entry slightly past the end of glyf; clamp.
        Ok(&self.glyf[start..end.min(self.glyf.len())])
    }

    /// The bounding box stored in the glyph header (`x_min, y_min, x_max, y_max`).
    pub(crate) fn header_bbox(&self, gid: u16) -> Option<[i16; 4]> {
        let d = self.glyph_data(gid).ok()?;
        Some([i16_at(d, 2)?, i16_at(d, 4)?, i16_at(d, 6)?, i16_at(d, 8)?])
    }

    /// Emits the outline of `gid` to `sink`.
    pub(crate) fn outline<S: OutlineSink + ?Sized>(
        &self,
        gid: u16,
        sink: &mut S,
        scratch: &mut GlyfScratch,
    ) -> Result<(), FontError> {
        scratch.points.clear();
        scratch.ends.clear();
        scratch.components = 0;
        self.load(gid, 0, scratch)?;
        emit(scratch, sink);
        Ok(())
    }

    /// Appends the points and contours of `gid` to `scratch`.
    fn load(&self, gid: u16, depth: u32, s: &mut GlyfScratch) -> Result<(), FontError> {
        if depth > MAX_DEPTH {
            return Err(FontError::LimitExceeded);
        }
        let d = self.glyph_data(gid)?;
        if d.is_empty() {
            return Ok(());
        }
        let n = i16_at(d, 0).ok_or(FontError::MalformedGlyph)?;
        if n >= 0 { load_simple(d, n as usize, s) } else { self.load_composite(d, depth, s) }
    }

    fn load_composite(&self, d: &[u8], depth: u32, s: &mut GlyfScratch) -> Result<(), FontError> {
        let bad = FontError::MalformedGlyph;
        let base = s.points.len();
        let mut r = Reader::at(d, 10);
        loop {
            s.components += 1;
            if s.components > MAX_COMPONENTS {
                return Err(FontError::LimitExceeded);
            }
            let flags = r.u16().ok_or(bad)?;
            let child = r.u16().ok_or(bad)?;
            let xy = flags & ARGS_ARE_XY_VALUES != 0;
            let (arg1, arg2) = if flags & ARG_1_AND_2_ARE_WORDS != 0 {
                let (a, b) = (r.u16().ok_or(bad)?, r.u16().ok_or(bad)?);
                if xy { (a as i16 as i32, b as i16 as i32) } else { (a as i32, b as i32) }
            } else {
                let (a, b) = (r.u8().ok_or(bad)?, r.u8().ok_or(bad)?);
                if xy { (a as i8 as i32, b as i8 as i32) } else { (a as i32, b as i32) }
            };
            let (mut a, mut b, mut c, mut dd) = (1.0f32, 0.0f32, 0.0f32, 1.0f32);
            if flags & WE_HAVE_A_SCALE != 0 {
                a = r.f2dot14().ok_or(bad)?;
                dd = a;
            } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
                a = r.f2dot14().ok_or(bad)?;
                dd = r.f2dot14().ok_or(bad)?;
            } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
                a = r.f2dot14().ok_or(bad)?;
                b = r.f2dot14().ok_or(bad)?;
                c = r.f2dot14().ok_or(bad)?;
                dd = r.f2dot14().ok_or(bad)?;
            }
            let start = s.points.len();
            self.load(child, depth + 1, s)?;
            let transformed = a != 1.0 || b != 0.0 || c != 0.0 || dd != 1.0;
            if transformed {
                for p in &mut s.points[start..] {
                    let (x, y) = (p.x, p.y);
                    p.x = a * x + c * y;
                    p.y = b * x + dd * y;
                }
            }
            let (dx, dy) = if xy {
                let (x, y) = (arg1 as f32, arg2 as f32);
                if transformed && flags & SCALED_COMPONENT_OFFSET != 0 && flags & UNSCALED_COMPONENT_OFFSET == 0 {
                    (a * x + c * y, b * x + dd * y)
                } else {
                    (x, y)
                }
            } else {
                // Point matching: align point arg1 of the glyph so far with point arg2 of the child.
                let parent = s.points.get(base + arg1 as usize).filter(|_| base + (arg1 as usize) < start);
                let own = s.points.get(start + arg2 as usize);
                match (parent, own) {
                    (Some(p), Some(q)) => (p.x - q.x, p.y - q.y),
                    _ => return Err(bad),
                }
            };
            if dx != 0.0 || dy != 0.0 {
                for p in &mut s.points[start..] {
                    p.x += dx;
                    p.y += dy;
                }
            }
            if flags & MORE_COMPONENTS == 0 {
                return Ok(());
            }
        }
    }
}

/// Decodes a simple glyph with `n` contours and appends it to `s`.
fn load_simple(d: &[u8], n: usize, s: &mut GlyfScratch) -> Result<(), FontError> {
    let bad = FontError::MalformedGlyph;
    if n == 0 {
        return Ok(());
    }
    let base = s.points.len();
    let first_end = s.ends.len();
    let mut num_points = 0usize;
    for i in 0..n {
        let e = u16_at(d, 10 + i * 2).ok_or(bad)? as usize + 1;
        if e < num_points {
            return Err(bad);
        }
        num_points = e;
        s.ends.push(base + e);
    }
    if base + num_points > MAX_POINTS {
        return Err(FontError::LimitExceeded);
    }
    let ins_len = u16_at(d, 10 + n * 2).ok_or(bad)? as usize;
    let mut r = Reader::at(d, 12 + n * 2);
    r.skip(ins_len).ok_or(bad)?;
    // Flags.
    s.flags.clear();
    while s.flags.len() < num_points {
        let f = r.u8().ok_or(bad)?;
        s.flags.push(f);
        if f & REPEAT != 0 {
            let count = r.u8().ok_or(bad)? as usize;
            let count = count.min(num_points - s.flags.len());
            for _ in 0..count {
                s.flags.push(f);
            }
        }
    }
    // X coordinates.
    s.points.resize(base + num_points, GPoint::default());
    let mut x = 0i32;
    for i in 0..num_points {
        let f = s.flags[i];
        if f & X_SHORT != 0 {
            let v = r.u8().ok_or(bad)? as i32;
            x += if f & X_SAME_OR_POSITIVE != 0 { v } else { -v };
        } else if f & X_SAME_OR_POSITIVE == 0 {
            x += r.i16().ok_or(bad)? as i32;
        }
        let p = &mut s.points[base + i];
        p.x = x as f32;
        p.on = f & ON_CURVE != 0;
    }
    let mut y = 0i32;
    for i in 0..num_points {
        let f = s.flags[i];
        if f & Y_SHORT != 0 {
            let v = r.u8().ok_or(bad)? as i32;
            y += if f & Y_SAME_OR_POSITIVE != 0 { v } else { -v };
        } else if f & Y_SAME_OR_POSITIVE == 0 {
            y += r.i16().ok_or(bad)? as i32;
        }
        s.points[base + i].y = y as f32;
    }
    // Drop empty contours (repeated end indices).
    let mut prev = base;
    let mut w = first_end;
    for i in first_end..s.ends.len() {
        let e = s.ends[i];
        if e > prev {
            s.ends[w] = e;
            w += 1;
            prev = e;
        }
    }
    s.ends.truncate(w);
    Ok(())
}

/// Converts the decoded quadratic B-spline contours into path commands.
fn emit<S: OutlineSink + ?Sized>(s: &GlyfScratch, sink: &mut S) {
    let mut start = 0usize;
    for &end in &s.ends {
        let pts = &s.points[start..end.min(s.points.len())];
        start = end;
        let n = pts.len();
        if n == 0 {
            continue;
        }
        // Pick the starting on-curve point.
        let (first, begin) = if pts[0].on {
            ((pts[0].x, pts[0].y), 1)
        } else if pts[n - 1].on {
            ((pts[n - 1].x, pts[n - 1].y), 0)
        } else {
            (((pts[0].x + pts[n - 1].x) * 0.5, (pts[0].y + pts[n - 1].y) * 0.5), 0)
        };
        // When starting from the last point, it must not be visited again.
        let count = if !pts[0].on && pts[n - 1].on { n - 1 } else { n - begin };
        sink.move_to(first.0, first.1);
        let mut ctrl: Option<(f32, f32)> = None;
        for k in 0..count {
            let p = pts[(begin + k) % n];
            if p.on {
                match ctrl.take() {
                    Some(c) => sink.quad_to(c.0, c.1, p.x, p.y),
                    None => sink.line_to(p.x, p.y),
                }
            } else {
                if let Some(c) = ctrl {
                    sink.quad_to(c.0, c.1, (c.0 + p.x) * 0.5, (c.1 + p.y) * 0.5);
                }
                ctrl = Some((p.x, p.y));
            }
        }
        match ctrl {
            Some(c) => sink.quad_to(c.0, c.1, first.0, first.1),
            None => sink.line_to(first.0, first.1),
        }
        sink.close();
    }
}
