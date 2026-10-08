//! The desktop: the wallpaper, shortcut icons (the Trash first, then the
//! home folder, games and the files in `~/Desktop`) and the context menus
//! of the desktop and its icons.
//!
//! Items of `~/Desktop` can be dragged onto the Trash, or into the home
//! folder or a folder on the desktop. The icons follow `~/Desktop` and the
//! Trash as other programs change them (see [`Desktop::stale`]).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vfiles::{FileKind, Fs, HOME, trash};
use vgfx::{Align, Color, Rect};
use vui::{Cursor, Icon, MenuItem, Ui};

use crate::{Action, Model, apps, chrome, taskbar};

/// What a desktop icon stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The Trash (`full` with anything in it).
    Trash { full: bool },
    /// A folder: the home folder, or one in `~/Desktop`.
    Folder { path: String, on_desktop: bool },
    /// A file in `~/Desktop`.
    File(String),
    /// An application.
    App,
}

/// A desktop icon.
pub struct Shortcut {
    pub label: String,
    /// Application to start, with its arguments.
    pub app: String,
    pub args: Vec<String>,
    pub icon: Icon,
    /// Tile colour key (an app id).
    pub color: String,
    pub kind: Kind,
}

impl Shortcut {
    /// The folder that items dropped on this icon go into (the Trash's
    /// items, for the Trash).
    fn drop_dir(&self) -> Option<&str> {
        match &self.kind {
            Kind::Trash { .. } => Some(trash::FILES),
            Kind::Folder { path, .. } => Some(path),
            _ => None,
        }
    }

    /// The item of `~/Desktop` this icon is: it can be dragged, and moved
    /// to the Trash.
    fn desktop_path(&self) -> Option<&str> {
        match &self.kind {
            Kind::File(path) | Kind::Folder { path, on_desktop: true } => Some(path),
            _ => None,
        }
    }
}

const CELL_W: i32 = 92;
const CELL_H: i32 = 98;
const MARGIN: i32 = 14;
const DESKTOP_DIR: &str = "/home/user/Desktop";
/// How far the pointer moves with the button down before it drags.
const DRAG_START: i32 = 8;

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

/// What the icons show of the file system: whether anything is in the
/// Trash, and what is in `~/Desktop` (name, folder), in icon order.
#[derive(Clone, Default, PartialEq, Eq)]
struct Contents {
    trash_full: bool,
    desktop: Vec<(String, bool)>,
}

fn contents(fs: &Fs) -> Contents {
    let mut desktop: Vec<(String, bool)> = fs
        .read_dir(DESKTOP_DIR)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| !e.name.starts_with('.'))
        .map(|e| (e.name, e.is_dir))
        .collect();
    desktop.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    Contents { trash_full: trash::count(fs) > 0, desktop }
}

/// An icon being dragged.
struct Drag {
    index: usize,
    /// Where the button went down.
    origin: (i32, i32),
    /// The pointer moved far enough to count as a drag.
    active: bool,
}

/// What the desktop looked like when last presented.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Look {
    hovered: Option<usize>,
    selected: Option<usize>,
    menu_open: bool,
    wallpaper: u64,
    items: u64,
    /// The pointer while an icon is dragged, and the icon it would drop on.
    drag: Option<(i32, i32)>,
    target: Option<usize>,
}

/// Choices of an icon's context menu.
#[derive(Clone, Copy)]
enum IconAction {
    Open,
    EmptyTrash,
    MoveToTrash,
}

pub struct Desktop {
    items: Vec<Shortcut>,
    selected: Option<usize>,
    /// Incremented when `items` changes.
    generation: u64,
    shown: Option<Look>,
    /// What the icons were made from.
    contents: Contents,
    drag: Option<Drag>,
    /// The icon the dragged one is over, if it takes drops.
    target: Option<usize>,
    /// The icon whose context menu is open.
    menu_for: Option<usize>,
    /// A context menu was open in the last frame.
    menu_open: bool,
}

impl Desktop {
    pub fn new() -> Desktop {
        Desktop {
            items: Vec::new(),
            selected: None,
            generation: 0,
            shown: None,
            contents: Contents::default(),
            drag: None,
            target: None,
            menu_for: None,
            menu_open: false,
        }
    }

    /// Whether `~/Desktop` or the Trash changed since the icons were made.
    pub fn stale(&self, fs: &Fs) -> bool {
        contents(fs) != self.contents
    }

    /// An icon is being dragged, or a menu is open: the icons should not
    /// change under the pointer.
    pub fn busy(&self) -> bool {
        self.drag.as_ref().is_some_and(|d| d.active) || self.menu_open
    }

