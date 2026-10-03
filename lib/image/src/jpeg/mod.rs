//! JPEG decoding and encoding.
//!
//! # Decoder
//!
//! * Baseline and extended sequential (`SOF0`, `SOF1`) and progressive (`SOF2`) Huffman-coded
//!   images with 8-bit samples; spectral selection and successive approximation are supported.
//! * Grayscale, YCbCr, RGB (Adobe transform 0), and Adobe CMYK/YCCK images.
//! * Any integral sampling factors (4:4:4, 4:2:2, 4:2:0, 4:1:1, 4:4:0, ...), with "fancy"
//!   triangle-filter chroma upsampling identical to libjpeg's for the common 2x cases.
//! * Restart intervals (`DRI`/`RSTn`), multiple scans, missing `DHT` (standard tables are used,
//!   as for motion JPEG).
//! * EXIF orientation from `APP1`: exposed by [`read_info`] and applied by [`decode`] (see
//!   [`DecodeOptions::apply_orientation`]).
//! * libjpeg's accurate integer IDCT ("islow") and fixed-point YCbCr conversion, so output
//!   matches libjpeg's default (`JDCT_ISLOW`, fancy upsampling) closely.
//!
//! Lossless, hierarchical, arithmetic-coded and 12-bit files are rejected with
//! [`ImageError::Unsupported`]. Damaged entropy-coded data is decoded on a best-effort basis
//! (like libjpeg): a truncated file yields an image whose missing part is gray.
//!
//! # Encoder
//!
//! Baseline (or progressive) JPEG with libjpeg-compatible quality scaling (1..=100), 4:4:4,
//! 4:2:2 or 4:2:0 chroma subsampling or grayscale, standard or optimized Huffman tables and
//! optional restart markers. See [`EncodeOptions`].

mod color;
mod dct;
mod decoder;
mod encoder;
mod huffman;

#[cfg(test)]
pub(crate) use encoder::{encode_planes, encode_sampled, encode_separate_scans};

use alloc::vec::Vec;

use crate::DecodeOptions;
use crate::error::ImageError;
use crate::image::{Image, Orientation};

/// Natural (row-major) index of each zigzag position.
pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54,
    47, 55, 62, 63,
];

/// How the components of a JPEG file are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorModel {
    /// One component.
    Gray,
    /// Luma and chroma (JFIF).
    YCbCr,
    /// Three components without color transform (Adobe transform 0 or component IDs `R`,`G`,`B`).
    Rgb,
    /// Four components, Adobe (inverted) CMYK.
    Cmyk,
    /// Four components, Adobe YCCK.
    Ycck,
}

/// Header information of a JPEG file (see [`read_info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegInfo {
    /// Width as stored (before applying `orientation`).
    pub width: u32,
    /// Height as stored (before applying `orientation`).
    pub height: u32,
    /// Number of components (1, 3 or 4).
    pub components: u8,
    /// Progressive (`SOF2`) file.
    pub progressive: bool,
    /// EXIF orientation (`Normal` if absent).
    pub orientation: Orientation,
    /// Color interpretation.
    pub color_model: ColorModel,
}

/// Chroma subsampling used by the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsampling {
    /// Full-resolution chroma.
    Yuv444,
    /// Chroma halved horizontally.
    Yuv422,
    /// Chroma halved in both directions (the usual choice for photos).
    Yuv420,
}

/// Encoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Quality 1..=100 (libjpeg scale; 75 is libjpeg's default, 90 is visually near-lossless).
    pub quality: u8,
    /// Chroma subsampling (ignored for grayscale).
    pub subsampling: Subsampling,
    /// Build Huffman tables optimized for the image (smaller files, two passes).
    pub optimize_huffman: bool,
    /// Write a progressive JPEG (always with optimized tables).
    pub progressive: bool,
    /// Write a single-component (grayscale) JPEG from the luma.
    pub grayscale: bool,
    /// Insert a restart marker every this many MCUs (0 = none).
    pub restart_interval: u16,
}

