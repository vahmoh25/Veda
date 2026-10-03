//! Marker parsing, Huffman decoding of baseline and progressive scans, and image assembly.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::color;
use super::dct::{idct, idct_dc};
use super::huffman::{self, DecodeTable, FAST_BITS};
use super::{ColorModel, JpegInfo, ZIGZAG};
use crate::DecodeOptions;
use crate::error::ImageError;
use crate::image::{Image, Orientation};
use crate::util::{be16, try_vec};

/// Zero bytes the bit reader may synthesize past the end of a scan's data before the rest of the
/// scan is considered missing (a valid scan needs at most a few).
const MAX_PADDING: usize = 1024;

/// Returns `true` if any byte of `v` is `0xFF`.
#[inline(always)]
fn has_ff(v: u64) -> bool {
    let x = !v;
    (x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080) != 0
}

/// MSB-first reader for entropy-coded data. Removes byte stuffing (`FF 00`), stops at markers
/// and then supplies zero bits.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte to read. When a marker is reached it stays on the marker's first `0xFF`.
    pub pos: usize,
    /// Buffered bits, left-aligned (the next bit is bit 63).
    bits: u64,
    /// Number of valid bits in `bits`.
    count: u32,
    /// A marker or the end of the data was reached.
    marker_hit: bool,
    /// Zero bytes supplied since then.
    padded: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8], pos: usize) -> Self {
        BitReader { data, pos, bits: 0, count: 0, marker_hit: false, padded: 0 }
    }

    /// Ensures at least `n` (<= 57) bits are buffered.
    #[inline(always)]
    pub(crate) fn ensure(&mut self, n: u32) {
        if self.count < n {
            self.refill();
        }
    }

    fn refill(&mut self) {
        if !self.marker_hit
            && let Some(chunk) = self.data.get(self.pos..self.pos + 8)
        {
            let v = u64::from_be_bytes(chunk.try_into().unwrap_or([0xFF; 8]));
            if !has_ff(v) {
                let k = (64 - self.count) / 8;
                let shift = 64 - 8 * k;
                self.bits |= (v >> shift) << (shift - self.count);
                self.pos += k as usize;
                self.count += 8 * k;
                return;
            }
        }
        while self.count <= 56 {
            let b = self.next_byte();
            self.bits |= (b as u64) << (56 - self.count);
            self.count += 8;
        }
    }

    fn next_byte(&mut self) -> u8 {
        if self.marker_hit {
            self.padded += 1;
            return 0;
        }
        let Some(&b) = self.data.get(self.pos) else {
            self.marker_hit = true;
            self.padded += 1;
            return 0;
        };
        if b != 0xFF {
            self.pos += 1;
            return b;
        }
        let mut p = self.pos + 1;
        while self.data.get(p) == Some(&0xFF) {
            p += 1;
        }
        if self.data.get(p) == Some(&0) {
            self.pos = p + 1;
            0xFF
        } else {
            self.marker_hit = true;
            self.padded += 1;
            0
        }
    }

    /// Returns `true` once the data is exhausted far beyond what the scan could still need.
    pub(crate) fn exhausted(&self) -> bool {
        self.marker_hit && self.padded > MAX_PADDING
    }

    #[inline(always)]
    fn peek(&self, n: u32) -> usize {
        (self.bits >> (64 - n)) as usize
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.bits <<= n;
        self.count = self.count.saturating_sub(n);
    }

    /// Reads `n` (1..=16) bits.
    #[inline(always)]
    pub(crate) fn bits(&mut self, n: u32) -> u32 {
        let v = (self.bits >> (64 - n)) as u32;
        self.consume(n);
        v
    }

    /// Reads one bit.
    #[inline(always)]
    pub(crate) fn bit(&mut self) -> bool {
        self.ensure(1);
        let v = self.bits >> 63 != 0;
        self.consume(1);
        v
    }

    /// Reads `s` (1..=16) magnitude bits and sign-extends them (T.81 `EXTEND`).
    #[inline(always)]
    pub(crate) fn receive_extend(&mut self, s: u32) -> i32 {
        let v = self.bits(s) as i32;
        if v < 1 << (s - 1) { v - (1 << s) + 1 } else { v }
    }

    /// Decodes one Huffman symbol (at least 16 bits must be buffered).
    #[inline(always)]
    pub(crate) fn decode(&mut self, t: &DecodeTable) -> u8 {
        let e = t.fast[self.peek(FAST_BITS)];
        if e != 0 {
            self.consume((e >> 8) as u32);
            return e as u8;
        }
        self.decode_slow(t)
    }

    #[inline(never)]
    fn decode_slow(&mut self, t: &DecodeTable) -> u8 {
        for len in FAST_BITS + 1..=16 {
            let code = self.peek(len) as i32;
            if code <= t.maxcode[len as usize] {
                self.consume(len);
                let idx = code + t.valoffset[len as usize];
                return t.values.get(idx as usize).copied().unwrap_or(0);
            }
        }
        // Not a valid code: corrupt data. Returning 0 ends the block (EOB / zero difference).
        0
    }

    /// Handles a restart marker: discards buffered bits and skips past the next `RSTn` marker.
    pub(crate) fn restart(&mut self) {
        self.bits = 0;
        self.count = 0;
        self.padded = 0;
        let d = self.data;
        let mut p = self.pos;
        loop {
            match d.get(p) {
                None => {
                    self.pos = p;
                    self.marker_hit = true;
                    return;
                }
                Some(&0xFF) => {
                    let mut q = p + 1;
                    while d.get(q) == Some(&0xFF) {
                        q += 1;
                    }
                    match d.get(q) {
                        Some(&0) => p = q + 1,
                        Some(&m) if (0xD0..=0xD7).contains(&m) => {
                            self.pos = q + 1;
                            self.marker_hit = false;
                            return;
                        }
                        _ => {
                            // Some other marker: the rest of the scan is missing.
                            self.pos = p;
                            self.marker_hit = true;
                            return;
                        }
                    }
                }
                Some(_) => p += 1,
            }
        }
    }
}

