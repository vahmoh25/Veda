//! Character to glyph mapping (`cmap` table).
//!
//! Supports subtable formats 4 (BMP segments), 12 (segmented coverage), 13 (many-to-one ranges),
//! 6 (trimmed table) and 0 (byte encoding). The best Unicode subtable is chosen in the order
//! (3,10), (0,6), (0,4), (3,1), (0,3), other Unicode encodings, then (3,0) symbol.

use crate::parse::{u16_at, u32_at};

/// A selected cmap subtable.
#[derive(Clone, Copy)]
pub(crate) struct Cmap<'a> {
    data: &'a [u8],
    kind: Kind,
    /// Microsoft symbol encoding: characters live at U+F000 + code.
    symbol: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    /// Format 0: 256 glyph id bytes at offset 6.
    Format0,
    /// Format 4 with `seg_count` segments.
    Format4 { seg_count: usize },
    /// Format 6: `first` code, `count` entries.
    Format6 { first: u32, count: u32 },
    /// Format 12 (`constant = false`) or 13 (`constant = true`) with `groups` groups.
    Groups { groups: u32, constant: bool },
}

/// Ranks an encoding record; lower is better, `None` = not usable.
fn rank(platform: u16, encoding: u16) -> Option<u8> {
    Some(match (platform, encoding) {
        (3, 10) => 0,
        (0, 6) => 1,
        (0, 4) => 2,
        (3, 1) => 3,
        (0, 3) => 4,
        (0, _) => 5,
        (3, 0) => 6,
        _ => return None,
    })
}

