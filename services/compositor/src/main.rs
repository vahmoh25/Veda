//! `compositor` — the Vindows window system.
//!
//! Owns the framebuffer and composes client windows into it. It provides two
//! services:
//!
//! * `display` — clients create windows, attach shared pixel buffers and
//!   present frames (see `vproto::display`);
//! * `input` — device drivers report keyboard and pointer events.
//!
//! Rendering is damage driven: only regions that changed are recomposited
//! into the back buffer and copied to the framebuffer, at most once per
//! display frame.

#![no_std]
#![no_main]

extern crate alloc;

mod decor;
mod keymap;
mod window;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vgfx::{Bitmap, Canvas, Damage, Rect, Text};
use vipc::WaitSet;
use vproto::display::{
    self as dp, Cursor, DisplayError, ScreenInfo, WindowEvent, WindowInfo, WindowKind, WindowSpec, WindowState, display,
};
use vproto::input::{InputEvent, keys};
use vrt::object::{Channel, Vmo};
use vrt::println;
use vrt::vm::Mapping;

use decor::{Decor, DecorState};
use keymap::Keyboard;
use window::{Anim, AnimKind, Buffers, Part, TITLE_HEIGHT, Window};

vrt::entry!(main);

const FRAME_NS: u64 = 16_666_666;
const DOUBLE_CLICK_NS: u64 = 400_000_000;

/// The framebuffer and the back buffer we compose into.
struct Screen {
    fb: Mapping,
    pitch: usize,
    rgb: bool,
    back: Bitmap,
}

impl Screen {
    fn rect(&self) -> Rect {
        self.back.rect()
    }

    /// Copies a region of the back buffer to the framebuffer.
    fn flush(&mut self, r: Rect) {
        let r = r.intersect(&self.rect());
        let w = self.back.width;
        let fb = self.fb.as_ptr();
        for y in r.y..r.bottom() {
            let src = &self.back.pixels[(y * w + r.x) as usize..(y * w + r.right()) as usize];
            // SAFETY: the framebuffer mapping covers `pitch * height` bytes.
            let dst = unsafe { core::slice::from_raw_parts_mut((fb.add(y as usize * self.pitch) as *mut u32).add(r.x as usize), r.w as usize) };
            if self.rgb {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = (s & 0xFF00_FF00) | ((s >> 16) & 0xFF) | ((s & 0xFF) << 16);
                }
            } else {
                dst.copy_from_slice(src);
            }
        }
    }
}

/// An interactive move or resize in progress.
#[derive(Clone, Copy)]
enum Drag {
    Move { id: u32, dx: i32, dy: i32 },
    Resize { id: u32, edge: (i8, i8), start: Rect, px: i32, py: i32 },
    /// Pointer grab by a client (button held inside its client area).
    Client { id: u32 },
    /// Pressing a caption button (activates on release over it).
    Button { id: u32, part: Part },
}

struct DisplayClient {
    channel: Channel,
    windows: Vec<u32>,
}

struct Compositor {
    screen: Screen,
    decor: Decor,
    windows: BTreeMap<u32, Window>,
    /// Bottom-to-top stacking order of normal windows.
    order: Vec<u32>,
    focused: Option<u32>,
    next_id: u32,
    damage: Damage,
    clients: BTreeMap<u64, DisplayClient>,
    shell: Option<u64>,
    keyboard: Keyboard,
    pointer: (i32, i32),
    cursor_rect: Rect,
    cursor_shape: Cursor,
    buttons: u8,
    drag: Option<Drag>,
    decor_state: DecorState,
    hover_window: Option<u32>,
    last_click: (u64, u32, u8, i32, i32),
    click_count: u8,
    clipboard: String,
    last_frame: u64,
    work_area: Rect,
    alt_tab: Option<usize>,
}

fn layer(kind: WindowKind) -> u8 {
    match kind {
        WindowKind::Desktop => 0,
        WindowKind::Normal | WindowKind::Borderless => 1,
        WindowKind::Panel => 2,
        WindowKind::Popup => 3,
        WindowKind::Notification => 4,
    }
}

impl Compositor {
    fn send(&self, id: u32, ev: WindowEvent) {
        if let Some(w) = self.windows.get(&id) {
            let _ = vipc::send_event(&w.events, dp::EVENT, ev);
        }
    }

    /// Windows in paint order (bottom to top).
    fn paint_order(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.order.clone();
        v.sort_by_key(|id| self.windows.get(id).map(|w| layer(w.kind)).unwrap_or(0));
        v
    }