/// One image component (Y, Cb, Cr, ...).
struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// Blocks per row and column, padded to whole MCUs.
    bw: usize,
    bh: usize,
    /// Component size in samples.
    cw: usize,
    ch: usize,
    /// Decoded samples with a pitch of `bw * 8`.
    plane: Vec<u8>,
    /// Quantized coefficients of all blocks (progressive images only).
    coefs: Vec<i16>,
    /// DC predictor.
    pred: i32,
    /// Quantization table (zigzag order), latched at the component's first scan.
    qt: Option<[u16; 64]>,
}

struct Frame {
    width: u32,
    height: u32,
    progressive: bool,
    comps: Vec<Component>,
    hmax: usize,
    vmax: usize,
    mcus_x: usize,
    mcus_y: usize,
    allocated: bool,
}

/// Parameters of one scan.
struct Scan {
    n: usize,
    comp: [usize; 4],
    dc: [usize; 4],
    ac: [usize; 4],
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
}

/// The decoder state while walking the marker stream.
pub(crate) struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    opts: DecodeOptions,
    qt: [Option<[u16; 64]>; 4],
    dc: [Option<Box<DecodeTable>>; 4],
    ac: [Option<Box<DecodeTable>>; 4],
    restart_interval: usize,
    frame: Option<Frame>,
    adobe_transform: Option<u8>,
    jfif: bool,
    orientation: Orientation,
    scans: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(data: &'a [u8], opts: &DecodeOptions) -> Self {
        Decoder {
            data,
            pos: 0,
            opts: *opts,
            qt: [None; 4],
            dc: [None, None, None, None],
            ac: [None, None, None, None],
            restart_interval: 0,
            frame: None,
            adobe_transform: None,
            jfif: false,
            orientation: Orientation::Normal,
            scans: 0,
        }
    }

    /// Header information; available once the frame header has been parsed.
    pub(crate) fn info(&self) -> Result<JpegInfo, ImageError> {
        let f = self.frame.as_ref().ok_or(ImageError::Invalid("JPEG has no frame header"))?;
        Ok(JpegInfo {
            width: f.width,
            height: f.height,
            components: f.comps.len() as u8,
            progressive: f.progressive,
            orientation: self.orientation,
            color_model: self.color_model(f),
        })
    }

    fn color_model(&self, f: &Frame) -> ColorModel {
        match f.comps.len() {
            1 => ColorModel::Gray,
            3 => match self.adobe_transform {
                Some(0) => ColorModel::Rgb,
                Some(_) => ColorModel::YCbCr,
                None => {
                    let ids: Vec<u8> = f.comps.iter().map(|c| c.id).collect();
                    if !self.jfif && ids == b"RGB" { ColorModel::Rgb } else { ColorModel::YCbCr }
                }
            },
            _ => {
                if self.adobe_transform == Some(2) {
                    ColorModel::Ycck
                } else {
                    ColorModel::Cmyk
                }
            }
        }
    }

    /// Reads markers until the first scan (only headers).
    pub(crate) fn read_headers(&mut self) -> Result<JpegInfo, ImageError> {
        let r = self.run(true);
        match (r, self.frame.is_some()) {
            (_, true) => self.info(),
            (Err(e), false) => Err(e),
            (Ok(()), false) => Err(ImageError::Invalid("JPEG has no frame header")),
        }
    }

    /// Decodes the whole image (without applying the EXIF orientation).
    pub(crate) fn decode(&mut self) -> Result<Image, ImageError> {
        self.run(false)?;
        if self.scans == 0 {
            return Err(if self.frame.is_some() {
                ImageError::Truncated
            } else {
                ImageError::Invalid("JPEG has no image")
            });
        }
        let model = self.frame.as_ref().map(|f| self.color_model(f)).unwrap_or(ColorModel::Gray);
        let frame = self.frame.as_mut().ok_or(ImageError::Invalid("JPEG has no frame header"))?;
        if frame.progressive {
            finish_progressive(frame)?;
        }
        render(frame, model)
    }

    pub(crate) fn orientation(&self) -> Orientation {
        self.orientation
    }

    fn run(&mut self, headers_only: bool) -> Result<(), ImageError> {
        if !self.data.starts_with(&[0xFF, 0xD8]) {
            return Err(ImageError::UnknownFormat);
        }
        self.pos = 2;
        loop {
            match self.step(headers_only) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                // Damage after at least one complete scan: keep what was decoded.
                Err(_) if self.scans > 0 => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// Handles one marker; returns `true` when parsing is complete.
    fn step(&mut self, headers_only: bool) -> Result<bool, ImageError> {
        let marker = self.next_marker()?;
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => {}
            0xD9 => return Ok(true),
            0xC0..=0xC2 => {
                let s = self.segment()?;
                self.parse_sof(s, marker == 0xC2)?;
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(ImageError::Unsupported("lossless, hierarchical or arithmetic-coded JPEG"));
            }
            0xC4 => {
                let s = self.segment()?;
                self.parse_dht(s)?;
            }
            0xCC => return Err(ImageError::Unsupported("arithmetic-coded JPEG")),
            0xDB => {
                let s = self.segment()?;
                self.parse_dqt(s)?;
            }
            0xDD => {
                let s = self.segment()?;
                self.restart_interval = be16(s, 0)? as usize;
            }
            0xDA => {
                let s = self.segment()?;
                if headers_only {
                    return Ok(true);
                }
                let scan = self.parse_sos(s)?;
                self.decode_scan(&scan)?;
                self.scans += 1;
            }
            0xE0 => {
                let s = self.segment()?;
                if s.starts_with(b"JFIF\0") {
                    self.jfif = true;
                }
            }
            0xE1 => {
                let s = self.segment()?;
                if let Some(o) = exif_orientation(s) {
                    self.orientation = o;
                }
            }
            0xEE => {
                let s = self.segment()?;
                if s.len() >= 12 && s.starts_with(b"Adobe") {
                    self.adobe_transform = Some(s[11]);
                }
            }
            _ => {
                self.segment()?;
            }
        }
        Ok(false)
    }

    /// Skips to the next marker and returns its code.
    fn next_marker(&mut self) -> Result<u8, ImageError> {
        loop {
            let rest = self.data.get(self.pos..).unwrap_or(&[]);
            let i = rest.iter().position(|&b| b == 0xFF).ok_or(ImageError::Truncated)?;
            self.pos += i + 1;
            loop {
                let b = *self.data.get(self.pos).ok_or(ImageError::Truncated)?;
                self.pos += 1;
                match b {
                    0xFF => continue,
                    0x00 => break,
                    m => return Ok(m),
                }
            }
        }
    }

    /// Reads a marker segment body (after its length field).
    fn segment(&mut self) -> Result<&'a [u8], ImageError> {
        let len = be16(self.data, self.pos)? as usize;
        if len < 2 {
            return Err(ImageError::Invalid("invalid JPEG segment length"));
        }
        let s = self.data.get(self.pos + 2..self.pos + len).ok_or(ImageError::Truncated)?;
        self.pos += len;
        Ok(s)
    }

    fn parse_sof(&mut self, s: &[u8], progressive: bool) -> Result<(), ImageError> {
        if self.frame.is_some() {
            return Err(ImageError::Invalid("JPEG has more than one frame"));
        }
        if s.len() < 6 {
            return Err(ImageError::Invalid("JPEG frame header is too short"));
        }
        if s[0] != 8 {
            return Err(ImageError::Unsupported("JPEG sample precision other than 8 bits"));
        }
        let height = be16(s, 1)? as u32;
        let width = be16(s, 3)? as u32;
        let n = s[5] as usize;
        if height == 0 {
            return Err(ImageError::Unsupported("JPEG height defined by a DNL marker"));
        }
        if !matches!(n, 1 | 3 | 4) {
            return Err(ImageError::Unsupported("JPEG component count other than 1, 3 or 4"));
        }
        if s.len() < 6 + 3 * n {
            return Err(ImageError::Invalid("JPEG frame header is too short"));
        }
        self.opts.check_dimensions(width, height)?;
        let mut comps = Vec::with_capacity(n);
        for i in 0..n {
            let (id, hv, tq) = (s[6 + 3 * i], s[7 + 3 * i], s[8 + 3 * i] as usize);
            let (h, v) = ((hv >> 4) as usize, (hv & 15) as usize);
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) || tq > 3 {
                return Err(ImageError::Invalid("invalid JPEG component parameters"));
            }
            comps.push(Component {
                id,
                h,
                v,
                tq,
                bw: 0,
                bh: 0,
                cw: 0,
                ch: 0,
                plane: Vec::new(),
                coefs: Vec::new(),
                pred: 0,
                qt: None,
            });
        }
        let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
        let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
        if comps.iter().any(|c| hmax % c.h != 0 || vmax % c.v != 0) {
            return Err(ImageError::Unsupported("non-integral JPEG sampling factors"));
        }
        let (w, h) = (width as usize, height as usize);
        let mcus_x = w.div_ceil(8 * hmax);
        let mcus_y = h.div_ceil(8 * vmax);
        for c in &mut comps {
            c.bw = mcus_x * c.h;
            c.bh = mcus_y * c.v;
            c.cw = (w * c.h).div_ceil(hmax);
            c.ch = (h * c.v).div_ceil(vmax);
        }
        self.frame = Some(Frame { width, height, progressive, comps, hmax, vmax, mcus_x, mcus_y, allocated: false });
        Ok(())
    }

    fn parse_dht(&mut self, mut s: &[u8]) -> Result<(), ImageError> {
        while !s.is_empty() {
            if s.len() < 17 {
                return Err(ImageError::Invalid("JPEG Huffman table segment is too short"));
            }
            let (class, slot) = (s[0] >> 4, (s[0] & 15) as usize);
            if class > 1 || slot > 3 {
                return Err(ImageError::Invalid("invalid JPEG Huffman table class or index"));
            }
            let counts: [u8; 16] = core::array::from_fn(|i| s[1 + i]);
            let total: usize = counts.iter().map(|&c| c as usize).sum();
            let symbols =
                s.get(17..17 + total).ok_or(ImageError::Invalid("JPEG Huffman table segment is too short"))?;
            let mut table = Box::new(DecodeTable::new(&counts, symbols)?);
            if class == 1 {
                table.build_fast_ac();
                self.ac[slot] = Some(table);
            } else {
                self.dc[slot] = Some(table);
            }
            s = &s[17 + total..];
        }
        Ok(())
    }

    fn parse_dqt(&mut self, mut s: &[u8]) -> Result<(), ImageError> {
        while !s.is_empty() {
            let (precision, slot) = (s[0] >> 4, (s[0] & 15) as usize);
            if precision > 1 || slot > 3 {
                return Err(ImageError::Invalid("invalid JPEG quantization table"));
            }
            let n = if precision == 0 { 64 } else { 128 };
            let body = s.get(1..1 + n).ok_or(ImageError::Invalid("JPEG quantization table segment is too short"))?;
            let table: [u16; 64] = core::array::from_fn(|k| {
                if precision == 0 { body[k] as u16 } else { u16::from_be_bytes([body[2 * k], body[2 * k + 1]]) }
            });
            self.qt[slot] = Some(table);
            s = &s[1 + n..];
        }
        Ok(())
    }

    fn parse_sos(&mut self, s: &[u8]) -> Result<Scan, ImageError> {
        let frame = self.frame.as_ref().ok_or(ImageError::Invalid("JPEG scan before the frame header"))?;
        let n = *s.first().ok_or(ImageError::Invalid("empty JPEG scan header"))? as usize;
        if n == 0 || n > 4 || s.len() < 1 + 2 * n + 3 {
            return Err(ImageError::Invalid("invalid JPEG scan header"));
        }
        let mut scan = Scan { n, comp: [0; 4], dc: [0; 4], ac: [0; 4], ss: 0, se: 63, ah: 0, al: 0 };
        for i in 0..n {
            let (id, tables) = (s[1 + 2 * i], s[2 + 2 * i]);
            let ci = frame
                .comps
                .iter()
                .position(|c| c.id == id)
                .ok_or(ImageError::Invalid("JPEG scan references an unknown component"))?;
            if scan.comp[..i].contains(&ci) {
                return Err(ImageError::Invalid("JPEG scan lists a component twice"));
            }
            let (dc, ac) = ((tables >> 4) as usize, (tables & 15) as usize);
            if dc > 3 || ac > 3 {
                return Err(ImageError::Invalid("invalid JPEG Huffman table selector"));
            }
            scan.comp[i] = ci;
            scan.dc[i] = dc;
            scan.ac[i] = ac;
        }
        let p = 1 + 2 * n;
        if frame.progressive {
            scan.ss = s[p] as usize;
            scan.se = s[p + 1] as usize;
            scan.ah = (s[p + 2] >> 4) as u32;
            scan.al = (s[p + 2] & 15) as u32;
            let bad = scan.ss > scan.se
                || scan.se > 63
                || (scan.ss == 0 && scan.se != 0)
                || (scan.ss > 0 && n != 1)
                || scan.ah > 13
                || scan.al > 13;
            if bad {
                return Err(ImageError::Invalid("invalid progressive JPEG scan parameters"));
            }
        }
        Ok(scan)
    }

    fn decode_scan(&mut self, scan: &Scan) -> Result<(), ImageError> {
        let frame = self.frame.as_mut().ok_or(ImageError::Invalid("JPEG scan before the frame header"))?;
        let progressive = frame.progressive;
        // Tables: fall back to the standard ones when a stream omits them (motion JPEG).
        for i in 0..scan.n {
            let need_dc = !progressive || (scan.ss == 0 && scan.ah == 0);
            let need_ac = !progressive || scan.ss > 0;
            if need_dc && self.dc[scan.dc[i]].is_none() {
                let spec = huffman::standard(false, scan.dc[i]);
                self.dc[scan.dc[i]] = Some(Box::new(DecodeTable::new(&spec.counts, &spec.symbols)?));
            }
            if need_ac && self.ac[scan.ac[i]].is_none() {
                let spec = huffman::standard(true, scan.ac[i]);
                let mut t = Box::new(DecodeTable::new(&spec.counts, &spec.symbols)?);
                t.build_fast_ac();
                self.ac[scan.ac[i]] = Some(t);
            }
            let c = &mut frame.comps[scan.comp[i]];
            if c.qt.is_none() {
                c.qt = Some(self.qt[c.tq].ok_or(ImageError::Invalid("JPEG quantization table is missing"))?);
            }
        }
        if !frame.allocated {
            // Refuse to allocate for an image the remaining data cannot plausibly describe.
            let blocks: usize = frame.comps.iter().map(|c| c.bw * c.bh).sum();
            if (self.data.len() - self.pos).saturating_mul(32) < blocks {
                return Err(ImageError::Truncated);
            }
            for c in &mut frame.comps {
                if progressive {
                    c.coefs = try_vec(c.bw * c.bh * 64, 0i16)?;
                } else {
                    c.plane = try_vec(c.bw * 8 * c.bh * 8, 128u8)?;
                }
            }
            frame.allocated = true;
        }
        let mut br = BitReader::new(self.data, self.pos);
        let tables = Tables { dc: &self.dc, ac: &self.ac };
        let result = if progressive {
            progressive_scan(frame, scan, &tables, self.restart_interval, &mut br)
        } else {
            sequential_scan(frame, scan, &tables, self.restart_interval, &mut br)
        };
        self.pos = br.pos;
        result
    }
}

