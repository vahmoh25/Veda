//! The decoded image type and the EXIF orientation enum.

use alloc::vec::Vec;
use core::fmt;

use crate::error::ImageError;
use crate::util::try_vec;

/// A decoded image.
///
/// `pixels` holds `width * height` pixels in row-major order without padding. Each pixel is a
/// `u32` in `0xAARRGGBB` order with straight (non-premultiplied) alpha.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height` pixels, `0xAARRGGBB`, row-major.
    pub pixels: Vec<u32>,
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

impl Image {
    /// Creates a `width x height` image filled with transparent black.
    ///
    /// # Panics
    ///
    /// Panics if the pixel count overflows `usize` or the allocation fails; use [`Image::try_new`]
    /// for sizes that come from untrusted input.
    pub fn new(width: u32, height: u32) -> Image {
        Image::filled(width, height, 0)
    }

    /// Creates a `width x height` image filled with transparent black, reporting allocation
    /// failure as [`ImageError::OutOfMemory`] instead of aborting.
    pub fn try_new(width: u32, height: u32) -> Result<Image, ImageError> {
        let n = (width as usize).checked_mul(height as usize).ok_or(ImageError::OutOfMemory)?;
        Ok(Image { width, height, pixels: try_vec(n, 0)? })
    }

    /// Creates a `width x height` image filled with `color` (`0xAARRGGBB`).
    ///
    /// # Panics
    ///
    /// Panics if the pixel count overflows `usize` or the allocation fails.
    pub fn filled(width: u32, height: u32, color: u32) -> Image {
        let n = (width as usize).checked_mul(height as usize).expect("image size overflows usize");
        Image { width, height, pixels: alloc::vec![color; n] }
    }

    /// Creates an image by calling `f(x, y)` for every pixel, row by row.
    ///
    /// # Panics
    ///
    /// Panics if the pixel count overflows `usize` or the allocation fails.
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> u32) -> Image {
        let n = (width as usize).checked_mul(height as usize).expect("image size overflows usize");
        let mut pixels = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                pixels.push(f(x, y));
            }
        }
        Image { width, height, pixels }
    }

    /// Wraps an existing pixel buffer; fails unless `pixels.len() == width * height`.
    pub fn from_pixels(width: u32, height: u32, pixels: Vec<u32>) -> Result<Image, ImageError> {
        if (width as u64) * (height as u64) != pixels.len() as u64 {
            return Err(ImageError::InvalidArgument("pixel buffer length does not match the dimensions"));
        }
        Ok(Image { width, height, pixels })
    }

    /// Builds an image from tightly packed 8-bit RGBA bytes (`R, G, B, A` per pixel).
    pub fn from_rgba8(width: u32, height: u32, rgba: &[u8]) -> Result<Image, ImageError> {
        if (width as u64) * (height as u64) * 4 != rgba.len() as u64 {
            return Err(ImageError::InvalidArgument("RGBA buffer length does not match the dimensions"));
        }
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(rgba.len() / 4)?;
        pixels.extend(rgba.chunks_exact(4).map(|p| u32::from_be_bytes([p[3], p[0], p[1], p[2]])));
        Ok(Image { width, height, pixels })
    }

    /// Returns the pixels as tightly packed 8-bit RGBA bytes (`R, G, B, A` per pixel).
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for &p in &self.pixels {
            let [a, r, g, b] = p.to_be_bytes();
            out.extend_from_slice(&[r, g, b, a]);
        }
        out
    }

    /// Returns `true` if the image has no pixels.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Returns the pixel at `(x, y)`, or `None` if it is outside the image.
    #[inline]
    pub fn get(&self, x: u32, y: u32) -> Option<u32> {
        if x < self.width && y < self.height {
            self.pixels.get(y as usize * self.width as usize + x as usize).copied()
        } else {
            None
        }
    }

    /// Sets the pixel at `(x, y)` to `color`. Returns `false` (and does nothing) if the position is
    /// outside the image.
    #[inline]
    pub fn set(&mut self, x: u32, y: u32, color: u32) -> bool {
        if x < self.width && y < self.height {
            if let Some(p) = self.pixels.get_mut(y as usize * self.width as usize + x as usize) {
                *p = color;
                return true;
            }
        }
        false
    }

    /// Returns row `y`.
    ///
    /// # Panics
    ///
    /// Panics if `y >= height`.
    #[inline]
    pub fn row(&self, y: u32) -> &[u32] {
        let w = self.width as usize;
        &self.pixels[y as usize * w..y as usize * w + w]
    }

    /// Returns row `y` mutably.
    ///
    /// # Panics
    ///
    /// Panics if `y >= height`.
    #[inline]
    pub fn row_mut(&mut self, y: u32) -> &mut [u32] {
        let w = self.width as usize;
        &mut self.pixels[y as usize * w..y as usize * w + w]
    }

    /// Returns `true` if any pixel is not fully opaque.
    pub fn has_alpha(&self) -> bool {
        self.pixels.iter().any(|&p| p < 0xFF00_0000)
    }
}

