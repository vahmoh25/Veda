//! JPEG encoding: baseline (sequential) or progressive, 4:4:4 / 4:2:2 / 4:2:0 or grayscale.
//!
//! The pipeline is libjpeg's: fixed-point RGB to YCbCr, box-filter chroma downsampling, the
//! "islow" forward DCT, quantization with the Annex K tables scaled by quality (libjpeg's
//! `jpeg_quality_scaling`), and Huffman coding with the standard tables or with tables optimized
//! for the image (two passes). Progressive files use libjpeg's default scan script
//! (`jpeg_simple_progression`, including successive approximation) and always use optimized
//! tables, as libjpeg does.

use alloc::vec;
use alloc::vec::Vec;

use super::dct::fdct;
use super::huffman::{self, Spec};
use super::{EncodeOptions, Subsampling, ZIGZAG};
use crate::error::ImageError;
use crate::image::Image;
use crate::util::try_vec;

/// Annex K.1 luminance quantization table (natural order).
const STD_LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51,
    87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
];
/// Annex K.1 chrominance quantization table (natural order).
const STD_CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99,
];

/// Scales a base table for `quality` (1..=100) exactly like libjpeg, limited to baseline values.
pub(crate) fn scaled_table(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = quality.clamp(1, 100) as u32;
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    base.map(|b| ((b as u32 * scale + 50) / 100).clamp(1, 255) as u16)
}

/// One component's quantized coefficients.
struct Comp {
    id: u8,
    /// Sampling factors.
    h: usize,
    v: usize,
    /// Quantization / Huffman table slot (0 = luma, 1 = chroma).
    table: usize,
    /// Blocks per row (padded to whole MCUs).
    bw: usize,
    /// Blocks covering the component's real size (used by non-interleaved scans).
    real_bw: usize,
    real_bh: usize,
    /// Coefficients, 64 per block in natural order.
    coefs: Vec<i16>,
}

impl Comp {
    fn block(&self, bx: usize, by: usize) -> &[i16] {
        &self.coefs[(by * self.bw + bx) * 64..][..64]
    }
}

/// MSB-first bit writer with JPEG byte stuffing.
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    #[inline]
    fn put(&mut self, bits: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (bits as u64 & ((1u64 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            self.n -= 8;
            let b = (self.acc >> self.n) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
        }
    }

    /// Pads the last byte with 1-bits.
    fn flush(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put((1 << pad) - 1, pad);
        }
        self.acc = 0;
    }
}

/// Number of bits needed for the magnitude of `v` (the JPEG "category").
#[inline]
fn category(v: i32) -> u32 {
    32 - v.unsigned_abs().leading_zeros()
}

/// The `n` low bits that encode `v` (one's complement for negative values).
#[inline]
fn magnitude_bits(v: i32, n: u32) -> u32 {
    let v = if v < 0 { v - 1 } else { v };
    (v as u32) & ((1u32 << n) - 1)
}

/// Huffman coder that either counts symbol frequencies or writes codes.
struct Entropy {
    w: BitWriter,
    counting: bool,
    freq: [[u32; 256]; 4],
    codes: [[u16; 256]; 4],
    lens: [[u8; 256]; 4],
    /// Progressive state: pending end-of-band run and buffered correction bits.
    eobrun: u32,
    corrections: Vec<u8>,
}

/// Correction bits buffered before an end-of-band run is forced out (libjpeg's MAX_CORR_BITS).
const MAX_CORR_BITS: usize = 1000;

impl Entropy {
    fn new(out: Vec<u8>) -> Self {
        Entropy {
            w: BitWriter { out, acc: 0, n: 0 },
            counting: false,
            freq: [[0; 256]; 4],
            codes: [[0; 256]; 4],
            lens: [[0; 256]; 4],
            eobrun: 0,
            corrections: Vec::new(),
        }
    }

    #[inline]
    fn symbol(&mut self, slot: usize, s: u8) {
        if self.counting {
            self.freq[slot][s as usize] += 1;
        } else {
            self.w.put(self.codes[slot][s as usize] as u32, self.lens[slot][s as usize] as u32);
        }
    }

