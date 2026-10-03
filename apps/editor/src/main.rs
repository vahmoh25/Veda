//! Text Editor: a tabbed editor for plain text and source code with syntax
//! highlighting, find and replace, word wrap and unlimited undo.
//!
//! The editing model (buffer, selection, undo, layout, highlighting) is the
//! host-tested `vtext` crate; [`view`] draws it and maps input to edits.
//! This module is the application around it: tabs, menus, files, dialogs,
//! the find bar and the status bar.

#![no_std]
#![no_main]

extern crate alloc;

mod view;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vproto::display::modifiers;
use vproto::fs::FsError;
use vproto::input::keys;
use vproto::vfs;
use vrt::object::Vmo;
use vtext::highlight::Language;
use vtext::{Document, Pos};
use vui::{App, FileDialog, FileDialogResult, Font, Icon, Menu, MenuItem, Ui, WindowSpec};

use view::{Command, TextView, ViewOptions};

vrt::entry!(main);

const DEFAULT_DIR: &str = "/home/user/Documents";
const DEFAULT_FONT_SIZE: f32 = 14.0;
const TOAST_NS: u64 = 2_500_000_000;

/// The parent directory of an absolute path.
fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).into()
}

/// Reads a text file; invalid UTF-8 is replaced (the flag reports it).
fn read_text(vfs: &vfs::Client, path: &str) -> Result<(String, bool), FsError> {
    let (vmo, len) = vfs.read_file(path.into()).map_err(|_| FsError::Io)??;
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        vmo.read(0, &mut buf).map_err(|_| FsError::Io)?;
    }
    Ok(match String::from_utf8(buf) {
        Ok(s) => (s, false),
        Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
    })
}

fn write_text(vfs: Option<&vfs::Client>, path: &str, text: &str) -> Result<(), String> {
    let vfs = vfs.ok_or_else(|| String::from("the file system is not available"))?;
    let vmo = Vmo::create(text.len().max(1)).map_err(|_| String::from("out of memory"))?;
    vmo.write(0, text.as_bytes()).map_err(|_| String::from("out of memory"))?;
    match vfs.write_file(path.into(), vmo, text.len() as u64) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the file system is not available".into()),
    }
}

/// One open document.
struct Tab {
    doc: Document,
    path: Option<String>,
    title: String,
    lang: Language,
    view: TextView,
    read_only: bool,
    /// The file was not valid UTF-8; invalid bytes were replaced.
    lossy: bool,
}

impl Tab {
    fn untitled(n: u32) -> Tab {
        let title = if n <= 1 { "Untitled".into() } else { format!("Untitled {n}") };
        Tab {
            doc: Document::new(),
            path: None,
            title,
            lang: Language::Plain,
            view: TextView::new(),
            read_only: false,
            lossy: false,
        }
    }

    fn from_file(path: &str, text: &str, read_only: bool, lossy: bool) -> Tab {
        Tab {
            doc: Document::from_text(text),
            path: Some(path.into()),
            title: file_name(path),
            lang: Language::for_path(path),
            view: TextView::new(),
            read_only,
            lossy,
        }
    }

    /// Empty, untitled and untouched: replaced when a file is opened.
    fn is_pristine(&self) -> bool {
        self.path.is_none() && !self.doc.is_modified() && self.doc.buffer().is_empty()
    }

    fn dir(&self) -> String {
        self.path.as_deref().map_or(DEFAULT_DIR, parent).into()
    }
}

/// What to do after a "save changes?" question is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    Nothing,
    CloseTab(usize),
    Quit,
}

enum Modal {
    Open(FileDialog),
    SaveAs { dialog: FileDialog, tab: usize, then: Then },
    ConfirmClose { tab: usize, then: Then },
    GotoLine(String),
    Message { title: String, text: String },
    Shortcuts,
    About,
}

/// The find (and replace) bar.
struct Find {
    query: String,
    replacement: String,
    show_replace: bool,
    case_sensitive: bool,
    focus_query: bool,
    /// What `matches` was computed for: (tab, revision, query, case).
    key: Option<(usize, u64, String, bool)>,
    matches: Vec<(Pos, Pos)>,
}

struct Editor {
    tabs: Vec<Tab>,
    active: usize,
    modal: Option<Modal>,
    find: Option<Find>,
    opts: ViewOptions,
    vfs: Option<vfs::Client>,
    untitled: u32,
    quitting: bool,
    toast: Option<(String, u64)>,
    title: String,
    first_frame: bool,
}