    /// Rebuilds the icon list: the Trash, fixed shortcuts and `~/Desktop`.
    pub fn refresh(&mut self, m: &Model, fs: &Fs) {
        let c = contents(fs);
        let mut items = Vec::new();
        if m.app("files").is_some() {
            items.push(Shortcut {
                label: "Trash".into(),
                app: "files".into(),
                args: vec![trash::FILES.into()],
                icon: if c.trash_full { Icon::TrashFull } else { Icon::Trash },
                color: "trash".into(),
                kind: Kind::Trash { full: c.trash_full },
            });
            items.push(Shortcut {
                label: "Home".into(),
                app: "files".into(),
                args: vec![HOME.into()],
                icon: Icon::Home,
                color: "home".into(),
                kind: Kind::Folder { path: HOME.into(), on_desktop: false },
            });
        }
        for id in ["racer", "starfall", "prism"] {
            if let Some(app) = m.app(id) {
                items.push(Shortcut {
                    label: app.name.clone(),
                    app: id.into(),
                    args: Vec::new(),
                    icon: apps::icon_for(app),
                    color: id.into(),
                    kind: Kind::App,
                });
            }
        }
        for (name, is_dir) in &c.desktop {
            let path = format!("{DESKTOP_DIR}/{name}");
            if *is_dir {
                items.push(Shortcut {
                    label: name.clone(),
                    app: vfiles::kind::FILES.id.into(),
                    args: vec![path.clone()],
                    icon: Icon::Folder,
                    color: "files".into(),
                    kind: Kind::Folder { path, on_desktop: true },
                });
            } else if let Some((app, icon)) = app_for_file(name) {
                items.push(Shortcut {
                    label: name.clone(),
                    app: app.into(),
                    args: vec![path.clone()],
                    icon,
                    color: app.into(),
                    kind: Kind::File(path),
                });
            }
        }
        // The selection stays on its icon if it is still there.
        let selected = self.selected.and_then(|i| self.items.get(i)).map(|s| (s.label.clone(), s.args.clone()));
        self.selected =
            selected.and_then(|(label, args)| items.iter().position(|s| s.label == label && s.args == args));
        self.items = items;
        self.contents = c;
        self.drag = None;
        self.target = None;
        self.menu_for = None;
        self.generation += 1;
    }

    fn cell(&self, i: usize, screen_h: i32) -> Rect {
        let rows = ((screen_h - taskbar::HEIGHT - 2 * MARGIN) / CELL_H).max(1) as usize;
        let (col, row) = ((i / rows) as i32, (i % rows) as i32);
        Rect::new(MARGIN + col * (CELL_W + 4), MARGIN + row * CELL_H, CELL_W, CELL_H - 6)
    }

    /// The icon at `p` that takes drops, other than the dragged one.
    fn target_at(&self, p: (i32, i32), dragged: usize, screen_h: i32) -> Option<usize> {
        (0..self.items.len())
            .find(|&i| i != dragged && self.items[i].drop_dir().is_some() && self.cell(i, screen_h).contains(p.0, p.1))
    }

    /// Follows a drag; on release over the Trash or a folder, asks for the
    /// move. Returns the pointer while an icon is being dragged.
    fn track_drag(&mut self, ui: &mut Ui, m: &mut Model) -> Option<(i32, i32)> {
        let d = self.drag.as_mut()?;
        if ui.input.down[0] {
            let p = ui.input.pointer?;
            if !d.active && (p.0 - d.origin.0).abs() + (p.1 - d.origin.1).abs() > DRAG_START {
                d.active = true;
            }
            if !d.active {
                return None;
            }
            let index = d.index;
            ui.set_cursor(Cursor::Move);
            self.target = self.target_at(p, index, ui.height);
            return Some(p);
        }
        // Released.
        let d = self.drag.take()?;
        let target = self.target.take();
        if let (true, Some(t), Some(path)) = (d.active, target, self.items[d.index].desktop_path()) {
            let path = path.to_string();
            match &self.items[t].kind {
                Kind::Trash { .. } => m.push(Action::Trash(path)),
                Kind::Folder { path: dir, .. } => m.push(Action::MoveInto(path, dir.clone())),
                _ => {}
            }
        }
        None
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        // Interaction first: it decides whether anything visible changed.
        let mut hovered = None;
        let mut on_icon = false;
        // A new press ends what is left of an earlier drag.
        if ui.input.pressed[0] {
            self.drag = None;
        }
        for i in 0..self.items.len() {
            let r = self.cell(i, ui.height);
            let resp = ui.interact(ui.id("icon") ^ ((i as u64 + 1) << 24), r);
            if resp.hovered {
                hovered = Some(i);
                ui.set_cursor(Cursor::Hand);
            }
            if resp.pressed || resp.right_clicked {
                on_icon = true;
                self.selected = Some(i);
            }
            if resp.pressed
                && !resp.double_clicked
                && self.items[i].desktop_path().is_some()
                && let Some(origin) = ui.input.pointer
            {
                self.drag = Some(Drag { index: i, origin, active: false });
            }
            if resp.double_clicked {
                self.drag = None;
                let s = &self.items[i];
                m.push(Action::Launch(s.app.clone(), s.args.clone()));
            }
            if resp.right_clicked {
                let (x, y) = ui.input.pointer.unwrap_or((0, 0));
                self.menu_for = Some(i);
                ui.open_context_menu("icon", x, y);
            }
        }
        let dragging = self.track_drag(ui, m);
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
            MenuItem::new("About Veda"),
        ];
        let chosen = ui.context_menu("desktop", &menu);
        let action = match chosen {
            Some(0) => Some(Action::NextWallpaper),
            Some(1) => Some(Action::Launch("settings".into(), Vec::new())),
            Some(3) => Some(Action::Launch("terminal".into(), Vec::new())),
            Some(4) => Some(Action::Launch("files".into(), vec![HOME.into()])),
            Some(6) => Some(Action::RefreshDesktop),
            Some(7) => Some(Action::Launch("about".into(), Vec::new())),
            _ => None,
        };
        if let Some(a) = action {
            m.push(a);
        }
        self.icon_menu(ui, m);

