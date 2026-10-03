//! Drawing of window decorations, client surfaces and the mouse cursor.

use alloc::vec::Vec;

use vgfx::color::{over, scale};
use vgfx::{Bitmap, Canvas, Color, FillRule, Path, Rect, ShadowTemplate, StrokeStyle, Text};
use vmath::FloatExt;
use vproto::display::Cursor;

use crate::switcher::Switcher;
use crate::window::{CORNER_RADIUS, Part, TITLE_HEIGHT, Window};

/// Visual theme of the window system (dark).
pub mod theme {
    use vgfx::Color;
    pub const DESKTOP_TOP: Color = Color::hex(0x0B1022);
    pub const DESKTOP_BOTTOM: Color = Color::hex(0x161C38);
    pub const TITLE_ACTIVE: Color = Color::hex(0x202026);
    pub const TITLE_INACTIVE: Color = Color::hex(0x1A1A1E);
    pub const TITLE_TEXT: Color = Color::hex(0xE8E8EE);
    pub const TITLE_TEXT_INACTIVE: Color = Color::hex(0x80808A);
    pub const ACCENT: Color = Color::hex(0x5B8CFF);
    pub const BORDER: Color = Color::rgba(255, 255, 255, 26);
    pub const BORDER_INACTIVE: Color = Color::rgba(255, 255, 255, 16);
    pub const BUTTON_HOVER: Color = Color::rgba(255, 255, 255, 24);
    pub const BUTTON_PRESSED: Color = Color::rgba(255, 255, 255, 14);
    pub const CLOSE_HOVER: Color = Color::hex(0xC42B1C);
    pub const GLYPH: Color = Color::hex(0xD4D4DA);
    pub const SHADOW: Color = Color::rgba(0, 0, 0, 150);
    pub const SHADOW_INACTIVE: Color = Color::rgba(0, 0, 0, 90);
    /// Placeholder fill while a client has not drawn yet.
    pub const CLIENT_BACKGROUND: Color = Color::hex(0x1C1C21);
}

pub struct Decor {
    shadow: ShadowTemplate,
    popup_shadow: ShadowTemplate,
    pub text: Text,
    pub title_font: usize,
    cursors: Vec<(Cursor, Bitmap, i32, i32)>,
}

/// Mouse-interaction state that affects how decorations look.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct DecorState {
    pub hover: Option<(u32, Part)>,
    pub pressed: Option<(u32, Part)>,
}

impl Decor {
    pub fn new(text: Text, title_font: usize) -> Decor {
        Decor {
            shadow: ShadowTemplate::new(CORNER_RADIUS, 22),
            popup_shadow: ShadowTemplate::new(12, 18),
            text,
            title_font,
            cursors: build_cursors(),
        }
    }

    pub fn cursor(&self, c: Cursor) -> Option<(&Bitmap, i32, i32)> {
        let c = if c == Cursor::Busy { Cursor::Arrow } else { c };
        self.cursors.iter().find(|(k, ..)| *k == c).or_else(|| self.cursors.first()).map(|(_, b, hx, hy)| (b, *hx, *hy))
    }

    /// Draws a complete window (shadow, decorations, client content).
    pub fn draw_window(&mut self, c: &mut Canvas, w: &Window, focused: bool, st: DecorState, opacity: f32, dy: i32) {
        let op = (opacity.clamp(0.0, 1.0) * 255.0) as u32;
        if op == 0 {
            return;
        }
        let frame = w.frame().translate(0, dy);
        let client = w.client_rect.translate(0, dy);
        match w.kind {
            vproto::display::WindowKind::Normal if w.decorated() => {
                let maximized = w.state == vproto::display::WindowState::Maximized;
                let radius = if maximized { 0 } else { CORNER_RADIUS };
                if !maximized {
                    let sc = if focused { theme::SHADOW } else { theme::SHADOW_INACTIVE };
                    self.shadow.draw(c, frame.translate(0, 6), sc.fade(opacity), op < 255);
                }
                // Title bar with rounded top corners.
                let tc = if focused { theme::TITLE_ACTIVE } else { theme::TITLE_INACTIVE };
                let title = Rect::new(frame.x, frame.y, frame.w, TITLE_HEIGHT);
                c.save();
                c.clip_to(title);
                c.fill_rounded_rect(
                    Rect::new(frame.x, frame.y, frame.w, TITLE_HEIGHT + radius * 2),
                    radius as f32,
                    tc.fade(opacity),
                );
                c.restore();
                self.draw_client(c, w, client, op, radius);
                self.draw_title_content(c, w, title, focused, st, opacity);
                if !maximized {
                    let bc = if focused { theme::BORDER } else { theme::BORDER_INACTIVE };
                    c.stroke_rounded_rect(frame, radius as f32, 1.0, bc.fade(opacity));
                }
            }
            vproto::display::WindowKind::Popup | vproto::display::WindowKind::Notification => {
                self.popup_shadow.draw(c, frame.translate(0, 8), Color::rgba(0, 0, 0, 120).fade(opacity), true);
                self.draw_client(c, w, client, op, 0);
            }
            _ => self.draw_client(c, w, client, op, 0),
        }
    }

