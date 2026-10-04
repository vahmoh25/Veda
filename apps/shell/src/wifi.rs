//! The network flyout opened from the network icon on the taskbar: Wi-Fi on
//! and off, the networks in range, joining one (with its password), leaving
//! it, and a link to the network settings.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Rect};
use vnet::wifi::{ConnState, NetworkInfo, Security, WlanStatus, signal_bars};
use vui::{Font, Icon, Ui};

use crate::{Action, Model, chrome, taskbar};

const WIDTH: i32 = 360;
const HEIGHT: i32 = 520;
const PAD: i32 = 16;
/// Height of a collapsed network row.
const ROW: i32 = 54;

/// Where the flyout appears: above the tray, at the right edge.
pub fn placement(screen: Rect) -> Rect {
    Rect::new(screen.w - WIDTH - 12, screen.h - taskbar::HEIGHT - 12 - HEIGHT, WIDTH, HEIGHT)
}

/// The Wi-Fi icon for a signal level in bars (0-4).
pub fn bars_icon(bars: u8) -> Icon {
    match bars {
        0 => Icon::WifiNone,
        1 => Icon::WifiWeak,
        2 => Icon::WifiFair,
        _ => Icon::Wifi,
    }
}

fn has_adapter(s: &Option<WlanStatus>) -> bool {
    s.as_ref().is_some_and(|s| s.state != ConnState::NoAdapter)
}

fn joining(s: &WlanStatus) -> bool {
    matches!(s.state, ConnState::Authenticating | ConnState::Associating | ConnState::Securing)
}

/// Draws a Wi-Fi signal as bright bars over the faint full fan (so a weak
/// signal still reads as a Wi-Fi icon).
pub fn draw_signal(ui: &mut Ui, r: Rect, bars: u8, size: f32, color: vgfx::Color) {
    let faint = ui.theme().text_faint;
    if bars < 3 {
        ui.icon(r, Icon::Wifi, size, faint);
    }
    ui.icon(r, bars_icon(bars), size, color);
}

/// Draws the taskbar's network icon: the Wi-Fi signal when connected, a
/// faint Wi-Fi fan when not, the crossed fan when Wi-Fi is off, and the
/// wired icon on machines without Wi-Fi.
pub fn draw_icon(ui: &mut Ui, r: Rect, m: &Model) {
    let t = ui.theme().clone();
    match &m.wifi {
        Some(s) if has_adapter(&m.wifi) => match s.state {
            ConnState::RadioOff => ui.icon(r, Icon::WifiOff, 18.0, t.text),
            ConnState::Connected => draw_signal(ui, r, signal_bars(s.signal_dbm), 18.0, t.text),
            // Not connected: with a wired connection the wired icon says
            // more, otherwise a faint Wi-Fi fan invites a click.
            _ if m.wired_up => ui.icon(r, Icon::Network, 18.0, t.text),
            _ => ui.icon(r, Icon::Wifi, 18.0, t.text_faint),
        },
        _ => ui.icon(r, Icon::Network, 18.0, if m.wired_up { t.text } else { t.text_faint }),
    }
}

/// The taskbar tooltip.
pub fn tooltip(m: &Model) -> String {
    match &m.wifi {
        Some(s) if has_adapter(&m.wifi) => match s.state {
            ConnState::Connected => format!("{}\nInternet access", s.name),
            ConnState::RadioOff => "Wi-Fi is off".into(),
            _ if joining(s) => format!("Connecting to {}", s.name),
            _ if m.wired_up => "Wired network connected".into(),
            _ => "Not connected".into(),
        },
        _ if m.wired_up => "Wired network connected".into(),
        _ => "No network".into(),
    }
}

/// How a network is described under its name.
fn subtitle(m: &Model, n: &NetworkInfo) -> String {
    let st = m.wifi.as_ref();
    if n.connected {
        return if n.security == Security::Open { "Connected".into() } else { "Connected, secured".into() };
    }
    if st.is_some_and(|s| joining(s) && s.ssid.0 == n.ssid.0) {
        return "Connecting\u{2026}".into();
    }
    if !n.security.supported() {
        return format!("Not supported ({})", n.security.label().trim_end_matches(" (unsupported)"));
    }
    let base = if n.security == Security::Open { "Open" } else { "Secured" };
    if n.saved { format!("{base}, saved") } else { base.into() }
}