impl<'a> Cmap<'a> {
    /// Selects the best supported subtable of a `cmap` table (`None` if there is none).
    pub(crate) fn parse(cmap: &'a [u8]) -> Option<Cmap<'a>> {
        let num = u16_at(cmap, 2)? as usize;
        let mut best: Option<(u8, Cmap<'a>)> = None;
        for i in 0..num {
            let rec = 4 + i * 8;
            let (Some(platform), Some(encoding), Some(offset)) =
                (u16_at(cmap, rec), u16_at(cmap, rec + 2), u32_at(cmap, rec + 4))
            else {
                break;
            };
            let Some(r) = rank(platform, encoding) else { continue };
            if best.as_ref().is_some_and(|(b, _)| *b <= r) {
                continue;
            }
            let Some(sub) = cmap.get(offset as usize..) else { continue };
            if let Some(mut c) = Cmap::subtable(sub) {
                c.symbol = (platform, encoding) == (3, 0);
                best = Some((r, c));
            }
        }
        best.map(|(_, c)| c)
    }

    /// Validates a subtable and determines its kind.
    fn subtable(d: &'a [u8]) -> Option<Cmap<'a>> {
        let format = u16_at(d, 0)?;
        let kind = match format {
            0 => {
                d.get(6..6 + 256)?;
                Kind::Format0
            }
            4 => {
                let seg_count = (u16_at(d, 6)? / 2) as usize;
                if seg_count == 0 {
                    return None;
                }
                // endCode, pad, startCode, idDelta, idRangeOffset arrays must be present.
                d.get(14..16 + seg_count * 8)?;
                Kind::Format4 { seg_count }
            }
            6 => {
                let first = u16_at(d, 6)? as u32;
                let count = u16_at(d, 8)? as u32;
                d.get(10..10 + count as usize * 2)?;
                Kind::Format6 { first, count }
            }
            12 | 13 => {
                let groups = u32_at(d, 12)?;
                let len = (groups as usize).checked_mul(12)?.checked_add(16)?;
                d.get(..len)?;
                Kind::Groups { groups, constant: format == 13 }
            }
            _ => return None,
        };
        Some(Cmap { data: d, kind, symbol: false })
    }

    /// Maps a Unicode scalar value to a glyph id (`None` for unmapped characters or glyph 0).
    pub(crate) fn lookup(&self, c: u32) -> Option<u16> {
        let g = self.lookup_raw(c);
        if g.is_none() && self.symbol && c < 0x100 {
            return self.lookup_raw(0xF000 + c);
        }
        g
    }

    fn lookup_raw(&self, c: u32) -> Option<u16> {
        let d = self.data;
        let g = match self.kind {
            Kind::Format0 => {
                if c >= 256 {
                    return None;
                }
                *d.get(6 + c as usize)? as u16
            }
            Kind::Format4 { seg_count } => {
                if c > 0xFFFF {
                    return None;
                }
                let ends = 14;
                let starts = 16 + seg_count * 2;
                let deltas = starts + seg_count * 2;
                let ranges = deltas + seg_count * 2;
                // First segment whose end code is >= c.
                let (mut lo, mut hi) = (0usize, seg_count);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if (u16_at(d, ends + mid * 2)? as u32) < c {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                if lo >= seg_count {
                    return None;
                }
                let start = u16_at(d, starts + lo * 2)? as u32;
                if c < start {
                    return None;
                }
                let delta = u16_at(d, deltas + lo * 2)?;
                let ro_pos = ranges + lo * 2;
                let ro = u16_at(d, ro_pos)? as usize;
                if ro == 0 {
                    (c as u16).wrapping_add(delta)
                } else {
                    let addr = ro_pos + ro + (c - start) as usize * 2;
                    let g = u16_at(d, addr)?;
                    if g == 0 {
                        return None;
                    }
                    g.wrapping_add(delta)
                }
            }
            Kind::Format6 { first, count } => {
                let i = c.checked_sub(first)?;
                if i >= count {
                    return None;
                }
                u16_at(d, 10 + i as usize * 2)?
            }
            Kind::Groups { groups, constant } => {
                let (mut lo, mut hi) = (0u32, groups);
                let mut found = None;
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let rec = 16 + mid as usize * 12;
                    let start = u32_at(d, rec)?;
                    let end = u32_at(d, rec + 4)?;
                    if c < start {
                        hi = mid;
                    } else if c > end {
                        lo = mid + 1;
                    } else {
                        let base = u32_at(d, rec + 8)?;
                        found = Some(if constant { base } else { base.checked_add(c - start)? });
                        break;
                    }
                }
                let g = found?;
                if g > 0xFFFF {
                    return None;
                }
                g as u16
            }
        };
        if g == 0 { None } else { Some(g) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn be16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_be_bytes());
    }

    #[test]
    fn format4_and_12() {
        // cmap with one (3,1) format 4 subtable: 'A'..'C' -> 10..12 via delta, 'a'..'b' via
        // glyphIdArray, plus the 0xFFFF terminator.
        let mut sub = Vec::new();
        let seg = 3u16;
        be16(&mut sub, 4);
        be16(&mut sub, 0);
        be16(&mut sub, 0);
        be16(&mut sub, seg * 2);
        for _ in 0..3 {
            be16(&mut sub, 0);
        }
        for e in [0x43, 0x62, 0xFFFF] {
            be16(&mut sub, e);
        }
        be16(&mut sub, 0);
        for s in [0x41, 0x61, 0xFFFF] {
            be16(&mut sub, s);
        }
        for d in [10u16.wrapping_sub(0x41), 0, 1] {
            be16(&mut sub, d);
        }
        // idRangeOffset for segment 1 points to glyphIdArray right after the array (2 entries
        // later = 4 bytes from its own position).
        for r in [0u16, 4, 0] {
            be16(&mut sub, r);
        }
        be16(&mut sub, 20);
        be16(&mut sub, 21);
        let mut cmap = Vec::new();
        be16(&mut cmap, 0);
        be16(&mut cmap, 1);
        be16(&mut cmap, 3);
        be16(&mut cmap, 1);
        cmap.extend_from_slice(&12u32.to_be_bytes());
        cmap.extend_from_slice(&sub);
        let c = Cmap::parse(&cmap).unwrap();
        assert_eq!(c.lookup('A' as u32), Some(10));
        assert_eq!(c.lookup('C' as u32), Some(12));
        assert_eq!(c.lookup('D' as u32), None);
        assert_eq!(c.lookup('a' as u32), Some(20));
        assert_eq!(c.lookup('b' as u32), Some(21));
        assert_eq!(c.lookup(0x1F600), None);

        // Format 12 subtable with two groups.
        let mut sub = Vec::new();
        be16(&mut sub, 12);
        be16(&mut sub, 0);
        sub.extend_from_slice(&40u32.to_be_bytes());
        sub.extend_from_slice(&0u32.to_be_bytes());
        sub.extend_from_slice(&2u32.to_be_bytes());
        for (s, e, g) in [(0x20u32, 0x7Eu32, 1u32), (0x1F600, 0x1F64F, 500)] {
            sub.extend_from_slice(&s.to_be_bytes());
            sub.extend_from_slice(&e.to_be_bytes());
            sub.extend_from_slice(&g.to_be_bytes());
        }
        let c = Cmap::subtable(&sub).unwrap();
        assert_eq!(c.lookup(' ' as u32), Some(1));
        assert_eq!(c.lookup('~' as u32), Some(1 + 0x7E - 0x20));
        assert_eq!(c.lookup(0x1F601), Some(501));
        assert_eq!(c.lookup(0x7F), None);
        // Truncated subtables are rejected, not read out of bounds.
        assert!(Cmap::subtable(&sub[..30]).is_none());
    }
}
