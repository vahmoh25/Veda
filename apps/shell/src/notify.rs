//! Notification bubbles, stacked above the taskbar in the bottom-right
//! corner. They disappear after a few seconds (not while hovered) or when
//! clicked.
//!
//! The agent's approval requests appear here while its window is closed:
//! they carry Allow and Deny buttons and stay until answered (or until the
//! request expires).

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Rect};
use vproto::display::{WindowKind, WindowSpec};
use vrt::println;
use vui::window::Display;
use vui::{Font, Host, Icon, Ui};

use vproto::agent::ApprovalRequest;

use crate::{Action, Model, apps, chrome, taskbar};

const WIDTH: i32 = 360;
const HEIGHT: i32 = 88;
/// Height of an approval request.
const APPROVAL_HEIGHT: i32 = 136;
const GAP: i32 = 10;
const LIFETIME_NS: u64 = 6_000_000_000;
const MAX_SHOWN: usize = 4;

/// What a notification shows, and what happened to it this frame.
struct View {
    title: String,
    body: String,
    icon: Icon,
    /// Tile colour key.
    color: String,
    origin: (i32, i32),
    hovered: bool,
    clicked: bool,
    /// An approval request: its id and whether "always allow" is offered.
    approval: Option<(u64, bool)>,
    always: bool,
}

impl View {
    fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let t = ui.theme().clone();
        let (w, h) = (ui.width, ui.height);
        chrome::popup_background(&mut ui.canvas, &m.wallpaper.blurred, self.origin);
        if let Some((id, allow_always)) = self.approval {
            self.approval_view(ui, m, id, allow_always);
            chrome::popup_frame(&mut ui.canvas);
            return;
        }
        let resp = ui.interact(ui.id("note"), ui.rect());
        self.hovered = resp.hovered;
        self.clicked |= resp.clicked;
        apps::draw_tile(&mut ui.canvas, Rect::new(18, (h - 42) / 2, 42, 42), &self.color, self.icon);
        let text_w = w - 76 - 36;
        let font = ui.ctx.font(Font::Bold);
        let title = ui.ctx.text.ellipsize(font, 14.0, &self.title, text_w as f32);
        ui.label(Rect::new(76, 16, text_w, 22), &title, Font::Bold, 14.0, t.text, Align::Left);
        ui.paragraph(Rect::new(76, 40, w - 76 - 18, h - 46), &self.body, 13.0, t.text_dim);
        if resp.hovered {
            ui.icon(Rect::new(w - 34, 10, 22, 22), Icon::Close, 12.0, t.text_dim);
            ui.set_cursor(vui::Cursor::Hand);
        }
        chrome::popup_frame(&mut ui.canvas);
    }

    /// An approval request: what the agent wants to do, and the buttons.
    fn approval_view(&mut self, ui: &mut Ui, m: &mut Model, id: u64, allow_always: bool) {
        let t = ui.theme().clone();
        let w = ui.width;
        let h = ui.height;
        crate::agent::draw_circle(ui, 38.0, 38.0, 13.0, &m.agent, 0.0);
        let used = crate::agent::approval_title(ui, Rect::new(68, 14, w - 86, 22), &self.title);
        let dy = used - 22;
        ui.paragraph(Rect::new(68, 38 + dy, w - 86, 44 - dy), &self.body, 12.5, t.text_dim);
        let by = h - 44;
        if allow_always {
            ui.checkbox(Rect::new(14, by + 4, 130, 26), "Always allow", &mut self.always);
        }
        if ui.button(Rect::new(w - 192, by, 84, 32), "Deny") {
            m.push(Action::AgentDecide { id, allow: false, always: false });
            self.clicked = true;
        }
        if ui.primary_button(Rect::new(w - 100, by, 84, 32), "Allow") {
            m.push(Action::AgentDecide { id, allow: true, always: self.always });
            self.clicked = true;
        }
    }
}

struct Note {
    host: Host,
    view: View,
    expires: u64,
}

impl Note {
    fn height(&self) -> i32 {
        if self.view.approval.is_some() { APPROVAL_HEIGHT } else { HEIGHT }
    }
}

pub struct Notifications {
    notes: Vec<Note>,
}

impl Notifications {
    pub fn new() -> Notifications {
        Notifications { notes: Vec::new() }
    }

    /// Where a note goes: stacked upwards from above the taskbar.
    fn origin_for(&self, screen: Rect, slot: usize, height: i32) -> (i32, i32) {
        let below: i32 = self.notes.iter().take(slot).map(|n| n.height() + GAP).sum();
        (screen.w - WIDTH - 14, screen.h - taskbar::HEIGHT - 14 - height - below)
    }