        self.menu_open = ui.state.context_menu.is_some();
        let look = Look {
            hovered,
            selected: self.selected,
            menu_open: self.menu_open,
            wallpaper: m.wallpaper_generation,
            items: self.generation,
            drag: dragging,
            target: self.target.filter(|_| dragging.is_some()),
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
            if look.target == Some(i) {
                ui.canvas.fill_rounded_rect(r, 8.0, t.accent.with_alpha(70));
                ui.canvas.stroke_rounded_rect(r, 8.0, 1.5, t.accent);
            } else if selected {
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
        if let (Some(p), Some(d)) = (dragging, &self.drag) {
            self.draw_dragged(ui, d.index, p, look.target);
        }
    }

    /// The dragged icon under the pointer, and what dropping it does.
    fn draw_dragged(&self, ui: &mut Ui, index: usize, (px, py): (i32, i32), target: Option<usize>) {
        let t = ui.theme().clone();
        let s = &self.items[index];
        let tile = Rect::new(px - 20, py - 20, 40, 40);
        ui.canvas.draw_shadow(tile.translate(0, 4), 12, 10, Color::rgba(0, 0, 0, 120));
        apps::draw_tile(&mut ui.canvas, tile, &s.color, s.icon);
        let Some(target) = target.and_then(|i| self.items.get(i)) else { return };
        let text = match &target.kind {
            Kind::Trash { .. } => "Move to the Trash".to_string(),
            _ => format!("Move to {}", target.label),
        };
        let w = ui.measure(&text, vui::Font::Regular, t.small_size) as i32 + 24;
        let badge = Rect::new((px + 24).min(ui.width - w - 4), (py + 16).min(ui.height - 34), w, 28);
        ui.canvas.fill_rounded_rect(badge, 8.0, Color::hex(0x2E2E36).with_alpha(240));
        ui.canvas.stroke_rounded_rect(badge, 8.0, 1.0, t.accent);
        ui.label(badge, &text, vui::Font::Regular, t.small_size, t.text, Align::Center);
    }

    /// The context menu of the icon right-clicked last.
    fn icon_menu(&mut self, ui: &mut Ui, m: &mut Model) {
        let Some(s) = self.menu_for.and_then(|i| self.items.get(i)) else { return };
        let mut entries: Vec<(MenuItem, Option<IconAction>)> = vec![(MenuItem::new("Open"), Some(IconAction::Open))];
        match &s.kind {
            Kind::Trash { full } => {
                entries.push((MenuItem::separator(), None));
                entries.push((MenuItem::new("Empty Trash").enabled(*full), Some(IconAction::EmptyTrash)));
            }
            _ if s.desktop_path().is_some() => {
                entries.push((MenuItem::separator(), None));
                entries.push((MenuItem::new("Move to Trash"), Some(IconAction::MoveToTrash)));
            }
            _ => {}
        }
        let menu: Vec<MenuItem> = entries.iter().map(|(item, _)| item.clone()).collect();
        let Some(choice) = ui.context_menu("icon", &menu).and_then(|i| entries[i].1) else { return };
        match choice {
            IconAction::Open => m.push(Action::Launch(s.app.clone(), s.args.clone())),
            // Files asks first, in a window of its own.
            IconAction::EmptyTrash => m.push(Action::Launch("files".into(), vec!["--empty-trash".into()])),
            IconAction::MoveToTrash => {
                if let Some(path) = s.desktop_path() {
                    m.push(Action::Trash(path.to_string()));
                }
            }
        }
        self.menu_for = None;
    }
}
