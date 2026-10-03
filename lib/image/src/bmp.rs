//! BMP (Windows/OS/2 bitmap) decoding and encoding.
//!
//! Supported headers: `BITMAPCOREHEADER` (OS/2 1.x), `BITMAPINFOHEADER` and its V2/V3 variants,
//! OS/2 2.x headers, `BITMAPV4HEADER` and `BITMAPV5HEADER`. Supported pixel formats:
//!
//! * 1, 4 and 8 bits per pixel with a palette, uncompressed or RLE4/RLE8 compressed;
//! * 16 bits (5:5:5 by default, or any channel masks via `BI_BITFIELDS`);
//! * 24 bits (BGR);
//! * 32 bits (BGRX/BGRA, or channel masks via `BI_BITFIELDS` / `BI_ALPHABITFIELDS`);
//! * embedded JPEG or PNG data (`BI_JPEG` / `BI_PNG`).
//!
//! Both bottom-up (positive height) and top-down (negative height) row orders are handled.
//!
//! Alpha follows the behavior of web browsers: a 32-bit image's fourth byte (or a 16/32-bit
//! image's alpha mask) is used as alpha unless it is zero for every pixel, in which case the image
//! is treated as opaque. Pixels skipped by RLE "delta" or "end of line" codes are transparent.
//!
//! The encoder writes 32-bit BGRA with a `BITMAPV4HEADER` and explicit channel masks, which
//! preserves alpha and is understood by Windows and every common decoder.

use alloc::vec::Vec;

use crate::error::ImageError;
use crate::image::Image;
use crate::util::{le16, le32, try_vec};
use crate::DecodeOptions;

const BI_RGB: u32 = 0;
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_JPEG: u32 = 4;
const BI_PNG: u32 = 5;
const BI_ALPHABITFIELDS: u32 = 6;

/// Header information of a BMP file (see [`read_info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BmpInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per pixel (1, 4, 8, 16, 24 or 32; 0 for embedded JPEG/PNG).
    pub bits_per_pixel: u16,
    /// The `biCompression` value (0 = none, 1 = RLE8, 2 = RLE4, 3 = bit fields, ...).
    pub compression: u32,
    /// Rows are stored top to bottom (negative height in the header).
    pub top_down: bool,
    /// The decoded image will contain non-opaque pixels (32-bit images with a meaningful alpha
    /// channel). RLE images are reported as opaque even if some pixels are skipped.
    pub has_alpha: bool,
}

/// Returns `true` if `data` looks like a BMP file (signature and a known header size).
pub(crate) fn is_bmp(data: &[u8]) -> bool {
    data.len() >= 18 && data.starts_with(b"BM") && matches!(le32(data, 14), Ok(12 | 16..=64 | 108 | 124))
}

/// Parsed file and info headers.
#[derive(Debug, Clone, Copy)]
struct Header {
    width: u32,
    height: u32,
    top_down: bool,
    bpp: u16,
    compression: u32,
    /// Offset of the pixel data from the start of the file.
    data_offset: usize,
    /// Offset and entry size (3 or 4 bytes) of the color table, and the number of entries.
    palette_offset: usize,
    palette_entry: usize,
    palette_len: usize,
    /// Red, green, blue and alpha masks (for 16/32-bit images).
    masks: [u32; 4],
}

