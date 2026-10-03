//! `vgfx` — Vindows 2D graphics.
//!
//! * [`Canvas`]: drawing on a borrowed buffer of premultiplied `0xAARRGGBB`
//!   pixels — rectangles, gradients, anti-aliased rounded rectangles and
//!   circles, vector paths (through `vraster`), coverage masks, bitmaps
//!   (with scaling) — all clipped and translated.
//! * [`Bitmap`]: owned images with resizing and blurring.
//! * [`ShadowTemplate`]: fast soft shadows.
//! * [`Text`]: fonts and cached glyph rendering (through `vfont`).
//!
//! Everything is integer- and cache-friendly because Vindows usually runs
//! under CPU emulation.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bitmap;
pub mod canvas;
pub mod color;
pub mod geom;
pub mod shadow;
pub mod text;

pub use bitmap::Bitmap;
pub use canvas::{Canvas, Filter};
pub use color::Color;
pub use geom::{Damage, Rect};
pub use shadow::ShadowTemplate;
pub use text::{Align, LineMetrics, Text};
pub use vraster::{FillRule, LineCap, LineJoin, Path, StrokeStyle, Transform};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_rectangles_intersect_to_canonical_empty() {
        let a = Rect::new(0, 0, 100, 100);
        assert_eq!(a.intersect(&Rect::new(1960, 10, 50, 50)), Rect::default());
        assert!(!a.intersects(&Rect::new(-60, 0, 50, 50)));
        assert_eq!(a.intersect(&Rect::new(50, 50, 100, 100)), Rect::new(50, 50, 50, 50));
    }

    #[test]
    fn drawing_entirely_outside_the_canvas_is_a_no_op() {
        // Regression: a fill far right of the clip used to slice past a row.
        let mut b = Bitmap::new(16, 8);
        let mut c = Canvas::for_bitmap(&mut b);
        c.fill_rect(Rect::new(40, 2, 10, 3), Color::WHITE);
        c.fill_rect(Rect::new(-40, 2, 10, 3), Color::WHITE);
        c.fill_rounded_rect(Rect::new(30, -20, 10, 10), 3.0, Color::WHITE);
        c.draw_shadow(Rect::new(60, 60, 20, 20), 4, 6, Color::BLACK);
        let src = Bitmap::filled(4, 4, 0xFFFF_FFFF);
        c.draw_bitmap(&src, 100, 0, 255);
        c.blit(&src, src.rect(), -50, -50);
        drop(c);
        assert!(b.pixels.iter().all(|&p| p == 0));
    }

    #[test]
    fn scaled_drawing_samples_pixel_centres() {
        let src = Bitmap::from_straight(2, 1, alloc::vec![0xFF00_0000, 0xFFFF_FFFF]);
        let scaled = |filter| {
            let mut b = Bitmap::new(4, 1);
            Canvas::for_bitmap(&mut b).draw_bitmap_scaled(&src, Rect::new(0, 0, 4, 1), filter, 255);
            b.pixels
        };
        assert_eq!(scaled(Filter::Nearest), [0xFF00_0000, 0xFF00_0000, 0xFFFF_FFFF, 0xFFFF_FFFF]);
        // Bilinear: edges clamp; inner pixels are a quarter and three
        // quarters of the way from black to white (255 × ¼ and 255 × ¾,
        // truncated by the 8-bit blend).
        let p = scaled(Filter::Bilinear);
        assert_eq!((p[0], p[3]), (0xFF00_0000, 0xFFFF_FFFF));
        assert_eq!((p[1] & 0xFF, p[2] & 0xFF), (63, 191));
    }

    #[test]
    fn fill_and_clip() {
        let mut b = Bitmap::new(20, 10);
        let mut c = Canvas::for_bitmap(&mut b);
        c.clip_to(Rect::new(5, 0, 10, 10));
        c.fill_rect(Rect::new(0, 0, 20, 10), Color::rgb(255, 0, 0));
        assert_eq!(b.get(4, 5), 0);
        assert_eq!(b.get(5, 5), 0xFFFF_0000);
        assert_eq!(b.get(14, 5), 0xFFFF_0000);
        assert_eq!(b.get(15, 5), 0);
    }

    #[test]
    fn rounded_rect_corners_are_antialiased() {
        let mut b = Bitmap::new(40, 40);
        let mut c = Canvas::for_bitmap(&mut b);
        c.fill_rounded_rect(Rect::new(0, 0, 40, 40), 10.0, Color::WHITE);
        assert_eq!(b.get(0, 0) >> 24, 0, "corner outside the arc");
        assert_eq!(b.get(20, 20), 0xFFFF_FFFF, "interior");
        assert_eq!(b.get(20, 0), 0xFFFF_FFFF, "straight top edge");
        let partial = b.get(3, 2) >> 24;
        assert!(partial > 0 && partial < 255, "arc pixel is partially covered: {partial}");
    }

    #[test]
    fn shadow_is_symmetric_and_fades_out() {
        let mut b = Bitmap::new(100, 100);
        let mut c = Canvas::for_bitmap(&mut b);
        ShadowTemplate::new(8, 10).draw(&mut c, Rect::new(20, 20, 60, 60), Color::BLACK, true);
        assert_eq!(b.get(0, 0) >> 24, 0);
        assert_eq!(b.get(50, 50) >> 24, 255);
        assert_eq!(b.get(15, 50) >> 24, b.get(84, 50) >> 24);
        assert_eq!(b.get(50, 15) >> 24, b.get(50, 84) >> 24);
        assert!(b.get(50, 12) >> 24 < b.get(50, 22) >> 24);
    }

    #[test]
    fn damage_merges_overlaps() {
        let mut d = Damage::new();
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(5, 5, 10, 10));
        d.add(Rect::new(100, 100, 5, 5));
        assert_eq!(d.rects().len(), 2);
        assert_eq!(d.bounds(), Rect::new(0, 0, 105, 105));
    }
}