impl Default for EncodeOptions {
    /// Quality 90, 4:2:0, standard tables, baseline, color, no restart markers.
    fn default() -> Self {
        EncodeOptions {
            quality: 90,
            subsampling: Subsampling::Yuv420,
            optimize_huffman: false,
            progressive: false,
            grayscale: false,
            restart_interval: 0,
        }
    }
}

/// Reads the JPEG headers (up to the first scan) without decoding pixels.
pub fn read_info(data: &[u8]) -> Result<JpegInfo, ImageError> {
    decoder::Decoder::new(data, &DecodeOptions::DEFAULT).read_headers()
}

/// Decodes a JPEG image with the default [`DecodeOptions`] (EXIF orientation applied).
pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    decode_with(data, &DecodeOptions::DEFAULT)
}

/// Decodes a JPEG image. With `options.apply_orientation` the result is rotated/flipped to the
/// upright orientation given by the EXIF tag.
pub fn decode_with(data: &[u8], options: &DecodeOptions) -> Result<Image, ImageError> {
    let mut d = decoder::Decoder::new(data, options);
    let img = d.decode()?;
    let orientation = d.orientation();
    if options.apply_orientation && orientation != Orientation::Normal {
        Ok(crate::ops::apply_orientation(&img, orientation))
    } else {
        Ok(img)
    }
}

