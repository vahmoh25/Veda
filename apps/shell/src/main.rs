//! The Vindows desktop shell.
//!
//! The shell owns the compositor's shell surfaces:
//!
//! * the **desktop** ([`desktop`]): wallpaper and desktop icons;
//! * the **taskbar** ([`taskbar`]): start button, pinned and running apps,
//!   clock;
//! * the **start menu** ([`start`]), **calendar** ([`calendar`]),
//!   **volume** ([`volume`]) and **network** ([`wifi`]) popups;
//! * **notifications** ([`notify`]);
//! * the voice **agent**'s tray item, window and approval requests
//!   ([`agent`]).
//!
//! Every surface is a [`vui::Host`] window. Surfaces draw from the shared
//! [`Model`] and request changes as [`Action`]s, which the main loop performs
//! after each round of drawing. The shell also serves the `shell` protocol so
//! applications can change the wallpaper and post notifications.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod apps;
mod calendar;
mod chrome;
mod desktop;
mod notify;
mod presence;
mod start;
mod taskbar;
mod tooltip;
mod volume;
mod wallpaper;
mod wifi;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vgfx::Rect;
use vproto::audio::{AudioStatus, audio};
use vproto::display::{WindowEvent, WindowInfo, WindowKind, WindowSpec, WindowState};
use vproto::init::{AppInfo, ClientIdentity, TaskEvent, launcher};
use vproto::shell::{ShellError, shell};
use vproto::vfs;
use vrt::object::{Channel, Vmo};
use vrt::println;
use vui::Host;
use vui::window::Display;

use wallpaper::Wallpaper;

vrt::entry!(main);

/// Where the chosen wallpaper is remembered.
const WALLPAPER_CONFIG: &str = "/home/user/.config/wallpaper";

/// How long after a popup was dismissed by an outside click a click on its
/// taskbar button still counts as "close" rather than "open again".
pub const DISMISS_GRACE_NS: u64 = 250_000_000;

/// A request from a shell surface, performed by the main loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Starts an installed application with arguments.
    Launch(String, Vec<String>),
    /// Raises, restores and focuses a window.
    Activate(u32),
    Minimize(u32),
    ToggleStart,
    CloseStart,
    ToggleCalendar,
    CloseCalendar,
    ToggleVolume,
    CloseVolume,
    ToggleWifi,
    CloseWifi,
    /// Turns the Wi-Fi radio on or off.
    WifiRadio(bool),
    WifiScan,
    /// Joins a Wi-Fi network.
    WifiConnect {
        ssid: Vec<u8>,
        password: Option<String>,
        auto: bool,
        hidden: bool,
    },
    WifiDisconnect,
    /// Opens the network page of Settings.
    OpenNetworkSettings,
    /// Opens or closes the agent's window.
    ToggleAgent,
    CloseAgent,
    /// Starts or ends a conversation with the agent.
    AgentWake,
    AgentSleep,
    /// The user's answer to an approval request.
    AgentDecide {
        id: u64,
        allow: bool,
        always: bool,
    },
    /// Turns the agent's microphone off (or on).
    AgentMute(bool),
    /// Sets the master volume (0..=1) and mute state.
    SetVolume(f32, bool),
    /// Minimises every window, or restores them if the desktop is showing.
    ShowDesktop,
    Power(u32),
    NextWallpaper,
    /// Re-reads the desktop folder.
    RefreshDesktop,
}

/// State shared by all shell surfaces.
pub struct Model {
    pub screen: Rect,
    pub apps: Vec<AppInfo>,
    /// Application windows (normal and borderless), bottom to top.
    pub windows: Vec<WindowInfo>,
    pub wallpaper: Wallpaper,
    /// Incremented whenever the wallpaper changes.
    pub wallpaper_generation: u64,
    /// The audio system's state (`None` without the audio service).
    pub audio: Option<AudioStatus>,
    /// The Wi-Fi state (`None` without the Wi-Fi service).
    pub wifi: Option<vnet::wifi::WlanStatus>,
    /// Wi-Fi networks in range.
    pub wifi_networks: Vec<vnet::wifi::NetworkInfo>,
    /// The last failed connection attempt: SSID and why.
    pub wifi_error: Option<(Vec<u8>, String)>,
    /// A wired interface is connected, and how to describe it.
    pub wired_up: bool,
    pub wired_label: String,
    pub start_open: bool,
    pub calendar_open: bool,
    pub volume_open: bool,
    pub wifi_open: bool,
    /// When a popup was last dismissed (see [`DISMISS_GRACE_NS`]).
    pub start_dismissed_at: u64,
    pub calendar_dismissed_at: u64,
    pub volume_dismissed_at: u64,
    pub wifi_dismissed_at: u64,
    /// The taskbar button under the pointer, for its tooltip.
    pub tip: Option<tooltip::Tip>,
    /// The voice agent's state.
    pub agent: agent::AgentModel,
    pub actions: Vec<Action>,
}

impl Model {
    pub fn app(&self, id: &str) -> Option<&AppInfo> {
        self.apps.iter().find(|a| a.id == id)
    }

    pub fn push(&mut self, a: Action) {
        self.actions.push(a);
    }
}

