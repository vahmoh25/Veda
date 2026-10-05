//! Player-specific widgets drawn on top of `vui`.

use vgfx::{Color, Rect};
use vmath::FloatExt;
use vui::{Cursor, Icon, Ui};

/// A seek bar: a rounded track, the played part in `accent`, a knob when
/// hovered or dragged. `fraction` is the current position; `drag` holds the
/// fraction while the user drags. Returns the fraction to seek to when the
/// button is released.
pub fn seek_bar(ui: &mut Ui, r: Rect, fraction: f32, accent: Color, drag: &mut Option<f32>) -> Option<f32> {
    let id = ui.id("seek-bar");
    let hit = r.inset(0, -8, 0, -8);
    let resp = ui.interact(id, hit);
    let under = ui.input.pointer.map(|(px, _)| ((px - r.x) as f32 / r.w.max(1) as f32).clamp(0.0, 1.0));
    let mut commit = None;
    if resp.pressed || resp.held {
        *drag = under.or(*drag);
    } else if let Some(f) = drag.take() {
        commit = Some(under.unwrap_or(f));
    }
    // On release the bar shows where the song is going already.
    let f = commit.or(*drag).unwrap_or(fraction).clamp(0.0, 1.0);
    if resp.hovered {
        ui.set_cursor(Cursor::Hand);
    }
    let active = resp.hovered || drag.is_some();
    let h = if active { 6 } else { 4 };
    let cy = r.y + r.h / 2;
    let track = Rect::new(r.x, cy - h / 2, r.w, h);
    ui.canvas.fill_rounded_rect(track, h as f32 / 2.0, Color::rgba(255, 255, 255, 38));
    let w = (r.w as f32 * f) as i32;
    if w > 0 {
        ui.canvas.save();
        ui.canvas.clip_to(Rect::new(r.x, track.y - 1, w, h + 2));
        let filled = Rect::new(r.x, track.y, r.w, h);
        ui.canvas.fill_rounded_rect(filled, h as f32 / 2.0, accent);
        ui.canvas.restore();
    }
    if active {
        let kx = r.x as f32 + r.w as f32 * f;
        ui.canvas.fill_circle(kx, cy as f32 + 0.5, 8.0, Color::rgba(0, 0, 0, 60));
        ui.canvas.fill_circle(kx, cy as f32, 7.0, Color::WHITE);
    }
    commit
}

/// The big round play/pause button. Returns true when clicked.
pub fn play_button(ui: &mut Ui, center: (i32, i32), radius: i32, playing: bool, accent: Color, accent2: Color) -> bool {
    let r = Rect::new(center.0 - radius, center.1 - radius, radius * 2, radius * 2);
    let id = ui.id("play-button");
    let resp = ui.interact(id, r);
    let hover = ui.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 8.0);
    let grow = hover * 2.0 - if resp.held { 2.0 } else { 0.0 };
    let rr = radius as f32 + grow;
    let (cx, cy) = (center.0 as f32, center.1 as f32);
    ui.canvas.fill_circle(cx, cy + 3.0, rr + 2.0, Color::rgba(0, 0, 0, 70));
    let top = accent.lerp(Color::WHITE, 0.12 + hover * 0.08);
    let rect = Rect::new((cx - rr) as i32, (cy - rr) as i32, (rr * 2.0) as i32, (rr * 2.0) as i32);
    ui.canvas.fill_rounded_rect_gradient(rect, rr, top, accent2);
    let icon = if playing { Icon::Pause } else { Icon::Play };
    // The play triangle looks centred when nudged right a little.
    let nudge = if playing { 0 } else { 2 };
    icon.draw(&mut ui.canvas, rect.translate(nudge, 0), radius as f32 * 0.85, Color::WHITE);
    if resp.hovered {
        ui.set_cursor(Cursor::Hand);
        ui.tooltip(id, r, if playing { "Pause (Space)" } else { "Play (Space)" });
    }
    resp.clicked
}

/// A borderless icon button that can be toggled on (drawn in `accent`
/// with a dot underneath). Returns true when clicked.
pub fn toggle_icon(
    ui: &mut Ui,
    r: Rect,
    icon: Icon,
    on: bool,
    badge: Option<&str>,
    tooltip: &str,
    accent: Color,
) -> bool {
    let id = ui.id(tooltip) ^ 0x7091;
    let resp = ui.interact(id, r);
    let hover = ui.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 8.0);
    if hover > 0.0 {
        ui.canvas.fill_rounded_rect(r, r.h as f32 / 2.0, Color::rgba(255, 255, 255, (hover * 22.0) as u8));
    }
    let base = if on { accent } else { Color::rgba(236, 236, 241, 200) };
    let color = base.lerp(Color::WHITE, hover * 0.25);
    icon.draw(&mut ui.canvas, r, 20.0, color);
    if on {
        let (cx, _) = r.center();
        ui.canvas.fill_circle(cx as f32, (r.bottom() - 3) as f32, 2.0, accent);
    }
    if let Some(b) = badge {
        let br = Rect::new(r.right() - 15, r.y + 4, 12, 12);
        ui.canvas.fill_rounded_rect(br, 6.0, accent);
        let size = 9.0;
        ui.label(br, b, vui::Font::Bold, size, Color::rgb(20, 20, 26), vgfx::Align::Center);
    }
    if resp.hovered {
        ui.set_cursor(Cursor::Hand);
        ui.tooltip(id, r, tooltip);
    }
    resp.clicked
}

/// A small three-bar "now playing" equaliser glyph, animated by `t`
/// (seconds) when `moving`.
pub fn eq_glyph(ui: &mut Ui, r: Rect, t: f32, moving: bool, color: Color) {
    let bw = 3;
    let gap = 2;
    let total = bw * 3 + gap * 2;
    let x0 = r.x + (r.w - total) / 2;
    let base = r.y + r.h / 2 + 7;
    for i in 0..3 {
        let phase = i as f32 * 1.7;
        let h = if moving {
            4.0 + 9.0 * (0.5 + 0.5 * FloatExt::sin(t * (5.0 + i as f32 * 1.3) + phase))
        } else {
            4.0 + i as f32 * 2.0
        };
        let h = h as i32;
        ui.canvas.fill_rounded_rect(Rect::new(x0 + i * (bw + gap), base - h, bw, h), 1.0, color);
    }
}