/// Encodes `image` as JPEG (alpha is ignored).
pub fn encode(image: &Image, options: &EncodeOptions) -> Result<Vec<u8>, ImageError> {
    encoder::encode(image, options)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec;

    /// Peak signal-to-noise ratio over the RGB channels.
    pub(crate) fn psnr(a: &Image, b: &Image) -> f64 {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let mut se = 0u64;
        for (&p, &q) in a.pixels.iter().zip(&b.pixels) {
            for shift in [0, 8, 16] {
                let d = ((p >> shift) & 0xFF) as i64 - ((q >> shift) & 0xFF) as i64;
                se += (d * d) as u64;
            }
        }
        let mse = se as f64 / (a.pixels.len() * 3) as f64;
        if mse == 0.0 {
            return 99.0;
        }
        // 10 * log10(255^2 / mse) without libm: log10 via a simple series on the mantissa.
        10.0 * log10(255.0 * 255.0 / mse)
    }

    fn log10(x: f64) -> f64 {
        // ln(x) = 2 atanh((x-1)/(x+1)) after scaling into [1, 2).
        let (mut m, mut e) = (x, 0i32);
        while m >= 2.0 {
            m /= 2.0;
            e += 1;
        }
        while m < 1.0 {
            m *= 2.0;
            e -= 1;
        }
        let t = (m - 1.0) / (m + 1.0);
        let (mut term, mut sum, t2) = (t, 0.0, t * t);
        for k in 0..30 {
            sum += term / (2 * k + 1) as f64;
            term *= t2;
        }
        (2.0 * sum + e as f64 * core::f64::consts::LN_2) / core::f64::consts::LN_10
    }

    /// A smooth photo-like test image with some edges and texture.
    pub(crate) fn photo(w: u32, h: u32) -> Image {
        Image::from_fn(w, h, |x, y| {
            let fx = x as f64 / w as f64;
            let fy = y as f64 / h as f64;
            let r = (fx * 200.0 + 30.0) as u32;
            let g = ((1.0 - fy) * 180.0 + 40.0) as u32;
            let disc = (x as i64 - w as i64 / 2).pow(2) + (y as i64 - h as i64 / 3).pow(2);
            let b = if disc < (w as i64 * w as i64) / 25 { 230 } else { ((fx + fy) * 60.0) as u32 + 20 };
            let tex = (x * 7 + y * 13) % 11;
            0xFF00_0000 | (r + tex).min(255) << 16 | (g + tex / 2).min(255) << 8 | b.min(255)
        })
    }

    #[test]
    fn baseline_round_trips() {
        for &(w, h) in &[(1, 1), (3, 5), (17, 9), (64, 48), (123, 77)] {
            let img = photo(w, h);
            for subsampling in [Subsampling::Yuv444, Subsampling::Yuv422, Subsampling::Yuv420] {
                for optimize_huffman in [false, true] {
                    let opts = EncodeOptions { quality: 90, subsampling, optimize_huffman, ..Default::default() };
                    let file = encode(&img, &opts).unwrap();
                    let back = decode(&file).unwrap();
                    assert_eq!((back.width, back.height), (w, h));
                    let p = psnr(&img, &back);
                    // Hard chroma edges in tiny images suffer from subsampling; see `quality_90_psnr`.
                    let min = match (subsampling, w * h >= 64 * 48) {
                        (Subsampling::Yuv444, _) | (_, true) => 30.0,
                        _ => 20.0,
                    };
                    assert!(p > min, "{w}x{h} {subsampling:?} optimize {optimize_huffman}: PSNR {p:.1}");
                    let info = read_info(&file).unwrap();
                    assert_eq!((info.width, info.height, info.components), (w, h, 3));
                    assert!(!info.progressive);
                    assert_eq!(info.color_model, ColorModel::YCbCr);
                }
            }
        }
    }

    #[test]
    fn quality_90_psnr() {
        let img = photo(256, 192);
        for subsampling in [Subsampling::Yuv444, Subsampling::Yuv420] {
            let file = encode(&img, &EncodeOptions { quality: 90, subsampling, ..Default::default() }).unwrap();
            let p = psnr(&img, &decode(&file).unwrap());
            assert!(p > 30.0, "{subsampling:?}: {p:.2} dB");
        }
        // Quality orders file size and fidelity.
        let lo = encode(&img, &EncodeOptions { quality: 20, ..Default::default() }).unwrap();
        let hi = encode(&img, &EncodeOptions { quality: 98, ..Default::default() }).unwrap();
        assert!(lo.len() < hi.len());
        assert!(psnr(&img, &decode(&lo).unwrap()) < psnr(&img, &decode(&hi).unwrap()));
    }

    #[test]
    fn progressive_matches_baseline_exactly() {
        for &(w, h) in &[(1, 1), (9, 7), (40, 33), (129, 65)] {
            let img = photo(w, h);
            for subsampling in [Subsampling::Yuv444, Subsampling::Yuv420, Subsampling::Yuv422] {
                for restart_interval in [0, 3] {
                    for grayscale in [false, true] {
                        let base = EncodeOptions {
                            quality: 85,
                            subsampling,
                            grayscale,
                            restart_interval,
                            ..Default::default()
                        };
                        let prog = EncodeOptions { progressive: true, ..base };
                        let a = decode(&encode(&img, &base).unwrap()).unwrap();
                        let file = encode(&img, &prog).unwrap();
                        let info = read_info(&file).unwrap();
                        assert!(info.progressive);
                        let b = decode(&file).unwrap();
                        assert_eq!(a, b, "{w}x{h} {subsampling:?} restart {restart_interval} gray {grayscale}");
                    }
                }
            }
        }
    }

    #[test]
    fn non_interleaved_baseline_scans() {
        let img = photo(70, 45);
        for (hs, vs) in [(1, 1), (2, 2), (2, 1)] {
            for optimize_huffman in [false, true] {
                for restart_interval in [0, 4] {
                    let opts = EncodeOptions { optimize_huffman, restart_interval, ..Default::default() };
                    let interleaved = decode(&encoder::encode_sampled(&img, &opts, hs, vs).unwrap()).unwrap();
                    let file = encoder::encode_separate_scans(&img, &opts, hs, vs).unwrap();
                    assert_eq!(file.windows(2).filter(|w| w == &[0xFF, 0xDA]).count(), 3);
                    assert_eq!(decode(&file).unwrap(), interleaved, "{hs}x{vs} optimize {optimize_huffman}");
                }
            }
        }
        // Same for raw planes, and a file truncated after the first scan decodes as gray luma.
        let planes: [Vec<u8>; 3] = [16, 8, 0].map(|s| img.pixels.iter().map(|&p| (p >> s) as u8).collect());
        let opts = EncodeOptions::default();
        let a = encode_planes(&[&planes[0], &planes[1], &planes[2]], 70, 45, None, &opts, false).unwrap();
        let b = encode_planes(&[&planes[0], &planes[1], &planes[2]], 70, 45, None, &opts, true).unwrap();
        assert_eq!(decode(&a).unwrap(), decode(&b).unwrap());
        let second_scan = b.windows(2).enumerate().filter(|(_, w)| w == &[0xFF, 0xDA]).nth(1).unwrap().0;
        let partial = decode(&b[..second_scan]).unwrap();
        assert_eq!((partial.width, partial.height), (70, 45));
    }

    #[test]
    fn unusual_sampling_factors() {
        let img = photo(75, 41);
        for (hs, vs) in [(1, 2), (4, 1), (4, 2), (3, 1), (1, 3), (2, 4), (4, 4), (3, 2)] {
            let file =
                encoder::encode_sampled(&img, &EncodeOptions { quality: 95, ..Default::default() }, hs, vs).unwrap();
            let back = decode(&file).unwrap();
            assert_eq!((back.width, back.height), (75, 41));
            let p = psnr(&img, &back);
            assert!(p > 22.0, "{hs}x{vs}: {p:.1} dB");
            // Progressive coding of the same coefficients decodes identically.
            let prog = EncodeOptions { quality: 95, progressive: true, ..Default::default() };
            let pfile = encoder::encode_sampled(&img, &prog, hs, vs).unwrap();
            assert_eq!(decode(&pfile).unwrap(), back, "{hs}x{vs} progressive");
        }
    }

    #[test]
    fn grayscale_and_restarts() {
        let img = photo(70, 50);
        let gray = encode(&img, &EncodeOptions { grayscale: true, ..Default::default() }).unwrap();
        let info = read_info(&gray).unwrap();
        assert_eq!(info.components, 1);
        assert_eq!(info.color_model, ColorModel::Gray);
        let g = decode(&gray).unwrap();
        assert!(g.pixels.iter().all(|&p| {
            let (r, gg, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            r == gg && gg == b && p >> 24 == 0xFF
        }));
        let plain = decode(&encode(&img, &EncodeOptions::default()).unwrap()).unwrap();
        for ri in [1, 2, 7, 100] {
            let file = encode(&img, &EncodeOptions { restart_interval: ri, ..Default::default() }).unwrap();
            assert_eq!(decode(&file).unwrap(), plain, "restart interval {ri}");
        }
    }

    /// Inserts an EXIF APP1 segment with the given orientation after SOI.
    pub(crate) fn with_orientation(file: &[u8], orientation: u16) -> Vec<u8> {
        let mut app1 = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
        app1.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1]);
        app1.extend_from_slice(&orientation.to_be_bytes());
        app1.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut out = file[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&file[2..]);
        out
    }

    #[test]
    fn exif_orientation_is_applied() {
        let img = photo(40, 24);
        let file = encode(&img, &EncodeOptions::default()).unwrap();
        let plain = decode(&file).unwrap();
        for v in 1..=8u16 {
            let o = Orientation::from_exif(v).unwrap();
            let tagged = with_orientation(&file, v);
            let info = read_info(&tagged).unwrap();
            assert_eq!(info.orientation, o);
            let generic = crate::read_info(&tagged).unwrap();
            assert_eq!((generic.width, generic.height), if o.swaps_dimensions() { (24, 40) } else { (40, 24) });
            let decoded = decode(&tagged).unwrap();
            assert_eq!(decoded, crate::ops::apply_orientation(&plain, o));
            let raw =
                decode_with(&tagged, &DecodeOptions { apply_orientation: false, ..DecodeOptions::DEFAULT }).unwrap();
            assert_eq!(raw, plain);
        }
    }

    #[test]
    fn handles_damage() {
        let img = photo(64, 64);
        let file = encode(&img, &EncodeOptions::default()).unwrap();
        // Truncated entropy data still yields an image (bottom part gray).
        let cut = decode(&file[..file.len() / 2]).unwrap();
        assert_eq!((cut.width, cut.height), (64, 64));
        // Headers only: no image.
        let sos = file.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        assert!(decode(&file[..sos]).is_err());
        assert!(read_info(&file[..sos]).is_ok());
        // Unsupported process.
        let mut sof3 = file.clone();
        let sof = sof3.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        sof3[sof + 1] = 0xC3;
        assert!(matches!(decode(&sof3), Err(ImageError::Unsupported(_))));
        // Too large for the limits.
        let small = DecodeOptions { max_pixels: 1000, ..DecodeOptions::DEFAULT };
        assert!(matches!(decode_with(&file, &small), Err(ImageError::TooLarge { .. })));
        assert!(decode(&[0xFF, 0xD8, 0xFF]).is_err());
        assert!(decode(&[]).is_err());
        let _ = vec![0u8; 0];
    }
}

