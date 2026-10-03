//! CFF outlines (`CFF ` table, Type 2 charstrings).
//!
//! The parser reads the header, the Top DICT (CharStrings, Private, and for CID-keyed fonts
//! FDArray/FDSelect), the Private DICTs (local Subrs, default/nominal width) and the global Subrs.
//! The charstring interpreter supports every path operator including the flex family, local and
//! global subroutines with bias, the hint operators (stems are counted so that `hintmask` and
//! `cntrmask` skip the right number of mask bytes), the optional leading width argument and
//! `endchar`. Hints themselves are ignored; `seac` accents are not supported (the base glyph is
//! drawn). Nesting depth, stack size and the number of executed operations are bounded.

use alloc::vec::Vec;

use crate::FontError;
use crate::outline::OutlineSink;
use crate::parse::{i16_at, u8_at, u16_at, u24_at, u32_at};

/// Type 2 argument stack limit.
const MAX_STACK: usize = 48;
/// Type 2 subroutine nesting limit.
const MAX_CALL_DEPTH: usize = 10;
/// Upper bound for operators executed per glyph (guards against exponential subroutine fan-out).
const MAX_OPS: u32 = 200_000;
/// Upper bound for the number of font dicts in a CID-keyed font.
const MAX_FDS: u32 = 256;

/// A CFF INDEX structure.
#[derive(Clone, Copy, Default)]
pub(crate) struct Index<'a> {
    data: &'a [u8],
    count: u32,
    off_size: u8,
    offsets: usize,
    base: usize,
}

fn read_offset(d: &[u8], pos: usize, size: u8) -> Option<usize> {
    Some(match size {
        1 => u8_at(d, pos)? as usize,
        2 => u16_at(d, pos)? as usize,
        3 => u24_at(d, pos)? as usize,
        4 => u32_at(d, pos)? as usize,
        _ => return None,
    })
}

impl<'a> Index<'a> {
    /// Parses an INDEX at `pos`; returns it and the position just after it.
    fn parse(data: &'a [u8], pos: usize) -> Option<(Index<'a>, usize)> {
        let count = u16_at(data, pos)? as u32;
        if count == 0 {
            return Some((Index { data, ..Index::default() }, pos + 2));
        }
        let off_size = u8_at(data, pos + 2)?;
        if !(1..=4).contains(&off_size) {
            return None;
        }
        let offsets = pos + 3;
        let base = offsets + (count as usize + 1) * off_size as usize - 1;
        let last = read_offset(data, offsets + count as usize * off_size as usize, off_size)?;
        let end = base.checked_add(last)?;
        if end > data.len() {
            return None;
        }
        Some((Index { data, count, off_size, offsets, base }, end))
    }

    /// Number of objects.
    pub(crate) fn len(&self) -> u32 {
        self.count
    }

    /// Object `i`.
    pub(crate) fn get(&self, i: u32) -> Option<&'a [u8]> {
        if i >= self.count {
            return None;
        }
        let a = read_offset(self.data, self.offsets + i as usize * self.off_size as usize, self.off_size)?;
        let b = read_offset(self.data, self.offsets + (i as usize + 1) * self.off_size as usize, self.off_size)?;
        if a == 0 || b < a {
            return None;
        }
        self.data.get(self.base + a..self.base + b)
    }
}

/// Subroutine index bias for `count` subroutines.
fn bias(count: u32) -> i32 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// Parses a DICT, calling `f(operator, operands)` for every entry. Two-byte operators are
/// `0x0C00 | second byte`.
fn parse_dict(d: &[u8], mut f: impl FnMut(u16, &[f64])) -> Option<()> {
    let mut stack = [0.0f64; MAX_STACK];
    let mut sp = 0usize;
    let mut i = 0usize;
    while i < d.len() {
        let b0 = d[i];
        let v = match b0 {
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    0x0C00 | *d.get(i)? as u16
                } else {
                    b0 as u16
                };
                i += 1;
                f(op, &stack[..sp]);
                sp = 0;
                continue;
            }
            28 => {
                let v = i16_at(d, i + 1)? as f64;
                i += 3;
                v
            }
            29 => {
                let v = u32_at(d, i + 1)? as i32 as f64;
                i += 5;
                v
            }
            30 => {
                i += 1;
                parse_real(d, &mut i)?
            }
            32..=246 => {
                i += 1;
                b0 as f64 - 139.0
            }
            247..=250 => {
                let b1 = *d.get(i + 1)? as f64;
                i += 2;
                (b0 as f64 - 247.0) * 256.0 + b1 + 108.0
            }
            251..=254 => {
                let b1 = *d.get(i + 1)? as f64;
                i += 2;
                -(b0 as f64 - 251.0) * 256.0 - b1 - 108.0
            }
            _ => return None,
        };
        if sp >= MAX_STACK {
            return None;
        }
        stack[sp] = v;
        sp += 1;
    }
    Some(())
}

