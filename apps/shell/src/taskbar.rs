//! The taskbar: start button, pinned and running applications in the middle,
//! and the network, volume and clock buttons on the right.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vproto::display::WindowState;
use vui::{Font, Icon, Ui};

use crate::tooltip::Tip;
use crate::{Action, DISMISS_GRACE_NS, Model, apps, chrome, volume, wifi};

/// Height of the taskbar in pixels.
pub const HEIGHT: i32 = 48;
/// Width of one button slot.
const SLOT: i32 = 46;

/// A window as listed under its taskbar button.
struct Win {
    id: u32,
    title: String,
    focused: bool,
    minimized: bool,
}

/// One button: an app (pinned and/or running) or a lone window.
struct Item {
    key: String,
    /// App id (selects the tile colour and what to launch).
    app: String,
    /// Display name of the app.
    name: String,
    icon: Icon,
    windows: Vec<Win>,
}

impl Item {
    /// The button's tooltip: the window title, the app's name, or both.
    fn tip(&self) -> String {
        match self.windows.as_slice() {
            [] => self.name.clone(),
            [w] => w.title.clone(),
            ws => format!("{} \u{2014} {} windows", self.name, ws.len()),
        }
    }
}

fn items(m: &Model) -> Vec<Item> {
    let mut items: Vec<Item> = m
        .apps
        .iter()
        .filter(|a| a.pinned)
        .map(|a| Item {
            key: a.id.clone(),
            app: a.id.clone(),
            name: a.name.clone(),
            icon: apps::icon_for(a),
            windows: Vec::new(),
        })
        .collect();
    for w in &m.windows {
        let key = if w.app_id.is_empty() { format!("#{}", w.id) } else { w.app_id.clone() };
        let win =
            Win { id: w.id, title: w.title.clone(), focused: w.focused, minimized: w.state == WindowState::Minimized };
        if let Some(it) = items.iter_mut().find(|i| i.key == key) {
            it.windows.push(win);
        } else {
            let app = m.app(&w.app_id);
            let icon = app.map(apps::icon_for).or_else(|| Icon::by_name(&w.app_id)).unwrap_or(Icon::Grid);
            let name = app.map_or_else(|| w.title.clone(), |a| a.name.clone());
            items.push(Item { key, app: w.app_id.clone(), name, icon, windows: alloc::vec![win] });
        }
    }
    items
}

pub struct Taskbar {
    start_pressed_at: u64,
    clock_pressed_at: u64,
    volume_pressed_at: u64,
    network_pressed_at: u64,
}

impl Taskbar {
    pub fn new() -> Taskbar {
        Taskbar { start_pressed_at: 0, clock_pressed_at: 0, volume_pressed_at: 0, network_pressed_at: 0 }
    }

