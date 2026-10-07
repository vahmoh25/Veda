//! Pixel transfers: where a rectangle's pixels are in application memory
//! (OpenGL ES 3.0 sections 3.7.1 and 4.3.1, the pixel storage modes), and
//! conversion between the application's `(format, type)` layouts and a
//! resource's storage format.
//!
//! Transfers move color values as stored: sRGB-encoded texels are uploaded
//! and read back unconverted, as the specification requires (only sampling,
//! blending and blits convert them).

use alloc::vec::Vec;

use crate::context::PixelStore;
use crate::format::{Client, Format, Texel};
use crate::gl;

/// Pixel data an application passes in: a slice, or — when a buffer is
/// bound to `PIXEL_UNPACK_BUFFER` — an offset into that buffer. `None`
/// leaves a new image's contents undefined (zero, in Veda).
#[derive(Clone, Copy, Debug, Default)]
pub enum Pixels<'a> {
    #[default]
    None,
    Data(&'a [u8]),
    Offset(usize),
}

impl<'a> From<&'a [u8]> for Pixels<'a> {
    fn from(d: &'a [u8]) -> Pixels<'a> {
        Pixels::Data(d)
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for Pixels<'a> {
    fn from(d: &'a [u8; N]) -> Pixels<'a> {
        Pixels::Data(d)
    }
}

impl<'a> From<&'a Vec<u8>> for Pixels<'a> {
    fn from(d: &'a Vec<u8>) -> Pixels<'a> {
        Pixels::Data(d)
    }
}

impl<'a> From<Option<&'a [u8]>> for Pixels<'a> {
    fn from(d: Option<&'a [u8]>) -> Pixels<'a> {
        d.map_or(Pixels::None, Pixels::Data)
    }
}

/// Where `ReadPixels` puts pixels: a slice, or an offset into the buffer
/// bound to `PIXEL_PACK_BUFFER`.
#[derive(Debug)]
pub enum PixelsMut<'a> {
    Data(&'a mut [u8]),
    Offset(usize),
}

impl<'a> From<&'a mut [u8]> for PixelsMut<'a> {
    fn from(d: &'a mut [u8]) -> PixelsMut<'a> {
        PixelsMut::Data(d)
    }
}

impl<'a> From<&'a mut Vec<u8>> for PixelsMut<'a> {
    fn from(d: &'a mut Vec<u8>) -> PixelsMut<'a> {
        PixelsMut::Data(d)
    }
}

/// A zeroed buffer of `len` bytes, or `None` if there is not enough memory
/// (sizes come from applications, so running out must be an error, not an
/// abort).
pub fn try_zeroed(len: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, 0);
    Some(v)
}

/// Where the pixels of a `width` x `height` x `depth` box are, in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// The first pixel (after the skips).
    pub start: usize,
    /// From one row to the next.
    pub row: usize,
    /// From one image (3D slice) to the next.
    pub image: usize,
    /// Bytes of one pixel.
    pub pixel: usize,
    /// Bytes of one row's pixels.
    pub row_bytes: usize,
    /// One past the last byte the pixels occupy (0 for an empty box).
    pub end: usize,
}

/// The layout of a box of pixels of `client`'s layout under the storage
/// modes `store`, or `None` if its size does not fit in memory.
pub fn layout(client: Client, width: u32, height: u32, depth: u32, store: &PixelStore) -> Option<Layout> {
    let (n, s) = if client.packed() { (1, client.type_bytes()) } else { (client.components(), client.type_bytes()) };
    let pixel = n * s;
    group_layout(pixel, s, width, height, depth, store)
}

/// [`layout`] for groups of `pixel` bytes made of `element`-byte values.
pub fn group_layout(
    pixel: usize,
    element: usize,
    width: u32,
    height: u32,
    depth: u32,
    store: &PixelStore,
) -> Option<Layout> {
    let (w, h, d) = (width as usize, height as usize, depth as usize);
    let l = if store.row_length > 0 { store.row_length as usize } else { w };
    let a = store.alignment.max(1) as usize;
    let unpadded = pixel.checked_mul(l)?;
    let row = if element >= a { unpadded } else { unpadded.div_ceil(a).checked_mul(a)? };
    let image_rows = if store.image_height > 0 { store.image_height as usize } else { h };
    let image = row.checked_mul(image_rows)?;
    let start = (store.skip_images as usize)
        .checked_mul(image)?
        .checked_add((store.skip_rows as usize).checked_mul(row)?)?
        .checked_add((store.skip_pixels as usize).checked_mul(pixel)?)?;
    let row_bytes = pixel.checked_mul(w)?;
    let end = if w == 0 || h == 0 || d == 0 {
        0
    } else {
        start
            .checked_add((d - 1).checked_mul(image)?)?
            .checked_add((h - 1).checked_mul(row)?)?
            .checked_add(row_bytes)?
    };
    Some(Layout { start, row, image, pixel, row_bytes, end })
}

