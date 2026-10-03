//! Presenting the internal render target: scaling it to the window with
//! bilinear filtering.
//!
//! The scalers write disjoint row ranges `[y0, y1)` of the destination, so
//! several threads can fill one image; hence the raw destination pointers.

use core::arch::x86_64::{
    __m128i, _mm_avg_epu8, _mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_storeu_si128, _mm_unpackhi_epi32,
    _mm_unpacklo_epi32,
};
use core::mem::MaybeUninit;

use super::scalar_loop;

/// Linear interpolation of two opaque pixels, t in 0..=256.
#[inline(always)]
fn lerp_px(a: u32, b: u32, t: u32) -> u32 {
    let it = 256 - t;
    let rb = ((a & 0x00FF_00FF).wrapping_mul(it).wrapping_add((b & 0x00FF_00FF).wrapping_mul(t)) >> 8) & 0x00FF_00FF;
    let ag =
        ((a >> 8) & 0x00FF_00FF).wrapping_mul(it).wrapping_add(((b >> 8) & 0x00FF_00FF).wrapping_mul(t)) & 0xFF00_FF00;
    rb | ag
}

/// Rounding-up average of two pixels per byte (like `pavgb`).
#[inline(always)]
fn avg_px(a: u32, b: u32) -> u32 {
    (a | b) - (((a ^ b) & 0xFEFE_FEFE) >> 1)
}

/// 3/4 a + 1/4 b per byte.
#[inline(always)]
fn mix31(a: u32, b: u32) -> u32 {
    avg_px(a, avg_px(a, b))
}

/// 3/4 a + 1/4 b per byte, four pixels at a time.
#[inline(always)]
fn mix31_simd(a: __m128i, b: __m128i) -> __m128i {
    // SAFETY: SSE2 is part of the x86-64 baseline.
    unsafe { _mm_avg_epu8(a, _mm_avg_epu8(a, b)) }
}

/// Loads four pixels.
///
/// # Safety
///
/// `p` must be valid for reading four pixels.
#[inline(always)]
unsafe fn load4(p: *const u32) -> __m128i {
    // SAFETY: as required of the caller (unaligned loads are allowed).
    unsafe { _mm_loadu_si128(p.cast()) }
}

/// Stores four pixels.
///
/// # Safety
///
/// `p` must be valid for writing four pixels.
#[inline(always)]
unsafe fn store4(p: *mut u32, v: __m128i) {
    // SAFETY: as required of the caller (unaligned stores are allowed).
    unsafe { _mm_storeu_si128(p.cast(), v) }
}

/// Bilinear scaling of the opaque `sw` x `sh` source (`sstride` pixels per
/// row) to `dw` x `dh` pixels: writes destination rows [y0, y1).
///
/// # Safety
///
/// `dst` must be valid for writing rows [y0, y1) of `dstride` pixels (at
/// least `dw`), which no other thread may access meanwhile.
pub unsafe fn upscale(
    src: &[u32],
    sw: i32,
    sh: i32,
    sstride: i32,
    dst: *mut u32,
    dw: i32,
    dh: i32,
    dstride: i32,
    y0: i32,
    y1: i32,
) {
    const MAXW: usize = 4096;
    if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 || dw as usize > MAXW {
        return;
    }
    // Separable filter: source rows are scaled horizontally once (two are
    // kept, as consecutive destination rows mostly share them), then each
    // destination row blends two of them vertically.
    let mut rows = [[MaybeUninit::<u32>::uninit(); MAXW]; 2];
    let mut row_src = [-1i32; 2];
    let stepx = ((sw as i64) << 16) / dw as i64;
    let stepy = ((sh as i64) << 16) / dh as i64;
    let maxy = ((sh - 1) as i64) << 16;
    let dw = dw as usize;
    // The horizontally scaled source row `sy`, scaling it if needed.
    let mut get = |sy: i32| -> *const u32 {
        if let Some(k) = row_src.iter().position(|&r| r == sy) {
            return rows[k].as_ptr().cast();
        }
        // Replace the row that is not the other one needed (the lower one).
        let k = if row_src[0] < row_src[1] { 0 } else { 1 };
        let start = sy as usize * sstride as usize;
        let out = &mut rows[k][..dw];
        scale_row(&src[start..start + sw as usize], dw, stepx, out);
        row_src[k] = sy;
        rows[k].as_ptr().cast()
    };
    for y in y0..y1 {
        let fy = (y as i64 * stepy + (stepy >> 1) - 32768).clamp(0, maxy);
        let sy = (fy >> 16) as i32;
        let wy = (fy >> 8) as u32 & 255;
        let sy1 = if sy + 1 < sh { sy + 1 } else { sy };
        // SAFETY: the caller lends us this row of the destination; the
        // scaled rows hold `dw` initialised pixels.
        unsafe {
            let d = core::slice::from_raw_parts_mut(dst.add(y as usize * dstride as usize), dw);
            if wy == 0 || sy1 == sy {
                let a = core::slice::from_raw_parts(get(sy), dw);
                for (d, &a) in d.iter_mut().zip(a) {
                    scalar_loop();
                    *d = a | 0xFF00_0000;
                }
            } else {
                // Rows only increase within a band, so fetching sy1 evicts
                // an older row, never sy.
                let a = get(sy);
                let b = get(sy1);
                let (a, b) = (core::slice::from_raw_parts(a, dw), core::slice::from_raw_parts(b, dw));
                for ((d, &a), &b) in d.iter_mut().zip(a).zip(b) {
                    scalar_loop();
                    *d = lerp_px(a, b, wy) | 0xFF00_0000;
                }
            }
        }
    }
}