    #[inline]
    fn bits(&mut self, v: u32, n: u32) {
        if !self.counting {
            self.w.put(v, n);
        }
    }

    fn set_table(&mut self, slot: usize, spec: &Spec) {
        let (codes, lens) = spec.encode_table();
        self.codes[slot] = codes;
        self.lens[slot] = lens;
    }

    /// Emits a pending end-of-band run (and its buffered correction bits).
    fn emit_eobrun(&mut self, slot: usize) {
        if self.eobrun > 0 {
            let nbits = 31 - self.eobrun.leading_zeros();
            self.symbol(slot, (nbits << 4) as u8);
            if nbits > 0 {
                self.bits(self.eobrun & ((1 << nbits) - 1), nbits);
            }
            self.eobrun = 0;
            let corr = core::mem::take(&mut self.corrections);
            for &b in &corr {
                self.bits(b as u32, 1);
            }
            self.corrections = corr;
            self.corrections.clear();
        }
    }

    /// Baseline: one block (DC difference and run-length coded AC terms).
    fn sequential_block(&mut self, blk: &[i16], last_dc: &mut i32, dc_slot: usize, ac_slot: usize) {
        let dc = blk[0] as i32;
        let diff = dc - *last_dc;
        *last_dc = dc;
        let n = category(diff);
        self.symbol(dc_slot, n as u8);
        self.bits(magnitude_bits(diff, n), n);
        let mut run = 0u32;
        for &z in &ZIGZAG[1..] {
            let v = blk[z] as i32;
            if v == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                self.symbol(ac_slot, 0xF0);
                run -= 16;
            }
            let n = category(v);
            self.symbol(ac_slot, (run << 4 | n) as u8);
            self.bits(magnitude_bits(v, n), n);
            run = 0;
        }
        if run > 0 {
            self.symbol(ac_slot, 0);
        }
    }

    fn dc_first(&mut self, blk: &[i16], last_dc: &mut i32, al: u32, slot: usize) {
        let v = (blk[0] as i32) >> al;
        let diff = v - *last_dc;
        *last_dc = v;
        let n = category(diff);
        self.symbol(slot, n as u8);
        self.bits(magnitude_bits(diff, n), n);
    }

    fn dc_refine(&mut self, blk: &[i16], al: u32) {
        self.bits(((blk[0] as i32 >> al) & 1) as u32, 1);
    }

    fn ac_first(&mut self, blk: &[i16], ss: usize, se: usize, al: u32, slot: usize) {
        let mut run = 0u32;
        for &z in &ZIGZAG[ss..=se] {
            let c = blk[z] as i32;
            let mag = c.unsigned_abs() >> al;
            if mag == 0 {
                run += 1;
                continue;
            }
            self.emit_eobrun(slot);
            while run > 15 {
                self.symbol(slot, 0xF0);
                run -= 16;
            }
            let n = 32 - mag.leading_zeros();
            self.symbol(slot, (run << 4 | n) as u8);
            let bits = if c < 0 { !mag } else { mag };
            self.bits(bits & ((1 << n) - 1), n);
            run = 0;
        }
        if run > 0 {
            self.eobrun += 1;
            if self.eobrun == 0x7FFF {
                self.emit_eobrun(slot);
            }
        }
    }

    fn ac_refine(&mut self, blk: &[i16], ss: usize, se: usize, al: u32, slot: usize) {
        let mut abs = [0u32; 64];
        let mut eob = 0;
        for k in ss..=se {
            abs[k] = (blk[ZIGZAG[k]] as i32).unsigned_abs() >> al;
            if abs[k] == 1 {
                eob = k;
            }
        }
        let mut run = 0u32;
        // Correction bits of this block not yet emitted.
        let mut pending = [0u8; 64];
        let mut npending = 0;
        for k in ss..=se {
            let a = abs[k];
            if a == 0 {
                run += 1;
                continue;
            }
            // Emit ZRLs, unless they can be folded into an end-of-band.
            while run > 15 && k <= eob {
                self.emit_eobrun(slot);
                self.symbol(slot, 0xF0);
                run -= 16;
                for &b in &pending[..npending] {
                    self.bits(b as u32, 1);
                }
                npending = 0;
            }
            if a > 1 {
                // Previously nonzero: only a correction bit.
                pending[npending] = (a & 1) as u8;
                npending += 1;
                continue;
            }
            self.emit_eobrun(slot);
            self.symbol(slot, (run << 4 | 1) as u8);
            self.bits(if blk[ZIGZAG[k]] < 0 { 0 } else { 1 }, 1);
            for &b in &pending[..npending] {
                self.bits(b as u32, 1);
            }
            npending = 0;
            run = 0;
        }
        if run > 0 || npending > 0 {
            self.eobrun += 1;
            self.corrections.extend_from_slice(&pending[..npending]);
            if self.eobrun == 0x7FFF || self.corrections.len() > MAX_CORR_BITS - 64 + 1 {
                self.emit_eobrun(slot);
            }
        }
    }
}