/// The format transfers treat a storage format as: sRGB formats as their
/// linear twins, so that values move unconverted.
pub fn raw(f: Format) -> Format {
    match f {
        Format::Rgba8Srgb => Format::Rgba8Unorm,
        Format::Rgbx8Srgb => Format::Rgbx8Unorm,
        f => f,
    }
}

/// Whether `client` pixels are byte for byte `f`'s texels.
pub fn same_layout(client: Client, f: Format) -> bool {
    use Format as F;
    use gl::*;
    let f = raw(f);
    matches!(
        (client.format, client.ty, f),
        (RGBA, UNSIGNED_BYTE, F::Rgba8Unorm)
            | (RGBA, BYTE, F::Rgba8Snorm)
            | (RGBA_INTEGER, UNSIGNED_BYTE, F::Rgba8Uint)
            | (RGBA_INTEGER, BYTE, F::Rgba8Sint)
            | (RG, UNSIGNED_BYTE, F::Rg8Unorm)
            | (RG, BYTE, F::Rg8Snorm)
            | (RG_INTEGER, UNSIGNED_BYTE, F::Rg8Uint)
            | (RG_INTEGER, BYTE, F::Rg8Sint)
            | (RED, UNSIGNED_BYTE, F::R8Unorm)
            | (RED, BYTE, F::R8Snorm)
            | (RED_INTEGER, UNSIGNED_BYTE, F::R8Uint)
            | (RED_INTEGER, BYTE, F::R8Sint)
            | (RGBA, HALF_FLOAT, F::Rgba16Float)
            | (RG, HALF_FLOAT, F::Rg16Float)
            | (RED, HALF_FLOAT, F::R16Float)
            | (RGBA, FLOAT, F::Rgba32Float)
            | (RG, FLOAT, F::Rg32Float)
            | (RED, FLOAT, F::R32Float)
            | (RGBA_INTEGER, UNSIGNED_SHORT, F::Rgba16Uint)
            | (RGBA_INTEGER, SHORT, F::Rgba16Sint)
            | (RG_INTEGER, UNSIGNED_SHORT, F::Rg16Uint)
            | (RG_INTEGER, SHORT, F::Rg16Sint)
            | (RED_INTEGER, UNSIGNED_SHORT, F::R16Uint)
            | (RED_INTEGER, SHORT, F::R16Sint)
            | (RGBA_INTEGER, UNSIGNED_INT, F::Rgba32Uint)
            | (RGBA_INTEGER, INT, F::Rgba32Sint)
            | (RG_INTEGER, UNSIGNED_INT, F::Rg32Uint)
            | (RG_INTEGER, INT, F::Rg32Sint)
            | (RED_INTEGER, UNSIGNED_INT, F::R32Uint)
            | (RED_INTEGER, INT, F::R32Sint)
            | (RGB, UNSIGNED_SHORT_5_6_5, F::B5G6R5Unorm)
            | (RGBA, UNSIGNED_SHORT_4_4_4_4, F::Rgba4Unorm)
            | (RGBA, UNSIGNED_SHORT_5_5_5_1, F::Rgb5A1Unorm)
            | (RGBA, UNSIGNED_INT_2_10_10_10_REV, F::Rgb10A2Unorm)
            | (RGBA_INTEGER, UNSIGNED_INT_2_10_10_10_REV, F::Rgb10A2Uint)
            | (RGB, UNSIGNED_INT_10F_11F_11F_REV, F::R11G11B10Float)
            | (RGB, UNSIGNED_INT_5_9_9_9_REV, F::Rgb9E5Float)
            | (DEPTH_COMPONENT, UNSIGNED_SHORT, F::D16Unorm)
            | (DEPTH_COMPONENT, FLOAT, F::D32Float)
            | (DEPTH_STENCIL, UNSIGNED_INT_24_8, F::D24UnormS8Uint)
            | (DEPTH_STENCIL, FLOAT_32_UNSIGNED_INT_24_8_REV, F::D32FloatS8Uint)
            | (LUMINANCE, UNSIGNED_BYTE, F::L8Unorm)
            | (ALPHA, UNSIGNED_BYTE, F::A8Unorm)
            | (LUMINANCE_ALPHA, UNSIGNED_BYTE, F::L8A8Unorm)
            | (LUMINANCE, FLOAT, F::L32Float)
            | (ALPHA, FLOAT, F::A32Float)
            | (LUMINANCE_ALPHA, FLOAT, F::L32A32Float)
            | (LUMINANCE, HALF_FLOAT, F::L16Float)
            | (ALPHA, HALF_FLOAT, F::A16Float)
            | (LUMINANCE_ALPHA, HALF_FLOAT, F::L16A16Float)
    )
}