pub struct WifiFlyout {
    origin: (i32, i32),
    /// The SSID of the expanded row.
    selected: Option<Vec<u8>>,
    password: String,
    reveal: bool,
    auto_connect: bool,
    /// The "hidden network" row is expanded.
    hidden_open: bool,
    hidden_ssid: String,
}

impl WifiFlyout {
    pub fn new(origin: (i32, i32)) -> WifiFlyout {
        WifiFlyout {
            origin,
            selected: None,
            password: String::new(),
            reveal: false,
            auto_connect: true,
            hidden_open: false,
            hidden_ssid: String::new(),
        }
    }

    fn select(&mut self, ssid: Option<Vec<u8>>) {
        self.selected = ssid;
        self.password.clear();
        self.reveal = false;
        self.auto_connect = true;
        self.hidden_open = false;
    }

    /// Whether the expanded network needs a password typed in.
    fn needs_password(n: &NetworkInfo) -> bool {
        n.security.needs_password() && !n.saved && !n.connected
    }

    fn row_height(&self, n: &NetworkInfo) -> i32 {
        if self.selected.as_deref() != Some(&n.ssid.0) {
            return ROW;
        }
        if Self::needs_password(n) && n.security.supported() { ROW + 136 } else { ROW + 52 }
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let t = ui.theme().clone();
        let w = ui.width;
        chrome::popup_background(&mut ui.canvas, &m.wallpaper.blurred, self.origin);
        if ui.input.key(vproto::input::keys::ESC) {
            ui.close_window();
        }

        // Header: title and the Wi-Fi switch.
        ui.label(Rect::new(PAD, 14, 200, 26), "Wi-Fi", Font::Bold, 16.0, t.text, Align::Left);
        let adapter = has_adapter(&m.wifi);
        if adapter {
            let mut on = m.wifi.as_ref().is_some_and(|s| s.state != ConnState::RadioOff);
            if ui.toggle(Rect::new(w - PAD - 44, 16, 44, 22), "wifi-radio", &mut on) {
                m.push(Action::WifiRadio(on));
            }
        }
        let status = match &m.wifi {
            None => "The Wi-Fi service is not running".into(),
            Some(_) if !adapter => "No Wi-Fi adapter".into(),
            Some(s) if s.state == ConnState::RadioOff => "Wi-Fi is turned off".into(),
            Some(s) if s.state == ConnState::Connected => format!("Connected to {}", s.name),
            Some(s) if joining(s) => format!("Connecting to {}\u{2026}", s.name),
            Some(s) => match s.last_failure {
                Some(f) => format!("Not connected: {f}"),
                None => "Not connected".into(),
            },
        };
        ui.label(Rect::new(PAD, 40, w - 2 * PAD, 20), &status, Font::Regular, 12.0, t.text_dim, Align::Left);
        if m.wired_up {
            ui.label(
                Rect::new(PAD, 58, w - 2 * PAD, 18),
                &m.wired_label,
                Font::Regular,
                12.0,
                t.text_faint,
                Align::Left,
            );
        }

        let list = Rect::new(8, 82, w - 16, HEIGHT - 82 - 56);
        let usable = adapter && m.wifi.as_ref().is_some_and(|s| s.state != ConnState::RadioOff);
        if usable {
            self.networks(ui, m, list);
        } else {
            let msg = if adapter { "Turn Wi-Fi on to see networks in range." } else { "" };
            ui.label(list, msg, Font::Regular, 13.0, t.text_faint, Align::Center);
        }

        // Footer.
        ui.separator(0, HEIGHT - 52, w);
        let scanning = m.wifi.as_ref().is_some_and(|s| s.scanning);
        if usable {
            let r = Rect::new(PAD - 6, HEIGHT - 46, 40, 40);
            if scanning {
                ui.spinner(r.center().0, r.center().1, 8.0);
            } else if ui.icon_button(r, Icon::Refresh, "Look for networks") {
                m.push(Action::WifiScan);
            }
        }
        if ui.button(Rect::new(w - PAD - 170, HEIGHT - 42, 170, 32), "Network settings") {
            m.push(Action::OpenNetworkSettings);
        }
        chrome::popup_frame(&mut ui.canvas);
    }

