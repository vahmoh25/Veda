//! Float to fixed-point conversions for the integer pipeline.

use vmath::Vec3;

/// Rounds to the nearest integer, saturating at the `i32` range.
#[inline]
pub(crate) fn round_i32(v: f32) -> i32 {
    // `as` saturates (and maps NaN to 0).
    if v >= 0.0 { (v + 0.5) as i32 } else { (v - 0.5) as i32 }
}

/// 16.16 fixed point.
#[inline]
pub(crate) fn fx16(v: f32) -> i32 {
    round_i32(v * 65536.0)
}

/// 2.14 fixed point (unit vectors).
#[inline]
pub(crate) fn fx14(v: f32) -> i32 {
    round_i32(v * 16384.0)
}

/// 8.8 fixed point (colour multipliers, 1.0 = 256), clamped to `0..=max`.
#[inline]
pub(crate) fn fx8(v: f32, max: i32) -> i32 {
    round_i32(v * 256.0).clamp(0, max)
}

/// An RGB colour (0..1 per channel) in output units scaled by 256.
#[inline]
pub(crate) fn out_rgb(c: Vec3) -> [i32; 4] {
    let k = 255.0 * 256.0;
    [round_i32(c.x * k).clamp(0, 0xFFFF), round_i32(c.y * k).clamp(0, 0xFFFF), round_i32(c.z * k).clamp(0, 0xFFFF), 0]
}

/// A unit direction in 2.14.
#[inline]
pub(crate) fn dir14(d: Vec3) -> [i32; 4] {
    let d = d.normalize_or_zero();
    [fx14(d.x), fx14(d.y), fx14(d.z), 0]
}

/// The red, green and blue channels of a `0xAARRGGBB` colour as 0..1.
#[inline]
pub fn rgb_of(c: u32) -> Vec3 {
    Vec3::new(((c >> 16) & 255) as f32 / 255.0, ((c >> 8) & 255) as f32 / 255.0, (c & 255) as f32 / 255.0)
}

/// The alpha of a `0xAARRGGBB` colour as 0..1.
#[inline]
pub fn alpha_of(c: u32) -> f32 {
    (c >> 24) as f32 / 255.0
}

/// Packs 0..1 channels into an opaque `0xFFRRGGBB` colour.
#[inline]
pub fn pack_rgb(c: Vec3) -> u32 {
    let ch = |v: f32| round_i32(v * 255.0).clamp(0, 255) as u32;
    0xFF00_0000 | ch(c.x) << 16 | ch(c.y) << 8 | ch(c.z)
}

/// Packs 0..1 channels and alpha into a straight-alpha `0xAARRGGBB` colour.
#[inline]
pub fn pack_rgba(c: Vec3, a: f32) -> u32 {
    let ch = |v: f32| round_i32(v * 255.0).clamp(0, 255) as u32;
    ch(a) << 24 | ch(c.x) << 16 | ch(c.y) << 8 | ch(c.z)
}
