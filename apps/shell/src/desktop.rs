//! The desktop: the wallpaper, shortcut icons (home folder, games and the
//! files in `~/Desktop`) and the desktop context menu.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vfiles::FileKind;
use vgfx::{Align, Color, Rect};
use vproto::vfs;
use vui::{Icon, MenuItem, Ui};

use crate::{Action, Model, apps, chrome, taskbar};

/// A desktop icon.
pub struct Shortcut {
    pub label: String,
    /// Application to start, with its arguments.
    pub app: String,
    pub args: Vec<String>,
    pub icon: Icon,
    /// Tile colour key (an app id).
    pub color: String,
}

const CELL_W: i32 = 92;
const CELL_H: i32 = 98;
const MARGIN: i32 = 14;
const DESKTOP_DIR: &str = "/home/user/Desktop";

/// Which application opens a file, by extension.
pub fn app_for_file(name: &str) -> Option<(&'static str, Icon)> {
    let app = vfiles::default_app(name)?;
    let icon = match vfiles::file_kind(name) {
        FileKind::Image => Icon::Image,
        FileKind::Audio => Icon::Music,
        _ => Icon::Document,
    };
    Some((app.id, icon))
}

/// What the desktop looked like when last presented.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Look {
    hovered: Option<usize>,
    selected: Option<usize>,
    menu_open: bool,
    wallpaper: u64,
    items: u64,
}

pub struct Desktop {
    items: Vec<Shortcut>,
    selected: Option<usize>,
    /// Incremented when `items` changes.
    generation: u64,
    shown: Option<Look>,
}

impl Desktop {
    pub fn new() -> Desktop {
        Desktop { items: Vec::new(), selected: None, generation: 0, shown: None }
    }

