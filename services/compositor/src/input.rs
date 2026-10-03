//! Pointer and keyboard input: hit testing, moving and resizing windows,
//! clicks and focus, key delivery and the global shortcuts.

use vgfx::Rect;
use vproto::display::{self as dp, Cursor, WindowEvent, WindowKind, WindowState};
use vproto::input::keys;

use crate::keymap;
use crate::state::{Compositor, Drag};
use crate::window::{Part, TITLE_HEIGHT};

/// Presses closer together than this count as a double (or triple) click.
const DOUBLE_CLICK_NS: u64 = 400_000_000;

impl Compositor {
    // ---- pointer -----------------------------------------------------------

    /// Topmost window under the point.
    pub(crate) fn window_at(&self, x: i32, y: i32) -> Option<(u32, Part)> {
        for id in self.paint_order().into_iter().rev() {
            let w = &self.windows[&id];
            if !w.visible() || w.state == WindowState::Minimized || w.closing {
                continue;
            }
            if let Some(part) = w.hit(x, y) {
                return Some((id, part));
            }
        }
        None
    }

    pub(crate) fn set_cursor_shape(&mut self, c: Cursor) {
        if self.cursor_shape != c {
            self.cursor_shape = c;
            self.update_cursor_rect();
        }
    }

    pub(crate) fn update_cursor_rect(&mut self) {
        self.damage.add(self.cursor_rect);
        if let Some((b, hx, hy)) = self.decor.cursor(self.cursor_shape) {
            self.cursor_rect = Rect::new(self.pointer.0 - hx, self.pointer.1 - hy, b.width, b.height);
        }
        self.damage.add(self.cursor_rect);
    }

    pub(crate) fn pointer_moved(&mut self, x: i32, y: i32) {
        let s = self.screen_rect();
        let (x, y) = (x.clamp(0, s.w - 1), y.clamp(0, s.h - 1));
        if (x, y) == self.pointer {
            return;
        }
        self.pointer = (x, y);
        match self.drag {
            Some(Drag::Move { id, origin, restore: true, .. }) => {
                if (x - origin.0).abs() + (y - origin.1).abs() >= 6 {
                    self.unsnap_for_drag(id, origin, (x, y));
                }
            }
            Some(Drag::Move { id, dx, dy, .. }) => {
                let area = self.work_area;
                if let Some(w) = self.windows.get_mut(&id) {
                    let before = w.paint_bounds();
                    let ny = (y - dy).max(area.y + TITLE_HEIGHT).min(area.bottom() - 8);
                    w.client_rect = Rect::new(x - dx, ny, w.client_rect.w, w.client_rect.h);
                    w.auto_slot = None;
                    let after = w.paint_bounds();
                    self.damage.add(before);
                    self.damage.add(after);
                }
                self.update_snap(id, x, y);
            }
            Some(Drag::Resize { id, edge, start, px, py }) => {
                if let Some(w) = self.windows.get_mut(&id) {
                    let before = w.paint_bounds();
                    let (ddx, ddy) = (x - px, y - py);
                    let mut r = start;
                    if edge.0 < 0 {
                        let nw = (start.w - ddx).max(w.min_w);
                        r.x = start.right() - nw;
                        r.w = nw;
                    } else if edge.0 > 0 {
                        r.w = (start.w + ddx).max(w.min_w);
                    }
                    if edge.1 < 0 {
                        let nh = (start.h - ddy).max(w.min_h);
                        r.y = start.bottom() - nh;
                        r.h = nh;
                    } else if edge.1 > 0 {
                        r.h = (start.h + ddy).max(w.min_h);
                    }
                    if r != w.client_rect {
                        w.client_rect = r;
                        w.auto_slot = None;
                        let after = w.paint_bounds();
                        self.damage.add(before);
                        self.damage.add(after);
                        let (cw, ch, st) = (r.w as u32, r.h as u32, w.state);
                        self.send(id, WindowEvent::Configure { width: cw, height: ch, state: st });
                    }
                }
            }
            Some(Drag::Client { id }) => {
                if let Some(w) = self.windows.get(&id) {
                    let c = w.client_rect;
                    self.send(id, WindowEvent::PointerMove { x: x - c.x, y: y - c.y });
                }
            }
            _ => self.update_hover(),
        }
        self.update_cursor_rect();
    }

