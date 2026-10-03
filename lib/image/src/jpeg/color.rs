//! Chroma upsampling and color conversion for the JPEG decoder.
//!
//! Upsampling is "fancy" (libjpeg's triangle filter): each output sample is a 3:1 blend of the
//! nearest and the next-nearest input sample in each direction, which is bilinear interpolation
//! for the centered chroma siting JPEG uses. The 2x horizontal (4:2:2), 2x vertical (4:4:0) and
//! 2x2 (4:2:0) cases have dedicated loops with libjpeg's exact rounding; other integral factors
//! (4:1:1 and exotic ones) use a general bilinear filter.
//!
//! YCbCr to RGB conversion uses libjpeg's 16-bit fixed-point coefficients.

use alloc::vec::Vec;

/// 2:1 horizontal upsampling of one row: `out` receives `2 * inp.len()` samples.
pub(crate) fn h2v1(inp: &[u8], out: &mut [u8]) {
    let n = inp.len();
    if n == 1 {
        out[0] = inp[0];
        out[1] = inp[0];
        return;
    }
    out[0] = inp[0];
    out[1] = ((inp[0] as u32 * 3 + inp[1] as u32 + 2) >> 2) as u8;
    for (o, w) in out[2..].chunks_exact_mut(2).zip(inp.windows(3)) {
        let v = w[1] as u32 * 3;
        o[0] = ((v + w[0] as u32 + 1) >> 2) as u8;
        o[1] = ((v + w[2] as u32 + 2) >> 2) as u8;
    }
    let l = n - 1;
    out[2 * l] = ((inp[l] as u32 * 3 + inp[l - 1] as u32 + 1) >> 2) as u8;
    out[2 * l + 1] = inp[l];
}

/// 1:2 vertical upsampling: blends the nearest row with the next-nearest one. `lower` selects
/// the rounding libjpeg uses for the lower of the two output rows.
pub(crate) fn h1v2(near: &[u8], far: &[u8], lower: bool, out: &mut [u8]) {
    let bias = if lower { 2 } else { 1 };
    for ((o, &a), &b) in out.iter_mut().zip(near).zip(far) {
        *o = ((a as u32 * 3 + b as u32 + bias) >> 2) as u8;
    }
}

/// 2:1 upsampling in both directions for one output row: `near` is the nearest input row and
/// `far` the next-nearest one.
pub(crate) fn h2v2(near: &[u8], far: &[u8], out: &mut [u8], colsum: &mut Vec<u16>) {
    colsum.clear();
    colsum.extend(near.iter().zip(far).map(|(&a, &b)| a as u16 * 3 + b as u16));
    let cs = &colsum[..];
    let n = cs.len();
    if n == 1 {
        out[0] = ((cs[0] as u32 * 4 + 8) >> 4) as u8;
        out[1] = ((cs[0] as u32 * 4 + 7) >> 4) as u8;
        return;
    }
    out[0] = ((cs[0] as u32 * 4 + 8) >> 4) as u8;
    out[1] = ((cs[0] as u32 * 3 + cs[1] as u32 + 7) >> 4) as u8;
    for (o, w) in out[2..].chunks_exact_mut(2).zip(cs.windows(3)) {
        let this = w[1] as u32 * 3;
        o[0] = ((this + w[0] as u32 + 8) >> 4) as u8;
        o[1] = ((this + w[2] as u32 + 7) >> 4) as u8;
    }
    let l = n - 1;
    out[2 * l] = ((cs[l] as u32 * 3 + cs[l - 1] as u32 + 8) >> 4) as u8;
    out[2 * l + 1] = ((cs[l] as u32 * 4 + 7) >> 4) as u8;
}

/// General bilinear upsampling by an integral horizontal factor `fx` of a vertically blended
/// row (`w_near * near + w_far * far`, weights summing to `2 fy`).
pub(crate) fn generic(near: &[u8], far: &[u8], w_near: u32, w_far: u32, fx: usize, out: &mut [u8]) {
    let n = near.len();
    let denom = (w_near + w_far) * 2 * fx as u32;
    let col = |i: usize| w_near * near[i] as u32 + w_far * far[i] as u32;
    for (x, o) in out.iter_mut().enumerate().take(n * fx) {
        let (cx, phase) = (x / fx, x % fx);
        let d = 2 * phase as isize + 1 - fx as isize;
        let (nx, wn) = match d.signum() {
            -1 => (cx.saturating_sub(1), d.unsigned_abs() as u32),
            1 => ((cx + 1).min(n - 1), d as u32),
            _ => (cx, 0),
        };
        let ws = 2 * fx as u32 - wn;
        *o = ((ws * col(cx) + wn * col(nx) + denom / 2) / denom) as u8;
    }
}

#[inline(always)]
fn clamp(x: i32) -> u32 {
    x.clamp(0, 255) as u32
}