    /// Shows an approval request from the agent (until it is answered).
    pub fn post_approval(&mut self, display: &Display, m: &Model, req: &ApprovalRequest) {
        if self.notes.iter().any(|n| n.view.approval.is_some_and(|(id, _)| id == req.id)) {
            return;
        }
        let origin = self.origin_for(m.screen, self.notes.len(), APPROVAL_HEIGHT);
        let mut spec = WindowSpec::new("Approval", WIDTH as u32, APPROVAL_HEIGHT as u32);
        spec.kind = WindowKind::Notification;
        spec.x = origin.0;
        spec.y = origin.1;
        spec.resizable = false;
        spec.app_id = "shell".into();
        match Host::new(display, spec) {
            Ok(host) => {
                let view = View {
                    title: req.action.clone(),
                    body: req.detail.clone(),
                    icon: Icon::Info,
                    color: String::new(),
                    origin,
                    hovered: false,
                    clicked: false,
                    approval: Some((req.id, req.allow_always)),
                    always: false,
                };
                self.notes.push(Note { host, view, expires: u64::MAX });
            }
            Err(e) => println!("cannot show an approval request: {:?}", e),
        }
    }

    /// Takes an answered or expired approval request away.
    pub fn remove_approval(&mut self, id: u64, screen: Rect) {
        let before = self.notes.len();
        self.notes.retain(|n| n.view.approval.is_none_or(|(a, _)| a != id));
        if self.notes.len() != before {
            self.relayout(screen);
        }
    }

    /// Takes every approval request away (the agent's window shows them).
    pub fn remove_approvals(&mut self, screen: Rect) {
        let before = self.notes.len();
        self.notes.retain(|n| n.view.approval.is_none());
        if self.notes.len() != before {
            self.relayout(screen);
        }
    }

    /// Shows a notification. `icon` is an icon name (see `vui::Icon::by_name`).
    pub fn post(&mut self, display: &Display, m: &Model, title: &str, body: &str, icon: &str) {
        if self.notes.len() >= MAX_SHOWN
            && let Some(i) = self.notes.iter().position(|n| n.view.approval.is_none())
        {
            self.notes.remove(i);
            self.relayout(m.screen);
        }
        let origin = self.origin_for(m.screen, self.notes.len(), HEIGHT);
        let mut spec = WindowSpec::new(title, WIDTH as u32, HEIGHT as u32);
        spec.kind = WindowKind::Notification;
        spec.x = origin.0;
        spec.y = origin.1;
        spec.resizable = false;
        spec.app_id = "shell".into();
        match Host::new(display, spec) {
            Ok(host) => {
                let view = View {
                    title: title.into(),
                    body: body.into(),
                    // An installed application's id shows its own tile.
                    icon: m.app(icon).map(apps::icon_for).or_else(|| Icon::by_name(icon)).unwrap_or(Icon::Info),
                    color: icon.into(),
                    origin,
                    hovered: false,
                    clicked: false,
                    approval: None,
                    always: false,
                };
                self.notes.push(Note { host, view, expires: vrt::time::now_ns() + LIFETIME_NS });
            }
            Err(e) => println!("cannot show a notification: {:?}", e),
        }
    }

    /// Moves the notifications to their slots (after one went away).
    fn relayout(&mut self, screen: Rect) {
        let origins: Vec<(i32, i32)> =
            (0..self.notes.len()).map(|i| self.origin_for(screen, i, self.notes[i].height())).collect();
        for (n, o) in self.notes.iter_mut().zip(origins) {
            if o != n.view.origin {
                n.view.origin = o;
                n.host.window.set_position(o.0, o.1);
                n.host.invalidate();
            }
        }
    }

    pub fn pump(&mut self, m: &mut Model) {
        let now = vrt::time::now_ns();
        for n in &mut self.notes {
            n.host.pump(|ui| n.view.update(ui, m));
            if n.view.hovered {
                n.expires = n.expires.max(now + 2_000_000_000);
            }
        }
        let before = self.notes.len();
        self.notes.retain(|n| !n.view.clicked && now < n.expires && !n.host.window.closed);
        if self.notes.len() != before {
            self.relayout(m.screen);
        }
    }

    pub fn hosts(&self) -> impl Iterator<Item = &Host> {
        self.notes.iter().map(|n| &n.host)
    }

    /// When the next notification expires.
    pub fn deadline(&self) -> u64 {
        self.notes.iter().map(|n| n.expires).min().unwrap_or(vabi::DEADLINE_INFINITE)
    }

    pub fn invalidate_all(&mut self) {
        for n in &mut self.notes {
            n.host.invalidate();
        }
    }
}
