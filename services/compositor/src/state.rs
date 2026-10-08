//! The window manager's state: windows and their stacking order, focus,
//! window states and snapping, the Alt+Tab switcher, and the shell's
//! special windows.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Damage, Rect};
use vproto::display::{self as dp, Cursor, WindowEvent, WindowKind, WindowSpec, WindowState};
use vproto::input::keys;
use vrt::object::Channel;

use crate::decor::{Decor, DecorState};
use crate::gpu::{Gpu, Pending};
use crate::keymap::Keyboard;
use crate::screen::Screen;
use crate::startup::Startup;
use crate::switcher::{self, Switcher};
use crate::window::{Anim, AnimKind, Part, TITLE_HEIGHT, Window};

/// An interactive move or resize in progress.
#[derive(Clone, Copy)]
pub(crate) enum Drag {
    /// Moving by the title bar. `restore`: the window is maximised or
    /// snapped and returns to its normal size once the pointer really moves
    /// away from `origin`.
    Move {
        id: u32,
        dx: i32,
        dy: i32,
        origin: (i32, i32),
        restore: bool,
    },
    Resize {
        id: u32,
        edge: (i8, i8),
        start: Rect,
        px: i32,
        py: i32,
    },
    /// Pointer grab by a client (button held inside its client area).
    Client {
        id: u32,
    },
    /// Pressing a caption button (activates on release over it).
    Button {
        id: u32,
        part: Part,
    },
}

/// Where a dragged window snaps when released at a screen edge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Snap {
    Maximize,
    Left,
    Right,
}

impl Snap {
    /// The part of the work area `area` that a window snapped here fills
    /// (title bar included).
    pub(crate) fn rect(self, area: Rect) -> Rect {
        let half = area.w / 2;
        match self {
            Snap::Maximize => area,
            Snap::Left => Rect::new(area.x, area.y, half, area.h),
            Snap::Right => Rect::new(area.x + half, area.y, area.w - half, area.h),
        }
    }
}

pub(crate) struct DisplayClient {
    pub(crate) channel: Channel,
    pub(crate) windows: Vec<u32>,
}

pub(crate) struct Compositor {
    pub(crate) screen: Screen,
    pub(crate) decor: Decor,
    pub(crate) windows: BTreeMap<u32, Window>,
    /// Bottom-to-top stacking order of normal windows.
    pub(crate) order: Vec<u32>,
    pub(crate) focused: Option<u32>,
    pub(crate) next_id: u32,
    pub(crate) damage: Damage,
    pub(crate) clients: BTreeMap<u64, DisplayClient>,
    pub(crate) shell: Option<u64>,
    pub(crate) keyboard: Keyboard,
    pub(crate) pointer: (i32, i32),
    pub(crate) cursor_rect: Rect,
    pub(crate) cursor_shape: Cursor,
    pub(crate) buttons: u8,
    pub(crate) drag: Option<Drag>,
    pub(crate) decor_state: DecorState,
    pub(crate) hover_window: Option<u32>,
    pub(crate) last_click: (u64, u32, u8, i32, i32),
    pub(crate) click_count: u8,
    pub(crate) clipboard: String,
    pub(crate) last_frame: u64,
    pub(crate) work_area: Rect,
    /// The Alt+Tab switcher while Alt is held.
    pub(crate) switcher: Option<Switcher>,
    /// Snap target (and its preview rectangle) of the window being moved.
    pub(crate) snap: Option<(Snap, Rect)>,
    /// Windows minimised by Super+D, restored by the next Super+D.
    pub(crate) desktop_shown: Vec<u32>,
    /// The startup sequence, while it runs (see [`Startup`]).
    pub(crate) startup: Option<Startup>,
    /// The GPU, while it composes (see `gpu`); one set up and waiting for
    /// its check; its setup under way; whether the display's pictures
    /// have been offered to it.
    pub(crate) gpu: Option<Gpu>,
    pub(crate) gpu_ready: Option<Gpu>,
    pub(crate) gpu_setup: Option<Pending>,
    /// The driver whose pictures the GPU was set up for (its connection).
    pub(crate) gpu_tried: Option<u64>,
}

/// The client rectangle of an automatically placed `w` x `h` window, with a
/// title bar `tb` high: centred in the work area `area`, its top-left corner
/// kept inside when the window is larger than the area.
fn auto_rect(area: Rect, w: i32, h: i32, tb: i32) -> Rect {
    let x = area.x + (area.w - w) / 2;
    let y = area.y + (area.h - h - tb) / 2;
    Rect::new(x.max(area.x), y.max(area.y) + tb, w, h)
}