/// A scan of the progressive script: components, spectral band and successive approximation.
struct ScanSpec {
    comps: &'static [usize],
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
}

const fn scan(comps: &'static [usize], ss: usize, se: usize, ah: u32, al: u32) -> ScanSpec {
    ScanSpec { comps, ss, se, ah, al }
}

/// libjpeg's `jpeg_simple_progression` for YCbCr.
const COLOR_SCRIPT: [ScanSpec; 10] = [
    scan(&[0, 1, 2], 0, 0, 0, 1),
    scan(&[0], 1, 5, 0, 2),
    scan(&[2], 1, 63, 0, 1),
    scan(&[1], 1, 63, 0, 1),
    scan(&[0], 6, 63, 0, 2),
    scan(&[0], 1, 63, 2, 1),
    scan(&[0, 1, 2], 0, 0, 1, 0),
    scan(&[2], 1, 63, 1, 0),
    scan(&[1], 1, 63, 1, 0),
    scan(&[0], 1, 63, 1, 0),
];

/// libjpeg's `jpeg_simple_progression` for grayscale.
const GRAY_SCRIPT: [ScanSpec; 6] = [
    scan(&[0], 0, 0, 0, 1),
    scan(&[0], 1, 5, 0, 2),
    scan(&[0], 6, 63, 0, 2),
    scan(&[0], 1, 63, 2, 1),
    scan(&[0], 0, 0, 1, 0),
    scan(&[0], 1, 63, 1, 0),
];

/// A simple successive-approximation script for four-component (CMYK) images.
const FOUR_SCRIPT: [ScanSpec; 10] = [
    scan(&[0, 1, 2, 3], 0, 0, 0, 1),
    scan(&[0], 1, 63, 0, 1),
    scan(&[1], 1, 63, 0, 1),
    scan(&[2], 1, 63, 0, 1),
    scan(&[3], 1, 63, 0, 1),
    scan(&[0, 1, 2, 3], 0, 0, 1, 0),
    scan(&[0], 1, 63, 1, 0),
    scan(&[1], 1, 63, 1, 0),
    scan(&[2], 1, 63, 1, 0),
    scan(&[3], 1, 63, 1, 0),
];

fn marker(out: &mut Vec<u8>, code: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, code]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

fn write_dht(out: &mut Vec<u8>, class: u8, slot: u8, spec: &Spec) {
    let mut body = vec![class << 4 | slot];
    body.extend_from_slice(&spec.counts);
    body.extend_from_slice(&spec.symbols);
    marker(out, 0xC4, &body);
}

/// Encodes `image` as JPEG. Alpha is ignored.
pub(crate) fn encode(image: &Image, opts: &EncodeOptions) -> Result<Vec<u8>, ImageError> {
    let (hs, vs) = match (opts.grayscale, opts.subsampling) {
        (true, _) | (false, Subsampling::Yuv444) => (1, 1),
        (false, Subsampling::Yuv422) => (2, 1),
        (false, Subsampling::Yuv420) => (2, 2),
    };
    encode_sampled(image, opts, hs, vs)
}