    pub(crate) fn update_hover(&mut self) {
        let (x, y) = self.pointer;
        let hit = self.window_at(x, y);
        let new_hover_window = hit.map(|(id, _)| id);
        if new_hover_window != self.hover_window {
            if let Some(old) = self.hover_window {
                self.send(old, WindowEvent::PointerLeave {});
            }
            self.hover_window = new_hover_window;
        }
        let button_hover = hit.filter(|(_, p)| matches!(p, Part::Close | Part::Maximize | Part::Minimize));
        if button_hover != self.decor_state.hover {
            for (id, _) in [self.decor_state.hover, button_hover].into_iter().flatten() {
                if let Some(w) = self.windows.get(&id) {
                    let r = w.title_rect();
                    self.damage.add(r);
                }
            }
            self.decor_state.hover = button_hover;
        }
        let shape = match hit {
            Some((id, Part::Client)) => {
                let w = &self.windows[&id];
                let c = w.client_rect;
                self.send(id, WindowEvent::PointerMove { x: x - c.x, y: y - c.y });
                w.cursor
            }
            Some((_, Part::Edge(dx, dy))) => match (dx, dy) {
                (0, _) => Cursor::ResizeVertical,
                (_, 0) => Cursor::ResizeHorizontal,
                (a, b) if a == b => Cursor::ResizeDiagonal,
                _ => Cursor::ResizeAntiDiagonal,
            },
            _ => Cursor::Arrow,
        };
        self.set_cursor_shape(shape);
    }

