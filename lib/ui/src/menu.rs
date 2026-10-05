//! Menu bars, dropdown menus and context menus.

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vproto::input::keys;

use crate::theme::Font;
use crate::ui::{Overlay, Ui};

/// One entry of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    /// Shortcut hint shown on the right (e.g. "Ctrl+S").
    pub shortcut: String,
    pub enabled: bool,
    /// A separator line instead of an item.
    pub separator: bool,
    pub checked: bool,
}

impl MenuItem {
    pub fn new(label: &str) -> MenuItem {
        MenuItem { label: label.into(), shortcut: String::new(), enabled: true, separator: false, checked: false }
    }

    pub fn shortcut(mut self, s: &str) -> MenuItem {
        self.shortcut = s.into();
        self
    }

    pub fn enabled(mut self, e: bool) -> MenuItem {
        self.enabled = e;
        self
    }

    pub fn checked(mut self, c: bool) -> MenuItem {
        self.checked = c;
        self
    }

    pub fn separator() -> MenuItem {
        MenuItem { label: String::new(), shortcut: String::new(), enabled: false, separator: true, checked: false }
    }
}

/// A titled menu in a menu bar.
pub struct Menu {
    pub title: String,
    pub items: Vec<MenuItem>,
}

impl Menu {
    pub fn new(title: &str, items: Vec<MenuItem>) -> Menu {
        Menu { title: title.into(), items }
    }
}

const ITEM_H: i32 = 32;
const SEP_H: i32 = 9;

fn dropdown_size(ui: &Ui, items: &[MenuItem]) -> (i32, i32) {
    let size = ui.ctx.theme.font_size;
    let mut w = 180;
    let mut h = 8;
    for it in items {
        if it.separator {
            h += SEP_H;
            continue;
        }
        let lw =
            ui.measure(&it.label, Font::Regular, size) as i32 + ui.measure(&it.shortcut, Font::Regular, size) as i32;
        w = w.max(lw + 90);
        h += ITEM_H;
    }
    (w, h)
}

/// Item index at a point inside a dropdown, if it is an enabled item.
fn item_at(items: &[MenuItem], rect: Rect, x: i32, y: i32) -> Option<usize> {
    if !rect.contains(x, y) {
        return None;
    }
    let mut cy = rect.y + 4;
    for (i, it) in items.iter().enumerate() {
        let h = if it.separator { SEP_H } else { ITEM_H };
        if y >= cy && y < cy + h {
            return (!it.separator && it.enabled).then_some(i);
        }
        cy += h;
    }
    None
}

/// The next selectable item after (or before) `cur`, wrapping around; with
/// no `cur`, the first (or last) one.
fn step_enabled(items: &[MenuItem], cur: Option<usize>, forward: bool) -> Option<usize> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let mut i = match (cur, forward) {
        (Some(c), true) => c + 1,
        (Some(c), false) => c + n - 1,
        (None, true) => 0,
        (None, false) => n - 1,
    } % n;
    for _ in 0..n {
        if items[i].enabled && !items[i].separator {
            return Some(i);
        }
        i = if forward { (i + 1) % n } else { (i + n - 1) % n };
    }
    None
}

/// Draws an open dropdown (called from `Ui::finish`).
pub(crate) fn draw_dropdown(ui: &mut Ui, rect: Rect, items: &[MenuItem], hovered: Option<usize>) {
    let t = ui.ctx.theme.clone();
    ui.canvas.draw_shadow(rect.translate(0, 6), 8, 16, Color::rgba(0, 0, 0, 130));
    ui.canvas.fill_rounded_rect(rect, 8.0, Color::hex(0x2A2A31));
    ui.canvas.stroke_rounded_rect(rect, 8.0, 1.0, t.border_strong);
    let mut y = rect.y + 4;
    for (i, it) in items.iter().enumerate() {
        if it.separator {
            ui.canvas.fill_rect(Rect::new(rect.x + 10, y + SEP_H / 2, rect.w - 20, 1), t.border);
            y += SEP_H;
            continue;
        }
        let r = Rect::new(rect.x + 4, y, rect.w - 8, ITEM_H);
        if hovered == Some(i) {
            ui.canvas.fill_rounded_rect(r, 5.0, t.accent);
        }
        let color = if !it.enabled {
            t.text_faint
        } else if hovered == Some(i) {
            t.on_accent
        } else {
            t.text
        };
        if it.checked {
            crate::icons::Icon::Check.draw(&mut ui.canvas, Rect::new(r.x + 6, r.y, 18, r.h), 14.0, color);
        }
        ui.label(Rect::new(r.x + 30, r.y, r.w - 40, r.h), &it.label, Font::Regular, t.font_size, color, Align::Left);
        if !it.shortcut.is_empty() {
            let sc = if hovered == Some(i) { t.on_accent.with_alpha(200) } else { t.text_faint };
            ui.label(Rect::new(r.x, r.y, r.w - 12, r.h), &it.shortcut, Font::Regular, t.small_size, sc, Align::Right);
        }
        y += ITEM_H;
    }
}

impl<'a> Ui<'a> {
    /// Schedules drawing a dropdown at `rect`, which also takes the pointer
    /// in the next frame.
    fn show_dropdown(&mut self, rect: Rect, items: &[MenuItem], hovered: Option<usize>) {
        self.new_overlay_rects.push(rect);
        self.overlays.push(Overlay::Menu { rect, items: items.to_vec(), hovered });
    }

