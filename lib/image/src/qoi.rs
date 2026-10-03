//! QOI ("Quite OK Image") decoding and encoding, following the specification at
//! <https://qoiformat.org/qoi-specification.pdf>.
//!
//! QOI is a simple lossless format: each pixel is coded relative to the previous one (run,
//! small difference, luma difference, index into a 64-entry hash of recent colors, or a literal).
//! Both directions run in a single pass over the data.

use alloc::vec::Vec;

use crate::DecodeOptions;
use crate::error::ImageError;
use crate::image::Image;
use crate::util::{be32, try_vec};

const MAGIC: &[u8; 4] = b"qoif";
const HEADER_LEN: usize = 14;
const END_MARKER: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

const OP_INDEX: u8 = 0x00;
const OP_DIFF: u8 = 0x40;
const OP_LUMA: u8 = 0x80;
const OP_RUN: u8 = 0xC0;
const OP_RGB: u8 = 0xFE;
const OP_RGBA: u8 = 0xFF;
const MASK_2: u8 = 0xC0;

/// Header information of a QOI file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QoiInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// 3 (RGB) or 4 (RGBA), as declared by the header.
    pub channels: u8,
    /// 0 = sRGB with linear alpha, 1 = all channels linear.
    pub colorspace: u8,
}

/// Reads the QOI header.
pub fn read_info(data: &[u8]) -> Result<QoiInfo, ImageError> {
    if !data.starts_with(MAGIC) {
        return Err(ImageError::UnknownFormat);
    }
    let width = be32(data, 4)?;
    let height = be32(data, 8)?;
    let channels = *data.get(12).ok_or(ImageError::Truncated)?;
    let colorspace = *data.get(13).ok_or(ImageError::Truncated)?;
    if channels != 3 && channels != 4 {
        return Err(ImageError::Invalid("QOI channel count must be 3 or 4"));
    }
    if colorspace > 1 {
        return Err(ImageError::Invalid("unknown QOI colorspace"));
    }
    Ok(QoiInfo { width, height, channels, colorspace })
}

#[inline(always)]
fn hash(px: [u8; 4]) -> usize {
    let [r, g, b, a] = px;
    (r as usize * 3 + g as usize * 5 + b as usize * 7 + a as usize * 11) % 64
}

/// Decodes a QOI image with the default [`DecodeOptions`].
pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    decode_with(data, &DecodeOptions::DEFAULT)
}