    /// Draws the Alt+Tab switcher overlay; `titles` are the window titles in
    /// the switcher's order.
    pub fn draw_switcher(&mut self, c: &mut Canvas, s: &Switcher, titles: &[&str]) {
        let panel = s.rect;
        self.popup_shadow.draw(c, panel.translate(0, 10), Color::rgba(0, 0, 0, 150), true);
        c.fill_rounded_rect(panel, 16.0, Color::rgba(30, 30, 38, 242));
        c.stroke_rounded_rect(panel, 16.0, 1.0, Color::rgba(255, 255, 255, 34));
        for (i, title) in titles.iter().enumerate() {
            let cell = s.cell(i);
            let selected = i == s.selected;
            if selected {
                c.fill_rounded_rect(cell, 12.0, Color::rgba(255, 255, 255, 20));
                c.stroke_rounded_rect(cell, 12.0, 2.0, theme::ACCENT);
            }
            let bx = s.thumb_box(i);
            match s.thumbs.get(i).and_then(|t| t.as_ref()) {
                Some(b) => {
                    let r = Rect::new(bx.x + (bx.w - b.width) / 2, bx.y + (bx.h - b.height) / 2, b.width, b.height);
                    c.draw_shadow(r.translate(0, 3), 6, 10, Color::rgba(0, 0, 0, 110));
                    c.draw_bitmap(b, r.x, r.y, 255);
                    c.stroke_rounded_rect(r.inflate(1), 2.0, 1.0, Color::rgba(255, 255, 255, 30));
                }
                None => c.fill_rounded_rect(bx.inset(24, 12, 24, 12), 6.0, theme::CLIENT_BACKGROUND),
            }
            let font = self.title_font;
            let label = self.text.ellipsize(font, 13.0, title, (cell.w - 24) as f32);
            let color = if selected { theme::TITLE_TEXT } else { theme::TITLE_TEXT_INACTIVE };
            let tr = Rect::new(cell.x + 12, bx.bottom() + 10, cell.w - 24, 24);
            self.text.draw_in(c, font, 13.0, tr, &label, color, vgfx::Align::Center);
        }
    }

    fn draw_title_content(
        &mut self,
        c: &mut Canvas,
        w: &Window,
        title: Rect,
        focused: bool,
        st: DecorState,
        opacity: f32,
    ) {
        let text_color = if focused { theme::TITLE_TEXT } else { theme::TITLE_TEXT_INACTIVE }.fade(opacity);
        let buttons = if w.resizable { 3 } else { 2 };
        let text_rect =
            Rect::new(title.x + 16, title.y, title.w - 16 - buttons * crate::window::BUTTON_WIDTH - 8, TITLE_HEIGHT);
        let font = self.title_font;
        self.text.draw_in(c, font, 13.0, text_rect, &w.title, text_color, vgfx::Align::Left);
        let dy = title.y - w.title_rect().y;
        for part in [Part::Minimize, Part::Maximize, Part::Close] {
            if part == Part::Maximize && !w.resizable {
                continue;
            }
            let r = w.button_rect(part).translate(0, dy);
            let hovered = st.hover == Some((w.id, part));
            let pressed = st.pressed == Some((w.id, part));
            if hovered || pressed {
                let bg = match (part, pressed) {
                    (Part::Close, _) => theme::CLOSE_HOVER,
                    (_, true) => theme::BUTTON_PRESSED,
                    _ => theme::BUTTON_HOVER,
                };
                // Respect the window's rounded top-right corner.
                let radius = if part == Part::Close && w.state != vproto::display::WindowState::Maximized {
                    CORNER_RADIUS
                } else {
                    0
                };
                c.save();
                c.clip_to(r);
                if radius > 0 {
                    c.fill_rounded_rect(
                        Rect::new(r.x - radius, r.y, r.w + radius, r.h + radius),
                        radius as f32,
                        bg.fade(opacity),
                    );
                } else {
                    c.fill_rect(r, bg.fade(opacity));
                }
                c.restore();
            }
            let glyph = if part == Part::Close && hovered { Color::WHITE } else { theme::GLYPH }.fade(opacity);
            draw_button_glyph(c, part, r, glyph, w.state == vproto::display::WindowState::Maximized);
        }
    }