    pub(crate) fn pointer_button(&mut self, button: u8, pressed: bool) {
        let (x, y) = self.pointer;
        let bit = 1u8 << button.min(7);
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        if !pressed {
            match self.drag.take() {
                Some(Drag::Move { id, .. }) => {
                    if let Some((snap, r)) = self.snap.take() {
                        self.damage.add(r.inflate(8));
                        self.apply_snap(id, snap, r);
                    }
                }
                Some(Drag::Client { id }) => {
                    if let Some(w) = self.windows.get(&id) {
                        let c = w.client_rect;
                        self.send(
                            id,
                            WindowEvent::PointerButton {
                                x: x - c.x,
                                y: y - c.y,
                                button,
                                pressed: false,
                                clicks: self.click_count,
                            },
                        );
                    }
                    if self.buttons != 0 {
                        self.drag = Some(Drag::Client { id });
                    }
                }
                Some(Drag::Button { id, part }) => {
                    self.decor_state.pressed = None;
                    self.damage_window(id);
                    if self.window_at(x, y) == Some((id, part)) {
                        match part {
                            Part::Close => self.send(id, WindowEvent::CloseRequested {}),
                            Part::Minimize => self.set_state(id, WindowState::Minimized),
                            Part::Maximize => {
                                let st = self.windows.get(&id).map(|w| w.state);
                                let next = if st == Some(WindowState::Maximized) {
                                    WindowState::Normal
                                } else {
                                    WindowState::Maximized
                                };
                                self.set_state(id, next);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            self.update_hover();
            return;
        }

        // Button press.
        let now = vrt::time::now_ns();
        let hit = self.window_at(x, y);
        let target = hit.map(|(id, _)| id).unwrap_or(0);
        let (t, last_id, last_button, lx, ly) = self.last_click;
        if now - t < DOUBLE_CLICK_NS
            && last_id == target
            && last_button == button
            && (x - lx).abs() < 5
            && (y - ly).abs() < 5
        {
            self.click_count = self.click_count.saturating_add(1);
        } else {
            self.click_count = 1;
        }
        self.last_click = (now, target, button, x, y);

        let Some((id, part)) = hit else {
            // Clicking the bare background dismisses popups.
            self.focus(None);
            return;
        };
        let kind = self.windows[&id].kind;
        // Clicking outside a focused popup closes it.
        if let Some(f) = self.focused
            && f != id
            && self.windows.get(&f).is_some_and(|w| w.kind == WindowKind::Popup)
        {
            self.send(f, WindowEvent::CloseRequested {});
        }
        if matches!(kind, WindowKind::Normal | WindowKind::Borderless) {
            self.raise(id);
            self.focus(Some(id));
        } else if kind == WindowKind::Popup {
            self.focus(Some(id));
        }
        match part {
            Part::Client => {
                let c = self.windows[&id].client_rect;
                self.send(
                    id,
                    WindowEvent::PointerButton {
                        x: x - c.x,
                        y: y - c.y,
                        button,
                        pressed: true,
                        clicks: self.click_count,
                    },
                );
                self.drag = Some(Drag::Client { id });
            }
            Part::Title if button == 0 => {
                if self.click_count >= 2 && self.windows[&id].resizable {
                    let st = self.windows[&id].state;
                    self.set_state(
                        id,
                        if st == WindowState::Maximized { WindowState::Normal } else { WindowState::Maximized },
                    );
                    return;
                }
                let w = &self.windows[&id];
                let restore = w.state == WindowState::Maximized || w.snapped.is_some();
                let c = w.client_rect;
                self.drag = Some(Drag::Move { id, dx: x - c.x, dy: y - c.y, origin: (x, y), restore });
                self.set_cursor_shape(Cursor::Move);
            }
            Part::Close | Part::Minimize | Part::Maximize if button == 0 => {
                self.decor_state.pressed = Some((id, part));
                self.damage_window(id);
                self.drag = Some(Drag::Button { id, part });
            }
            Part::Edge(dx, dy) if button == 0 => {
                let c = self.windows[&id].client_rect;
                self.drag = Some(Drag::Resize { id, edge: (dx, dy), start: c, px: x, py: y });
            }
            _ => {}
        }
    }

    pub(crate) fn scroll(&mut self, dx: i32, dy: i32) {
        let (x, y) = self.pointer;
        if let Some((id, Part::Client)) = self.window_at(x, y) {
            self.send(id, WindowEvent::Scroll { dx, dy });
        }
    }

    // ---- keyboard ----------------------------------------------------------

    pub(crate) fn key(&mut self, code: u16, pressed: bool) {
        let now = vrt::time::now_ns();
        let out = self.keyboard.process(code, pressed, now);
        let mods = out.modifiers;
        let alt = mods & dp::modifiers::ALT != 0;
        // Super tapped on its own: start menu.
        if self.keyboard.take_meta_tap(code, pressed) {
            self.send_to_shell(WindowEvent::StartMenuKey {});
            return;
        }
        if pressed && alt && code == keys::TAB {
            self.switcher_step(mods & dp::modifiers::SHIFT != 0);
            return;
        }
        if self.switcher.is_some() {
            if pressed && code == keys::ESC {
                self.close_switcher(false);
                return;
            }
            if !pressed && matches!(code, keys::LEFTALT | keys::RIGHTALT) {
                // Activate the choice; the Alt release still reaches it below.
                self.close_switcher(true);
            }
        }
        if pressed && alt && code == keys::F4 {
            if let Some(f) = self.focused {
                self.send(f, WindowEvent::CloseRequested {});
            }
            return;
        }
        if pressed && mods & dp::modifiers::SUPER != 0 && code == keys::SPACE {
            self.send_to_shell(WindowEvent::AgentKey {});
            return;
        }
        if pressed
            && mods & dp::modifiers::SUPER != 0
            && matches!(code, keys::LEFT | keys::RIGHT | keys::UP | keys::DOWN | keys::D)
        {
            if code == keys::D {
                self.toggle_desktop();
            } else {
                self.arrange_focused(code);
            }
            return;
        }
        self.deliver_key(out);
    }

    pub(crate) fn deliver_key(&mut self, out: keymap::KeyOutput) {
        if let Some(f) = self.focused {
            self.send(
                f,
                WindowEvent::Key {
                    code: out.code,
                    pressed: out.pressed,
                    repeat: out.repeat,
                    modifiers: out.modifiers,
                    text: out.text,
                },
            );
        }
    }
}
