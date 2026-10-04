//! Settings: personalise Vindows and look up system details.
//!
//! * **Personalization**: a gallery of the system wallpapers (from the desktop
//!   shell, or `/system/wallpapers` when the shell is not running) and of the
//!   pictures in `~/Pictures`; clicking one makes it the desktop wallpaper.
//! * **Agent**: the voice agent: Deepgram API key, name and voice, language
//!   and speech models, listening, and what it remembers.
//! * **Network & Internet**: Wi-Fi (switch, connection, networks in range,
//!   saved networks), interfaces and addresses, Wi-Fi diagnostics.
//! * **Display**: the screen resolution and the work area left by panels.
//! * **System**: version, processor, memory and uptime, refreshed live.
//! * **About**: version information and licences.
//!
//! [`control`] lets the voice agent show pages and change its own voice,
//! name and language model.
//!
//! Usage: `settings [personalization|agent|network|display|system|about]`.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod control;
mod network;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vgfx::{Bitmap, FillRule, Filter, LineJoin, Path, StrokeStyle};
use vmath::FloatExt;
use vproto::display::{ScreenInfo, modifiers};
use vproto::init::launcher;
use vproto::input::keys;
use vui::{Align, App, ButtonKind, Color, Cursor, Font, Icon, Rect, Ui, WindowSpec};

use vfiles::Fs;
use vfiles::thumbs::Thumbnailer;
use vproto::shell::{ShellLink, ShellReply};

vrt::entry!(main);

/// The shell's name for its built-in generated wallpaper.
const PROCEDURAL: &str = "procedural";
const WALLPAPER_DIR: &str = "/system/wallpapers";
const PICTURES_DIR: &str = "/home/user/Pictures";
const SIDEBAR_W: i32 = 230;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Personalization,
    Agent,
    Network,
    Display,
    System,
    About,
}

const SECTIONS: [(Section, &str, Icon, &str); 6] = [
    (Section::Personalization, "Personalization", Icon::Palette, "Wallpaper"),
    (Section::Agent, "Agent", Icon::Agent, "Voice, model, memory"),
    (Section::Network, "Network & Internet", Icon::Wifi, "Wi-Fi, Ethernet, status"),
    (Section::Display, "Display", Icon::Monitor, "Resolution, work area"),
    (Section::System, "System", Icon::Cpu, "Processor, memory"),
    (Section::About, "About", Icon::Info, "Version, licences"),
];

/// Whether the desktop shell can change the wallpaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellState {
    Checking,
    Available,
    Unavailable,
}

struct Settings {
    section: Section,
    fs: Fs,
    shell: Option<ShellLink>,
    shell_state: ShellState,
    /// When to ask the shell again after it was unavailable.
    retry_at: u64,
    wallpapers: Vec<String>,
    pictures: Vec<String>,
    current: String,
    /// The wallpaper being applied and since when.
    applying: Option<(String, u64)>,
    /// "Wallpaper changed" confirmation until this time.
    applied_until: u64,
    error: Option<String>,
    thumbs: Option<Thumbnailer>,
    info: vabi::SystemInfo,
    info_at: u64,
    screen: Option<ScreenInfo>,
    licence: Option<String>,
    show_licence: bool,
    /// Keyboard cursor in the wallpaper gallery (index into choices()).
    cursor: Option<usize>,
    /// Height of the content drawn last frame (for scrolling).
    content_h: i32,
    net: network::NetworkPage,
    /// Started the first time the Agent page is shown.
    agent: Option<agent::AgentPage>,
}

/// The pictures (files Photos can open) in `dir`, as sorted paths.
fn images_in(fs: &Fs, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = fs
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| !e.is_dir && vfiles::kind::is_image(&e.name))
        .map(|e| vfiles::path::join(dir, &e.name))
        .collect();
    v.sort();
    v
}

/// A title for a picture: its file name without the extension, with dashes
/// and underscores as spaces and a capital first letter ("misty-forest.jpg"
/// → "Misty forest").
fn title_of(path: &str) -> String {
    let mut out = String::new();
    for (i, c) in vfiles::path::file_stem(path).chars().enumerate() {
        let c = if c == '-' || c == '_' { ' ' } else { c };
        if i == 0 { out.extend(c.to_uppercase()) } else { out.push(c) }
    }
    out
}

fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).trim().to_string()
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes >> 20)
}

