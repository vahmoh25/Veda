//! Small internal helpers: fallible allocation and bounds-checked integer reads.

use alloc::vec::Vec;

use crate::error::ImageError;

/// Allocates a vector of `len` copies of `value`, reporting allocation failure as
/// [`ImageError::OutOfMemory`] instead of aborting.
pub(crate) fn try_vec<T: Clone>(len: usize, value: T) -> Result<Vec<T>, ImageError> {
    let mut v = Vec::new();
    v.try_reserve_exact(len)?;
    v.resize(len, value);
    Ok(v)
}

/// Reads `N` bytes at `off`, or fails with [`ImageError::Truncated`].
#[inline]
pub(crate) fn bytes<const N: usize>(data: &[u8], off: usize) -> Result<[u8; N], ImageError> {
    match data.get(off..).and_then(|s| s.get(..N)) {
        Some(s) => {
            let mut a = [0u8; N];
            a.copy_from_slice(s);
            Ok(a)
        }
        None => Err(ImageError::Truncated),
    }
}

/// Reads a big-endian `u16` at `off`.
#[inline]
pub(crate) fn be16(data: &[u8], off: usize) -> Result<u16, ImageError> {
    Ok(u16::from_be_bytes(bytes(data, off)?))
}

/// Reads a big-endian `u32` at `off`.
#[inline]
pub(crate) fn be32(data: &[u8], off: usize) -> Result<u32, ImageError> {
    Ok(u32::from_be_bytes(bytes(data, off)?))
}

/// Reads a little-endian `u16` at `off`.
#[inline]
pub(crate) fn le16(data: &[u8], off: usize) -> Result<u16, ImageError> {
    Ok(u16::from_le_bytes(bytes(data, off)?))
}

/// Reads a little-endian `u32` at `off`.
#[inline]
pub(crate) fn le32(data: &[u8], off: usize) -> Result<u32, ImageError> {
    Ok(u32::from_le_bytes(bytes(data, off)?))
}

/// Packs 8-bit channels into a `0xAARRGGBB` pixel.
#[inline(always)]
pub(crate) fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | (r << 16) | (g << 8) | b
}