/// Horizontally scales source row `s` to `dw` pixels in `out`.
fn scale_row(s: &[u32], dw: usize, stepx: i64, out: &mut [MaybeUninit<u32>]) {
    let maxx = ((s.len() - 1) as i64) << 16;
    let mut fx = (stepx >> 1) - 32768;
    for o in &mut out[..dw] {
        scalar_loop();
        let cx = fx.clamp(0, maxx);
        let sx = (cx >> 16) as usize;
        let sx1 = (sx + 1).min(s.len() - 1);
        o.write(lerp_px(s[sx], s[sx1], (cx >> 8) as u32 & 255));
        fx += stepx;
    }
}

/// The rows of a 2x scaled band: source rows [sy0, sy1) cover destination
/// rows [y0, y1) (clipped to them).
fn band_rows(sh: i32, y0: i32, y1: i32) -> core::ops::Range<i32> {
    (y0 >> 1)..((y1 + 1) >> 1).min(sh)
}

/// The destination row `y` if it is in [y0, y1), else null.
///
/// # Safety
///
/// As for the scalers: rows [y0, y1) of `dst` must be valid.
#[inline(always)]
unsafe fn dst_row(dst: *mut u32, dstride: i32, y: i32, y0: i32, y1: i32) -> *mut u32 {
    if y >= y0 && y < y1 {
        // SAFETY: as required of the caller.
        unsafe { dst.add(y as usize * dstride as usize) }
    } else {
        core::ptr::null_mut()
    }
}

