//! Standard widgets.

use alloc::string::String;

use vgfx::{Align, Color, Rect};
use vproto::display::Cursor;
use vproto::input::keys;

use crate::icons::Icon;
use crate::theme::Font;
use crate::ui::{Id, Overlay, Ui};

/// Visual style of a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Accent-filled call to action.
    Primary,
    /// Neutral filled button.
    Secondary,
    /// No background until hovered (toolbars).
    Ghost,
    /// Red, for destructive actions.
    Danger,
}

/// Per-row information passed to list row painters.
#[derive(Debug, Clone, Copy)]
pub struct RowState {
    pub selected: bool,
    pub hovered: bool,
    pub focused: bool,
}

/// What happened in a list view this frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListResponse {
    /// Row clicked (or selected with the keyboard).
    pub clicked: Option<usize>,
    /// Row double-clicked or activated with Enter.
    pub activated: Option<usize>,
    /// Row right-clicked, with the pointer position.
    pub context: Option<(usize, i32, i32)>,
}

impl<'a> Ui<'a> {
    fn button_colors(&self, kind: ButtonKind, hover: f32, held: bool) -> (Color, Color) {
        let t = &self.ctx.theme;
        match kind {
            ButtonKind::Primary => {
                let base = if held { t.accent_pressed } else { t.accent.lerp(t.accent_hover, hover) };
                (base, t.on_accent)
            }
            ButtonKind::Secondary => {
                let base = if held { t.control_pressed } else { t.control.lerp(t.control_hover, hover) };
                (base, t.text)
            }
            ButtonKind::Ghost => {
                let a = if held { 18 } else { (hover * 26.0) as u8 };
                (Color::rgba(255, 255, 255, a), t.text)
            }
            ButtonKind::Danger => {
                let base = if held { t.danger.shade(-0.15) } else { t.danger.lerp(t.danger.shade(0.12), hover) };
                (base, Color::WHITE)
            }
        }
    }

