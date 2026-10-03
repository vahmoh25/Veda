//! Tooltips for the taskbar. The taskbar is too short to draw them itself,
//! so a tooltip is a small window of the notification layer (which never
//! takes the keyboard focus) placed just above the hovered button.

use alloc::string::String;

use vgfx::{Align, Color, Rect};
use vproto::display::{WindowKind, WindowSpec};
use vui::window::Display;
use vui::{Font, Host};

use crate::taskbar;

/// How long the pointer must rest on a button before its tooltip appears.
pub const DELAY_NS: u64 = 600_000_000;
const HEIGHT: i32 = 32;
const PAD: i32 = 12;

/// What the taskbar wants to explain: a label for a button whose centre is
/// at `anchor_x` (screen coordinates), hovered since `since`.
#[derive(Debug, Clone, PartialEq)]
pub struct Tip {
    pub label: String,
    pub anchor_x: i32,
    /// Width of the label in pixels.
    pub text_width: i32,
    pub since: u64,
}

/// The tooltip window on screen.
pub struct Tooltip {
    host: Host,
    label: String,
}

impl Tooltip {
    /// Opens a tooltip for `tip` on a screen of `screen` size.
    pub fn open(display: &Display, screen: Rect, tip: &Tip) -> Option<Tooltip> {
        let w = tip.text_width + 2 * PAD;
        let x = (tip.anchor_x - w / 2).clamp(6, screen.w - w - 6);
        let y = screen.h - taskbar::HEIGHT - HEIGHT - 8;
        let mut spec = WindowSpec::new("Tooltip", w as u32, HEIGHT as u32);
        spec.kind = WindowKind::Notification;
        spec.x = x;
        spec.y = y;
        spec.resizable = false;
        spec.app_id = "shell".into();
        let host = Host::new(display, spec).ok()?;
        Some(Tooltip { host, label: tip.label.clone() })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn pump(&mut self) {
        let label = &self.label;
        self.host.pump(|ui| {
            let t = ui.theme().clone();
            let r = ui.rect();
            ui.canvas.clear(Color::TRANSPARENT);
            ui.canvas.fill_rounded_rect(r, 7.0, Color::hex(0x2B2B33));
            ui.canvas.stroke_rounded_rect(r, 7.0, 1.0, Color::rgba(255, 255, 255, 34));
            ui.label(r, label, Font::Regular, 13.0, t.text, Align::Center);
        });
    }
}