fn format_uptime(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    let (d, h, m) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60);
    match (d, h) {
        (0, 0) => format!("{m} min {} s", s % 60),
        (0, _) => format!("{h} h {m} min"),
        _ => format!("{d} d {h} h {m} min"),
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Paints over the corners of `r` outside a rounded rectangle of `radius`,
/// so an image drawn in `r` appears to have rounded corners.
fn mask_corners(ui: &mut Ui, r: Rect, radius: f32, bg: Color) {
    use core::f32::consts::{FRAC_PI_2, PI};
    let (l, t, rt, b) = (r.x as f32, r.y as f32, r.right() as f32, r.bottom() as f32);
    let k = radius;
    let mut p = Path::new();
    p.move_to(l, t);
    p.line_to(l + k, t);
    p.arc(l + k, t + k, k, -FRAC_PI_2, -FRAC_PI_2);
    p.close();
    p.move_to(rt, t);
    p.line_to(rt, t + k);
    p.arc(rt - k, t + k, k, 0.0, -FRAC_PI_2);
    p.close();
    p.move_to(rt, b);
    p.line_to(rt - k, b);
    p.arc(rt - k, b - k, k, FRAC_PI_2, -FRAC_PI_2);
    p.close();
    p.move_to(l, b);
    p.line_to(l, b - k);
    p.arc(l + k, b - k, k, PI, -FRAC_PI_2);
    p.close();
    ui.canvas.fill_path(&p, bg, FillRule::NonZero);
}

/// Draws a bitmap scaled to cover `r` (cropping the overflow).
fn draw_cover(ui: &mut Ui, r: Rect, bmp: &Bitmap) {
    let (bw, bh) = (bmp.width.max(1) as f32, bmp.height.max(1) as f32);
    let scale = (r.w as f32 / bw).max(r.h as f32 / bh);
    let (w, h) = ((bw * scale) as i32 + 1, (bh * scale) as i32 + 1);
    let dst = Rect::new(r.x + (r.w - w) / 2, r.y + (r.h - h) / 2, w, h);
    ui.canvas.save();
    ui.canvas.clip_to(r);
    ui.canvas.draw_bitmap_scaled(bmp, dst, Filter::Bilinear, 255);
    ui.canvas.restore();
}

/// A small rendition of the shell's generated wallpaper.
fn draw_procedural(ui: &mut Ui, r: Rect) {
    ui.canvas.save();
    ui.canvas.clip_to(r);
    ui.canvas.fill_vertical_gradient(r, Color::hex(0x0C1446), Color::hex(0x170A36));
    let blooms: [(f32, f32, f32, Color, f32); 4] = [
        (0.22, 0.30, 0.65, Color::rgb(59, 91, 255), 0.50),
        (0.80, 0.62, 0.60, Color::rgb(164, 59, 255), 0.42),
        (0.58, 0.12, 0.42, Color::rgb(31, 209, 255), 0.28),
        (0.45, 0.95, 0.50, Color::rgb(255, 70, 160), 0.18),
    ];
    for (fx, fy, fr, c, s) in blooms {
        let (cx, cy) = (r.x as f32 + fx * r.w as f32, r.y as f32 + fy * r.h as f32);
        let rad = fr * r.w as f32 * 0.55;
        for k in 0..7 {
            let f = 1.0 - k as f32 / 7.0;
            ui.canvas.fill_circle(cx, cy, rad * f, c.with_alpha((s * 34.0) as u8));
        }
    }
    let mut ribbon = Path::new();
    for i in 0..=48 {
        let fx = i as f32 / 48.0;
        let fy = 0.56 + 0.10 * (fx * 5.2 + 0.8).sin() + 0.04 * (fx * 13.0).sin();
        let (x, y) = (r.x as f32 + fx * r.w as f32, r.y as f32 + fy * r.h as f32);
        if i == 0 {
            ribbon.move_to(x, y);
        } else {
            ribbon.line_to(x, y);
        }
    }
    let glow = StrokeStyle::new((r.h as f32 / 14.0).max(3.0)).with_join(LineJoin::Round);
    ui.canvas.stroke_path(&ribbon, &glow, Color::rgba(190, 170, 255, 50));
    let core = StrokeStyle::new((r.h as f32 / 60.0).max(1.2)).with_join(LineJoin::Round);
    ui.canvas.stroke_path(&ribbon, &core, Color::rgba(235, 230, 255, 200));
    ui.canvas.restore();
}

impl Settings {
    fn new(section: Section) -> Settings {
        let fs = Fs::connect();
        let wallpapers = images_in(&fs, WALLPAPER_DIR);
        let pictures = images_in(&fs, PICTURES_DIR);
        let shell = ShellLink::start();
        if let Some(s) = &shell {
            s.request_wallpapers();
        }
        let licence = None;
        Settings {
            section,
            fs,
            shell,
            shell_state: ShellState::Checking,
            retry_at: 0,
            wallpapers,
            pictures,
            current: String::new(),
            applying: None,
            applied_until: 0,
            error: None,
            thumbs: Thumbnailer::new(320, 200),
            info: vrt::object::system_info().unwrap_or_default(),
            info_at: 0,
            screen: None,
            licence,
            show_licence: false,
            cursor: None,
            content_h: 600,
            net: network::NetworkPage::new(),
            agent: None,
        }
    }

    /// Every wallpaper choice: system wallpapers, then the user's pictures.
    fn choices(&self) -> Vec<String> {
        let mut v = self.wallpapers.clone();
        v.extend(self.pictures.iter().cloned());
        v
    }

    fn apply(&mut self, path: &str, now: u64) {
        if self.applying.is_some() {
            return;
        }
        match (&self.shell, self.shell_state) {
            (Some(s), ShellState::Available) => {
                s.set_wallpaper(path);
                self.applying = Some((path.to_string(), now));
            }
            _ => {
                self.error =
                    Some("The desktop shell isn't running, so the wallpaper can't be changed right now.".into())
            }
        }
    }

    /// Handles replies from the shell worker.
    fn poll_shell(&mut self, now: u64) {
        let Some(link) = &self.shell else { return };
        for reply in link.take() {
            match reply {
                ShellReply::Unavailable => {
                    self.shell_state = ShellState::Unavailable;
                    self.retry_at = now + 3_000_000_000;
                }
                ShellReply::Wallpapers { list: wallpapers, current } => {
                    self.shell_state = ShellState::Available;
                    if !wallpapers.is_empty() {
                        self.wallpapers = wallpapers;
                    }
                    self.current = current;
                }
                ShellReply::WallpaperSet { path, result } => {
                    self.applying = None;
                    match result {
                        Ok(()) => {
                            vrt::println!("wallpaper changed to {path}");
                            self.current = path;
                            self.applied_until = now + 3_000_000_000;
                        }
                        Err(e) => {
                            vrt::println!("cannot change the wallpaper to {path}: {e}");
                            self.error = Some(e.to_string());
                            if self.shell_state == ShellState::Available {
                                link.request_wallpapers();
                            }
                        }
                    }
                }
            }
        }
        if self.shell_state == ShellState::Unavailable && now >= self.retry_at {
            self.retry_at = u64::MAX;
            link.request_wallpapers();
        }
    }

    fn title_of(path: &str) -> String {
        if path == PROCEDURAL { "Light Ribbon".to_string() } else { title_of(path) }
    }

    fn launch_app(&self, id: &str) {
        if let Ok(ch) = vproto::connect(launcher::NAME) {
            let _ = launcher::Client::new(ch).launch_app(id.into(), Vec::new());
        }
    }

    // ---- drawing -----------------------------------------------------------

    fn draw_sidebar(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(r, t.surface);
        ui.canvas.fill_rect(Rect::new(r.right() - 1, r.y, 1, r.h), t.border);
        let badge = Rect::new(r.x + 20, r.y + 20, 36, 36);
        ui.canvas.fill_rounded_rect_gradient(badge, 9.0, Color::hex(0x98A2B8), Color::hex(0x56607A));
        ui.icon(badge, Icon::Settings, 20.0, Color::WHITE);
        ui.label(
            Rect::new(badge.right() + 12, badge.y, 140, 36),
            "Settings",
            Font::Bold,
            t.heading_size + 1.0,
            t.text,
            Align::Left,
        );
        let mut y = r.y + 84;
        for (section, title, icon, hint) in SECTIONS {
            let row = Rect::new(r.x + 10, y, r.w - 20, 54);
            let id = ui.id(title) ^ 0x5ec7;
            let resp = ui.interact(id, row);
            let active = self.section == section;
            if active {
                ui.canvas.fill_rounded_rect(row, t.radius_large, Color::rgba(255, 255, 255, 16));
                ui.canvas.fill_rounded_rect(Rect::new(row.x, row.y + 15, 3, row.h - 30), 1.5, t.accent);
            } else if resp.hovered {
                ui.canvas.fill_rounded_rect(row, t.radius_large, Color::rgba(255, 255, 255, 8));
            }
            if resp.hovered {
                ui.set_cursor(Cursor::Hand);
            }
            ui.icon(
                Rect::new(row.x + 14, row.y, 22, row.h),
                icon,
                19.0,
                if active { t.accent_hover } else { t.text_dim },
            );
            ui.label(
                Rect::new(row.x + 48, row.y + 8, row.w - 56, 22),
                title,
                if active { Font::Bold } else { Font::Regular },
                t.font_size,
                t.text,
                Align::Left,
            );
            ui.label(
                Rect::new(row.x + 48, row.y + 28, row.w - 56, 18),
                hint,
                Font::Regular,
                t.small_size,
                t.text_faint,
                Align::Left,
            );
            if resp.clicked && self.section != section {
                self.section = section;
                // Rows above were drawn with the old selection.
                ui.repaint();
            }
            y += 58;
        }
    }

    /// Page title and subtitle; returns the y below them.
    fn page_header(ui: &mut Ui, r: Rect, title: &str, subtitle: &str) -> i32 {
        let t = ui.theme().clone();
        ui.label(Rect::new(r.x, r.y, r.w, 36), title, Font::Bold, t.title_size, t.text, Align::Left);
        ui.label(Rect::new(r.x, r.y + 38, r.w, 22), subtitle, Font::Regular, t.font_size, t.text_dim, Align::Left);
        r.y + 78
    }

    fn section_heading(ui: &mut Ui, x: i32, y: i32, w: i32, text: &str) {
        let t = ui.theme().clone();
        ui.label(Rect::new(x, y, w, 24), text, Font::Bold, t.heading_size - 2.0, t.text, Align::Left);
    }

    /// Draws one wallpaper thumbnail (or its placeholder).
    fn draw_thumb(&mut self, ui: &mut Ui, r: Rect, path: &str, bg: Color) {
        let t = ui.theme().clone();
        if path == PROCEDURAL {
            draw_procedural(ui, r);
        } else if let Some(bmp) = self.thumbs.as_mut().and_then(|th| th.get(path)) {
            draw_cover(ui, r, bmp);
        } else {
            ui.canvas.fill_rect(r, t.control);
            ui.icon(r, Icon::Image, 26.0, t.text_faint);
        }
        mask_corners(ui, r, 8.0, bg);
    }

    /// A gallery of wallpaper choices; returns its height.
    fn gallery(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32, paths: &[String], first_index: usize) -> i32 {
        let t = ui.theme().clone();
        let gap = 16;
        let cols = ((w + gap) / (190 + gap)).max(2);
        let tile_w = (w - gap * (cols - 1)) / cols;
        let thumb_h = tile_w * 10 / 16;
        let tile_h = thumb_h + 34;
        let now = ui.now();
        for (i, path) in paths.iter().enumerate() {
            let (col, row) = (i as i32 % cols, i as i32 / cols);
            let tile = Rect::new(x + col * (tile_w + gap), y + row * (tile_h + gap), tile_w, tile_h);
            let thumb = Rect::new(tile.x, tile.y, tile.w, thumb_h);
            let id = ui.id(path) ^ 0x6a11;
            let resp = ui.interact(id, tile);
            let current = *path == self.current;
            let focused = self.cursor == Some(first_index + i);
            if resp.hovered {
                ui.set_cursor(Cursor::Hand);
                let hint = if current {
                    "Current wallpaper".to_string()
                } else {
                    format!("Use “{}” as the wallpaper", Self::title_of(path))
                };
                ui.tooltip(id, thumb, &hint);
            }
            let lift = if resp.hovered && !current { -2 } else { 0 };
            let thumb = thumb.translate(0, lift);
            ui.canvas.draw_shadow(
                thumb.translate(0, 3),
                8,
                10,
                Color::rgba(0, 0, 0, if resp.hovered { 120 } else { 70 }),
            );
            self.draw_thumb(ui, thumb, path, t.bg);
            if current {
                ui.canvas.stroke_rounded_rect(thumb.inflate(3), 11.0, 2.5, t.accent);
                let c = (thumb.right() - 16, thumb.y + 16);
                ui.canvas.fill_circle(c.0 as f32, c.1 as f32, 11.0, t.accent);
                ui.icon(Rect::new(c.0 - 9, c.1 - 9, 18, 18), Icon::Check, 13.0, Color::WHITE);
            } else if focused {
                ui.canvas.stroke_rounded_rect(thumb.inflate(3), 11.0, 1.5, t.border_strong);
            } else if resp.hovered {
                ui.canvas.stroke_rounded_rect(thumb, 8.0, 1.0, Color::rgba(255, 255, 255, 60));
            }
            if self.applying.as_ref().is_some_and(|(p, _)| p == path) {
                ui.canvas.fill_rounded_rect(thumb, 8.0, Color::rgba(0, 0, 0, 120));
                let (cx, cy) = thumb.center();
                ui.spinner(cx, cy, 14.0);
            }
            let label_color = if current { t.text } else { t.text_dim };
            let font = if current { Font::Bold } else { Font::Regular };
            ui.label(
                Rect::new(tile.x + 2, thumb.bottom() + 8 - lift, tile.w - 4, 20),
                &Self::title_of(path),
                font,
                t.font_size - 1.0,
                label_color,
                Align::Left,
            );
            if resp.clicked && !current {
                self.cursor = Some(first_index + i);
                self.apply(path, now);
            }
        }
        let rows = (paths.len() as i32 + cols - 1) / cols;
        rows * (tile_h + gap)
    }

    fn personalization(&mut self, ui: &mut Ui, r: Rect) -> i32 {
        let t = ui.theme().clone();
        let mut y = Self::page_header(
            ui,
            r,
            "Personalization",
            "Make the desktop yours. Click a picture to use it as the wallpaper.",
        );
        // Preview of the current wallpaper on a small "screen" (smaller in
        // narrow windows, leaving room for the text beside it).
        let screen_w = (r.w * 45 / 100).clamp(170, 284);
        let screen_h = screen_w * 10 / 16;
        let card = Rect::new(r.x, y, r.w, (screen_h + 36).max(204));
        ui.card(card);
        let screen = Rect::new(card.x + 18, card.y + (card.h - screen_h) / 2, screen_w, screen_h);
        let current = self.current.clone();
        if current.is_empty() {
            ui.canvas.fill_rounded_rect(screen, 8.0, t.control);
            ui.icon(screen, Icon::Wallpaper, 30.0, t.text_faint);
        } else {
            self.draw_thumb(ui, screen, &current, t.surface);
            // A tiny taskbar and window, to make it read as a desktop.
            let bar = Rect::new(screen.x, screen.bottom() - 14, screen.w, 14);
            ui.canvas.fill_rect(bar, Color::rgba(16, 16, 22, 200));
            for k in 0..5 {
                let ic = Rect::new(screen.x + screen.w / 2 - 34 + k * 14, bar.y + 3, 9, 8);
                ui.canvas.fill_rounded_rect(ic, 2.0, Color::rgba(255, 255, 255, if k == 1 { 200 } else { 110 }));
            }
            let win =
                Rect::new(screen.x + screen.w / 6, screen.y + screen.h / 6, screen.w * 42 / 100, screen.h * 43 / 100);
            ui.canvas.draw_shadow(win.translate(0, 3), 4, 8, Color::rgba(0, 0, 0, 100));
            ui.canvas.fill_rounded_rect(win, 4.0, Color::rgba(30, 30, 36, 235));
            ui.canvas.fill_rect(Rect::new(win.x, win.y + 10, win.w, 1), Color::rgba(255, 255, 255, 25));
            mask_corners(ui, screen, 8.0, t.surface);
        }
        ui.canvas.stroke_rounded_rect(screen.inflate(1), 9.0, 1.0, t.border_strong);
        let tx = screen.right() + 24;
        let tw = card.right() - tx - 18;
        ui.label(
            Rect::new(tx, card.y + 22, tw, 18),
            "Current wallpaper",
            Font::Regular,
            t.small_size,
            t.text_faint,
            Align::Left,
        );
        let name = if current.is_empty() { "Not available".to_string() } else { Self::title_of(&current) };
        ui.label(Rect::new(tx, card.y + 42, tw, 30), &name, Font::Bold, t.heading_size + 2.0, t.text, Align::Left);
        let location = if current == PROCEDURAL {
            "Generated by the desktop".to_string()
        } else if current.is_empty() {
            "Unknown while the shell is offline".to_string()
        } else {
            current.clone()
        };
        ui.label(Rect::new(tx, card.y + 74, tw, 20), &location, Font::Regular, t.small_size, t.text_dim, Align::Left);
        let now = ui.now();
        let status_r = Rect::new(tx, card.y + 110, tw, 24);
        if let Some((path, since)) = self.applying.clone() {
            ui.spinner(status_r.x + 9, status_r.y + 12, 8.0);
            let msg = if now.saturating_sub(since) > 8_000_000_000 {
                format!("Still applying “{}”… the desktop is busy", Self::title_of(&path))
            } else {
                format!("Applying “{}”…", Self::title_of(&path))
            };
            ui.label(status_r.inset(26, 0, 0, 0), &msg, Font::Regular, t.font_size - 1.0, t.text_dim, Align::Left);
        } else if now < self.applied_until {
            ui.icon(Rect::new(status_r.x, status_r.y, 18, status_r.h), Icon::Check, 16.0, t.success);
            ui.label(
                status_r.inset(26, 0, 0, 0),
                "Wallpaper changed",
                Font::Regular,
                t.font_size - 1.0,
                t.success,
                Align::Left,
            );
            ui.repaint_at(self.applied_until);
        }
        match self.shell_state {
            ShellState::Checking => {
                ui.label(
                    Rect::new(tx, card.bottom() - 46, tw, 20),
                    "Connecting to the desktop…",
                    Font::Regular,
                    t.small_size,
                    t.text_faint,
                    Align::Left,
                );
            }
            ShellState::Unavailable => {
                // Size the banner to its (wrapped) message.
                let msg = "The desktop shell isn't running, so the wallpaper can't be changed right now.";
                let text_w = (tw - 38 - 104).max(80);
                let font = ui.ctx.font(Font::Regular);
                let m = ui.ctx.text.metrics(font, t.small_size);
                let lines = ui.ctx.text.wrap(font, t.small_size, msg, text_w as f32).len().max(1);
                let text_h = (lines as f32 * m.line_height) as i32;
                let bh = (text_h + 18).max(46);
                let banner = Rect::new(tx, card.bottom() - 16 - bh, tw, bh);
                ui.canvas.fill_rounded_rect(banner, t.radius, t.warning.with_alpha(28));
                ui.canvas.stroke_rounded_rect(banner, t.radius, 1.0, t.warning.with_alpha(90));
                ui.icon(Rect::new(banner.x + 10, banner.y + 10, 20, 20), Icon::Warning, 17.0, t.warning);
                ui.paragraph(
                    Rect::new(banner.x + 38, banner.y + (bh - text_h) / 2, text_w, text_h + 4),
                    msg,
                    t.small_size,
                    t.text,
                );
                let btn = Rect::new(banner.right() - 96, banner.y + (bh - 32) / 2, 86, 32);
                if ui.button(btn, "Try again") {
                    if let Some(s) = &self.shell {
                        s.request_wallpapers();
                    }
                    self.shell_state = ShellState::Checking;
                }
            }
            ShellState::Available => {
                ui.label(
                    Rect::new(tx, card.bottom() - 46, tw, 20),
                    "Changes apply right away.",
                    Font::Regular,
                    t.small_size,
                    t.text_faint,
                    Align::Left,
                );
            }
        }
        y = card.bottom() + 30;
        Self::section_heading(ui, r.x, y, r.w, "Wallpapers");
        y += 34;
        let walls = self.wallpapers.clone();
        if walls.is_empty() {
            ui.label(
                Rect::new(r.x, y, r.w, 22),
                "No wallpapers are installed in /system/wallpapers.",
                Font::Regular,
                t.font_size,
                t.text_dim,
                Align::Left,
            );
            y += 40;
        } else {
            y += self.gallery(ui, r.x, y, r.w, &walls, 0) + 10;
        }
        Self::section_heading(ui, r.x, y, r.w, "Your pictures");
        ui.label(
            Rect::new(r.x + 140, y, r.w - 140, 24),
            "From your Pictures folder",
            Font::Regular,
            t.small_size,
            t.text_faint,
            Align::Right,
        );
        y += 34;
        let pics = self.pictures.clone();
        if pics.is_empty() {
            let msg = "Pictures you save in your Pictures folder (PNG, JPEG, BMP or QOI) appear here.";
            ui.label(Rect::new(r.x, y, r.w, 22), msg, Font::Regular, t.font_size, t.text_dim, Align::Left);
            y += 40;
        } else {
            y += self.gallery(ui, r.x, y, r.w, &pics, walls.len());
        }
        y - r.y + 10
    }

    fn info_row(ui: &mut Ui, x: i32, y: i32, w: i32, icon: Icon, label: &str, value: &str) {
        let t = ui.theme().clone();
        ui.icon(Rect::new(x, y, 22, 32), icon, 17.0, t.accent);
        ui.label(Rect::new(x + 34, y, 150, 32), label, Font::Regular, t.font_size, t.text_dim, Align::Left);
        ui.label(Rect::new(x + 190, y, w - 190, 32), value, Font::Bold, t.font_size, t.text, Align::Left);
    }

    fn display(&mut self, ui: &mut Ui, r: Rect) -> i32 {
        let t = ui.theme().clone();
        let mut y = Self::page_header(ui, r, "Display", "Your screen and the space windows can use.");
        let Some(s) = self.screen else {
            ui.label(
                Rect::new(r.x, y, r.w, 24),
                "Screen information is not available.",
                Font::Regular,
                t.font_size,
                t.text_dim,
                Align::Left,
            );
            return y + 40 - r.y;
        };
        // Diagram: the screen with its work area and panels; the details
        // go beside it, or below it in a narrow window.
        let narrow = r.w < 600;
        let max_w = if narrow { (r.w - 48).min(320) } else { (r.w - 340).clamp(160, 320) } as f32;
        let scale = (max_w / s.width.max(1) as f32).min(190.0 / s.height.max(1) as f32);
        let (sw, sh) = ((s.width as f32 * scale) as i32, (s.height as f32 * scale) as i32);
        let card_h = if narrow { sh + 48 + 5 * 38 + 10 } else { 250 };
        let card = Rect::new(r.x, y, r.w, card_h);
        ui.card(card);
        let scr = if narrow {
            Rect::new(card.x + 24, card.y + 24, sw, sh)
        } else {
            Rect::new(card.x + 24, card.y + (card.h - sh) / 2, sw, sh)
        };
        let current = self.current.clone();
        if current.is_empty() {
            ui.canvas.fill_rect(scr, Color::hex(0x1E2550));
        } else {
            self.draw_thumb(ui, scr, &current, t.surface);
        }
        let wa = s.work_area;
        let work = Rect::new(
            scr.x + (wa.x as f32 * scale) as i32,
            scr.y + (wa.y as f32 * scale) as i32,
            (wa.w as f32 * scale) as i32,
            (wa.h as f32 * scale) as i32,
        );
        // Shade the panel area (outside the work area).
        if work.bottom() < scr.bottom() {
            ui.canvas.fill_rect(
                Rect::new(scr.x, work.bottom(), scr.w, scr.bottom() - work.bottom()),
                Color::rgba(10, 10, 14, 220),
            );
        }
        if work.y > scr.y {
            ui.canvas.fill_rect(Rect::new(scr.x, scr.y, scr.w, work.y - scr.y), Color::rgba(10, 10, 14, 220));
        }
        ui.canvas.stroke_rounded_rect(work.inflate(-1), 2.0, 1.5, t.accent_hover);
        ui.canvas.stroke_rounded_rect(scr.inflate(2), 5.0, 2.0, t.border_strong);
        let (tx, mut ry) = if narrow { (card.x + 24, scr.bottom() + 20) } else { (scr.right() + 30, card.y + 28) };
        let tw = card.right() - tx - 20;
        let ratio = gcd(s.width, s.height).max(1);
        let (rw, rh) = match (s.width / ratio, s.height / ratio) {
            (8, 5) => (16, 10),
            other => other,
        };
        let panel = s.height.saturating_sub(wa.h) + s.width.saturating_sub(wa.w);
        let rows: Vec<(&str, String)> = vec![
            ("Resolution", format!("{} × {}", s.width, s.height)),
            ("Aspect ratio", format!("{rw}:{rh}")),
            ("Work area", format!("{} × {}", wa.w, wa.h)),
            ("Taskbar", if panel > 0 { format!("{panel} px") } else { "hidden".to_string() }),
            ("Colour", "32-bit true colour".to_string()),
        ];
        for (k, v) in rows {
            ui.label(Rect::new(tx, ry, 120, 30), k, Font::Regular, t.font_size, t.text_dim, Align::Left);
            ui.label(Rect::new(tx + 120, ry, tw - 120, 30), &v, Font::Bold, t.font_size, t.text, Align::Left);
            ry += 38;
        }
        y = card.bottom() + 20;
        let note_r = Rect::new(r.x, y, r.w, 60);
        ui.icon(Rect::new(note_r.x, note_r.y, 20, 24), Icon::Info, 16.0, t.text_faint);
        ui.paragraph(
            Rect::new(note_r.x + 30, note_r.y + 3, note_r.w - 30, 60),
            "The screen mode is chosen by the boot loader (resolution= in \\VINDOWS\\BOOT.CFG, or --resolution when building). Maximised windows fill the work area.",
            t.font_size - 1.0,
            t.text_dim,
        );
        y + 70 - r.y
    }

    fn system(&mut self, ui: &mut Ui, r: Rect) -> i32 {
        let mut y = Self::page_header(ui, r, "System", "What this computer is running on. Updated every second.");
        let i = self.info;
        let card = Rect::new(r.x, y, r.w, 6 * 38 + 70);
        ui.card(card);
        let x = card.x + 22;
        let w = card.w - 44;
        let mut ry = card.y + 16;
        let used = i.total_memory.saturating_sub(i.free_memory);
        let rows: [(Icon, &str, String); 6] = [
            (Icon::Monitor, "Device name", "vindows".to_string()),
            (Icon::Info, "Version", cstr(&i.version)),
            (Icon::Cpu, "Processor", cstr(&i.cpu_brand)),
            (Icon::Grid, "Cores", format!("{} logical processors", i.cpu_count)),
            (Icon::Clock, "Up time", format_uptime(i.uptime_ns)),
            (Icon::List, "Processes", format!("{} processes, {} threads", i.process_count, i.thread_count)),
        ];
        for (icon, label, value) in rows.iter() {
            Self::info_row(ui, x, ry, w, *icon, label, value);
            ry += 38;
        }
        Self::info_row(
            ui,
            x,
            ry,
            w,
            Icon::Chart,
            "Memory",
            &format!("{} in use of {} ({} free)", mib(used), mib(i.total_memory), mib(i.free_memory)),
        );
        let frac = if i.total_memory > 0 { used as f32 / i.total_memory as f32 } else { 0.0 };
        ui.progress(Rect::new(x + 190, ry + 34, (w - 190).min(360), 6), frac);
        y = card.bottom() + 20;
        let b1 = Rect::new(r.x, y, 190, 38);
        if ui.button_full(b1, Some(Icon::Chart), "Task Manager", ButtonKind::Secondary) {
            self.launch_app("taskmgr");
        }
        let b2 = Rect::new(b1.right() + 12, y, 160, 38);
        if ui.button_full(b2, Some(Icon::Terminal), "Terminal", ButtonKind::Secondary) {
            self.launch_app("terminal");
        }
        y + 50 - r.y
    }

    fn about(&mut self, ui: &mut Ui, r: Rect) -> i32 {
        let t = ui.theme().clone();
        let mut y = r.y;
        // The Vindows logo: the ring.
        vui::draw_logo(&mut ui.canvas, Rect::new(r.x, y + 4, 64, 64));
        ui.label(Rect::new(r.x + 88, y, r.w - 88, 40), "Vindows", Font::Bold, t.title_size + 4.0, t.text, Align::Left);
        let version = cstr(&self.info.version);
        ui.label(
            Rect::new(r.x + 90, y + 40, r.w - 90, 22),
            &format!("{version} · microkernel edition"),
            Font::Regular,
            t.font_size,
            t.text_dim,
            Align::Left,
        );
        y += 92;
        let blurb = "Vindows is an x86-64 operating system written from scratch in Rust: a capability-based microkernel, with drivers, file systems, the window system and every application running as isolated user-space processes.";
        y += ui.paragraph(Rect::new(r.x, y, r.w, 80), blurb, t.font_size, t.text_dim) + 22;
        let card = Rect::new(r.x, y, r.w, 168);
        ui.card(card);
        let x = card.x + 22;
        let w = card.w - 44;
        let mut ry = card.y + 14;
        let rows: [(&str, &str); 4] = [
            ("Vindows", "MIT License · © Vahid Mohammadi"),
            ("Inter", "SIL Open Font License 1.1 · © The Inter Project Authors"),
            ("JetBrains Mono", "SIL Open Font License 1.1 · © The JetBrains Mono Project Authors"),
            ("Lato", "SIL Open Font License 1.1 · © tyPoland Łukasz Dziedzic"),
        ];
        for (name, lic) in rows {
            ui.label(Rect::new(x, ry, 150, 34), name, Font::Bold, t.font_size, t.text, Align::Left);
            ui.label(
                Rect::new(x + 150, ry, w - 150, 34),
                lic,
                Font::Regular,
                t.font_size - 1.0,
                t.text_dim,
                Align::Left,
            );
            ry += 36;
        }
        y = card.bottom() + 18;
        let mut show = self.show_licence;
        ui.toggle(Rect::new(r.x, y, 44, 28), "licence-toggle", &mut show);
        if show && self.licence.is_none() {
            let text = self.fs.read("/system/fonts/OFL.txt").ok().map(|d| String::from_utf8_lossy(&d).into_owned());
            self.licence = Some(text.unwrap_or_else(|| "The licence text could not be read.".into()));
        }
        self.show_licence = show;
        ui.label(
            Rect::new(r.x + 54, y, 300, 28),
            "Show the font licence (OFL 1.1)",
            Font::Regular,
            t.font_size,
            t.text,
            Align::Left,
        );
        y += 42;
        if self.show_licence
            && let Some(text) = self.licence.clone()
        {
            let font = ui.ctx.font(Font::Mono);
            let size = t.small_size;
            let m = ui.ctx.text.metrics(font, size);
            let lines = ui.ctx.text.wrap(font, size, &text, (r.w - 32) as f32);
            let h = (lines.len() as f32 * m.line_height) as i32 + 28;
            let box_r = Rect::new(r.x, y, r.w, h);
            ui.canvas.fill_rounded_rect(box_r, t.radius, Color::hex(0x151519));
            ui.canvas.stroke_rounded_rect(box_r, t.radius, 1.0, t.border);
            let mut ly = box_r.y as f32 + 14.0 + m.ascent;
            for range in lines {
                let line = text[range].trim_end();
                ui.ctx.text.draw(&mut ui.canvas, font, size, (box_r.x + 16) as f32, ly.floor(), line, t.text_dim);
                ly += m.line_height;
            }
            y += h + 10;
        }
        y - r.y
    }

    fn handle_keys(&mut self, ui: &mut Ui) {
        let now = ui.now();
        let n = self.choices().len();
        for k in ui.input.keys.clone() {
            let idx = SECTIONS.iter().position(|s| s.0 == self.section).unwrap_or(0);
            let ctrl = k.modifiers & modifiers::CTRL != 0;
            match k.code {
                keys::UP | keys::PAGEUP if idx > 0 => self.section = SECTIONS[idx - 1].0,
                keys::DOWN | keys::PAGEDOWN if idx + 1 < SECTIONS.len() => self.section = SECTIONS[idx + 1].0,
                keys::TAB if ctrl => self.section = SECTIONS[(idx + 1) % SECTIONS.len()].0,
                keys::LEFT if self.section == Section::Personalization && n > 0 => {
                    self.cursor = Some(self.cursor.map_or(0, |c| c.saturating_sub(1)));
                }
                keys::RIGHT if self.section == Section::Personalization && n > 0 => {
                    self.cursor = Some(self.cursor.map_or(0, |c| (c + 1).min(n - 1)));
                }
                keys::ENTER | keys::KPENTER | keys::SPACE if self.section == Section::Personalization => {
                    if let Some(p) = self.cursor.and_then(|c| self.choices().get(c).cloned()) {
                        self.apply(&p, now);
                    }
                }
                keys::F5 => {
                    self.pictures = images_in(&self.fs, PICTURES_DIR);
                    if let Some(s) = &self.shell {
                        s.request_wallpapers();
                    }
                }
                _ => {}
            }
        }
    }
}

impl App for Settings {
    fn update(&mut self, ui: &mut Ui) {
        let now = ui.now();
        self.poll_shell(now);
        if let Some(th) = &mut self.thumbs {
            th.collect();
        }
        if now >= self.info_at {
            if let Ok(i) = vrt::object::system_info() {
                self.info = i;
            }
            self.screen = ui.ctx.display.screen_info().ok();
            self.info_at = now + 1_000_000_000;
            if self.section == Section::Personalization {
                self.pictures = images_in(&self.fs, PICTURES_DIR);
            }
        }
        if matches!(self.section, Section::System | Section::Display) {
            ui.repaint_at(self.info_at);
        }
        if self.shell_state == ShellState::Unavailable && self.retry_at != u64::MAX {
            ui.repaint_at(self.retry_at);
        }
        if self.section == Section::Network {
            let next = self.net.poll(now);
            ui.repaint_at(next);
        }
        if self.section == Section::Agent {
            let next = self.agent.get_or_insert_with(agent::AgentPage::new).poll(now);
            ui.repaint_at(next);
        }
        // Keys go to the error message while it is shown.
        let error_at_start = self.error.is_some();
        if !error_at_start {
            self.handle_keys(ui);
        }
        let full = ui.rect();
        let (side, content) = full.split_left(SIDEBAR_W);
        self.draw_sidebar(ui, side);
        let section = self.section;
        let id = match section {
            Section::Personalization => "content-p",
            Section::Agent => "content-g",
            Section::Network => "content-n",
            Section::Display => "content-d",
            Section::System => "content-s",
            Section::About => "content-a",
        };
        let h = self.content_h;
        let used = ui.scroll_area(content, id, h, |ui, off| {
            let r = Rect::new(content.x + 36, content.y + 30 - off, content.w - 72, content.h);
            let used = match section {
                Section::Personalization => self.personalization(ui, r),
                Section::Agent => self.agent.as_mut().map_or(0, |p| p.draw(ui, r)),
                Section::Network => self.net.draw(ui, r),
                Section::Display => self.display(ui, r),
                Section::System => self.system(ui, r),
                Section::About => self.about(ui, r),
            };
            used + 60
        });
        if used != self.content_h {
            self.content_h = used;
            ui.repaint();
        }
        if section == Section::Agent
            && let Some(p) = &mut self.agent
        {
            p.overlay(ui);
        }
        // A message raised this frame is shown from the next one, so the key
        // that caused it does not also dismiss it.
        if let Some(msg) = self.error.clone().filter(|_| error_at_start) {
            if ui.message_box("Couldn't change the wallpaper", &msg, &["OK"]).is_some() {
                self.error = None;
            }
        } else if self.error.is_some() {
            ui.repaint();
        }
    }

    fn agent_info(&self) -> Option<vui::agent::AppAgentInfo> {
        Some(control::info())
    }

    fn agent_state(&self) -> vui::agent::Value {
        control::state(self)
    }

    fn agent_invoke(&mut self, action: &str, args: &vui::agent::Value) -> Result<vui::agent::Value, String> {
        Settings::agent_invoke(self, action, args)
    }

    fn wait_handles(&self) -> Vec<(vabi::RawHandle, u32)> {
        let mut v = Vec::new();
        if let Some(s) = &self.shell {
            v.push((s.event_handle(), vabi::signals::SIGNALED));
        }
        if let Some(t) = &self.thumbs {
            v.push((t.event_handle(), vabi::signals::SIGNALED));
        }
        if let Some(h) = self.net.wait_handle() {
            v.push((h, vabi::signals::READABLE | vabi::signals::PEER_CLOSED));
        }
        if let Some(h) = self.agent.as_ref().and_then(|p| p.wait_handle()) {
            v.push((h, vabi::signals::SIGNALED));
        }
        v
    }
}

fn main() -> i32 {
    let section = match vrt::env::args().get(1).map(|s| s.to_ascii_lowercase()) {
        Some(s) if s.starts_with("agent") || s.starts_with("voice") => Section::Agent,
        Some(s) if s.starts_with("disp") => Section::Display,
        Some(s) if s.starts_with("net") || s.starts_with("wi") => Section::Network,
        Some(s) if s.starts_with("sys") => Section::System,
        Some(s) if s.starts_with("about") => Section::About,
        _ => Section::Personalization,
    };
    let app = Settings::new(section);
    let mut spec = WindowSpec::new("Settings", 920, 620);
    spec.app_id = "settings".into();
    spec.min_width = 720;
    spec.min_height = 460;
    vui::run(spec, app)
}