/// YCbCr to RGB (JFIF / ITU-R BT.601 full range).
#[inline(always)]
fn ycc(y: u8, cb: u8, cr: u8) -> (u32, u32, u32) {
    let y = y as i32;
    let cb = cb as i32 - 128;
    let cr = cr as i32 - 128;
    let r = y + ((91881 * cr + 32768) >> 16);
    let g = y + ((-22554 * cb - 46802 * cr + 32768) >> 16);
    let b = y + ((116130 * cb + 32768) >> 16);
    (clamp(r), clamp(g), clamp(b))
}

pub(crate) fn gray(y: &[u8], out: &mut [u32]) {
    for (o, &v) in out.iter_mut().zip(y) {
        *o = 0xFF00_0000 | v as u32 * 0x0001_0101;
    }
}

pub(crate) fn ycc_to_rgb(y: &[u8], cb: &[u8], cr: &[u8], out: &mut [u32]) {
    for (((o, &y), &cb), &cr) in out.iter_mut().zip(y).zip(cb).zip(cr) {
        let (r, g, b) = ycc(y, cb, cr);
        *o = 0xFF00_0000 | r << 16 | g << 8 | b;
    }
}

pub(crate) fn rgb(r: &[u8], g: &[u8], b: &[u8], out: &mut [u32]) {
    for (((o, &r), &g), &b) in out.iter_mut().zip(r).zip(g).zip(b) {
        *o = 0xFF00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
    }
}

/// `a * b / 255`, rounded.
#[inline(always)]
fn mul255(a: u32, b: u32) -> u32 {
    let t = a * b + 128;
    (t + (t >> 8)) >> 8
}

/// Adobe CMYK (stored inverted: 255 = no ink).
pub(crate) fn cmyk(c: &[u8], m: &[u8], y: &[u8], k: &[u8], out: &mut [u32]) {
    for ((((o, &c), &m), &y), &k) in out.iter_mut().zip(c).zip(m).zip(y).zip(k) {
        let k = k as u32;
        *o = 0xFF00_0000 | mul255(c as u32, k) << 16 | mul255(m as u32, k) << 8 | mul255(y as u32, k);
    }
}

/// Adobe YCCK: YCbCr-coded inverted CMY plus K.
pub(crate) fn ycck(y: &[u8], cb: &[u8], cr: &[u8], k: &[u8], out: &mut [u32]) {
    for ((((o, &y), &cb), &cr), &k) in out.iter_mut().zip(y).zip(cb).zip(cr).zip(k) {
        let (r, g, b) = ycc(y, cb, cr);
        let k = k as u32;
        *o = 0xFF00_0000 | mul255(255 - r, k) << 16 | mul255(255 - g, k) << 8 | mul255(255 - b, k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsampling_preserves_constants_and_interpolates() {
        let flat = [77u8; 9];
        let mut out = [0u8; 18];
        h2v1(&flat, &mut out);
        assert!(out.iter().all(|&v| v == 77));
        let mut colsum = Vec::new();
        h2v2(&flat, &flat, &mut out, &mut colsum);
        assert!(out.iter().all(|&v| v == 77));
        for fx in 1..=4 {
            let mut o = [0u8; 36];
            generic(&flat, &flat, 3, 1, fx, &mut o);
            assert!(o[..9 * fx].iter().all(|&v| v == 77), "fx {fx}");
        }
        // A ramp stays a (finer) ramp.
        let ramp = [0u8, 40, 80, 120];
        let mut out = [0u8; 8];
        h2v1(&ramp, &mut out);
        assert_eq!(out, [0, 10, 30, 50, 70, 90, 110, 120]);
        // Generic 2x matches the triangle filter.
        let mut g = [0u8; 8];
        generic(&ramp, &ramp, 2, 0, 2, &mut g);
        assert_eq!(g, [0, 10, 30, 50, 70, 90, 110, 120]);
        // Single-sample rows.
        let mut o2 = [0u8; 2];
        h2v1(&[9], &mut o2);
        assert_eq!(o2, [9, 9]);
        h2v2(&[9], &[9], &mut o2, &mut colsum);
        assert_eq!(o2, [9, 9]);
    }

    #[test]
    fn color_conversion() {
        let mut out = [0u32; 4];
        ycc_to_rgb(&[0, 255, 128, 76], &[128, 128, 128, 85], &[128, 128, 128, 255], &mut out);
        assert_eq!(out[0], 0xFF00_0000);
        assert_eq!(out[1], 0xFFFF_FFFF);
        assert_eq!(out[2], 0xFF80_8080);
        let (r, g, b) = ((out[3] >> 16) & 0xFF, (out[3] >> 8) & 0xFF, out[3] & 0xFF);
        assert!(r > 250 && g < 5 && b < 5, "{r} {g} {b}");
        cmyk(&[255, 0], &[255, 0], &[255, 255], &[255, 255], &mut out[..2]);
        assert_eq!(&out[..2], &[0xFFFF_FFFF, 0xFF00_00FF]);
        for a in 0..=255 {
            for b in [0, 1, 127, 128, 254, 255] {
                assert_eq!(mul255(a, b), (a * b + 127) / 255);
            }
        }
    }
}