    /// Rebuilds the icon list: fixed shortcuts plus `~/Desktop`.
    pub fn refresh(&mut self, m: &Model, vfs: Option<&vfs::Client>) {
        let mut items = Vec::new();
        if m.app("files").is_some() {
            items.push(Shortcut {
                label: "Home".into(),
                app: "files".into(),
                args: vec!["/home/user".into()],
                icon: Icon::Home,
                color: "home".into(),
            });
        }
        for id in ["racer", "starfall"] {
            if let Some(app) = m.app(id) {
                items.push(Shortcut {
                    label: app.name.clone(),
                    app: id.into(),
                    args: Vec::new(),
                    icon: apps::icon_for(app),
                    color: id.into(),
                });
            }
        }
        if let Some(Ok(Ok(mut entries))) = vfs.map(|v| v.read_dir(DESKTOP_DIR.into())) {
            entries.sort_by(|a, b| {
                b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            for e in entries.iter().filter(|e| !e.name.starts_with('.')) {
                let path = format!("{DESKTOP_DIR}/{}", e.name);
                if e.is_dir {
                    items.push(Shortcut {
                        label: e.name.clone(),
                        app: vfiles::kind::FILES.id.into(),
                        args: vec![path],
                        icon: Icon::Folder,
                        color: "files".into(),
                    });
                } else if let Some((app, icon)) = app_for_file(&e.name) {
                    items.push(Shortcut {
                        label: e.name.clone(),
                        app: app.into(),
                        args: vec![path],
                        icon,
                        color: app.into(),
                    });
                }
            }
        }
        self.items = items;
        self.selected = None;
        self.generation += 1;
    }

    fn cell(&self, i: usize, screen_h: i32) -> Rect {
        let rows = ((screen_h - taskbar::HEIGHT - 2 * MARGIN) / CELL_H).max(1) as usize;
        let (col, row) = ((i / rows) as i32, (i % rows) as i32);
        Rect::new(MARGIN + col * (CELL_W + 4), MARGIN + row * CELL_H, CELL_W, CELL_H - 6)
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        // Interaction first: it decides whether anything visible changed.
        let mut hovered = None;
        let mut on_icon = false;
        for i in 0..self.items.len() {
            let r = self.cell(i, ui.height);
            let resp = ui.interact(ui.id("icon") ^ ((i as u64 + 1) << 24), r);
            if resp.hovered {
                hovered = Some(i);
                ui.set_cursor(vui::Cursor::Hand);
            }
            if resp.pressed || resp.right_clicked {
                on_icon = true;
                self.selected = Some(i);
            }
            if resp.double_clicked {
                let s = &self.items[i];
                m.push(Action::Launch(s.app.clone(), s.args.clone()));
            }
        }
        let pressed_bare = !on_icon && ui.pointer().is_some() && (ui.input.pressed[0] || ui.input.pressed[1]);
        if pressed_bare {
            self.selected = None;
        }
        if pressed_bare && ui.input.pressed[1] {
            let (x, y) = ui.input.pointer.unwrap_or((0, 0));
            ui.open_context_menu("desktop", x, y);
        }
        let menu = [
            MenuItem::new("Next wallpaper"),
            MenuItem::new("Personalize"),
            MenuItem::separator(),
            MenuItem::new("Open Terminal"),
            MenuItem::new("Open Home folder"),
            MenuItem::separator(),
            MenuItem::new("Refresh"),
            MenuItem::new("About Vindows"),
        ];
        let chosen = ui.context_menu("desktop", &menu);
        let action = match chosen {
            Some(0) => Some(Action::NextWallpaper),
            Some(1) => Some(Action::Launch("settings".into(), Vec::new())),
            Some(3) => Some(Action::Launch("terminal".into(), Vec::new())),
            Some(4) => Some(Action::Launch("files".into(), vec!["/home/user".into()])),
            Some(6) => Some(Action::RefreshDesktop),
            Some(7) => Some(Action::Launch("about".into(), Vec::new())),
            _ => None,
        };
        if let Some(a) = action {
            m.push(a);
        }

        let look = Look {
            hovered,
            selected: self.selected,
            menu_open: ui.state.context_menu.is_some(),
            wallpaper: m.wallpaper_generation,
            items: self.generation,
        };
        // An open menu animates and tracks the pointer: always redraw it.
        if self.shown == Some(look) && !look.menu_open {
            ui.skip_present();
            return;
        }
        self.shown = Some(look);

        // The wallpaper fills the screen; scale it if the size changed.
        let wp = &m.wallpaper.image;
        if wp.width == ui.width && wp.height == ui.height {
            ui.canvas.blit(wp, wp.rect(), 0, 0);
        } else {
            ui.canvas.draw_bitmap_scaled(wp, ui.rect(), vgfx::Filter::Bilinear, 255);
        }
        let t = ui.theme().clone();
        for i in 0..self.items.len() {
            let r = self.cell(i, ui.height);
            let selected = self.selected == Some(i);
            if selected {
                ui.canvas.fill_rounded_rect(r, 8.0, Color::rgba(91, 140, 255, 72));
                ui.canvas.stroke_rounded_rect(r, 8.0, 1.0, Color::rgba(140, 175, 255, 150));
            } else if hovered == Some(i) {
                ui.canvas.fill_rounded_rect(r, 8.0, Color::rgba(255, 255, 255, 30));
                ui.canvas.stroke_rounded_rect(r, 8.0, 1.0, Color::rgba(255, 255, 255, 36));
            }
            let s = &self.items[i];
            let tile = Rect::new(r.x + (r.w - 50) / 2, r.y + 9, 50, 50);
            ui.canvas.draw_shadow(tile.translate(0, 3), 13, 8, Color::rgba(0, 0, 0, 90));
            apps::draw_tile(&mut ui.canvas, tile, &s.color, s.icon);
            let font = ui.ctx.font(vui::Font::Regular);
            let label = ui.ctx.text.ellipsize(font, t.small_size + 0.5, &s.label, (r.w - 8) as f32);
            chrome::shadowed_label(
                ui,
                Rect::new(r.x + 2, tile.bottom() + 6, r.w - 4, 20),
                &label,
                t.small_size + 0.5,
                Align::Center,
            );
        }
    }
}