/// The Huffman tables in effect for a scan.
struct Tables<'t> {
    dc: &'t [Option<Box<DecodeTable>>; 4],
    ac: &'t [Option<Box<DecodeTable>>; 4],
}

impl Tables<'_> {
    fn dc(&self, slot: usize) -> Result<&DecodeTable, ImageError> {
        self.dc[slot].as_deref().ok_or(ImageError::Invalid("JPEG Huffman table is missing"))
    }

    fn ac(&self, slot: usize) -> Result<&DecodeTable, ImageError> {
        self.ac[slot].as_deref().ok_or(ImageError::Invalid("JPEG Huffman table is missing"))
    }
}

/// Decodes the coefficients of one baseline block, dequantizing them into natural order.
/// Returns the zigzag index of the last coefficient written (0 = DC only).
#[inline(always)]
fn decode_block(
    br: &mut BitReader,
    dc: &DecodeTable,
    ac: &DecodeTable,
    q: &[i32; 64],
    pred: &mut i32,
    block: &mut [i32; 64],
) -> usize {
    *block = [0; 64];
    br.ensure(32);
    let t = br.decode(dc) as u32;
    let diff = if t == 0 || t > 16 { 0 } else { br.receive_extend(t) };
    *pred = pred.wrapping_add(diff);
    block[0] = (*pred as i64 * q[0] as i64).clamp(-(1 << 24), 1 << 24) as i32;
    let mut last = 0;
    let mut k = 1;
    while k < 64 {
        br.ensure(32);
        let fast = ac.fast_ac[br.peek(FAST_BITS)];
        if fast != 0 {
            k += ((fast >> 4) & 15) as usize;
            br.consume((fast & 15) as u32);
            if k > 63 {
                break;
            }
            block[ZIGZAG[k]] = (fast >> 8) as i32 * q[k];
            last = k;
            k += 1;
            continue;
        }
        let rs = br.decode(ac);
        let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
        if s == 0 {
            if r != 15 {
                break;
            }
            k += 16;
            continue;
        }
        k += r;
        let v = br.receive_extend(s);
        if k > 63 {
            break;
        }
        block[ZIGZAG[k]] = v * q[k];
        last = k;
        k += 1;
    }
    last
}