    /// Hover/press/active background of a button.
    fn button_bg(ui: &mut Ui, r: Rect, hovered: bool, held: bool, active: bool) {
        let a = if held {
            12
        } else if hovered {
            24
        } else if active {
            16
        } else {
            0
        };
        if a > 0 {
            ui.canvas.fill_rounded_rect(r, 6.0, Color::rgba(255, 255, 255, a));
        }
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let (w, h) = (ui.width, ui.height);
        let t = ui.theme().clone();
        chrome::mica(&mut ui.canvas, &m.wallpaper.blurred, (0, m.screen.h - HEIGHT), Color::rgba(20, 20, 28, 200));
        ui.canvas.fill_rect(Rect::new(0, 0, w, 1), Color::rgba(255, 255, 255, 26));

        let items = items(m);
        let total = (items.len() as i32 + 1) * SLOT;
        let mut x = (w - total) / 2;
        // The hovered button's tooltip and the centre of the button.
        let mut tip: Option<(String, i32)> = None;

        // Start button.
        let r = Rect::new(x, 4, SLOT - 2, h - 8);
        let resp = ui.interact(ui.id("start"), r);
        Self::button_bg(ui, r, resp.hovered, resp.held, m.start_open);
        if resp.hovered {
            tip = Some(("Start".into(), r.center().0));
        }
        let logo = if resp.held { 20 } else { 22 };
        vui::draw_logo(&mut ui.canvas, r.centered(logo, logo));
        if resp.pressed {
            self.start_pressed_at = ui.now();
        }
        // A click that dismissed the open menu must not reopen it.
        if resp.clicked && m.start_dismissed_at + DISMISS_GRACE_NS < self.start_pressed_at {
            m.push(Action::ToggleStart);
        }
        x += SLOT;

        // Applications.
        for item in &items {
            let r = Rect::new(x, 4, SLOT - 2, h - 8);
            let id = ui.id(&item.key);
            let resp = ui.interact(id, r);
            let focused = item.windows.iter().any(|w| w.focused && !w.minimized);
            Self::button_bg(ui, r, resp.hovered, resp.held, focused);
            let size = if resp.held { 24 } else { 26 };
            let tile = Rect::new(r.x + (r.w - size) / 2, r.y + 6 + (26 - size) / 2, size, size);
            apps::draw_tile(&mut ui.canvas, tile, &item.app, item.icon);
            if !item.windows.is_empty() {
                let k = ui.animate(id ^ 0x1d1c, if focused { 1.0 } else { 0.0 }, 7.0);
                let iw = 6.0 + 10.0 * k;
                let color = Color::rgba(200, 200, 215, 255).lerp(t.accent, k);
                let ir = Rect::new(r.x + (r.w - iw as i32) / 2, r.bottom() - 4, iw as i32, 3);
                ui.canvas.fill_rounded_rect(ir, 1.5, color);
            }
            if resp.hovered {
                ui.set_cursor(vui::Cursor::Hand);
                tip = Some((item.tip(), r.center().0));
            }
            if resp.clicked {
                m.push(Self::click_action(item));
            }
            // Middle click opens another instance.
            if resp.hovered && ui.input.pressed[2] && !item.app.is_empty() {
                m.push(Action::Launch(item.app.clone(), Vec::new()));
            }
            x += SLOT;
        }

        // Clock (opens the calendar).
        let show_desktop = Rect::new(w - 10, 0, 10, h);
        let clock = Rect::new(show_desktop.x - 6 - 96, 4, 96, h - 8);
        let resp = ui.interact(ui.id("clock"), clock);
        Self::button_bg(ui, clock, resp.hovered, resp.held, m.calendar_open);
        let now = vrt::time::DateTime::now();
        let time = format!("{:02}:{:02}", now.hour, now.minute);
        let date = format!("{} {} {}", &now.weekday_name()[..3], now.day, &now.month_name()[..3]);
        ui.label(Rect::new(clock.x, clock.y + 3, clock.w, 18), &time, Font::Regular, 13.0, t.text, Align::Center);
        ui.label(Rect::new(clock.x, clock.y + 20, clock.w, 16), &date, Font::Regular, 12.0, t.text_dim, Align::Center);
        if resp.pressed {
            self.clock_pressed_at = ui.now();
        }
        if resp.hovered {
            let full = format!("{}, {} {} {}", now.weekday_name(), now.day, now.month_name(), now.year);
            tip = Some((full, clock.center().0));
        }
        if resp.clicked && m.calendar_dismissed_at + DISMISS_GRACE_NS < self.clock_pressed_at {
            m.push(Action::ToggleCalendar);
        }
        // Redraw when the minute changes.
        let into_minute = vrt::time::unix_time_ns() % 60_000_000_000;
        let now_ns = ui.now();
        ui.repaint_at(now_ns + 60_000_000_000 - into_minute);

        // Volume: click for the volume control, scroll to adjust.
        let vol = Rect::new(clock.x - 44, 4, 40, h - 8);
        let resp = ui.interact(ui.id("volume"), vol);
        Self::button_bg(ui, vol, resp.hovered, resp.held, m.volume_open);
        let (level, muted) = m.audio.as_ref().map_or((1.0, false), |a| (a.master_volume, a.muted));
        ui.icon(vol, volume::icon(level, muted), 18.0, t.text);
        if resp.pressed {
            self.volume_pressed_at = ui.now();
        }
        if resp.clicked && m.volume_dismissed_at + DISMISS_GRACE_NS < self.volume_pressed_at {
            m.push(Action::ToggleVolume);
        }
        if resp.hovered && ui.input.scroll.1 != 0 && m.audio.is_some() {
            m.push(Action::SetVolume(level + ui.input.scroll.1 as f32 * 0.05, false));
        }
        if resp.hovered {
            let label = match &m.audio {
                None => "No sound device".into(),
                Some(_) if muted => "Volume: muted".into(),
                Some(_) => format!("Volume: {}%", (level * 100.0 + 0.5) as u32),
            };
            tip = Some((label, vol.center().0));
        }

        // Network: opens the Wi-Fi flyout.
        let net = Rect::new(vol.x - 44, 4, 40, h - 8);
        let resp = ui.interact(ui.id("network"), net);
        Self::button_bg(ui, net, resp.hovered, resp.held, m.wifi_open);
        let (icon, dim) = wifi::icon(m);
        ui.icon(net, icon, 18.0, if dim { t.text_faint } else { t.text });
        if resp.pressed {
            self.network_pressed_at = ui.now();
        }
        if resp.clicked && m.wifi_dismissed_at + DISMISS_GRACE_NS < self.network_pressed_at {
            m.push(Action::ToggleWifi);
        }
        if resp.hovered {
            tip = Some((wifi::tooltip(m).replace('\n', " \u{00b7} "), net.center().0));
        }

        // "Show desktop" sliver at the far right.
        let resp = ui.interact(ui.id("show-desktop"), show_desktop);
        if resp.hovered {
            ui.canvas.fill_rect(Rect::new(show_desktop.x, 10, 1, h - 20), Color::rgba(255, 255, 255, 90));
            tip = Some(("Show desktop".into(), show_desktop.center().0));
        }
        if resp.clicked {
            m.push(Action::ShowDesktop);
        }

        // A press hides the tooltip; it comes back after resting again.
        if ui.input.pressed.iter().any(|&p| p) {
            tip = None;
        }
        let now_ns = ui.now();
        m.tip = tip.map(|(label, anchor_x)| {
            let since = match &m.tip {
                Some(old) if old.label == label => old.since,
                _ => now_ns,
            };
            let text_width = ui.measure(&label, Font::Regular, 13.0) as i32;
            Tip { label, anchor_x, text_width, since }
        });
    }

    /// What clicking an app button does: launch it, focus or minimise its
    /// window, or cycle through its windows.
    fn click_action(item: &Item) -> Action {
        let ws = &item.windows;
        if ws.is_empty() {
            return Action::Launch(item.app.clone(), Vec::new());
        }
        match ws.iter().position(|w| w.focused && !w.minimized) {
            Some(i) if ws.len() == 1 => Action::Minimize(ws[i].id),
            Some(i) => Action::Activate(ws[(i + 1) % ws.len()].id),
            // Topmost window of the app (the list is in stacking order).
            None => Action::Activate(ws[ws.len() - 1].id),
        }
    }
}
