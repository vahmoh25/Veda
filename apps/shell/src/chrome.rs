//! Shared styling of shell surfaces: translucent "mica" backgrounds sampled
//! from the blurred wallpaper, and rounded window corners.

use vgfx::{Bitmap, Canvas, Color, Rect};
use vmath::FloatExt;

/// Corner radius of popups (start menu, calendar, notifications).
pub const POPUP_RADIUS: i32 = 12;

/// Fills the canvas with the blurred wallpaper as seen through a surface whose
/// top-left corner is at `origin` on screen, tinted with `tint`.
pub fn mica(c: &mut Canvas, blurred: &Bitmap, origin: (i32, i32), tint: Color) {
    let r = Rect::new(0, 0, c.width(), c.height());
    c.blit(blurred, Rect::new(origin.0, origin.1, r.w, r.h), 0, 0);
    c.fill_rect(r, tint);
}

/// Standard popup background: mica, a hairline border and rounded corners.
/// Call [`round_corners`] after drawing the content.
pub fn popup_background(c: &mut Canvas, blurred: &Bitmap, origin: (i32, i32)) {
    mica(c, blurred, origin, Color::rgba(24, 24, 32, 222));
}

/// Finishes a popup: border and transparent, anti-aliased corners.
pub fn popup_frame(c: &mut Canvas) {
    let r = Rect::new(0, 0, c.width(), c.height());
    c.stroke_rounded_rect(r, POPUP_RADIUS as f32, 1.0, Color::rgba(255, 255, 255, 30));
    round_corners(c, POPUP_RADIUS);
}

/// Makes everything outside rounded corners of `radius` transparent.
pub fn round_corners(c: &mut Canvas, radius: i32) {
    let (w, h) = (c.width(), c.height());
    let radius = radius.min(w / 2).min(h / 2);
    let (pixels, stride) = c.pixels_mut();
    let r = radius as f32;
    for cy in 0..radius {
        for cx in 0..radius {
            let dx = r - (cx as f32 + 0.5);
            let dy = r - (cy as f32 + 0.5);
            let cov = ((r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0) * 255.0) as u32;
            if cov >= 255 {
                continue;
            }
            for (x, y) in [(cx, cy), (w - 1 - cx, cy), (cx, h - 1 - cy), (w - 1 - cx, h - 1 - cy)] {
                let p = &mut pixels[(y * stride + x) as usize];
                *p = vgfx::color::scale(*p, cov);
            }
        }
    }
}

/// Text with a soft dark shadow, legible on any wallpaper.
pub fn shadowed_label(ui: &mut vui::Ui, r: Rect, text: &str, size: f32, align: vgfx::Align) {
    let font = ui.ctx.font(vui::Font::Regular);
    ui.ctx.text.draw_in(&mut ui.canvas, font, size, r.translate(0, 1), text, Color::rgba(0, 0, 0, 150), align);
    ui.ctx.text.draw_in(&mut ui.canvas, font, size, r, text, Color::WHITE, align);
}