fn sequential_scan(
    frame: &mut Frame,
    scan: &Scan,
    tables: &Tables,
    restart_interval: usize,
    br: &mut BitReader,
) -> Result<(), ImageError> {
    let single = scan.n == 1;
    let (mcus_x, mcus_y) = if single {
        let c = &frame.comps[scan.comp[0]];
        (c.cw.div_ceil(8), c.ch.div_ceil(8))
    } else {
        (frame.mcus_x, frame.mcus_y)
    };
    let mut q = [[0i32; 64]; 4];
    let mut tabs: Vec<(&DecodeTable, &DecodeTable)> = Vec::with_capacity(scan.n);
    for (i, qi) in q.iter_mut().enumerate().take(scan.n) {
        let c = &mut frame.comps[scan.comp[i]];
        c.pred = 0;
        *qi = c.qt.unwrap_or([1; 64]).map(|v| v as i32);
        tabs.push((tables.dc(scan.dc[i])?, tables.ac(scan.ac[i])?));
    }
    let mut block = [0i32; 64];
    let mut todo = restart_interval;
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval != 0 {
                if todo == 0 {
                    br.restart();
                    for i in 0..scan.n {
                        frame.comps[scan.comp[i]].pred = 0;
                    }
                    todo = restart_interval;
                }
                todo -= 1;
            }
            for (i, &(dc, ac)) in tabs.iter().enumerate() {
                let c = &mut frame.comps[scan.comp[i]];
                let (nh, nv) = if single { (1, 1) } else { (c.h, c.v) };
                for v in 0..nv {
                    for h in 0..nh {
                        let (bx, by) = if single { (mx, my) } else { (mx * c.h + h, my * c.v + v) };
                        let last = decode_block(br, dc, ac, &q[i], &mut c.pred, &mut block);
                        if bx * 8 < c.cw && by * 8 < c.ch {
                            let stride = c.bw * 8;
                            let out = &mut c.plane[by * 8 * stride + bx * 8..];
                            if last == 0 {
                                idct_dc(block[0], out, stride);
                            } else {
                                idct(&block, out, stride);
                            }
                        }
                    }
                }
            }
        }
        if br.exhausted() {
            break;
        }
    }
    Ok(())
}

