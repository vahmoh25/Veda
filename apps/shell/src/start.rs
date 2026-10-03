//! The start menu: search, pinned apps, the full app list and power options.

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vproto::init::{AppInfo, power};
use vproto::input::keys;
use vui::{ButtonKind, Font, Icon, MenuItem, Ui};

use crate::{Action, Model, apps, chrome, taskbar};

const WIDTH: i32 = 600;
const HEIGHT: i32 = 620;
const PAD: i32 = 26;
const FOOTER_H: i32 = 64;

/// Where the menu appears: centred above the taskbar.
pub fn placement(screen: Rect) -> Rect {
    let h = HEIGHT.min(screen.h - taskbar::HEIGHT - 24);
    let w = WIDTH.min(screen.w - 24);
    Rect::new((screen.w - w) / 2, screen.h - taskbar::HEIGHT - 12 - h, w, h)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    AllApps,
}

pub struct StartMenu {
    origin: (i32, i32),
    query: String,
    view: View,
    /// Highlighted search result.
    selected: usize,
}

impl StartMenu {
    pub fn new(origin: (i32, i32)) -> StartMenu {
        StartMenu { origin, query: String::new(), view: View::Home, selected: 0 }
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let (w, h) = (ui.width, ui.height);
        let t = ui.theme().clone();
        chrome::popup_background(&mut ui.canvas, &m.wallpaper.blurred, self.origin);

        // Typing always goes to the search box.
        if ui.input.key(keys::ESC) {
            if self.query.is_empty() {
                ui.close_window();
            } else {
                self.query.clear();
            }
        }
        ui.focus_text_input("search");
        let sr = Rect::new(PAD, PAD, w - 2 * PAD, 40);
        let resp = ui.text_input(sr, "search", &mut self.query, "Search for apps");
        ui.icon(Rect::new(sr.right() - 38, sr.y, 32, sr.h), Icon::Search, 16.0, t.text_faint);
        if resp.changed {
            self.selected = 0;
        }

        let body = Rect::new(PAD, sr.bottom() + 20, w - 2 * PAD, h - FOOTER_H - sr.bottom() - 28);
        if !self.query.trim().is_empty() {
            self.search(ui, m, body, resp.submitted);
        } else if self.view == View::AllApps {
            self.all_apps(ui, m, body);
        } else {
            self.home(ui, m, body);
        }
        self.footer(ui, m, Rect::new(0, h - FOOTER_H, w, FOOTER_H));
        chrome::popup_frame(&mut ui.canvas);
    }

    fn section_title(ui: &mut Ui, r: Rect, text: &str) {
        let c = ui.theme().text;
        ui.label(r, text, Font::Bold, 14.0, c, Align::Left);
    }

    /// Pinned apps in a grid, the rest below as "More apps".
    fn home(&mut self, ui: &mut Ui, m: &mut Model, body: Rect) {
        let t = ui.theme().clone();
        Self::section_title(ui, Rect::new(body.x + 4, body.y, 200, 24), "Pinned");
        if ui.button_full(
            Rect::new(body.right() - 104, body.y - 3, 104, 30),
            Some(Icon::ChevronRight),
            "All apps",
            ButtonKind::Ghost,
        ) {
            self.view = View::AllApps;
        }
        let pinned: Vec<&AppInfo> = m.apps.iter().filter(|a| a.pinned).collect();
        let cols = 6;
        let cell_w = body.w / cols;
        let cell_h = 86;
        let grid_y = body.y + 36;
        let mut launch = None;
        for (i, app) in pinned.iter().enumerate().take(18) {
            let (col, row) = (i as i32 % cols, i as i32 / cols);
            let r = Rect::new(body.x + col * cell_w, grid_y + row * cell_h, cell_w, cell_h - 4);
            if Self::app_tile(ui, r, app) {
                launch = Some(app.id.clone());
            }
        }
        let rows = (pinned.len().min(18) as i32 + cols - 1) / cols;
        let more: Vec<&AppInfo> = m.apps.iter().filter(|a| !a.pinned).collect();
        let my = grid_y + rows * cell_h + 14;
        if !more.is_empty() && my + 80 < body.bottom() {
            Self::section_title(ui, Rect::new(body.x + 4, my, 200, 24), "More apps");
            let col_w = body.w / 2;
            for (i, app) in more.iter().enumerate() {
                let r = Rect::new(body.x + (i as i32 % 2) * col_w, my + 32 + (i as i32 / 2) * 56, col_w - 6, 52);
                if r.bottom() > body.bottom() {
                    break;
                }
                if Self::app_row(ui, r, app, false, &t) {
                    launch = Some(app.id.clone());
                }
            }
        }
        if let Some(id) = launch {
            m.push(Action::Launch(id, Vec::new()));
        }
    }