const SHORTCUTS: [(&str, &str); 18] = [
    ("Ctrl+N", "New tab"),
    ("Ctrl+O", "Open a file"),
    ("Ctrl+S", "Save"),
    ("Ctrl+Shift+S", "Save as"),
    ("Ctrl+W", "Close tab"),
    ("Ctrl+Tab", "Next tab"),
    ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
    ("Ctrl+X / C / V", "Cut, copy, paste"),
    ("Ctrl+F / Ctrl+H", "Find / replace"),
    ("F3 / Shift+F3", "Next / previous match"),
    ("Ctrl+G", "Go to line"),
    ("Ctrl+L", "Select line"),
    ("Ctrl+/", "Toggle comment"),
    ("Tab / Shift+Tab", "Indent / outdent"),
    ("Alt+Up / Alt+Down", "Move lines"),
    ("Ctrl+Left / Right", "Move by word"),
    ("Alt+Z", "Toggle word wrap"),
    ("Ctrl++ / Ctrl+-", "Zoom in / out"),
];

impl Editor {
    fn new(args: &[String]) -> Editor {
        let vfs = vproto::connect(vfs::NAME).ok().map(vfs::Client::new);
        let mut ed = Editor {
            tabs: vec![Tab::untitled(1)],
            active: 0,
            modal: None,
            find: None,
            opts: ViewOptions { wrap: true, line_numbers: true, font_size: DEFAULT_FONT_SIZE },
            vfs,
            untitled: 1,
            quitting: false,
            toast: None,
            title: String::new(),
            first_frame: true,
        };
        for path in args {
            ed.open_path(path, true);
        }
        ed
    }

