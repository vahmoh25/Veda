//! PNG decoding and encoding.
//!
//! The decoder supports every color type and bit depth of the PNG specification (grayscale,
//! grayscale + alpha, RGB, RGBA and palette images at 1/2/4/8/16 bits per sample), `tRNS`
//! transparency, Adam7 interlacing, image data split across any number of `IDAT` chunks, and
//! CRC verification (optional). Unknown ancillary chunks (text, color management, APNG frames,
//! ...) are ignored; unknown critical chunks are rejected. 16-bit samples are rounded to 8 bits.
//!
//! Decoding inflates all image data into one buffer whose size is known from the header, then
//! unfilters and converts it row by row straight into the output image. Palette and low-depth
//! grayscale images are converted through a 256-entry lookup table; the Sub/Average/Paeth
//! filters are specialized per pixel size and use a branch-free Paeth predictor.
//!
//! The encoder writes 8-bit RGBA, or RGB when every pixel is opaque, chooses a filter for each
//! row adaptively (the one with the smallest sum of absolute differences) and compresses with
//! [`crate::deflate`].

use alloc::vec;
use alloc::vec::Vec;

use crate::checksum::{crc32, crc32_update};
use crate::error::ImageError;
use crate::image::Image;
use crate::util::{be32, try_vec};
use crate::{DecodeOptions, inflate};

/// The eight bytes every PNG file starts with.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The color type of a PNG image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorType {
    /// Grayscale (1, 2, 4, 8 or 16 bits).
    Gray,
    /// RGB (8 or 16 bits per sample).
    Rgb,
    /// Palette indices (1, 2, 4 or 8 bits).
    Indexed,
    /// Grayscale with alpha (8 or 16 bits per sample).
    GrayAlpha,
    /// RGB with alpha (8 or 16 bits per sample).
    Rgba,
}

impl ColorType {
    fn from_code(code: u8) -> Option<ColorType> {
        Some(match code {
            0 => ColorType::Gray,
            2 => ColorType::Rgb,
            3 => ColorType::Indexed,
            4 => ColorType::GrayAlpha,
            6 => ColorType::Rgba,
            _ => return None,
        })
    }

    /// Samples per pixel.
    pub fn channels(self) -> usize {
        match self {
            ColorType::Gray | ColorType::Indexed => 1,
            ColorType::GrayAlpha => 2,
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
        }
    }
}

/// Header information of a PNG file (see [`read_info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PngInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per sample (1, 2, 4, 8 or 16).
    pub bit_depth: u8,
    /// Color type.
    pub color_type: ColorType,
    /// Adam7 interlacing.
    pub interlaced: bool,
    /// An alpha channel or a `tRNS` chunk is present.
    pub has_alpha: bool,
}

/// Parsed `IHDR`.
#[derive(Debug, Clone, Copy)]
struct Header {
    width: u32,
    height: u32,
    depth: u8,
    color: ColorType,
    interlaced: bool,
}

impl Header {
    fn parse(body: &[u8]) -> Result<Header, ImageError> {
        if body.len() != 13 {
            return Err(ImageError::Invalid("IHDR has the wrong length"));
        }
        let width = be32(body, 0)?;
        let height = be32(body, 4)?;
        let depth = body[8];
        let color = ColorType::from_code(body[9]).ok_or(ImageError::Invalid("unknown PNG color type"))?;
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err(ImageError::Invalid("invalid PNG dimensions"));
        }
        let depth_ok = match color {
            ColorType::Gray => matches!(depth, 1 | 2 | 4 | 8 | 16),
            ColorType::Indexed => matches!(depth, 1 | 2 | 4 | 8),
            _ => matches!(depth, 8 | 16),
        };
        if !depth_ok {
            return Err(ImageError::Invalid("invalid bit depth for the PNG color type"));
        }
        if body[10] != 0 || body[11] != 0 {
            return Err(ImageError::Unsupported("unknown PNG compression or filter method"));
        }
        let interlaced = match body[12] {
            0 => false,
            1 => true,
            _ => return Err(ImageError::Unsupported("unknown PNG interlace method")),
        };
        Ok(Header { width, height, depth, color, interlaced })
    }

    fn bits_per_pixel(&self) -> usize {
        self.color.channels() * self.depth as usize
    }

    /// Bytes per row of `w` pixels, excluding the filter byte.
    fn row_bytes(&self, w: usize) -> usize {
        (w * self.bits_per_pixel()).div_ceil(8)
    }

    /// Distance in bytes to the "left" byte used by the filters.
    fn filter_bpp(&self) -> usize {
        (self.bits_per_pixel() / 8).max(1)
    }

    /// Size of the decompressed (filtered) image data.
    fn raw_size(&self) -> Option<usize> {
        let (w, h) = (self.width as usize, self.height as usize);
        if !self.interlaced {
            return h.checked_mul(self.row_bytes(w).checked_add(1)?);
        }
        let mut total = 0usize;
        for p in ADAM7 {
            let (pw, ph) = pass_size(p, w, h);
            if pw > 0 && ph > 0 {
                total = total.checked_add(ph.checked_mul(self.row_bytes(pw) + 1)?)?;
            }
        }
        Some(total)
    }
}

