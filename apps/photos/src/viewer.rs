//! The viewer: one picture, fitted to the window or zoomed and panned, with a toolbar,
//! navigation arrows, a filmstrip, an info panel and a full-screen mode.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vgfx::{Align, Bitmap, Color, Rect};
use vimage::Orientation;
use vproto::display::modifiers;
use vproto::input::keys;
use vui::{ButtonKind, Cursor, Font, Icon, Ui};

#[allow(unused_imports)]
use vmath::FloatExt;

use crate::catalog::{self, Thumb};
use crate::library::empty_state;
use crate::loader::{STRIP_H, STRIP_W};
use crate::render::{Placement, Quality, draw_rounded, rotated};
use crate::{Photos, Slot};

pub const TOOLBAR_H: i32 = 54;
pub const STRIP_BAR_H: i32 = 86;
const PANEL_W: i32 = 300;
const MAX_SCALE: f32 = 16.0;
/// Full-screen controls hide this long after the pointer stops moving.
const CONTROLS_NS: u64 = 2_500_000_000;
/// Drafts are refined once the view has been still for this long.
const SETTLE_NS: u64 = 180_000_000;
const STAGE_BG: u32 = 0x111115;

/// The area pictures are fitted into.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Viewport {
    pub w: i32,
    pub h: i32,
    /// Free space kept around a fitted picture.
    pub margin: i32,
}

/// An explicit zoom: scale and top-left corner of the picture in viewport pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Zoom {
    pub scale: f32,
    pub x: f32,
    pub y: f32,
}

/// How the picture on screen is shown.
#[derive(Debug, Default)]
pub(crate) struct View {
    /// `None`: fitted to the viewport (never enlarged beyond 100%).
    pub zoom: Option<Zoom>,
    /// Clockwise quarter turns (view only; the file is not changed).
    pub rot: u8,
    /// A pan in progress: pointer position and picture position when it started.
    pub drag: Option<(i32, i32, f32, f32)>,
    /// When the view last changed by zooming, panning or resizing.
    pub moved_at: u64,
}

impl View {
    /// The scale that fits the (rotated) picture into the viewport.
    pub fn fit(&self, (w, h): (u32, u32), vp: Viewport) -> f32 {
        let (rw, rh) = rotated(w.max(1), h.max(1), self.rot);
        let aw = (vp.w - 2 * vp.margin).max(1) as f32;
        let ah = (vp.h - 2 * vp.margin).max(1) as f32;
        (aw / rw as f32).min(ah / rh as f32).min(1.0)
    }

    /// Where the picture appears in the viewport.
    pub fn placement(&self, dims: (u32, u32), vp: Viewport) -> Placement {
        let (rw, rh) = rotated(dims.0.max(1), dims.1.max(1), self.rot);
        let (scale, x, y) = match self.zoom {
            Some(z) => (z.scale, z.x, z.y),
            None => {
                let s = self.fit(dims, vp);
                (s, 0.0, 0.0)
            }
        };
        let (x, y) = clamp_offset(x, y, rw as f32 * scale, rh as f32 * scale, vp);
        Placement { x, y, scale, rot: self.rot }
    }

    /// Zooms by `factor`, keeping the picture point under `anchor` (viewport pixels) in place.
    pub fn zoom_by(&mut self, factor: f32, anchor: (f32, f32), dims: (u32, u32), vp: Viewport, now: u64) {
        let scale = self.placement(dims, vp).scale * factor;
        self.zoom_to(scale, anchor, dims, vp, now);
    }

    /// Sets the scale (back to fitting when it gets there), keeping the point under `anchor`.
    pub fn zoom_to(&mut self, scale: f32, anchor: (f32, f32), dims: (u32, u32), vp: Viewport, now: u64) {
        let cur = self.placement(dims, vp);
        let fit = self.fit(dims, vp);
        let scale = scale.clamp(fit, MAX_SCALE);
        if scale <= fit * 1.001 {
            self.zoom = None;
        } else {
            let px = (anchor.0 - cur.x) / cur.scale;
            let py = (anchor.1 - cur.y) / cur.scale;
            self.zoom = Some(Zoom { scale, x: anchor.0 - px * scale, y: anchor.1 - py * scale });
            let p = self.placement(dims, vp);
            self.zoom = Some(Zoom { scale, x: p.x, y: p.y });
        }
        self.drag = None;
        self.moved_at = now;
    }