    fn tab(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn new_tab(&mut self) {
        self.untitled += 1;
        self.tabs.push(Tab::untitled(self.untitled));
        self.active = self.tabs.len() - 1;
    }

    fn message(&mut self, title: &str, text: String) {
        self.modal = Some(Modal::Message { title: title.into(), text });
    }

    fn show_toast(&mut self, text: String, now: u64) {
        self.toast = Some((text, now + TOAST_NS));
    }

    /// Opens `path` in a tab (or switches to it if already open). With
    /// `create`, a missing file opens as a new empty document with that path.
    fn open_path(&mut self, path: &str, create: bool) {
        if let Some(i) = self.tabs.iter().position(|t| t.path.as_deref() == Some(path)) {
            self.active = i;
            return;
        }
        let Some(vfs) = &self.vfs else {
            return self.message("Couldn't open the file", "The file system is not available.".into());
        };
        let tab = match read_text(vfs, path) {
            Ok((text, lossy)) => {
                let st = vfs.stat(path.into()).ok().and_then(|r| r.ok());
                let read_only = path.starts_with("/system/") || st.is_some_and(|s| s.read_only);
                Tab::from_file(path, &text, read_only, lossy)
            }
            Err(FsError::NotFound) if create => Tab::from_file(path, "", false, false),
            Err(e) => return self.message("Couldn't open the file", format!("{path}: {e}.")),
        };
        if self.tabs[self.active].is_pristine() {
            self.tabs[self.active] = tab;
        } else {
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
    }

    fn show_open(&mut self) {
        let dir = self.tabs[self.active].dir();
        self.modal = Some(Modal::Open(FileDialog::open("Open", &dir)));
    }

    /// Saves a tab (asking for a name if it has none), then continues with `then`.
    fn save(&mut self, ui: &mut Ui, i: usize, then: Then) {
        let tab = &self.tabs[i];
        let Some(path) = tab.path.clone().filter(|_| !tab.read_only) else {
            return self.save_as(i, then);
        };
        let text = tab.doc.text();
        match write_text(self.vfs.as_ref(), &path, &text) {
            Ok(()) => {
                let tab = &mut self.tabs[i];
                tab.doc.mark_saved();
                tab.lossy = false;
                let msg = format!("Saved {}", tab.title);
                self.show_toast(msg, ui.now());
                self.after(ui, then);
            }
            Err(e) => self.message("Couldn't save the file", format!("{path}: {e}.")),
        }
    }

    fn save_as(&mut self, i: usize, then: Then) {
        let tab = &self.tabs[i];
        let dir = if tab.read_only { DEFAULT_DIR.into() } else { tab.dir() };
        let name = if tab.path.is_some() { tab.title.clone() } else { format!("{}.txt", tab.title) };
        self.modal = Some(Modal::SaveAs { dialog: FileDialog::save("Save As", &dir, &name), tab: i, then });
    }

    fn close_tab(&mut self, i: usize) {
        if self.tabs[i].doc.is_modified() {
            self.active = i;
            self.modal = Some(Modal::ConfirmClose { tab: i, then: Then::CloseTab(i) });
        } else {
            self.remove_tab(i);
        }
    }

    fn remove_tab(&mut self, i: usize) {
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.untitled = 1;
            self.tabs.push(Tab::untitled(1));
        }
        if i < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        if let Some(f) = &mut self.find {
            f.key = None;
        }
    }

    fn after(&mut self, ui: &mut Ui, then: Then) {
        match then {
            Then::Nothing => {}
            Then::CloseTab(i) => self.remove_tab(i),
            Then::Quit => self.request_quit(ui),
        }
    }

    /// Asks about unsaved tabs one by one, then closes the window.
    fn request_quit(&mut self, ui: &mut Ui) {
        if let Some(i) = self.tabs.iter().position(|t| t.doc.is_modified()) {
            self.active = i;
            self.modal = Some(Modal::ConfirmClose { tab: i, then: Then::Quit });
        } else {
            self.quitting = true;
            ui.close_window();
        }
    }

    fn command(&mut self, ui: &mut Ui, cmd: Command) {
        let tab = &mut self.tabs[self.active];
        tab.view.command(ui, &mut tab.doc, tab.lang, tab.read_only, cmd);
        TextView::focus(ui);
    }

    fn zoom(&mut self, delta: f32) {
        self.opts.font_size =
            if delta == 0.0 { DEFAULT_FONT_SIZE } else { (self.opts.font_size + delta).clamp(9.0, 32.0) };
    }

    fn open_find(&mut self, ui: &mut Ui, replace: bool) {
        let tab = &self.tabs[self.active];
        let selected = tab.doc.selected_text();
        let read_only = tab.read_only;
        let f = self.find.get_or_insert_with(|| Find {
            query: String::new(),
            replacement: String::new(),
            show_replace: false,
            case_sensitive: false,
            focus_query: true,
            key: None,
            matches: Vec::new(),
        });
        if !selected.is_empty() && !selected.contains('\n') {
            f.query = selected;
        }
        f.show_replace = replace && !read_only;
        f.focus_query = true;
        ui.repaint();
    }

    // ---- menus and shortcuts -------------------------------------------------

    fn menus(&self) -> [Menu; 4] {
        let tab = &self.tabs[self.active];
        let doc = &tab.doc;
        let edit = !tab.read_only;
        let has_sel = !doc.selection().is_empty();
        [
            Menu::new(
                "File",
                vec![
                    MenuItem::new("New Tab").shortcut("Ctrl+N"),
                    MenuItem::new("Open...").shortcut("Ctrl+O"),
                    MenuItem::separator(),
                    MenuItem::new("Save").shortcut("Ctrl+S"),
                    MenuItem::new("Save As...").shortcut("Ctrl+Shift+S"),
                    MenuItem::separator(),
                    MenuItem::new("Close Tab").shortcut("Ctrl+W"),
                    MenuItem::new("Exit"),
                ],
            ),
            Menu::new(
                "Edit",
                vec![
                    MenuItem::new("Undo").shortcut("Ctrl+Z").enabled(edit && doc.can_undo()),
                    MenuItem::new("Redo").shortcut("Ctrl+Y").enabled(edit && doc.can_redo()),
                    MenuItem::separator(),
                    MenuItem::new("Cut").shortcut("Ctrl+X").enabled(edit && has_sel),
                    MenuItem::new("Copy").shortcut("Ctrl+C").enabled(has_sel),
                    MenuItem::new("Paste").shortcut("Ctrl+V").enabled(edit),
                    MenuItem::new("Delete").shortcut("Del").enabled(edit && has_sel),
                    MenuItem::separator(),
                    MenuItem::new("Find...").shortcut("Ctrl+F"),
                    MenuItem::new("Replace...").shortcut("Ctrl+H").enabled(edit),
                    MenuItem::new("Go to Line...").shortcut("Ctrl+G"),
                    MenuItem::separator(),
                    MenuItem::new("Toggle Comment")
                        .shortcut("Ctrl+/")
                        .enabled(edit && tab.lang.line_comment().is_some()),
                    MenuItem::new("Move Line Up").shortcut("Alt+Up").enabled(edit),
                    MenuItem::new("Move Line Down").shortcut("Alt+Down").enabled(edit),
                    MenuItem::separator(),
                    MenuItem::new("Select All").shortcut("Ctrl+A"),
                ],
            ),
            Menu::new(
                "View",
                vec![
                    MenuItem::new("Word Wrap").shortcut("Alt+Z").checked(self.opts.wrap),
                    MenuItem::new("Line Numbers").checked(self.opts.line_numbers),
                    MenuItem::separator(),
                    MenuItem::new("Zoom In").shortcut("Ctrl++"),
                    MenuItem::new("Zoom Out").shortcut("Ctrl+-"),
                    MenuItem::new("Reset Zoom").shortcut("Ctrl+0"),
                ],
            ),
            Menu::new("Help", vec![MenuItem::new("Keyboard Shortcuts"), MenuItem::new("About Text Editor")]),
        ]
    }

    fn menu_action(&mut self, ui: &mut Ui, menu: usize, item: usize) {
        let active = self.active;
        match (menu, item) {
            (0, 0) => self.new_tab(),
            (0, 1) => self.show_open(),
            (0, 3) => self.save(ui, active, Then::Nothing),
            (0, 4) => self.save_as(active, Then::Nothing),
            (0, 6) => self.close_tab(active),
            (0, 7) => self.request_quit(ui),
            (1, 0) => self.command(ui, Command::Undo),
            (1, 1) => self.command(ui, Command::Redo),
            (1, 3) => self.command(ui, Command::Cut),
            (1, 4) => self.command(ui, Command::Copy),
            (1, 5) => self.command(ui, Command::Paste),
            (1, 6) => self.command(ui, Command::Delete),
            (1, 8) => self.open_find(ui, false),
            (1, 9) => self.open_find(ui, true),
            (1, 10) => self.modal = Some(Modal::GotoLine(String::new())),
            (1, 12) => self.command(ui, Command::ToggleComment),
            (1, 13) => self.command(ui, Command::MoveLineUp),
            (1, 14) => self.command(ui, Command::MoveLineDown),
            (1, 16) => self.command(ui, Command::SelectAll),
            (2, 0) => self.opts.wrap = !self.opts.wrap,
            (2, 1) => self.opts.line_numbers = !self.opts.line_numbers,
            (2, 3) => self.zoom(1.0),
            (2, 4) => self.zoom(-1.0),
            (2, 5) => self.zoom(0.0),
            (3, 0) => self.modal = Some(Modal::Shortcuts),
            (3, 1) => self.modal = Some(Modal::About),
            _ => {}
        }
    }

    /// Application-wide keyboard shortcuts (editing keys belong to the view).
    fn shortcuts(&mut self, ui: &mut Ui) {
        for k in ui.input.keys.clone() {
            let ctrl = k.modifiers & modifiers::CTRL != 0;
            let shift = k.modifiers & modifiers::SHIFT != 0;
            let alt = k.modifiers & modifiers::ALT != 0;
            let active = self.active;
            match k.code {
                keys::N if ctrl => self.new_tab(),
                keys::O if ctrl => self.show_open(),
                keys::S if ctrl && shift => self.save_as(active, Then::Nothing),
                keys::S if ctrl => self.save(ui, active, Then::Nothing),
                keys::W if ctrl => self.close_tab(active),
                keys::TAB if ctrl => {
                    let n = self.tabs.len();
                    self.active = if shift { (self.active + n - 1) % n } else { (self.active + 1) % n };
                }
                keys::F if ctrl => self.open_find(ui, false),
                keys::H if ctrl => self.open_find(ui, true),
                keys::G if ctrl => self.modal = Some(Modal::GotoLine(String::new())),
                keys::EQUAL | keys::KPPLUS if ctrl => self.zoom(1.0),
                keys::MINUS | keys::KPMINUS if ctrl => self.zoom(-1.0),
                keys::KEY_0 | keys::KP0 if ctrl => self.zoom(0.0),
                keys::Z if alt => self.opts.wrap = !self.opts.wrap,
                keys::F3 => self.find_step(ui, !shift),
                keys::ESC if self.find.is_some() => {
                    self.find = None;
                    TextView::focus(ui);
                }
                _ => continue,
            }
            if self.modal.is_some() {
                break;
            }
        }
    }

    // ---- find bar ------------------------------------------------------------

    /// Selects the next (or previous) match of the find query.
    fn find_step(&mut self, ui: &mut Ui, forward: bool) {
        let Some(f) = &self.find else { return self.open_find(ui, false) };
        let (query, case) = (f.query.clone(), f.case_sensitive);
        let tab = &mut self.tabs[self.active];
        let found = if forward { tab.doc.find_next(&query, case) } else { tab.doc.find_prev(&query, case) };
        if found {
            tab.view.reveal_cursor();
        }
    }

    fn find_bar(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme().clone();
        let Editor { find, tabs, active, .. } = self;
        let Some(f) = find else { return };
        let tab = &mut tabs[*active];
        ui.canvas.fill_rect(r, t.surface);
        ui.canvas.fill_rect(Rect::new(r.x, r.bottom() - 1, r.w, 1), t.border);

        let row = Rect::new(r.x + 12, r.y + 8, r.w - 24, 30);
        if f.focus_query {
            ui.focus_text_input("find-query");
            f.focus_query = false;
        }
        let input_w = (row.w - 330).clamp(180, 360);
        let q = ui.text_input(Rect::new(row.x, row.y, input_w, 30), "find-query", &mut f.query, "Find");

        // Matches for highlighting and the counter.
        let key = (*active, tab.doc.revision(), f.query.clone(), f.case_sensitive);
        if f.key.as_ref() != Some(&key) {
            f.matches = tab.doc.buffer().find_all(&f.query, f.case_sensitive, 10_000);
            f.key = Some(key);
        }
        if q.changed && !f.query.is_empty() {
            // Incremental search from the start of the selection.
            let start = tab.doc.selection().start();
            tab.doc.set_cursor(start, false);
            if tab.doc.find_next(&f.query, f.case_sensitive) {
                tab.view.reveal_cursor();
            }
        }
        if q.submitted {
            let found = if ui.input.shift() {
                tab.doc.find_prev(&f.query, f.case_sensitive)
            } else {
                tab.doc.find_next(&f.query, f.case_sensitive)
            };
            if found {
                tab.view.reveal_cursor();
            }
        }

        let mut x = row.x + input_w + 10;
        let sel = tab.doc.selection();
        let current = f.matches.binary_search(&(sel.start(), sel.end())).ok();
        let count = match (f.matches.len(), current) {
            (_, _) if f.query.is_empty() => String::new(),
            (0, _) => "No results".into(),
            (n, Some(i)) => format!("{} of {n}", i + 1),
            (n, None) => format!("{n} matches"),
        };
        let count_color = if f.matches.is_empty() && !f.query.is_empty() { t.danger } else { t.text_dim };
        ui.label(Rect::new(x, row.y, 92, 30), &count, Font::Regular, t.small_size + 0.5, count_color, Align::Left);
        x += 96;
        let mut step = None;
        if ui.icon_button(Rect::new(x, row.y, 30, 30), Icon::ChevronUp, "Previous match (Shift+F3)") {
            step = Some(false);
        }
        if ui.icon_button(Rect::new(x + 32, row.y, 30, 30), Icon::ChevronDown, "Next match (F3)") {
            step = Some(true);
        }
        if let Some(forward) = step {
            let found = if forward {
                tab.doc.find_next(&f.query, f.case_sensitive)
            } else {
                tab.doc.find_prev(&f.query, f.case_sensitive)
            };
            if found {
                tab.view.reveal_cursor();
            }
        }
        x += 70;
        // Match case toggle.
        let case_r = Rect::new(x, row.y, 36, 30);
        let case_resp = ui.interact(ui.id("find-case"), case_r);
        if f.case_sensitive {
            ui.canvas.fill_rounded_rect(case_r, t.radius, t.accent.with_alpha(70));
            ui.canvas.stroke_rounded_rect(case_r, t.radius, 1.0, t.accent);
        } else if case_resp.hovered {
            ui.canvas.fill_rounded_rect(case_r, t.radius, Color::rgba(255, 255, 255, 20));
        }
        ui.label(case_r, "Aa", Font::Bold, 13.0, if f.case_sensitive { t.text } else { t.text_dim }, Align::Center);
        if case_resp.clicked {
            f.case_sensitive = !f.case_sensitive;
        }
        x += 40;
        if !tab.read_only && ui.icon_button(Rect::new(x, row.y, 30, 30), Icon::Edit, "Toggle replace (Ctrl+H)") {
            f.show_replace = !f.show_replace;
        }
        let close = ui.icon_button(Rect::new(row.right() - 30, row.y, 30, 30), Icon::Close, "Close (Esc)");

        if f.show_replace && !tab.read_only {
            let row2 = Rect::new(row.x, row.bottom() + 8, row.w, 30);
            ui.text_input(
                Rect::new(row2.x, row2.y, input_w, 30),
                "find-replacement",
                &mut f.replacement,
                "Replace with",
            );
            let bx = row2.x + input_w + 10;
            if ui.button(Rect::new(bx, row2.y, 92, 30), "Replace")
                && !f.query.is_empty()
                && tab.doc.replace_next(&f.query, &f.replacement, f.case_sensitive)
            {
                tab.view.reveal_cursor();
            }
            if ui.button(Rect::new(bx + 100, row2.y, 110, 30), "Replace All") && !f.query.is_empty() {
                let n = tab.doc.replace_all(&f.query, &f.replacement, f.case_sensitive);
                let msg =
                    if n == 1 { "Replaced 1 occurrence".to_string() } else { format!("Replaced {n} occurrences") };
                self.toast = Some((msg, ui.now() + TOAST_NS));
            }
        }
        if close {
            self.find = None;
            TextView::focus(ui);
        }
    }

    // ---- chrome --------------------------------------------------------------

    fn tab_strip(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(r, t.surface);
        ui.canvas.fill_rect(Rect::new(r.x, r.bottom() - 1, r.w, 1), t.border);
        let n = self.tabs.len() as i32;
        let tab_w = ((r.w - 60) / n.max(1) - 2).clamp(96, 220);
        let mut x = r.x + 8;
        let (mut activate, mut close) = (None, None);
        for (i, tab) in self.tabs.iter().enumerate() {
            let tr = Rect::new(x, r.y + 6, tab_w, r.h - 6);
            if tr.right() > r.right() - 40 {
                break;
            }
            let id = ui.id("tab") ^ ((i as u64 + 1) << 24);
            let resp = ui.interact(id, tr);
            let is_active = i == self.active;
            ui.canvas.save();
            ui.canvas.clip_to(tr);
            if is_active {
                ui.canvas.fill_rounded_rect(Rect::new(tr.x, tr.y, tr.w, tr.h + 10), 8.0, t.bg);
                ui.canvas.fill_rounded_rect(Rect::new(tr.x + 10, tr.y, tr.w - 20, 2), 1.0, t.accent);
            } else if resp.hovered {
                ui.canvas.fill_rounded_rect(
                    Rect::new(tr.x, tr.y, tr.w, tr.h + 10),
                    8.0,
                    Color::rgba(255, 255, 255, 12),
                );
            }
            ui.canvas.restore();
            let icon = if tab.read_only { Icon::Lock } else { Icon::Document };
            ui.icon(Rect::new(tr.x + 10, tr.y, 18, tr.h), icon, 14.0, if is_active { t.accent } else { t.text_faint });
            let font = ui.ctx.font(Font::Regular);
            let label = ui.ctx.text.ellipsize(font, 13.0, &tab.title, (tr.w - 70) as f32);
            ui.label(
                Rect::new(tr.x + 34, tr.y, tr.w - 64, tr.h),
                &label,
                Font::Regular,
                13.0,
                if is_active { t.text } else { t.text_dim },
                Align::Left,
            );
            let cr = Rect::new(tr.right() - 28, tr.y + (tr.h - 22) / 2, 22, 22);
            let close_resp = ui.interact(id ^ 0xc105e, cr);
            if tab.doc.is_modified() && !resp.hovered && !close_resp.hovered {
                ui.canvas.fill_circle(cr.x as f32 + 11.0, cr.y as f32 + 11.0, 4.0, t.text_dim);
            } else if is_active || resp.hovered {
                if close_resp.hovered {
                    ui.canvas.fill_rounded_rect(cr, 5.0, Color::rgba(255, 255, 255, 30));
                }
                ui.icon(cr, Icon::Close, 10.0, t.text_dim);
            }
            if close_resp.clicked || (resp.hovered && ui.input.pressed[2]) {
                close = Some(i);
            } else if resp.pressed {
                activate = Some(i);
            }
            x += tab_w + 2;
        }
        if ui.icon_button(Rect::new(x + 4, r.y + 8, 30, 26), Icon::Plus, "New tab (Ctrl+N)") {
            self.new_tab();
        }
        if let Some(i) = activate {
            self.active = i;
            TextView::focus(ui);
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(r, t.surface);
        ui.canvas.fill_rect(Rect::new(r.x, r.y, r.w, 1), t.border);
        let tab = &self.tabs[self.active];
        let doc = &tab.doc;
        let c = doc.cursor();
        let col = vtext::display_col(doc.buffer().line(c.line), c.col, doc.tab_width) + 1;
        let mut left = format!("Ln {}, Col {}", c.line + 1, col);
        if !doc.selection().is_empty() {
            let n = doc.selected_text().chars().count();
            left.push_str(&format!("   ({n} selected)"));
        }
        let size = t.small_size + 0.5;
        ui.label(Rect::new(r.x + 14, r.y, 320, r.h), &left, Font::Regular, size, t.text_dim, Align::Left);
        if tab.read_only {
            ui.label(Rect::new(r.x + 260, r.y, 120, r.h), "Read-only", Font::Bold, size, t.warning, Align::Left);
        }
        let zoom = format!("{}%", (self.opts.font_size / DEFAULT_FONT_SIZE * 100.0 + 0.5) as u32);
        let encoding = if tab.lossy { "UTF-8 (repaired)" } else { "UTF-8" };
        let items = [tab.lang.name(), doc.buffer().line_ending.name(), encoding, zoom.as_str()];
        let mut x = r.right() - 14;
        for item in items.iter().rev() {
            let w = ui.measure(item, Font::Regular, size) as i32;
            x -= w;
            ui.label(Rect::new(x, r.y, w + 2, r.h), item, Font::Regular, size, t.text_dim, Align::Left);
            x -= 26;
        }
    }

    fn draw_toast(&mut self, ui: &mut Ui, bottom: i32) {
        let Some((text, until)) = &self.toast else { return };
        if ui.now() >= *until {
            self.toast = None;
            return;
        }
        let until = *until;
        let t = ui.theme().clone();
        let w = ui.measure(text, Font::Regular, 13.0) as i32 + 52;
        let r = Rect::new(ui.width - w - 24, bottom - 52, w, 38);
        ui.canvas.draw_shadow(r.translate(0, 4), 10, 12, Color::rgba(0, 0, 0, 110));
        ui.canvas.fill_rounded_rect(r, 10.0, Color::hex(0x2E2E36));
        ui.canvas.stroke_rounded_rect(r, 10.0, 1.0, t.border_strong);
        ui.icon(Rect::new(r.x + 10, r.y, 20, r.h), Icon::Check, 14.0, t.success);
        let text = text.clone();
        ui.label(Rect::new(r.x + 36, r.y, w - 44, r.h), &text, Font::Regular, 13.0, t.text, Align::Left);
        ui.repaint_at(until);
    }

    // ---- modals --------------------------------------------------------------

    fn modal(&mut self, ui: &mut Ui) {
        let Some(mut modal) = self.modal.take() else { return };
        let keep = match &mut modal {
            Modal::Open(dialog) => match dialog.show(ui) {
                Some(FileDialogResult::Chosen(path)) => {
                    self.open_path(&path, false);
                    TextView::focus(ui);
                    false
                }
                Some(FileDialogResult::Cancelled) => false,
                None => true,
            },
            Modal::SaveAs { dialog, tab, then } => match dialog.show(ui) {
                Some(FileDialogResult::Chosen(path)) => {
                    let (tab, then) = (*tab, *then);
                    let t = &mut self.tabs[tab];
                    t.title = file_name(&path);
                    t.lang = Language::for_path(&path);
                    t.path = Some(path);
                    t.read_only = false;
                    self.save(ui, tab, then);
                    false
                }
                Some(FileDialogResult::Cancelled) => false,
                None => true,
            },
            Modal::ConfirmClose { tab, then } => {
                let (tab, then) = (*tab, *then);
                let text = format!("Do you want to save the changes you made to {}?", self.tabs[tab].title);
                match ui.message_box("Save changes?", &text, &["Save", "Don't Save", "Cancel"]) {
                    Some(0) => {
                        self.save(ui, tab, then);
                        false
                    }
                    Some(1) => {
                        match then {
                            Then::Quit => {
                                self.remove_tab(tab);
                                self.request_quit(ui);
                            }
                            other => self.after(ui, other),
                        }
                        false
                    }
                    Some(_) => false,
                    None => true,
                }
            }
            Modal::GotoLine(text) => {
                let lines = self.tabs[self.active].doc.buffer().line_count();
                let mut outcome = None;
                if ui.input.key(keys::ESC) {
                    outcome = Some(false);
                }
                ui.modal(400, 176, |ui, card| {
                    let inner = card.inset(22, 18, 22, 18);
                    ui.heading(Rect::new(inner.x, inner.y, inner.w, 26), "Go to Line");
                    ui.focus_text_input("goto-line");
                    let hint = format!("Line number (1\u{2013}{lines})");
                    let resp = ui.text_input(Rect::new(inner.x, inner.y + 40, inner.w, 36), "goto-line", text, &hint);
                    let by = inner.bottom() - 34;
                    if ui.primary_button(Rect::new(inner.right() - 96, by, 96, 34), "Go") || resp.submitted {
                        outcome = Some(true);
                    }
                    if ui.button(Rect::new(inner.right() - 200, by, 96, 34), "Cancel") {
                        outcome = Some(false);
                    }
                });
                match outcome {
                    Some(true) => {
                        if let Ok(n) = text.trim().parse::<usize>() {
                            let tab = self.tab();
                            tab.doc.goto_line(n);
                            tab.view.reveal_cursor();
                        }
                        TextView::focus(ui);
                        false
                    }
                    Some(false) => {
                        TextView::focus(ui);
                        false
                    }
                    None => true,
                }
            }
            Modal::Message { title, text } => ui.message_box(title, text, &["OK"]).is_none(),
            Modal::About => ui
                .message_box(
                    "Text Editor",
                    "Vindows Text Editor 0.1 \u{2014} tabs, syntax highlighting for Rust, C/C++, TOML and Markdown, find and replace, word wrap and unlimited undo.",
                    &["OK"],
                )
                .is_none(),
            Modal::Shortcuts => {
                let mut done = ui.input.key(keys::ESC);
                let t = ui.theme().clone();
                ui.modal(560, 520, |ui, card| {
                    let inner = card.inset(24, 18, 24, 18);
                    ui.heading(Rect::new(inner.x, inner.y, inner.w, 26), "Keyboard Shortcuts");
                    for (i, (keys, what)) in SHORTCUTS.iter().enumerate() {
                        let y = inner.y + 40 + i as i32 * 22;
                        ui.label(Rect::new(inner.x, y, 200, 22), keys, Font::Mono, 12.5, t.accent_hover, Align::Left);
                        ui.label(Rect::new(inner.x + 210, y, inner.w - 210, 22), what, Font::Regular, 13.0, t.text, Align::Left);
                    }
                    if ui.primary_button(Rect::new(inner.right() - 96, inner.bottom() - 34, 96, 34), "Close") {
                        done = true;
                    }
                });
                !done
            }
        };
        if keep && self.modal.is_none() {
            self.modal = Some(modal);
        }
    }
}

impl App for Editor {
    fn update(&mut self, ui: &mut Ui) {
        let t = ui.theme().clone();
        let (w, h) = (ui.width, ui.height);
        if self.first_frame {
            TextView::focus(ui);
            self.first_frame = false;
        }
        if self.modal.is_none() {
            self.shortcuts(ui);
        }

        // Menu bar.
        let mb = Rect::new(0, 0, w, 32);
        ui.canvas.fill_rect(mb, t.surface);
        let menus = self.menus();
        let chosen = ui.menu_bar(mb, &menus);

        // Tabs, find bar, text and status bar.
        let tabs_r = Rect::new(0, mb.bottom(), w, 40);
        self.tab_strip(ui, tabs_r);
        let status = Rect::new(0, h - 28, w, 28);
        let find_h = match &self.find {
            Some(f) if f.show_replace && !self.tabs[self.active].read_only => 86,
            Some(_) => 46,
            None => 0,
        };
        let find_r = Rect::new(0, tabs_r.bottom(), w, find_h);
        if find_h > 0 {
            self.find_bar(ui, find_r);
        }
        let editor_r = Rect::new(0, find_r.bottom(), w, status.y - find_r.bottom());
        let empty: Vec<(Pos, Pos)> = Vec::new();
        let opts = self.opts;
        let Editor { tabs, active, find, .. } = self;
        let matches =
            find.as_ref().filter(|f| f.key.as_ref().is_some_and(|k| k.0 == *active)).map_or(&empty, |f| &f.matches);
        let tab = &mut tabs[*active];
        tab.view.update(ui, editor_r, &mut tab.doc, tab.lang, opts, tab.read_only, matches);
        self.status_bar(ui, status);
        self.draw_toast(ui, status.y);

        if let Some((m, i)) = chosen {
            self.menu_action(ui, m, i);
        }
        self.modal(ui);

        // Window title: file name, a dot when modified.
        let tab = &self.tabs[self.active];
        let dot = if tab.doc.is_modified() { "\u{2022} " } else { "" };
        let title = format!("{dot}{} \u{2014} Text Editor", tab.title);
        if title != self.title {
            let _ = ui.ctx.display.set_title(ui.ctx.window_id, title.clone());
            self.title = title;
        }
    }

    fn close_requested(&mut self) -> bool {
        if self.quitting {
            return true;
        }
        match self.tabs.iter().position(|t| t.doc.is_modified()) {
            None => true,
            Some(i) => {
                if self.modal.is_none() {
                    self.active = i;
                    self.modal = Some(Modal::ConfirmClose { tab: i, then: Then::Quit });
                }
                false
            }
        }
    }
}

fn main() -> i32 {
    let args = vrt::env::args();
    let editor = Editor::new(args.get(1..).unwrap_or(&[]));
    let mut spec = WindowSpec::new("Text Editor", 980, 680);
    spec.min_width = 480;
    spec.min_height = 320;
    spec.app_id = "editor".into();
    vui::run(spec, editor)
}