/// Fast 2x upscaling: each source pixel s(x, y) becomes the 2x2 block
///
/// ```text
/// s             avg(s, right)
/// avg(s, down)  avg of all four
/// ```
///
/// i.e. bilinear sampling at the source pixel corners (the image moves by
/// half a source pixel, which is invisible). One SSE2 average per output
/// pixel. Writes destination rows [y0, y1).
///
/// # Safety
///
/// `src` must hold `sh` rows of `sstride` pixels (at least `sw`), and
/// `dst` must be valid for writing rows [y0, y1) of `dstride` pixels (at
/// least 2 * `sw`), which no other thread may access meanwhile.
pub unsafe fn upscale2x_fast(
    src: *const u32,
    sw: i32,
    sh: i32,
    sstride: i32,
    dst: *mut u32,
    dstride: i32,
    y0: i32,
    y1: i32,
) {
    if sw <= 0 || sh <= 0 {
        return;
    }
    // SAFETY: SSE2 is part of the x86-64 baseline; every pointer below stays
    // inside the rows the caller vouches for.
    unsafe {
        let opaque = _mm_set1_epi32(0xFF00_0000u32 as i32);
        for sy in band_rows(sh, y0, y1) {
            let r0 = src.add(sy as usize * sstride as usize);
            let r1 = if sy + 1 < sh { r0.add(sstride as usize) } else { r0 };
            let da = dst_row(dst, dstride, 2 * sy, y0, y1);
            let db = dst_row(dst, dstride, 2 * sy + 1, y0, y1);
            let mut x = 0;
            while x + 5 <= sw {
                let xs = x as usize;
                let a = load4(r0.add(xs));
                let b = load4(r0.add(xs + 1));
                let ab = _mm_avg_epu8(a, b);
                if !da.is_null() {
                    store4(da.add(2 * xs), _mm_or_si128(_mm_unpacklo_epi32(a, ab), opaque));
                    store4(da.add(2 * xs + 4), _mm_or_si128(_mm_unpackhi_epi32(a, ab), opaque));
                }
                if !db.is_null() {
                    let c = load4(r1.add(xs));
                    let d = load4(r1.add(xs + 1));
                    let ac = _mm_avg_epu8(a, c);
                    let abcd = _mm_avg_epu8(ab, _mm_avg_epu8(c, d));
                    store4(db.add(2 * xs), _mm_or_si128(_mm_unpacklo_epi32(ac, abcd), opaque));
                    store4(db.add(2 * xs + 4), _mm_or_si128(_mm_unpackhi_epi32(ac, abcd), opaque));
                }
                x += 4;
            }
            while x < sw {
                let (xs, xr) = (x as usize, (x + 1).min(sw - 1) as usize);
                let a = *r0.add(xs);
                let ab = avg_px(a, *r0.add(xr));
                if !da.is_null() {
                    *da.add(2 * xs) = a | 0xFF00_0000;
                    *da.add(2 * xs + 1) = ab | 0xFF00_0000;
                }
                if !db.is_null() {
                    let c = *r1.add(xs);
                    *db.add(2 * xs) = avg_px(a, c) | 0xFF00_0000;
                    *db.add(2 * xs + 1) = avg_px(ab, avg_px(c, *r1.add(xr))) | 0xFF00_0000;
                }
                x += 1;
            }
        }
    }
}

/// The same filter as [`upscale2x_fast`] in scalar code, which can be the
/// faster one under CPU emulation ([`crate::Renderer::calibrate`] picks).
///
/// # Safety
///
/// As for [`upscale2x_fast`].
pub unsafe fn upscale2x_swar(
    src: *const u32,
    sw: i32,
    sh: i32,
    sstride: i32,
    dst: *mut u32,
    dstride: i32,
    y0: i32,
    y1: i32,
) {
    if sw <= 0 || sh <= 0 {
        return;
    }
    // SAFETY: every pointer below stays inside the rows the caller vouches
    // for; pairs of destination pixels are written as one 64-bit value.
    unsafe {
        for sy in band_rows(sh, y0, y1) {
            let r0 = src.add(sy as usize * sstride as usize);
            let r1 = if sy + 1 < sh { r0.add(sstride as usize) } else { r0 };
            let da = dst_row(dst, dstride, 2 * sy, y0, y1);
            let db = dst_row(dst, dstride, 2 * sy + 1, y0, y1);
            let (mut a, mut c) = (*r0, *r1);
            for x in 0..sw as usize {
                scalar_loop();
                let xr = (x + 1).min(sw as usize - 1);
                let (b, d) = (*r0.add(xr), *r1.add(xr));
                let ab = avg_px(a, b);
                if !da.is_null() {
                    let pair = (a | 0xFF00_0000) as u64 | ((ab | 0xFF00_0000) as u64) << 32;
                    da.add(2 * x).cast::<u64>().write_unaligned(pair);
                }
                if !db.is_null() {
                    let bottom = avg_px(ab, avg_px(c, d));
                    let pair = (avg_px(a, c) | 0xFF00_0000) as u64 | ((bottom | 0xFF00_0000) as u64) << 32;
                    db.add(2 * x).cast::<u64>().write_unaligned(pair);
                }
                a = b;
                c = d;
            }
        }
    }
}