/// Encodes with luma sampling factors `hs x vs` (chroma is always 1x1), e.g. `(1, 2)` for 4:4:0
/// or `(4, 1)` for 4:1:1.
pub(crate) fn encode_sampled(image: &Image, opts: &EncodeOptions, hs: usize, vs: usize) -> Result<Vec<u8>, ImageError> {
    encode_impl(image, opts, hs, vs, false)
}

/// Test helper: like [`encode_sampled`], but a baseline file gets one scan per component.
#[cfg(test)]
pub(crate) fn encode_separate_scans(
    image: &Image,
    opts: &EncodeOptions,
    hs: usize,
    vs: usize,
) -> Result<Vec<u8>, ImageError> {
    encode_impl(image, opts, hs, vs, true)
}

fn encode_impl(
    image: &Image,
    opts: &EncodeOptions,
    hs: usize,
    vs: usize,
    separate_scans: bool,
) -> Result<Vec<u8>, ImageError> {
    let (w, h) = (image.width as usize, image.height as usize);
    if image.is_empty() {
        return Err(ImageError::InvalidArgument("cannot encode an empty image"));
    }
    if image.pixels.len() != w * h {
        return Err(ImageError::InvalidArgument("pixel buffer length does not match the dimensions"));
    }
    if w > 65535 || h > 65535 {
        return Err(ImageError::InvalidArgument("image is too large for JPEG"));
    }
    let gray = opts.grayscale;
    let (hs, vs) = if gray { (1, 1) } else { (hs.clamp(1, 4), vs.clamp(1, 4)) };
    let mcus_x = w.div_ceil(8 * hs);
    let mcus_y = h.div_ceil(8 * vs);
    let (pw, ph) = (mcus_x * 8 * hs, mcus_y * 8 * vs);

    // Color conversion into MCU-padded planes (edges replicated).
    let ncomp = if gray { 1 } else { 3 };
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(ncomp);
    for _ in 0..ncomp {
        planes.push(try_vec(pw * ph, 0u8)?);
    }
    for y in 0..ph {
        let src = &image.pixels[y.min(h - 1) * w..][..w];
        for x in 0..pw {
            let p = src[x.min(w - 1)];
            let (r, g, b) = (((p >> 16) & 0xFF) as i32, ((p >> 8) & 0xFF) as i32, (p & 0xFF) as i32);
            let i = y * pw + x;
            planes[0][i] = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8;
            if !gray {
                planes[1][i] = ((-11059 * r - 21709 * g + 32768 * b + (128 << 16) + 32767) >> 16) as u8;
                planes[2][i] = ((32768 * r - 27439 * g - 5329 * b + (128 << 16) + 32767) >> 16) as u8;
            }
        }
    }
    // Chroma downsampling: box filter; the 2x cases use libjpeg's alternating rounding bias.
    if !gray && (hs, vs) != (1, 1) {
        let n = (hs * vs) as u32;
        for plane in planes.iter_mut().skip(1) {
            let (cw, chh) = (pw / hs, ph / vs);
            let mut small = try_vec(cw * chh, 0u8)?;
            for y in 0..chh {
                let (mut bias, toggle) = match (hs, vs) {
                    (2, 2) => (1, 3),
                    (2, 1) => (0, 1),
                    _ => (n / 2, 0),
                };
                for x in 0..cw {
                    let sum: u32 = (0..vs)
                        .flat_map(|dy| (0..hs).map(move |dx| (dy, dx)))
                        .map(|(dy, dx)| plane[(y * vs + dy) * pw + x * hs + dx] as u32)
                        .sum();
                    small[y * cw + x] = ((sum + bias) / n) as u8;
                    bias ^= toggle;
                }
            }
            *plane = small;
        }
    }

    // Forward DCT and quantization.
    let tables = [scaled_table(&STD_LUMA_Q, opts.quality), scaled_table(&STD_CHROMA_Q, opts.quality)];
    let mut comps: Vec<Comp> = Vec::with_capacity(ncomp);
    for (ci, plane) in planes.iter().enumerate() {
        let (ch, cv) = if ci == 0 { (hs, vs) } else { (1, 1) };
        let (cpw, cph) = (pw * ch / hs, ph * cv / vs);
        let bw = cpw / 8;
        let table = if ci == 0 { 0 } else { 1 };
        let coefs = quantize_plane(plane, cpw, cph, &tables[table])?;
        comps.push(Comp {
            id: ci as u8 + 1,
            h: ch,
            v: cv,
            table,
            bw,
            real_bw: (w * ch).div_ceil(hs).div_ceil(8),
            real_bh: (h * cv).div_ceil(vs).div_ceil(8),
            coefs,
        });
    }
    drop(planes);
    write_file(&comps, &tables, w, h, opts, (mcus_x, mcus_y), None, separate_scans)
}