#[cfg(test)]
mod adobe_tests {
    use super::tests::{photo, psnr};
    use super::*;

    /// The encoder's RGB to YCbCr conversion (libjpeg's).
    fn to_ycc(r: i32, g: i32, b: i32) -> [u8; 3] {
        [
            ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8,
            ((-11059 * r - 21709 * g + 32768 * b + (128 << 16) + 32767) >> 16) as u8,
            ((32768 * r - 27439 * g - 5329 * b + (128 << 16) + 32767) >> 16) as u8,
        ]
    }

    fn channels(img: &Image) -> [Vec<u8>; 3] {
        [16, 8, 0].map(|s| img.pixels.iter().map(|&p| (p >> s) as u8).collect())
    }

    #[test]
    fn adobe_rgb_cmyk_and_ycck() {
        let img = photo(61, 37);
        let (w, h) = (61, 37);
        let opts = EncodeOptions { quality: 95, ..Default::default() };
        let [r, g, b] = channels(&img);

        // Adobe RGB (transform 0): components are R, G, B directly.
        let file = encode_planes(&[&r, &g, &b], w, h, Some(0), &opts, false).unwrap();
        assert_eq!(read_info(&file).unwrap().color_model, ColorModel::Rgb);
        let p = psnr(&img, &decode(&file).unwrap());
        assert!(p > 35.0, "RGB: {p:.1} dB");

        // Adobe CMYK (inverted): with K = 255 (no black ink) the stored C, M, Y equal R, G, B.
        // Halve the brightness in the right half through K.
        let k: Vec<u8> = (0..w * h).map(|i| if i % w < w / 2 { 255 } else { 128 }).collect();
        let expected = Image::from_fn(w as u32, h as u32, |x, y| {
            let p = img.get(x, y).unwrap();
            let kk = if (x as usize) < w / 2 { 255 } else { 128 };
            let m = |c: u32| (c * kk + 127) / 255;
            0xFF00_0000 | m((p >> 16) & 0xFF) << 16 | m((p >> 8) & 0xFF) << 8 | m(p & 0xFF)
        });
        let file = encode_planes(&[&r, &g, &b, &k], w, h, Some(0), &opts, false).unwrap();
        let info = read_info(&file).unwrap();
        assert_eq!((info.components, info.color_model), (4, ColorModel::Cmyk));
        let cmyk = decode(&file).unwrap();
        let p = psnr(&expected, &cmyk);
        assert!(p > 33.0, "CMYK: {p:.1} dB");
        // Progressive coding of four components decodes identically.
        let prog = encode_planes(&[&r, &g, &b, &k], w, h, Some(0), &EncodeOptions { progressive: true, ..opts }, false)
            .unwrap();
        assert_eq!(decode(&prog).unwrap(), cmyk);

        // YCCK (transform 2): YCbCr of the inverted stored CMY values, plus K.
        let mut ycc: [Vec<u8>; 3] = Default::default();
        for i in 0..w * h {
            let t = to_ycc(255 - r[i] as i32, 255 - g[i] as i32, 255 - b[i] as i32);
            for c in 0..3 {
                ycc[c].push(t[c]);
            }
        }
        let file = encode_planes(&[&ycc[0], &ycc[1], &ycc[2], &k], w, h, Some(2), &opts, false).unwrap();
        assert_eq!(read_info(&file).unwrap().color_model, ColorModel::Ycck);
        let p = psnr(&expected, &decode(&file).unwrap());
        assert!(p > 33.0, "YCCK: {p:.1} dB");

        // Without an Adobe marker (this helper then writes JFIF) three components are YCbCr.
        let file = encode_planes(&[&r, &g, &b], w, h, None, &opts, false).unwrap();
        assert_eq!(read_info(&file).unwrap().color_model, ColorModel::YCbCr);
    }
}