fn progressive_scan(
    frame: &mut Frame,
    scan: &Scan,
    tables: &Tables,
    restart_interval: usize,
    br: &mut BitReader,
) -> Result<(), ImageError> {
    let dc_first = scan.ss == 0 && scan.ah == 0;
    let mut dcs: [Option<&DecodeTable>; 4] = [None; 4];
    for (i, dc) in dcs.iter_mut().enumerate().take(scan.n) {
        frame.comps[scan.comp[i]].pred = 0;
        if dc_first {
            *dc = Some(tables.dc(scan.dc[i])?);
        }
    }
    let ac = if scan.ss > 0 { Some(tables.ac(scan.ac[0])?) } else { None };
    let mut eobrun = 0u32;
    let mut todo = restart_interval;
    let mut restart = |br: &mut BitReader, comps: &mut [Component], eobrun: &mut u32| {
        if restart_interval != 0 {
            if todo == 0 {
                br.restart();
                for i in 0..scan.n {
                    comps[scan.comp[i]].pred = 0;
                }
                *eobrun = 0;
                todo = restart_interval;
            }
            todo -= 1;
        }
    };

    if scan.n == 1 {
        let ci = scan.comp[0];
        let (nbx, nby) = (frame.comps[ci].cw.div_ceil(8), frame.comps[ci].ch.div_ceil(8));
        for by in 0..nby {
            for bx in 0..nbx {
                restart(br, &mut frame.comps, &mut eobrun);
                let c = &mut frame.comps[ci];
                let blk = &mut c.coefs[(by * c.bw + bx) * 64..][..64];
                match (scan.ss == 0, scan.ah == 0, dcs[0], ac) {
                    (true, true, Some(dc), _) => decode_dc_first(br, dc, &mut c.pred, scan.al, blk),
                    (true, false, _, _) => decode_dc_refine(br, scan.al, blk),
                    (false, true, _, Some(ac)) => decode_ac_first(br, ac, scan, &mut eobrun, blk),
                    (false, false, _, Some(ac)) => decode_ac_refine(br, ac, scan, &mut eobrun, blk),
                    _ => {}
                }
            }
            if br.exhausted() {
                break;
            }
        }
    } else {
        // Interleaved scans are always DC scans.
        for my in 0..frame.mcus_y {
            for mx in 0..frame.mcus_x {
                restart(br, &mut frame.comps, &mut eobrun);
                for (i, dc) in dcs.iter().enumerate().take(scan.n) {
                    let c = &mut frame.comps[scan.comp[i]];
                    for v in 0..c.v {
                        for h in 0..c.h {
                            let (bx, by) = (mx * c.h + h, my * c.v + v);
                            let blk = &mut c.coefs[(by * c.bw + bx) * 64..][..64];
                            match dc {
                                Some(dc) if scan.ah == 0 => decode_dc_first(br, dc, &mut c.pred, scan.al, blk),
                                _ => decode_dc_refine(br, scan.al, blk),
                            }
                        }
                    }
                }
            }
            if br.exhausted() {
                break;
            }
        }
    }
    Ok(())
}