/// Converts application pixels (`src`, laid out as `layout` says) to `f`'s
/// texels, tightly packed: rows of `width * f.bytes()` bytes, images of
/// `height` rows. `src` must hold `layout.end` bytes.
pub fn unpack(
    client: Client,
    src: &[u8],
    layout: &Layout,
    width: u32,
    height: u32,
    depth: u32,
    f: Format,
) -> Option<Vec<u8>> {
    let tb = f.bytes();
    let (w, h, d) = (width as usize, height as usize, depth as usize);
    let mut out = try_zeroed(w.checked_mul(h)?.checked_mul(d)?.checked_mul(tb)?)?;
    if out.is_empty() {
        return Some(out);
    }
    let raw_f = raw(f);
    let same = same_layout(client, f);
    let rgb8 = client.format == gl::RGB && client.ty == gl::UNSIGNED_BYTE && raw_f == Format::Rgbx8Unorm;
    for z in 0..d {
        for y in 0..h {
            let s = layout.start + z * layout.image + y * layout.row;
            let srow = &src[s..s + layout.row_bytes];
            let o = (z * h + y) * w * tb;
            let drow = &mut out[o..o + w * tb];
            if same {
                drow.copy_from_slice(srow);
            } else if rgb8 {
                for (px, sp) in drow.as_chunks_mut::<4>().0.iter_mut().zip(srow.as_chunks::<3>().0) {
                    px.copy_from_slice(&[sp[0], sp[1], sp[2], 255]);
                }
            } else {
                for (px, sp) in drow.chunks_exact_mut(tb).zip(srow.chunks_exact(layout.pixel)) {
                    raw_f.encode(&client.decode(sp), px);
                }
            }
        }
    }
    Some(out)
}

/// Converts `f`'s texels (tightly packed rows of `width`) to application
/// pixels, into `dst` as `layout` says (bytes between rows are left
/// alone). `dst` must hold `layout.end` bytes.
pub fn pack(f: Format, src: &[u8], width: u32, height: u32, client: Client, layout: &Layout, dst: &mut [u8]) {
    let tb = f.bytes();
    let (w, h) = (width as usize, height as usize);
    let raw_f = raw(f);
    let same = same_layout(client, f);
    for y in 0..h {
        let srow = &src[y * w * tb..(y + 1) * w * tb];
        let o = layout.start + y * layout.row;
        let drow = &mut dst[o..o + layout.row_bytes];
        if same {
            drow.copy_from_slice(srow);
        } else {
            for (px, dp) in srow.chunks_exact(tb).zip(drow.chunks_exact_mut(layout.pixel)) {
                client.encode(&raw_f.decode(px), dp);
            }
        }
    }
}