/// Parses a DICT real number (BCD nibbles); `i` points after the 30 prefix byte.
fn parse_real(d: &[u8], i: &mut usize) -> Option<f64> {
    let mut mant = 0.0f64;
    let mut frac_digits = 0i32;
    let mut in_frac = false;
    let mut neg = false;
    let mut exp = 0i32;
    let mut exp_neg = false;
    let mut in_exp = false;
    loop {
        let b = *d.get(*i)?;
        *i += 1;
        for nib in [b >> 4, b & 0x0F] {
            match nib {
                0..=9 => {
                    if in_exp {
                        exp = (exp * 10 + nib as i32).min(1000);
                    } else if mant < 1e18 {
                        mant = mant * 10.0 + nib as f64;
                        if in_frac {
                            frac_digits += 1;
                        }
                    } else if !in_frac {
                        exp += 1;
                    }
                }
                0x0A => in_frac = true,
                0x0B => in_exp = true,
                0x0C => {
                    in_exp = true;
                    exp_neg = true;
                }
                0x0E => neg = true,
                0x0F => {
                    let e = if exp_neg { -exp } else { exp } - frac_digits;
                    let mut v = mant;
                    let mut k = e.clamp(-330, 330);
                    while k > 0 {
                        v *= 10.0;
                        k -= 1;
                    }
                    while k < 0 {
                        v /= 10.0;
                        k += 1;
                    }
                    return Some(if neg { -v } else { v });
                }
                _ => {}
            }
        }
    }
}

/// Values from a Private DICT.
#[derive(Clone, Copy, Default)]
struct Private<'a> {
    subrs: Index<'a>,
    bias: i32,
}

/// Glyph to font dict mapping of CID-keyed fonts.
#[derive(Clone, Copy)]
enum FdSelect<'a> {
    Format0(&'a [u8]),
    Format3(&'a [u8], u16),
}

impl FdSelect<'_> {
    fn fd(&self, gid: u16) -> Option<usize> {
        match *self {
            FdSelect::Format0(d) => u8_at(d, 1 + gid as usize).map(|v| v as usize),
            FdSelect::Format3(d, n) => {
                // Ranges at 3 + 3 * i: first glyph (u16), fd (u8); a sentinel glyph follows.
                let (mut lo, mut hi) = (0usize, n as usize);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let first = u16_at(d, 3 + mid * 3)?;
                    let next = u16_at(d, 3 + (mid + 1) * 3)?;
                    if gid < first {
                        hi = mid;
                    } else if gid >= next {
                        lo = mid + 1;
                    } else {
                        return u8_at(d, 3 + mid * 3 + 2).map(|v| v as usize);
                    }
                }
                None
            }
        }
    }
}

/// A parsed CFF font.
#[derive(Clone)]
pub(crate) struct Cff<'a> {
    charstrings: Index<'a>,
    gsubrs: Index<'a>,
    gbias: i32,
    privates: Vec<Private<'a>>,
    fd_select: Option<FdSelect<'a>>,
}

fn parse_private<'a>(d: &'a [u8], size: usize, offset: usize) -> Option<Private<'a>> {
    let pd = d.get(offset..offset.checked_add(size)?)?;
    let mut subrs_off = None;
    parse_dict(pd, |op, args| {
        if op == 19 {
            subrs_off = args.first().copied();
        }
    })?;
    let subrs = match subrs_off {
        Some(o) if o >= 0.0 => Index::parse(d, offset.checked_add(o as usize)?)?.0,
        _ => Index { data: d, ..Index::default() },
    };
    Some(Private { subrs, bias: bias(subrs.len()) })
}