/// Adam7 passes as `(x0, y0, dx, dy)`.
const ADAM7: [(usize, usize, usize, usize); 7] =
    [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

fn pass_size((x0, y0, dx, dy): (usize, usize, usize, usize), w: usize, h: usize) -> (usize, usize) {
    let pw = if w > x0 { (w - x0).div_ceil(dx) } else { 0 };
    let ph = if h > y0 { (h - y0).div_ceil(dy) } else { 0 };
    (pw, ph)
}

/// Chunk type reported for an ancillary chunk whose CRC did not match (it is then ignored).
const DROPPED: [u8; 4] = *b"dRoP";

/// One chunk: type and body.
struct Chunk<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

/// Iterates over the chunks after the signature, optionally verifying CRCs.
struct Chunks<'a> {
    data: &'a [u8],
    pos: usize,
    verify: bool,
}

impl<'a> Chunks<'a> {
    fn next_chunk(&mut self) -> Result<Option<Chunk<'a>>, ImageError> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let len = be32(self.data, self.pos)? as usize;
        if len > i32::MAX as usize {
            return Err(ImageError::Invalid("PNG chunk length is too large"));
        }
        let kind: [u8; 4] = crate::util::bytes(self.data, self.pos + 4)?;
        let body = self.data.get(self.pos + 8..self.pos + 8 + len).ok_or(ImageError::Truncated)?;
        let crc = be32(self.data, self.pos + 8 + len)?;
        self.pos += 12 + len;
        if self.verify && crc32_update(crc32(&kind), body) != crc {
            if kind[0] & 0x20 == 0 {
                return Err(ImageError::ChecksumMismatch("PNG chunk CRC"));
            }
            // Like libpng: a damaged ancillary chunk is dropped, not fatal.
            return Ok(Some(Chunk { kind: DROPPED, body: &[] }));
        }
        Ok(Some(Chunk { kind, body }))
    }
}

/// Everything the decoder needs from the chunk stream.
struct Parsed<'a> {
    header: Header,
    palette: &'a [u8],
    trns: Option<&'a [u8]>,
    idat: Vec<&'a [u8]>,
}

fn parse(data: &[u8], verify: bool, stop_at_idat: bool) -> Result<Parsed<'_>, ImageError> {
    if !data.starts_with(&SIGNATURE) {
        return Err(ImageError::UnknownFormat);
    }
    let mut chunks = Chunks { data, pos: 8, verify };
    let first = chunks.next_chunk()?.ok_or(ImageError::Truncated)?;
    if &first.kind != b"IHDR" {
        return Err(ImageError::Invalid("PNG does not start with IHDR"));
    }
    let header = Header::parse(first.body)?;
    let mut parsed = Parsed { header, palette: &[], trns: None, idat: Vec::new() };
    let mut seen_iend = false;
    loop {
        let chunk = match chunks.next_chunk() {
            Ok(Some(c)) => c,
            Ok(None) => break,
            // Tolerate damage after the image data (a truncated or garbled trailer).
            Err(ImageError::Truncated) if !parsed.idat.is_empty() => break,
            Err(e) => return Err(e),
        };
        match &chunk.kind {
            b"IDAT" => {
                if stop_at_idat {
                    parsed.idat.push(chunk.body);
                    return Ok(parsed);
                }
                parsed.idat.push(chunk.body);
            }
            b"PLTE" => {
                if chunk.body.len() % 3 != 0 || chunk.body.len() > 768 {
                    return Err(ImageError::Invalid("invalid PLTE chunk"));
                }
                parsed.palette = chunk.body;
            }
            b"tRNS" => parsed.trns = Some(chunk.body),
            b"IEND" => {
                seen_iend = true;
                break;
            }
            b"IHDR" => return Err(ImageError::Invalid("duplicate IHDR chunk")),
            kind if kind[0] & 0x20 == 0 => return Err(ImageError::Unsupported("unknown critical PNG chunk")),
            _ => {}
        }
    }
    if parsed.idat.is_empty() {
        return Err(if seen_iend { ImageError::Invalid("PNG has no image data") } else { ImageError::Truncated });
    }
    if header.color == ColorType::Indexed && parsed.palette.is_empty() {
        return Err(ImageError::Invalid("palette image without PLTE chunk"));
    }
    Ok(parsed)
}