    /// Moves a zoomed picture so that its top-left corner is at (x, y), within limits.
    pub fn pan_to(&mut self, x: f32, y: f32, dims: (u32, u32), vp: Viewport, now: u64) {
        if let Some(z) = self.zoom {
            self.zoom = Some(Zoom { x, y, ..z });
            let p = self.placement(dims, vp);
            self.zoom = Some(Zoom { scale: z.scale, x: p.x, y: p.y });
            self.moved_at = now;
        }
    }
}

/// Centres a picture smaller than the viewport; keeps a larger one covering it.
fn clamp_offset(x: f32, y: f32, sw: f32, sh: f32, vp: Viewport) -> (f32, f32) {
    let (vw, vh) = (vp.w as f32, vp.h as f32);
    let x = if sw <= vw { (vw - sw) / 2.0 } else { x.clamp(vw - sw, 0.0) };
    let y = if sh <= vh { (vh - sh) / 2.0 } else { y.clamp(vh - sh, 0.0) };
    (x.round(), y.round())
}

/// Something the user asked for this frame (applied after drawing).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Prev,
    Next,
    First,
    Last,
    Go(usize),
    Library,
    Fullscreen(bool),
    /// Zoom around the viewport centre.
    ZoomBy(f32),
    Fit,
    /// 100%, around the pointer if it is over the picture.
    Actual,
    Rotate(i8),
    ToggleInfo,
    Wallpaper,
    Retry,
}

/// The next `w` pixels to the left of `x` (for right-aligned toolbar buttons).
fn take_left(x: &mut i32, w: i32, y: i32, h: i32) -> Rect {
    *x -= w;
    let r = Rect::new(*x, y, w, h);
    *x -= 4;
    r
}

impl Photos {
    pub(crate) fn viewer(&mut self, ui: &mut Ui) {
        if self.entries.is_empty() {
            self.close_viewer();
            self.library(ui);
            return;
        }
        self.current = self.current.min(self.entries.len() - 1);
        let now = ui.now();
        let full = ui.rect();
        let fullscreen = self.fullscreen;
        let (toolbar, rest) = if fullscreen { (Rect::default(), full) } else { full.split_top(TOOLBAR_H) };
        let (main, strip) = if fullscreen { (rest, Rect::default()) } else { rest.split_bottom(STRIP_BAR_H) };
        let with_panel = self.show_info && !fullscreen && main.w >= PANEL_W + 280;
        let (stage, panel) = if with_panel { main.split_right(PANEL_W) } else { (main, Rect::default()) };
        let vp = Viewport { w: stage.w, h: stage.h, margin: if fullscreen { 0 } else { 18 } };

        // The picture on screen first, then a preview of it, then the neighbours.
        self.want_picture(self.current, true);
        self.want_thumbs(self.current, true);
        self.entries[self.current].used = now;
        self.prefetch();
        let dims = self.dims();

        let near_top = ui.input.pointer.is_some_and(|(_, y)| y < 84);
        let controls = !fullscreen || now < self.pointer_moved_at + CONTROLS_NS || near_top;
        let mut actions = Vec::new();
        self.viewer_keys(ui, &mut actions);
        self.stage(ui, stage, vp, dims, controls, &mut actions);
        if fullscreen {
            self.fullscreen_controls(ui, full, controls, near_top, &mut actions);
        } else {
            self.toolbar(ui, toolbar, vp, dims, &mut actions);
            self.filmstrip(ui, strip, &mut actions);
            if with_panel {
                self.info_panel(ui, panel, vp, dims, &mut actions);
            }
        }
        for a in actions {
            self.apply(ui, a, stage, vp, dims);
        }
    }

    /// Size of the picture on screen, as soon as anything about it is known.
    fn dims(&self) -> Option<(u32, u32)> {
        let e = &self.entries[self.current];
        if let Some((_, Slot::Ready(p))) = self.pictures.iter().find(|(p, _)| *p == e.path) {
            return Some((p.width(), p.height()));
        }
        if let Some(m) = &e.meta {
            return Some((m.width, m.height));
        }
        if let Thumb::Ready(t) = &e.thumb {
            return Some((t.preview.width as u32, t.preview.height as u32));
        }
        None
    }