fn decode_dc_first(br: &mut BitReader, dc: &DecodeTable, pred: &mut i32, al: u32, blk: &mut [i16]) {
    br.ensure(32);
    let t = br.decode(dc) as u32;
    let diff = if t == 0 || t > 16 { 0 } else { br.receive_extend(t) };
    *pred = pred.wrapping_add(diff);
    blk[0] = pred.wrapping_shl(al) as i16;
}

fn decode_dc_refine(br: &mut BitReader, al: u32, blk: &mut [i16]) {
    if br.bit() {
        blk[0] |= (1u16 << al) as i16;
    }
}

fn decode_ac_first(br: &mut BitReader, ac: &DecodeTable, scan: &Scan, eobrun: &mut u32, blk: &mut [i16]) {
    if *eobrun > 0 {
        *eobrun -= 1;
        return;
    }
    let mut k = scan.ss;
    while k <= scan.se {
        br.ensure(32);
        let rs = br.decode(ac);
        let (r, s) = ((rs >> 4) as u32, (rs & 15) as u32);
        if s != 0 {
            k += r as usize;
            let v = br.receive_extend(s);
            if k > 63 {
                break;
            }
            blk[ZIGZAG[k]] = v.wrapping_shl(scan.al) as i16;
            k += 1;
        } else if r < 15 {
            *eobrun = (1 << r) - 1;
            if r > 0 {
                *eobrun += br.bits(r);
            }
            break;
        } else {
            k += 16;
        }
    }
}