/// The stacking layer of a window (higher layers are drawn on top). The
/// focused full-screen window covers the panels; when another window takes
/// the focus it drops back among the normal windows.
pub(crate) fn layer(w: &Window, focused: bool) -> u8 {
    match w.kind {
        WindowKind::Desktop => 0,
        WindowKind::Normal | WindowKind::Borderless if focused && w.state == WindowState::Fullscreen => 3,
        WindowKind::Normal | WindowKind::Borderless => 1,
        WindowKind::Panel => 2,
        WindowKind::Popup => 4,
        WindowKind::Notification => 5,
    }
}

impl Compositor {
    pub(crate) fn send(&self, id: u32, ev: WindowEvent) {
        if let Some(w) = self.windows.get(&id) {
            let _ = vipc::send_event(&w.events, dp::EVENT, ev);
        }
    }

    /// Windows in paint order (bottom to top).
    pub(crate) fn paint_order(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.order.clone();
        v.sort_by_key(|id| self.windows.get(id).map(|w| layer(w, self.focused == Some(*id))).unwrap_or(0));
        v
    }

    pub(crate) fn damage_window(&mut self, id: u32) {
        if let Some(w) = self.windows.get(&id) {
            let r = w.paint_bounds();
            self.damage.add(r.inflate(2));
        }
    }

    pub(crate) fn screen_rect(&self) -> Rect {
        self.screen.rect()
    }

    /// Whether the desktop has drawn itself: the shell's desktop window and
    /// its panels (the taskbar) have each presented a frame.
    pub(crate) fn desktop_ready(&self) -> bool {
        let Some(desktop) = self.windows.values().find(|w| w.kind == WindowKind::Desktop) else { return false };
        desktop.current.is_some()
            && self
                .windows
                .values()
                .filter(|w| w.kind == WindowKind::Panel && w.client == desktop.client)
                .all(|w| w.current.is_some())
    }

    /// Recomputes the work area (the screen minus the panels) and fits the
    /// windows to it if it changed.
    pub(crate) fn update_work_area(&mut self) {
        let s = self.screen_rect();
        let old = self.work_area;
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
        if area != old {
            self.fit_to_work_area();
        }
    }

    /// After the work area changed (a panel appeared or went away):
    /// maximised and snapped windows take their new space, and windows that
    /// were placed automatically and never moved are placed again — so
    /// where a window opens does not depend on whether the taskbar existed
    /// yet.
    fn fit_to_work_area(&mut self) {
        let area = self.work_area;
        let mut configure = Vec::new();
        for w in self.windows.values_mut() {
            if !matches!(w.kind, WindowKind::Normal | WindowKind::Borderless) || w.closing {
                continue;
            }
            let tb = if w.kind == WindowKind::Normal { TITLE_HEIGHT } else { 0 };
            if w.auto_placed {
                w.restore_rect = auto_rect(area, w.restore_rect.w, w.restore_rect.h, tb);
            }
            let target = match (w.state, w.snapped) {
                (WindowState::Maximized, _) => Rect::new(area.x, area.y + TITLE_HEIGHT, area.w, area.h - TITLE_HEIGHT),
                (WindowState::Normal, Some(snap)) => {
                    let r = snap.rect(area);
                    Rect::new(r.x, r.y + TITLE_HEIGHT, r.w, r.h - TITLE_HEIGHT)
                }
                (WindowState::Normal, None) if w.auto_placed => w.restore_rect,
                _ => continue,
            };
            if target == w.client_rect {
                continue;
            }
            let before = w.paint_bounds();
            let resized = (target.w, target.h) != (w.client_rect.w, w.client_rect.h);
            w.client_rect = target;
            self.damage.add(before);
            self.damage.add(w.paint_bounds());
            if resized {
                configure.push((w.id, target, w.state));
            }
        }
        for (id, r, state) in configure {
            self.send(id, WindowEvent::Configure { width: r.w as u32, height: r.h as u32, state });
        }
    }

    pub(crate) fn notify_shell(&self) {
        if let Some(sc) = self.shell {
            for (id, w) in &self.windows {
                if w.client == sc && w.kind == WindowKind::Panel {
                    self.send(*id, WindowEvent::WindowsChanged {});
                }
            }
        }
    }

    pub(crate) fn raise(&mut self, id: u32) {
        self.order.retain(|&x| x != id);
        self.order.push(id);
        self.damage_window(id);
    }