/// Decodes a QOI image.
pub fn decode_with(data: &[u8], options: &DecodeOptions) -> Result<Image, ImageError> {
    let info = read_info(data)?;
    options.check_dimensions(info.width, info.height)?;
    let n = info.width as usize * info.height as usize;
    // Every byte codes at most 62 pixels (a run); refuse sizes the data cannot possibly hold.
    let body = &data[HEADER_LEN..];
    if n / 62 > body.len() {
        return Err(ImageError::Truncated);
    }
    let mut pixels = try_vec(n, 0u32)?;
    let mut index = [[0u8; 4]; 64];
    let mut px = [0u8, 0, 0, 255];
    let mut pos = 0;
    let mut i = 0;
    while i < n {
        let b1 = *body.get(pos).ok_or(ImageError::Truncated)?;
        pos += 1;
        let mut run = 1;
        match b1 {
            OP_RGB => {
                let s = body.get(pos..pos + 3).ok_or(ImageError::Truncated)?;
                px[..3].copy_from_slice(s);
                pos += 3;
            }
            OP_RGBA => {
                let s = body.get(pos..pos + 4).ok_or(ImageError::Truncated)?;
                px.copy_from_slice(s);
                pos += 4;
            }
            _ => match b1 & MASK_2 {
                OP_INDEX => px = index[b1 as usize],
                OP_DIFF => {
                    px[0] = px[0].wrapping_add((b1 >> 4) & 3).wrapping_sub(2);
                    px[1] = px[1].wrapping_add((b1 >> 2) & 3).wrapping_sub(2);
                    px[2] = px[2].wrapping_add(b1 & 3).wrapping_sub(2);
                }
                OP_LUMA => {
                    let b2 = *body.get(pos).ok_or(ImageError::Truncated)?;
                    pos += 1;
                    let vg = (b1 & 0x3F).wrapping_sub(32);
                    px[0] = px[0].wrapping_add(vg.wrapping_sub(8).wrapping_add(b2 >> 4));
                    px[1] = px[1].wrapping_add(vg);
                    px[2] = px[2].wrapping_add(vg.wrapping_sub(8).wrapping_add(b2 & 0x0F));
                }
                _ => run = (b1 & 0x3F) as usize + 1,
            },
        }
        index[hash(px)] = px;
        let [r, g, b, a] = px;
        let value = (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
        let end = (i + run).min(n);
        pixels[i..end].fill(value);
        i = end;
    }
    Ok(Image { width: info.width, height: info.height, pixels })
}

/// Encodes `image` as QOI. The header declares 4 channels if any pixel is not opaque, otherwise
/// 3; the colorspace is sRGB.
pub fn encode(image: &Image) -> Result<Vec<u8>, ImageError> {
    if image.is_empty() {
        return Err(ImageError::InvalidArgument("cannot encode an empty image"));
    }
    if image.pixels.len() as u64 != image.width as u64 * image.height as u64 {
        return Err(ImageError::InvalidArgument("pixel buffer length does not match the dimensions"));
    }
    let channels = if image.has_alpha() { 4 } else { 3 };
    let mut out = Vec::new();
    out.try_reserve(HEADER_LEN + image.pixels.len() * 2 + END_MARKER.len())?;
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&image.width.to_be_bytes());
    out.extend_from_slice(&image.height.to_be_bytes());
    out.extend_from_slice(&[channels, 0]);

    let mut index = [[0u8; 4]; 64];
    let mut prev = [0u8, 0, 0, 255];
    let mut run = 0u8;
    let last = image.pixels.len() - 1;
    for (i, &p) in image.pixels.iter().enumerate() {
        let px = [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8];
        if px == prev {
            run += 1;
            if run == 62 || i == last {
                out.push(OP_RUN | (run - 1));
                run = 0;
            }
            continue;
        }
        if run > 0 {
            out.push(OP_RUN | (run - 1));
            run = 0;
        }
        let h = hash(px);
        if index[h] == px {
            out.push(OP_INDEX | h as u8);
        } else {
            index[h] = px;
            if px[3] == prev[3] {
                let vr = px[0].wrapping_sub(prev[0]) as i8;
                let vg = px[1].wrapping_sub(prev[1]) as i8;
                let vb = px[2].wrapping_sub(prev[2]) as i8;
                let vg_r = vr.wrapping_sub(vg);
                let vg_b = vb.wrapping_sub(vg);
                if (-2..=1).contains(&vr) && (-2..=1).contains(&vg) && (-2..=1).contains(&vb) {
                    out.push(OP_DIFF | ((vr + 2) as u8) << 4 | ((vg + 2) as u8) << 2 | (vb + 2) as u8);
                } else if (-8..=7).contains(&vg_r) && (-32..=31).contains(&vg) && (-8..=7).contains(&vg_b) {
                    out.push(OP_LUMA | (vg + 32) as u8);
                    out.push(((vg_r + 8) as u8) << 4 | (vg_b + 8) as u8);
                } else {
                    out.extend_from_slice(&[OP_RGB, px[0], px[1], px[2]]);
                }
            } else {
                out.extend_from_slice(&[OP_RGBA, px[0], px[1], px[2], px[3]]);
            }
        }
        prev = px;
    }
    out.extend_from_slice(&END_MARKER);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut seed = 7u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for &(w, h) in &[(1, 1), (3, 5), (17, 9), (100, 64)] {
            let imgs = [
                Image::from_fn(w, h, |x, y| 0xFF00_0000 | (x * 3) << 16 | (y * 5) << 8 | ((x + y) & 0xFF)),
                Image::from_fn(w, h, |_, _| rnd()),
                Image::from_fn(w, h, |x, _| if x % 5 < 3 { 0x8012_3456 } else { 0xFF65_4321 }),
                Image::filled(w, h, 0xFF00_0000),
            ];
            for img in &imgs {
                let file = encode(img).unwrap();
                assert_eq!(&decode(&file).unwrap(), img);
                assert_eq!(read_info(&file).unwrap().channels, if img.has_alpha() { 4 } else { 3 });
            }
        }
        // A long run crosses the 62-pixel run limit several times.
        let flat = Image::filled(1000, 3, 0xFF10_2030);
        let file = encode(&flat).unwrap();
        assert!(file.len() < 100);
        assert_eq!(decode(&file).unwrap(), flat);
    }

    #[test]
    fn rejects_bad_input() {
        let img = Image::from_fn(8, 8, |x, y| 0xFF00_0000 | (x * 30) << 8 | (y * 30));
        let file = encode(&img).unwrap();
        assert_eq!(decode(&file[..20]), Err(ImageError::Truncated));
        let mut bad = file.clone();
        bad[12] = 5;
        assert!(decode(&bad).is_err());
        let mut huge = file.clone();
        huge[4..8].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(matches!(decode(&huge), Err(ImageError::TooLarge { .. })));
    }
}