fn parse_header(data: &[u8]) -> Result<Header, ImageError> {
    if !data.starts_with(b"BM") {
        return Err(ImageError::UnknownFormat);
    }
    let hsize = le32(data, 14)?;
    let (width, height, bpp, compression, colors_used, entry) = match hsize {
        12 => {
            let w = le16(data, 18)? as i64;
            let h = le16(data, 20)? as i64;
            (w, h, le16(data, 24)?, BI_RGB, 0, 3)
        }
        16..=64 | 108 | 124 => {
            let w = le32(data, 18)? as i32 as i64;
            let h = le32(data, 22)? as i32 as i64;
            let bpp = le16(data, 28)?;
            let compression = if hsize >= 20 { le32(data, 30)? } else { BI_RGB };
            let used = if hsize >= 36 { le32(data, 46)? } else { 0 };
            (w, h, bpp, compression, used, 4)
        }
        _ => return Err(ImageError::Unsupported("unknown BMP header size")),
    };
    if width <= 0 || height == 0 || height == i32::MIN as i64 {
        return Err(ImageError::Invalid("invalid BMP dimensions"));
    }
    let top_down = height < 0;
    let height = height.unsigned_abs();
    if width > u32::MAX as i64 || height > u32::MAX as u64 {
        return Err(ImageError::Invalid("invalid BMP dimensions"));
    }

    let mut masks = [0u32; 4];
    let explicit_masks = matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS);
    if hsize >= 52 || (hsize == 40 && explicit_masks) {
        masks[0] = le32(data, 54)?;
        masks[1] = le32(data, 58)?;
        masks[2] = le32(data, 62)?;
    }
    if hsize >= 56 || (hsize == 40 && compression == BI_ALPHABITFIELDS) {
        masks[3] = le32(data, 66)?;
    }
    let mut palette_offset = 14 + hsize as usize;
    if hsize == 40 {
        palette_offset += match compression {
            BI_BITFIELDS => 12,
            BI_ALPHABITFIELDS => 16,
            _ => 0,
        };
    }
    let palette_len = if bpp <= 8 {
        let max = 1usize << bpp;
        if colors_used == 0 { max } else { (colors_used as usize).min(max) }
    } else {
        0
    };
    let mut data_offset = le32(data, 10)? as usize;
    if data_offset < palette_offset || data_offset >= data.len() {
        // Some writers leave the offset at zero; the pixels then follow the color table.
        data_offset = palette_offset + palette_len * entry;
    }

    // Validate the format combination.
    match (compression, bpp) {
        (BI_RGB, 1 | 4 | 8 | 16 | 24 | 32) => {}
        (BI_RLE8, 8) | (BI_RLE4, 4) => {}
        (BI_BITFIELDS | BI_ALPHABITFIELDS, 16 | 32) => {}
        (BI_JPEG | BI_PNG, _) => {}
        (BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS, 64) => return Err(ImageError::Unsupported("64-bit BMP")),
        (BI_RLE8 | BI_RLE4 | BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS, _) => {
            return Err(ImageError::Invalid("invalid BMP bit depth for its compression"));
        }
        _ => return Err(ImageError::Unsupported("unknown BMP compression")),
    }
    if explicit_masks && hsize >= 108 && masks[..3] == [0, 0, 0] {
        return Err(ImageError::Invalid("BMP bit field masks are empty"));
    }
    if !explicit_masks {
        masks = match bpp {
            16 => [0x7C00, 0x03E0, 0x001F, 0],
            _ => [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0],
        };
    }
    Ok(Header {
        width: width as u32,
        height: height as u32,
        top_down,
        bpp,
        compression,
        data_offset,
        palette_offset,
        palette_entry: entry,
        palette_len,
        masks,
    })
}

impl Header {
    fn stride(&self) -> usize {
        (self.width as usize * self.bpp as usize).div_ceil(32) * 4
    }

    /// Pixel data, requiring all rows (the padding of the last row may be missing).
    fn pixel_data<'a>(&self, data: &'a [u8]) -> Result<&'a [u8], ImageError> {
        let stride = self.stride();
        let need = stride * (self.height as usize - 1) + (self.width as usize * self.bpp as usize).div_ceil(8);
        let pixels = data.get(self.data_offset..).ok_or(ImageError::Truncated)?;
        if pixels.len() < need {
            return Err(ImageError::Truncated);
        }
        Ok(pixels)
    }

    /// Image row (top = 0) of the `i`-th stored row.
    fn image_row(&self, i: usize) -> usize {
        if self.top_down { i } else { self.height as usize - 1 - i }
    }

    /// The color table as `0xFFRRGGBB` pixels, padded with opaque black to 256 entries.
    fn palette(&self, data: &[u8]) -> [u32; 256] {
        let mut lut = [0xFF00_0000u32; 256];
        if let Some(table) = data.get(self.palette_offset..) {
            for (entry, c) in lut.iter_mut().zip(table.chunks_exact(self.palette_entry)).take(self.palette_len) {
                *entry = 0xFF00_0000 | (c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32;
            }
        }
        lut
    }

    /// Whether 32-bit pixels carry an alpha channel worth looking at.
    fn alpha_source(&self) -> bool {
        self.bpp == 32 && (self.compression == BI_RGB || self.masks[3] != 0)
            || self.bpp == 16 && self.masks[3] != 0
    }
}