    /// Copies the client buffer into `dst`, rounding the bottom corners by
    /// `radius` and applying `opacity` (0..=255).
    fn draw_client(&mut self, c: &mut Canvas, w: &Window, dst: Rect, opacity: u32, radius: i32) {
        let (Some(buf), Some(index)) = (&w.buffers, w.current) else {
            if w.decorated() {
                c.fill_rect(dst, theme::CLIENT_BACKGROUND);
            }
            return;
        };
        let src = buf.pixels(index);
        let clip = c.clip_rect();
        let vis = dst.intersect(&clip).intersect(&Rect::new(dst.x, dst.y, buf.width, buf.height));
        // Areas of the client rect not covered by the buffer (mid-resize).
        if w.decorated() && (buf.width < dst.w || buf.height < dst.h) {
            c.fill_rect(Rect::new(dst.x + buf.width, dst.y, dst.w - buf.width, dst.h), theme::CLIENT_BACKGROUND);
            c.fill_rect(
                Rect::new(dst.x, dst.y + buf.height, buf.width.min(dst.w), dst.h - buf.height),
                theme::CLIENT_BACKGROUND,
            );
        }
        if vis.is_empty() {
            return;
        }
        let b = c.bounds();
        let (pixels, stride) = c.pixels_mut();
        let rad = radius as f32;
        let corner_top = dst.bottom() - radius;
        for y in vis.y..vis.bottom() {
            let sy = y - dst.y;
            let srow = &src[(sy * buf.stride) as usize..];
            let drow = ((y - b.y) * stride - b.x) as isize;
            let in_corner_rows = radius > 0 && y >= corner_top;
            for x in vis.x..vis.right() {
                let sx = x - dst.x;
                let mut px = srow[sx as usize];
                let mut cov = opacity;
                if in_corner_rows {
                    let cx = if x < dst.x + radius {
                        Some(dst.x as f32 + rad)
                    } else if x >= dst.right() - radius {
                        Some(dst.right() as f32 - rad)
                    } else {
                        None
                    };
                    if let Some(cx) = cx {
                        let fx = x as f32 + 0.5 - cx;
                        let fy = y as f32 + 0.5 - (dst.bottom() as f32 - rad);
                        let d = (fx * fx + fy * fy).sqrt();
                        cov = (cov as f32 * (rad - d + 0.5).clamp(0.0, 1.0)) as u32;
                    }
                }
                if cov == 0 {
                    continue;
                }
                if cov < 255 {
                    px = scale(px, cov);
                }
                let d = &mut pixels[(drow + x as isize) as usize];
                *d = if px >> 24 == 255 { px } else { over(px, *d) };
            }
        }
    }
}

fn draw_button_glyph(c: &mut Canvas, part: Part, r: Rect, color: Color, maximized: bool) {
    let (cx, cy) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0);
    let s = 5.0;
    let stroke = StrokeStyle::new(1.0);
    let mut p = Path::new();
    match part {
        Part::Minimize => {
            p.move_to(cx - s, cy + 0.5);
            p.line_to(cx + s, cy + 0.5);
        }
        Part::Maximize if maximized => {
            p.rounded_rect(cx - s + 0.5, cy - s + 2.5, 2.0 * s - 2.0, 2.0 * s - 2.0, [1.0; 4]);
            p.move_to(cx - s + 2.5, cy - s + 0.5);
            p.line_to(cx + s + 0.5, cy - s + 0.5);
            p.line_to(cx + s + 0.5, cy + s - 2.5);
        }
        Part::Maximize => p.rounded_rect(cx - s + 0.5, cy - s + 0.5, 2.0 * s, 2.0 * s, [1.5; 4]),
        Part::Close => {
            p.move_to(cx - s, cy - s);
            p.line_to(cx + s, cy + s);
            p.move_to(cx + s, cy - s);
            p.line_to(cx - s, cy + s);
        }
        _ => return,
    }
    c.stroke_path(&p, &stroke, color);
}

/// The translucent rectangle showing where a dragged window will snap.
pub fn draw_snap_preview(c: &mut Canvas, r: Rect) {
    let r = r.inset(8, 8, 8, 8);
    c.fill_rounded_rect(r, 12.0, Color::rgba(91, 140, 255, 46));
    c.stroke_rounded_rect(r, 12.0, 2.0, Color::rgba(150, 185, 255, 170));
}