/// Reads the PNG header (and scans for a `tRNS` chunk before the image data) without decoding.
pub fn read_info(data: &[u8]) -> Result<PngInfo, ImageError> {
    let p = parse(data, false, true)?;
    let h = p.header;
    Ok(PngInfo {
        width: h.width,
        height: h.height,
        bit_depth: h.depth,
        color_type: h.color,
        interlaced: h.interlaced,
        has_alpha: matches!(h.color, ColorType::GrayAlpha | ColorType::Rgba) || p.trns.is_some(),
    })
}

/// Decodes a PNG image with the default [`DecodeOptions`].
pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    decode_with(data, &DecodeOptions::DEFAULT)
}

/// Decodes a PNG image.
pub fn decode_with(data: &[u8], options: &DecodeOptions) -> Result<Image, ImageError> {
    let p = parse(data, options.verify_checksums, false)?;
    let h = p.header;
    options.check_dimensions(h.width, h.height)?;
    let raw_size = h.raw_size().ok_or(ImageError::TooLarge { width: h.width, height: h.height })?;

    let idat_len: usize = p.idat.iter().map(|c| c.len()).sum();
    // DEFLATE expands by at most 1032:1; refuse to allocate for data that cannot be there.
    if raw_size / 1032 > idat_len + 64 {
        return Err(ImageError::Truncated);
    }
    let raw = if p.idat.len() == 1 {
        inflate::zlib_decompress_exact(p.idat[0], raw_size, options.verify_checksums)?
    } else {
        let mut joined = Vec::new();
        joined.try_reserve_exact(idat_len)?;
        for c in &p.idat {
            joined.extend_from_slice(c);
        }
        inflate::zlib_decompress_exact(&joined, raw_size, options.verify_checksums)?
    };

    let conv = Converter::new(&h, p.palette, p.trns);
    let mut img = Image::try_new(h.width, h.height)?;
    if h.interlaced {
        decode_adam7(&h, raw, &conv, &mut img)?;
    } else {
        decode_rows(&h, raw, &conv, &mut img)?;
    }
    Ok(img)
}

fn decode_rows(h: &Header, mut raw: Vec<u8>, conv: &Converter, img: &mut Image) -> Result<(), ImageError> {
    let w = h.width as usize;
    let stride = h.row_bytes(w);
    let bpp = h.filter_bpp();
    let zero = vec![0u8; stride];
    for (y, out_row) in img.pixels.chunks_exact_mut(w).enumerate() {
        let start = y * (stride + 1);
        let (before, rest) = raw.split_at_mut(start);
        let (filter, rest) = rest.split_first_mut().ok_or(ImageError::Truncated)?;
        let cur = rest.get_mut(..stride).ok_or(ImageError::Truncated)?;
        let prev = if y == 0 { &zero[..] } else { &before[start - stride..start] };
        unfilter(*filter, cur, prev, bpp)?;
        conv.convert(cur, out_row);
    }
    Ok(())
}

fn decode_adam7(h: &Header, mut raw: Vec<u8>, conv: &Converter, img: &mut Image) -> Result<(), ImageError> {
    let (w, hgt) = (h.width as usize, h.height as usize);
    let bpp = h.filter_bpp();
    let mut tmp = try_vec(w, 0u32)?;
    let zero = try_vec(h.row_bytes(w), 0u8)?;
    let mut offset = 0;
    for pass in ADAM7 {
        let (x0, y0, dx, dy) = pass;
        let (pw, ph) = pass_size(pass, w, hgt);
        if pw == 0 || ph == 0 {
            continue;
        }
        let stride = h.row_bytes(pw);
        let data = raw.get_mut(offset..offset + ph * (stride + 1)).ok_or(ImageError::Truncated)?;
        offset += ph * (stride + 1);
        for r in 0..ph {
            let start = r * (stride + 1);
            let (before, rest) = data.split_at_mut(start);
            let (filter, rest) = rest.split_first_mut().ok_or(ImageError::Truncated)?;
            let cur = &mut rest[..stride];
            let prev = if r == 0 { &zero[..stride] } else { &before[start - stride..start] };
            unfilter(*filter, cur, prev, bpp)?;
            conv.convert(cur, &mut tmp[..pw]);
            let y = y0 + r * dy;
            let row = &mut img.pixels[y * w..y * w + w];
            for (i, &px) in tmp[..pw].iter().enumerate() {
                row[x0 + i * dx] = px;
            }
        }
    }
    Ok(())
}