/// Extracts one channel from a pixel with a bit mask and scales it to 8 bits.
struct Channel {
    mask: u32,
    shift: u32,
    bits: u32,
    /// Scale table for channels narrower than 8 bits.
    lut: [u8; 256],
}

impl Channel {
    fn new(mask: u32) -> Channel {
        let shift = if mask == 0 { 0 } else { mask.trailing_zeros() };
        let bits = (mask >> shift).trailing_ones().min(32);
        let mut lut = [0u8; 256];
        if (1..8).contains(&bits) {
            let max = (1u32 << bits) - 1;
            for (v, e) in lut.iter_mut().enumerate().take(max as usize + 1) {
                *e = ((v as u32 * 255 + max / 2) / max) as u8;
            }
        }
        Channel { mask, shift, bits, lut }
    }

    #[inline(always)]
    fn get(&self, v: u32) -> u32 {
        let x = (v & self.mask) >> self.shift;
        if self.bits >= 8 {
            (x >> (self.bits - 8)) & 0xFF
        } else {
            self.lut[(x & 0xFF) as usize] as u32
        }
    }
}

/// Reads the BMP headers without decoding pixels.
pub fn read_info(data: &[u8]) -> Result<BmpInfo, ImageError> {
    let h = parse_header(data)?;
    let mut has_alpha = false;
    if h.alpha_source() && h.compression != BI_JPEG && h.compression != BI_PNG {
        if let Ok(pixels) = h.pixel_data(data) {
            let alpha = Channel::new(if h.compression == BI_RGB { 0xFF00_0000 } else { h.masks[3] });
            let (mut any_nonzero, mut any_partial) = (false, false);
            let bytes = h.bpp as usize / 8;
            for i in 0..h.height as usize {
                let row = &pixels[i * h.stride()..];
                for px in row.chunks_exact(bytes).take(h.width as usize) {
                    let v = if bytes == 4 { u32::from_le_bytes([px[0], px[1], px[2], px[3]]) } else { px[0] as u32 | (px[1] as u32) << 8 };
                    let a = alpha.get(v);
                    any_nonzero |= a != 0;
                    any_partial |= a != 255;
                }
            }
            has_alpha = any_nonzero && any_partial;
        }
    }
    Ok(BmpInfo {
        width: h.width,
        height: h.height,
        bits_per_pixel: h.bpp,
        compression: h.compression,
        top_down: h.top_down,
        has_alpha,
    })
}

/// Decodes a BMP image with the default [`DecodeOptions`].
pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    decode_with(data, &DecodeOptions::DEFAULT)
}