/// Renders a cursor shape: white fill with a dark outline and a soft shadow.
fn render_cursor(path: &Path, w: i32, h: i32) -> Bitmap {
    let mut shape = Bitmap::new(w, h);
    {
        let mut c = Canvas::for_bitmap(&mut shape);
        c.fill_path(path, Color::BLACK, FillRule::NonZero);
    }
    let mut out = vgfx::shadow::silhouette_shadow(&shape, 3);
    // The shadow bitmap is padded by 6 px; shift it down-right by 1 px.
    let pad = 6;
    let mut final_bm = Bitmap::new(w + pad, h + pad);
    {
        let mut c = Canvas::for_bitmap(&mut final_bm);
        for p in out.pixels.iter_mut() {
            *p = scale(*p, 120);
        }
        c.draw_bitmap(&out, -pad + 1, -pad + 2, 255);
        let t = vgfx::Transform::IDENTITY;
        c.stroke_path_transformed(
            path,
            &StrokeStyle::new(2.0).with_join(vgfx::LineJoin::Round),
            &t,
            Color::rgba(20, 20, 26, 255),
        );
        c.fill_path(path, Color::WHITE, FillRule::NonZero);
    }
    final_bm
}

fn build_cursors() -> Vec<(Cursor, Bitmap, i32, i32)> {
    let mut v = Vec::new();
    // Arrow (hotspot at the tip).
    let mut arrow = Path::new();
    arrow.move_to(2.0, 2.0);
    arrow.line_to(2.0, 19.0);
    arrow.line_to(6.2, 15.0);
    arrow.line_to(9.0, 21.0);
    arrow.line_to(11.6, 19.9);
    arrow.line_to(8.9, 14.1);
    arrow.line_to(14.6, 14.1);
    arrow.close();
    v.push((Cursor::Arrow, render_cursor(&arrow, 20, 26), 2, 2));
    // Text I-beam.
    let mut beam = Path::new();
    beam.rect(4.0, 3.0, 6.0, 1.6);
    beam.rect(6.2, 3.0, 1.6, 17.0);
    beam.rect(4.0, 18.4, 6.0, 1.6);
    v.push((Cursor::Text, render_cursor(&beam, 16, 24), 7, 11));
    // Pointing hand.
    let mut hand = Path::new();
    hand.rounded_rect(6.0, 2.0, 4.0, 12.0, [2.0; 4]);
    hand.rounded_rect(3.0, 10.0, 14.0, 10.0, [4.0, 4.0, 5.0, 5.0]);
    hand.rounded_rect(10.0, 7.5, 3.6, 6.0, [1.8; 4]);
    hand.rounded_rect(13.4, 8.5, 3.6, 5.0, [1.8; 4]);
    hand.rounded_rect(2.2, 11.5, 3.6, 6.0, [1.8; 4]);
    v.push((Cursor::Hand, render_cursor(&hand, 22, 26), 8, 2));
    // Resize arrows.
    let double_arrow = |horizontal: bool| -> Path {
        let mut p = Path::new();
        let pts: [(f32, f32); 10] = [
            (0.0, 6.0),
            (5.0, 1.0),
            (5.0, 4.5),
            (13.0, 4.5),
            (13.0, 1.0),
            (18.0, 6.0),
            (13.0, 11.0),
            (13.0, 7.5),
            (5.0, 7.5),
            (5.0, 11.0),
        ];
        for (i, &(x, y)) in pts.iter().enumerate() {
            let (x, y) = if horizontal { (x + 2.0, y + 5.0) } else { (y + 5.0, x + 2.0) };
            if i == 0 { p.move_to(x, y) } else { p.line_to(x, y) }
        }
        p.close();
        p
    };
    v.push((Cursor::ResizeHorizontal, render_cursor(&double_arrow(true), 24, 18), 11, 11));
    v.push((Cursor::ResizeVertical, render_cursor(&double_arrow(false), 18, 24), 11, 11));
    let diag = |anti: bool| -> Path {
        let mut p = double_arrow(true);
        let angle = if anti { -core::f32::consts::FRAC_PI_4 } else { core::f32::consts::FRAC_PI_4 };
        p.transform(&vgfx::Transform::rotate_about(angle, 11.0, 11.0).then_translate(1.0, 1.0));
        p
    };
    v.push((Cursor::ResizeDiagonal, render_cursor(&diag(false), 24, 24), 12, 12));
    v.push((Cursor::ResizeAntiDiagonal, render_cursor(&diag(true), 24, 24), 12, 12));
    // Move: four-way.
    let mut mv = double_arrow(true);
    mv.append(&double_arrow(false));
    v.push((Cursor::Move, render_cursor(&mv, 24, 24), 11, 11));
    let mut cross = Path::new();
    cross.rect(10.0, 2.0, 2.0, 18.0);
    cross.rect(2.0, 10.0, 18.0, 2.0);
    v.push((Cursor::Crosshair, render_cursor(&cross, 24, 24), 11, 11));
    v.push((Cursor::Hidden, Bitmap::new(1, 1), 0, 0));
    v
}

/// Background shown where no desktop window covers the screen.
pub fn draw_background(c: &mut Canvas, screen: Rect) {
    c.fill_vertical_gradient(screen, theme::DESKTOP_TOP, theme::DESKTOP_BOTTOM);
}
