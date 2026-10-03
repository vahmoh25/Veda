//! Image codecs and resampling for Vindows.
//!
//! `vimage` decodes and encodes the image formats used by the photo viewer, wallpapers, icons and
//! screenshots, and provides the image operations those applications need.
//!
//! | Format | Decoding | Encoding |
//! |--------|----------|----------|
//! | PNG  ([`png`])  | every color type and bit depth, Adam7, `tRNS` | RGB8 / RGBA8, adaptive filters |
//! | JPEG ([`jpeg`]) | baseline + progressive Huffman, any sampling, restarts, EXIF orientation | baseline or progressive, 4:4:4 / 4:2:2 / 4:2:0 |
//! | BMP  ([`bmp`])  | 1/4/8/16/24/32 bit, bit fields, RLE4/RLE8, core/V4/V5 headers | 32-bit BGRA |
//! | QOI  ([`qoi`])  | yes | yes |
//!
//! The [`inflate`] and [`deflate`] modules contain the zlib implementation used by PNG; they are
//! public so other components can use them. [`checksum`] provides CRC-32 and Adler-32.
//!
//! # Pixels
//!
//! Every decoder produces an [`Image`]: `width * height` pixels in row-major order without
//! padding, each a `u32` in `0xAARRGGBB` order with straight (non-premultiplied) alpha. That is
//! the byte order `B, G, R, A` in memory, which matches the Vindows framebuffer.
//!
//! # Entry points
//!
//! * [`detect`] identifies a format from its magic bytes.
//! * [`decode`] / [`decode_with`] decode any supported format.
//! * [`read_info`] returns dimensions and alpha information without decoding pixels.
//! * [`encode`] writes an image with default settings; the format modules offer more control.
//! * [`ops`] (re-exported here) resizes, crops, flips and rotates images.
//!
//! # Robustness
//!
//! Decoders never panic, never loop forever and never trust sizes from the file: dimensions are
//! limited by [`DecodeOptions`] (16384 x 16384 and 64 Mi pixels by default), allocations are
//! fallible ([`ImageError::OutOfMemory`]) and are only made once the input could plausibly fill
//! them. Malformed input is reported as an [`ImageError`].

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bmp;
pub mod checksum;
pub mod deflate;
mod error;
mod image;
pub mod inflate;
pub mod jpeg;
mod mathf;
pub mod ops;
pub mod png;
pub mod qoi;
mod util;

#[cfg(test)]
mod tests;

use alloc::vec::Vec;

pub use error::ImageError;
pub use image::{Image, Orientation};
pub use ops::{
    Filter, apply_orientation, crop, flip_horizontal, flip_vertical, premultiply, premultiply_pixel, resize, rotate90,
    rotate180, rotate270, thumbnail, unpremultiply, unpremultiply_pixel,
};

/// Default maximum width or height accepted by the decoders.
pub const MAX_DIMENSION: u32 = 16384;
/// Default maximum number of pixels (`width * height`) accepted by the decoders.
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// An image file format supported by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// Portable Network Graphics.
    Png,
    /// JPEG (JFIF / EXIF).
    Jpeg,
    /// Windows bitmap.
    Bmp,
    /// Quite OK Image format.
    Qoi,
}

impl Format {
    /// A short human-readable name, e.g. `"PNG"`.
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Bmp => "BMP",
            Format::Qoi => "QOI",
        }
    }

    /// The usual file extension without the dot, e.g. `"png"`.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Bmp => "bmp",
            Format::Qoi => "qoi",
        }
    }

    /// The MIME type, e.g. `"image/png"`.
    pub fn mime_type(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Bmp => "image/bmp",
            Format::Qoi => "image/qoi",
        }
    }
}

/// Basic facts about an image file, obtained without decoding its pixels (see [`read_info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    /// The file format.
    pub format: Format,
    /// Width of the image [`decode`] returns, i.e. after applying [`ImageInfo::orientation`].
    pub width: u32,
    /// Height of the image [`decode`] returns, i.e. after applying [`ImageInfo::orientation`].
    pub height: u32,
    /// Whether the image may contain non-opaque pixels (alpha channel, PNG `tRNS`, BMP alpha mask).
    pub has_alpha: bool,
    /// EXIF orientation stored in the file ([`Orientation::Normal`] for formats without EXIF).
    pub orientation: Orientation,
    /// The file is a progressive JPEG or an Adam7-interlaced PNG.
    pub progressive: bool,
}