impl<'a> Cff<'a> {
    /// Parses the `CFF ` table.
    pub(crate) fn parse(d: &'a [u8]) -> Result<Cff<'a>, FontError> {
        let bad = FontError::MalformedTable(*b"CFF ");
        let major = u8_at(d, 0).ok_or(bad)?;
        if major != 1 {
            return Err(FontError::UnsupportedFormat);
        }
        let hdr_size = u8_at(d, 2).ok_or(bad)? as usize;
        let (_names, p) = Index::parse(d, hdr_size).ok_or(bad)?;
        let (top_dicts, p) = Index::parse(d, p).ok_or(bad)?;
        let (_strings, p) = Index::parse(d, p).ok_or(bad)?;
        let (gsubrs, _) = Index::parse(d, p).ok_or(bad)?;
        let top = top_dicts.get(0).ok_or(bad)?;
        let mut charstrings = None;
        let mut private = None;
        let mut fd_array = None;
        let mut fd_select = None;
        let mut cid = false;
        let mut cs_type = 2.0;
        parse_dict(top, |op, args| match op {
            17 => charstrings = args.first().copied(),
            18 if args.len() >= 2 => private = Some((args[0], args[1])),
            0x0C06 => cs_type = args.first().copied().unwrap_or(2.0),
            0x0C1E => cid = true,
            0x0C24 => fd_array = args.first().copied(),
            0x0C25 => fd_select = args.first().copied(),
            _ => {}
        })
        .ok_or(bad)?;
        if cs_type != 2.0 {
            return Err(FontError::UnsupportedFormat);
        }
        let cs_off = charstrings.filter(|v| *v > 0.0).ok_or(bad)? as usize;
        let (charstrings, _) = Index::parse(d, cs_off).ok_or(bad)?;
        let mut privates = Vec::new();
        let mut select = None;
        if cid {
            let (Some(fa), Some(fs)) = (fd_array, fd_select) else { return Err(bad) };
            let (fds, _) = Index::parse(d, fa as usize).ok_or(bad)?;
            if fds.len() > MAX_FDS {
                return Err(bad);
            }
            for i in 0..fds.len() {
                let fd = fds.get(i).ok_or(bad)?;
                let mut pv = None;
                parse_dict(fd, |op, args| {
                    if op == 18 && args.len() >= 2 {
                        pv = Some((args[0], args[1]));
                    }
                })
                .ok_or(bad)?;
                let p = match pv {
                    Some((size, off)) => parse_private(d, size as usize, off as usize).ok_or(bad)?,
                    None => Private { subrs: Index { data: d, ..Index::default() }, bias: 107 },
                };
                privates.push(p);
            }
            let fs = fs as usize;
            select = Some(match u8_at(d, fs).ok_or(bad)? {
                0 => FdSelect::Format0(d.get(fs..).ok_or(bad)?),
                3 => FdSelect::Format3(d.get(fs..).ok_or(bad)?, u16_at(d, fs + 1).ok_or(bad)?),
                _ => return Err(bad),
            });
        } else {
            let p = match private {
                Some((size, off)) => parse_private(d, size as usize, off as usize).ok_or(bad)?,
                None => Private { subrs: Index { data: d, ..Index::default() }, bias: 107 },
            };
            privates.push(p);
        }
        Ok(Cff { charstrings, gbias: bias(gsubrs.len()), gsubrs, privates, fd_select: select })
    }

    /// Number of charstrings (should equal the number of glyphs).
    pub(crate) fn num_glyphs(&self) -> u32 {
        self.charstrings.len()
    }

    /// Runs the charstring of `gid`, emitting the outline to `sink`.
    pub(crate) fn outline<S: OutlineSink + ?Sized>(&self, gid: u16, sink: &mut S) -> Result<(), FontError> {
        let cs = self.charstrings.get(gid as u32).ok_or(FontError::InvalidGlyph)?;
        let private = match &self.fd_select {
            Some(sel) => sel.fd(gid).and_then(|i| self.privates.get(i)),
            None => self.privates.first(),
        }
        .ok_or(FontError::MalformedGlyph)?;
        let mut ip =
            Interp { stack: [0.0; MAX_STACK], sp: 0, x: 0.0, y: 0.0, nhints: 0, seen_width: false, open: false, sink };
        let r = ip.run(cs, &self.gsubrs, self.gbias, private);
        if ip.open {
            ip.sink.close();
        }
        r
    }
}

/// Type 2 charstring interpreter state.
struct Interp<'s, S: OutlineSink + ?Sized> {
    stack: [f32; MAX_STACK],
    sp: usize,
    x: f32,
    y: f32,
    nhints: u32,
    seen_width: bool,
    open: bool,
    sink: &'s mut S,
}

