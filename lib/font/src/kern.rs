//! Pair kerning.
//!
//! Kerning values come from the GPOS table when it has PairPos lookups (lookup type 2, formats 1
//! and 2, possibly wrapped in extension lookups of type 9) referenced by the `kern` feature of the
//! preferred script (`latn`, then `DFLT`, then the first script; all `kern` features if that yields
//! nothing). Otherwise the legacy `kern` table (format 0 subtables, Microsoft and Apple headers) is
//! used. Only the first glyph's x-advance adjustment is applied (standard pair kerning).
//!
//! Lookups are cached in a small direct-mapped table per font ([`KernCache`]).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;

use crate::parse::{i16_at, tag_at, u16_at, u32_at};

/// Upper bound on the number of PairPos subtables considered.
const MAX_SUBTABLES: usize = 4096;

/// Kerning data of a font.
#[derive(Clone)]
pub(crate) enum Kerning<'a> {
    /// No kerning.
    None,
    /// GPOS PairPos subtables grouped by lookup.
    Gpos {
        data: &'a [u8],
        /// Offsets (within GPOS) of PairPos subtables, in lookup order.
        subtables: Vec<u32>,
        /// Exclusive end index into `subtables` of every lookup.
        lookup_ends: Vec<u32>,
    },
    /// Format 0 subtables of the `kern` table: (pairs data, number of pairs, override flag).
    Kern(Vec<(&'a [u8], u32, bool)>),
}

impl<'a> Kerning<'a> {
    /// Builds the kerning data from the GPOS and kern tables (either may be empty).
    pub(crate) fn new(gpos: &'a [u8], kern: &'a [u8]) -> Kerning<'a> {
        if let Some(k) = parse_gpos(gpos) {
            return k;
        }
        parse_kern(kern).unwrap_or(Kerning::None)
    }

    /// Returns `true` if the font has any kerning data.
    pub(crate) fn is_some(&self) -> bool {
        !matches!(self, Kerning::None)
    }

    /// The kerning adjustment (font units) between `left` and `right`.
    pub(crate) fn lookup(&self, left: u16, right: u16) -> i16 {
        match self {
            Kerning::None => 0,
            Kerning::Gpos { data, subtables, lookup_ends } => {
                let mut total = 0i32;
                let mut start = 0usize;
                for &end in lookup_ends {
                    let end = end as usize;
                    for &st in &subtables[start..end] {
                        if let Some(v) = pair_value(data, st as usize, left, right) {
                            total += v as i32;
                            break;
                        }
                    }
                    start = end;
                }
                total.clamp(i16::MIN as i32, i16::MAX as i32) as i16
            }
            Kerning::Kern(subs) => {
                let key = ((left as u32) << 16) | right as u32;
                let mut total = 0i32;
                for &(pairs, n, overrides) in subs {
                    let (mut lo, mut hi) = (0u32, n);
                    while lo < hi {
                        let mid = lo + (hi - lo) / 2;
                        let rec = mid as usize * 6;
                        let Some(k) = u32_at(pairs, rec) else { break };
                        if k < key {
                            lo = mid + 1;
                        } else if k > key {
                            hi = mid;
                        } else {
                            let v = i16_at(pairs, rec + 4).unwrap_or(0) as i32;
                            total = if overrides { v } else { total + v };
                            break;
                        }
                    }
                }
                total.clamp(i16::MIN as i32, i16::MAX as i32) as i16
            }
        }
    }
}

