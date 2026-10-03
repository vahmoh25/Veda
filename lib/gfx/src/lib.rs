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