impl<S: OutlineSink + ?Sized> Interp<'_, S> {
    #[inline]
    fn push(&mut self, v: f32) -> Result<(), FontError> {
        if self.sp >= MAX_STACK {
            return Err(FontError::MalformedGlyph);
        }
        self.stack[self.sp] = v;
        self.sp += 1;
        Ok(())
    }

    /// Handles the optional width argument of the first stack-clearing operator; returns the index
    /// of the first real argument.
    #[inline]
    fn width(&mut self, has_extra: bool) -> usize {
        if self.seen_width {
            return 0;
        }
        self.seen_width = true;
        if has_extra { 1 } else { 0 }
    }

    /// Stem hint operators (and the implicit vstem of hintmask/cntrmask).
    fn stems(&mut self) {
        let first = self.width(self.sp % 2 == 1);
        self.nhints += ((self.sp - first.min(self.sp)) / 2) as u32;
        self.sp = 0;
    }

    fn move_rel(&mut self, dx: f32, dy: f32) {
        if self.open {
            self.sink.close();
        }
        self.x += dx;
        self.y += dy;
        self.sink.move_to(self.x, self.y);
        self.open = true;
    }

    #[inline]
    fn ensure_open(&mut self) {
        if !self.open {
            self.sink.move_to(self.x, self.y);
            self.open = true;
        }
    }

    fn line_rel(&mut self, dx: f32, dy: f32) {
        self.ensure_open();
        self.x += dx;
        self.y += dy;
        self.sink.line_to(self.x, self.y);
    }

    fn curve_rel(&mut self, dx1: f32, dy1: f32, dx2: f32, dy2: f32, dx3: f32, dy3: f32) {
        self.ensure_open();
        let (x1, y1) = (self.x + dx1, self.y + dy1);
        let (x2, y2) = (x1 + dx2, y1 + dy2);
        let (x3, y3) = (x2 + dx3, y2 + dy3);
        self.sink.curve_to(x1, y1, x2, y2, x3, y3);
        self.x = x3;
        self.y = y3;
    }

    fn run<'a>(&mut self, cs: &'a [u8], gsubrs: &Index<'a>, gbias: i32, pv: &Private<'a>) -> Result<(), FontError> {
        let bad = FontError::MalformedGlyph;
        let mut frames: [(&'a [u8], usize); MAX_CALL_DEPTH] = [(&[], 0); MAX_CALL_DEPTH];
        let mut depth = 0usize;
        let mut code = cs;
        let mut pc = 0usize;
        let mut ops = 0u32;
        loop {
            if pc >= code.len() {
                // End of a subroutine without `return`, or of the charstring without `endchar`.
                if depth == 0 {
                    return Ok(());
                }
                depth -= 1;
                (code, pc) = frames[depth];
                continue;
            }
            ops += 1;
            if ops > MAX_OPS {
                return Err(FontError::LimitExceeded);
            }
            let b0 = code[pc];
            pc += 1;
            match b0 {
                32..=246 => self.push(b0 as f32 - 139.0)?,
                247..=250 => {
                    let b1 = *code.get(pc).ok_or(bad)? as i32;
                    pc += 1;
                    self.push(((b0 as i32 - 247) * 256 + b1 + 108) as f32)?;
                }
                251..=254 => {
                    let b1 = *code.get(pc).ok_or(bad)? as i32;
                    pc += 1;
                    self.push((-(b0 as i32 - 251) * 256 - b1 - 108) as f32)?;
                }
                28 => {
                    let v = i16_at(code, pc).ok_or(bad)?;
                    pc += 2;
                    self.push(v as f32)?;
                }
                255 => {
                    let v = u32_at(code, pc).ok_or(bad)? as i32;
                    pc += 4;
                    self.push(v as f32 / 65536.0)?;
                }
                // hstem, vstem, hstemhm, vstemhm
                1 | 3 | 18 | 23 => self.stems(),
                // hintmask, cntrmask
                19 | 20 => {
                    self.stems();
                    pc += (self.nhints as usize).div_ceil(8);
                }
                // rmoveto
                21 => {
                    let i = self.width(self.sp > 2);
                    if self.sp < i + 2 {
                        return Err(bad);
                    }
                    let (dx, dy) = (self.stack[i], self.stack[i + 1]);
                    self.move_rel(dx, dy);
                    self.sp = 0;
                }
                // hmoveto, vmoveto
                22 | 4 => {
                    let i = self.width(self.sp > 1);
                    if self.sp < i + 1 {
                        return Err(bad);
                    }
                    let d = self.stack[i];
                    if b0 == 22 {
                        self.move_rel(d, 0.0)
                    } else {
                        self.move_rel(0.0, d)
                    }
                    self.sp = 0;
                }
                // rlineto
                5 => {
                    let mut i = 0;
                    while i + 2 <= self.sp {
                        self.line_rel(self.stack[i], self.stack[i + 1]);
                        i += 2;
                    }
                    self.sp = 0;
                }
                // hlineto, vlineto
                6 | 7 => {
                    let mut horizontal = b0 == 6;
                    for i in 0..self.sp {
                        let d = self.stack[i];
                        if horizontal {
                            self.line_rel(d, 0.0)
                        } else {
                            self.line_rel(0.0, d)
                        }
                        horizontal = !horizontal;
                    }
                    self.sp = 0;
                }
                // rrcurveto
                8 => {
                    let mut i = 0;
                    while i + 6 <= self.sp {
                        let s = &self.stack;
                        let (a, b, c, d, e, f) = (s[i], s[i + 1], s[i + 2], s[i + 3], s[i + 4], s[i + 5]);
                        self.curve_rel(a, b, c, d, e, f);
                        i += 6;
                    }
                    self.sp = 0;
                }
                // rcurveline
                24 => {
                    if self.sp < 8 {
                        return Err(bad);
                    }
                    let mut i = 0;
                    while i + 6 <= self.sp - 2 {
                        let s = &self.stack;
                        let (a, b, c, d, e, f) = (s[i], s[i + 1], s[i + 2], s[i + 3], s[i + 4], s[i + 5]);
                        self.curve_rel(a, b, c, d, e, f);
                        i += 6;
                    }
                    self.line_rel(self.stack[i], self.stack[i + 1]);
                    self.sp = 0;
                }
                // rlinecurve
                25 => {
                    if self.sp < 8 {
                        return Err(bad);
                    }
                    let mut i = 0;
                    while i + 2 <= self.sp - 6 {
                        self.line_rel(self.stack[i], self.stack[i + 1]);
                        i += 2;
                    }
                    let s = &self.stack;
                    let (a, b, c, d, e, f) = (s[i], s[i + 1], s[i + 2], s[i + 3], s[i + 4], s[i + 5]);
                    self.curve_rel(a, b, c, d, e, f);
                    self.sp = 0;
                }
                // vvcurveto
                26 => {
                    let mut i = 0;
                    let mut dx1 = 0.0;
                    if self.sp % 4 == 1 {
                        dx1 = self.stack[0];
                        i = 1;
                    }
                    while i + 4 <= self.sp {
                        let s = &self.stack;
                        let (a, b, c, d) = (s[i], s[i + 1], s[i + 2], s[i + 3]);
                        self.curve_rel(dx1, a, b, c, 0.0, d);
                        dx1 = 0.0;
                        i += 4;
                    }
                    self.sp = 0;
                }
                // hhcurveto
                27 => {
                    let mut i = 0;
                    let mut dy1 = 0.0;
                    if self.sp % 4 == 1 {
                        dy1 = self.stack[0];
                        i = 1;
                    }
                    while i + 4 <= self.sp {
                        let s = &self.stack;
                        let (a, b, c, d) = (s[i], s[i + 1], s[i + 2], s[i + 3]);
                        self.curve_rel(a, dy1, b, c, d, 0.0);
                        dy1 = 0.0;
                        i += 4;
                    }
                    self.sp = 0;
                }
                // vhcurveto, hvcurveto
                30 | 31 => {
                    let mut horizontal = b0 == 31;
                    let mut i = 0;
                    while i + 4 <= self.sp {
                        let last = self.sp - i == 5;
                        let s = &self.stack;
                        let (a, b, c, d) = (s[i], s[i + 1], s[i + 2], s[i + 3]);
                        let extra = if last { s[i + 4] } else { 0.0 };
                        if horizontal {
                            self.curve_rel(a, 0.0, b, c, extra, d);
                        } else {
                            self.curve_rel(0.0, a, b, c, d, extra);
                        }
                        i += if last { 5 } else { 4 };
                        horizontal = !horizontal;
                    }
                    self.sp = 0;
                }
                // callsubr, callgsubr
                10 | 29 => {
                    if self.sp == 0 {
                        return Err(bad);
                    }
                    self.sp -= 1;
                    let n = self.stack[self.sp];
                    if !n.is_finite() || n.abs() > 70_000.0 {
                        return Err(bad);
                    }
                    let (index, b) = if b0 == 10 { (&pv.subrs, pv.bias) } else { (gsubrs, gbias) };
                    let i = n as i32 + b;
                    if i < 0 {
                        return Err(bad);
                    }
                    let sub = index.get(i as u32).ok_or(bad)?;
                    if depth >= MAX_CALL_DEPTH {
                        return Err(FontError::LimitExceeded);
                    }
                    frames[depth] = (code, pc);
                    depth += 1;
                    code = sub;
                    pc = 0;
                }
                // return
                11 => {
                    if depth == 0 {
                        return Ok(());
                    }
                    depth -= 1;
                    (code, pc) = frames[depth];
                }
                // endchar (a seac accent's 4 extra arguments are ignored)
                14 => {
                    self.width(self.sp == 1 || self.sp == 5);
                    self.sp = 0;
                    return Ok(());
                }
                12 => {
                    let b1 = *code.get(pc).ok_or(bad)?;
                    pc += 1;
                    let s = self.stack;
                    match b1 {
                        // flex
                        35 => {
                            if self.sp < 13 {
                                return Err(bad);
                            }
                            self.curve_rel(s[0], s[1], s[2], s[3], s[4], s[5]);
                            self.curve_rel(s[6], s[7], s[8], s[9], s[10], s[11]);
                        }
                        // hflex
                        34 => {
                            if self.sp < 7 {
                                return Err(bad);
                            }
                            self.curve_rel(s[0], 0.0, s[1], s[2], s[3], 0.0);
                            self.curve_rel(s[4], 0.0, s[5], -s[2], s[6], 0.0);
                        }
                        // hflex1
                        36 => {
                            if self.sp < 9 {
                                return Err(bad);
                            }
                            self.curve_rel(s[0], s[1], s[2], s[3], s[4], 0.0);
                            self.curve_rel(s[5], 0.0, s[6], s[7], s[8], -(s[1] + s[3] + s[7]));
                        }
                        // flex1
                        37 => {
                            if self.sp < 11 {
                                return Err(bad);
                            }
                            let dx = s[0] + s[2] + s[4] + s[6] + s[8];
                            let dy = s[1] + s[3] + s[5] + s[7] + s[9];
                            let (dx6, dy6) = if dx.abs() > dy.abs() { (s[10], -dy) } else { (-dx, s[10]) };
                            self.curve_rel(s[0], s[1], s[2], s[3], s[4], s[5]);
                            self.curve_rel(s[6], s[7], s[8], s[9], dx6, dy6);
                        }
                        // Deprecated arithmetic/storage operators and reserved escapes.
                        _ => {}
                    }
                    self.sp = 0;
                }
                // Reserved operators clear the stack.
                _ => self.sp = 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    #[derive(Default)]
    struct Rec(Vec<(char, f32, f32)>);

    impl OutlineSink for Rec {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(('M', x, y));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(('L', x, y));
        }
        fn quad_to(&mut self, _: f32, _: f32, x: f32, y: f32) {
            self.0.push(('Q', x, y));
        }
        fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, x: f32, y: f32) {
            self.0.push(('C', x, y));
        }
        fn close(&mut self) {
            self.0.push(('Z', 0.0, 0.0));
        }
    }

    fn num(v: i32) -> Vec<u8> {
        match v {
            -107..=107 => vec![(v + 139) as u8],
            108..=1131 => {
                let w = v - 108;
                vec![(w / 256 + 247) as u8, (w % 256) as u8]
            }
            -1131..=-108 => {
                let w = -v - 108;
                vec![(w / 256 + 251) as u8, (w % 256) as u8]
            }
            _ => vec![28, (v >> 8) as u8, v as u8],
        }
    }

    fn run(cs: &[u8], gsubrs: &Index<'_>, pv: &Private<'_>) -> (Result<(), FontError>, Rec) {
        let mut rec = Rec::default();
        let mut ip = Interp {
            stack: [0.0; MAX_STACK],
            sp: 0,
            x: 0.0,
            y: 0.0,
            nhints: 0,
            seen_width: false,
            open: false,
            sink: &mut rec,
        };
        let r = ip.run(cs, gsubrs, bias(gsubrs.len()), pv);
        if ip.open {
            ip.sink.close();
        }
        (r, rec)
    }

    fn build_index(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(objects.len() as u16).to_be_bytes());
        out.push(4);
        let mut off = 1u32;
        out.extend_from_slice(&off.to_be_bytes());
        for o in objects {
            off += o.len() as u32;
            out.extend_from_slice(&off.to_be_bytes());
        }
        for o in objects {
            out.extend_from_slice(o);
        }
        out
    }

    #[test]
    fn width_hints_and_paths() {
        let empty = Index::default();
        let pv = Private::default();
        // width=50, hstem x2 (4 args), vstemhm with 2 args, hintmask (3 stems -> 1 byte), rmoveto,
        // rlineto, hvcurveto with trailing extra arg, endchar.
        let mut cs = Vec::new();
        for v in [50, 10, 20, 30, 40] {
            cs.extend(num(v));
        }
        cs.push(1);
        for v in [5, 15] {
            cs.extend(num(v));
        }
        cs.push(23);
        cs.push(19);
        cs.push(0xFF); // mask byte (must be skipped, not read as a number)
        for v in [100, 200] {
            cs.extend(num(v));
        }
        cs.push(21);
        for v in [10, 0] {
            cs.extend(num(v));
        }
        cs.push(5);
        for v in [10, 20, 30, 40, 7] {
            cs.extend(num(v));
        }
        cs.push(31);
        cs.push(14);
        let (r, rec) = run(&cs, &empty, &pv);
        assert!(r.is_ok());
        assert_eq!(rec.0[0], ('M', 100.0, 200.0));
        assert_eq!(rec.0[1], ('L', 110.0, 200.0));
        // hvcurveto: (dx1=10, 0), (dx2=20, dy2=30), (dx3=extra 7, dy3=40)
        assert_eq!(rec.0[2], ('C', 110.0 + 10.0 + 20.0 + 7.0, 200.0 + 30.0 + 40.0));
        assert_eq!(rec.0.last().unwrap().0, 'Z');
    }

    #[test]
    fn subroutines_flex_and_limits() {
        // Global subr 0 (biased index -107): draws a line and returns.
        let mut sub = Vec::new();
        sub.extend(num(5));
        sub.extend(num(6));
        sub.push(5);
        sub.push(11);
        let gidx = build_index(&[sub]);
        let (gsubrs, _) = Index::parse(&gidx, 0).unwrap();
        let pv = Private::default();
        let mut cs = Vec::new();
        cs.extend(num(0));
        cs.extend(num(0));
        cs.push(21);
        cs.extend(num(-107));
        cs.push(29);
        // hflex1: dx1 dy1 dx2 dy2 dx3 dx4 dx5 dy5 dx6 -> ends at the starting y.
        for v in [10, 5, 10, 5, 10, 10, 10, -3, 10] {
            cs.extend(num(v));
        }
        cs.extend([12, 36]);
        cs.push(14);
        let (r, rec) = run(&cs, &gsubrs, &pv);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(rec.0[1], ('L', 5.0, 6.0));
        let end = rec.0[3];
        assert_eq!(end.0, 'C');
        assert!((end.2 - 6.0).abs() < 1e-6, "hflex1 must return to the start y: {end:?}");

        // A subroutine that calls itself must hit the depth limit, not overflow.
        let mut rec_sub = Vec::new();
        rec_sub.extend(num(-107));
        rec_sub.push(29);
        let gidx = build_index(&[rec_sub]);
        let (gsubrs, _) = Index::parse(&gidx, 0).unwrap();
        let mut cs = num(-107);
        cs.push(29);
        let (r, _) = run(&cs, &gsubrs, &pv);
        assert_eq!(r, Err(FontError::LimitExceeded));
        // Stack overflow and truncated operands are errors.
        let cs: Vec<u8> = (0..60).flat_map(|_| num(1)).collect();
        assert_eq!(run(&cs, &gsubrs, &pv).0, Err(FontError::MalformedGlyph));
        assert_eq!(run(&[28, 1], &gsubrs, &pv).0, Err(FontError::MalformedGlyph));
    }

    #[test]
    fn fd_select_formats() {
        let f0 = [0u8, 0, 1, 1, 2];
        let s = FdSelect::Format0(&f0);
        assert_eq!((s.fd(0), s.fd(2), s.fd(3), s.fd(4)), (Some(0), Some(1), Some(2), None));
        // Format 3: [0, 10) -> 0, [10, 50) -> 3, [50, 60) -> 1, sentinel 60.
        let f3 = [3u8, 0, 3, 0, 0, 0, 0, 10, 3, 0, 50, 1, 0, 60];
        let s = FdSelect::Format3(&f3, 3);
        for (g, fd) in
            [(0, Some(0)), (9, Some(0)), (10, Some(3)), (49, Some(3)), (50, Some(1)), (59, Some(1)), (60, None)]
        {
            assert_eq!(s.fd(g), fd, "glyph {g}");
        }
        assert_eq!(FdSelect::Format3(&f3[..9], 3).fd(55), None);
    }

    /// A DICT integer operand with a fixed 5-byte encoding.
    fn int5(v: usize) -> Vec<u8> {
        let v = v as u32;
        vec![29, (v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8]
    }

    /// Builds a CID-keyed CFF with two font dicts whose local subroutine 0 draws different lines.
    fn cid_cff() -> Vec<u8> {
        let charstrings = {
            let mut g = num(0);
            g.extend(num(0));
            g.push(21);
            g.extend(num(-107));
            g.extend([10, 14]);
            build_index(&[vec![14], g.clone(), g])
        };
        let fd_select = vec![3u8, 0, 2, 0, 0, 0, 0, 2, 1, 0, 3];
        let subrs = |dx: i32, dy: i32| {
            let mut s = num(dx);
            s.extend(num(dy));
            s.extend([5, 11]);
            build_index(&[s])
        };
        let private = || {
            let mut p = int5(6);
            p.push(19);
            p
        };
        // Fixed-size Top DICT: ROS, CharStrings, FDArray, FDSelect.
        let top = |cs: usize, fa: usize, fs: usize| {
            let mut t = num(391);
            t.extend(num(392));
            t.extend(num(0));
            t.extend([12, 30]);
            t.extend(int5(cs));
            t.push(17);
            t.extend(int5(fa));
            t.extend([12, 36]);
            t.extend(int5(fs));
            t.extend([12, 37]);
            t
        };
        let header = [1u8, 0, 4, 4];
        let names = build_index(&[b"T".to_vec()]);
        let top_len = build_index(&[top(0, 0, 0)]).len();
        let cs_off = header.len() + names.len() + top_len + 2 + 2;
        let fs_off = cs_off + charstrings.len();
        let fa_off = fs_off + fd_select.len();
        let p_len = private().len();
        let s0 = subrs(10, 0);
        let fd_dict = |size: usize, off: usize| {
            let mut d = int5(size);
            d.extend(int5(off));
            d.push(18);
            d
        };
        let fa_len = build_index(&[fd_dict(0, 0), fd_dict(0, 0)]).len();
        let p0 = fa_off + fa_len;
        let p1 = p0 + p_len + s0.len();
        let fd_array = build_index(&[fd_dict(p_len, p0), fd_dict(p_len, p1)]);
        let mut out = header.to_vec();
        out.extend(names);
        out.extend(build_index(&[top(cs_off, fa_off, fs_off)]));
        out.extend([0, 0, 0, 0]); // empty String and Global Subr INDEXes
        assert_eq!(out.len(), cs_off);
        out.extend(charstrings);
        out.extend(fd_select);
        out.extend(fd_array);
        assert_eq!(out.len(), p0);
        out.extend(private());
        out.extend(s0);
        assert_eq!(out.len(), p1);
        out.extend(private());
        out.extend(subrs(0, 20));
        out
    }

    #[test]
    fn cid_keyed_font_uses_per_fd_subroutines() {
        let data = cid_cff();
        let cff = Cff::parse(&data).unwrap();
        assert_eq!(cff.num_glyphs(), 3);
        let mut rec = Rec::default();
        cff.outline(1, &mut rec).unwrap();
        assert_eq!(rec.0, [('M', 0.0, 0.0), ('L', 10.0, 0.0), ('Z', 0.0, 0.0)]);
        let mut rec = Rec::default();
        cff.outline(2, &mut rec).unwrap();
        assert_eq!(rec.0, [('M', 0.0, 0.0), ('L', 0.0, 20.0), ('Z', 0.0, 0.0)]);
        let mut rec = Rec::default();
        cff.outline(0, &mut rec).unwrap();
        assert!(rec.0.is_empty());
        assert_eq!(cff.outline(3, &mut rec), Err(FontError::InvalidGlyph));
        // Truncations of the table never panic.
        for n in 0..data.len() {
            if let Ok(c) = Cff::parse(&data[..n]) {
                for g in 0..3 {
                    let _ = c.outline(g, &mut Rec::default());
                }
            }
        }
    }

    #[test]
    fn dict_and_reals() {
        // 2.25 encoded as 30 [2 . 2 5 f] = 0x2a 0x25 0xff; -1.5E-3 = 30 [e 1 . 5 c 3 f]
        let d = [30, 0x2A, 0x25, 0xFF, 30, 0xE1, 0xA5, 0xC3, 0xFF, 139, 12, 7];
        let mut got = Vec::new();
        parse_dict(&d, |op, args| got.push((op, args.to_vec()))).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 0x0C07);
        assert!((got[0].1[0] - 2.25).abs() < 1e-12);
        assert!((got[0].1[1] + 0.0015).abs() < 1e-12);
        assert_eq!(got[0].1[2], 0.0);
        assert!(parse_dict(&[255], |_, _| {}).is_none());
    }
}
