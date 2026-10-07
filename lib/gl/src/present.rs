//! Showing images in windows: bilinear scaling of 0xAARRGGBB images.

use core::ops::Range;

use crate::backend::Present;

/// Interpolates two 0xAARRGGBB words with an 8-bit weight (`f` in
/// 0..=256), two channels at a time.
#[inline(always)]
fn lerp(a: u32, b: u32, f: u32) -> u32 {
    let g = 256 - f;
    let rb = ((a & 0x00FF_00FF) * g + (b & 0x00FF_00FF) * f + 0x0080_0080) >> 8;
    let ag = ((a >> 8) & 0x00FF_00FF) * g + ((b >> 8) & 0x00FF_00FF) * f + 0x0080_0080;
    (rb & 0x00FF_00FF) | (ag & 0xFF00_FF00)
}

/// Writes rows `rows` of `dst`: the image `src` (`sw` x `sh`, rows from the
/// top, packed) scaled to it, bilinearly (or copied if the sizes match).
pub fn scale(src: &[u32], sw: usize, sh: usize, dst: &mut Present<'_>, rows: Range<usize>) {
    let (dw, dh, stride) = (dst.width as usize, dst.height as usize, dst.stride);
    let at = (rows.start * stride).min(dst.pixels.len());
    scale_rows(src, sw, sh, dw, dh, &mut dst.pixels[at..], stride, rows);
}

/// [`scale`] into `out`, which starts at row `rows.start` of a `dw` x `dh`
/// image with rows `stride` words apart.
pub fn scale_rows(
    src: &[u32],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    out: &mut [u32],
    stride: usize,
    rows: Range<usize>,
) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    // Source positions in 1/256 pixels, step per destination pixel.
    let step_x = ((sw << 8) / dw) as i64;
    let step_y = ((sh << 8) / dh) as i64;
    for y in rows.start..rows.end.min(dh) {
        let o = (y - rows.start) * stride;
        let Some(out) = out.get_mut(o..o + dw) else { break };
        if sw == dw && sh == dh {
            out.copy_from_slice(&src[y * sw..(y + 1) * sw]);
            continue;
        }
        let fy = (y as i64 * step_y + step_y / 2 - 128).clamp(0, ((sh - 1) << 8) as i64);
        let (y0, wy) = ((fy >> 8) as usize, (fy & 0xFF) as u32);
        let y1 = (y0 + 1).min(sh - 1);
        let (r0, r1) = (&src[y0 * sw..(y0 + 1) * sw], &src[y1 * sw..(y1 + 1) * sw]);
        let mut fx = step_x / 2 - 128;
        for d in out.iter_mut() {
            let x = fx.clamp(0, ((sw - 1) << 8) as i64);
            let (x0, wx) = ((x >> 8) as usize, (x & 0xFF) as u32);
            let x1 = (x0 + 1).min(sw - 1);
            *d = lerp(lerp(r0[x0], r0[x1], wx), lerp(r1[x0], r1[x1], wx), wy);
            fx += step_x;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn scales_up_and_copies() {
        let src = [0xFF00_0000, 0xFFFF_FFFF];
        let mut px = vec![0u32; 4];
        let mut dst = Present { pixels: &mut px, stride: 4, width: 4, height: 1, opaque: true };
        scale(&src, 2, 1, &mut dst, 0..1);
        // Ends clamp, the middle blends.
        assert_eq!(px[0], 0xFF00_0000);
        assert_eq!(px[3], 0xFFFF_FFFF);
        assert!(px[1] & 0xFF > 0x30 && px[1] & 0xFF < 0x50, "{:#x}", px[1]);
        let mut same = vec![0u32; 2];
        let mut dst = Present { pixels: &mut same, stride: 2, width: 2, height: 1, opaque: true };
        scale(&src, 2, 1, &mut dst, 0..1);
        assert_eq!(same, src);
    }
}