/// Reverses one row's filter in place. `prev` is the previous (already unfiltered) row, or zeros.
fn unfilter(filter: u8, cur: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), ImageError> {
    macro_rules! by_bpp {
        ($f:ident $(, $arg:expr)*) => {
            match bpp {
                1 => $f::<1>(cur $(, $arg)*),
                2 => $f::<2>(cur $(, $arg)*),
                3 => $f::<3>(cur $(, $arg)*),
                4 => $f::<4>(cur $(, $arg)*),
                6 => $f::<6>(cur $(, $arg)*),
                _ => $f::<8>(cur $(, $arg)*),
            }
        };
    }
    match filter {
        0 => {}
        1 => by_bpp!(unfilter_sub),
        2 => {
            for (c, &p) in cur.iter_mut().zip(prev) {
                *c = c.wrapping_add(p);
            }
        }
        3 => by_bpp!(unfilter_avg, prev),
        4 => by_bpp!(unfilter_paeth, prev),
        _ => return Err(ImageError::Invalid("unknown PNG filter type")),
    }
    Ok(())
}

fn unfilter_sub<const N: usize>(cur: &mut [u8]) {
    let mut a = [0u8; N];
    for px in cur.as_chunks_mut::<N>().0 {
        for i in 0..N {
            px[i] = px[i].wrapping_add(a[i]);
            a[i] = px[i];
        }
    }
}

fn unfilter_avg<const N: usize>(cur: &mut [u8], prev: &[u8]) {
    let mut a = [0u8; N];
    for (px, up) in cur.as_chunks_mut::<N>().0.iter_mut().zip(prev.as_chunks::<N>().0) {
        for i in 0..N {
            let v = px[i].wrapping_add(((a[i] as u16 + up[i] as u16) >> 1) as u8);
            px[i] = v;
            a[i] = v;
        }
    }
}

fn unfilter_paeth<const N: usize>(cur: &mut [u8], prev: &[u8]) {
    let mut a = [0u8; N];
    let mut c = [0u8; N];
    for (px, up) in cur.as_chunks_mut::<N>().0.iter_mut().zip(prev.as_chunks::<N>().0) {
        for i in 0..N {
            let b = up[i];
            let v = px[i].wrapping_add(paeth(a[i], b, c[i]));
            px[i] = v;
            a[i] = v;
            c[i] = b;
        }
    }
}

/// The Paeth predictor, branch-free (the comparisons compile to conditional moves).
#[inline(always)]
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (a as i16, b as i16, c as i16);
    let pa = (ib - ic).abs();
    let pb = (ia - ic).abs();
    let pc = (ia + ib - 2 * ic).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Rounds a 16-bit sample to 8 bits.
#[inline(always)]
fn scale16(hi: u8, lo: u8) -> u32 {
    let v = ((hi as u32) << 8) | lo as u32;
    (v * 255 + 32895) >> 16
}

/// Packs 8-bit channels into a `0xAARRGGBB` pixel.
#[inline(always)]
fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Converts unfiltered rows to `0xAARRGGBB` pixels.
struct Converter {
    color: ColorType,
    depth: u8,
    /// Index/gray value to pixel, for palette images and grayscale up to 8 bits.
    lut: [u32; 256],
    trns_gray: Option<u16>,
    trns_rgb: Option<[u16; 3]>,
}

impl Converter {
    fn new(h: &Header, palette: &[u8], trns: Option<&[u8]>) -> Converter {
        let mut c =
            Converter { color: h.color, depth: h.depth, lut: [0xFF00_0000; 256], trns_gray: None, trns_rgb: None };
        match h.color {
            ColorType::Indexed => {
                for (entry, &[r, g, b]) in c.lut.iter_mut().zip(palette.as_chunks::<3>().0) {
                    *entry = argb(255, r as u32, g as u32, b as u32);
                }
                if let Some(t) = trns {
                    for (entry, &a) in c.lut.iter_mut().zip(t) {
                        *entry = (*entry & 0x00FF_FFFF) | ((a as u32) << 24);
                    }
                }
            }
            ColorType::Gray => {
                c.trns_gray = trns.and_then(|t| crate::util::be16(t, 0).ok());
                if h.depth <= 8 {
                    let max = (1u32 << h.depth) - 1;
                    for v in 0..=max {
                        let g = v * 255 / max;
                        let a = if c.trns_gray == Some(v as u16) { 0 } else { 255 };
                        c.lut[v as usize] = argb(a, g, g, g);
                    }
                }
            }
            ColorType::Rgb => {
                if let Some(t) = trns
                    && let (Ok(r), Ok(g), Ok(b)) =
                        (crate::util::be16(t, 0), crate::util::be16(t, 2), crate::util::be16(t, 4))
                {
                    c.trns_rgb = Some([r, g, b]);
                }
            }
            ColorType::GrayAlpha | ColorType::Rgba => {}
        }
        c
    }