/// Decodes a BMP image.
pub fn decode_with(data: &[u8], options: &DecodeOptions) -> Result<Image, ImageError> {
    let h = parse_header(data)?;
    match h.compression {
        BI_JPEG | BI_PNG => {
            let inner = data.get(h.data_offset..).ok_or(ImageError::Truncated)?;
            return if h.compression == BI_JPEG {
                crate::jpeg::decode_with(inner, options)
            } else {
                crate::png::decode_with(inner, options)
            };
        }
        _ => {}
    }
    options.check_dimensions(h.width, h.height)?;
    let (w, ht) = (h.width as usize, h.height as usize);
    if h.compression == BI_RLE8 || h.compression == BI_RLE4 {
        let src = data.get(h.data_offset..).ok_or(ImageError::Truncated)?;
        // An RLE run codes at most 255 pixels per 2 bytes, but "end of bitmap" can skip any
        // number of rows; still require a minimum amount of data per row.
        if src.len() < ht / 128 {
            return Err(ImageError::Truncated);
        }
        let mut img = Image::try_new(h.width, h.height)?;
        decode_rle(&h, src, &h.palette(data), &mut img);
        return Ok(img);
    }

    let pixels = h.pixel_data(data)?;
    let stride = h.stride();
    let mut out = try_vec(w * ht, 0u32)?;
    match h.bpp {
        1 | 4 | 8 => {
            let lut = h.palette(data);
            let depth = h.bpp as usize;
            let per_byte = 8 / depth;
            let mask = ((1u16 << depth) - 1) as u8;
            for i in 0..ht {
                let row = &pixels[i * stride..];
                let dst = &mut out[h.image_row(i) * w..][..w];
                if depth == 8 {
                    for (d, &s) in dst.iter_mut().zip(row) {
                        *d = lut[s as usize];
                    }
                } else {
                    for (chunk, &byte) in dst.chunks_mut(per_byte).zip(row) {
                        let mut shift = 8 - depth as i32;
                        for d in chunk {
                            *d = lut[((byte >> shift) & mask) as usize];
                            shift -= depth as i32;
                        }
                    }
                }
            }
        }
        24 => {
            for i in 0..ht {
                let row = &pixels[i * stride..];
                let dst = &mut out[h.image_row(i) * w..][..w];
                for (d, s) in dst.iter_mut().zip(row.chunks_exact(3)) {
                    *d = 0xFF00_0000 | (s[2] as u32) << 16 | (s[1] as u32) << 8 | s[0] as u32;
                }
            }
        }
        32 if h.compression == BI_RGB => {
            // BGRA in memory is exactly 0xAARRGGBB little-endian.
            for i in 0..ht {
                let row = &pixels[i * stride..];
                let dst = &mut out[h.image_row(i) * w..][..w];
                for (d, s) in dst.iter_mut().zip(row.chunks_exact(4)) {
                    *d = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                }
            }
        }
        _ => {
            let [r, g, b, a] = h.masks.map(Channel::new);
            let has_alpha = h.masks[3] != 0;
            let bytes = h.bpp as usize / 8;
            for i in 0..ht {
                let row = &pixels[i * stride..];
                let dst = &mut out[h.image_row(i) * w..][..w];
                for (d, s) in dst.iter_mut().zip(row.chunks_exact(bytes)) {
                    let v = if bytes == 4 {
                        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
                    } else {
                        s[0] as u32 | (s[1] as u32) << 8
                    };
                    let alpha = if has_alpha { a.get(v) } else { 255 };
                    *d = alpha << 24 | r.get(v) << 16 | g.get(v) << 8 | b.get(v);
                }
            }
        }
    }
    if h.alpha_source() && out.iter().all(|&p| p >> 24 == 0) {
        // An all-zero alpha channel is padding, not transparency.
        for p in &mut out {
            *p |= 0xFF00_0000;
        }
    }
    Ok(Image { width: h.width, height: h.height, pixels: out })
}

/// Decodes RLE8/RLE4 data into `img` (which starts fully transparent). Stops quietly at the end
/// of the data, at an "end of bitmap" code, or after the last row.
fn decode_rle(h: &Header, src: &[u8], lut: &[u32; 256], img: &mut Image) {
    let (w, ht) = (h.width as usize, h.height as usize);
    let rle4 = h.compression == BI_RLE4;
    let mut put = |x: usize, row: usize, index: u8| {
        if x < w && row < ht {
            img.pixels[h.image_row(row) * w + x] = lut[index as usize];
        }
    };
    let (mut x, mut row, mut pos) = (0usize, 0usize, 0usize);
    while row < ht && pos + 1 < src.len() {
        let (n, b) = (src[pos] as usize, src[pos + 1]);
        pos += 2;
        if n > 0 {
            for i in 0..n {
                let index = if !rle4 {
                    b
                } else if i % 2 == 0 {
                    b >> 4
                } else {
                    b & 0x0F
                };
                put(x, row, index);
                x += 1;
            }
            continue;
        }
        match b {
            0 => {
                x = 0;
                row += 1;
            }
            1 => break,
            2 => {
                let (Some(&dx), Some(&dy)) = (src.get(pos), src.get(pos + 1)) else { break };
                pos += 2;
                x += dx as usize;
                row += dy as usize;
            }
            count => {
                let count = count as usize;
                let len = if rle4 { count.div_ceil(2) } else { count };
                let Some(run) = src.get(pos..pos + len) else { break };
                for i in 0..count {
                    let index = if !rle4 {
                        run[i]
                    } else if i % 2 == 0 {
                        run[i / 2] >> 4
                    } else {
                        run[i / 2] & 0x0F
                    };
                    put(x, row, index);
                    x += 1;
                }
                // Absolute runs are padded to a 16-bit boundary.
                pos += len + (len & 1);
            }
        }
    }
}

