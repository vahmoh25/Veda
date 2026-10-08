//! Drawing of window decorations, client surfaces and the mouse cursor:
//! by the processor into a canvas, or by the GPU (`gpu`), which draws the
//! same shapes from the same formulas and takes what is text or a drawing
//! (title bars, cursors, the switcher) from bitmaps drawn here.

use alloc::vec::Vec;

use vgfx::color::{over, scale};
use vgfx::{Bitmap, Canvas, Color, FillRule, Path, Rect, ShadowTemplate, StrokeStyle, Text};
use vmath::FloatExt;
use vproto::display::{Cursor, WindowKind, WindowState};

use crate::gpu::{Gpu, Slot, Texture, rgba};
use crate::switcher::Switcher;
use crate::window::{CORNER_RADIUS, Part, TITLE_HEIGHT, Window};

/// Decorated windows' shadows: blurred this far around their frame, which
/// they are below by this much.
const SHADOW_BLUR: i32 = 22;
const SHADOW_DROP: i32 = 6;
/// Popups' and notifications' shadows: their corners' radius, the blur,
/// how far below, and their colour.
const POPUP_RADIUS: i32 = 12;
const POPUP_BLUR: i32 = 18;
const POPUP_DROP: i32 = 8;
const POPUP_SHADOW: Color = Color::rgba(0, 0, 0, 120);

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
            shadow: ShadowTemplate::new(CORNER_RADIUS, SHADOW_BLUR),
            popup_shadow: ShadowTemplate::new(POPUP_RADIUS, POPUP_BLUR),
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
            WindowKind::Normal if w.decorated() => {
                let maximized = w.state == WindowState::Maximized;
                let radius = if maximized { 0 } else { CORNER_RADIUS };
                if !maximized {
                    let sc = if focused { theme::SHADOW } else { theme::SHADOW_INACTIVE };
                    self.shadow.draw(c, frame.translate(0, SHADOW_DROP), sc.fade(opacity), op < 255);
                }
                let title = Rect::new(frame.x, frame.y, frame.w, TITLE_HEIGHT);
                self.draw_title_bar(c, w, title, focused, st, opacity);
                self.draw_client(c, w, client, op, radius);
                if !maximized {
                    let bc = if focused { theme::BORDER } else { theme::BORDER_INACTIVE };
                    c.stroke_rounded_rect(frame, radius as f32, 1.0, bc.fade(opacity));
                }
            }
            WindowKind::Popup | WindowKind::Notification => {
                self.popup_shadow.draw(c, frame.translate(0, POPUP_DROP), POPUP_SHADOW.fade(opacity), true);
                self.draw_client(c, w, client, op, 0);
            }
            _ => self.draw_client(c, w, client, op, 0),
        }
    }

    /// A decorated window's title bar in `title`: its background, rounded
    /// at the top unless the window is maximised, its title and buttons.
    fn draw_title_bar(&mut self, c: &mut Canvas, w: &Window, title: Rect, focused: bool, st: DecorState, opacity: f32) {
        let radius = if w.state == WindowState::Maximized { 0 } else { CORNER_RADIUS };
        let tc = if focused { theme::TITLE_ACTIVE } else { theme::TITLE_INACTIVE };
        c.save();
        c.clip_to(title);
        c.fill_rounded_rect(
            Rect::new(title.x, title.y, title.w, TITLE_HEIGHT + radius * 2),
            radius as f32,
            tc.fade(opacity),
        );
        c.restore();
        self.draw_title_content(c, w, title, focused, st, opacity);
    }

    /// A decorated window's title bar alone, as [`Decor::draw_window`]
    /// draws it (for the GPU's texture).
    fn title_bitmap(&mut self, w: &Window, focused: bool, st: DecorState) -> Bitmap {
        let f = w.frame();
        let mut b = Bitmap::new(f.w.max(1), TITLE_HEIGHT);
        let mut c = Canvas::for_bitmap(&mut b);
        c.translate(-f.x, -f.y);
        self.draw_title_bar(&mut c, w, Rect::new(f.x, f.y, f.w, TITLE_HEIGHT), focused, st, 1.0);
        drop(c);
        b
    }

    /// Draws a complete window with the GPU, as [`Decor::draw_window`]
    /// does: its shadow and border by shaders, its title bar from a texture
    /// drawn here (again when it changes), its client's pixels from
    /// `content`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_window_gpu(
        &mut self,
        g: &mut Gpu,
        w: &Window,
        focused: bool,
        st: DecorState,
        opacity: f32,
        dy: i32,
        content: Option<Texture>,
    ) {
        let opacity = opacity.clamp(0.0, 1.0);
        if (opacity * 255.0) as u32 == 0 {
            return;
        }
        let frame = w.frame().translate(0, dy);
        let client = w.client_rect.translate(0, dy);
        match w.kind {
            WindowKind::Normal if w.decorated() => {
                let maximized = w.state == WindowState::Maximized;
                let radius = if maximized { 0 } else { CORNER_RADIUS };
                if !maximized {
                    let sc = if focused { theme::SHADOW } else { theme::SHADOW_INACTIVE };
                    let r = frame.translate(0, SHADOW_DROP);
                    let hollow = (opacity * 255.0) as u32 >= 255;
                    g.shadow(r, CORNER_RADIUS as f32, SHADOW_BLUR, rgba(sc.fade(opacity).premul()), hollow);
                }
                let key = title_key(w, focused, st);
                let t = match g.cached(Slot::Title(w.id), key) {
                    Some(t) => t,
                    None => {
                        let b = self.title_bitmap(w, focused, st);
                        g.upload(Slot::Title(w.id), key, &b)
                    }
                };
                let title = Rect::new(frame.x, frame.y, frame.w, TITLE_HEIGHT);
                g.image(t, Rect::new(0, 0, title.w, title.h), title, opacity, false, None);
                draw_client_gpu(g, w, client, opacity, radius, content);
                if !maximized {
                    let bc = if focused { theme::BORDER } else { theme::BORDER_INACTIVE };
                    g.stroke(frame, radius as f32, 1.0, rgba(bc.fade(opacity).premul()));
                }
            }
            WindowKind::Popup | WindowKind::Notification => {
                let r = frame.translate(0, POPUP_DROP);
                g.shadow(r, POPUP_RADIUS as f32, POPUP_BLUR, rgba(POPUP_SHADOW.fade(opacity).premul()), false);
                draw_client_gpu(g, w, client, opacity, 0, content);
            }
            _ => draw_client_gpu(g, w, client, opacity, 0, content),
        }
    }

    /// The switcher alone, as [`Decor::draw_switcher`] draws it: a bitmap
    /// of `s.bounds()` (for the GPU's texture).
    pub fn switcher_bitmap(&mut self, s: &Switcher, titles: &[&str]) -> Bitmap {
        let b = s.bounds();
        let mut bitmap = Bitmap::new(b.w.max(1), b.h.max(1));
        let mut c = Canvas::for_bitmap(&mut bitmap);
        c.translate(-b.x, -b.y);
        self.draw_switcher(&mut c, s, titles);
        drop(c);
        bitmap
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
        // Opaque windows (not fading) are copied row by row, except for
        // the anti-aliased rounded corners.
        let copy = w.opaque() && opacity == 255;
        for y in vis.y..vis.bottom() {
            let sy = y - dst.y;
            let srow = &src[(sy * buf.stride) as usize..];
            let drow = ((y - b.y) * stride - b.x) as isize;
            let in_corner_rows = radius > 0 && y >= corner_top;
            if copy && !in_corner_rows {
                let (from, to, n) = ((vis.x - dst.x) as usize, (drow + vis.x as isize) as usize, vis.w as usize);
                pixels[to..to + n].copy_from_slice(&srow[from..from + n]);
                continue;
            }
            for x in vis.x..vis.right() {
                let sx = x - dst.x;
                let mut px = srow[sx as usize];
                if copy {
                    px |= 0xFF00_0000;
                }
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

/// Draws a window's client area with the GPU, as `Decor::draw_client`
/// does: its pixels from `content`, the bottom corners rounded by `radius`,
/// at `opacity`; where the client has not drawn (yet, or since it was made
/// larger), the placeholder.
fn draw_client_gpu(g: &mut Gpu, w: &Window, dst: Rect, opacity: f32, radius: i32, content: Option<Texture>) {
    let background = rgba(theme::CLIENT_BACKGROUND.premul());
    let (Some(buf), Some(_), Some(t)) = (&w.buffers, w.current, content) else {
        if w.decorated() {
            g.fill(dst, 0.0, background);
        }
        return;
    };
    if w.decorated() && (buf.width < dst.w || buf.height < dst.h) {
        g.fill(Rect::new(dst.x + buf.width, dst.y, dst.w - buf.width, dst.h), 0.0, background);
        g.fill(Rect::new(dst.x, dst.y + buf.height, buf.width.min(dst.w), dst.h - buf.height), 0.0, background);
    }
    let vis = dst.intersect(&Rect::new(dst.x, dst.y, buf.width, buf.height));
    let round = (radius > 0).then_some((dst, radius as f32));
    g.image(t, Rect::new(0, 0, vis.w, vis.h), vis, opacity, w.opaque(), round);
}

/// What a decorated window's title bar is drawn from: its texture is
/// drawn again when this changes (never 0).
fn title_key(w: &Window, focused: bool, st: DecorState) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    let mut eat = |v: u64| h = (h ^ v).wrapping_mul(0x0000_0100_0000_01B3);
    for b in w.title.bytes() {
        eat(u64::from(b));
    }
    let part = |p: Option<(u32, Part)>| match p.filter(|(id, _)| *id == w.id).map(|(_, p)| p) {
        None => 0,
        Some(Part::Client) => 1,
        Some(Part::Title) => 2,
        Some(Part::Minimize) => 3,
        Some(Part::Maximize) => 4,
        Some(Part::Close) => 5,
        Some(Part::Edge(..)) => 6,
    };
    eat(w.frame().w as u64);
    eat(u64::from(focused) | u64::from(w.resizable) << 1 | u64::from(w.state == WindowState::Maximized) << 2);
    eat(part(st.hover) | part(st.pressed) << 8);
    h | 1
}

/// The translucent rectangle showing where a dragged window will snap.
pub fn draw_snap_preview(c: &mut Canvas, r: Rect) {
    let r = r.inset(8, 8, 8, 8);
    c.fill_rounded_rect(r, 12.0, Color::rgba(91, 140, 255, 46));
    c.stroke_rounded_rect(r, 12.0, 2.0, Color::rgba(150, 185, 255, 170));
}

/// [`draw_snap_preview`] with the GPU.
pub fn draw_snap_preview_gpu(g: &mut Gpu, r: Rect) {
    let r = r.inset(8, 8, 8, 8);
    g.fill(r, 12.0, rgba(Color::rgba(91, 140, 255, 46).premul()));
    g.stroke(r, 12.0, 2.0, rgba(Color::rgba(150, 185, 255, 170).premul()));
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

/// [`draw_background`] with the GPU.
pub fn draw_background_gpu(g: &mut Gpu, screen: Rect) {
    g.gradient(screen, rgba(theme::DESKTOP_TOP.premul()), rgba(theme::DESKTOP_BOTTOM.premul()));
}