/// EXIF orientation: how the stored pixels must be transformed to display the image upright.
///
/// The discriminants are the EXIF tag values (1..=8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Orientation {
    /// Stored upright (EXIF 1).
    #[default]
    Normal = 1,
    /// Mirror left/right (EXIF 2).
    FlipHorizontal = 2,
    /// Rotate by 180 degrees (EXIF 3).
    Rotate180 = 3,
    /// Mirror top/bottom (EXIF 4).
    FlipVertical = 4,
    /// Mirror across the main diagonal (EXIF 5): output `(x, y)` is input `(y, x)`.
    Transpose = 5,
    /// Rotate by 90 degrees clockwise (EXIF 6).
    Rotate90 = 6,
    /// Mirror across the anti-diagonal (EXIF 7).
    Transverse = 7,
    /// Rotate by 270 degrees clockwise, i.e. 90 degrees counter-clockwise (EXIF 8).
    Rotate270 = 8,
}

impl Orientation {
    /// Converts an EXIF orientation tag value; `None` for values outside `1..=8`.
    pub fn from_exif(value: u16) -> Option<Orientation> {
        Some(match value {
            1 => Orientation::Normal,
            2 => Orientation::FlipHorizontal,
            3 => Orientation::Rotate180,
            4 => Orientation::FlipVertical,
            5 => Orientation::Transpose,
            6 => Orientation::Rotate90,
            7 => Orientation::Transverse,
            8 => Orientation::Rotate270,
            _ => return None,
        })
    }

    /// The EXIF tag value (1..=8).
    pub fn exif_value(self) -> u16 {
        self as u16
    }

    /// Returns `true` if applying the orientation swaps width and height.
    pub fn swaps_dimensions(self) -> bool {
        matches!(self, Orientation::Transpose | Orientation::Rotate90 | Orientation::Transverse | Orientation::Rotate270)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conveniences() {
        let mut img = Image::from_fn(3, 2, |x, y| 0xFF00_0000 | (x << 8) | y);
        assert_eq!(img.get(2, 1), Some(0xFF00_0201));
        assert_eq!(img.get(3, 0), None);
        assert!(img.set(0, 0, 0x1234_5678));
        assert!(!img.set(0, 2, 0));
        assert_eq!(img.row(0)[0], 0x1234_5678);
        assert!(img.has_alpha());
        let bytes = img.to_rgba8();
        assert_eq!(&bytes[..4], &[0x34, 0x56, 0x78, 0x12]);
        assert_eq!(Image::from_rgba8(3, 2, &bytes).unwrap(), img);
        assert!(Image::from_pixels(2, 2, alloc::vec![0; 3]).is_err());
        assert_eq!(Image::try_new(4, 4).unwrap().pixels.len(), 16);
        for v in 1..=8 {
            assert_eq!(Orientation::from_exif(v).unwrap().exif_value(), v);
        }
        assert_eq!(Orientation::from_exif(9), None);
    }
}