/// A texel of `f` as the neutral form, with sRGB values left encoded.
pub fn decode_raw(f: Format, b: &[u8]) -> Texel {
    raw(f).decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn store(alignment: u32) -> PixelStore {
        PixelStore { alignment, ..PixelStore::default() }
    }

    #[test]
    fn rows_are_aligned_as_the_specification_says() {
        // RGB bytes: 3 per pixel, rows padded to 4.
        let l = layout(Client::new(gl::RGB, gl::UNSIGNED_BYTE), 5, 3, 1, &store(4)).unwrap();
        assert_eq!((l.row, l.row_bytes, l.end), (16, 15, 16 * 2 + 15));
        // Elements at least as large as the alignment are not padded.
        let l = layout(Client::new(gl::RGB, gl::FLOAT), 5, 2, 1, &store(4)).unwrap();
        assert_eq!(l.row, 60);
        // ... but 4-byte floats are with an alignment of 8.
        let l = layout(Client::new(gl::RGB, gl::FLOAT), 5, 2, 1, &store(8)).unwrap();
        assert_eq!(l.row, 64);
        // Packed types count as one element.
        let l = layout(Client::new(gl::RGB, gl::UNSIGNED_SHORT_5_6_5), 3, 2, 1, &store(4)).unwrap();
        assert_eq!(l.row, 8);
    }

    #[test]
    fn skips_row_length_and_image_height() {
        let s =
            PixelStore { alignment: 1, row_length: 10, image_height: 4, skip_pixels: 2, skip_rows: 1, skip_images: 1 };
        let l = layout(Client::new(gl::RGBA, gl::UNSIGNED_BYTE), 3, 2, 2, &s).unwrap();
        assert_eq!(l.row, 40);
        assert_eq!(l.image, 160);
        assert_eq!(l.start, 160 + 40 + 8);
        assert_eq!(l.end, l.start + 160 + 40 + 12);
    }

    #[test]
    fn huge_boxes_do_not_overflow() {
        let s = PixelStore { row_length: u32::MAX, image_height: u32::MAX, skip_images: u32::MAX, ..store(1) };
        let l = layout(Client::new(gl::RGBA, gl::FLOAT), u32::MAX, u32::MAX, u32::MAX, &s);
        assert!(l.is_none() || l.unwrap().end > 0);
    }

    #[test]
    fn unpack_converts_and_expands() {
        let src = [10u8, 20, 30, 40, 50, 60, 0, 0];
        let l = layout(Client::new(gl::RGB, gl::UNSIGNED_BYTE), 2, 1, 1, &store(4)).unwrap();
        let out = unpack(Client::new(gl::RGB, gl::UNSIGNED_BYTE), &src, &l, 2, 1, 1, Format::Rgbx8Unorm).unwrap();
        assert_eq!(out, [10, 20, 30, 255, 40, 50, 60, 255]);
        // sRGB values move unconverted.
        let l4 = layout(Client::new(gl::RGBA, gl::UNSIGNED_BYTE), 1, 1, 1, &store(4)).unwrap();
        let out = unpack(Client::new(gl::RGBA, gl::UNSIGNED_BYTE), &src, &l4, 1, 1, 1, Format::Rgba8Srgb).unwrap();
        assert_eq!(out, [10, 20, 30, 40]);
        // Bytes to 4-bit components round to nearest.
        let px = [0u8, 255, 136, 119];
        let l = layout(Client::new(gl::RGBA, gl::UNSIGNED_BYTE), 1, 1, 1, &store(4)).unwrap();
        let out = unpack(Client::new(gl::RGBA, gl::UNSIGNED_BYTE), &px, &l, 1, 1, 1, Format::Rgba4Unorm).unwrap();
        let v = u16::from_le_bytes([out[0], out[1]]);
        assert_eq!(v, (15 << 8) | (8 << 4) | 7);
    }

    #[test]
    fn pack_round_trips_and_keeps_padding() {
        let texels = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let client = Client::new(gl::RGBA, gl::UNSIGNED_BYTE);
        let s = PixelStore { row_length: 3, ..store(4) };
        let l = layout(client, 1, 2, 1, &s).unwrap();
        let mut dst = vec![0xAAu8; l.end];
        pack(Format::Rgba8Unorm, &texels, 1, 2, client, &l, &mut dst);
        assert_eq!(&dst[0..4], &[1, 2, 3, 4]);
        assert_eq!(&dst[4..12], &[0xAA; 8]);
        assert_eq!(&dst[12..16], &[5, 6, 7, 8]);
        // Float framebuffers read as floats.
        let f = 0.25f32.to_bits().to_le_bytes();
        let one = 1.0f32.to_bits().to_le_bytes();
        let half = [f, f, f, one].concat();
        let client = Client::new(gl::RGBA, gl::FLOAT);
        let l = layout(client, 1, 1, 1, &store(4)).unwrap();
        let mut dst = vec![0u8; 16];
        pack(Format::Rgba32Float, &half, 1, 1, client, &l, &mut dst);
        assert_eq!(dst, half);
    }
}