    /// Every app, alphabetically, grouped by initial.
    fn all_apps(&mut self, ui: &mut Ui, m: &mut Model, body: Rect) {
        let t = ui.theme().clone();
        Self::section_title(ui, Rect::new(body.x + 4, body.y, 200, 24), "All apps");
        if ui.button_full(
            Rect::new(body.right() - 92, body.y - 3, 92, 30),
            Some(Icon::ChevronLeft),
            "Back",
            ButtonKind::Ghost,
        ) {
            self.view = View::Home;
        }
        enum Row<'a> {
            Initial(char),
            App(&'a AppInfo),
        }
        let mut sorted: Vec<&AppInfo> = m.apps.iter().collect();
        sorted.sort_by_key(|a| a.name.to_lowercase());
        let mut rows: Vec<Row> = Vec::new();
        for a in &sorted {
            let initial = a.name.chars().next().unwrap_or('#').to_ascii_uppercase();
            if !matches!(rows.iter().rev().find(|r| matches!(r, Row::Initial(_))), Some(Row::Initial(c)) if *c == initial)
            {
                rows.push(Row::Initial(initial));
            }
            rows.push(Row::App(a));
        }
        let height = |r: &Row| if matches!(r, Row::App(_)) { 52 } else { 30 };
        let list = Rect::new(body.x, body.y + 34, body.w, body.h - 34);
        let content_h: i32 = rows.iter().map(height).sum();
        let mut launch = None;
        ui.scroll_area(list, "all-apps", content_h, |ui, offset| {
            let mut y = list.y - offset;
            for row in &rows {
                let r = Rect::new(list.x, y, list.w - 14, height(row));
                y += r.h;
                if r.bottom() < list.y || r.y > list.bottom() {
                    continue;
                }
                match row {
                    Row::Initial(c) => {
                        let mut buf = [0u8; 4];
                        let letter = c.encode_utf8(&mut buf);
                        ui.label(Rect::new(r.x + 10, r.y + 6, 40, 22), letter, Font::Bold, 13.0, t.accent, Align::Left);
                    }
                    Row::App(app) => {
                        if Self::app_row(ui, r, app, false, &t) {
                            launch = Some(app.id.clone());
                        }
                    }
                }
            }
        });
        if let Some(id) = launch {
            m.push(Action::Launch(id, Vec::new()));
        }
    }

    /// Apps matching the query; Enter opens the highlighted one.
    fn search(&mut self, ui: &mut Ui, m: &mut Model, body: Rect, submitted: bool) {
        let t = ui.theme().clone();
        let mut results: Vec<&AppInfo> = m.apps.iter().filter(|a| apps::matches(a, &self.query)).collect();
        results.sort_by_key(|a| (apps::rank(a, &self.query), a.name.to_lowercase()));
        if results.is_empty() {
            let msg = alloc::format!("No apps match \u{201c}{}\u{201d}", self.query.trim());
            ui.label(Rect::new(body.x, body.y + 40, body.w, 24), &msg, Font::Regular, 14.0, t.text_dim, Align::Center);
            return;
        }
        for k in &ui.input.keys {
            match k.code {
                keys::DOWN => self.selected = (self.selected + 1).min(results.len() - 1),
                keys::UP => self.selected = self.selected.saturating_sub(1),
                _ => {}
            }
        }
        self.selected = self.selected.min(results.len() - 1);
        let mut launch = submitted.then(|| results[self.selected].id.clone());

        // The best match gets a large card.
        Self::section_title(ui, Rect::new(body.x + 4, body.y, 200, 24), "Best match");
        let best = results[0];
        let card = Rect::new(body.x, body.y + 32, body.w, 92);
        let resp = ui.interact(ui.id("best"), card);
        let bg = if self.selected == 0 || resp.hovered {
            Color::rgba(255, 255, 255, 22)
        } else {
            Color::rgba(255, 255, 255, 10)
        };
        ui.canvas.fill_rounded_rect(card, 8.0, bg);
        if self.selected == 0 {
            ui.canvas.stroke_rounded_rect(card, 8.0, 1.0, t.accent.with_alpha(160));
        }
        apps::draw_tile(&mut ui.canvas, Rect::new(card.x + 18, card.y + 18, 56, 56), &best.id, apps::icon_for(best));
        ui.label(
            Rect::new(card.x + 92, card.y + 18, card.w - 200, 26),
            &best.name,
            Font::Bold,
            17.0,
            t.text,
            Align::Left,
        );
        let desc = if best.description.is_empty() { &best.category } else { &best.description };
        let font = ui.ctx.font(Font::Regular);
        let desc = ui.ctx.text.ellipsize(font, 13.0, desc, (card.w - 210) as f32);
        ui.label(
            Rect::new(card.x + 92, card.y + 46, card.w - 200, 22),
            &desc,
            Font::Regular,
            13.0,
            t.text_dim,
            Align::Left,
        );
        if ui.button_full(Rect::new(card.right() - 98, card.y + 28, 82, 34), None, "Open", ButtonKind::Primary)
            || resp.clicked
        {
            launch = Some(best.id.clone());
        }

        if results.len() > 1 {
            let y = card.bottom() + 18;
            Self::section_title(ui, Rect::new(body.x + 4, y, 200, 24), "Apps");
            for (i, app) in results.iter().enumerate().skip(1) {
                let r = Rect::new(body.x, y + 30 + (i as i32 - 1) * 54, body.w, 52);
                if r.bottom() > body.bottom() {
                    break;
                }
                if Self::app_row(ui, r, app, self.selected == i, &t) {
                    launch = Some(app.id.clone());
                }
            }
        }
        if let Some(id) = launch {
            m.push(Action::Launch(id, Vec::new()));
        }
    }