/// An open popup: its window and its UI.
struct Popup<T> {
    host: Host,
    ui: T,
}

struct Shell {
    display: Display,
    vfs: Option<vfs::Client>,
    launcher: Option<launcher::Client>,
    model: Model,
    desktop_host: Host,
    desktop: desktop::Desktop,
    taskbar_host: Host,
    taskbar: taskbar::Taskbar,
    audio: Option<audio::Client>,
    start: Option<Popup<start::StartMenu>>,
    calendar: Option<Popup<calendar::Calendar>>,
    volume: Option<Popup<volume::VolumeFlyout>>,
    wifi: Option<Popup<wifi::WifiFlyout>>,
    /// Wi-Fi events (`None` while the Wi-Fi service is not running).
    wifi_watch: Option<vnet::wifi::Watcher>,
    /// When to next look at the network state (and retry the watcher).
    next_net_poll: u64,
    tooltip: Option<tooltip::Tooltip>,
    notes: notify::Notifications,
    listener: Option<Channel>,
    clients: Vec<(Channel, ClientIdentity)>,
    /// Who sent the request being answered.
    caller: ClientIdentity,
    /// Crash reports from the launcher (`launcher::watch`).
    task_events: Option<Channel>,
    /// Windows minimised by "show desktop", restored by the next click.
    hidden_by_show_desktop: Vec<u32>,
    /// The agent's interface link (`None` while the agent is not running).
    agent: Option<agent::AgentLink>,
    agent_win: Option<Popup<agent::AgentWindow>>,
    /// When to try attaching the agent's interface again.
    next_agent_try: u64,
}

/// Connects to a service only if it is running: a call to a service that
/// never registers would wait forever.
pub fn connect_running(name: &str) -> Option<Channel> {
    let names = vproto::with_registry(|r| r.list()).ok()?.ok()?;
    if names.iter().any(|n| n == name) { vproto::connect(name).ok() } else { None }
}

/// Reads a whole file.
pub fn read_file(vfs: &vfs::Client, path: &str) -> Option<Vec<u8>> {
    let (vmo, len) = vfs.read_file(path.into()).ok()?.ok()?;
    let mut buf = alloc::vec![0u8; len as usize];
    vmo.read(0, &mut buf).ok()?;
    Some(buf)
}

/// Replaces a file's contents, creating parent directories as needed.
fn write_file(vfs: &vfs::Client, path: &str, data: &[u8]) -> bool {
    let mut dir = String::new();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        dir.push('/');
        dir.push_str(part);
        let _ = vfs.mkdir(dir.clone());
    }
    let Ok(vmo) = Vmo::create(data.len().max(1)) else { return false };
    if vmo.write(0, data).is_err() {
        return false;
    }
    matches!(vfs.write_file(path.into(), vmo, data.len() as u64), Ok(Ok(())))
}

impl Shell {
    /// Loads the wallpaper at `path` (or the procedural one).
    fn load_wallpaper(&self, path: &str) -> Option<Wallpaper> {
        let (w, h) = (self.model.screen.w, self.model.screen.h);
        if path == wallpaper::PROCEDURAL {
            return Some(Wallpaper::procedural(w, h));
        }
        let bytes = read_file(self.vfs.as_ref()?, path)?;
        Wallpaper::load(path, &bytes, w, h)
    }

    fn set_wallpaper(&mut self, wp: Wallpaper) {
        if let Some(vfs) = &self.vfs {
            write_file(vfs, WALLPAPER_CONFIG, wp.path.as_bytes());
        }
        self.model.wallpaper = wp;
        self.model.wallpaper_generation += 1;
        self.invalidate_all();
    }

    fn wallpapers(&self) -> Vec<String> {
        let mut list = self.vfs.as_ref().map(wallpaper::list).unwrap_or_default();
        list.push(wallpaper::PROCEDURAL.into());
        list
    }

    fn next_wallpaper(&mut self) {
        let list = self.wallpapers();
        let cur = list.iter().position(|p| *p == self.model.wallpaper.path);
        let next = cur.map_or(0, |i| (i + 1) % list.len());
        if let Some(wp) = self.load_wallpaper(&list[next]) {
            self.set_wallpaper(wp);
        }
    }

    fn invalidate_all(&mut self) {
        self.desktop_host.invalidate();
        self.taskbar_host.invalidate();
        if let Some(p) = &mut self.start {
            p.host.invalidate();
        }
        if let Some(p) = &mut self.calendar {
            p.host.invalidate();
        }
        if let Some(p) = &mut self.volume {
            p.host.invalidate();
        }
        if let Some(p) = &mut self.wifi {
            p.host.invalidate();
        }
        self.notes.invalidate_all();
    }

    /// Re-reads the audio system's state, reconnecting if the audio
    /// service was restarted.
    fn refresh_audio(&mut self) {
        if self.audio.is_none() {
            self.audio = connect_running(audio::NAME).map(audio::Client::new);
        }
        self.model.audio = self.audio.as_ref().and_then(|a| a.status().ok());
        if self.model.audio.is_none() {
            self.audio = None;
        }
        self.taskbar_host.invalidate();
    }