/// Horizontally doubles one source row into `out` (2 * `s.len()` pixels)
/// with 3/4 - 1/4 weights: the pixel centres of the doubled row fall at
/// 1/4 and 3/4 of the source pixels.
fn double_row(s: &[u32], out: &mut [u32]) {
    let sw = s.len();
    if sw == 0 {
        return;
    }
    let scalar = |x: usize, out: &mut [u32]| {
        let (c, l, r) = (s[x], s[x.saturating_sub(1)], s[(x + 1).min(sw - 1)]);
        out[2 * x] = mix31(c, l);
        out[2 * x + 1] = mix31(c, r);
    };
    if sw < 8 {
        for x in 0..sw {
            scalar(x, out);
        }
        return;
    }
    // Edges in scalar code, the middle four pixels at a time.
    scalar(0, out);
    let mut x = 1;
    while x + 4 < sw {
        // SAFETY: SSE2 is part of the x86-64 baseline; x - 1 .. x + 5 and
        // 2x .. 2x + 8 are inside the rows.
        unsafe {
            let c = load4(s.as_ptr().add(x));
            let l = load4(s.as_ptr().add(x - 1));
            let r = load4(s.as_ptr().add(x + 1));
            let even = mix31_simd(c, l);
            let odd = mix31_simd(c, r);
            store4(out.as_mut_ptr().add(2 * x), _mm_unpacklo_epi32(even, odd));
            store4(out.as_mut_ptr().add(2 * x + 4), _mm_unpackhi_epi32(even, odd));
        }
        x += 4;
    }
    while x < sw {
        scalar(x, out);
        x += 1;
    }
}

/// Writes destination row `d` as 3/4 `a` + 1/4 `b` (both doubled rows).
fn mix_rows(a: &[u32], b: &[u32], d: &mut [u32]) {
    let dw = d.len();
    let mut x = 0;
    // SAFETY: SSE2 is part of the x86-64 baseline; x .. x + 4 is inside all
    // three rows.
    unsafe {
        let opaque = _mm_set1_epi32(0xFF00_0000u32 as i32);
        while x + 4 <= dw {
            let v = mix31_simd(load4(a.as_ptr().add(x)), load4(b.as_ptr().add(x)));
            store4(d.as_mut_ptr().add(x), _mm_or_si128(v, opaque));
            x += 4;
        }
    }
    for x in x..dw {
        scalar_loop();
        d[x] = mix31(a[x], b[x]) | 0xFF00_0000;
    }
}

/// Exactly 2x upscaling with bilinear filtering (3:1 weights). Writes
/// destination rows [y0, y1).
///
/// # Safety
///
/// As for [`upscale2x_fast`].
pub unsafe fn upscale2x(
    src: *const u32,
    sw: i32,
    sh: i32,
    sstride: i32,
    dst: *mut u32,
    dstride: i32,
    y0: i32,
    y1: i32,
) {
    const MAXW: usize = 2048;
    if sw <= 0 || sh <= 0 || sw as usize > MAXW {
        return;
    }
    let rows = band_rows(sh, y0, y1);
    if rows.is_empty() {
        return;
    }
    let (sw, dw) = (sw as usize, 2 * sw as usize);
    // SAFETY: as required of the caller.
    let row = |sy: i32| unsafe { core::slice::from_raw_parts(src.add(sy as usize * sstride as usize), sw) };
    // A rolling window of horizontally doubled source rows (above, current,
    // below). Destination row 2*sy mixes the current row 3:1 with the row
    // above, row 2*sy+1 with the row below.
    let mut bufs = [[0u32; 2 * MAXW]; 3];
    let [prev, cur, next] = &mut bufs;
    let (mut prev, mut cur, mut next) = (&mut prev[..dw], &mut cur[..dw], &mut next[..dw]);
    double_row(row((rows.start - 1).max(0)), prev);
    double_row(row(rows.start), cur);
    for sy in rows {
        double_row(row(if sy + 1 < sh { sy + 1 } else { sy }), next);
        for (y, other) in [(2 * sy, &*prev), (2 * sy + 1, &*next)] {
            // SAFETY: as required of the caller.
            let d = unsafe { dst_row(dst, dstride, y, y0, y1) };
            if !d.is_null() {
                // SAFETY: the caller lends us this row, at least dw pixels.
                mix_rows(cur, other, unsafe { core::slice::from_raw_parts_mut(d, dw) });
            }
        }
        // Rotate the window.
        core::mem::swap(&mut prev, &mut cur);
        core::mem::swap(&mut cur, &mut next);
    }
}
