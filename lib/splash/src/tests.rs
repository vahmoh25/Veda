use std::vec;
use std::vec::Vec;

use crate::*;

/// The 8x8 Bayer matrix, written out (by row).
const BAYER: [[u32; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// The splash, written plainly as the reference: each channel of the
/// gradient at the middle of its row, plus the pixel's threshold, rounded
/// down (all of it in 128ths of a step and `h`ths); the ring over it.
fn reference(w: u32, h: u32) -> Vec<u32> {
    let mut out = vec![0; (w * h) as usize];
    let (top, bottom) = (0x070A16u32, 0x121A35u32);
    let outer = (h / 12).clamp(28, 96) as i32 * 16;
    let inner = outer * 7 / 9;
    let cx = w as i32 * 8;
    let cy = (h as i32 / 2 - h as i32 / 16) * 16;
    for y in 0..h {
        let dy = y as i32 * 16 + 8 - cy;
        for x in 0..w {
            let mut c = 0;
            for s in [16, 8, 0] {
                let (t, b) = (((top >> s) & 0xFF) as i64, ((bottom >> s) & 0xFF) as i64);
                let threshold = BAYER[(y % 8) as usize][(x % 8) as usize] as i64;
                let n = t * 128 * h as i64 + (b - t) * (2 * y as i64 + 1) * 64 + (2 * threshold + 1) * h as i64;
                c |= ((n / (128 * h as i64)) as u32) << s;
            }
            let dx = x as i32 * 16 + 8 - cx;
            if dx.abs() <= outer + 16 && dy.abs() <= outer + 16 {
                let d = isqrt((dx * dx + dy * dy) as u32) as i32;
                let cov = ((outer + 8 - d).clamp(0, 16) * (d - inner + 8).clamp(0, 16)) as u32;
                if cov > 0 {
                    c = mix(c, 0xFFFFFF, cov);
                }
            }
            out[(y * w + x) as usize] = c;
        }
    }
    out
}

#[test]
fn rows_make_the_picture() {
    for (w, h) in [(1280, 800), (1920, 1080), (800, 600), (3840, 2400), (320, 200), (100, 600)] {
        let mut rows = vec![0; (w * h) as usize];
        for (y, out) in rows.chunks_exact_mut(w as usize).enumerate() {
            row(w, h, y as u32, out);
        }
        assert!(rows == reference(w, h), "{}x{}", w, h);
    }
}

#[test]
fn the_pattern() {
    for y in 0..16 {
        for x in 0..16 {
            assert_eq!(bayer(x, y), BAYER[(y % 8) as usize][(x % 8) as usize], "({x}, {y})");
        }
    }
}

#[test]
fn part_of_a_row() {
    let (w, h) = (1920, 1080);
    let mut all = vec![0; w as usize];
    for y in [0, 1, 7, 500, 1079] {
        background(y, h, 0, &mut all);
        let mut part = vec![0; 101];
        background(y, h, 333, &mut part);
        assert_eq!(part[..], all[333..434], "row {y}");
    }
}

#[test]
fn the_gradient_has_no_bands() {
    let (w, h) = (1920u32, 1080u32);
    let mut rows = vec![0; (w * 8) as usize];
    for s in [16, 8, 0] {
        let (t, b) = (((TOP >> s) & 0xFF) as f64, ((BOTTOM >> s) & 0xFF) as f64);
        let exact = |y: u32| t + (b - t) * (y as f64 + 0.5) / h as f64;
        let channel = |px: u32| ((px >> s) & 0xFF) as f64;
        for top in (0..h).step_by(8) {
            for (i, out) in rows.chunks_exact_mut(w as usize).enumerate() {
                background(top + i as u32, h, 0, out);
            }
            // Every pixel is one of the two steps around the gradient...
            for (i, out) in rows.chunks_exact(w as usize).enumerate() {
                let v = exact(top + i as u32);
                for &px in out {
                    let c = channel(px);
                    assert!(c == v.floor() || c == v.floor() + 1.0, "row {}: {c} for {v}", top + i as u32);
                }
            }
            // ...in the proportion the gradient falls between them, over
            // every 8x8 square (to a sixteenth of a step: undithered, it
            // would be off by up to half a step, in bands).
            let mean_exact = (top..top + 8).map(exact).sum::<f64>() / 8.0;
            for x in (0..w as usize).step_by(8) {
                let square = rows.chunks_exact(w as usize).flat_map(|r| &r[x..x + 8]);
                let mean = square.map(|&px| channel(px)).sum::<f64>() / 64.0;
                assert!((mean - mean_exact).abs() <= 1.0 / 16.0, "rows {top}..: {mean} for {mean_exact}");
            }
        }
    }
}

#[test]
fn the_ring() {
    // 1280x800: 66 pixels across, centred 50 pixels above the middle.
    let r = Ring::place(1280, 800);
    assert_eq!((r.cx / 16, r.cy / 16, r.outer / 16, r.inner), (640, 350, 66, 821));
    // Solid inside the band, nothing in the hole or beyond it.
    assert_eq!(r.coverage(640, 350 - 60), 256);
    assert_eq!(r.coverage(640, 350), 0);
    assert_eq!(r.coverage(640, 350 - 70), 0);
    // The largest is 96 pixels, the smallest 28.
    assert_eq!(Ring::place(3840, 2400).outer, 96 * 16);
    assert_eq!(Ring::place(320, 200).outer, 28 * 16);
    let big = r.scaled(5, 4);
    assert_eq!((big.outer, big.inner, big.cx), (r.outer * 5 / 4, r.inner * 5 / 4, r.cx));
}

#[test]
fn the_gradient() {
    // From the top colour to the bottom one.
    let mut row = [0; 8];
    background(0, 800, 0, &mut row);
    assert_eq!(row, [TOP; 8]);
    background(799, 800, 0, &mut row);
    assert_eq!(row, [BOTTOM; 8]);
    assert_eq!(mix(0x000000, 0xFFFFFF, 128), 0x7F7F7F);
    assert_eq!(isqrt(1 << 20), 1 << 10);
    assert_eq!(isqrt(99), 9);
}