    fn set_volume(&mut self, volume: f32, muted: bool) {
        let Some(a) = &self.audio else { return };
        let volume = volume.clamp(0.0, 1.0);
        let _ = a.set_master(volume, muted);
        if let Some(st) = &mut self.model.audio {
            st.master_volume = volume;
            st.muted = muted;
        }
        self.taskbar_host.invalidate();
        if let Some(p) = &mut self.volume {
            p.host.invalidate();
        }
    }

    fn refresh_windows(&mut self) {
        if let Ok(list) = self.display.list_windows() {
            self.model.windows =
                list.into_iter().filter(|w| matches!(w.kind, WindowKind::Normal | WindowKind::Borderless)).collect();
        }
        self.taskbar_host.invalidate();
    }

    fn launch(&mut self, app: &str, args: Vec<String>) {
        let Some(launcher) = &self.launcher else { return };
        let name = self.model.app(app).map(|a| a.name.clone()).unwrap_or_else(|| app.into());
        match launcher.launch_app(app.into(), args) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                println!("cannot start {app}: {:?}", e);
                self.post_notification(
                    &format!("Couldn't open {name}"),
                    &format!("The application failed to start ({e:?})."),
                    "warning",
                );
            }
            Err(e) => println!("launcher unavailable: {}", e),
        }
    }

    fn post_notification(&mut self, title: &str, body: &str, icon: &str) {
        self.notes.post(&self.display, &self.model, title, body, icon);
    }

    fn popup_spec(title: &str, r: Rect) -> WindowSpec {
        let mut spec = WindowSpec::new(title, r.w as u32, r.h as u32);
        spec.kind = WindowKind::Popup;
        spec.x = r.x;
        spec.y = r.y;
        spec.resizable = false;
        spec.app_id = "shell".into();
        spec
    }

    fn open_start(&mut self) {
        self.close_calendar();
        self.close_volume();
        self.close_wifi();
        let r = start::placement(self.model.screen);
        match Host::new(&self.display, Self::popup_spec("Start", r)) {
            Ok(host) => {
                self.start = Some(Popup { host, ui: start::StartMenu::new((r.x, r.y)) });
                self.model.start_open = true;
                self.taskbar_host.invalidate();
            }
            Err(e) => println!("cannot open the start menu: {:?}", e),
        }
    }

    fn close_start(&mut self) {
        if self.start.take().is_some() {
            self.model.start_open = false;
            self.taskbar_host.invalidate();
        }
    }

    fn open_calendar(&mut self) {
        self.close_start();
        self.close_volume();
        self.close_wifi();
        let r = calendar::placement(self.model.screen);
        match Host::new(&self.display, Self::popup_spec("Calendar", r)) {
            Ok(host) => {
                self.calendar = Some(Popup { host, ui: calendar::Calendar::new((r.x, r.y)) });
                self.model.calendar_open = true;
                self.taskbar_host.invalidate();
            }
            Err(e) => println!("cannot open the calendar: {:?}", e),
        }
    }

    fn close_calendar(&mut self) {
        if self.calendar.take().is_some() {
            self.model.calendar_open = false;
            self.taskbar_host.invalidate();
        }
    }

    fn open_volume(&mut self) {
        self.close_start();
        self.close_calendar();
        self.close_wifi();
        self.refresh_audio();
        let r = volume::placement(self.model.screen);
        match Host::new(&self.display, Self::popup_spec("Volume", r)) {
            Ok(host) => {
                self.volume = Some(Popup { host, ui: volume::VolumeFlyout::new((r.x, r.y)) });
                self.model.volume_open = true;
                self.taskbar_host.invalidate();
            }
            Err(e) => println!("cannot open the volume control: {:?}", e),
        }
    }

    fn open_agent(&mut self) {
        self.close_start();
        self.close_calendar();
        self.close_volume();
        self.close_wifi();
        let r = agent::placement(self.model.screen);
        let mut spec = WindowSpec::new(self.model.agent.name(), r.w as u32, r.h as u32);
        // A panel: it stays above the windows the agent opens or the user
        // works in, without taking the keyboard from them.
        spec.kind = WindowKind::Panel;
        spec.x = r.x;
        spec.y = r.y;
        spec.resizable = false;
        spec.app_id = "agent".into();
        match Host::new(&self.display, spec) {
            Ok(host) => {
                self.agent_win = Some(Popup { host, ui: agent::AgentWindow::new() });
                self.model.agent.window_open = true;
                // Approvals now appear in the window.
                self.notes.remove_approvals(self.model.screen);
                if let Some(a) = &self.agent {
                    a.set_window_open(true);
                    // Opening the agent means wanting to talk.
                    if !self.model.agent.active() {
                        a.wake();
                    }
                }
            }
            Err(e) => println!("cannot open the agent's window: {:?}", e),
        }
    }

    fn close_agent(&mut self) {
        if self.agent_win.take().is_none() {
            return;
        }
        self.model.agent.window_open = false;
        if let Some(a) = &self.agent {
            a.set_window_open(false);
        }
        for req in self.model.agent.approvals.clone() {
            self.notes.post_approval(&self.display, &self.model, &req);
        }
    }

    /// Attaches the agent's interface and takes its events.
    fn check_agent(&mut self) {
        let now = vrt::time::now_ns();
        if self.agent.is_none() && now >= self.next_agent_try {
            self.next_agent_try = now + 2_000_000_000;
            self.agent = agent::AgentLink::connect();
            if let Some(a) = &self.agent {
                a.set_window_open(self.agent_win.is_some());
            }
        }
        let Some(link) = &self.agent else { return };
        let (voice, mic) = link.levels();
        let (v0, m0) = self.model.agent.levels;
        let smooth = |old: f32, new: f32| if new > old { new } else { old * 0.75 + new * 0.25 };
        self.model.agent.levels = (smooth(v0, voice), smooth(m0, mic));
        let Some(events) = link.take_events() else {
            // The agent service went away; it will be restarted.
            self.agent = None;
            self.model.agent.status = None;
            self.model.agent.approvals.clear();
            self.notes.remove_approvals(self.model.screen);
            self.taskbar_host.invalidate();
            return;
        };
        let changed = !events.is_empty();
        for ev in events {
            match ev {
                vproto::agent::AgentEvent::Status { status } => self.model.agent.set_status(status, now),
                vproto::agent::AgentEvent::Approval { request } => {
                    if !self.model.agent.approvals.iter().any(|r| r.id == request.id) {
                        if self.agent_win.is_none() {
                            self.notes.post_approval(&self.display, &self.model, &request);
                        }
                        self.model.agent.approvals.push(request);
                    }
                }
                vproto::agent::AgentEvent::ApprovalDone { id } => {
                    self.model.agent.approvals.retain(|r| r.id != id);
                    self.notes.remove_approval(id, self.model.screen);
                }
            }
        }
        // The orb is redrawn when something changed, and keeps moving
        // while the agent talks or listens (the wait wakes us for that).
        if changed || self.model.agent.active() {
            self.taskbar_host.invalidate();
        }
        if changed && let Some(p) = &mut self.agent_win {
            p.host.invalidate();
        }
    }

    fn close_volume(&mut self) {
        if self.volume.take().is_some() {
            self.model.volume_open = false;
            self.taskbar_host.invalidate();
        }
    }

    fn open_wifi(&mut self) {
        self.close_start();
        self.close_calendar();
        self.close_volume();
        self.refresh_wifi();
        let _ = vnet::wifi::scan();
        let r = wifi::placement(self.model.screen);
        match Host::new(&self.display, Self::popup_spec("Network", r)) {
            Ok(host) => {
                self.wifi = Some(Popup { host, ui: wifi::WifiFlyout::new((r.x, r.y)) });
                self.model.wifi_open = true;
                self.taskbar_host.invalidate();
            }
            Err(e) => println!("cannot open the network flyout: {:?}", e),
        }
    }

    fn close_wifi(&mut self) {
        if self.wifi.take().is_some() {
            self.model.wifi_open = false;
            self.taskbar_host.invalidate();
        }
    }

    fn invalidate_network(&mut self) {
        self.taskbar_host.invalidate();
        if let Some(p) = &mut self.wifi {
            p.host.invalidate();
        }
    }

    /// Re-reads the Wi-Fi status and the networks in range.
    fn refresh_wifi(&mut self) {
        match vnet::wifi::status() {
            Ok(s) => {
                self.model.wifi = Some(s);
                self.model.wifi_networks = vnet::wifi::networks().unwrap_or_default();
            }
            Err(_) => {
                self.model.wifi = None;
                self.model.wifi_networks.clear();
                self.wifi_watch = None;
            }
        }
        self.invalidate_network();
    }

    /// Looks at the wired network, and (re)connects to the Wi-Fi service's
    /// events if needed. Runs every few seconds.
    fn poll_network(&mut self) {
        let now = vrt::time::now_ns();
        if now < self.next_net_poll {
            return;
        }
        // Every second while the Wi-Fi service is away (it may be
        // restarting), otherwise every five.
        self.next_net_poll = now + if self.wifi_watch.is_none() { 1_000_000_000 } else { 5_000_000_000 };
        let (up, label) = match vnet::interfaces() {
            Ok(list) => match list
                .iter()
                .find(|i| i.kind == vnet::InterfaceKind::Ethernet && i.link_up && !i.gateways.is_empty())
            {
                Some(i) => (true, format!("Wired: {} connected", i.name)),
                None => (false, String::new()),
            },
            Err(_) => (false, String::new()),
        };
        if (up, &label) != (self.model.wired_up, &self.model.wired_label) {
            self.model.wired_up = up;
            self.model.wired_label = label;
            self.invalidate_network();
        }
        if self.wifi_watch.is_none() && vnet::wifi::available() {
            self.wifi_watch = vnet::wifi::Watcher::new().ok();
            self.refresh_wifi();
        }
    }

    /// Handles Wi-Fi events: status changes, finished scans, failures.
    fn check_wifi(&mut self) {
        let Some(w) = &self.wifi_watch else { return };
        let mut events = Vec::new();
        let gone = loop {
            match w.try_next() {
                Ok(Some(ev)) => events.push(ev),
                Ok(None) => break false,
                Err(_) => break true,
            }
        };
        if gone {
            self.wifi_watch = None;
            self.model.wifi = None;
            self.model.wifi_networks.clear();
            self.invalidate_network();
        }
        for ev in events {
            match ev {
                vnet::wifi::WlanEvent::StatusChanged { status } => {
                    let changed_network = self.model.wifi.as_ref().map(|s| (s.state, s.ssid.clone()))
                        != Some((status.state, status.ssid.clone()));
                    if status.state == vnet::wifi::ConnState::Connected {
                        self.model.wifi_error = None;
                    }
                    self.model.wifi = Some(status);
                    if changed_network {
                        self.model.wifi_networks = vnet::wifi::networks().unwrap_or_default();
                    }
                    self.invalidate_network();
                }
                vnet::wifi::WlanEvent::ScanDone {} => {
                    self.model.wifi_networks = vnet::wifi::networks().unwrap_or_default();
                    self.invalidate_network();
                }
                vnet::wifi::WlanEvent::ConnectFailed { ssid, name, reason } => {
                    let why = format!("{reason}");
                    let mut msg = why.clone();
                    if let Some(c) = msg.get_mut(0..1) {
                        c.make_ascii_uppercase();
                    }
                    self.model.wifi_error = Some((ssid.0, msg));
                    if self.wifi.is_none() {
                        self.post_notification(
                            &format!("Couldn't connect to {name}"),
                            &format!("Because {why}."),
                            "wifi",
                        );
                    }
                    self.invalidate_network();
                }
            }
        }
    }

    fn wifi_connect(&mut self, ssid: Vec<u8>, password: Option<String>, auto: bool, hidden: bool) {
        self.model.wifi_error = None;
        let opts = vnet::wifi::ConnectOptions { security: None, save: true, auto_connect: auto, hidden };
        if let Err(e) = vnet::wifi::connect_with(&ssid, password.as_deref(), &opts) {
            let mut msg = format!("{e}");
            if let Some(c) = msg.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            self.model.wifi_error = Some((ssid, msg));
        }
        self.invalidate_network();
    }

    fn show_desktop(&mut self) {
        let visible: Vec<u32> =
            self.model.windows.iter().filter(|w| w.state != WindowState::Minimized).map(|w| w.id).collect();
        if visible.is_empty() {
            for id in core::mem::take(&mut self.hidden_by_show_desktop) {
                let _ = self.display.activate_window(id);
            }
        } else {
            for &id in &visible {
                let _ = self.display.minimize_window(id);
            }
            self.hidden_by_show_desktop = visible;
        }
    }

    /// Shows, replaces or hides the taskbar tooltip.
    fn update_tooltip(&mut self) {
        let popup_open =
            self.start.is_some() || self.calendar.is_some() || self.volume.is_some() || self.wifi.is_some();
        let due = self.model.tip.as_ref().filter(|t| !popup_open && vrt::time::now_ns() >= t.since + tooltip::DELAY_NS);
        match due {
            Some(tip) => {
                if self.tooltip.as_ref().map(|t| t.label()) != Some(tip.label.as_str()) {
                    self.tooltip = tooltip::Tooltip::open(&self.display, self.model.screen, tip);
                }
            }
            None => self.tooltip = None,
        }
        if let Some(t) = &mut self.tooltip {
            t.pump();
        }
    }

    // ---- main loop ----------------------------------------------------------

    /// Lets every surface process its events and draw.
    fn pump(&mut self) {
        let now = vrt::time::now_ns();
        let Shell { model, desktop_host, desktop, taskbar_host, taskbar, start, calendar, volume, wifi, notes, .. } =
            self;
        desktop_host.pump(|ui| desktop.update(ui, model));
        let mut windows_changed = false;
        for ev in taskbar_host.pump(|ui| taskbar.update(ui, model)) {
            match ev {
                WindowEvent::WindowsChanged {} => windows_changed = true,
                WindowEvent::StartMenuKey {} => model.push(Action::ToggleStart),
                WindowEvent::AgentKey {} => model.push(Action::ToggleAgent),
                _ => {}
            }
        }
        if let Some(p) = &mut self.agent_win {
            let events = p.host.pump(|ui| p.ui.update(ui, model));
            if p.host.window.closed
                || p.host.close_requested
                || events.iter().any(|e| matches!(e, WindowEvent::CloseRequested {}))
            {
                model.push(Action::CloseAgent);
            }
        }
        if let Some(p) = start {
            let events = p.host.pump(|ui| p.ui.update(ui, model));
            if p.host.window.closed || events.iter().any(|e| matches!(e, WindowEvent::CloseRequested {})) {
                model.start_dismissed_at = now;
                model.push(Action::CloseStart);
            } else if p.host.close_requested {
                model.push(Action::CloseStart);
            }
        }
        if let Some(p) = calendar {
            let events = p.host.pump(|ui| p.ui.update(ui, model));
            if p.host.window.closed || events.iter().any(|e| matches!(e, WindowEvent::CloseRequested {})) {
                model.calendar_dismissed_at = now;
                model.push(Action::CloseCalendar);
            } else if p.host.close_requested {
                model.push(Action::CloseCalendar);
            }
        }
        if let Some(p) = volume {
            let events = p.host.pump(|ui| p.ui.update(ui, model));
            if p.host.window.closed || events.iter().any(|e| matches!(e, WindowEvent::CloseRequested {})) {
                model.volume_dismissed_at = now;
                model.push(Action::CloseVolume);
            } else if p.host.close_requested {
                model.push(Action::CloseVolume);
            }
        }
        if let Some(p) = wifi {
            let events = p.host.pump(|ui| p.ui.update(ui, model));
            if p.host.window.closed || events.iter().any(|e| matches!(e, WindowEvent::CloseRequested {})) {
                model.wifi_dismissed_at = now;
                model.push(Action::CloseWifi);
            } else if p.host.close_requested {
                model.push(Action::CloseWifi);
            }
        }
        notes.pump(model);
        if windows_changed {
            self.refresh_windows();
        }
    }

    fn perform_actions(&mut self) {
        for action in core::mem::take(&mut self.model.actions) {
            match action {
                Action::Launch(app, args) => {
                    self.close_start();
                    self.launch(&app, args);
                }
                Action::Activate(id) => {
                    let _ = self.display.activate_window(id);
                }
                Action::Minimize(id) => {
                    let _ = self.display.minimize_window(id);
                }
                Action::ToggleStart => {
                    if self.start.is_some() {
                        self.close_start();
                    } else {
                        self.open_start();
                    }
                }
                Action::CloseStart => self.close_start(),
                Action::ToggleCalendar => {
                    if self.calendar.is_some() {
                        self.close_calendar();
                    } else {
                        self.open_calendar();
                    }
                }
                Action::CloseCalendar => self.close_calendar(),
                Action::ToggleVolume => {
                    if self.volume.is_some() {
                        self.close_volume();
                    } else {
                        self.open_volume();
                    }
                }
                Action::CloseVolume => self.close_volume(),
                Action::ToggleWifi => {
                    if self.wifi.is_some() {
                        self.close_wifi();
                    } else {
                        self.open_wifi();
                    }
                }
                Action::CloseWifi => self.close_wifi(),
                Action::WifiRadio(on) => {
                    let _ = vnet::wifi::set_radio(on);
                    self.refresh_wifi();
                }
                Action::WifiScan => {
                    let _ = vnet::wifi::scan();
                    self.refresh_wifi();
                }
                Action::WifiConnect { ssid, password, auto, hidden } => self.wifi_connect(ssid, password, auto, hidden),
                Action::WifiDisconnect => {
                    let _ = vnet::wifi::disconnect();
                    self.refresh_wifi();
                }
                Action::ToggleAgent => {
                    if self.agent_win.is_some() {
                        self.close_agent();
                    } else {
                        self.open_agent();
                    }
                }
                Action::CloseAgent => self.close_agent(),
                Action::AgentWake => {
                    if let Some(a) = &self.agent {
                        a.wake();
                    }
                }
                Action::AgentSleep => {
                    if let Some(a) = &self.agent {
                        a.sleep();
                    }
                }
                Action::AgentDecide { id, allow, always } => {
                    if let Some(a) = &self.agent {
                        a.decide(id, allow, always);
                    }
                    self.model.agent.approvals.retain(|r| r.id != id);
                    self.notes.remove_approval(id, self.model.screen);
                    self.taskbar_host.invalidate();
                }
                Action::AgentMute(muted) => {
                    if let Some(a) = &self.agent {
                        a.set_muted(muted);
                    }
                }
                Action::OpenNetworkSettings => {
                    self.close_wifi();
                    self.launch("settings", alloc::vec![String::from("network")]);
                }
                Action::SetVolume(v, muted) => self.set_volume(v, muted),
                Action::ShowDesktop => self.show_desktop(),
                Action::Power(what) => {
                    self.close_start();
                    // Write the home directory to disk before the power goes.
                    if let Some(vfs) = &self.vfs {
                        let _ = vfs.sync();
                    }
                    if let Some(l) = &self.launcher {
                        let _ = l.power(what);
                    }
                }
                Action::NextWallpaper => self.next_wallpaper(),
                Action::RefreshDesktop => {
                    self.desktop.refresh(&self.model, self.vfs.as_ref());
                    self.desktop_host.invalidate();
                }
            }
        }
    }

    /// Accepts connections and answers `shell` protocol requests.
    fn serve(&mut self) {
        if let Some(l) = &self.listener {
            while let Some(client) = vproto::accept_with_identity(l) {
                self.clients.push(client);
            }
        }
        for (ch, who) in core::mem::take(&mut self.clients) {
            self.caller = who.clone();
            let alive = loop {
                match ch.read() {
                    Ok(msg) => match shell::dispatch(self, msg) {
                        Ok(reply) => {
                            let _ = reply.send(&ch);
                        }
                        Err(e) => println!("bad request: {}", e),
                    },
                    Err(vabi::Error::ShouldWait) => break true,
                    Err(_) => break false,
                }
            };
            if alive {
                self.clients.push((ch, who));
            }
        }
    }

    /// Turns the launcher's crash reports into notifications.
    fn check_tasks(&mut self) {
        let Some(ch) = self.task_events.take() else { return };
        let mut crashed = Vec::new();
        let alive = loop {
            match ch.read() {
                Ok(msg) => {
                    if let Ok((_, TaskEvent::Crashed { name, .. })) = vipc::decode_event::<TaskEvent>(msg) {
                        crashed.push(name);
                    }
                }
                Err(vabi::Error::ShouldWait) => break true,
                Err(_) => break false,
            }
        };
        if alive {
            self.task_events = Some(ch);
        }
        for name in crashed {
            let (title, body, icon) = match self.model.app(&name) {
                Some(app) => (
                    app.name.clone(),
                    "It stopped because of an error. You can open it again from the start menu.",
                    app.id.clone(),
                ),
                None => (name, "It stopped because of an error.", String::from("warning")),
            };
            println!("{} closed unexpectedly", title);
            self.post_notification(&format!("{title} closed unexpectedly"), body, &icon);
        }
    }

    /// Blocks until a surface, client or timer needs attention.
    fn wait(&mut self) {
        let mut items: Vec<WaitItem> = Vec::new();
        let mut deadline = vabi::DEADLINE_INFINITE;
        let mut add_host = |h: &Host, items: &mut Vec<WaitItem>| {
            items.push(h.wait_item());
            deadline = deadline.min(h.deadline());
        };
        add_host(&self.desktop_host, &mut items);
        add_host(&self.taskbar_host, &mut items);
        if let Some(p) = &self.start {
            add_host(&p.host, &mut items);
        }
        if let Some(p) = &self.calendar {
            add_host(&p.host, &mut items);
        }
        if let Some(p) = &self.volume {
            add_host(&p.host, &mut items);
        }
        if let Some(p) = &self.wifi {
            add_host(&p.host, &mut items);
        }
        if let Some(p) = &self.agent_win {
            add_host(&p.host, &mut items);
        }
        if let Some(t) = &self.tooltip {
            add_host(t.host(), &mut items);
        }
        for h in self.notes.hosts() {
            add_host(h, &mut items);
        }
        deadline = deadline.min(self.notes.deadline());
        if let (Some(tip), None) = (&self.model.tip, &self.tooltip) {
            deadline = deadline.min(tip.since + tooltip::DELAY_NS);
        }
        let readable = signals::READABLE | signals::PEER_CLOSED;
        if let Some(l) = &self.listener {
            items.push(WaitItem { handle: l.raw(), signals: readable, ..Default::default() });
        }
        for (c, _) in &self.clients {
            items.push(WaitItem { handle: c.raw(), signals: readable, ..Default::default() });
        }
        if let Some(c) = &self.task_events {
            items.push(WaitItem { handle: c.raw(), signals: readable, ..Default::default() });
        }
        if let Some(w) = &self.wifi_watch {
            items.push(WaitItem { handle: w.handle().raw(), signals: readable, ..Default::default() });
        }
        match &self.agent {
            Some(a) => {
                items.push(WaitItem { handle: a.events_handle(), signals: readable, ..Default::default() });
                // The tray's orb moves while the agent talks or listens.
                if self.model.agent.active() {
                    deadline = deadline.min(vrt::time::now_ns() + 33_000_000);
                }
            }
            None => deadline = deadline.min(self.next_agent_try),
        }
        deadline = deadline.min(self.next_net_poll);
        items.truncate(vabi::WAIT_MANY_MAX);
        if !self.model.actions.is_empty() {
            deadline = 0;
        }
        let _ = vrt::object::wait_many(&mut items, deadline);
    }

    /// Runs until the display goes away (the window system stopped); `init`
    /// then restarts the shell along with it. Returns the exit code.
    fn run(&mut self) -> i32 {
        loop {
            self.pump();
            self.update_tooltip();
            if self.desktop_host.window.closed || self.taskbar_host.window.closed {
                println!("lost the display; exiting");
                return 2;
            }
            self.perform_actions();
            self.serve();
            self.check_tasks();
            self.poll_network();
            self.check_wifi();
            self.check_agent();
            self.wait();
        }
    }
}