/// Feature indices of the preferred script's default language system.
fn preferred_features(d: &[u8], script_list: usize) -> Option<Vec<u16>> {
    let n = u16_at(d, script_list)? as usize;
    let mut chosen = None;
    for want in [*b"latn", *b"DFLT"] {
        for i in 0..n {
            let rec = script_list + 2 + i * 6;
            if tag_at(d, rec)? == want {
                chosen = Some(rec);
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
    }
    let rec = match chosen {
        Some(r) => r,
        None if n > 0 => script_list + 2,
        None => return None,
    };
    let script = script_list + u16_at(d, rec + 4)? as usize;
    let mut ls_off = u16_at(d, script)? as usize;
    if ls_off == 0 {
        // No default language system: use the first one.
        if u16_at(d, script + 2)? == 0 {
            return None;
        }
        ls_off = u16_at(d, script + 8)? as usize;
    }
    let ls = script + ls_off;
    let required = u16_at(d, ls + 2)?;
    let count = u16_at(d, ls + 4)? as usize;
    let mut out = Vec::with_capacity(count + 1);
    if required != 0xFFFF {
        out.push(required);
    }
    for i in 0..count {
        out.push(u16_at(d, ls + 6 + i * 2)?);
    }
    Some(out)
}

/// Appends the lookup indices of feature `fi` to `out` if it is a `kern` feature.
fn kern_feature_lookups(d: &[u8], feature_list: usize, fi: u16, out: &mut Vec<u16>) -> Option<()> {
    let n = u16_at(d, feature_list)?;
    if fi >= n {
        return None;
    }
    let rec = feature_list + 2 + fi as usize * 6;
    if tag_at(d, rec)? != *b"kern" {
        return Some(());
    }
    let f = feature_list + u16_at(d, rec + 4)? as usize;
    let count = u16_at(d, f + 2)? as usize;
    for i in 0..count {
        out.push(u16_at(d, f + 4 + i * 2)?);
    }
    Some(())
}

fn parse_gpos(d: &[u8]) -> Option<Kerning<'_>> {
    if u16_at(d, 0)? != 1 {
        return None;
    }
    let script_list = u16_at(d, 4)? as usize;
    let feature_list = u16_at(d, 6)? as usize;
    let lookup_list = u16_at(d, 8)? as usize;
    let mut lookups = Vec::new();
    if let Some(features) = preferred_features(d, script_list) {
        for fi in features {
            let _ = kern_feature_lookups(d, feature_list, fi, &mut lookups);
        }
    }
    if lookups.is_empty() {
        for fi in 0..u16_at(d, feature_list)? {
            let _ = kern_feature_lookups(d, feature_list, fi, &mut lookups);
        }
    }
    lookups.sort_unstable();
    lookups.dedup();
    let n_lookups = u16_at(d, lookup_list)?;
    let mut subtables = Vec::new();
    let mut lookup_ends = Vec::new();
    for li in lookups {
        if li >= n_lookups {
            continue;
        }
        let Some(lo) = u16_at(d, lookup_list + 2 + li as usize * 2).map(|o| lookup_list + o as usize) else {
            continue;
        };
        let (Some(kind), Some(count)) = (u16_at(d, lo), u16_at(d, lo + 4)) else { continue };
        let before = subtables.len();
        for si in 0..count as usize {
            let Some(mut st) = u16_at(d, lo + 6 + si * 2).map(|o| lo + o as usize) else { break };
            let mut t = kind;
            if kind == 9 {
                let (Some(ext_type), Some(off)) = (u16_at(d, st + 2), u32_at(d, st + 4)) else { continue };
                t = ext_type;
                st += off as usize;
            }
            if t != 2 || !matches!(u16_at(d, st), Some(1) | Some(2)) || subtables.len() >= MAX_SUBTABLES {
                continue;
            }
            subtables.push(st as u32);
        }
        if subtables.len() > before {
            lookup_ends.push(subtables.len() as u32);
        }
    }
    if subtables.is_empty() {
        return None;
    }
    Some(Kerning::Gpos { data: d, subtables, lookup_ends })
}

fn parse_kern(d: &[u8]) -> Option<Kerning<'_>> {
    let mut subs = Vec::new();
    let version = u16_at(d, 0)?;
    if version == 0 {
        // Microsoft: u16 version, u16 nTables; subtable: version, length, coverage (format in the
        // high byte; 1 = horizontal, 4 = cross-stream, 8 = override).
        let n = u16_at(d, 2)?;
        let mut pos = 4usize;
        for _ in 0..n {
            let len = u16_at(d, pos + 2)? as usize;
            let coverage = u16_at(d, pos + 4)?;
            let format = coverage >> 8;
            let n_pairs = u16_at(d, pos + 6)? as u32;
            if format == 0 && coverage & 0x1 != 0 && coverage & 0x6 == 0 {
                let pairs = d.get(pos + 14..)?;
                let n_pairs = n_pairs.min((pairs.len() / 6) as u32);
                subs.push((pairs, n_pairs, coverage & 0x8 != 0));
            }
            if len < 6 {
                break;
            }
            // Subtable lengths are 16-bit and may overflow for large format 0 tables.
            pos += if format == 0 { 14 + n_pairs as usize * 6 } else { len };
        }
    } else if version == 1 && u16_at(d, 2)? == 0 {
        // Apple: u32 version 0x00010000, u32 nTables; subtable: u32 length, u16 coverage
        // (0x8000 vertical, 0x4000 cross-stream, format in the low byte), u16 tuple index.
        let n = u32_at(d, 4)?.min(64);
        let mut pos = 8usize;
        for _ in 0..n {
            let len = u32_at(d, pos)? as usize;
            let coverage = u16_at(d, pos + 4)?;
            if coverage & 0xFF == 0 && coverage & 0xE000 == 0 {
                let n_pairs = u16_at(d, pos + 8)? as u32;
                let pairs = d.get(pos + 16..)?;
                subs.push((pairs, n_pairs.min((pairs.len() / 6) as u32), false));
            }
            if len < 8 {
                break;
            }
            pos = pos.checked_add(len)?;
        }
    }
    if subs.is_empty() { None } else { Some(Kerning::Kern(subs)) }
}