    fn convert(&self, src: &[u8], dst: &mut [u32]) {
        let lut = &self.lut;
        match (self.color, self.depth) {
            (ColorType::Gray | ColorType::Indexed, 8) => {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = lut[s as usize];
                }
            }
            (ColorType::Gray | ColorType::Indexed, depth) if depth < 8 => {
                let per_byte = 8 / depth as usize;
                let mask = (1u8 << depth) - 1;
                for (chunk, &byte) in dst.chunks_mut(per_byte).zip(src) {
                    let mut shift = 8 - depth as i32;
                    for d in chunk {
                        *d = lut[((byte >> shift) & mask) as usize];
                        shift -= depth as i32;
                    }
                }
            }
            (ColorType::Gray, _) => {
                for (d, &[hi, lo]) in dst.iter_mut().zip(src.as_chunks::<2>().0) {
                    let a = if self.trns_gray == Some(u16::from_be_bytes([hi, lo])) { 0 } else { 255 };
                    let g = scale16(hi, lo);
                    *d = argb(a, g, g, g);
                }
            }
            (ColorType::GrayAlpha, 8) => {
                for (d, &[g, a]) in dst.iter_mut().zip(src.as_chunks::<2>().0) {
                    let g = g as u32;
                    *d = argb(a as u32, g, g, g);
                }
            }
            (ColorType::GrayAlpha, _) => {
                for (d, &[g0, g1, a0, a1]) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                    let g = scale16(g0, g1);
                    *d = argb(scale16(a0, a1), g, g, g);
                }
            }
            (ColorType::Rgb, 8) => match self.trns_rgb {
                None => {
                    for (d, &[r, g, b]) in dst.iter_mut().zip(src.as_chunks::<3>().0) {
                        *d = argb(255, r as u32, g as u32, b as u32);
                    }
                }
                Some(key) => {
                    for (d, &[r, g, b]) in dst.iter_mut().zip(src.as_chunks::<3>().0) {
                        let a = if [r as u16, g as u16, b as u16] == key { 0 } else { 255 };
                        *d = argb(a, r as u32, g as u32, b as u32);
                    }
                }
            },
            (ColorType::Rgb, _) => {
                for (d, &[r0, r1, g0, g1, b0, b1]) in dst.iter_mut().zip(src.as_chunks::<6>().0) {
                    let key =
                        [u16::from_be_bytes([r0, r1]), u16::from_be_bytes([g0, g1]), u16::from_be_bytes([b0, b1])];
                    let a = if Some(key) == self.trns_rgb { 0 } else { 255 };
                    *d = argb(a, scale16(r0, r1), scale16(g0, g1), scale16(b0, b1));
                }
            }
            (ColorType::Rgba, 8) => {
                for (d, &[r, g, b, a]) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                    *d = u32::from_le_bytes([b, g, r, a]);
                }
            }
            (ColorType::Rgba, _) => {
                for (d, &[r0, r1, g0, g1, b0, b1, a0, a1]) in dst.iter_mut().zip(src.as_chunks::<8>().0) {
                    *d = argb(scale16(a0, a1), scale16(r0, r1), scale16(g0, g1), scale16(b0, b1));
                }
            }
            (ColorType::Indexed, _) => {}
        }
    }
}