    fn damage_window(&mut self, id: u32) {
        if let Some(w) = self.windows.get(&id) {
            let r = w.paint_bounds();
            self.damage.add(r.inflate(2));
        }
    }

    fn screen_rect(&self) -> Rect {
        self.screen.rect()
    }

    fn update_work_area(&mut self) {
        let s = self.screen_rect();
        let mut area = s;
        for w in self.windows.values() {
            if w.kind == WindowKind::Panel && w.state != WindowState::Minimized {
                let f = w.frame();
                if f.bottom() >= s.bottom() - 2 && f.w > s.w / 2 {
                    area.h = area.h.min(f.y - area.y);
                } else if f.y <= s.y + 2 && f.w > s.w / 2 {
                    let cut = f.bottom() - area.y;
                    area.y += cut;
                    area.h -= cut;
                }
            }
        }
        self.work_area = area;
    }

    fn notify_shell(&self) {
        if let Some(sc) = self.shell {
            for (id, w) in &self.windows {
                if w.client == sc && w.kind == WindowKind::Panel {
                    self.send(*id, WindowEvent::WindowsChanged {});
                }
            }
        }
    }

    fn raise(&mut self, id: u32) {
        self.order.retain(|&x| x != id);
        self.order.push(id);
        self.damage_window(id);
    }

    fn focus(&mut self, id: Option<u32>) {
        if self.focused == id {
            return;
        }
        if let Some(old) = self.focused {
            self.send(old, WindowEvent::Focus { focused: false });
            self.damage_window(old);
            // Popups close when they lose focus.
            if let Some(w) = self.windows.get(&old) {
                if w.kind == WindowKind::Popup {
                    self.send(old, WindowEvent::CloseRequested {});
                }
            }
        }
        self.focused = id;
        self.keyboard.reset_repeat();
        if let Some(new) = id {
            self.send(new, WindowEvent::Focus { focused: true });
            self.damage_window(new);
        }
        self.notify_shell();
    }

    /// Focuses the topmost visible normal window.
    fn focus_top(&mut self) {
        let top = self.order.iter().rev().copied().find(|id| {
            self.windows.get(id).is_some_and(|w| w.kind == WindowKind::Normal && w.state != WindowState::Minimized && !w.closing)
        });
        self.focus(top);
    }

    fn place_new_window(&self, spec: &WindowSpec) -> Rect {
        let area = self.work_area;
        let (w, h) = (spec.width as i32, spec.height as i32);
        let tb = if spec.kind == WindowKind::Normal { TITLE_HEIGHT } else { 0 };
        if spec.x != i32::MIN && spec.y != i32::MIN {
            return Rect::new(spec.x, spec.y + tb, w, h);
        }
        match spec.kind {
            WindowKind::Desktop => self.screen_rect(),
            _ => {
                let n = self.windows.values().filter(|w| w.kind == WindowKind::Normal).count() as i32;
                let x = area.x + (area.w - w) / 2 + (n % 6) * 28 - 70;
                let y = area.y + (area.h - h - tb) / 2 + (n % 6) * 28 - 50;
                Rect::new(x.clamp(area.x, (area.right() - w).max(area.x)), y.max(area.y) + tb, w, h)
            }
        }
    }

    fn set_state(&mut self, id: u32, state: WindowState) {
        let area = self.work_area;
        let screen = self.screen_rect();
        let Some(w) = self.windows.get_mut(&id) else { return };
        if w.state == state {
            return;
        }
        let before = w.paint_bounds();
        let old = w.state;
        match state {
            WindowState::Maximized => {
                if old == WindowState::Normal {
                    w.restore_rect = w.client_rect;
                }
                w.state = state;
                w.client_rect = Rect::new(area.x, area.y + TITLE_HEIGHT, area.w, area.h - TITLE_HEIGHT);
            }
            WindowState::Fullscreen => {
                if old == WindowState::Normal {
                    w.restore_rect = w.client_rect;
                }
                w.state = state;
                w.client_rect = screen;
            }
            WindowState::Normal => {
                w.state = state;
                if old == WindowState::Maximized || old == WindowState::Fullscreen {
                    w.client_rect = w.restore_rect;
                }
                if old == WindowState::Minimized {
                    w.anim = Some(Anim { kind: AnimKind::Restore, start: vrt::time::now_ns(), duration: 160_000_000 });
                }
            }
            WindowState::Minimized => {
                w.state = state;
                w.anim = Some(Anim { kind: AnimKind::Minimize, start: vrt::time::now_ns(), duration: 170_000_000 });
            }
        }
        let (cw, ch, st) = (w.client_rect.w as u32, w.client_rect.h as u32, w.state);
        let after = w.paint_bounds();
        self.damage.add(before);
        self.damage.add(after);
        if state != WindowState::Minimized {
            self.send(id, WindowEvent::Configure { width: cw, height: ch, state: st });
        }
        if state == WindowState::Minimized {
            if self.focused == Some(id) {
                self.focused = None;
                self.focus_top();
            }
        } else {
            self.raise(id);
            self.focus(Some(id));
        }
        self.notify_shell();
    }