    /// Handles an open dropdown at `rect` and schedules drawing it unless it
    /// closes; returns the chosen item. Clicking outside or pressing Esc
    /// closes it.
    fn run_dropdown(&mut self, rect: Rect, items: &[MenuItem], close: &mut bool) -> Option<usize> {
        let input = self.menu_input;
        let p = input.pointer;
        let pointed = p.and_then(|(x, y)| item_at(items, rect, x, y));
        // The keyboard highlight holds until the pointer moves.
        let mut key = self.state.menu_key.filter(|&(i, at)| at == p && i < items.len()).map(|(i, _)| i);
        let mut chosen = None;
        for k in &input.keys {
            let cur = key.or(pointed);
            match k.code {
                keys::DOWN => key = step_enabled(items, cur, true),
                keys::UP => key = step_enabled(items, cur, false),
                keys::HOME => key = step_enabled(items, None, true),
                keys::END => key = step_enabled(items, None, false),
                keys::ENTER | keys::KPENTER | keys::SPACE => {
                    if let Some(i) = cur.filter(|&i| items[i].enabled && !items[i].separator) {
                        chosen = Some(i);
                        *close = true;
                    }
                }
                keys::ESC => *close = true,
                _ => {}
            }
        }
        if input.released[0]
            && let Some(i) = pointed
        {
            chosen = Some(i);
            *close = true;
        }
        if input.pressed[0] && !p.is_some_and(|(x, y)| rect.contains(x, y)) {
            *close = true;
        }
        self.state.menu_key = if *close { None } else { key.map(|i| (i, p)) };
        // A menu that closes is not drawn in this frame: the frame may stay
        // on screen (see `Ui::skip_present`).
        if !*close {
            self.show_dropdown(rect, items, key.or(pointed));
        }
        chosen
    }

    /// A menu bar. Returns `(menu index, item index)` when an item is chosen.
    pub fn menu_bar(&mut self, r: Rect, menus: &[Menu]) -> Option<(usize, usize)> {
        let id = self.id("menubar") ^ r.y as u64;
        let t = self.ctx.theme.clone();
        let open = match self.state.open_menu {
            Some((mid, i)) if mid == id => Some(i),
            _ => None,
        };
        let mut x = r.x + 4;
        let mut result = None;
        let mut new_open = open;
        let mut title_rects = Vec::new();
        for (i, m) in menus.iter().enumerate() {
            let w = self.measure(&m.title, Font::Regular, t.font_size) as i32 + 22;
            let tr = Rect::new(x, r.y + 3, w, r.h - 6);
            title_rects.push(tr);
            // Titles react even when an overlay is open (so menus can be swept).
            let over = self.input.pointer.is_some_and(|(px, py)| tr.contains(px, py));
            if over && self.input.pressed[0] {
                new_open = if open == Some(i) { None } else { Some(i) };
            } else if over && open.is_some() && open != Some(i) {
                new_open = Some(i);
            }
            let active = new_open == Some(i);
            if active || (over && !self.blocked) {
                self.canvas.fill_rounded_rect(tr, t.radius, Color::rgba(255, 255, 255, if active { 26 } else { 14 }));
            }
            self.label(tr, &m.title, Font::Regular, t.font_size, t.text, Align::Center);
            x += w;
        }
        // Keyboard: F10 opens the first menu, Left/Right move between menus;
        // a menu opened this way highlights its first item.
        let n = menus.len();
        let before = new_open;
        for k in &self.menu_input.keys {
            new_open = match (k.code, new_open) {
                (keys::F10, None) if n > 0 => Some(0),
                (keys::LEFT, Some(i)) => Some((i + n - 1) % n),
                (keys::RIGHT, Some(i)) => Some((i + 1) % n),
                (_, cur) => cur,
            };
        }
        if new_open != before
            && let Some(i) = new_open
        {
            self.state.menu_key = step_enabled(&menus[i].items, None, true).map(|it| (it, self.menu_input.pointer));
        }
        if let Some(i) = new_open {
            let (w, h) = dropdown_size(self, &menus[i].items);
            let tr = title_rects[i];
            let rect = Rect::new(tr.x, tr.bottom() + 4, w, h);
            let mut close = false;
            // Ignore the press that opened the menu.
            let just_opened = open != Some(i);
            if !just_opened {
                if let Some(item) = self.run_dropdown(rect, &menus[i].items, &mut close) {
                    result = Some((i, item));
                }
                // A press on the title bar row toggles handled above.
                if close
                    && self.input.pointer.is_some_and(|(px, py)| title_rects.iter().any(|t| t.contains(px, py)))
                    && result.is_none()
                {
                    close = false;
                    self.show_dropdown(rect, &menus[i].items, None);
                }
            } else {
                let hovered = self.state.menu_key.map(|(item, _)| item);
                self.show_dropdown(rect, &menus[i].items, hovered);
            }
            new_open = if close { None } else { Some(i) };
        }
        self.state.open_menu = new_open.map(|i| (id, i));
        result
    }

    /// Opens a context menu at (x, y) (window coordinates).
    pub fn open_context_menu(&mut self, id_label: &str, x: i32, y: i32) {
        let id = self.id(id_label) ^ 0xc0e7;
        self.state.context_menu = Some((id, x, y));
        self.state.menu_key = None;
    }

    /// Shows the context menu `id_label` if open; returns the chosen item.
    pub fn context_menu(&mut self, id_label: &str, items: &[MenuItem]) -> Option<usize> {
        let id = self.id(id_label) ^ 0xc0e7;
        let (cid, x, y) = self.state.context_menu?;
        if cid != id {
            return None;
        }
        let (w, h) = dropdown_size(self, items);
        let rx = x.min(self.width - w - 4).max(4);
        let ry = if y + h > self.height - 4 { (y - h).max(4) } else { y };
        let rect = Rect::new(rx, ry, w, h);
        let mut close = false;
        let chosen = if self.input.pressed[1] && !rect.contains(x, y) {
            None
        } else {
            self.run_dropdown(rect, items, &mut close)
        };
        if close {
            self.state.context_menu = None;
        }
        chosen
    }
}