/// Forward DCT and quantization of a padded plane (`cpw x cph`, multiples of 8) into
/// coefficients, 64 per block in natural order.
fn quantize_plane(plane: &[u8], cpw: usize, cph: usize, qt: &[u16; 64]) -> Result<Vec<i16>, ImageError> {
    let (bw, bh) = (cpw / 8, cph / 8);
    let div: [i32; 64] = qt.map(|q| q as i32 * 8);
    let mut coefs = try_vec(bw * bh * 64, 0i16)?;
    for by in 0..bh {
        for bx in 0..bw {
            let mut blk = [0i32; 64];
            for (y, row) in blk.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                let src = &plane[(by * 8 + y) * cpw + bx * 8..][..8];
                for (d, &s) in row.iter_mut().zip(src) {
                    *d = s as i32 - 128;
                }
            }
            fdct(&mut blk);
            let out = &mut coefs[(by * bw + bx) * 64..][..64];
            for ((o, &v), &d) in out.iter_mut().zip(&blk).zip(&div) {
                let q = (v.abs() + d / 2) / d;
                *o = if v < 0 { -q } else { q } as i16;
            }
        }
    }
    Ok(coefs)
}

/// Test helper: encodes raw component planes (`w x h` each, all sampled 1x1), writing an Adobe
/// `APP14` segment with the given color transform instead of JFIF when requested. Used to
/// produce RGB, CMYK and YCCK files.
/// With `separate_scans`, a baseline file gets one scan per component.
#[cfg(test)]
pub(crate) fn encode_planes(
    planes: &[&[u8]],
    w: usize,
    h: usize,
    adobe_transform: Option<u8>,
    opts: &EncodeOptions,
    separate_scans: bool,
) -> Result<Vec<u8>, ImageError> {
    let mcus = (w.div_ceil(8), h.div_ceil(8));
    let (pw, ph) = (mcus.0 * 8, mcus.1 * 8);
    let tables = [scaled_table(&STD_LUMA_Q, opts.quality), scaled_table(&STD_CHROMA_Q, opts.quality)];
    let mut comps = Vec::new();
    for (ci, plane) in planes.iter().enumerate() {
        let padded: Vec<u8> =
            (0..ph).flat_map(|y| (0..pw).map(move |x| plane[y.min(h - 1) * w + x.min(w - 1)])).collect();
        let table = if ci == 0 { 0 } else { 1 };
        let coefs = quantize_plane(&padded, pw, ph, &tables[table])?;
        comps.push(Comp { id: ci as u8 + 1, h: 1, v: 1, table, bw: pw / 8, real_bw: mcus.0, real_bh: mcus.1, coefs });
    }
    let app14 = adobe_transform.map(|t| [b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, t]);
    write_file(&comps, &tables, w, h, opts, mcus, app14.as_ref().map(|b| (0xEE, &b[..])), separate_scans)
}