/// Index of `g` in a coverage table.
fn coverage_index(d: &[u8], cov: usize, g: u16) -> Option<u16> {
    match u16_at(d, cov)? {
        1 => {
            let n = u16_at(d, cov + 2)? as usize;
            let (mut lo, mut hi) = (0usize, n);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let v = u16_at(d, cov + 4 + mid * 2)?;
                if v < g {
                    lo = mid + 1;
                } else if v > g {
                    hi = mid;
                } else {
                    return Some(mid as u16);
                }
            }
            None
        }
        2 => {
            let n = u16_at(d, cov + 2)? as usize;
            let (mut lo, mut hi) = (0usize, n);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let rec = cov + 4 + mid * 6;
                let start = u16_at(d, rec)?;
                let end = u16_at(d, rec + 2)?;
                if g < start {
                    hi = mid;
                } else if g > end {
                    lo = mid + 1;
                } else {
                    return Some(u16_at(d, rec + 4)?.wrapping_add(g - start));
                }
            }
            None
        }
        _ => None,
    }
}

/// Class of `g` in a class definition table (0 if not listed).
fn class_of(d: &[u8], cd: usize, g: u16) -> u16 {
    (|| -> Option<u16> {
        match u16_at(d, cd)? {
            1 => {
                let start = u16_at(d, cd + 2)?;
                let n = u16_at(d, cd + 4)?;
                if g < start || g - start >= n {
                    return Some(0);
                }
                u16_at(d, cd + 6 + (g - start) as usize * 2)
            }
            2 => {
                let n = u16_at(d, cd + 2)? as usize;
                let (mut lo, mut hi) = (0usize, n);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let rec = cd + 4 + mid * 6;
                    let start = u16_at(d, rec)?;
                    let end = u16_at(d, rec + 2)?;
                    if g < start {
                        hi = mid;
                    } else if g > end {
                        lo = mid + 1;
                    } else {
                        return u16_at(d, rec + 4);
                    }
                }
                Some(0)
            }
            _ => Some(0),
        }
    })()
    .unwrap_or(0)
}

/// Size in bytes of a ValueRecord with the given format.
#[inline]
fn value_size(format: u16) -> usize {
    (format & 0xFF).count_ones() as usize * 2
}