/// Encodes `image` as a 32-bit BGRA BMP (`BITMAPV4HEADER` with bit field masks, bottom-up rows).
pub fn encode(image: &Image) -> Result<Vec<u8>, ImageError> {
    let (w, h) = (image.width as usize, image.height as usize);
    if image.is_empty() {
        return Err(ImageError::InvalidArgument("cannot encode an empty image"));
    }
    if image.pixels.len() != w * h {
        return Err(ImageError::InvalidArgument("pixel buffer length does not match the dimensions"));
    }
    if image.width > i32::MAX as u32 || image.height > i32::MAX as u32 || w * h * 4 > u32::MAX as usize - 122 {
        return Err(ImageError::InvalidArgument("image is too large for BMP"));
    }
    let data_size = w * h * 4;
    let file_size = 14 + 108 + data_size;
    let mut out = Vec::new();
    out.try_reserve_exact(file_size)?;
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&122u32.to_le_bytes());
    // BITMAPV4HEADER
    out.extend_from_slice(&108u32.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
    out.extend_from_slice(&(data_size as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // 72 DPI
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&[0; 8]); // colors used / important
    for mask in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000] {
        out.extend_from_slice(&mask.to_le_bytes());
    }
    out.extend_from_slice(b"BGRs"); // LCS_sRGB, stored little-endian
    out.extend_from_slice(&[0; 36 + 12]); // endpoints and gamma (unused for sRGB)
    for row in image.pixels.chunks_exact(w).rev() {
        for &p in row {
            out.extend_from_slice(&p.to_le_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec;

    /// Builds a BMP with a `BITMAPINFOHEADER` (or core header) around raw pixel rows.
    pub(crate) fn build(
        hsize: u32,
        width: i32,
        height: i32,
        bpp: u16,
        compression: u32,
        palette: &[[u8; 4]],
        masks: &[u32],
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(&hsize.to_le_bytes());
        if hsize == 12 {
            info.extend_from_slice(&(width as u16).to_le_bytes());
            info.extend_from_slice(&(height as u16).to_le_bytes());
            info.extend_from_slice(&1u16.to_le_bytes());
            info.extend_from_slice(&bpp.to_le_bytes());
        } else {
            info.extend_from_slice(&width.to_le_bytes());
            info.extend_from_slice(&height.to_le_bytes());
            info.extend_from_slice(&1u16.to_le_bytes());
            info.extend_from_slice(&bpp.to_le_bytes());
            info.extend_from_slice(&compression.to_le_bytes());
            info.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
            info.extend_from_slice(&[0; 8]);
            info.extend_from_slice(&(palette.len() as u32).to_le_bytes());
            info.extend_from_slice(&[0; 4]);
            for m in masks {
                info.extend_from_slice(&m.to_le_bytes());
            }
            info.resize(hsize as usize, 0);
        }
        if hsize == 40 {
            for m in masks {
                info.extend_from_slice(&m.to_le_bytes());
            }
        }
        let mut pal = Vec::new();
        for p in palette {
            pal.extend_from_slice(if hsize == 12 { &p[..3] } else { &p[..] });
        }
        let offset = 14 + info.len() + pal.len();
        let mut out = b"BM".to_vec();
        out.extend_from_slice(&((offset + pixels.len()) as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        out.extend_from_slice(&info);
        out.extend_from_slice(&pal);
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn round_trip() {
        for &(w, h) in &[(1, 1), (3, 5), (17, 9)] {
            let img = Image::from_fn(w, h, |x, y| (x * 40) << 24 | (y * 30) << 16 | 0x1234);
            let file = encode(&img).unwrap();
            assert_eq!(decode(&file).unwrap(), img);
            assert!(read_info(&file).unwrap().has_alpha);
            let opaque = Image::from_fn(w, h, |x, y| 0xFF00_0000 | x << 8 | y);
            let file = encode(&opaque).unwrap();
            assert_eq!(decode(&file).unwrap(), opaque);
            assert!(!read_info(&file).unwrap().has_alpha);
        }
    }

    #[test]
    fn palettized_and_packed_formats() {
        let pal = [[0, 0, 255, 0], [0, 255, 0, 0], [255, 0, 0, 0], [10, 20, 30, 0]];
        // 1-bit, 3x2, bottom-up: stored rows are bottom first.
        let bits = build(40, 3, 2, 1, 0, &pal[..2], &[], &[0b1010_0000, 0, 0, 0, 0b0100_0000, 0, 0, 0]);
        let img = decode(&bits).unwrap();
        assert_eq!(img.pixels, vec![0xFF00FF00, 0xFFFF0000, 0xFF00FF00, 0xFFFF0000, 0xFF00FF00, 0xFFFF0000]);
        // 4-bit top-down, core header with 3-byte palette entries.
        let nib = build(12, 3, 1, 4, 0, &pal, &[], &[0x01, 0x30, 0, 0]);
        assert_eq!(decode(&nib).unwrap().pixels, vec![0xFFFF0000, 0xFF00FF00, 0xFF1E140A]);
        // 8-bit, top-down (negative height).
        let b8 = build(40, 2, -2, 8, 0, &pal, &[], &[0, 1, 0, 0, 2, 3, 0, 0]);
        assert_eq!(decode(&b8).unwrap().pixels, vec![0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFF1E140A]);
        // 24-bit with row padding.
        let b24 = build(40, 1, 2, 24, 0, &[], &[], &[1, 2, 3, 0, 4, 5, 6, 0]);
        assert_eq!(decode(&b24).unwrap().pixels, vec![0xFF060504, 0xFF030201]);
        // 16-bit 5:5:5 default and 5:6:5 bit fields.
        let b555 = build(40, 2, 1, 16, 0, &[], &[], &[0x00, 0x7C, 0x1F, 0x00]);
        assert_eq!(decode(&b555).unwrap().pixels, vec![0xFFFF0000, 0xFF0000FF]);
        let b565 = build(40, 2, 1, 16, 3, &[], &[0xF800, 0x07E0, 0x001F], &[0xE0, 0x07, 0x00, 0xF8]);
        assert_eq!(decode(&b565).unwrap().pixels, vec![0xFF00FF00, 0xFFFF0000]);
        // 32-bit without alpha (all zero alpha bytes) is opaque; V5 header with alpha mask.
        let b32 = build(40, 1, 1, 32, 0, &[], &[], &[1, 2, 3, 0]);
        assert_eq!(decode(&b32).unwrap().pixels, vec![0xFF030201]);
        let v5 = build(124, 2, 1, 32, 3, &[], &[0xFF0000, 0xFF00, 0xFF, 0xFF000000], &[1, 2, 3, 0x80, 4, 5, 6, 0]);
        assert_eq!(decode(&v5).unwrap().pixels, vec![0x80030201, 0x00060504]);
        assert!(read_info(&v5).unwrap().has_alpha);
    }

    #[test]
    fn rle() {
        let pal = [[0, 0, 0, 0], [255, 255, 255, 0], [0, 0, 255, 0], [0, 255, 0, 0]];
        // RLE8 4x3: row0 = run of 3 x index1 + abs [2]; EOL; row1 = delta (1,0) then index 3; EOL; row2 abs 4.
        let data = [3, 1, 0, 3, 2, 2, 2, 0, 0, 0, 0, 2, 1, 0, 1, 3, 0, 0, 0, 4, 1, 2, 3, 1, 0, 1];
        let file = build(40, 4, 3, 8, 1, &pal, &[], &data);
        let img = decode(&file).unwrap();
        let (w, b, r, g) = (0xFFFFFFFF, 0xFF000000, 0xFFFF0000, 0xFF00FF00);
        assert_eq!(&img.pixels[8..12], &[w, w, w, r]); // bottom row = first stored row
        assert_eq!(&img.pixels[4..8], &[0, g, 0, 0]);
        assert_eq!(&img.pixels[0..4], &[w, r, g, w]);
        let _ = b;
        // RLE4: 5 pixels alternating 1,2 then end of bitmap.
        let file = build(40, 5, 1, 4, 2, &pal, &[], &[5, 0x12, 0, 1]);
        assert_eq!(decode(&file).unwrap().pixels, vec![w, r, w, r, w]);
    }

    #[test]
    fn rejects_bad_input() {
        let img = Image::from_fn(4, 4, |x, y| 0xFF00_0000 | x << 8 | y);
        let file = encode(&img).unwrap();
        assert_eq!(decode(&file[..130]), Err(ImageError::Truncated));
        let mut bad = file.clone();
        bad[28] = 7; // 7 bits per pixel
        assert!(decode(&bad).is_err());
        let mut zero = file.clone();
        zero[18..22].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode(&zero).is_err());
    }
}