    fn viewer_keys(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        for k in &ui.input.keys {
            let shift = k.modifiers & modifiers::SHIFT != 0;
            if k.modifiers & (modifiers::CTRL | modifiers::ALT | modifiers::SUPER) != 0 {
                continue;
            }
            let a = match k.code {
                keys::LEFT | keys::PAGEUP | keys::BACKSPACE => Action::Prev,
                keys::RIGHT | keys::PAGEDOWN | keys::SPACE => Action::Next,
                keys::HOME => Action::First,
                keys::END => Action::Last,
                keys::ESC if self.fullscreen => Action::Fullscreen(false),
                keys::ESC => Action::Library,
                keys::F11 | keys::F => Action::Fullscreen(!self.fullscreen),
                keys::EQUAL | keys::KPPLUS => Action::ZoomBy(1.25),
                keys::MINUS | keys::KPMINUS => Action::ZoomBy(0.8),
                keys::KEY_0 | keys::KP0 => Action::Fit,
                keys::KEY_1 | keys::KP1 => Action::Actual,
                keys::R => Action::Rotate(if shift { -1 } else { 1 }),
                keys::I => Action::ToggleInfo,
                _ => continue,
            };
            actions.push(a);
        }
    }

    /// The picture (or its preview), loading and error states, the arrows, and zooming and
    /// panning with the pointer.
    fn stage(
        &mut self,
        ui: &mut Ui,
        stage: Rect,
        vp: Viewport,
        dims: Option<(u32, u32)>,
        controls: bool,
        actions: &mut Vec<Action>,
    ) {
        let now = ui.now();
        let bg = if self.fullscreen { Color::BLACK } else { Color::hex(STAGE_BG) };
        ui.canvas.fill_rect(stage, bg);
        let index = self.current;
        let mut loading = true;
        let mut failed = None;
        let mut damaged = false;
        {
            let path = &self.entries[index].path;
            let slot = self.pictures.iter().find(|(p, _)| p == path).map(|(_, s)| s);
            let source: Option<Source> = match slot {
                Some(Slot::Ready(p)) => {
                    loading = false;
                    damaged = p.damaged;
                    Some(Source {
                        id: Arc::as_ptr(p) as usize,
                        levels: &p.levels[..],
                        dims: (p.width(), p.height()),
                        alpha: p.has_alpha,
                    })
                }
                Some(Slot::Failed(m)) => {
                    loading = false;
                    failed = Some(m.clone());
                    None
                }
                _ => match &self.entries[index].thumb {
                    Thumb::Ready(th) => {
                        let d = dims.unwrap_or((th.preview.width as u32, th.preview.height as u32));
                        Some(Source {
                            id: th.preview.pixels.as_ptr() as usize,
                            levels: core::slice::from_ref(&th.preview),
                            dims: d,
                            alpha: th.has_alpha,
                        })
                    }
                    _ => None,
                },
            };
            if let Some(Source { id, levels, dims: d, alpha }) = source {
                let p = self.view.placement(d, vp);
                let visible = p.visible(d.0, d.1, vp.w, vp.h);
                let settled = self.view.drag.is_none() && now >= self.view.moved_at + SETTLE_NS;
                let quality = if settled { Quality::Fine } else { Quality::Draft };
                self.cache.update(id, levels, d, &p, visible, quality, alpha);
                self.cache.draw(&mut ui.canvas, (stage.x, stage.y));
                if self.cache.is_draft() {
                    ui.repaint_at(self.view.moved_at + SETTLE_NS);
                }
            }
        }
        let t = ui.theme().clone();
        if loading {
            let (cx, cy) = stage.center();
            ui.canvas.fill_circle(cx as f32, cy as f32, 30.0, Color::rgba(0, 0, 0, 120));
            ui.spinner(cx, cy, 16.0);
        }
        if let Some(msg) = &failed {
            let area = stage.inset(24, 0, 24, 40);
            let bottom = empty_state(ui, area, Icon::Warning, "Can't show this picture", msg);
            let b = Rect::new(area.x + (area.w - 132) / 2, bottom + 14, 132, 38);
            if ui.button_full(b, Some(Icon::Refresh), "Try again", ButtonKind::Secondary) {
                actions.push(Action::Retry);
            }
        }
        if damaged {
            let text = "Parts of this file are damaged";
            let w = ui.measure(text, Font::Regular, 13.0) as i32 + 46;
            let chip = Rect::new(stage.x + 16, stage.y + 14, w, 32);
            ui.canvas.fill_rounded_rect(chip, 16.0, Color::rgba(20, 20, 24, 210));
            ui.icon(Rect::new(chip.x + 10, chip.y, 18, chip.h), Icon::Warning, 15.0, t.warning);
            ui.label(Rect::new(chip.x + 34, chip.y, w - 40, chip.h), text, Font::Regular, 13.0, t.text, Align::Left);
        }

        // Previous / next arrows.
        let n = self.entries.len();
        let show = n > 1 && controls && (self.fullscreen || ui.hovered(stage));
        let fade = ui.animate(ui.id("arrows"), if show { 1.0 } else { 0.0 }, 6.0);
        let left = Rect::new(stage.x + 18, stage.y + stage.h / 2 - 25, 50, 50);
        let right = Rect::new(stage.right() - 68, stage.y + stage.h / 2 - 25, 50, 50);
        let mut over_arrow = false;
        if fade > 0.01 {
            if index > 0 {
                over_arrow |= ui.hovered(left);
                if arrow(ui, left, Icon::ChevronLeft, fade) {
                    actions.push(Action::Prev);
                }
            }
            if index + 1 < n {
                over_arrow |= ui.hovered(right);
                if arrow(ui, right, Icon::ChevronRight, fade) {
                    actions.push(Action::Next);
                }
            }
        }

        // Zooming with the wheel, panning by dragging, full screen with a double-click.
        let id = ui.id("stage");
        let resp = ui.interact(id, stage);
        let pointer = ui.input.pointer;
        if !over_arrow && let Some(d) = dims {
            if resp.double_clicked {
                actions.push(Action::Fullscreen(!self.fullscreen));
                self.view.drag = None;
            } else if resp.pressed
                && let (Some(z), Some((px, py))) = (self.view.zoom, pointer)
            {
                self.view.drag = Some((px, py, z.x, z.y));
            }
            if resp.hovered
                && ui.input.scroll.1 != 0
                && let Some((px, py)) = pointer
            {
                let factor = 1.2f32.powi(ui.input.scroll.1.clamp(-6, 6));
                self.view.zoom_by(factor, ((px - stage.x) as f32, (py - stage.y) as f32), d, vp, now);
            }
        }
        if let Some((sx, sy, ox, oy)) = self.view.drag {
            match (ui.input.down[0], pointer, dims) {
                (true, Some((px, py)), Some(d)) => {
                    self.view.pan_to(ox + (px - sx) as f32, oy + (py - sy) as f32, d, vp, now);
                    ui.set_cursor(Cursor::Move);
                }
                _ => {
                    self.view.drag = None;
                    self.view.moved_at = now;
                }
            }
        } else if resp.hovered && !over_arrow && self.view.zoom.is_some() {
            ui.set_cursor(Cursor::Move);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, bar: Rect, vp: Viewport, dims: Option<(u32, u32)>, actions: &mut Vec<Action>) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(bar, Color::hex(0x1E1E24));
        ui.canvas.fill_rect(Rect::new(bar.x, bar.bottom() - 1, bar.w, 1), t.border);
        let y = bar.y + (bar.h - 36) / 2;
        if ui.button_full(Rect::new(bar.x + 10, y, 128, 36), Some(Icon::ChevronLeft), "All photos", ButtonKind::Ghost) {
            actions.push(Action::Library);
        }
        let mut x = bar.right() - 12;
        let r = take_left(&mut x, 36, y, 36);
        if ui.icon_button(r, Icon::Fullscreen, "Full screen (F11)") {
            actions.push(Action::Fullscreen(true));
        }
        let r = take_left(&mut x, 36, y, 36);
        if self.show_info {
            ui.canvas.fill_rounded_rect(r, t.radius, t.accent.with_alpha(60));
        }
        if ui.icon_button(r, Icon::Info, "Details (I)") {
            actions.push(Action::ToggleInfo);
        }
        let r = take_left(&mut x, 36, y, 36);
        if ui.icon_button(r, Icon::Wallpaper, "Set as wallpaper") {
            actions.push(Action::Wallpaper);
        }
        x -= 8;
        ui.canvas.fill_rect(Rect::new(x, y + 8, 1, 20), t.border_strong);
        x -= 12;
        let r = take_left(&mut x, 36, y, 36);
        if ui.icon_button(r, Icon::RotateRight, "Rotate (R)") {
            actions.push(Action::Rotate(1));
        }
        x -= 8;
        ui.canvas.fill_rect(Rect::new(x, y + 8, 1, 20), t.border_strong);
        x -= 12;
        let r = take_left(&mut x, 36, y, 36);
        if ui.icon_button(r, Icon::ZoomIn, "Zoom in (+)") {
            actions.push(Action::ZoomBy(1.25));
        }
        let r = take_left(&mut x, 64, y, 36);
        let scale = dims.map(|d| self.view.placement(d, vp).scale);
        let label = scale.map(|s| format!("{}%", (s * 100.0 + 0.5) as i32)).unwrap_or_else(|| String::from("–"));
        if ui.button_full(r, None, &label, ButtonKind::Ghost) {
            actions.push(if self.view.zoom.is_none() { Action::Actual } else { Action::Fit });
        }
        let r = take_left(&mut x, 36, y, 36);
        if ui.icon_button(r, Icon::ZoomOut, "Zoom out (-)") {
            actions.push(Action::ZoomBy(0.8));
        }
        // Name and position in the folder.
        let name_x = bar.x + 152;
        let name_w = x - 16 - name_x;
        if name_w > 40 {
            let e = &self.entries[self.current];
            let mut sub = format!("{} of {}", self.current + 1, self.entries.len());
            if let Some((w, h)) = dims {
                sub.push_str(&format!("  ·  {w} × {h}"));
            }
            let name = e.name.clone();
            ui.label(Rect::new(name_x, bar.y + 8, name_w, 20), &name, Font::Bold, 14.0, t.text, Align::Left);
            ui.label(Rect::new(name_x, bar.y + 29, name_w, 16), &sub, Font::Regular, 12.0, t.text_dim, Align::Left);
        }
    }