    fn networks(&mut self, ui: &mut Ui, m: &mut Model, list: Rect) {
        let t = ui.theme().clone();
        let nets = m.wifi_networks.clone();
        if let Some(sel) = &self.selected
            && !nets.iter().any(|n| &n.ssid.0 == sel)
        {
            self.selected = None;
        }
        let hidden_h = if self.hidden_open { ROW + 136 } else { ROW };
        let content_h: i32 = nets.iter().map(|n| self.row_height(n)).sum::<i32>() + hidden_h;
        if nets.is_empty() {
            let scanning = m.wifi.as_ref().is_some_and(|s| s.scanning);
            let msg = if scanning { "Looking for networks\u{2026}" } else { "No networks found" };
            ui.label(Rect::new(list.x, list.y, list.w, 40), msg, Font::Regular, 13.0, t.text_faint, Align::Center);
        }
        ui.scroll_area(list, "wifi-list", content_h, |ui, offset| {
            let mut y = list.y - offset + if nets.is_empty() { 40 } else { 0 };
            for n in &nets {
                let h = self.row_height(n);
                self.network_row(ui, m, Rect::new(list.x, y, list.w - 10, h), n);
                y += h;
            }
            self.hidden_row(ui, m, Rect::new(list.x, y, list.w - 10, hidden_h));
        });
    }

    fn network_row(&mut self, ui: &mut Ui, m: &mut Model, r: Rect, n: &NetworkInfo) {
        let t = ui.theme().clone();
        let expanded = self.selected.as_deref() == Some(&n.ssid.0);
        let head = Rect::new(r.x, r.y, r.w, ROW);
        let id = ui.id(&format!("wifi-row-{}", n.name));
        let resp = ui.interact(id, head);
        if expanded {
            ui.canvas.fill_rounded_rect(r.inset(2, 1, 2, 1), t.radius, vgfx::Color::rgba(255, 255, 255, 16));
        } else if resp.hovered {
            ui.canvas.fill_rounded_rect(head.inset(2, 1, 2, 1), t.radius, vgfx::Color::rgba(255, 255, 255, 10));
        }
        if resp.clicked {
            self.select(if expanded { None } else { Some(n.ssid.0.clone()) });
            ui.repaint();
        }
        let color = if n.security.supported() { t.text } else { t.text_faint };
        draw_signal(ui, Rect::new(head.x + 8, head.y + 13, 28, 28), n.bars, 22.0, color);
        if n.security.needs_password() {
            ui.icon(Rect::new(head.x + 26, head.y + 30, 14, 14), Icon::Lock, 11.0, color);
        }
        ui.label(Rect::new(head.x + 48, head.y + 8, head.w - 56, 20), &n.name, Font::Regular, 14.0, color, Align::Left);
        let sub = subtitle(m, n);
        ui.label(
            Rect::new(head.x + 48, head.y + 28, head.w - 56, 18),
            &sub,
            Font::Regular,
            12.0,
            t.text_dim,
            Align::Left,
        );
        if !expanded {
            return;
        }

        // The expanded part.
        let mut y = head.bottom() + 4;
        let x = head.x + 48;
        let bw = 110;
        let right = r.right() - 12;
        let error = m.wifi_error.as_ref().filter(|(s, _)| *s == n.ssid.0).map(|(_, e)| e.clone());
        let busy = m.wifi.as_ref().is_some_and(|s| joining(s) && s.ssid.0 == n.ssid.0);
        if n.connected || busy {
            let label = if busy { "Cancel" } else { "Disconnect" };
            if ui.button(Rect::new(right - bw, y + 4, bw, 32), label) {
                m.push(Action::WifiDisconnect);
            }
            return;
        }
        if !n.security.supported() {
            let msg = "Veda cannot join networks with this security.";
            ui.label(Rect::new(x, y, right - x, 40), msg, Font::Regular, 12.0, t.text_faint, Align::Left);
            return;
        }
        if Self::needs_password(n) {
            let field = Rect::new(x, y, right - x - 56, 34);
            let pw_id = format!("wifi-password-{}", n.name);
            let resp = ui.password_input(field, &pw_id, &mut self.password, "Password", self.reveal);
            if resp.changed {
                m.wifi_error = None;
            }
            let label = if self.reveal { "Hide" } else { "Show" };
            if ui.button(Rect::new(field.right() + 6, y, 50, 34), label) {
                self.reveal = !self.reveal;
                ui.repaint();
            }
            y += 42;
            ui.checkbox(Rect::new(x, y, right - x, 24), "Connect automatically", &mut self.auto_connect);
            y += 30;
            let msg = error.unwrap_or_default();
            ui.label(Rect::new(x, y + 6, right - x - bw - 8, 20), &msg, Font::Regular, 12.0, t.danger, Align::Left);
            let valid = vwlan_passphrase_ok(&self.password);
            let submit =
                (resp.submitted && valid) || (ui.primary_button(Rect::new(right - bw, y, bw, 32), "Connect") && valid);
            if submit {
                m.push(Action::WifiConnect {
                    ssid: n.ssid.0.clone(),
                    password: Some(core::mem::take(&mut self.password)),
                    auto: self.auto_connect,
                    hidden: false,
                });
            }
        } else {
            if !n.saved {
                ui.checkbox(
                    Rect::new(x, y + 8, right - x - bw - 8, 24),
                    "Connect automatically",
                    &mut self.auto_connect,
                );
            } else if let Some(e) = error {
                ui.label(Rect::new(x, y + 8, right - x - bw - 8, 20), &e, Font::Regular, 12.0, t.danger, Align::Left);
            }
            if ui.primary_button(Rect::new(right - bw, y + 4, bw, 32), "Connect") {
                m.push(Action::WifiConnect {
                    ssid: n.ssid.0.clone(),
                    password: None,
                    auto: self.auto_connect || n.saved,
                    hidden: false,
                });
            }
        }
    }