    pub(crate) fn focus(&mut self, id: Option<u32>) {
        if self.focused == id {
            return;
        }
        if let Some(old) = self.focused {
            self.send(old, WindowEvent::Focus { focused: false });
            self.damage_window(old);
            // Popups close when they lose focus.
            if let Some(w) = self.windows.get(&old)
                && w.kind == WindowKind::Popup
            {
                self.send(old, WindowEvent::CloseRequested {});
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
    pub(crate) fn focus_top(&mut self) {
        let top = self.order.iter().rev().copied().find(|id| {
            self.windows
                .get(id)
                .is_some_and(|w| w.kind == WindowKind::Normal && w.state != WindowState::Minimized && !w.closing)
        });
        self.focus(top);
    }

    /// Where a new window goes: where the client asked, or in the centre of
    /// the work area. Returns the client rectangle and whether the window
    /// was placed automatically.
    pub(crate) fn place_new_window(&self, spec: &WindowSpec) -> (Rect, bool) {
        let (w, h) = (spec.width as i32, spec.height as i32);
        let tb = if spec.kind == WindowKind::Normal { TITLE_HEIGHT } else { 0 };
        if spec.x != i32::MIN && spec.y != i32::MIN {
            return (Rect::new(spec.x, spec.y + tb, w, h), false);
        }
        match spec.kind {
            WindowKind::Desktop => (self.screen_rect(), false),
            _ => (auto_rect(self.work_area, w, h, tb), true),
        }
    }

    pub(crate) fn set_state(&mut self, id: u32, state: WindowState) {
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
                // A snapped window keeps the size it had before snapping.
                if old == WindowState::Normal && w.snapped.is_none() {
                    w.restore_rect = w.client_rect;
                }
                w.snapped = None;
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

    /// A maximised or snapped window starts being dragged: it returns to its
    /// normal size, keeping the grabbed point of the title bar under the
    /// pointer.
    pub(crate) fn unsnap_for_drag(&mut self, id: u32, origin: (i32, i32), (x, y): (i32, i32)) {
        let Some(w) = self.windows.get(&id) else { return };
        let frac = (origin.0 - w.client_rect.x) as f32 / w.client_rect.w.max(1) as f32;
        let rest = w.restore_rect;
        if w.state == WindowState::Maximized {
            self.set_state(id, WindowState::Normal);
        }
        let Some(w) = self.windows.get_mut(&id) else { return };
        let before = w.paint_bounds();
        w.snapped = None;
        w.auto_placed = false;
        w.client_rect = Rect::new(x - (rest.w as f32 * frac) as i32, y + TITLE_HEIGHT / 2, rest.w, rest.h);
        let (c, st) = (w.client_rect, w.state);
        let after = w.paint_bounds();
        self.damage.add(before);
        self.damage.add(after);
        self.send(id, WindowEvent::Configure { width: c.w as u32, height: c.h as u32, state: st });
        self.drag = Some(Drag::Move { id, dx: x - c.x, dy: y - c.y, origin, restore: false });
    }

    /// Updates the snap preview for a window dragged to (x, y).
    pub(crate) fn update_snap(&mut self, id: u32, x: i32, y: i32) {
        let area = self.work_area;
        let resizable = self.windows.get(&id).is_some_and(|w| w.resizable);
        let zone = match () {
            _ if !resizable => None,
            _ if y <= area.y + 1 => Some(Snap::Maximize),
            _ if x <= area.x + 1 => Some(Snap::Left),
            _ if x >= area.right() - 2 => Some(Snap::Right),
            _ => None,
        };
        let target = zone.map(|z| (z, z.rect(area)));
        if target != self.snap {
            for (_, r) in [self.snap, target].into_iter().flatten() {
                self.damage.add(r.inflate(8));
            }
            self.snap = target;
        }
    }

    /// Snaps a window into `r` (the whole work area or one half of it).
    pub(crate) fn apply_snap(&mut self, id: u32, snap: Snap, r: Rect) {
        if snap == Snap::Maximize {
            self.set_state(id, WindowState::Maximized);
            return;
        }
        let Some(w) = self.windows.get_mut(&id) else { return };
        if w.snapped.is_none() {
            w.restore_rect = w.client_rect;
        }
        w.snapped = Some(snap);
        w.auto_placed = false;
        let before = w.paint_bounds();
        w.client_rect = Rect::new(r.x, r.y + TITLE_HEIGHT, r.w, r.h - TITLE_HEIGHT);
        let (c, st) = (w.client_rect, w.state);
        let after = w.paint_bounds();
        self.damage.add(before);
        self.damage.add(after);
        self.send(id, WindowEvent::Configure { width: c.w as u32, height: c.h as u32, state: st });
    }

    pub(crate) fn destroy_window(&mut self, id: u32) {
        self.damage_window(id);
        self.windows.remove(&id);
        if let Some(g) = &mut self.gpu {
            g.forget_window(id);
        }
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
        if matches!(self.drag, Some(Drag::Move { id: d, .. } | Drag::Resize { id: d, .. } | Drag::Client { id: d } | Drag::Button { id: d, .. }) if d == id)
        {
            self.drag = None;
        }
        self.update_work_area();
        self.notify_shell();
    }

    /// Super+arrows: Left/Right snap the focused window to that half of the
    /// screen, Up maximises it, Down restores it or, if it is already in its
    /// normal place, minimises it.
    pub(crate) fn arrange_focused(&mut self, code: u16) {
        let Some(id) = self.focused else { return };
        let Some(w) = self.windows.get(&id) else { return };
        if w.kind != WindowKind::Normal {
            return;
        }
        let (state, snapped, resizable) = (w.state, w.snapped.is_some(), w.resizable);
        let area = self.work_area;
        match code {
            keys::LEFT | keys::RIGHT if resizable => {
                if state == WindowState::Maximized {
                    self.set_state(id, WindowState::Normal);
                }
                let snap = if code == keys::LEFT { Snap::Left } else { Snap::Right };
                self.apply_snap(id, snap, snap.rect(area));
            }
            keys::UP if resizable => self.set_state(id, WindowState::Maximized),
            keys::DOWN if state == WindowState::Maximized => self.set_state(id, WindowState::Normal),
            keys::DOWN if snapped => {
                let Some(w) = self.windows.get_mut(&id) else { return };
                let before = w.paint_bounds();
                w.snapped = None;
                w.client_rect = w.restore_rect;
                let (c, st) = (w.client_rect, w.state);
                let after = w.paint_bounds();
                self.damage.add(before);
                self.damage.add(after);
                self.send(id, WindowEvent::Configure { width: c.w as u32, height: c.h as u32, state: st });
            }
            keys::DOWN => self.set_state(id, WindowState::Minimized),
            _ => {}
        }
    }

    /// Super+D: minimises every window, or brings them back if the desktop
    /// is already showing.
    pub(crate) fn toggle_desktop(&mut self) {
        let visible: Vec<u32> = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| w.kind == WindowKind::Normal && w.state != WindowState::Minimized && !w.closing)
            })
            .collect();
        if visible.is_empty() {
            for id in core::mem::take(&mut self.desktop_shown) {
                if self.windows.contains_key(&id) {
                    self.set_state(id, WindowState::Normal);
                }
            }
        } else {
            for &id in &visible {
                self.set_state(id, WindowState::Minimized);
            }
            self.desktop_shown = visible;
        }
    }

    pub(crate) fn send_to_shell(&self, ev: WindowEvent) {
        if let Some(sc) = self.shell
            && let Some((id, _)) = self.windows.iter().find(|(_, w)| w.client == sc && w.kind == WindowKind::Panel)
        {
            self.send(*id, ev);
        }
    }

    /// Alt+Tab: opens the window switcher or moves its selection.
    pub(crate) fn switcher_step(&mut self, backwards: bool) {
        if let Some(s) = &mut self.switcher {
            s.step(backwards);
            let b = s.bounds();
            self.damage.add(b);
            return;
        }
        // Most recently used first (the stacking order is kept that way).
        let windows: Vec<u32> = self
            .order
            .iter()
            .rev()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(|w| w.kind == WindowKind::Normal && !w.closing))
            .collect();
        if windows.is_empty() {
            return;
        }
        let thumbs = windows.iter().map(|id| switcher::thumbnail(&self.windows[id])).collect();
        let n = windows.len();
        let selected = match (n, backwards) {
            (1, _) => 0,
            (_, true) => n - 1,
            (_, false) => 1,
        };
        let s = Switcher::new(windows, thumbs, selected, self.screen_rect());
        self.damage.add(s.bounds());
        self.switcher = Some(s);
    }

    /// Closes the switcher, activating the selected window if `commit`.
    pub(crate) fn close_switcher(&mut self, commit: bool) {
        let Some(s) = self.switcher.take() else { return };
        self.damage.add(s.bounds());
        let id = s.windows[s.selected];
        if !commit || !self.windows.contains_key(&id) {
            return;
        }
        if self.windows[&id].state == WindowState::Minimized {
            self.set_state(id, WindowState::Normal);
        }
        self.raise(id);
        self.focus(Some(id));
    }
}