    /// A button with an optional icon and label. Returns `true` when clicked.
    pub fn button_full(&mut self, r: Rect, icon: Option<Icon>, label: &str, kind: ButtonKind) -> bool {
        let id = self.id(label) ^ (r.x as u64) << 32 ^ r.y as u64;
        let resp = self.interact(id, r);
        let hover = self.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 8.0);
        let (bg, fg) = self.button_colors(kind, hover, resp.held);
        let radius = self.ctx.theme.radius;
        if bg.a() > 0 {
            if kind == ButtonKind::Primary && !resp.held {
                let top = bg.shade(0.06);
                self.canvas.fill_rounded_rect_gradient(r, radius, top, bg.shade(-0.04));
            } else {
                self.canvas.fill_rounded_rect(r, radius, bg);
            }
        }
        if kind == ButtonKind::Secondary {
            let border = self.ctx.theme.border;
            self.canvas.stroke_rounded_rect(r, radius, 1.0, border);
        }
        if resp.hovered {
            self.set_cursor(Cursor::Hand);
        }
        let size = self.ctx.theme.font_size;
        match (icon, label.is_empty()) {
            (Some(icon), true) => icon.draw(&mut self.canvas, r, 18.0, fg),
            (Some(icon), false) => {
                let tw = self.measure(label, Font::Bold, size) as i32;
                let total = 18 + 8 + tw;
                let x = r.x + (r.w - total) / 2;
                icon.draw(&mut self.canvas, Rect::new(x, r.y, 18, r.h), 18.0, fg);
                self.label(Rect::new(x + 26, r.y, tw + 4, r.h), label, Font::Bold, size, fg, Align::Left);
            }
            (None, _) => self.label(r, label, Font::Bold, size, fg, Align::Center),
        }
        resp.clicked
    }

    /// A neutral button.
    pub fn button(&mut self, r: Rect, label: &str) -> bool {
        self.button_full(r, None, label, ButtonKind::Secondary)
    }

    /// An accent-coloured button.
    pub fn primary_button(&mut self, r: Rect, label: &str) -> bool {
        self.button_full(r, None, label, ButtonKind::Primary)
    }

    /// A borderless icon button with a tooltip.
    pub fn icon_button(&mut self, r: Rect, icon: Icon, tooltip: &str) -> bool {
        let id = self.id(tooltip) ^ (r.x as u64) << 32 ^ r.y as u64 ^ 0x1c0;
        let resp = self.interact(id, r);
        let hover = self.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 8.0);
        let (bg, fg) = self.button_colors(ButtonKind::Ghost, hover, resp.held);
        if bg.a() > 0 {
            let radius = self.ctx.theme.radius;
            self.canvas.fill_rounded_rect(r, radius, bg);
        }
        let size = (r.w.min(r.h) as f32 * 0.55).clamp(14.0, 22.0);
        icon.draw(&mut self.canvas, r, size, fg);
        if resp.hovered {
            self.set_cursor(Cursor::Hand);
            self.tooltip(id, r, tooltip);
        }
        resp.clicked
    }

    /// Shows `text` below `anchor` after the pointer rests on it briefly.
    pub fn tooltip(&mut self, id: Id, anchor: Rect, text: &str) {
        if text.is_empty() {
            return;
        }
        let now = self.now();
        let since = match self.ctx.state.tooltip {
            Some((tid, t)) if tid == id => t,
            _ => {
                self.ctx.state.tooltip = Some((id, now));
                now
            }
        };
        if now - since < 600_000_000 {
            self.repaint_at(since + 600_000_000);
            return;
        }
        let size = self.ctx.theme.small_size;
        let w = self.measure(text, Font::Regular, size) as i32 + 18;
        let h = 26;
        let mut x = anchor.x + (anchor.w - w) / 2;
        x = x.clamp(4, (self.width - w - 4).max(4));
        let mut y = anchor.bottom() + 6;
        if y + h > self.height - 4 {
            y = anchor.y - h - 6;
        }
        self.overlays.push(Overlay::Tooltip { rect: Rect::new(x, y, w, h), text: String::from(text) });
    }

    /// A checkbox with a label. Returns `true` if toggled.
    pub fn checkbox(&mut self, r: Rect, label: &str, value: &mut bool) -> bool {
        let id = self.id(label) ^ 0xc4ec;
        let resp = self.interact(id, r);
        let t = self.ctx.theme.clone();
        let box_r = Rect::new(r.x, r.y + (r.h - 18) / 2, 18, 18);
        if *value {
            self.canvas.fill_rounded_rect(box_r, 4.0, t.accent);
            Icon::Check.draw(&mut self.canvas, box_r, 14.0, t.on_accent);
        } else {
            self.canvas.fill_rounded_rect(box_r, 4.0, if resp.hovered { t.control_hover } else { t.control });
            self.canvas.stroke_rounded_rect(box_r, 4.0, 1.0, t.border_strong);
        }
        self.label(Rect::new(r.x + 28, r.y, r.w - 28, r.h), label, Font::Regular, t.font_size, t.text, Align::Left);
        if resp.clicked {
            *value = !*value;
        }
        resp.clicked
    }

    /// An on/off switch. Returns `true` if toggled.
    pub fn toggle(&mut self, r: Rect, id_label: &str, value: &mut bool) -> bool {
        let id = self.id(id_label) ^ 0x7067;
        let track = Rect::new(r.x, r.y + (r.h - 22) / 2, 42, 22);
        let resp = self.interact(id, track);
        if resp.clicked {
            *value = !*value;
        }
        let k = self.animate(id, if *value { 1.0 } else { 0.0 }, 7.0);
        let t = self.ctx.theme.clone();
        let bg = t.control_hover.lerp(t.accent, k);
        self.canvas.fill_rounded_rect(track, 11.0, bg);
        let knob_x = track.x as f32 + 11.0 + k * 20.0;
        self.canvas.fill_circle(knob_x, track.y as f32 + 11.0, 8.0, Color::WHITE);
        if resp.hovered {
            self.set_cursor(Cursor::Hand);
        }
        resp.clicked
    }

    /// A horizontal slider. Returns `true` while the value changes.
    pub fn slider(&mut self, r: Rect, id_label: &str, value: &mut f32, min: f32, max: f32) -> bool {
        let id = self.id(id_label) ^ 0x511d;
        let resp = self.interact(id, r);
        let t = self.ctx.theme.clone();
        let mut changed = false;
        let span = (max - min).max(f32::EPSILON);
        if resp.held || resp.pressed {
            if let Some((px, _)) = self.input.pointer {
                let f = ((px - r.x) as f32 / r.w.max(1) as f32).clamp(0.0, 1.0);
                let nv = min + f * span;
                if (nv - *value).abs() > f32::EPSILON {
                    *value = nv;
                    changed = true;
                }
            }
        }
        if resp.hovered && self.input.scroll.1 != 0 {
            *value = (*value + self.input.scroll.1 as f32 * span * 0.05).clamp(min, max);
            changed = true;
        }
        let f = ((*value - min) / span).clamp(0.0, 1.0);
        let cy = r.y + r.h / 2;
        let track = Rect::new(r.x, cy - 2, r.w, 4);
        self.canvas.fill_rounded_rect(track, 2.0, t.control_hover);
        let filled = Rect::new(r.x, cy - 2, (r.w as f32 * f) as i32, 4);
        self.canvas.fill_rounded_rect(filled, 2.0, t.accent);
        let kx = r.x as f32 + r.w as f32 * f;
        let grow = self.animate(id, if resp.hovered || resp.held { 1.0 } else { 0.0 }, 8.0);
        self.canvas.fill_circle(kx, cy as f32, 7.0 + grow * 1.5, Color::WHITE);
        self.canvas.fill_circle(kx, cy as f32, 3.5, t.accent);
        if resp.hovered {
            self.set_cursor(Cursor::Hand);
        }
        changed
    }

    /// A progress bar (`fraction` in 0..=1).
    pub fn progress(&mut self, r: Rect, fraction: f32) {
        let t = self.ctx.theme.clone();
        let radius = r.h as f32 / 2.0;
        self.canvas.fill_rounded_rect(r, radius, t.control_hover);
        let w = (r.w as f32 * fraction.clamp(0.0, 1.0)) as i32;
        if w > 0 {
            self.canvas.save();
            self.canvas.clip_to(Rect::new(r.x, r.y, w, r.h));
            self.canvas.fill_rounded_rect(r, radius, t.accent);
            self.canvas.restore();
        }
    }

    pub fn separator(&mut self, x: i32, y: i32, w: i32) {
        let c = self.ctx.theme.border;
        self.canvas.fill_rect(Rect::new(x, y, w, 1), c);
    }

    /// A raised card background.
    pub fn card(&mut self, r: Rect) {
        let t = self.ctx.theme.clone();
        self.canvas.fill_rounded_rect(r, t.radius_large, t.surface);
        self.canvas.stroke_rounded_rect(r, t.radius_large, 1.0, t.border);
    }

    /// A row of tabs; returns `true` if the selection changed.
    pub fn tabs(&mut self, r: Rect, labels: &[&str], selected: &mut usize) -> bool {
        let t = self.ctx.theme.clone();
        let mut changed = false;
        let mut x = r.x;
        for (i, label) in labels.iter().enumerate() {
            let w = self.measure(label, Font::Bold, t.font_size) as i32 + 28;
            let tr = Rect::new(x, r.y, w, r.h);
            let id = self.id(label) ^ 0x7ab5 ^ i as u64;
            let resp = self.interact(id, tr);
            if resp.clicked && *selected != i {
                *selected = i;
                changed = true;
            }
            let active = *selected == i;
            if resp.hovered && !active {
                self.canvas.fill_rounded_rect(tr.inset(2, 4, 2, 4), t.radius, Color::rgba(255, 255, 255, 14));
            }
            let color = if active { t.text } else { t.text_dim };
            self.label(tr, label, Font::Bold, t.font_size, color, Align::Center);
            if active {
                self.canvas.fill_rounded_rect(Rect::new(tr.x + 12, tr.bottom() - 3, tr.w - 24, 3), 1.5, t.accent);
            }
            x += w + 4;
        }
        changed
    }

    /// A vertically scrolling region. `f` draws the content given the
    /// scroll offset (content y = viewport y - offset). Returns `f`'s value.
    pub fn scroll_area<R>(&mut self, r: Rect, id_label: &str, content_h: i32, f: impl FnOnce(&mut Ui<'a>, i32) -> R) -> R {
        let id = self.id(id_label) ^ 0x5c01;
        let max = (content_h - r.h).max(0) as f32;
        let mut offset = self.ctx.state.floats.get(&id).copied().unwrap_or(0.0).clamp(0.0, max);
        if self.hovered(r) && self.input.scroll.1 != 0 {
            offset = (offset - self.input.scroll.1 as f32 * 48.0).clamp(0.0, max);
        }
        // Scrollbar dragging.
        let bar_id = id ^ 0xba5;
        let track = Rect::new(r.right() - 10, r.y + 2, 8, r.h - 4);
        if max > 0.0 {
            let thumb_h = ((r.h as f32 / content_h as f32) * track.h as f32).max(24.0) as i32;
            let thumb_y = track.y + ((offset / max) * (track.h - thumb_h) as f32) as i32;
            let thumb = Rect::new(track.x, thumb_y, track.w, thumb_h);
            let resp = self.interact(bar_id, track);
            if resp.held {
                if let Some((_, py)) = self.input.pointer {
                    let f = ((py - track.y - thumb_h / 2) as f32 / (track.h - thumb_h).max(1) as f32).clamp(0.0, 1.0);
                    offset = f * max;
                }
            }
            self.ctx.state.floats.insert(id, offset);
            let content_r = r;
            self.canvas.save();
            self.canvas.clip_to(content_r);
            let was_blocked = self.blocked;
            if !self.hovered(content_r) {
                self.blocked = true;
            }
            let out = f(self, offset as i32);
            self.blocked = was_blocked;
            self.canvas.restore();
            let hover = self.animate(bar_id, if resp.hovered || resp.held || self.hovered(r) { 1.0 } else { 0.0 }, 6.0);
            let alpha = (60.0 + hover * 80.0) as u8;
            self.canvas.fill_rounded_rect(thumb.inset(2, 0, 1, 0), 3.0, Color::rgba(255, 255, 255, alpha));
            out
        } else {
            self.ctx.state.floats.insert(id, 0.0);
            self.canvas.save();
            self.canvas.clip_to(r);
            let was_blocked = self.blocked;
            if !self.hovered(r) {
                self.blocked = true;
            }
            let out = f(self, 0);
            self.blocked = was_blocked;
            self.canvas.restore();
            out
        }
    }

    /// Scrolls a scroll area so that `[y, y+h)` (content coordinates) is visible.
    pub fn scroll_into_view(&mut self, id_label: &str, viewport_h: i32, y: i32, h: i32) {
        let id = self.id(id_label) ^ 0x5c01;
        let off = self.ctx.state.floats.get(&id).copied().unwrap_or(0.0) as i32;
        let new = if y < off { y } else if y + h > off + viewport_h { y + h - viewport_h } else { off };
        self.ctx.state.floats.insert(id, new.max(0) as f32);
    }

    /// A virtualised list with selection, keyboard navigation and
    /// activation. `paint` draws one row.
    pub fn list(
        &mut self,
        r: Rect,
        id_label: &str,
        count: usize,
        row_h: i32,
        selected: &mut Option<usize>,
        mut paint: impl FnMut(&mut Ui<'a>, usize, Rect, RowState),
    ) -> ListResponse {
        let id = self.id(id_label) ^ 0x1157;
        let mut out = ListResponse::default();
        let focused = self.focused(id);
        // Keyboard navigation.
        if focused && count > 0 {
            let cur = selected.unwrap_or(0);
            let page = (r.h / row_h.max(1)).max(1) as usize;
            let mut next = None;
            for k in &self.input.keys {
                next = match k.code {
                    keys::DOWN => Some((selected.map_or(0, |s| s + 1)).min(count - 1)),
                    keys::UP => Some(cur.saturating_sub(1)),
                    keys::PAGEDOWN => Some((cur + page).min(count - 1)),
                    keys::PAGEUP => Some(cur.saturating_sub(page)),
                    keys::HOME => Some(0),
                    keys::END => Some(count - 1),
                    keys::ENTER | keys::KPENTER => {
                        if let Some(s) = *selected {
                            out.activated = Some(s);
                        }
                        next
                    }
                    _ => next,
                };
            }
            if let Some(n) = next {
                *selected = Some(n);
                out.clicked = Some(n);
                self.scroll_into_view(id_label, r.h, n as i32 * row_h, row_h);
            }
        }
        let content_h = count as i32 * row_h;
        let pressed_in = self.hovered(r) && (self.input.pressed[0] || self.input.pressed[1]);
        if pressed_in {
            self.focus(id);
        }
        self.scroll_area(r, id_label, content_h, |ui, offset| {
            let first = (offset / row_h.max(1)) as usize;
            let visible = (r.h / row_h.max(1) + 2) as usize;
            for i in first..(first + visible).min(count) {
                let rr = Rect::new(r.x, r.y + i as i32 * row_h - offset, r.w, row_h);
                let row_id = id ^ ((i as u64 + 1) << 20);
                let resp = ui.interact(row_id, rr);
                if resp.pressed {
                    *selected = Some(i);
                    out.clicked = Some(i);
                    if resp.double_clicked {
                        out.activated = Some(i);
                    }
                }
                if resp.right_clicked {
                    *selected = Some(i);
                    if let Some((px, py)) = ui.input.pointer {
                        out.context = Some((i, px, py));
                    }
                }
                let state = RowState { selected: *selected == Some(i), hovered: resp.hovered, focused };
                paint(ui, i, rr, state);
            }
        });
        out
    }

    /// Default row background for list painters.
    pub fn row_background(&mut self, r: Rect, s: RowState) {
        let t = self.ctx.theme.clone();
        let inner = r.inset(4, 1, 4, 1);
        if s.selected {
            self.canvas.fill_rounded_rect(inner, t.radius, if s.focused { t.selection } else { Color::rgba(255, 255, 255, 22) });
        } else if s.hovered {
            self.canvas.fill_rounded_rect(inner, t.radius, Color::rgba(255, 255, 255, 12));
        }
    }

    /// Shows a modal dialog of the given size centred in the window. `f`
    /// draws its content inside the card rectangle and returns a value
    /// that is passed through. Everything outside is dimmed and inert.
    pub fn modal<R>(&mut self, w: i32, h: i32, f: impl FnOnce(&mut Ui<'a>, Rect) -> R) -> R {
        self.canvas.fill_rect(self.rect(), Color::rgba(0, 0, 0, 120));
        let card = self.rect().centered(w.min(self.width - 24), h.min(self.height - 24));
        self.canvas.draw_shadow(card.translate(0, 8), 12, 24, Color::rgba(0, 0, 0, 140));
        let t = self.ctx.theme.clone();
        self.canvas.fill_rounded_rect(card, t.radius_large, t.surface);
        self.canvas.stroke_rounded_rect(card, t.radius_large, 1.0, t.border_strong);
        self.modal_shown = true;
        let was_blocked = self.blocked;
        self.blocked = self.input.pointer.is_some_and(|(x, y)| self.ctx.state.overlays.iter().any(|o| o.contains(x, y)));
        self.in_modal = true;
        let out = f(self, card);
        self.in_modal = false;
        self.blocked = was_blocked;
        out
    }

    /// A standard message box with buttons; returns the index of the button
    /// pressed this frame (Esc picks the last button).
    pub fn message_box(&mut self, title: &str, message: &str, buttons: &[&str]) -> Option<usize> {
        let mut result = None;
        if self.input.key(keys::ESC) {
            result = Some(buttons.len().saturating_sub(1));
        }
        let picked = self.modal(440, 190, |ui, card| {
            let inner = card.inset(24, 20, 24, 20);
            ui.heading(Rect::new(inner.x, inner.y, inner.w, 26), title);
            let (size, color) = (ui.ctx.theme.font_size, ui.ctx.theme.text_dim);
            ui.paragraph(Rect::new(inner.x, inner.y + 38, inner.w, 60), message, size, color);
            let mut picked = None;
            let mut x = inner.right();
            for (i, label) in buttons.iter().enumerate().rev() {
                let w = (ui.measure(label, Font::Bold, size) as i32 + 32).max(92);
                x -= w;
                let kind = if i == 0 { ButtonKind::Primary } else { ButtonKind::Secondary };
                if ui.button_full(Rect::new(x, inner.bottom() - 36, w, 36), None, label, kind) {
                    picked = Some(i);
                }
                x -= 10;
            }
            if ui.input.key(keys::ENTER) {
                picked = Some(0);
            }
            picked
        });
        picked.or(result)
    }

    /// Draws an icon.
    pub fn icon(&mut self, r: Rect, icon: Icon, size: f32, color: Color) {
        icon.draw(&mut self.canvas, r, size, color);
    }

    /// A small rotating activity indicator.
    pub fn spinner(&mut self, cx: i32, cy: i32, radius: f32) {
        let t = (self.now() / 16_000_000) as f32 * 0.12;
        let accent = self.ctx.theme.accent;
        for i in 0..8 {
            let a = t + i as f32 * core::f32::consts::TAU / 8.0;
            let (s, c) = (vmath::FloatExt::sin(a), vmath::FloatExt::cos(a));
            let alpha = (255.0 * (i as f32 + 1.0) / 8.0) as u8;
            self.canvas.fill_circle(cx as f32 + c * radius, cy as f32 + s * radius, radius * 0.22, accent.with_alpha(alpha));
        }
        let now = self.now();
        self.repaint_at(now + 50_000_000);
    }
}
