//! A modal dialog for choosing a file to open or a place to save one,
//! browsing the file system through the VFS service.
//!
//! ```ignore
//! // In App::update:
//! if let Some(dialog) = &mut self.dialog {
//!     match dialog.show(ui) {
//!         Some(FileDialogResult::Chosen(path)) => { self.open(&path); self.dialog = None; }
//!         Some(FileDialogResult::Cancelled) => self.dialog = None,
//!         None => {}
//!     }
//! }
//! ```

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vproto::fs::DirEntry;
use vproto::input::keys;
use vproto::vfs;

use crate::icons::Icon;
use crate::theme::Font;
use crate::ui::Ui;
use crate::widgets::ButtonKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDialogMode {
    Open,
    Save,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDialogResult {
    Cancelled,
    /// Absolute path of the chosen file.
    Chosen(String),
}

/// Shortcuts in the sidebar.
const PLACES: [(&str, &str, Icon); 6] = [
    ("Home", "/home/user", Icon::Home),
    ("Desktop", "/home/user/Desktop", Icon::Monitor),
    ("Documents", "/home/user/Documents", Icon::Document),
    ("Pictures", "/home/user/Pictures", Icon::Image),
    ("Music", "/home/user/Music", Icon::Music),
    ("System", "/system", Icon::Cpu),
];

/// Joins a directory and a name (or returns `name` if it is absolute).
fn join(dir: &str, name: &str) -> String {
    if name.starts_with('/') {
        String::from(name)
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The parent directory of an absolute path.
fn parent(path: &str) -> &str {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

fn human_size(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f32 / 1024.0),
        _ => format!("{:.1} MB", n as f32 / 1_048_576.0),
    }
}

pub struct FileDialog {
    mode: FileDialogMode,
    title: String,
    dir: String,
    name: String,
    entries: Vec<DirEntry>,
    selected: Option<usize>,
    error: Option<String>,
    /// Lower-case extensions shown (empty = all files).
    filter: Vec<String>,
    vfs: Option<vfs::Client>,
    /// A path the user confirmed they want to replace.
    confirm_replace: Option<String>,
    focus_name: bool,
}

impl FileDialog {
    fn new(mode: FileDialogMode, title: &str, dir: &str, name: &str) -> FileDialog {
        let vfs = vproto::connect(vfs::NAME).ok().map(vfs::Client::new);
        let mut d = FileDialog {
            mode,
            title: title.into(),
            dir: String::new(),
            name: name.into(),
            entries: Vec::new(),
            selected: None,
            error: None,
            filter: Vec::new(),
            vfs,
            confirm_replace: None,
            focus_name: true,
        };
        if !d.navigate(dir) {
            d.navigate("/home/user");
        }
        d
    }

    /// A dialog choosing an existing file, starting in `dir`.
    pub fn open(title: &str, dir: &str) -> FileDialog {
        FileDialog::new(FileDialogMode::Open, title, dir, "")
    }

    /// A dialog choosing where to save, starting in `dir` with file name `name`.
    pub fn save(title: &str, dir: &str, name: &str) -> FileDialog {
        FileDialog::new(FileDialogMode::Save, title, dir, name)
    }

    /// Shows only files with these extensions (and folders).
    pub fn with_filter(mut self, extensions: &[&str]) -> FileDialog {
        self.filter = extensions.iter().map(|e| e.to_lowercase()).collect();
        let dir = self.dir.clone();
        self.navigate(&dir);
        self
    }

    fn visible(&self, e: &DirEntry) -> bool {
        if e.name.starts_with('.') {
            return false;
        }
        e.is_dir
            || self.filter.is_empty()
            || e.name.rsplit_once('.').is_some_and(|(_, ext)| self.filter.iter().any(|f| *f == ext.to_lowercase()))
    }

    /// Lists `dir`; returns `false` if it cannot be read.
    fn navigate(&mut self, dir: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            self.error = Some("The file system is not available.".into());
            return false;
        };
        match vfs.read_dir(dir.into()) {
            Ok(Ok(mut entries)) => {
                entries.retain(|e| self.visible(e));
                entries.sort_by(|a, b| {
                    b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                self.entries = entries;
                self.dir = if dir.len() > 1 { String::from(dir.trim_end_matches('/')) } else { String::from(dir) };
                self.selected = None;
                self.error = None;
                self.confirm_replace = None;
                true
            }
            Ok(Err(e)) => {
                self.error = Some(format!("Cannot open {dir}: {e}"));
                false
            }
            Err(_) => {
                self.error = Some("The file system is not available.".into());
                false
            }
        }
    }

    /// Acts on the name field: enters folders, or picks the file.
    fn submit(&mut self) -> Option<FileDialogResult> {
        let name = self.name.trim();
        if name.is_empty() {
            return None;
        }
        let path = join(&self.dir, name);
        let stat = self.vfs.as_ref().and_then(|v| v.stat(path.clone()).ok());
        match (self.mode, stat) {
            (_, Some(Ok(st))) if st.is_dir => {
                self.navigate(&path);
                self.name.clear();
                None
            }
            (FileDialogMode::Open, Some(Ok(_))) => Some(FileDialogResult::Chosen(path)),
            (FileDialogMode::Open, _) => {
                self.error = Some(format!("\u{201c}{name}\u{201d} does not exist."));
                None
            }
            (FileDialogMode::Save, Some(Ok(st))) if st.read_only => {
                self.error = Some("That file is read-only. Choose another name or folder.".into());
                None
            }
            (FileDialogMode::Save, Some(Ok(_))) if self.confirm_replace.as_deref() != Some(path.as_str()) => {
                self.error = Some(format!("\u{201c}{name}\u{201d} already exists. Save again to replace it."));
                self.confirm_replace = Some(path);
                None
            }
            (FileDialogMode::Save, _) => {
                if self.dir.starts_with("/system") {
                    self.error = Some("The system folder is read-only.".into());
                    return None;
                }
                Some(FileDialogResult::Chosen(path))
            }
        }
    }

    /// Draws the dialog. Returns the outcome once the user is done.
    pub fn show(&mut self, ui: &mut Ui) -> Option<FileDialogResult> {
        let mut result = None;
        if ui.input.key(keys::ESC) {
            return Some(FileDialogResult::Cancelled);
        }
        let t = ui.theme().clone();
        let (w, h) = ((ui.width - 40).min(720), (ui.height - 40).min(500));
        ui.modal(w, h, |ui, card| {
            let inner = card.inset(20, 16, 20, 18);
            ui.heading(Rect::new(inner.x, inner.y, inner.w - 40, 28), &self.title);
            if ui.icon_button(Rect::new(inner.right() - 32, inner.y - 2, 32, 32), Icon::Close, "Cancel") {
                result = Some(FileDialogResult::Cancelled);
            }

            // Sidebar with places.
            let side = Rect::new(inner.x, inner.y + 44, 150, inner.h - 44 - 96);
            for (i, (label, path, icon)) in PLACES.iter().enumerate() {
                let r = Rect::new(side.x, side.y + i as i32 * 34, side.w, 32);
                let id = ui.id(label) ^ 0xf11e;
                let resp = ui.interact(id, r);
                let current = self.dir == *path;
                if current || resp.hovered {
                    let a = if current { 26 } else { 14 };
                    ui.canvas.fill_rounded_rect(r, t.radius, Color::rgba(255, 255, 255, a));
                }
                ui.icon(Rect::new(r.x + 6, r.y, 24, r.h), *icon, 16.0, if current { t.accent } else { t.text_dim });
                ui.label(
                    Rect::new(r.x + 36, r.y, r.w - 40, r.h),
                    label,
                    Font::Regular,
                    t.font_size,
                    t.text,
                    Align::Left,
                );
                if resp.clicked {
                    self.navigate(path);
                }
            }

            // Path bar.
            let main = Rect::new(side.right() + 14, inner.y + 44, inner.right() - side.right() - 14, side.h);
            let up = Rect::new(main.x, main.y, 32, 30);
            if ui.icon_button(up, Icon::ChevronUp, "Up one level") && self.dir != "/" {
                let p = String::from(parent(&self.dir));
                self.navigate(&p);
            }
            let path_r = Rect::new(up.right() + 6, main.y, main.w - 38, 30);
            ui.canvas.fill_rounded_rect(path_r, t.radius, t.control);
            let font = ui.ctx.font(Font::Regular);
            let shown = ui.ctx.text.ellipsize(font, t.font_size, &self.dir, (path_r.w - 20) as f32);
            ui.label(path_r.inset(10, 0, 10, 0), &shown, Font::Regular, t.font_size, t.text_dim, Align::Left);

            // Directory listing.
            let list_r = Rect::new(main.x, main.y + 38, main.w, main.h - 38);
            ui.canvas.fill_rounded_rect(list_r, t.radius, Color::rgba(0, 0, 0, 40));
            let mut selected = self.selected;
            let entries = core::mem::take(&mut self.entries);
            let resp = ui.list(
                list_r.inset(0, 4, 0, 4),
                "file-dialog-list",
                entries.len(),
                32,
                &mut selected,
                |ui, i, r, st| {
                    ui.row_background(r, st);
                    let e = &entries[i];
                    let icon = if e.is_dir { Icon::Folder } else { Icon::File };
                    let color = if e.is_dir { Color::hex(0xF5B841) } else { t.text_dim };
                    ui.icon(Rect::new(r.x + 10, r.y, 22, r.h), icon, 16.0, color);
                    ui.label(
                        Rect::new(r.x + 40, r.y, r.w - 140, r.h),
                        &e.name,
                        Font::Regular,
                        t.font_size,
                        t.text,
                        Align::Left,
                    );
                    if !e.is_dir {
                        ui.label(
                            Rect::new(r.right() - 100, r.y, 86, r.h),
                            &human_size(e.size),
                            Font::Regular,
                            t.small_size,
                            t.text_faint,
                            Align::Right,
                        );
                    }
                },
            );
            self.entries = entries;
            if self.entries.is_empty() {
                ui.label(list_r, "This folder is empty", Font::Regular, t.font_size, t.text_faint, Align::Center);
            }
            if let Some(i) = resp.clicked
                && let Some(e) = self.entries.get(i)
                && !e.is_dir
            {
                self.name = e.name.clone();
                self.confirm_replace = None;
            }
            self.selected = selected;
            if let Some(i) = resp.activated
                && let Some(e) = self.entries.get(i).cloned()
            {
                if e.is_dir {
                    let p = join(&self.dir, &e.name);
                    self.navigate(&p);
                } else {
                    self.name = e.name;
                    result = result.take().or_else(|| self.submit());
                }
            }

            // File name, message and buttons.
            let y = list_r.bottom() + 14;
            ui.label(Rect::new(inner.x, y, 90, 36), "File name", Font::Regular, t.font_size, t.text_dim, Align::Left);
            if self.focus_name {
                ui.focus_text_input("file-dialog-name");
                self.focus_name = false;
            }
            let name_resp =
                ui.text_input(Rect::new(inner.x + 96, y, inner.w - 96, 36), "file-dialog-name", &mut self.name, "");
            if name_resp.changed {
                self.confirm_replace = None;
                self.error = None;
            }
            let by = inner.bottom() - 36;
            if let Some(err) = &self.error {
                ui.label(
                    Rect::new(inner.x, by, inner.w - 240, 36),
                    err,
                    Font::Regular,
                    t.small_size + 1.0,
                    t.danger,
                    Align::Left,
                );
            }
            let ok_label = if self.mode == FileDialogMode::Open { "Open" } else { "Save" };
            if ui.button_full(Rect::new(inner.right() - 104, by, 104, 36), None, ok_label, ButtonKind::Primary)
                || name_resp.submitted
            {
                result = result.take().or_else(|| self.submit());
            }
            if ui.button(Rect::new(inner.right() - 216, by, 104, 36), "Cancel") {
                result = Some(FileDialogResult::Cancelled);
            }
        });
        result
    }
}