    fn hidden_row(&mut self, ui: &mut Ui, m: &mut Model, r: Rect) {
        let t = ui.theme().clone();
        let head = Rect::new(r.x, r.y, r.w, ROW);
        let resp = ui.interact(ui.id("wifi-hidden-row"), head);
        if self.hidden_open {
            ui.canvas.fill_rounded_rect(r.inset(2, 1, 2, 1), t.radius, vgfx::Color::rgba(255, 255, 255, 16));
        } else if resp.hovered {
            ui.canvas.fill_rounded_rect(head.inset(2, 1, 2, 1), t.radius, vgfx::Color::rgba(255, 255, 255, 10));
        }
        if resp.clicked {
            let open = !self.hidden_open;
            self.select(None);
            self.hidden_open = open;
            ui.repaint();
        }
        ui.icon(Rect::new(head.x + 8, head.y + 13, 28, 28), Icon::Plus, 18.0, t.text_dim);
        ui.label(
            Rect::new(head.x + 48, head.y + 17, head.w - 56, 20),
            "Hidden network",
            Font::Regular,
            14.0,
            t.text,
            Align::Left,
        );
        if !self.hidden_open {
            return;
        }
        let x = head.x + 48;
        let right = r.right() - 12;
        let mut y = head.bottom();
        ui.text_input(Rect::new(x, y, right - x, 34), "wifi-hidden-ssid", &mut self.hidden_ssid, "Network name");
        y += 42;
        let resp = ui.password_input(
            Rect::new(x, y, right - x, 34),
            "wifi-hidden-password",
            &mut self.password,
            "Password (empty for an open network)",
            false,
        );
        y += 46;
        let name_ok = (1..=32).contains(&self.hidden_ssid.len());
        let pw_ok = self.password.is_empty() || vwlan_passphrase_ok(&self.password);
        if let Some((s, e)) = &m.wifi_error
            && *s == self.hidden_ssid.as_bytes()
        {
            ui.label(Rect::new(x, y + 6, right - x - 118, 20), e, Font::Regular, 12.0, t.danger, Align::Left);
        }
        let clicked = ui.primary_button(Rect::new(right - 110, y, 110, 32), "Connect");
        if (clicked || resp.submitted) && name_ok && pw_ok {
            let password = (!self.password.is_empty()).then(|| core::mem::take(&mut self.password));
            m.push(Action::WifiConnect {
                ssid: self.hidden_ssid.as_bytes().to_vec(),
                password,
                auto: true,
                hidden: true,
            });
        }
    }
}

/// The passphrase rules of WPA (8-63 printable characters, or 64 hex
/// digits), checked before sending a request.
fn vwlan_passphrase_ok(p: &str) -> bool {
    let printable = p.bytes().all(|b| (0x20..=0x7E).contains(&b));
    (printable && (8..=63).contains(&p.len())) || (p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}