/// Limits and switches for decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Maximum accepted width and height.
    pub max_dimension: u32,
    /// Maximum accepted `width * height`.
    pub max_pixels: u64,
    /// Verify PNG chunk CRCs and the zlib Adler-32 checksum. Disabling saves a little time.
    pub verify_checksums: bool,
    /// Rotate/flip JPEG images according to their EXIF orientation tag.
    pub apply_orientation: bool,
}

impl DecodeOptions {
    /// The defaults: [`MAX_DIMENSION`], [`MAX_PIXELS`], checksums verified, orientation applied.
    pub const DEFAULT: DecodeOptions =
        DecodeOptions { max_dimension: MAX_DIMENSION, max_pixels: MAX_PIXELS, verify_checksums: true, apply_orientation: true };

    /// Checks `width x height` against the limits.
    pub fn check_dimensions(&self, width: u32, height: u32) -> Result<(), ImageError> {
        if width == 0 || height == 0 {
            return Err(ImageError::Invalid("image has a zero dimension"));
        }
        if width > self.max_dimension || height > self.max_dimension || width as u64 * height as u64 > self.max_pixels {
            return Err(ImageError::TooLarge { width, height });
        }
        Ok(())
    }
}

impl Default for DecodeOptions {
    fn default() -> Self {
        DecodeOptions::DEFAULT
    }
}

/// Identifies the format of `data` from its magic bytes.
pub fn detect(data: &[u8]) -> Option<Format> {
    if data.starts_with(&png::SIGNATURE) {
        Some(Format::Png)
    } else if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        Some(Format::Jpeg)
    } else if data.starts_with(b"qoif") {
        Some(Format::Qoi)
    } else if bmp::is_bmp(data) {
        Some(Format::Bmp)
    } else {
        None
    }
}

/// Decodes an image in any supported format with the default [`DecodeOptions`].
pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    decode_with(data, &DecodeOptions::DEFAULT)
}

/// Decodes an image in any supported format.
pub fn decode_with(data: &[u8], options: &DecodeOptions) -> Result<Image, ImageError> {
    match detect(data) {
        Some(Format::Png) => png::decode_with(data, options),
        Some(Format::Jpeg) => jpeg::decode_with(data, options),
        Some(Format::Bmp) => bmp::decode_with(data, options),
        Some(Format::Qoi) => qoi::decode_with(data, options),
        None => Err(ImageError::UnknownFormat),
    }
}

/// Reads the format, dimensions and alpha information of an image without decoding its pixels.
///
/// This only parses headers (for BMP files with 32-bit pixels and no explicit alpha mask it also
/// scans the alpha bytes), so it is cheap enough for file listings.
pub fn read_info(data: &[u8]) -> Result<ImageInfo, ImageError> {
    match detect(data) {
        Some(Format::Png) => {
            let i = png::read_info(data)?;
            Ok(ImageInfo {
                format: Format::Png,
                width: i.width,
                height: i.height,
                has_alpha: i.has_alpha,
                orientation: Orientation::Normal,
                progressive: i.interlaced,
            })
        }
        Some(Format::Jpeg) => {
            let i = jpeg::read_info(data)?;
            let (width, height) =
                if i.orientation.swaps_dimensions() { (i.height, i.width) } else { (i.width, i.height) };
            Ok(ImageInfo {
                format: Format::Jpeg,
                width,
                height,
                has_alpha: false,
                orientation: i.orientation,
                progressive: i.progressive,
            })
        }
        Some(Format::Bmp) => {
            let i = bmp::read_info(data)?;
            Ok(ImageInfo {
                format: Format::Bmp,
                width: i.width,
                height: i.height,
                has_alpha: i.has_alpha,
                orientation: Orientation::Normal,
                progressive: false,
            })
        }
        Some(Format::Qoi) => {
            let i = qoi::read_info(data)?;
            Ok(ImageInfo {
                format: Format::Qoi,
                width: i.width,
                height: i.height,
                has_alpha: i.channels == 4,
                orientation: Orientation::Normal,
                progressive: false,
            })
        }
        None => Err(ImageError::UnknownFormat),
    }
}

/// Encodes `image` with default settings: PNG at compression level 6, JPEG at quality 90 with
/// 4:2:0 chroma subsampling (alpha is ignored), 32-bit BMP, or QOI.
pub fn encode(image: &Image, format: Format) -> Result<Vec<u8>, ImageError> {
    match format {
        Format::Png => png::encode(image, 6),
        Format::Jpeg => jpeg::encode(image, &jpeg::EncodeOptions::default()),
        Format::Bmp => bmp::encode(image),
        Format::Qoi => qoi::encode(image),
    }
}