    fn filmstrip(&mut self, ui: &mut Ui, bar: Rect, actions: &mut Vec<Action>) {
        let t = ui.theme().clone();
        let bar_bg = Color::hex(0x17171C);
        ui.canvas.fill_rect(bar, bar_bg);
        ui.canvas.fill_rect(Rect::new(bar.x, bar.y, bar.w, 1), t.border);
        let (sw, sh) = (STRIP_W as i32, STRIP_H as i32);
        let pitch = sw + 10;
        let n = self.entries.len() as i32;
        let cur = self.current as i32;
        // Centre the strip if it fits, otherwise centre the current picture.
        let total = n * pitch - 10;
        let x0 =
            if total <= bar.w - 40 { bar.x + (bar.w - total) / 2 } else { bar.x + bar.w / 2 - sw / 2 - cur * pitch };
        let y = bar.y + (bar.h - sh) / 2 + 1;
        ui.canvas.save();
        ui.canvas.clip_to(bar);
        for i in 0..n {
            let x = x0 + i * pitch;
            if x + sw < bar.x || x > bar.right() {
                continue;
            }
            let r = Rect::new(x, y, sw, sh);
            let id = ui.id("strip") ^ ((i as u64 + 1) << 24);
            let resp = ui.interact(id, r);
            let hover = ui.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 8.0);
            if resp.hovered {
                ui.set_cursor(Cursor::Hand);
            }
            if resp.clicked && i != cur {
                actions.push(Action::Go(i as usize));
            }
            self.want_thumbs(i as usize, false);
            self.entries[i as usize].used = ui.now();
            let selected = i == cur;
            let opacity = if selected { 255 } else { (140.0 + 100.0 * hover) as u8 };
            match &self.entries[i as usize].thumb {
                Thumb::Ready(th) => draw_rounded(&mut ui.canvas, &th.strip, x, y, 6, opacity),
                Thumb::Failed(_) => {
                    ui.canvas.fill_rounded_rect(r, 6.0, Color::hex(0x2A2A31));
                    ui.icon(r, Icon::Warning, 16.0, t.text_faint);
                }
                _ => ui.canvas.fill_rounded_rect(r, 6.0, Color::hex(0x2A2A31)),
            }
            if selected {
                ui.canvas.stroke_rounded_rect(r.inflate(3), 9.0, 2.0, t.accent);
            }
        }
        // Soft edges where the strip is cut off.
        if total > bar.w - 40 {
            let edge = 48;
            ui.canvas.fill_horizontal_gradient(
                Rect::new(bar.x, bar.y + 1, edge, bar.h - 1),
                bar_bg,
                bar_bg.with_alpha(0),
            );
            ui.canvas.fill_horizontal_gradient(
                Rect::new(bar.right() - edge, bar.y + 1, edge, bar.h - 1),
                bar_bg.with_alpha(0),
                bar_bg,
            );
        }
        ui.canvas.restore();
    }

    fn info_panel(
        &mut self,
        ui: &mut Ui,
        panel: Rect,
        vp: Viewport,
        dims: Option<(u32, u32)>,
        actions: &mut Vec<Action>,
    ) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(panel, Color::hex(0x1C1C22));
        ui.canvas.fill_rect(Rect::new(panel.x, panel.y, 1, panel.h), t.border);
        let inner = panel.inset(22, 18, 20, 20);
        ui.label(
            Rect::new(inner.x, inner.y, inner.w - 36, 30),
            "Details",
            Font::Bold,
            t.heading_size,
            t.text,
            Align::Left,
        );
        if ui.icon_button(Rect::new(inner.right() - 32, inner.y - 1, 32, 32), Icon::Close, "Close (I)") {
            actions.push(Action::ToggleInfo);
        }
        let e = &self.entries[self.current];
        let mut rows: Vec<(&str, String)> = Vec::new();
        rows.push(("Name", e.name.clone()));
        match &e.meta {
            Some(m) => {
                let mp = (m.width as u64 * m.height as u64) as f32 / 1_000_000.0;
                rows.push(("Dimensions", format!("{} × {} pixels  ·  {:.1} MP", m.width, m.height, mp)));
                let kind = match (m.format, m.progressive) {
                    (vimage::Format::Jpeg, true) => String::from("JPEG image, progressive"),
                    (vimage::Format::Png, true) => String::from("PNG image, interlaced"),
                    (f, _) => format!("{} image", f.name()),
                };
                rows.push(("Type", kind));
                rows.push(("File size", catalog::human_size(m.file_size)));
                rows.push(("Orientation", String::from(orientation_text(m.orientation))));
                rows.push(("Transparency", String::from(if m.has_alpha { "Yes" } else { "No" })));
            }
            None => {
                rows.push(("File size", if e.size > 0 { catalog::human_size(e.size) } else { String::from("–") }));
            }
        }
        rows.push(("Folder", catalog::split(&e.path).0));
        if let Some(d) = dims {
            let p = self.view.placement(d, vp);
            let mut v = format!("{}%", (p.scale * 100.0 + 0.5) as i32);
            if self.view.zoom.is_none() {
                v.push_str(", fitted");
            }
            if self.view.rot != 0 {
                v.push_str(&format!(", rotated {}°", self.view.rot as u32 * 90));
            }
            rows.push(("View", v));
        }
        let mut y = inner.y + 48;
        let button_top = inner.bottom() - 42;
        for (label, value) in rows {
            if y + 44 > button_top - 8 {
                break;
            }
            ui.label(Rect::new(inner.x, y, inner.w, 16), label, Font::Regular, 12.0, t.text_faint, Align::Left);
            ui.label(Rect::new(inner.x, y + 18, inner.w, 20), &value, Font::Regular, 14.0, t.text, Align::Left);
            y += 52;
        }
        let r = Rect::new(inner.x, button_top, inner.w, 42);
        let label = if self.wallpaper_busy { "Setting wallpaper…" } else { "Set as wallpaper" };
        if ui.button_full(r, Some(Icon::Wallpaper), label, ButtonKind::Primary) {
            actions.push(Action::Wallpaper);
        }
    }

    fn fullscreen_controls(
        &mut self,
        ui: &mut Ui,
        full: Rect,
        visible: bool,
        near_top: bool,
        actions: &mut Vec<Action>,
    ) {
        let now = ui.now();
        let fade = ui.animate(ui.id("fullscreen-controls"), if visible { 1.0 } else { 0.0 }, 5.0);
        if visible && !near_top {
            ui.repaint_at(self.pointer_moved_at + CONTROLS_NS);
        }
        if fade <= 0.01 {
            if self.view.drag.is_none() {
                ui.set_cursor(Cursor::Hidden);
            }
            return;
        }
        let _ = now;
        let t = ui.theme().clone();
        let bar = Rect::new(full.x, full.y, full.w, 84);
        ui.canvas.fill_vertical_gradient(bar, Color::rgba(0, 0, 0, (175.0 * fade) as u8), Color::rgba(0, 0, 0, 0));
        let e = &self.entries[self.current];
        let (name, sub) = (e.name.clone(), format!("{} of {}", self.current + 1, self.entries.len()));
        ui.label(
            Rect::new(full.x + 26, full.y + 16, full.w - 140, 22),
            &name,
            Font::Bold,
            15.0,
            Color::WHITE.fade(fade),
            Align::Left,
        );
        ui.label(
            Rect::new(full.x + 26, full.y + 39, full.w - 140, 18),
            &sub,
            Font::Regular,
            12.5,
            t.text_dim.fade(fade),
            Align::Left,
        );
        if fade > 0.5
            && ui.icon_button(Rect::new(full.right() - 58, full.y + 18, 40, 40), Icon::Close, "Exit full screen (Esc)")
        {
            actions.push(Action::Fullscreen(false));
        }
    }

    fn apply(&mut self, ui: &mut Ui, action: Action, stage: Rect, vp: Viewport, dims: Option<(u32, u32)>) {
        let now = ui.now();
        let n = self.entries.len();
        let centre = (vp.w as f32 / 2.0, vp.h as f32 / 2.0);
        match action {
            Action::Prev if self.current > 0 => self.go_to(self.current - 1),
            Action::Next if self.current + 1 < n => self.go_to(self.current + 1),
            Action::Prev | Action::Next => {}
            Action::First => self.go_to(0),
            Action::Last => self.go_to(n.saturating_sub(1)),
            Action::Go(i) => self.go_to(i),
            Action::Library => {
                self.set_fullscreen(ui, false);
                self.close_viewer();
            }
            Action::Fullscreen(on) => self.set_fullscreen(ui, on),
            Action::ZoomBy(f) => {
                if let Some(d) = dims {
                    self.view.zoom_by(f, centre, d, vp, now);
                }
            }
            Action::Fit => {
                self.view.zoom = None;
                self.view.moved_at = now;
            }
            Action::Actual => {
                if let Some(d) = dims {
                    let anchor = ui
                        .input
                        .pointer
                        .filter(|&(x, y)| stage.contains(x, y))
                        .map(|(x, y)| ((x - stage.x) as f32, (y - stage.y) as f32))
                        .unwrap_or(centre);
                    self.view.zoom_to(1.0, anchor, d, vp, now);
                }
            }
            Action::Rotate(dir) => {
                self.view.rot = (self.view.rot as i8 + dir).rem_euclid(4) as u8;
                self.view.zoom = None;
                self.view.moved_at = now;
            }
            Action::ToggleInfo => {
                self.show_info = !self.show_info;
                self.view.moved_at = now;
            }
            Action::Wallpaper => self.set_wallpaper(),
            Action::Retry => self.retry(),
        }
        ui.repaint();
    }
}