    /// A grid tile: icon above the name. Returns `true` when clicked.
    fn app_tile(ui: &mut Ui, r: Rect, app: &AppInfo) -> bool {
        let id = ui.id(&app.id) ^ 0x7111e;
        let resp = ui.interact(id, r);
        if resp.hovered {
            let a = if resp.held { 14 } else { 24 };
            ui.canvas.fill_rounded_rect(r, 8.0, Color::rgba(255, 255, 255, a));
            ui.set_cursor(vui::Cursor::Hand);
        }
        let s = if resp.held { 40 } else { 44 };
        let tile = Rect::new(r.x + (r.w - s) / 2, r.y + 10 + (44 - s) / 2, s, s);
        apps::draw_tile(&mut ui.canvas, tile, &app.id, apps::icon_for(app));
        let font = ui.ctx.font(Font::Regular);
        let name = ui.ctx.text.ellipsize(font, 12.5, &app.name, (r.w - 8) as f32);
        let c = ui.theme().text;
        ui.label(Rect::new(r.x + 2, r.y + 60, r.w - 4, 18), &name, Font::Regular, 12.5, c, Align::Center);
        resp.clicked
    }

    /// A list row: icon, name and description. Returns `true` when clicked.
    fn app_row(ui: &mut Ui, r: Rect, app: &AppInfo, highlighted: bool, t: &vui::Theme) -> bool {
        let id = ui.id(&app.id) ^ 0x2077 ^ (r.x as u64) << 40;
        let resp = ui.interact(id, r);
        if resp.hovered || highlighted {
            let a = if resp.held { 14 } else { 22 };
            ui.canvas.fill_rounded_rect(r, 8.0, Color::rgba(255, 255, 255, a));
        }
        if resp.hovered {
            ui.set_cursor(vui::Cursor::Hand);
        }
        apps::draw_tile(
            &mut ui.canvas,
            Rect::new(r.x + 10, r.y + (r.h - 34) / 2, 34, 34),
            &app.id,
            apps::icon_for(app),
        );
        let text_w = r.w - 66;
        ui.label(Rect::new(r.x + 56, r.y + 7, text_w, 20), &app.name, Font::Bold, 13.5, t.text, Align::Left);
        let desc = if app.description.is_empty() { &app.category } else { &app.description };
        let font = ui.ctx.font(Font::Regular);
        let desc = ui.ctx.text.ellipsize(font, 12.0, desc, text_w as f32);
        ui.label(Rect::new(r.x + 56, r.y + 26, text_w, 18), &desc, Font::Regular, 12.0, t.text_dim, Align::Left);
        resp.clicked
    }

    /// User, settings and power.
    fn footer(&mut self, ui: &mut Ui, m: &mut Model, r: Rect) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(r, Color::rgba(0, 0, 0, 46));
        ui.canvas.fill_rect(Rect::new(r.x, r.y, r.w, 1), Color::rgba(255, 255, 255, 18));
        let cy = r.y + r.h / 2;
        let avatar = Rect::new(r.x + PAD, cy - 17, 34, 34);
        ui.canvas.fill_rounded_rect_gradient(avatar, 17.0, t.accent, t.accent2);
        ui.icon(avatar, Icon::User, 18.0, Color::WHITE);
        ui.label(Rect::new(avatar.right() + 12, cy - 11, 200, 22), "User", Font::Bold, 14.0, t.text, Align::Left);

        let power_r = Rect::new(r.right() - PAD - 38, cy - 19, 38, 38);
        if ui.icon_button(power_r, Icon::Power, "Power") {
            ui.open_context_menu("power", power_r.x - 120, power_r.y - 8);
        }
        let settings_r = Rect::new(power_r.x - 46, cy - 19, 38, 38);
        if ui.icon_button(settings_r, Icon::Settings, "Settings") && m.app("settings").is_some() {
            m.push(Action::Launch("settings".into(), Vec::new()));
        }
        let menu = [MenuItem::new("Restart"), MenuItem::new("Shut down")];
        match ui.context_menu("power", &menu) {
            Some(0) => m.push(Action::Power(power::REBOOT)),
            Some(1) => m.push(Action::Power(power::SHUTDOWN)),
            _ => {}
        }
    }
}