impl shell::Server for Shell {
    fn set_wallpaper(&mut self, path: String) -> Result<(), ShellError> {
        if path != wallpaper::PROCEDURAL {
            let vfs = self.vfs.as_ref().ok_or(ShellError::Unavailable)?;
            vfs.stat(path.clone()).ok().and_then(|r| r.ok()).ok_or(ShellError::NotFound)?;
        }
        let wp = self.load_wallpaper(&path).ok_or(ShellError::BadImage)?;
        self.set_wallpaper(wp);
        Ok(())
    }

    fn wallpaper(&mut self) -> String {
        self.model.wallpaper.path.clone()
    }

    fn notify(&mut self, title: String, body: String, icon: String) {
        self.post_notification(&title, &body, &icon);
    }

    fn wallpapers(&mut self) -> Vec<String> {
        Shell::wallpapers(self)
    }

    fn window_action(&mut self, window: u32, action: String) -> Result<(), ShellError> {
        if !(self.caller.service && self.caller.name == "agent") {
            return Err(ShellError::Unavailable);
        }
        use vproto::display::arrangement;
        let r = match action.as_str() {
            "show_desktop" => {
                self.show_desktop();
                return Ok(());
            }
            "focus" => self.display.activate_window(window),
            "minimize" => self.display.minimize_window(window),
            "close" => self.display.close_window(window),
            "maximize" => self.display.arrange_window(window, arrangement::MAXIMIZE),
            "restore" => self.display.arrange_window(window, arrangement::RESTORE),
            "snap_left" => self.display.arrange_window(window, arrangement::SNAP_LEFT),
            "snap_right" => self.display.arrange_window(window, arrangement::SNAP_RIGHT),
            _ => return Err(ShellError::NotFound),
        };
        match r {
            Ok(Ok(())) => Ok(()),
            _ => Err(ShellError::NotFound),
        }
    }
}