/// A round, translucent previous/next button; returns `true` when clicked.
fn arrow(ui: &mut Ui, r: Rect, icon: Icon, fade: f32) -> bool {
    let id = ui.id("arrow") ^ ((r.x as u64) << 20);
    let resp = ui.interact(id, r);
    let hover = ui.animate(id, if resp.hovered { 1.0 } else { 0.0 }, 8.0);
    let (cx, cy, rad) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0, r.w as f32 / 2.0);
    ui.canvas.fill_circle(cx, cy, rad + 1.0, Color::rgba(255, 255, 255, (28.0 * fade) as u8));
    ui.canvas.fill_circle(cx, cy, rad, Color::rgba(22, 22, 27, ((165.0 + 60.0 * hover) * fade) as u8));
    ui.icon(r, icon, 22.0, Color::WHITE.fade(fade * (0.8 + 0.2 * hover)));
    if resp.hovered {
        ui.set_cursor(Cursor::Hand);
    }
    resp.clicked
}

fn orientation_text(o: Orientation) -> &'static str {
    match o {
        Orientation::Normal => "Upright",
        Orientation::FlipHorizontal => "Mirrored (from EXIF)",
        Orientation::Rotate180 => "Turned 180° (from EXIF)",
        Orientation::FlipVertical => "Upside down, mirrored (from EXIF)",
        Orientation::Transpose => "Turned and mirrored (from EXIF)",
        Orientation::Rotate90 => "Turned 90° clockwise (from EXIF)",
        Orientation::Transverse => "Turned and mirrored (from EXIF)",
        Orientation::Rotate270 => "Turned 90° anticlockwise (from EXIF)",
    }
}

/// Pixels to show for the picture on screen: the decoded picture or its preview.
struct Source<'a> {
    /// Identifies the pixels (for the view cache).
    id: usize,
    levels: &'a [Bitmap],
    /// Full size of the picture.
    dims: (u32, u32),
    alpha: bool,
}