/// Writes the headers and the entropy-coded data. `app` is an application segment
/// `(marker, body)` written instead of the JFIF `APP0` (for example an Adobe `APP14`).
/// Baseline images are written with one interleaved scan unless `separate_scans` is set.
#[allow(clippy::too_many_arguments)]
fn write_file(
    comps: &[Comp],
    tables: &[[u16; 64]; 2],
    w: usize,
    h: usize,
    opts: &EncodeOptions,
    mcus: (usize, usize),
    app: Option<(u8, &[u8])>,
    separate_scans: bool,
) -> Result<Vec<u8>, ImageError> {
    let ncomp = comps.len();
    let mut out = Vec::new();
    out.try_reserve(w * h / 4 + 1024)?;
    out.extend_from_slice(&[0xFF, 0xD8]);
    match app {
        Some((code, body)) => marker(&mut out, code, body),
        None => marker(&mut out, 0xE0, b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00"),
    }
    let used_tables = if comps.iter().any(|c| c.table == 1) { 2 } else { 1 };
    for (slot, t) in tables.iter().enumerate().take(used_tables) {
        let mut body = vec![slot as u8];
        body.extend(ZIGZAG.iter().map(|&z| t[z] as u8));
        marker(&mut out, 0xDB, &body);
    }
    let mut sof = vec![8];
    sof.extend_from_slice(&(h as u16).to_be_bytes());
    sof.extend_from_slice(&(w as u16).to_be_bytes());
    sof.push(ncomp as u8);
    for c in comps {
        sof.extend_from_slice(&[c.id, (c.h as u8) << 4 | c.v as u8, c.table as u8]);
    }
    marker(&mut out, if opts.progressive { 0xC2 } else { 0xC0 }, &sof);
    if opts.restart_interval > 0 {
        marker(&mut out, 0xDD, &opts.restart_interval.to_be_bytes());
    }

    let mut e = Entropy::new(out);
    if opts.progressive {
        let script: &[ScanSpec] = match ncomp {
            1 => &GRAY_SCRIPT,
            3 => &COLOR_SCRIPT,
            _ => &FOUR_SCRIPT,
        };
        for s in script {
            progressive_scan(&mut e, comps, s, opts.restart_interval as usize, mcus);
        }
    } else {
        sequential(&mut e, comps, opts, mcus, separate_scans);
    }
    let mut out = e.w.out;
    out.extend_from_slice(&[0xFF, 0xD9]);
    Ok(out)
}

/// One step of a scan: a restart marker or a block `(component, bx, by)`.
#[derive(Clone, Copy)]
enum Step {
    Restart(usize),
    Block(usize, usize, usize),
}

/// The order in which a scan visits blocks: MCU by MCU for interleaved scans, the component's
/// own blocks in raster order for single-component scans.
fn scan_order(comps: &[Comp], scan_comps: &[usize], mcus: (usize, usize), restart_interval: usize) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut units = 0usize;
    let mut unit = |steps: &mut Vec<Step>| {
        if restart_interval != 0 && units != 0 && units.is_multiple_of(restart_interval) {
            steps.push(Step::Restart(units / restart_interval - 1));
        }
        units += 1;
    };
    if scan_comps.len() == 1 {
        let ci = scan_comps[0];
        let c = &comps[ci];
        for by in 0..c.real_bh {
            for bx in 0..c.real_bw {
                unit(&mut steps);
                steps.push(Step::Block(ci, bx, by));
            }
        }
    } else {
        for my in 0..mcus.1 {
            for mx in 0..mcus.0 {
                unit(&mut steps);
                for &ci in scan_comps {
                    let c = &comps[ci];
                    for v in 0..c.v {
                        for h in 0..c.h {
                            steps.push(Step::Block(ci, mx * c.h + h, my * c.v + v));
                        }
                    }
                }
            }
        }
    }
    steps
}

/// Writes an `RSTn` marker into the entropy-coded data.
fn write_restart(e: &mut Entropy, n: usize) {
    if !e.counting {
        e.w.flush();
        e.w.out.extend_from_slice(&[0xFF, 0xD0 + (n % 8) as u8]);
    }
}

