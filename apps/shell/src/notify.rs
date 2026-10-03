//! Notification bubbles, stacked above the taskbar in the bottom-right
//! corner. They disappear after a few seconds (not while hovered) or when
//! clicked.

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Rect};
use vproto::display::{WindowKind, WindowSpec};
use vrt::println;
use vui::window::Display;
use vui::{Font, Host, Icon, Ui};

use crate::{Model, apps, chrome, taskbar};

const WIDTH: i32 = 360;
const HEIGHT: i32 = 88;
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
}

impl View {
    fn update(&mut self, ui: &mut Ui, m: &Model) {
        let t = ui.theme().clone();
        let (w, h) = (ui.width, ui.height);
        chrome::popup_background(&mut ui.canvas, &m.wallpaper.blurred, self.origin);
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
}

struct Note {
    host: Host,
    view: View,
    expires: u64,
}

pub struct Notifications {
    notes: Vec<Note>,
}

impl Notifications {
    pub fn new() -> Notifications {
        Notifications { notes: Vec::new() }
    }

    fn origin(screen: Rect, slot: usize) -> (i32, i32) {
        (screen.w - WIDTH - 14, screen.h - taskbar::HEIGHT - 14 - HEIGHT - slot as i32 * (HEIGHT + GAP))
    }

    /// Shows a notification. `icon` is an icon name (see `vui::Icon::by_name`).
    pub fn post(&mut self, display: &Display, m: &Model, title: &str, body: &str, icon: &str) {
        if self.notes.len() >= MAX_SHOWN {
            self.notes.remove(0);
            self.relayout(m.screen);
        }
        let origin = Self::origin(m.screen, self.notes.len());
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
                    icon: Icon::by_name(icon).unwrap_or(Icon::Info),
                    color: icon.into(),
                    origin,
                    hovered: false,
                    clicked: false,
                };
                self.notes.push(Note { host, view, expires: vrt::time::now_ns() + LIFETIME_NS });
            }
            Err(e) => println!("cannot show a notification: {:?}", e),
        }
    }

    /// Moves the notifications to their slots (after one went away).
    fn relayout(&mut self, screen: Rect) {
        for (i, n) in self.notes.iter_mut().enumerate() {
            let o = Self::origin(screen, i);
            if o != n.view.origin {
                n.view.origin = o;
                n.host.window.set_position(o.0, o.1);
                n.host.invalidate();
            }
        }
    }

    pub fn pump(&mut self, m: &Model) {
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