fn main() -> i32 {
    let display = match vui::window::connect() {
        Ok(d) => d,
        Err(e) => {
            println!("cannot connect to the display: {:?}", e);
            return 1;
        }
    };
    let screen = match display.screen_info() {
        Ok(s) => Rect::new(0, 0, s.width as i32, s.height as i32),
        Err(e) => {
            println!("cannot query the screen: {}", e);
            return 1;
        }
    };
    let vfs = vproto::connect(vfs::NAME).ok().map(vfs::Client::new);
    let launcher = vproto::connect(launcher::NAME).ok().map(launcher::Client::new);
    let task_events = launcher.as_ref().and_then(|l| l.watch().ok()).and_then(|r| r.ok());
    let audio = connect_running(audio::NAME).map(audio::Client::new);
    let mut apps = launcher.as_ref().and_then(|l| l.apps().ok()).unwrap_or_default();
    apps::sort(&mut apps);

    // The remembered wallpaper, else the first one installed.
    let saved = vfs.as_ref().and_then(|v| read_file(v, WALLPAPER_CONFIG)).and_then(|b| String::from_utf8(b).ok());
    let installed = vfs.as_ref().map(wallpaper::list).unwrap_or_default();
    let candidates = saved.into_iter().chain(installed.into_iter().take(1));
    let mut wp = None;
    for path in candidates {
        let bytes = vfs.as_ref().and_then(|v| read_file(v, &path));
        wp = bytes.and_then(|b| Wallpaper::load(&path, &b, screen.w, screen.h));
        if wp.is_some() {
            break;
        }
    }
    let wallpaper = wp.unwrap_or_else(|| Wallpaper::procedural(screen.w, screen.h));

    let mut spec = WindowSpec::new("Desktop", screen.w as u32, screen.h as u32);
    spec.kind = WindowKind::Desktop;
    spec.x = 0;
    spec.y = 0;
    spec.resizable = false;
    spec.app_id = "shell".into();
    let desktop_host = match Host::new(&display, spec) {
        Ok(h) => h,
        Err(e) => {
            println!("cannot create the desktop: {:?}", e);
            return 1;
        }
    };
    let mut spec = WindowSpec::new("Taskbar", screen.w as u32, taskbar::HEIGHT as u32);
    spec.kind = WindowKind::Panel;
    spec.x = 0;
    spec.y = screen.h - taskbar::HEIGHT;
    spec.resizable = false;
    spec.app_id = "shell".into();
    let taskbar_host = match Host::new(&display, spec) {
        Ok(h) => h,
        Err(e) => {
            println!("cannot create the taskbar: {:?}", e);
            return 1;
        }
    };

    let listener = vproto::register(shell::NAME).ok();
    if listener.is_none() {
        println!("cannot register the shell service");
    }
    let model = Model {
        screen,
        apps,
        windows: Vec::new(),
        wallpaper,
        wallpaper_generation: 0,
        audio: None,
        wifi: None,
        wifi_networks: Vec::new(),
        wifi_error: None,
        wired_up: false,
        wired_label: String::new(),
        start_open: false,
        calendar_open: false,
        volume_open: false,
        wifi_open: false,
        start_dismissed_at: 0,
        calendar_dismissed_at: 0,
        volume_dismissed_at: 0,
        wifi_dismissed_at: 0,
        tip: None,
        agent: agent::AgentModel::default(),
        actions: Vec::new(),
    };
    let mut desktop = desktop::Desktop::new();
    desktop.refresh(&model, vfs.as_ref());
    let mut shell = Shell {
        display,
        vfs,
        launcher,
        model,
        desktop_host,
        desktop,
        taskbar_host,
        taskbar: taskbar::Taskbar::new(),
        audio,
        start: None,
        calendar: None,
        volume: None,
        wifi: None,
        wifi_watch: None,
        next_net_poll: 0,
        tooltip: None,
        notes: notify::Notifications::new(),
        listener,
        clients: Vec::new(),
        caller: ClientIdentity::default(),
        agent: None,
        agent_win: None,
        next_agent_try: 0,
        task_events,
        hidden_by_show_desktop: Vec::new(),
    };
    shell.refresh_windows();
    shell.refresh_audio();
    println!("desktop ready ({}x{}, {} apps)", screen.w, screen.h, shell.model.apps.len());
    shell.run()
}