/// Writes a sequential (baseline) image: one interleaved scan, or one scan per component when
/// `separate_scans` is set.
fn sequential(e: &mut Entropy, comps: &[Comp], opts: &EncodeOptions, mcus: (usize, usize), separate_scans: bool) {
    let scans: Vec<Vec<usize>> = if separate_scans {
        (0..comps.len()).map(|c| alloc::vec![c]).collect()
    } else {
        alloc::vec![(0..comps.len()).collect()]
    };
    let orders: Vec<Vec<Step>> =
        scans.iter().map(|s| scan_order(comps, s, mcus, opts.restart_interval as usize)).collect();
    let run = |e: &mut Entropy, order: &[Step]| {
        let mut last = [0i32; 4];
        for &step in order {
            match step {
                Step::Restart(n) => {
                    write_restart(e, n);
                    last = [0; 4];
                }
                Step::Block(ci, bx, by) => {
                    let slot = comps[ci].table * 2;
                    e.sequential_block(comps[ci].block(bx, by), &mut last[ci], slot, slot + 1);
                }
            }
        }
    };
    let specs: [Spec; 4] = if opts.optimize_huffman {
        e.counting = true;
        e.freq = [[0; 256]; 4];
        for order in &orders {
            run(e, order);
        }
        e.counting = false;
        core::array::from_fn(|i| Spec::optimal(&e.freq[i]))
    } else {
        [
            huffman::standard(false, 0),
            huffman::standard(true, 0),
            huffman::standard(false, 1),
            huffman::standard(true, 1),
        ]
    };
    let used = if comps.len() == 1 { 2 } else { 4 };
    for (i, spec) in specs.iter().enumerate().take(used) {
        write_dht(&mut e.w.out, (i % 2) as u8, (i / 2) as u8, spec);
        e.set_table(i, spec);
    }
    for (scan, order) in scans.iter().zip(&orders) {
        let mut sos = vec![scan.len() as u8];
        for &ci in scan {
            let c = &comps[ci];
            sos.extend_from_slice(&[c.id, (c.table as u8) << 4 | c.table as u8]);
        }
        sos.extend_from_slice(&[0, 63, 0]);
        marker(&mut e.w.out, 0xDA, &sos);
        run(e, order);
        e.w.flush();
    }
}

fn progressive_scan(e: &mut Entropy, comps: &[Comp], s: &ScanSpec, ri: usize, mcus: (usize, usize)) {
    let is_dc = s.ss == 0;
    // Huffman table slots: DC scans use 0 (luma) and 1 (chroma); AC scans use slot 2.
    const AC_SLOT: usize = 2;
    let order = scan_order(comps, s.comps, mcus, ri);
    let pass = |e: &mut Entropy| {
        let mut last = [0i32; 4];
        for &step in &order {
            match step {
                Step::Restart(n) => {
                    e.emit_eobrun(AC_SLOT);
                    write_restart(e, n);
                    last = [0; 4];
                }
                Step::Block(ci, bx, by) => {
                    let blk = comps[ci].block(bx, by);
                    match (is_dc, s.ah == 0) {
                        (true, true) => e.dc_first(blk, &mut last[ci], s.al, comps[ci].table),
                        (true, false) => e.dc_refine(blk, s.al),
                        (false, true) => e.ac_first(blk, s.ss, s.se, s.al, AC_SLOT),
                        (false, false) => e.ac_refine(blk, s.ss, s.se, s.al, AC_SLOT),
                    }
                }
            }
        }
        e.emit_eobrun(AC_SLOT);
    };
    // DC refinement scans carry raw bits only; every other scan gets optimized tables.
    if !(is_dc && s.ah != 0) {
        e.counting = true;
        e.freq = [[0; 256]; 4];
        pass(e);
        e.counting = false;
        let mut slots: Vec<usize> =
            if is_dc { s.comps.iter().map(|&ci| comps[ci].table).collect() } else { vec![AC_SLOT] };
        slots.dedup();
        for slot in slots {
            let spec = Spec::optimal(&e.freq[slot]);
            let (class, id) = if is_dc { (0, slot as u8) } else { (1, 0) };
            write_dht(&mut e.w.out, class, id, &spec);
            e.set_table(slot, &spec);
        }
    }
    let mut sos = vec![s.comps.len() as u8];
    for &ci in s.comps {
        let c = &comps[ci];
        let tables = if is_dc { (c.table as u8) << 4 } else { 0 };
        sos.extend_from_slice(&[c.id, tables]);
    }
    sos.extend_from_slice(&[s.ss as u8, s.se as u8, (s.ah as u8) << 4 | s.al as u8]);
    marker(&mut e.w.out, 0xDA, &sos);
    pass(e);
    e.w.flush();
}