/// Appends a chunk (length, type, body, CRC) to `out`.
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encodes `image` as PNG with zlib compression `level` (0 = store, 9 = smallest; 6 is a good
/// default).
///
/// Fully opaque images are written as 8-bit RGB, others as 8-bit RGBA. Each row gets the filter
/// that minimizes the sum of absolute differences (no filtering at level 0).
pub fn encode(image: &Image, level: u8) -> Result<Vec<u8>, ImageError> {
    let (w, h) = (image.width as usize, image.height as usize);
    if image.is_empty() {
        return Err(ImageError::InvalidArgument("cannot encode an empty image"));
    }
    if image.pixels.len() != w * h {
        return Err(ImageError::InvalidArgument("pixel buffer length does not match the dimensions"));
    }
    if image.width > i32::MAX as u32 || image.height > i32::MAX as u32 {
        return Err(ImageError::InvalidArgument("image is too large for PNG"));
    }
    let opaque = !image.has_alpha();
    let raw = filtered_scanlines(image, opaque, level != 0)?;
    let z = crate::deflate::zlib_compress(&raw, level);
    drop(raw);

    let mut out = Vec::with_capacity(z.len() + 64);
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&image.width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&image.height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = if opaque { 2 } else { 6 };
    write_chunk(&mut out, b"IHDR", &ihdr);
    for part in z.chunks(1 << 20) {
        write_chunk(&mut out, b"IDAT", part);
    }
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// Packs `image` as 8-bit RGB (`opaque`) or RGBA rows, each preceded by its filter type: the
/// data that is zlib-compressed into `IDAT`. With `adaptive`, every row gets the filter with the
/// smallest sum of absolute differences; otherwise no filtering.
pub(crate) fn filtered_scanlines(image: &Image, opaque: bool, adaptive: bool) -> Result<Vec<u8>, ImageError> {
    let w = image.width as usize;
    let h = image.height as usize;
    let bpp = if opaque { 3 } else { 4 };
    let stride = w * bpp;
    let mut raw = Vec::new();
    raw.try_reserve_exact(h * (stride + 1))?;
    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    let mut cand: [Vec<u8>; 4] = core::array::from_fn(|_| vec![0u8; stride]);
    for row in image.pixels.chunks_exact(w) {
        if opaque {
            for (d, &p) in cur.as_chunks_mut::<3>().0.iter_mut().zip(row) {
                d.copy_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
        } else {
            for (d, &p) in cur.as_chunks_mut::<4>().0.iter_mut().zip(row) {
                d.copy_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]);
            }
        }
        if !adaptive {
            raw.push(0);
            raw.extend_from_slice(&cur);
        } else {
            let filter = choose_filter(&cur, &prev, bpp, &mut cand);
            raw.push(filter as u8);
            raw.extend_from_slice(if filter == 0 { &cur } else { &cand[filter - 1] });
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    Ok(raw)
}

/// Computes the Sub, Up, Average and Paeth filtered versions of `cur` into `cand` and returns the
/// filter type (0..=4) with the smallest sum of absolute (signed) byte values.
fn choose_filter(cur: &[u8], prev: &[u8], bpp: usize, cand: &mut [Vec<u8>; 4]) -> usize {
    let cost = |v: &[u8]| -> u64 { v.iter().map(|&b| (b as i8).unsigned_abs() as u64).sum() };
    let [sub, up, avg, pae] = cand;
    for i in 0..cur.len() {
        let x = cur[i];
        let b = prev[i];
        let (a, c) = if i >= bpp { (cur[i - bpp], prev[i - bpp]) } else { (0, 0) };
        sub[i] = x.wrapping_sub(a);
        up[i] = x.wrapping_sub(b);
        avg[i] = x.wrapping_sub(((a as u16 + b as u16) >> 1) as u8);
        pae[i] = x.wrapping_sub(paeth(a, b, c));
    }
    let mut best = 0;
    let mut best_cost = cost(cur);
    for (k, buf) in cand.iter().enumerate() {
        let c = cost(buf);
        if c < best_cost {
            best_cost = c;
            best = k + 1;
        }
    }
    best
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Test-only PNG writer able to produce every color type, bit depth and interlace mode.
    ///
    /// `samples` holds `width * height * channels` sample values (already in the target bit
    /// depth). Rows cycle through all five filter types; `idat_split` splits the zlib stream
    /// into chunks of that many bytes.
    pub(crate) struct TestPng<'a> {
        pub width: usize,
        pub height: usize,
        pub color: u8,
        pub depth: u8,
        pub interlaced: bool,
        pub samples: &'a [u16],
        pub palette: &'a [u8],
        pub trns: Option<&'a [u8]>,
        pub idat_split: usize,
        pub level: u8,
    }

    impl TestPng<'_> {
        pub(crate) fn channels(&self) -> usize {
            match self.color {
                0 | 3 => 1,
                4 => 2,
                2 => 3,
                _ => 4,
            }
        }

        fn pack_row(&self, xs: &[usize], y: usize) -> Vec<u8> {
            let ch = self.channels();
            let mut bits: Vec<u8> = Vec::new();
            let mut acc = 0u32;
            let mut nbits = 0;
            for &x in xs {
                for c in 0..ch {
                    let v = self.samples[(y * self.width + x) * ch + c] as u32;
                    if self.depth == 16 {
                        bits.extend_from_slice(&(v as u16).to_be_bytes());
                    } else if self.depth == 8 {
                        bits.push(v as u8);
                    } else {
                        acc = (acc << self.depth) | v;
                        nbits += self.depth as u32;
                        if nbits == 8 {
                            bits.push(acc as u8);
                            acc = 0;
                            nbits = 0;
                        }
                    }
                }
            }
            if nbits > 0 {
                bits.push((acc << (8 - nbits)) as u8);
            }
            bits
        }

        fn filter_rows(&self, rows: &[Vec<u8>], out: &mut Vec<u8>, counter: &mut usize) {
            let bpp = ((self.channels() * self.depth as usize) / 8).max(1);
            let mut prev = vec![0u8; rows.first().map_or(0, |r| r.len())];
            for row in rows {
                let f = *counter % 5;
                *counter += 1;
                out.push(f as u8);
                for i in 0..row.len() {
                    let a = if i >= bpp { row[i - bpp] } else { 0 };
                    let b = prev[i];
                    let c = if i >= bpp { prev[i - bpp] } else { 0 };
                    let pred = match f {
                        0 => 0,
                        1 => a,
                        2 => b,
                        3 => ((a as u16 + b as u16) / 2) as u8,
                        _ => paeth(a, b, c),
                    };
                    out.push(row[i].wrapping_sub(pred));
                }
                prev = row.clone();
            }
        }

        pub(crate) fn build(&self) -> Vec<u8> {
            let mut raw = Vec::new();
            let mut counter = 0;
            if self.interlaced {
                for pass in ADAM7 {
                    let (x0, y0, dx, dy) = pass;
                    let (pw, ph) = pass_size(pass, self.width, self.height);
                    if pw == 0 || ph == 0 {
                        continue;
                    }
                    let xs: Vec<usize> = (0..pw).map(|i| x0 + i * dx).collect();
                    let rows: Vec<Vec<u8>> = (0..ph).map(|r| self.pack_row(&xs, y0 + r * dy)).collect();
                    self.filter_rows(&rows, &mut raw, &mut counter);
                }
            } else {
                let xs: Vec<usize> = (0..self.width).collect();
                let rows: Vec<Vec<u8>> = (0..self.height).map(|y| self.pack_row(&xs, y)).collect();
                self.filter_rows(&rows, &mut raw, &mut counter);
            }
            let z = crate::deflate::zlib_compress(&raw, self.level);
            let mut out = SIGNATURE.to_vec();
            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&(self.width as u32).to_be_bytes());
            ihdr.extend_from_slice(&(self.height as u32).to_be_bytes());
            ihdr.extend_from_slice(&[self.depth, self.color, 0, 0, self.interlaced as u8]);
            write_chunk(&mut out, b"IHDR", &ihdr);
            write_chunk(&mut out, b"tEXt", b"Comment\0vimage test");
            if !self.palette.is_empty() {
                write_chunk(&mut out, b"PLTE", self.palette);
            }
            if let Some(t) = self.trns {
                write_chunk(&mut out, b"tRNS", t);
            }
            for part in z.chunks(self.idat_split.max(1)) {
                write_chunk(&mut out, b"IDAT", part);
            }
            write_chunk(&mut out, b"IEND", &[]);
            out
        }

        /// The pixels a correct decoder must produce.
        pub(crate) fn expected(&self) -> Vec<u32> {
            let ch = self.channels();
            let max = (1u32 << self.depth) - 1;
            let to8 = |v: u16| -> u32 {
                if self.depth == 16 { (v as u32 * 255 + 32895) >> 16 } else { v as u32 * 255 / max }
            };
            let trns16 = |i: usize| -> Option<u16> {
                self.trns.and_then(|t| t.get(i * 2..i * 2 + 2)).map(|b| u16::from_be_bytes([b[0], b[1]]))
            };
            (0..self.width * self.height)
                .map(|i| {
                    let s = &self.samples[i * ch..i * ch + ch];
                    match self.color {
                        0 => {
                            let a = if trns16(0) == Some(s[0]) { 0 } else { 255 };
                            (a << 24) | (to8(s[0]) * 0x10101)
                        }
                        4 => (to8(s[1]) << 24) | (to8(s[0]) * 0x10101),
                        2 => {
                            let t = [trns16(0), trns16(1), trns16(2)];
                            let a = if t == [Some(s[0]), Some(s[1]), Some(s[2])] { 0 } else { 255 };
                            a << 24 | to8(s[0]) << 16 | to8(s[1]) << 8 | to8(s[2])
                        }
                        6 => to8(s[3]) << 24 | to8(s[0]) << 16 | to8(s[1]) << 8 | to8(s[2]),
                        _ => {
                            let idx = s[0] as usize;
                            let p = &self.palette[idx * 3..idx * 3 + 3];
                            let a = self.trns.and_then(|t| t.get(idx)).map_or(255, |&a| a as u32);
                            a << 24 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32
                        }
                    }
                })
                .collect()
        }
    }

    fn rng(seed: &mut u32) -> u32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        *seed
    }

    #[test]
    fn every_color_type_depth_and_interlace() {
        let combos: [(u8, &[u8]); 5] =
            [(0, &[1, 2, 4, 8, 16]), (2, &[8, 16]), (3, &[1, 2, 4, 8]), (4, &[8, 16]), (6, &[8, 16])];
        let sizes = [(1, 1), (3, 5), (17, 9), (8, 8), (33, 2), (2, 33)];
        let mut seed = 0x1234_5678;
        for (color, depths) in combos {
            for &depth in depths {
                for interlaced in [false, true] {
                    for &(w, h) in &sizes {
                        let ch = match color {
                            0 | 3 => 1,
                            4 => 2,
                            2 => 3,
                            _ => 4,
                        };
                        let max = if color == 3 { (1u32 << depth).min(13) } else { 1u32 << depth };
                        let samples: Vec<u16> = (0..w * h * ch)
                            .map(|i| if i % 7 == 0 { (max - 1) as u16 } else { (rng(&mut seed) % max) as u16 })
                            .collect();
                        let palette: Vec<u8> =
                            if color == 3 { (0..13 * 3).map(|i| (i * 19) as u8).collect() } else { Vec::new() };
                        // tRNS: palette alpha for some entries, or a key color equal to the first sample.
                        let trns: Option<Vec<u8>> = match color {
                            3 => Some((0..7).map(|i| (i * 40) as u8).collect()),
                            0 => Some(samples[0].to_be_bytes().to_vec()),
                            2 => Some(samples[..3].iter().flat_map(|s| s.to_be_bytes()).collect()),
                            _ => None,
                        };
                        let t = TestPng {
                            width: w,
                            height: h,
                            color,
                            depth,
                            interlaced,
                            samples: &samples,
                            palette: &palette,
                            trns: trns.as_deref(),
                            idat_split: 7,
                            level: 6,
                        };
                        let file = t.build();
                        let img = decode(&file).unwrap_or_else(|e| panic!("color {color} depth {depth} {w}x{h}: {e}"));
                        assert_eq!((img.width as usize, img.height as usize), (w, h));
                        assert_eq!(
                            img.pixels,
                            t.expected(),
                            "color {color} depth {depth} interlaced {interlaced} {w}x{h}"
                        );
                        let info = read_info(&file).unwrap();
                        assert_eq!(info.interlaced, interlaced);
                        assert_eq!(info.bit_depth, depth);
                    }
                }
            }
        }
    }

    #[test]
    fn round_trip() {
        let mut seed = 99;
        for &(w, h) in &[(1, 1), (3, 5), (17, 9), (64, 40), (300, 7)] {
            let gradient = Image::from_fn(w, h, |x, y| 0xFF00_0000 | (x * 255 / w) << 16 | (y * 255 / h) << 8 | 0x40);
            let noise = Image::from_fn(w, h, |_, _| rng(&mut seed));
            let alpha = Image::from_fn(w, h, |x, y| ((x + y) % 3 * 120) << 24 | 0x00AB_CDEF);
            for img in [&gradient, &noise, &alpha] {
                for level in [0, 1, 6, 9] {
                    let file = encode(img, level).unwrap();
                    let back = decode(&file).unwrap();
                    assert_eq!(&back, img, "{w}x{h} level {level}");
                }
            }
            let info = read_info(&encode(&gradient, 6).unwrap()).unwrap();
            assert_eq!(info.color_type, ColorType::Rgb);
            assert!(!info.has_alpha);
            assert_eq!(read_info(&encode(&alpha, 6).unwrap()).unwrap().color_type, ColorType::Rgba);
        }
    }

    #[test]
    fn chunk_handling() {
        let img = Image::from_fn(10, 10, |x, y| 0xFF00_0000 | x << 4 | y);
        let file = encode(&img, 6).unwrap();
        // Corrupt the IHDR CRC: rejected, unless checksums are not verified.
        let mut bad = file.clone();
        bad[29] ^= 0xFF;
        assert_eq!(decode(&bad), Err(ImageError::ChecksumMismatch("PNG chunk CRC")));
        let lax = DecodeOptions { verify_checksums: false, ..DecodeOptions::DEFAULT };
        assert_eq!(decode_with(&bad, &lax).unwrap(), img);
        // Unknown critical chunk before IDAT.
        let mut crit = file[..33].to_vec();
        write_chunk(&mut crit, b"CgBI", &[1, 2, 3]);
        crit.extend_from_slice(&file[33..]);
        assert!(matches!(decode(&crit), Err(ImageError::Unsupported(_))));
        // Missing IEND is tolerated.
        assert_eq!(decode(&file[..file.len() - 12]).unwrap(), img);
        // Truncated image data is not.
        assert!(decode(&file[..file.len() - 30]).is_err());
        // Limits.
        let small = DecodeOptions { max_dimension: 9, ..DecodeOptions::DEFAULT };
        assert_eq!(decode_with(&file, &small), Err(ImageError::TooLarge { width: 10, height: 10 }));
    }
}