    fn destroy_window(&mut self, id: u32) {
        self.damage_window(id);
        self.windows.remove(&id);
        self.order.retain(|&x| x != id);
        for c in self.clients.values_mut() {
            c.windows.retain(|&x| x != id);
        }
        if self.focused == Some(id) {
            self.focused = None;
            self.focus_top();
        }
        if self.hover_window == Some(id) {
            self.hover_window = None;
        }
        if matches!(self.drag, Some(Drag::Move { id: d, .. } | Drag::Resize { id: d, .. } | Drag::Client { id: d } | Drag::Button { id: d, .. }) if d == id) {
            self.drag = None;
        }
        self.update_work_area();
        self.notify_shell();
    }

    // ---- pointer -----------------------------------------------------------

    /// Topmost window under the point.
    fn window_at(&self, x: i32, y: i32) -> Option<(u32, Part)> {
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

    fn set_cursor_shape(&mut self, c: Cursor) {
        if self.cursor_shape != c {
            self.cursor_shape = c;
            self.update_cursor_rect();
        }
    }

    fn update_cursor_rect(&mut self) {
        self.damage.add(self.cursor_rect);
        if let Some((b, hx, hy)) = self.decor.cursor(self.cursor_shape) {
            self.cursor_rect = Rect::new(self.pointer.0 - hx, self.pointer.1 - hy, b.width, b.height);
        }
        self.damage.add(self.cursor_rect);
    }

    fn pointer_moved(&mut self, x: i32, y: i32) {
        let s = self.screen_rect();
        let (x, y) = (x.clamp(0, s.w - 1), y.clamp(0, s.h - 1));
        if (x, y) == self.pointer {
            return;
        }
        self.pointer = (x, y);
        match self.drag {
            Some(Drag::Move { id, dx, dy }) => {
                let area = self.work_area;
                if let Some(w) = self.windows.get_mut(&id) {
                    let before = w.paint_bounds();
                    let ny = (y - dy).max(area.y + TITLE_HEIGHT).min(area.bottom() - 8);
                    w.client_rect = Rect::new(x - dx, ny, w.client_rect.w, w.client_rect.h);
                    let after = w.paint_bounds();
                    self.damage.add(before);
                    self.damage.add(after);
                }
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

    fn update_hover(&mut self) {
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

    fn pointer_button(&mut self, button: u8, pressed: bool) {
        let (x, y) = self.pointer;
        let bit = 1u8 << button.min(7);
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        if !pressed {
            match self.drag.take() {
                Some(Drag::Client { id }) => {
                    if let Some(w) = self.windows.get(&id) {
                        let c = w.client_rect;
                        self.send(id, WindowEvent::PointerButton { x: x - c.x, y: y - c.y, button, pressed: false, clicks: self.click_count });
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
                                let next = if st == Some(WindowState::Maximized) { WindowState::Normal } else { WindowState::Maximized };
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
        if now - t < DOUBLE_CLICK_NS && last_id == target && last_button == button && (x - lx).abs() < 5 && (y - ly).abs() < 5 {
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
        if let Some(f) = self.focused {
            if f != id && self.windows.get(&f).is_some_and(|w| w.kind == WindowKind::Popup) {
                self.send(f, WindowEvent::CloseRequested {});
            }
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
                self.send(id, WindowEvent::PointerButton { x: x - c.x, y: y - c.y, button, pressed: true, clicks: self.click_count });
                self.drag = Some(Drag::Client { id });
            }
            Part::Title if button == 0 => {
                if self.click_count >= 2 && self.windows[&id].resizable {
                    let st = self.windows[&id].state;
                    self.set_state(id, if st == WindowState::Maximized { WindowState::Normal } else { WindowState::Maximized });
                    return;
                }
                let w = &self.windows[&id];
                if w.state == WindowState::Maximized {
                    // Dragging a maximised window restores it under the cursor.
                    let rest = w.restore_rect;
                    let frac = (x - w.client_rect.x) as f32 / w.client_rect.w.max(1) as f32;
                    self.set_state(id, WindowState::Normal);
                    if let Some(w) = self.windows.get_mut(&id) {
                        let nx = x - (rest.w as f32 * frac) as i32;
                        w.client_rect = Rect::new(nx, y + TITLE_HEIGHT / 2, rest.w, rest.h);
                    }
                }
                let c = self.windows[&id].client_rect;
                self.drag = Some(Drag::Move { id, dx: x - c.x, dy: y - c.y });
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

    fn scroll(&mut self, dx: i32, dy: i32) {
        let (x, y) = self.pointer;
        if let Some((id, Part::Client)) = self.window_at(x, y) {
            self.send(id, WindowEvent::Scroll { dx, dy });
        }
    }

    // ---- keyboard ----------------------------------------------------------

    fn key(&mut self, code: u16, pressed: bool) {
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
            self.cycle_windows(mods & dp::modifiers::SHIFT != 0);
            return;
        }
        if !pressed && matches!(code, keys::LEFTALT | keys::RIGHTALT) {
            self.alt_tab = None;
        }
        if pressed && alt && code == keys::F4 {
            if let Some(f) = self.focused {
                self.send(f, WindowEvent::CloseRequested {});
            }
            return;
        }
        self.deliver_key(out);
    }

    fn deliver_key(&mut self, out: keymap::KeyOutput) {
        if let Some(f) = self.focused {
            self.send(f, WindowEvent::Key { code: out.code, pressed: out.pressed, repeat: out.repeat, modifiers: out.modifiers, text: out.text });
        }
    }

    fn send_to_shell(&self, ev: WindowEvent) {
        if let Some(sc) = self.shell {
            if let Some((id, _)) = self.windows.iter().find(|(_, w)| w.client == sc && w.kind == WindowKind::Panel) {
                self.send(*id, ev);
            }
        }
    }

    fn cycle_windows(&mut self, backwards: bool) {
        let candidates: Vec<u32> = self
            .order
            .iter()
            .rev()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(|w| w.kind == WindowKind::Normal && !w.closing))
            .collect();
        if candidates.len() < 2 {
            return;
        }
        let i = self.alt_tab.map(|i| if backwards { i + candidates.len() - 1 } else { i + 1 }).unwrap_or(1) % candidates.len();
        self.alt_tab = Some(i);
        let id = candidates[i];
        if self.windows[&id].state == WindowState::Minimized {
            self.set_state(id, WindowState::Normal);
        }
        self.raise(id);
        self.focus(Some(id));
    }

    // ---- rendering ---------------------------------------------------------

    fn animating(&self) -> bool {
        self.windows.values().any(|w| w.anim.is_some())
    }

    fn composite(&mut self) {
        let now = vrt::time::now_ns();
        // Advance animations.
        let mut finished_close = Vec::new();
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            let w = self.windows.get_mut(&id).unwrap();
            if let Some(a) = w.anim {
                let r = w.paint_bounds();
                self.damage.add(r.inflate(4));
                if a.done(now) {
                    w.anim = None;
                    if a.kind == AnimKind::Close {
                        finished_close.push(id);
                    }
                }
            }
        }
        for id in finished_close {
            self.destroy_window(id);
        }
        let screen = self.screen_rect();
        let order = self.paint_order();
        let rects = self.damage.take();
        for r in rects {
            let r = r.intersect(&screen);
            if r.is_empty() {
                continue;
            }
            let (w, h) = (self.screen.back.width, self.screen.back.height);
            let mut c = Canvas::new(&mut self.screen.back.pixels, w, h, w);
            c.clip_to(r);
            // Skip the background when an opaque desktop covers everything.
            let desktop_covers = order.iter().any(|id| {
                self.windows.get(id).is_some_and(|w| w.kind == WindowKind::Desktop && w.current.is_some() && w.frame().intersect(&r) == r)
            });
            if !desktop_covers {
                decor::draw_background(&mut c, screen);
            }
            for id in &order {
                let win = &self.windows[id];
                if !win.visible() || !win.paint_bounds().intersects(&r) {
                    continue;
                }
                let (opacity, dy) = match win.anim {
                    Some(a) => {
                        let p = a.progress(now);
                        match a.kind {
                            AnimKind::Open | AnimKind::Restore => (p, ((1.0 - p) * 14.0) as i32),
                            AnimKind::Close => (1.0 - p, (p * 10.0) as i32),
                            AnimKind::Minimize => (1.0 - p, (p * 40.0) as i32),
                        }
                    }
                    None => (1.0, 0),
                };
                self.decor.draw_window(&mut c, win, self.focused == Some(*id), self.decor_state, opacity, dy);
            }
            // Cursor on top.
            if self.cursor_rect.intersects(&r) {
                if let Some((b, hx, hy)) = self.decor.cursor(self.cursor_shape) {
                    c.draw_bitmap(b, self.pointer.0 - hx, self.pointer.1 - hy, 255);
                }
            }
            drop(c);
            self.screen.flush(r);
        }
        // Clients may now draw their next frame.
        let owed: Vec<(u32, u8)> = self.windows.iter_mut().filter_map(|(id, w)| w.frame_owed.take().map(|b| (*id, b))).collect();
        for (id, b) in owed {
            self.send(id, WindowEvent::FrameDone { shown: b });
        }
        self.last_frame = now;
    }
}

/// One display-protocol connection.
struct Session<'a> {
    comp: &'a mut Compositor,
    client: u64,
}

impl Session<'_> {
    fn own(&self, id: u32) -> Result<(), DisplayError> {
        match self.comp.windows.get(&id) {
            Some(w) if w.client == self.client => Ok(()),
            _ => Err(DisplayError::NoSuchWindow),
        }
    }

    fn is_shell(&self) -> bool {
        self.comp.shell == Some(self.client)
    }
}

impl display::Server for Session<'_> {
    fn create_window(&mut self, spec: WindowSpec) -> Result<(u32, Channel), DisplayError> {
        if spec.width == 0 || spec.height == 0 || spec.width > 8192 || spec.height > 8192 {
            return Err(DisplayError::Invalid);
        }
        if matches!(spec.kind, WindowKind::Desktop | WindowKind::Panel | WindowKind::Notification) {
            // The first client to create shell surfaces becomes the shell.
            match self.comp.shell {
                None => self.comp.shell = Some(self.client),
                Some(s) if s != self.client => return Err(DisplayError::Denied),
                _ => {}
            }
        }
        let (ours, theirs) = Channel::create().map_err(|_| DisplayError::NoMemory)?;
        let id = self.comp.next_id;
        self.comp.next_id += 1;
        let rect = self.comp.place_new_window(&spec);
        let animate = spec.kind == WindowKind::Normal || spec.kind == WindowKind::Popup;
        let win = Window {
            id,
            client: self.client,
            kind: spec.kind,
            title: spec.title,
            app_id: spec.app_id,
            state: WindowState::Normal,
            client_rect: rect,
            restore_rect: rect,
            min_w: spec.min_width.max(64) as i32,
            min_h: spec.min_height.max(32) as i32,
            resizable: spec.resizable && spec.kind == WindowKind::Normal,
            events: ours,
            buffers: None,
            current: None,
            frame_owed: None,
            cursor: Cursor::Arrow,
            anim: animate.then(|| Anim { kind: AnimKind::Open, start: vrt::time::now_ns(), duration: 180_000_000 }),
            closing: false,
        };
        let kind = win.kind;
        self.comp.windows.insert(id, win);
        self.comp.order.push(id);
        if let Some(c) = self.comp.clients.get_mut(&self.client) {
            c.windows.push(id);
        }
        // The client may already know its final size; tell it anyway.
        let _ = vipc::send_event(
            &self.comp.windows[&id].events,
            dp::EVENT,
            WindowEvent::Configure { width: rect.w as u32, height: rect.h as u32, state: WindowState::Normal },
        );
        if matches!(kind, WindowKind::Normal | WindowKind::Borderless | WindowKind::Popup) {
            self.comp.raise(id);
            self.comp.focus(Some(id));
        }
        if kind == WindowKind::Panel {
            self.comp.update_work_area();
        }
        self.comp.damage_window(id);
        self.comp.notify_shell();
        Ok((id, theirs))
    }

    fn attach_buffers(&mut self, id: u32, buffers: Vmo, width: u32, height: u32, stride: u32, count: u8) -> Result<(), DisplayError> {
        self.own(id)?;
        if width == 0 || height == 0 || stride < width || count == 0 || count > 3 || width > 8192 || height > 8192 {
            return Err(DisplayError::BadBuffer);
        }
        let bytes = stride as usize * height as usize * 4 * count as usize;
        let size = buffers.size().map_err(|_| DisplayError::BadBuffer)?;
        if size < bytes {
            return Err(DisplayError::BadBuffer);
        }
        let map = Mapping::new(buffers, bytes.next_multiple_of(4096), vabi::map_flags::READ).map_err(|_| DisplayError::NoMemory)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        w.buffers = Some(Buffers { map, width: width as i32, height: height as i32, stride: stride as i32, count });
        w.current = None;
        Ok(())
    }

    fn present(&mut self, id: u32, index: u8, damage: Vec<dp::Rect>) -> Result<(), DisplayError> {
        self.own(id)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        let Some(b) = &w.buffers else { return Err(DisplayError::BadBuffer) };
        if index >= b.count {
            return Err(DisplayError::BadBuffer);
        }
        let first = w.current.is_none();
        w.current = Some(index);
        // Acknowledged with FrameDone after the next composite.
        w.frame_owed = Some(index);
        let c = w.client_rect;
        if first || damage.is_empty() || w.anim.is_some() {
            let r = w.paint_bounds();
            self.comp.damage.add(r);
        } else {
            for d in damage {
                self.comp.damage.add(Rect::new(c.x + d.x, c.y + d.y, d.w as i32, d.h as i32).intersect(&c));
            }
        }
        Ok(())
    }

    fn set_title(&mut self, id: u32, title: String) -> Result<(), DisplayError> {
        self.own(id)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        w.title = title;
        let r = w.title_rect();
        self.comp.damage.add(r);
        self.comp.notify_shell();
        Ok(())
    }

    fn set_state(&mut self, id: u32, state: WindowState) -> Result<(), DisplayError> {
        self.own(id)?;
        self.comp.set_state(id, state);
        Ok(())
    }

    fn destroy_window(&mut self, id: u32) -> Result<(), DisplayError> {
        self.own(id)?;
        let w = self.comp.windows.get_mut(&id).unwrap();
        if w.kind == WindowKind::Normal && w.state != WindowState::Minimized {
            // Fade out, then remove.
            w.closing = true;
            w.anim = Some(Anim { kind: AnimKind::Close, start: vrt::time::now_ns(), duration: 130_000_000 });
            if self.comp.focused == Some(id) {
                self.comp.focused = None;
                self.comp.focus_top();
            }
            self.comp.notify_shell();
        } else {
            self.comp.destroy_window(id);
        }
        Ok(())
    }

    fn set_cursor(&mut self, id: u32, cursor: Cursor) -> Result<(), DisplayError> {
        self.own(id)?;
        self.comp.windows.get_mut(&id).unwrap().cursor = cursor;
        if self.comp.hover_window == Some(id) {
            self.comp.update_hover();
        }
        Ok(())
    }

    fn begin_move(&mut self, id: u32) -> Result<(), DisplayError> {
        self.own(id)?;
        let c = self.comp.windows[&id].client_rect;
        let (x, y) = self.comp.pointer;
        self.comp.drag = Some(Drag::Move { id, dx: x - c.x, dy: y - c.y });
        Ok(())
    }

    fn screen_info(&mut self) -> ScreenInfo {
        let s = self.comp.screen_rect();
        let a = self.comp.work_area;
        ScreenInfo { width: s.w as u32, height: s.h as u32, work_area: dp::Rect::new(a.x, a.y, a.w as u32, a.h as u32) }
    }

    fn set_clipboard(&mut self, text: String) {
        self.comp.clipboard = text;
    }

    fn get_clipboard(&mut self) -> String {
        self.comp.clipboard.clone()
    }

    fn set_position(&mut self, id: u32, x: i32, y: i32) -> Result<(), DisplayError> {
        self.own(id)?;
        self.comp.damage_window(id);
        let w = self.comp.windows.get_mut(&id).unwrap();
        let tb = if w.decorated() { TITLE_HEIGHT } else { 0 };
        w.client_rect = Rect::new(x, y + tb, w.client_rect.w, w.client_rect.h);
        self.comp.damage_window(id);
        if self.comp.windows[&id].kind == WindowKind::Panel {
            self.comp.update_work_area();
        }
        Ok(())
    }

    fn list_windows(&mut self) -> Vec<WindowInfo> {
        self.comp
            .order
            .iter()
            .filter_map(|id| self.comp.windows.get(id))
            .filter(|w| w.kind == WindowKind::Normal && !w.closing)
            .map(|w| WindowInfo {
                id: w.id,
                title: w.title.clone(),
                app_id: w.app_id.clone(),
                state: w.state,
                focused: self.comp.focused == Some(w.id),
                kind: w.kind,
            })
            .collect()
    }

    fn activate_window(&mut self, id: u32) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        let st = self.comp.windows.get(&id).map(|w| w.state).ok_or(DisplayError::NoSuchWindow)?;
        if st == WindowState::Minimized {
            self.comp.set_state(id, WindowState::Normal);
        }
        self.comp.raise(id);
        self.comp.focus(Some(id));
        Ok(())
    }

    fn minimize_window(&mut self, id: u32) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        if !self.comp.windows.contains_key(&id) {
            return Err(DisplayError::NoSuchWindow);
        }
        self.comp.set_state(id, WindowState::Minimized);
        Ok(())
    }

    fn close_window(&mut self, id: u32) -> Result<(), DisplayError> {
        if !self.is_shell() {
            return Err(DisplayError::Denied);
        }
        if !self.comp.windows.contains_key(&id) {
            return Err(DisplayError::NoSuchWindow);
        }
        self.comp.send(id, WindowEvent::CloseRequested {});
        Ok(())
    }
}

/// Reads a font file from the system image and leaks it (fonts live forever).
fn load_font(vfs: &vproto::vfs::Client, path: &str) -> Option<&'static [u8]> {
    let (vmo, len) = vfs.read_file(path.into()).ok()?.ok()?;
    let mut data = alloc::vec![0u8; len as usize];
    vmo.read(0, &mut data).ok()?;
    Some(data.leak())
}

fn main() -> i32 {
    use vabi::startup::role;
    let (Some(fb_vmo), Some(info_vmo)) =
        (vrt::env::take_handle(role::FRAMEBUFFER).map(Vmo::from_handle), vrt::env::take_handle(role::BOOT_INFO).map(Vmo::from_handle))
    else {
        println!("no framebuffer");
        return 1;
    };
    let mut raw = [0u8; core::mem::size_of::<vabi::KernelBootInfo>()];
    let _ = info_vmo.read(0, &mut raw);
    // SAFETY: plain data written by the kernel.
    let info: vabi::KernelBootInfo = unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const vabi::KernelBootInfo) };
    let (width, height, pitch) = (info.framebuffer_width as i32, info.framebuffer_height as i32, info.framebuffer_pitch as usize);
    let fb_size = (pitch * height as usize).next_multiple_of(4096);
    let fb = match Mapping::new(fb_vmo, fb_size, vabi::map_flags::READ | vabi::map_flags::WRITE) {
        Ok(m) => m,
        Err(e) => {
            println!("cannot map the framebuffer: {}", e);
            return 1;
        }
    };

    let mut text = Text::new();
    let mut title_font = 0;
    if let Ok(ch) = vproto::connect(vproto::vfs::NAME) {
        let vfs = vproto::vfs::Client::new(ch);
        for (i, path) in ["/system/fonts/Inter-SemiBold.otf", "/system/fonts/Inter-Regular.otf", "/system/fonts/Lato-Regular.ttf"].iter().enumerate() {
            if let Some(data) = load_font(&vfs, path) {
                if let Some(idx) = text.add_font(data) {
                    if i == 0 {
                        title_font = idx;
                    }
                }
            }
        }
    }
    if text.font_count() == 0 {
        println!("warning: no fonts available, titles will be blank");
    }

    let display_listener = vproto::register(display::NAME).expect("cannot register the display service");
    let input_listener = vproto::register(vproto::input::NAME).expect("cannot register the input service");
    println!("display {}x{} ready", width, height);

    let mut comp = Compositor {
        screen: Screen { fb, pitch, rgb: info.framebuffer_format == 2, back: Bitmap::new(width, height) },
        decor: Decor::new(text, title_font),
        windows: BTreeMap::new(),
        order: Vec::new(),
        focused: None,
        next_id: 1,
        damage: Damage::new(),
        clients: BTreeMap::new(),
        shell: None,
        keyboard: Keyboard::default(),
        pointer: (width / 2, height / 2),
        cursor_rect: Rect::default(),
        cursor_shape: Cursor::Arrow,
        buttons: 0,
        drag: None,
        decor_state: DecorState::default(),
        hover_window: None,
        last_click: (0, 0, 0, 0, 0),
        click_count: 0,
        clipboard: String::new(),
        last_frame: 0,
        work_area: Rect::new(0, 0, width, height),
        alt_tab: None,
    };
    comp.update_cursor_rect();
    comp.damage.add(comp.screen_rect());
    comp.composite();

    let mut inputs: BTreeMap<u64, Channel> = BTreeMap::new();
    let mut next_key = 10u64;
    const DISPLAY_KEY: u64 = 1;
    const INPUT_KEY: u64 = 2;
    const INPUT_BASE: u64 = 1 << 40;
    loop {
        let now = vrt::time::now_ns();
        let mut deadline = vabi::DEADLINE_INFINITE;
        if !comp.damage.is_empty() || comp.animating() {
            deadline = (comp.last_frame + FRAME_NS).max(now);
        }
        if let Some(r) = comp.keyboard.repeat_deadline() {
            deadline = deadline.min(r);
        }
        let mut ws = WaitSet::new();
        ws.add(display_listener.raw(), signals::READABLE, DISPLAY_KEY);
        ws.add(input_listener.raw(), signals::READABLE, INPUT_KEY);
        for (&k, c) in &comp.clients {
            ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        for (&k, c) in &inputs {
            ws.add(c.raw(), signals::READABLE | signals::PEER_CLOSED, INPUT_BASE | k);
        }
        let ready = ws.wait(deadline).unwrap_or_default();
        for (key, observed) in ready {
            match key {
                DISPLAY_KEY => {
                    while let Some(ch) = vproto::accept(&display_listener) {
                        next_key += 1;
                        comp.clients.insert(next_key, DisplayClient { channel: ch, windows: Vec::new() });
                    }
                }
                INPUT_KEY => {
                    while let Some(ch) = vproto::accept(&input_listener) {
                        next_key += 1;
                        inputs.insert(next_key, ch);
                    }
                }
                k if k & INPUT_BASE != 0 => {
                    let k = k & !INPUT_BASE;
                    if observed & signals::READABLE != 0 {
                        while let Some(Ok(msg)) = inputs.get(&k).map(|c| c.read()) {
                            if let Ok((_, events)) = vipc::decode_event::<Vec<InputEvent>>(msg) {
                                for ev in events {
                                    match ev {
                                        InputEvent::Absolute { x, y, max_x, max_y } => {
                                            let px = (x as i64 * width as i64 / max_x.max(1) as i64) as i32;
                                            let py = (y as i64 * height as i64 / max_y.max(1) as i64) as i32;
                                            comp.pointer_moved(px, py);
                                        }
                                        InputEvent::Motion { dx, dy } => {
                                            let (x, y) = comp.pointer;
                                            comp.pointer_moved(x + dx, y + dy);
                                        }
                                        InputEvent::Button { button, pressed } => comp.pointer_button(button, pressed),
                                        InputEvent::Scroll { dx, dy } => comp.scroll(dx, dy),
                                        InputEvent::Key { code, pressed } => comp.key(code, pressed),
                                    }
                                }
                            }
                        }
                    } else if observed & signals::PEER_CLOSED != 0 {
                        inputs.remove(&k);
                    }
                }
                k => {
                    if observed & signals::READABLE != 0 {
                        while let Some(Ok(msg)) = comp.clients.get(&k).map(|c| c.channel.read()) {
                            let mut s = Session { comp: &mut comp, client: k };
                            match display::dispatch(&mut s, msg) {
                                Ok(reply) => {
                                    if let Some(c) = comp.clients.get(&k) {
                                        let _ = reply.send(&c.channel);
                                    }
                                }
                                Err(e) => println!("bad display request: {}", e),
                            }
                        }
                    } else if observed & signals::PEER_CLOSED != 0 {
                        // Client gone: remove its windows.
                        if let Some(c) = comp.clients.remove(&k) {
                            for id in c.windows {
                                comp.destroy_window(id);
                            }
                        }
                        if comp.shell == Some(k) {
                            comp.shell = None;
                        }
                    }
                }
            }
        }
        let now = vrt::time::now_ns();
        while let Some(out) = comp.keyboard.poll_repeat(now) {
            comp.deliver_key(out);
        }
        if (!comp.damage.is_empty() || comp.animating()) && now >= comp.last_frame + FRAME_NS {
            comp.composite();
        }
    }
}
