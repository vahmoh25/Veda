//! The "Network & Internet" page: the Wi-Fi switch and connection, the
//! networks in range, saved networks, every interface with its addresses,
//! and diagnostics.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vnet::wifi::{self, ConnState, NetworkInfo, SavedNetwork, Security, WlanDiagnostics, WlanStatus, signal_bars};
use vnet::{Connectivity, InterfaceInfo, InterfaceKind, NetStatus};
use vui::{Align, ButtonKind, Color, Font, Icon, Rect, Ui};

/// How often the page re-reads everything while shown.
const REFRESH_NS: u64 = 1_000_000_000;

fn bars_icon(bars: u8) -> Icon {
    match bars {
        0 => Icon::WifiNone,
        1 => Icon::WifiWeak,
        2 => Icon::WifiFair,
        _ => Icon::Wifi,
    }
}

fn joining(s: &WlanStatus) -> bool {
    matches!(s.state, ConnState::Authenticating | ConnState::Associating | ConnState::Securing)
}

fn duration(s: u32) -> String {
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, (s / 60) % 60),
    }
}

fn mac(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

fn bytes(n: u64) -> String {
    match n {
        0..=9_999 => format!("{n} B"),
        10_000..=9_999_999 => format!("{} KB", n / 1000),
        _ => format!("{} MB", n / 1_000_000),
    }
}

fn passphrase_ok(p: &str) -> bool {
    let printable = p.bytes().all(|b| (0x20..=0x7E).contains(&b));
    (printable && (8..=63).contains(&p.len())) || (p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn capitalize(s: String) -> String {
    let mut s = s;
    if let Some(c) = s.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    s
}

pub struct NetworkPage {
    wifi: Option<WlanStatus>,
    networks: Vec<NetworkInfo>,
    saved: Vec<SavedNetwork>,
    ifaces: Vec<InterfaceInfo>,
    status: Option<NetStatus>,
    diag: Option<WlanDiagnostics>,
    show_diag: bool,
    watch: Option<wifi::Watcher>,
    refresh_at: u64,
    /// The network row showing its password field.
    selected: Option<Vec<u8>>,
    password: String,
    reveal: bool,
    /// The last error: the SSID it concerns and the message.
    error: Option<(Vec<u8>, String)>,
    /// Something changed during drawing: draw again.
    dirty: bool,
}

impl NetworkPage {
    pub fn new() -> NetworkPage {
        NetworkPage {
            wifi: None,
            networks: Vec::new(),
            saved: Vec::new(),
            ifaces: Vec::new(),
            status: None,
            diag: None,
            show_diag: false,
            watch: None,
            refresh_at: 0,
            selected: None,
            password: String::new(),
            reveal: false,
            error: None,
            dirty: false,
        }
    }

    /// The handle to wait on for Wi-Fi events.
    pub fn wait_handle(&self) -> Option<vabi::RawHandle> {
        self.watch.as_ref().map(|w| w.handle().raw())
    }

    fn refresh(&mut self) {
        self.dirty = true;
        self.wifi = wifi::status().ok();
        if self.wifi.is_some() {
            self.networks = wifi::networks().unwrap_or_default();
            self.saved = wifi::saved().unwrap_or_default();
            if self.show_diag {
                self.diag = wifi::diagnostics().ok();
            }
            if self.watch.is_none() {
                self.watch = wifi::Watcher::new().ok();
            }
        } else {
            self.networks.clear();
            self.saved.clear();
            self.watch = None;
        }
        self.ifaces = vnet::interfaces().unwrap_or_default();
        self.ifaces.retain(|i| i.kind != InterfaceKind::Loopback);
        self.status = vnet::status().ok();
    }

    /// Takes in events and refreshes when due. Returns when to look again.
    pub fn poll(&mut self, now: u64) -> u64 {
        let mut events = false;
        if let Some(w) = &self.watch {
            loop {
                match w.try_next() {
                    Ok(Some(wifi::WlanEvent::ConnectFailed { ssid, reason, .. })) => {
                        self.error = Some((ssid.0, capitalize(format!("{reason}"))));
                        events = true;
                    }
                    Ok(Some(_)) => events = true,
                    Ok(None) => break,
                    Err(_) => {
                        self.watch = None;
                        events = true;
                        break;
                    }
                }
            }
        }
        if events || now >= self.refresh_at {
            self.refresh();
            self.refresh_at = now + REFRESH_NS;
        }
        self.refresh_at
    }

    fn connect(&mut self, ssid: Vec<u8>, password: Option<String>) {
        self.error = None;
        let opts = wifi::ConnectOptions::default();
        if let Err(e) = wifi::connect_with(&ssid, password.as_deref(), &opts) {
            self.error = Some((ssid, capitalize(format!("{e}"))));
        }
        self.selected = None;
        self.refresh();
    }

    fn card_title(ui: &mut Ui, x: i32, y: i32, w: i32, title: &str) {
        let t = ui.theme().clone();
        ui.label(Rect::new(x, y, w, 24), title, Font::Bold, t.heading_size - 2.0, t.text, Align::Left);
    }

    fn row(ui: &mut Ui, x: i32, y: i32, w: i32, label: &str, value: &str) {
        let t = ui.theme().clone();
        ui.label(Rect::new(x, y, 160, 26), label, Font::Regular, t.font_size, t.text_dim, Align::Left);
        ui.label(Rect::new(x + 170, y, w - 170, 26), value, Font::Regular, t.font_size, t.text, Align::Left);
    }

    /// Draws the page; returns the height used.
    pub fn draw(&mut self, ui: &mut Ui, r: Rect) -> i32 {
        let t = ui.theme().clone();
        let summary = match &self.status {
            None => String::from("The network service is not running."),
            Some(s) => match s.connectivity {
                Connectivity::Routable => format!("Connected to the Internet through {}.", s.default_interface),
                Connectivity::Local => String::from("Connected to a local network without a way to the Internet."),
                _ => String::from("Not connected."),
            },
        };
        ui.label(Rect::new(r.x, r.y, r.w, 36), "Network & Internet", Font::Bold, t.title_size, t.text, Align::Left);
        ui.label(Rect::new(r.x, r.y + 38, r.w, 22), &summary, Font::Regular, t.font_size, t.text_dim, Align::Left);
        let mut y = r.y + 78;
        y = self.wifi_card(ui, r.x, y, r.w);
        if self.wifi.as_ref().is_some_and(|s| !matches!(s.state, ConnState::NoAdapter | ConnState::RadioOff)) {
            y = self.networks_card(ui, r.x, y, r.w);
        }
        if !self.saved.is_empty() {
            y = self.saved_card(ui, r.x, y, r.w);
        }
        y = self.interfaces_card(ui, r.x, y, r.w);
        if self.wifi.is_some() {
            y = self.diagnostics_card(ui, r.x, y, r.w);
        }
        if core::mem::take(&mut self.dirty) {
            ui.repaint();
        }
        y - r.y
    }

    fn wifi_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let connected = self.wifi.as_ref().is_some_and(|s| s.state == ConnState::Connected);
        let h = if connected { 74 + 6 * 28 + 52 } else { 74 };
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        let ix = card.x + 22;
        let iw = card.w - 44;
        let (icon, status) = match &self.wifi {
            None => (Icon::WifiOff, String::from("The Wi-Fi service is not running")),
            Some(s) => match s.state {
                ConnState::NoAdapter => (Icon::WifiOff, String::from("No Wi-Fi adapter")),
                ConnState::RadioOff => (Icon::WifiOff, String::from("Off")),
                ConnState::Connected => (bars_icon(signal_bars(s.signal_dbm)), format!("Connected to {}", s.name)),
                _ if joining(s) => (Icon::WifiNone, format!("Connecting to {}\u{2026}", s.name)),
                _ => (
                    Icon::WifiNone,
                    match s.last_failure {
                        Some(f) => format!("Not connected \u{2014} {f}"),
                        None => String::from("Not connected"),
                    },
                ),
            },
        };
        ui.icon(Rect::new(ix, card.y + 14, 28, 44), icon, 22.0, t.accent);
        ui.label(Rect::new(ix + 40, card.y + 14, iw - 120, 22), "Wi-Fi", Font::Bold, t.font_size, t.text, Align::Left);
        ui.label(
            Rect::new(ix + 40, card.y + 36, iw - 120, 20),
            &status,
            Font::Regular,
            t.small_size,
            t.text_dim,
            Align::Left,
        );
        if let Some(s) = &self.wifi
            && s.state != ConnState::NoAdapter
        {
            let mut on = s.state != ConnState::RadioOff;
            if ui.toggle(Rect::new(card.right() - 22 - 44, card.y + 25, 44, 22), "settings-wifi-radio", &mut on) {
                let _ = wifi::set_radio(on);
                self.refresh();
            }
        }
        if let (true, Some(s)) = (connected, self.wifi.clone()) {
            let mut ry = card.y + 74;
            let rows = [
                ("Network", s.name.clone()),
                ("Security", String::from(s.security.label())),
                ("Signal", format!("{} dBm ({} of 4 bars)", s.signal_dbm, signal_bars(s.signal_dbm))),
                (
                    "Channel",
                    format!("{} ({})", s.channel, if s.band == wifi::Band::Ghz5 { "5 GHz" } else { "2.4 GHz" }),
                ),
                ("Access point", mac(&s.bssid)),
                ("Connected for", duration(s.connected_s)),
            ];
            for (label, value) in rows {
                Self::row(ui, ix, ry, iw, label, &value);
                ry += 28;
            }
            if ui.button_full(Rect::new(ix, ry + 8, 140, 34), None, "Disconnect", ButtonKind::Secondary) {
                let _ = wifi::disconnect();
                self.refresh();
            }
        }
        card.bottom() + 20
    }

    fn networks_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let row_h = 52;
        let expanded_extra = 96;
        let nets = self.networks.clone();
        let rows_h: i32 = nets
            .iter()
            .map(|n| if self.selected.as_deref() == Some(&n.ssid.0) { row_h + expanded_extra } else { row_h })
            .sum();
        let card = Rect::new(x, y, w, 64 + rows_h.max(40) + 12);
        ui.card(card);
        let ix = card.x + 22;
        let iw = card.w - 44;
        Self::card_title(ui, ix, card.y + 16, iw - 140, "Networks in range");
        let scanning = self.wifi.as_ref().is_some_and(|s| s.scanning);
        let br = Rect::new(card.right() - 22 - 120, card.y + 12, 120, 32);
        if scanning {
            ui.spinner(br.right() - 16, br.center().1, 8.0);
        } else if ui.button_full(br, Some(Icon::Refresh), "Refresh", ButtonKind::Secondary) {
            let _ = wifi::scan();
            self.refresh();
        }
        let mut ry = card.y + 56;
        if nets.is_empty() {
            let msg = if scanning { "Looking for networks\u{2026}" } else { "No networks found." };
            ui.label(Rect::new(ix, ry, iw, 32), msg, Font::Regular, t.font_size, t.text_faint, Align::Left);
        }
        for n in &nets {
            let open = self.selected.as_deref() == Some(&n.ssid.0);
            let head = Rect::new(ix - 8, ry, iw + 16, row_h);
            let resp = ui.interact(ui.id(&format!("settings-net-{}", n.name)), head);
            if open {
                ui.canvas.fill_rounded_rect(
                    Rect::new(head.x, head.y, head.w, row_h + expanded_extra),
                    t.radius,
                    Color::rgba(255, 255, 255, 12),
                );
            } else if resp.hovered {
                ui.canvas.fill_rounded_rect(head, t.radius, Color::rgba(255, 255, 255, 8));
            }
            let color = if n.security.supported() { t.text } else { t.text_faint };
            ui.icon(Rect::new(ix, ry + 10, 28, 32), bars_icon(n.bars), 20.0, color);
            ui.label(Rect::new(ix + 40, ry + 6, iw - 260, 22), &n.name, Font::Regular, t.font_size, color, Align::Left);
            let sub = format!(
                "{}{}{}",
                n.security.label(),
                if n.saved { " \u{00b7} saved" } else { "" },
                if n.connected { " \u{00b7} connected" } else { "" }
            );
            ui.label(
                Rect::new(ix + 40, ry + 26, iw - 260, 20),
                &sub,
                Font::Regular,
                t.small_size,
                t.text_dim,
                Align::Left,
            );
            let detail = format!(
                "{} dBm \u{00b7} {} AP{}",
                n.signal_dbm,
                n.access_points,
                if n.access_points == 1 { "" } else { "s" }
            );
            ui.label(
                Rect::new(ix + iw - 250, ry, 120, row_h),
                &detail,
                Font::Regular,
                t.small_size,
                t.text_faint,
                Align::Right,
            );
            let action_r = Rect::new(ix + iw - 120, ry + 9, 120, 34);
            if n.connected {
                if ui.button_full(action_r, None, "Disconnect", ButtonKind::Secondary) {
                    let _ = wifi::disconnect();
                    self.refresh();
                }
            } else if n.security.supported() {
                let needs_password = n.security != Security::Open && !n.saved;
                if ui.button_full(action_r, None, "Connect", ButtonKind::Primary) {
                    if needs_password {
                        self.selected = if open { None } else { Some(n.ssid.0.clone()) };
                        ui.repaint();
                        self.password.clear();
                    } else {
                        self.connect(n.ssid.0.clone(), None);
                    }
                }
            }
            if resp.clicked && n.security.supported() && !n.connected && n.security != Security::Open && !n.saved {
                self.selected = if open { None } else { Some(n.ssid.0.clone()) };
                ui.repaint();
                self.password.clear();
            }
            ry += row_h;
            if open {
                let field = Rect::new(ix + 40, ry + 4, (iw - 40 - 200).min(320), 34);
                let resp = ui.password_input(
                    field,
                    &format!("settings-pw-{}", n.name),
                    &mut self.password,
                    "Password",
                    self.reveal,
                );
                let label = if self.reveal { "Hide" } else { "Show" };
                if ui.button(Rect::new(field.right() + 8, ry + 4, 60, 34), label) {
                    self.reveal = !self.reveal;
                    ui.repaint();
                }
                let valid = passphrase_ok(&self.password);
                let go =
                    ui.button_full(Rect::new(field.right() + 76, ry + 4, 110, 34), None, "Join", ButtonKind::Primary);
                if (go || resp.submitted) && valid {
                    let p = core::mem::take(&mut self.password);
                    self.connect(n.ssid.0.clone(), Some(p));
                }
                let msg = match &self.error {
                    Some((s, e)) if *s == n.ssid.0 => e.clone(),
                    _ if !self.password.is_empty() && !valid => String::from("Passwords are 8 to 63 characters."),
                    _ => String::new(),
                };
                ui.label(
                    Rect::new(ix + 40, ry + 46, iw - 40, 22),
                    &msg,
                    Font::Regular,
                    t.small_size,
                    t.danger,
                    Align::Left,
                );
                ry += expanded_extra;
            }
        }
        card.bottom() + 20
    }

    fn saved_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let row_h = 48;
        let saved = self.saved.clone();
        let card = Rect::new(x, y, w, 60 + saved.len() as i32 * row_h + 10);
        ui.card(card);
        let ix = card.x + 22;
        let iw = card.w - 44;
        Self::card_title(ui, ix, card.y + 16, iw, "Saved networks");
        let mut ry = card.y + 52;
        for s in &saved {
            ui.label(Rect::new(ix, ry + 4, iw - 330, 22), &s.name, Font::Regular, t.font_size, t.text, Align::Left);
            let sub = format!("{}{}", s.security.label(), if s.hidden { " \u{00b7} hidden" } else { "" });
            ui.label(Rect::new(ix, ry + 24, iw - 330, 20), &sub, Font::Regular, t.small_size, t.text_dim, Align::Left);
            ui.label(
                Rect::new(ix + iw - 320, ry, 150, row_h),
                "Connect automatically",
                Font::Regular,
                t.small_size,
                t.text_dim,
                Align::Right,
            );
            let mut auto = s.auto_connect;
            if ui.toggle(Rect::new(ix + iw - 160, ry + 13, 44, 22), &format!("settings-auto-{}", s.name), &mut auto) {
                let _ = wifi::set_auto_connect(&s.ssid.0, auto);
                self.refresh();
            }
            if ui.button_full(
                Rect::new(ix + iw - 100, ry + 7, 100, 34),
                Some(Icon::Trash),
                "Forget",
                ButtonKind::Secondary,
            ) {
                let _ = wifi::forget(&s.ssid.0);
                self.refresh();
            }
            ry += row_h;
        }
        card.bottom() + 20
    }

    fn interfaces_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let mut y = y;
        for i in self.ifaces.clone() {
            let lines = 6 + i.addresses.len() as i32;
            let card = Rect::new(x, y, w, 60 + lines * 26 + 8);
            ui.card(card);
            let ix = card.x + 22;
            let iw = card.w - 44;
            let (icon, kind) = match i.kind {
                InterfaceKind::Wireless => (Icon::Wifi, "Wi-Fi"),
                _ => (Icon::Network, "Ethernet"),
            };
            ui.icon(Rect::new(ix, card.y + 12, 24, 32), icon, 18.0, t.accent);
            let title = format!("{} \u{00b7} {}", i.name, kind);
            ui.label(
                Rect::new(ix + 34, card.y + 14, iw - 34, 26),
                &title,
                Font::Bold,
                t.font_size,
                t.text,
                Align::Left,
            );
            let mut ry = card.y + 52;
            Self::row(ui, ix, ry, iw, "Link", if i.link_up { "Up" } else { "Down" });
            ry += 26;
            Self::row(ui, ix, ry, iw, "Hardware address", &mac(&i.mac));
            ry += 26;
            for a in &i.addresses {
                let label = if a.address.is_ipv4() { "IPv4 address" } else { "IPv6 address" };
                Self::row(ui, ix, ry, iw, label, &format!("{}/{}", a.address, a.prefix_len));
                ry += 26;
            }
            let list = |v: Vec<String>| if v.is_empty() { String::from("\u{2014}") } else { v.join(", ") };
            let gw = list(i.gateways.iter().map(|g| format!("{g}")).collect());
            Self::row(ui, ix, ry, iw, "Gateway", &gw);
            ry += 26;
            let dns = list(i.dns.iter().map(|d| format!("{d}")).collect());
            Self::row(ui, ix, ry, iw, "DNS servers", &dns);
            ry += 26;
            let dhcp = format!(
                "{:?}{}",
                i.dhcp.state,
                if i.dhcp.lease_s > 0 { format!(" (lease {} s)", i.dhcp.lease_s) } else { String::new() }
            );
            Self::row(ui, ix, ry, iw, "DHCP", &dhcp);
            ry += 26;
            let st = i.stats;
            let traffic = format!(
                "received {} ({} packets), sent {} ({} packets)",
                bytes(st.rx_bytes),
                st.rx_packets,
                bytes(st.tx_bytes),
                st.tx_packets
            );
            Self::row(ui, ix, ry, iw, "Traffic", &traffic);
            y = card.bottom() + 16;
        }
        y + 4
    }

    fn diagnostics_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let lines: Vec<String> = match (&self.diag, self.show_diag) {
            (Some(d), true) => {
                let c = &d.counters;
                let mut v = alloc::vec![
                    format!(
                        "Scans {} \u{00b7} attempts {} \u{00b7} connections {} \u{00b7} disconnections {} \u{00b7} authentication failures {}",
                        c.scans, c.connect_attempts, c.connections, c.disconnections, c.auth_failures
                    ),
                    format!(
                        "Frames sent {} \u{00b7} received {} \u{00b7} decryption errors {} \u{00b7} replays dropped {} \u{00b7} group rekeys {} \u{00b7} roams {}",
                        c.data_tx, c.data_rx, c.decrypt_errors, c.replays, c.group_rekeys, c.roams
                    ),
                ];
                for e in d.log.iter().rev().take(14) {
                    v.push(format!("[{:>6}.{:01}] {}", e.time_ms / 1000, (e.time_ms % 1000) / 100, e.message));
                }
                v
            }
            _ => Vec::new(),
        };
        let card = Rect::new(x, y, w, 64 + lines.len() as i32 * 22 + 8);
        ui.card(card);
        let ix = card.x + 22;
        let iw = card.w - 44;
        Self::card_title(ui, ix, card.y + 16, iw - 80, "Wi-Fi diagnostics");
        let mut on = self.show_diag;
        if ui.toggle(Rect::new(card.right() - 22 - 44, card.y + 17, 44, 22), "settings-wifi-diag", &mut on) {
            self.show_diag = on;
            self.refresh();
        }
        let mut ry = card.y + 52;
        for l in &lines {
            ui.label(Rect::new(ix, ry, iw, 22), l, Font::Regular, t.small_size, t.text_dim, Align::Left);
            ry += 22;
        }
        card.bottom() + 20
    }
}