/// Refines one coefficient that is already nonzero (adds the next lower bit of its magnitude).
#[inline(always)]
fn refine(br: &mut BitReader, coef: &mut i16, p1: i16) {
    if br.bit() && (*coef & p1) == 0 {
        *coef = if *coef >= 0 { coef.wrapping_add(p1) } else { coef.wrapping_sub(p1) };
    }
}

fn decode_ac_refine(br: &mut BitReader, ac: &DecodeTable, scan: &Scan, eobrun: &mut u32, blk: &mut [i16]) {
    let p1 = (1u16 << scan.al) as i16;
    let mut k = scan.ss;
    if *eobrun == 0 {
        while k <= scan.se {
            br.ensure(32);
            let rs = br.decode(ac);
            let (mut r, s) = ((rs >> 4) as u32, rs & 15);
            let mut value = 0i16;
            if s != 0 {
                // The magnitude of a newly nonzero coefficient is always 1 (in this bit position).
                value = if br.bit() { p1 } else { p1.wrapping_neg() };
            } else if r != 15 {
                *eobrun = 1 << r;
                if r > 0 {
                    br.ensure(16);
                    *eobrun += br.bits(r);
                }
                break;
            }
            // Skip `r` zero coefficients, refining the nonzero ones passed on the way.
            while k <= scan.se {
                let z = ZIGZAG[k];
                if blk[z] != 0 {
                    refine(br, &mut blk[z], p1);
                } else {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                }
                k += 1;
            }
            if value != 0 && k <= 63 {
                blk[ZIGZAG[k]] = value;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        // Inside an end-of-band run: only refinement bits for nonzero coefficients remain.
        while k <= scan.se {
            let z = ZIGZAG[k];
            if blk[z] != 0 {
                refine(br, &mut blk[z], p1);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
}

/// Dequantizes and inverse-transforms every block of a progressive image.
fn finish_progressive(frame: &mut Frame) -> Result<(), ImageError> {
    for c in &mut frame.comps {
        let mut q = [1i32; 64];
        if let Some(t) = c.qt {
            for k in 0..64 {
                q[ZIGZAG[k]] = t[k] as i32;
            }
        }
        let stride = c.bw * 8;
        c.plane = try_vec(stride * c.bh * 8, 128u8)?;
        let mut block = [0i32; 64];
        for by in 0..c.ch.div_ceil(8) {
            for bx in 0..c.cw.div_ceil(8) {
                let src = &c.coefs[(by * c.bw + bx) * 64..][..64];
                let mut ac = 0;
                for ((b, &s), &qv) in block.iter_mut().zip(src).zip(&q) {
                    *b = s as i32 * qv;
                    ac |= s;
                }
                let out = &mut c.plane[by * 8 * stride + bx * 8..];
                if ac == src[0] && src[1..].iter().all(|&s| s == 0) {
                    idct_dc(block[0], out, stride);
                } else {
                    idct(&block, out, stride);
                }
            }
        }
        c.coefs = Vec::new();
    }
    Ok(())
}

/// Upsamples every component and converts the samples to RGB pixels.
fn render(frame: &Frame, model: ColorModel) -> Result<Image, ImageError> {
    let w = frame.width as usize;
    let mut img = Image::try_new(frame.width, frame.height)?;
    let mut bufs: [Vec<u8>; 4] = Default::default();
    for b in bufs.iter_mut().take(frame.comps.len()) {
        *b = try_vec(w + 16, 0u8)?;
    }
    let mut colsum: Vec<u16> = Vec::new();
    for (y, out) in img.pixels.chunks_exact_mut(w).enumerate() {
        // Upsample the subsampled components of this row into `bufs`.
        for (c, buf) in frame.comps.iter().zip(bufs.iter_mut()) {
            let (fx, fy) = (frame.hmax / c.h, frame.vmax / c.v);
            let stride = c.bw * 8;
            let row = |r: usize| &c.plane[r * stride..r * stride + c.cw];
            if fy == 1 {
                match fx {
                    1 => {}
                    2 => color::h2v1(row(y), buf),
                    _ => color::generic(row(y), row(y), 2, 0, fx, buf),
                }
                continue;
            }
            // The two nearest input rows and the weight of the farther one (in 1/(2 fy)).
            let cy = y / fy;
            let d = 2 * (y % fy) as isize + 1 - fy as isize;
            let far = match d.signum() {
                -1 => cy.saturating_sub(1),
                1 => (cy + 1).min(c.ch - 1),
                _ => cy,
            };
            match (fx, fy) {
                (1, 2) => color::h1v2(row(cy), row(far), d > 0, buf),
                (2, 2) => color::h2v2(row(cy), row(far), buf, &mut colsum),
                _ => {
                    let wf = d.unsigned_abs() as u32;
                    color::generic(row(cy), row(far), 2 * fy as u32 - wf, wf, fx, buf);
                }
            }
        }
        // Full-resolution components are read straight from their planes.
        let rows: [&[u8]; 4] = core::array::from_fn(|i| match frame.comps.get(i) {
            Some(c) if c.h == frame.hmax && c.v == frame.vmax => &c.plane[y * c.bw * 8..][..w],
            _ => &bufs[i][..],
        });
        let [b0, b1, b2, b3] = rows;
        match model {
            ColorModel::Gray => color::gray(b0, out),
            ColorModel::YCbCr => color::ycc_to_rgb(b0, b1, b2, out),
            ColorModel::Rgb => color::rgb(b0, b1, b2, out),
            ColorModel::Cmyk => color::cmyk(b0, b1, b2, b3, out),
            ColorModel::Ycck => color::ycck(b0, b1, b2, b3, out),
        }
    }
    Ok(img)
}

/// Extracts the orientation tag from an `APP1` EXIF segment.
fn exif_orientation(s: &[u8]) -> Option<Orientation> {
    let tiff = s.strip_prefix(b"Exif\0\0")?;
    let le = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let r16 = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let r32 = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) })
    };
    if r16(2)? != 42 {
        return None;
    }
    let ifd = r32(4)? as usize;
    let count = r16(ifd)? as usize;
    for i in 0..count.min(1024) {
        let e = ifd + 2 + i * 12;
        if r16(e)? == 0x0112 {
            let value = match r16(e + 2)? {
                3 => r16(e + 8)?,
                4 => r32(e + 8)? as u16,
                _ => return None,
            };
            return Orientation::from_exif(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_parsing() {
        let mut le = b"Exif\0\0II*\0\x08\0\0\0\x02\0".to_vec();
        le.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0, 0x30, 0, 0, 0]); // Make
        le.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]); // Orientation = 6
        assert_eq!(exif_orientation(&le), Some(Orientation::Rotate90));
        let mut be = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
        be.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 8, 0, 0]);
        assert_eq!(exif_orientation(&be), Some(Orientation::Rotate270));
        assert_eq!(exif_orientation(&be[..20]), None);
        assert_eq!(exif_orientation(b"Exif\0\0XX"), None);
    }

    #[test]
    fn bit_reader_stuffing_and_markers() {
        // 0xFF 0x00 is a stuffed 0xFF; 0xFF 0xD9 is a marker after which zeros are returned.
        let data = [0xAB, 0xFF, 0x00, 0x12, 0xFF, 0xD9];
        let mut br = BitReader::new(&data, 0);
        br.ensure(32);
        assert_eq!(br.bits(8), 0xAB);
        assert_eq!(br.bits(8), 0xFF);
        assert_eq!(br.bits(8), 0x12);
        assert_eq!(br.bits(16), 0);
        assert_eq!(br.pos, 4);
        // Restart skips to after an RST marker.
        let data = [0x12, 0x34, 0xFF, 0xD3, 0x56];
        let mut br = BitReader::new(&data, 0);
        br.ensure(8);
        br.restart();
        br.ensure(8);
        assert_eq!(br.bits(8), 0x56);
    }
}
