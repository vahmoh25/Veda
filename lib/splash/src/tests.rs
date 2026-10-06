use std::vec;
use std::vec::Vec;

use crate::*;

/// The splash as the boot loader painted it before this crate existed,
/// kept here as the reference: the picture must not change, or the
/// handover to the window system would show.
fn reference(w: u32, h: u32) -> Vec<u32> {
    let mut out = vec![0; (w * h) as usize];
    let (top, bottom) = (0x070A16, 0x121A35);
    let outer = (h / 12).clamp(28, 96) as i32 * 16;
    let inner = outer * 7 / 9;
    let cx = w as i32 * 8;
    let cy = (h as i32 / 2 - h as i32 / 16) * 16;
    for y in 0..h {
        let bg_row = mix(top, bottom, y * 256 / h.max(1));
        let dy = y as i32 * 16 + 8 - cy;
        for x in 0..w {
            let mut c = bg_row;
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

fn drawn(w: u32, h: u32) -> Vec<u32> {
    let mut out = vec![0; (w * h) as usize];
    draw(w, h, |x, y, c| out[(y * w + x) as usize] = c);
    out
}

#[test]
fn the_same_picture_as_the_loader_painted() {
    for (w, h) in [(1280, 800), (1920, 1080), (800, 600), (2560, 1600), (320, 200)] {
        assert!(drawn(w, h) == reference(w, h), "{}x{}", w, h);
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
    assert_eq!(background(0, 800), TOP);
    assert_eq!(background(800, 800), BOTTOM);
    assert_eq!(mix(0x000000, 0xFFFFFF, 128), 0x7F7F7F);
    assert_eq!(isqrt(1 << 20), 1 << 10);
    assert_eq!(isqrt(99), 9);
}