/// The x-advance of the first glyph if the PairPos subtable at `st` applies to the pair.
fn pair_value(d: &[u8], st: usize, g1: u16, g2: u16) -> Option<i16> {
    let format = u16_at(d, st)?;
    let cov = st + u16_at(d, st + 2)? as usize;
    let vf1 = u16_at(d, st + 4)?;
    let vf2 = u16_at(d, st + 6)?;
    let ci = coverage_index(d, cov, g1)? as usize;
    let (s1, s2) = (value_size(vf1), value_size(vf2));
    // XAdvance follows XPlacement and YPlacement when present.
    let x_adv = if vf1 & 0x0004 != 0 { Some((vf1 & 0x0003).count_ones() as usize * 2) } else { None };
    match format {
        1 => {
            if ci >= u16_at(d, st + 8)? as usize {
                return None;
            }
            let ps = st + u16_at(d, st + 10 + ci * 2)? as usize;
            let n = u16_at(d, ps)? as usize;
            let rec_size = 2 + s1 + s2;
            let (mut lo, mut hi) = (0usize, n);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let rec = ps + 2 + mid * rec_size;
                let second = u16_at(d, rec)?;
                if second < g2 {
                    lo = mid + 1;
                } else if second > g2 {
                    hi = mid;
                } else {
                    return Some(match x_adv {
                        Some(o) => i16_at(d, rec + 2 + o)?,
                        None => 0,
                    });
                }
            }
            None
        }
        2 => {
            let cd1 = st + u16_at(d, st + 8)? as usize;
            let cd2 = st + u16_at(d, st + 10)? as usize;
            let n1 = u16_at(d, st + 12)? as usize;
            let n2 = u16_at(d, st + 14)? as usize;
            let c1 = class_of(d, cd1, g1) as usize;
            let c2 = class_of(d, cd2, g2) as usize;
            if c1 >= n1 || c2 >= n2 {
                return None;
            }
            let rec = st + 16 + (c1 * n2 + c2) * (s1 + s2);
            Some(match x_adv {
                Some(o) => i16_at(d, rec + o)?,
                None => 0,
            })
        }
        _ => None,
    }
}

/// Number of entries of the per-font kerning cache (power of two).
const CACHE_SLOTS: usize = 1024;

/// A direct-mapped cache of kerning values keyed by glyph pair. Uses interior mutability so that
/// lookups work through shared font references (fonts are used single-threaded).
#[derive(Clone)]
pub(crate) struct KernCache {
    slots: Box<[Cell<u64>]>,
}

impl KernCache {
    pub(crate) fn new() -> Self {
        KernCache { slots: (0..CACHE_SLOTS).map(|_| Cell::new(0)).collect() }
    }

    #[inline]
    fn slot(key: u32) -> usize {
        (key.wrapping_mul(0x9E37_79B1) >> (32 - CACHE_SLOTS.trailing_zeros())) as usize
    }

    /// Entry layout: key in the high 32 bits, value in bits 16..32, bit 0 = valid.
    #[inline]
    pub(crate) fn get(&self, key: u32) -> Option<i16> {
        let e = self.slots[Self::slot(key)].get();
        if e & 1 != 0 && (e >> 32) as u32 == key { Some((e >> 16) as u16 as i16) } else { None }
    }

    #[inline]
    pub(crate) fn put(&self, key: u32, v: i16) {
        self.slots[Self::slot(key)].set(((key as u64) << 32) | ((v as u16 as u64) << 16) | 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kern_table_format0() {
        // Microsoft kern table with one horizontal format 0 subtable: (1,2) = -50, (3,4) = 20.
        let mut d = alloc::vec::Vec::new();
        for v in [0u16, 1, 0, 14 + 12, 0x0001, 2, 12, 1, 0] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        for (l, r, v) in [(1u16, 2u16, -50i16), (3, 4, 20)] {
            d.extend_from_slice(&l.to_be_bytes());
            d.extend_from_slice(&r.to_be_bytes());
            d.extend_from_slice(&v.to_be_bytes());
        }
        let k = Kerning::new(&[], &d);
        assert!(k.is_some());
        assert_eq!(k.lookup(1, 2), -50);
        assert_eq!(k.lookup(3, 4), 20);
        assert_eq!(k.lookup(2, 1), 0);
        // Truncated tables never panic.
        for n in 0..d.len() {
            let k = Kerning::new(&[], &d[..n]);
            let _ = k.lookup(1, 2);
        }
    }

    #[test]
    fn cache_roundtrip() {
        let c = KernCache::new();
        assert_eq!(c.get(0x0001_0002), None);
        c.put(0x0001_0002, -123);
        assert_eq!(c.get(0x0001_0002), Some(-123));
        c.put(0x0001_0002, 0);
        assert_eq!(c.get(0x0001_0002), Some(0));
    }
}
