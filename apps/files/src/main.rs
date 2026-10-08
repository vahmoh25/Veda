//! Files: the Veda file manager.
//!
//! Browses the virtual file system with a places sidebar, a breadcrumb
//! path bar with back/forward/up, a sortable list view and an icon grid
//! (with image thumbnails), a search filter, and the usual operations: open
//! with the default app, new folder or document, rename (inline), move to
//! the Trash (with undo) or delete for good (with confirmation),
//! cut/copy/paste, properties. Everything works from the keyboard as well
//! as the mouse; `/system` is shown read-only.
//!
//! The Trash ([`vfiles::trash`]) is a place of its own: its items are
//! listed with where they were deleted from and when, and can be restored
//! there (or dragged anywhere else) or deleted for good, one by one or by
//! emptying the Trash. [`agent`] lets the voice agent do the same.
//!
//! Usage: `files [PATH]` opens a folder, or the folder containing a file
//! with that file selected; `files --empty-trash` opens the Trash and asks
//! whether to empty it.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod icons;
mod view;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use vfiles::format::{friendly_time, human_size};
use vfiles::fs::{Error, Space};
use vfiles::kind::{default_app, kind_name};
use vfiles::path::{
    display_path, extension, file_name, is_read_only, is_within, join, natural_cmp, normalize, parent, resolve,
};
use vfiles::thumbs::Thumbnailer;
use vfiles::{Fs, HOME, trash};
use vproto::display::modifiers;
use vproto::init::{LaunchError, launcher};
use vproto::input::keys;
use vui::{App, Ui, WindowSpec};

vrt::entry!(main);

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: u64,
    /// For an item in the Trash: where it was deleted from, if known.
    origin: Option<String>,
    /// For an item in the Trash: when it was deleted (0 if not known).
    deleted: u64,
}

impl Entry {
    /// The name shown: an item in the Trash has the name it was deleted
    /// with (its own may have a number added).
    fn shown_name(&self) -> &str {
        self.origin.as_deref().map(file_name).filter(|n| !n.is_empty()).unwrap_or(&self.name)
    }
}

/// Items moved to the Trash by the last delete, which Ctrl+Z (or the
/// notice's Undo) puts back.
struct Trashed {
    /// Each item's name in the Trash, and where it was.
    items: Vec<(String, String)>,
    /// When (`vrt::time::now_ns`).
    at: u64,
}

/// How long the notice of a move to the Trash stays.
const TRASHED_NOTICE_NS: u64 = 8_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Size,
    Kind,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    List,
    Grid,
}

/// Facts shown in the Properties dialog.
struct Props {
    name: String,
    path: String,
    kind: String,
    size: String,
    contents: Option<String>,
    modified: String,
    read_only: bool,
    is_dir: bool,
    /// For an item in the Trash: where it was deleted from and when.
    deleted_from: Option<(String, String)>,
}

/// A dialog on top of the window.
enum Dialog {
    /// Delete these paths for good?
    ConfirmDelete(Vec<String>),
    /// Empty the Trash (of this many items)?
    ConfirmEmpty(usize),
    /// These items do not fit in the Trash: delete them for good?
    NoRoomInTrash(Vec<String>),
    Error {
        title: String,
        message: String,
    },
    Properties(Props),
    /// No app for this file; offer the Text Editor.
    OpenUnknown(String),
}

/// Items being dragged with the mouse.
struct Drag {
    paths: Vec<String>,
    /// Where the button went down.
    origin: (i32, i32),
    /// The pointer moved far enough to count as a drag.
    active: bool,
}

/// An inline rename in progress.
struct Rename {
    original: String,
    text: String,
    /// Focus has been given to the text box.
    focused: bool,
}

struct Files {
    fs: Fs,
    launcher: Option<launcher::Client>,
    cwd: String,
    back: Vec<String>,
    forward: Vec<String>,
    entries: Vec<Entry>,
    list_error: Option<String>,
    /// Indices into `entries` of the visible items, in display order.
    view: Vec<usize>,
    selected: BTreeSet<String>,
    /// Keyboard cursor and range anchor (indices into `view`).
    cursor: Option<usize>,
    anchor: Option<usize>,
    sort: SortKey,
    ascending: bool,
    mode: ViewMode,
    show_hidden: bool,
    search: String,
    /// The path bar is being edited (text).
    path_edit: Option<String>,
    path_edit_focused: bool,
    rename: Option<Rename>,
    /// Copied or cut paths (`true` = cut).
    clipboard: Option<(Vec<String>, bool)>,
    dialog: Option<Dialog>,
    thumbs: Option<Thumbnailer>,
    last_refresh: u64,
    was_focused: bool,
    /// A text field had keyboard focus last frame.
    typing: bool,
    focus_search: bool,
    /// Type-ahead buffer and the time of the last key.
    type_ahead: (String, u64),
    /// Scroll the cursor into view on the next frame.
    reveal_cursor: bool,
    /// Scroll back to the top on the next frame.
    reset_scroll: bool,
    /// Grid columns in the last frame (for keyboard navigation).
    grid_cols: usize,
    /// Rows per page in the last frame.
    page_rows: usize,
    /// How full the file system of the current folder is.
    space: Option<Space>,
    /// The sidebar place a context menu was opened for.
    context_target: Option<String>,
    drag: Option<Drag>,
    /// The folder under the pointer while dragging.
    drop_target: Option<String>,
    /// A press on an already selected item: select only it on release
    /// unless the press turns into a drag.
    pending_single: Option<usize>,
    /// What is known about items in the Trash, by name and inode (an item
    /// trashed later under the same name is another file).
    trash_info: BTreeMap<(String, u64), Option<trash::Info>>,
    /// The last move to the Trash, for undo.
    trashed: Option<Trashed>,
    /// The sort order of other folders while the Trash, sorted by when its
    /// items were deleted, is shown.
    saved_sort: Option<(SortKey, bool)>,
}

/// "1 item", "3 items".
fn items(n: usize) -> String {
    if n == 1 { "1 item".to_string() } else { format!("{n} items") }
}

/// Where an item in the Trash was deleted from, as shown (`~/Documents`;
/// empty if not known).
fn location(e: &Entry) -> String {
    e.origin.as_deref().map(|o| display_path(parent(o))).unwrap_or_default()
}

/// "“notes.txt”" for one path, "3 items" for more.
fn what(paths: &[String]) -> String {
    match paths {
        [one] => format!("“{}”", file_name(one)),
        _ => items(paths.len()),
    }
}

/// "12.3 MiB free of 32.0 MiB", plus a note for file systems kept only in
/// memory.
pub fn space_text(s: &Space) -> String {
    let free = s.total.saturating_sub(s.used);
    let mut t = format!("{} free of {}", human_size(free), human_size(s.total));
    if !s.persistent {
        t.push_str(" (memory, not kept after a restart)");
    }
    t
}

impl Files {
    fn new(start: &str) -> Files {
        let fs = Fs::connect();
        let mut f = Files {
            fs,
            launcher: None,
            cwd: HOME.to_string(),
            back: Vec::new(),
            forward: Vec::new(),
            entries: Vec::new(),
            list_error: None,
            view: Vec::new(),
            selected: BTreeSet::new(),
            cursor: None,
            anchor: None,
            sort: SortKey::Name,
            ascending: true,
            mode: ViewMode::List,
            show_hidden: false,
            search: String::new(),
            path_edit: None,
            path_edit_focused: false,
            rename: None,
            clipboard: None,
            dialog: None,
            thumbs: Thumbnailer::new(96, 72),
            last_refresh: 0,
            was_focused: true,
            typing: false,
            focus_search: false,
            type_ahead: (String::new(), 0),
            reveal_cursor: false,
            reset_scroll: false,
            grid_cols: 1,
            page_rows: 10,
            space: None,
            context_target: None,
            drag: None,
            drop_target: None,
            pending_single: None,
            trash_info: BTreeMap::new(),
            trashed: None,
            saved_sort: None,
        };
        let start = resolve(HOME, start);
        if trash::contains(&start) {
            let _ = trash::ensure(&f.fs);
        }
        match f.fs.stat(&start) {
            Ok(st) if st.is_dir => f.cwd = start,
            Ok(_) => {
                f.cwd = parent(&start).to_string();
                let name = file_name(&start).to_string();
                f.reload();
                f.select_name(&name);
            }
            Err(e) => {
                f.dialog = Some(Dialog::Error {
                    title: "Can't open location".into(),
                    message: format!("{start}: {e}. Showing your home folder instead."),
                });
            }
        }
        f.sort_for_place();
        f.reload();
        if f.mode == ViewMode::List && f.cwd.starts_with("/home/user/Pictures") {
            f.mode = ViewMode::Grid;
        }
        f
    }

    fn launcher(&mut self) -> Option<&launcher::Client> {
        if self.launcher.is_none() {
            self.launcher = vproto::connect(launcher::NAME).ok().map(launcher::Client::new);
        }
        self.launcher.as_ref()
    }

    /// Shows an error dialog (and notes the error in the system log).
    fn error(&mut self, title: &str, message: String) {
        vrt::println!("{title}: {}", message.replace('\n', "; "));
        self.dialog = Some(Dialog::Error { title: title.into(), message });
    }

    /// Reports the items an operation could not handle: `failed` lists each
    /// item's name with its error. A full disk gets its own explanation.
    fn report(&mut self, title: &str, failed: Vec<(String, Error)>) {
        let Some((first, _)) = failed.first() else { return };
        if failed.iter().any(|(_, e)| e.is_no_space()) {
            let what = if failed.len() == 1 { format!("“{first}”") } else { items(failed.len()) };
            let space = self.fs.space(&self.cwd).map(|s| format!(" ({})", space_text(&s))).unwrap_or_default();
            let hint = if trash::count(&self.fs) > 0 {
                "Empty the Trash, or delete files you no longer need, and try again."
            } else {
                "Delete files you no longer need and try again."
            };
            self.error(
                "Not enough space",
                format!("There is not enough free space{space}, so {what} could not be saved. {hint}"),
            );
            return;
        }
        let lines: Vec<String> = failed.iter().map(|(name, e)| format!("{name}: {e}")).collect();
        self.error(title, lines.join("\n"));
    }

    // ---- listing -----------------------------------------------------------

    /// Name of the entry under the keyboard cursor.
    fn cursor_name(&self) -> Option<String> {
        self.cursor.and_then(|c| self.view.get(c)).and_then(|&i| self.entries.get(i)).map(|e| e.name.clone())
    }

    /// Re-reads the current folder, keeping the selection where possible.
    fn reload(&mut self) {
        let cursor = self.cursor_name();
        // The view indexes the old entries; it is rebuilt below.
        self.view.clear();
        match self.fs.read_dir(&self.cwd) {
            Ok(list) => {
                let trash_root = self.trash_root();
                let mut known = BTreeMap::new();
                self.entries = list
                    .into_iter()
                    .map(|e| {
                        let mut entry = Entry {
                            name: e.name,
                            is_dir: e.is_dir,
                            size: e.size,
                            modified: e.modified,
                            origin: None,
                            deleted: 0,
                        };
                        if trash_root {
                            let key = (entry.name.clone(), e.inode);
                            let info = match self.trash_info.remove(&key) {
                                Some(info) => info,
                                None => trash::info(&self.fs, &entry.name),
                            };
                            if let Some(i) = &info {
                                entry.origin = Some(i.origin.clone());
                                entry.deleted = i.deleted;
                            }
                            known.insert(key, info);
                        }
                        entry
                    })
                    .collect();
                // Only what is in the Trash now stays known.
                if trash_root {
                    self.trash_info = known;
                }
                self.list_error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.list_error = Some(e.to_string());
            }
        }
        let names: BTreeSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        self.rebuild_view_keeping(cursor);
        self.space = self.fs.space(&self.cwd).ok();
    }

    /// Re-reads the current folder; changed or new files get new
    /// thumbnails.
    fn refresh(&mut self) {
        let before = self.entries.clone();
        self.reload();
        if before != self.entries
            && let Some(t) = &mut self.thumbs
        {
            for e in self.entries.iter().filter(|e| !before.contains(e)) {
                t.invalidate(&join(&self.cwd, &e.name));
            }
        }
    }

    /// Applies the filter and the sort order.
    fn rebuild_view(&mut self) {
        let cursor = self.cursor_name();
        self.rebuild_view_keeping(cursor);
    }

    /// Rebuilds the view, keeping the cursor on the entry called `cursor_name`.
    fn rebuild_view_keeping(&mut self, cursor_name: Option<String>) {
        let q = self.search.to_lowercase();
        // Everything in the Trash is shown: hidden files are as deleted as
        // any.
        let trash_root = self.trash_root();
        let show_hidden = self.show_hidden || trash_root;
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let n = self.entries[i].shown_name();
                (show_hidden || !n.starts_with('.')) && (q.is_empty() || n.to_lowercase().contains(&q))
            })
            .collect();
        let (sort, asc, entries) = (self.sort, self.ascending, &self.entries);
        v.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            // Folders always come first, except in the Trash, which is
            // sorted as a list of what was deleted.
            if ea.is_dir != eb.is_dir && !trash_root {
                return eb.is_dir.cmp(&ea.is_dir);
            }
            // In the Trash, the type and date columns are where items were
            // deleted from and when.
            let ord = match sort {
                SortKey::Name => Ordering::Equal,
                SortKey::Size => ea.size.cmp(&eb.size),
                SortKey::Kind if trash_root => location(ea).cmp(&location(eb)),
                SortKey::Kind => kind_name(&ea.name, ea.is_dir).cmp(&kind_name(&eb.name, eb.is_dir)),
                SortKey::Modified if trash_root => ea.deleted.cmp(&eb.deleted),
                SortKey::Modified => ea.modified.cmp(&eb.modified),
            }
            .then_with(|| natural_cmp(ea.shown_name(), eb.shown_name()))
            .then_with(|| natural_cmp(&ea.name, &eb.name));
            if asc { ord } else { ord.reverse() }
        });
        self.view = v;
        self.cursor = cursor_name.and_then(|n| self.view_index(&n)).or(if self.view.is_empty() {
            None
        } else {
            self.cursor.map(|c| c.min(self.view.len() - 1))
        });
        self.anchor = self.anchor.filter(|&a| a < self.view.len());
    }

    fn view_index(&self, name: &str) -> Option<usize> {
        self.view.iter().position(|&i| self.entries[i].name == name)
    }

    fn entry(&self, vi: usize) -> Option<&Entry> {
        self.view.get(vi).map(|&i| &self.entries[i])
    }

    fn path_of(&self, name: &str) -> String {
        join(&self.cwd, name)
    }

    fn read_only(&self) -> bool {
        is_read_only(&self.cwd)
    }

    /// The Trash, or a folder in it, is shown.
    fn in_trash(&self) -> bool {
        trash::contains(&self.cwd)
    }

    /// The Trash itself (its items) is shown.
    fn trash_root(&self) -> bool {
        self.cwd == trash::FILES
    }

    /// Nothing contains the folder shown that can be gone up to: the
    /// computer, or the Trash.
    fn at_top(&self) -> bool {
        self.cwd == "/" || self.trash_root()
    }

    /// Items can be made, renamed and pasted here: not in the system image,
    /// and not in the Trash, whose items are restored before they change.
    fn can_change(&self) -> bool {
        !self.read_only() && !self.in_trash()
    }

    /// The Trash is sorted newest deletion first; other folders keep the
    /// order chosen for them.
    fn sort_for_place(&mut self) {
        if self.trash_root() {
            if self.saved_sort.is_none() {
                self.saved_sort = Some((self.sort, self.ascending));
                self.sort = SortKey::Modified;
                self.ascending = false;
            }
        } else if let Some((sort, ascending)) = self.saved_sort.take() {
            self.sort = sort;
            self.ascending = ascending;
        }
    }

    /// The name of the item at `path` before it went to the Trash (its own
    /// name for anything else).
    fn original_name(&self, path: &str) -> String {
        let name = file_name(path);
        if parent(path) != trash::FILES {
            return name.to_string();
        }
        let listed = self.entries.iter().find(|e| self.trash_root() && e.name == name);
        listed
            .map(|e| e.shown_name().to_string())
            .or_else(|| trash::info(&self.fs, name).map(|i| file_name(&i.origin).to_string()))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| name.to_string())
    }

    /// Selected paths in display order.
    fn selected_paths(&self) -> Vec<String> {
        self.view
            .iter()
            .map(|&i| &self.entries[i])
            .filter(|e| self.selected.contains(&e.name))
            .map(|e| self.path_of(&e.name))
            .collect()
    }

    // ---- selection ---------------------------------------------------------

    fn select_only(&mut self, vi: usize) {
        self.selected.clear();
        if let Some(e) = self.entry(vi) {
            let n = e.name.clone();
            self.selected.insert(n);
        }
        self.cursor = Some(vi);
        self.anchor = Some(vi);
    }

    fn toggle(&mut self, vi: usize) {
        if let Some(e) = self.entry(vi) {
            let n = e.name.clone();
            if !self.selected.remove(&n) {
                self.selected.insert(n);
            }
        }
        self.cursor = Some(vi);
        self.anchor = Some(vi);
    }

    fn select_range(&mut self, to: usize) {
        let from = self.anchor.unwrap_or(to);
        self.selected.clear();
        for vi in from.min(to)..=from.max(to) {
            if let Some(e) = self.entry(vi) {
                let n = e.name.clone();
                self.selected.insert(n);
            }
        }
        self.cursor = Some(to);
    }

    fn select_all(&mut self) {
        self.selected = self.view.iter().map(|&i| self.entries[i].name.clone()).collect();
    }

    fn select_name(&mut self, name: &str) {
        if let Some(vi) = self.view_index(name) {
            self.select_only(vi);
            self.reveal_cursor = true;
        }
    }

    // ---- navigation --------------------------------------------------------

    /// Shows folder `path`; `record` adds the current folder to the history.
    fn navigate(&mut self, path: &str, record: bool) -> bool {
        let path = normalize(path);
        if path == trash::FILES {
            // Nothing has been deleted yet: the Trash is made now.
            let _ = trash::ensure(&self.fs);
        }
        match self.fs.stat(&path) {
            Ok(st) if st.is_dir => {}
            Ok(_) => {
                // A file: show its folder with the file selected.
                let dir = parent(&path);
                let name = file_name(&path).to_string();
                if self.navigate(dir, record) {
                    self.select_name(&name);
                }
                return true;
            }
            Err(e) => {
                self.error("Can't open folder", format!("{}: {e}.", display_path(&path)));
                return false;
            }
        }
        if path == self.cwd {
            self.reload();
            return true;
        }
        if record {
            self.back.push(core::mem::replace(&mut self.cwd, path.clone()));
            self.forward.clear();
        } else {
            self.cwd = path.clone();
        }
        let came_from = self.back.last().cloned();
        self.search.clear();
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.rename = None;
        self.path_edit = None;
        self.reset_scroll = true;
        if let Some(t) = &mut self.thumbs {
            t.cancel_queued();
        }
        self.sort_for_place();
        self.reload();
        // Going up selects the folder we came from.
        if let Some(prev) = came_from
            && parent(&prev) == self.cwd
            && record
        {
            let n = file_name(&prev).to_string();
            self.select_name(&n);
        }
        true
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back.pop() {
            let cur = self.cwd.clone();
            if self.navigate(&p, false) {
                self.forward.push(cur);
                let n = file_name(&self.forward.last().cloned().unwrap_or_default()).to_string();
                self.select_name(&n);
            }
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            let cur = self.cwd.clone();
            if self.navigate(&p, false) {
                self.back.push(cur);
            }
        }
    }

    fn go_up(&mut self) {
        if !self.at_top() {
            let child = self.cwd.clone();
            let up = parent(&self.cwd).to_string();
            if self.navigate(&up, true) {
                self.select_name(file_name(&child));
            }
        }
    }

    // ---- opening -----------------------------------------------------------

    fn launch(&mut self, exe: &str, args: Vec<String>) {
        let what = file_name(exe).trim_end_matches(".exe").to_string();
        let target = args.first().cloned();
        let r = match self.launcher() {
            Some(l) => l.launch(exe.into(), args),
            None => {
                self.error("Can't open", "The application launcher is not available.".into());
                return;
            }
        };
        match r {
            Ok(Ok(_)) => match target {
                Some(t) => vrt::println!("opened {t} with {what}"),
                None => vrt::println!("started {what}"),
            },
            Ok(Err(e)) => {
                let why = match e {
                    LaunchError::NotFound => "it is not installed",
                    LaunchError::BadImage => "it is not a valid program",
                    LaunchError::NoMemory => "there is not enough memory",
                    LaunchError::Denied => "permission was denied",
                    LaunchError::Failed => "it failed to start",
                };
                self.error("Can't open", format!("“{what}” could not be started: {why}."));
            }
            Err(_) => self.error("Can't open", "The application launcher is not responding.".into()),
        }
    }

    /// Opens a file with its default application.
    fn open_file(&mut self, path: &str) {
        let ext = extension(path);
        if let Some(app) = default_app(path) {
            self.launch(app.exe, vec![path.to_string()]);
        } else if ext == "exe" {
            if is_read_only(path) {
                self.launch(path, Vec::new());
            } else {
                self.error("Can't run program", "Only programs installed in /system/bin can be started.".into());
            }
        } else if ext == "app" && path.starts_with("/system/apps/") {
            let id = file_name(path).trim_end_matches(".app").to_string();
            let r = self.launcher().map(|l| l.launch_app(id, Vec::new()));
            if !matches!(r, Some(Ok(Ok(_)))) {
                self.error("Can't open", "The application could not be started.".into());
            }
        } else {
            self.dialog = Some(Dialog::OpenUnknown(path.to_string()));
        }
    }

    /// Opens the entry at view index `vi` (folders are entered).
    fn open_index(&mut self, vi: usize) {
        let Some(e) = self.entry(vi).cloned() else { return };
        let path = self.path_of(&e.name);
        if e.is_dir {
            self.navigate(&path, true);
        } else {
            self.open_file(&path);
        }
    }

    /// Opens the selection: a single folder is entered, files are opened.
    fn open_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            if let Some(c) = self.cursor {
                self.open_index(c);
            }
            return;
        }
        if paths.len() == 1 {
            if let Some(vi) = self.view_index(file_name(&paths[0])) {
                self.open_index(vi);
            }
            return;
        }
        for p in paths {
            if !self.fs.is_dir(&p) {
                self.open_file(&p);
            }
        }
    }

    fn open_terminal(&mut self, dir: &str) {
        self.launch("/system/bin/terminal.exe", vec![dir.to_string()]);
    }

    // ---- operations --------------------------------------------------------

    fn ensure_writable(&mut self, what: &str) -> bool {
        if self.read_only() {
            self.error(what, format!("{} is part of the read-only system image.", display_path(&self.cwd)));
            return false;
        }
        true
    }

    /// As [`Files::ensure_writable`], and refuses the Trash, where nothing
    /// is made, renamed or pasted.
    fn ensure_changeable(&mut self, what: &str) -> bool {
        if self.in_trash() {
            self.error(what, "Items in the Trash can't be changed. Restore them first.".into());
            return false;
        }
        self.ensure_writable(what)
    }

    fn new_folder(&mut self) {
        if !self.ensure_changeable("Can't create folder") {
            return;
        }
        let name = self.fs.unique_name(&self.cwd, "New folder");
        if self.make_folder(&name) {
            self.start_rename();
        }
    }

    /// Creates the folder `name` in the current folder and selects it;
    /// returns `false` (with the error reported) if it could not be made.
    fn make_folder(&mut self, name: &str) -> bool {
        match self.fs.mkdir(&self.path_of(name)) {
            Ok(()) => {
                vrt::println!("created folder {}", self.path_of(name));
                self.search.clear();
                self.reload();
                self.select_name(name);
                true
            }
            Err(e) => {
                self.report("Can't create folder", vec![(name.to_string(), e)]);
                false
            }
        }
    }

    fn new_document(&mut self) {
        if !self.ensure_changeable("Can't create document") {
            return;
        }
        let name = self.fs.unique_name(&self.cwd, "New document.txt");
        match self.fs.write(&self.path_of(&name), b"") {
            Ok(()) => {
                vrt::println!("created document {}", self.path_of(&name));
                self.search.clear();
                self.reload();
                self.select_name(&name);
                self.start_rename();
            }
            Err(e) => self.report("Can't create document", vec![(name, e)]),
        }
    }

    fn start_rename(&mut self) {
        if !self.ensure_changeable("Can't rename") {
            return;
        }
        let Some(c) = self.cursor else { return };
        let Some(e) = self.entry(c) else { return };
        let name = e.name.clone();
        self.select_only(c);
        self.reveal_cursor = true;
        self.rename = Some(Rename { original: name.clone(), text: name, focused: false });
    }

    fn commit_rename(&mut self) {
        let Some(r) = self.rename.take() else { return };
        let new = r.text.trim().to_string();
        if new == r.original || new.is_empty() {
            return;
        }
        if new.contains('/') || new == "." || new == ".." {
            self.error("Can't rename", format!("“{new}” is not a valid name. Names can't contain “/”."));
            return;
        }
        let (from, to) = (self.path_of(&r.original), self.path_of(&new));
        if self.fs.exists(&to) {
            self.error("Can't rename", format!("There is already an item called “{new}” in this folder."));
            return;
        }
        match self.fs.rename(&from, &to) {
            Ok(()) => {
                vrt::println!("renamed {from} to {new}");
                if let Some(t) = &mut self.thumbs {
                    t.invalidate(&from);
                }
                self.reload();
                self.select_name(&new);
            }
            Err(e) => self.report("Can't rename", vec![(r.original, e)]),
        }
    }

    /// Del: moves the selection to the Trash. With Shift (`permanently`),
    /// and for items in the Trash already, it is deleted for good once the
    /// user confirms.
    fn delete_selection(&mut self, permanently: bool) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        if !self.ensure_writable("Can't delete") {
            return;
        }
        if permanently || self.in_trash() {
            self.dialog = Some(Dialog::ConfirmDelete(paths));
        } else {
            self.move_to_trash(paths);
        }
    }

    /// Selects the item that took the place of the cursor's after items
    /// left the folder.
    fn after_removal(&mut self, cursor: Option<usize>) {
        self.selected.clear();
        self.reload();
        if let Some(c) = cursor.filter(|_| !self.view.is_empty()) {
            self.select_only(c.min(self.view.len() - 1));
        }
    }

    /// Moves `paths` to the Trash; what is too big for the room left is
    /// offered for deleting for good instead.
    fn move_to_trash(&mut self, paths: Vec<String>) {
        let (mut moved, mut errors, mut no_room) = (Vec::new(), Vec::new(), Vec::new());
        for p in &paths {
            match trash::put(&self.fs, p) {
                Ok(name) => moved.push((name, p.clone())),
                Err(e) if e.is_no_space() => no_room.push(p.clone()),
                Err(e) => errors.push((file_name(p).to_string(), e)),
            }
            if let Some(t) = &mut self.thumbs {
                t.invalidate(p);
            }
        }
        if !moved.is_empty() {
            vrt::println!("moved {} from {} to the Trash", items(moved.len()), self.cwd);
            self.trashed = Some(Trashed { items: moved, at: vrt::time::now_ns() });
        }
        self.after_removal(self.cursor);
        if !no_room.is_empty() {
            self.dialog = Some(Dialog::NoRoomInTrash(no_room));
        }
        self.report("Some items could not be moved to the Trash", errors);
    }

    /// Puts back what the last move to the Trash took (Ctrl+Z), unless it
    /// has left the Trash since.
    fn undo_trash(&mut self) {
        let Some(t) = self.trashed.take() else { return };
        let (mut restored, mut errors) = (Vec::new(), Vec::new());
        for (name, origin) in &t.items {
            // The name may belong to another item by now.
            if !trash::info(&self.fs, name).is_some_and(|i| i.origin == *origin) {
                continue;
            }
            match trash::restore(&self.fs, name) {
                Ok(path) => restored.push(path),
                Err(e) => errors.push((file_name(origin).to_string(), e)),
            }
        }
        self.restored(restored, errors);
    }

    /// Restores the selected items of the Trash where they were deleted
    /// from (`all`: every item).
    fn restore(&mut self, all: bool) {
        if !self.trash_root() {
            return;
        }
        let names: Vec<String> = if all {
            self.entries.iter().map(|e| e.name.clone()).collect()
        } else {
            self.selected_paths().iter().map(|p| file_name(p).to_string()).collect()
        };
        self.restore_names(names);
    }

    /// Restores the items of the Trash called `names` (their names there);
    /// returns where they are now.
    fn restore_names(&mut self, names: Vec<String>) -> Vec<String> {
        let (mut restored, mut errors) = (Vec::new(), Vec::new());
        for name in names {
            match trash::restore(&self.fs, &name) {
                Ok(path) => restored.push(path),
                Err(e) => errors.push((self.original_name(&join(trash::FILES, &name)), e)),
            }
        }
        self.restored(restored.clone(), errors);
        restored
    }

    /// Reports items put back from the Trash (and those that could not be).
    fn restored(&mut self, restored: Vec<String>, errors: Vec<(String, Error)>) {
        if !restored.is_empty() {
            vrt::println!("restored {} from the Trash", items(restored.len()));
        }
        self.after_removal(self.cursor);
        // Items put back into the folder shown are selected.
        let here: Vec<&str> = restored.iter().filter(|p| parent(p) == self.cwd).map(|p| file_name(p)).collect();
        if let Some(first) = here.first() {
            self.cursor = self.view_index(first);
            self.anchor = self.cursor;
            self.reveal_cursor = true;
            self.selected = here.iter().map(|n| n.to_string()).collect();
        }
        self.report("Some items could not be restored", errors);
    }

    /// Empty Trash: asks first.
    fn ask_empty_trash(&mut self) {
        let n = trash::count(&self.fs);
        if n > 0 {
            self.dialog = Some(Dialog::ConfirmEmpty(n));
        }
    }

    /// Deletes everything in the Trash for good.
    fn empty_trash(&mut self) {
        match trash::empty(&self.fs) {
            Ok(n) => vrt::println!("emptied the Trash ({})", items(n)),
            Err(e) => self.report("The Trash could not be emptied", vec![("Trash".into(), e)]),
        }
        self.trashed = None;
        // A folder of the Trash that was shown is gone with it.
        if self.in_trash() && !self.trash_root() {
            self.navigate(trash::FILES, false);
        }
        self.after_removal(None);
    }

    /// Deletes `paths` for good (in the Trash: with what is known about
    /// them).
    fn delete(&mut self, paths: Vec<String>) {
        let mut errors = Vec::new();
        for p in &paths {
            let r = match trash::item_name(p) {
                Some(name) if parent(p) == trash::FILES => trash::delete(&self.fs, name),
                _ => self.fs.remove_all(p),
            };
            if let Err(e) = r {
                errors.push((self.original_name(p), e));
            }
            if let Some(t) = &mut self.thumbs {
                t.invalidate(p);
            }
        }
        vrt::println!("deleted {} from {}", items(paths.len() - errors.len()), self.cwd);
        self.after_removal(self.cursor);
        self.report("Some items could not be deleted", errors);
    }

    fn copy_selection(&mut self, cut: bool) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        if cut && self.read_only() {
            self.error("Can't cut", "Items in the read-only system image can only be copied.".into());
            return;
        }
        if cut && self.in_trash() {
            self.error(
                "Can't cut",
                "Items in the Trash can only be copied. Restore them, or drag them to a folder, to move them.".into(),
            );
            return;
        }
        // Also put the paths on the text clipboard (for the Terminal).
        let _ = vproto::connect(vproto::display::display::NAME)
            .ok()
            .map(|ch| vproto::display::display::Client::new(ch).set_clipboard(paths.join("\n")));
        self.clipboard = Some((paths, cut));
    }

    fn paste(&mut self) {
        let Some((paths, cut)) = self.clipboard.clone() else { return };
        if !self.ensure_changeable("Can't paste") {
            return;
        }
        let (_, failed) = self.paste_paths(&paths, cut);
        if cut && failed == 0 {
            self.clipboard = None;
        }
    }

    /// Copies, or moves (`cut`), `paths` into the current folder under free
    /// names, selects what arrived and reports what could not be pasted.
    /// Returns the names the items have here, and how many failed.
    fn paste_paths(&mut self, paths: &[String], cut: bool) -> (Vec<String>, usize) {
        let mut errors = Vec::new();
        let mut pasted = Vec::new();
        for src in paths {
            let name = file_name(src).to_string();
            if cut && parent(src) == self.cwd {
                pasted.push(name);
                continue;
            }
            if is_within(&self.cwd, src) {
                errors.push((name, Error::IntoItself));
                continue;
            }
            let dir = self.cwd.clone();
            match self.transfer(src, &dir, !cut) {
                Ok(target_name) => pasted.push(target_name),
                Err(e) => errors.push((name, e)),
            }
        }
        let verb = if cut { "moved" } else { "pasted" };
        vrt::println!("{verb} {} into {}", items(pasted.len()), self.cwd);
        self.search.clear();
        self.reload();
        self.selected = pasted.iter().cloned().collect();
        if let Some(first) = pasted.first() {
            self.cursor = self.view_index(first);
            self.anchor = self.cursor;
            self.reveal_cursor = true;
        }
        let failed = errors.len();
        self.report("Some items could not be pasted", errors);
        (pasted, failed)
    }

    /// Moves (or copies) `src` into the folder `dir` under a free name, and
    /// returns that name. An item leaving the Trash gets back the name it
    /// was deleted with, and what the Trash knew of it goes.
    fn transfer(&mut self, src: &str, dir: &str, copy: bool) -> Result<String, Error> {
        let base = self.original_name(src);
        if !copy && parent(src) == trash::FILES {
            return trash::restore_to(&self.fs, file_name(src), dir, &base).map(|p| file_name(&p).to_string());
        }
        let name = self.fs.unique_name(dir, &base);
        let dst = join(dir, &name);
        let r = if copy { self.fs.copy(src, &dst) } else { self.fs.move_to(src, &dst) };
        r.map(|()| name)
    }

    /// Moves (or copies) dragged items into folder `target`. Dropped on the
    /// Trash, they go to the Trash.
    fn drop_into(&mut self, paths: Vec<String>, target: &str, copy: bool) {
        if target == trash::FILES {
            let paths: Vec<String> = paths.into_iter().filter(|p| !trash::contains(p)).collect();
            if !paths.is_empty() {
                self.move_to_trash(paths);
            }
            return;
        }
        if trash::contains(target) {
            self.error("Can't drop here", "Items can't be put into folders in the Trash.".into());
            return;
        }
        if is_read_only(target) {
            self.error("Can't drop here", format!("{} is part of the read-only system image.", display_path(target)));
            return;
        }
        let mut errors = Vec::new();
        let (mut moved, mut copied) = (0, 0);
        for src in &paths {
            let name = file_name(src).to_string();
            if src == target || is_within(target, src) {
                errors.push((name, Error::IntoItself));
                continue;
            }
            // Items from the read-only image can only be copied.
            let copy = copy || is_read_only(src);
            if !copy && parent(src) == target {
                continue;
            }
            match self.transfer(src, target, copy) {
                Ok(_) if copy => copied += 1,
                Ok(_) => moved += 1,
                Err(e) => errors.push((name, e)),
            }
            if let Some(t) = &mut self.thumbs {
                t.invalidate(src);
            }
        }
        if moved > 0 {
            vrt::println!("moved {} into {target}", items(moved));
        }
        if copied > 0 {
            vrt::println!("copied {} into {target}", items(copied));
        }
        self.selected.clear();
        self.reload();
        self.report("Some items could not be moved", errors);
    }

    fn show_properties(&mut self, path: &str) {
        let Ok(st) = self.fs.stat(path) else { return };
        let name = match path {
            "/" => "Computer".to_string(),
            trash::FILES => "Trash".to_string(),
            _ => self.original_name(path),
        };
        let deleted_from = match trash::item_name(path) {
            Some(item) if parent(path) == trash::FILES => {
                trash::info(&self.fs, item).map(|i| (display_path(parent(&i.origin)), friendly_time(i.deleted)))
            }
            _ => None,
        };
        let (size, contents) = if st.is_dir {
            let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
            self.measure(path, &mut files, &mut dirs, &mut bytes, 0);
            (human_size(bytes), Some(format!("{files} files, {dirs} folders")))
        } else {
            (format!("{} ({} bytes)", human_size(st.size), st.size), None)
        };
        self.dialog = Some(Dialog::Properties(Props {
            kind: kind_name(&name, st.is_dir),
            name,
            path: path.to_string(),
            size,
            contents,
            modified: friendly_time(st.modified),
            read_only: st.read_only,
            is_dir: st.is_dir,
            deleted_from,
        }));
    }

    fn measure(&self, dir: &str, files: &mut u64, dirs: &mut u64, bytes: &mut u64, depth: u32) {
        if depth > 32 {
            return;
        }
        for e in self.fs.read_dir(dir).unwrap_or_default() {
            if e.is_dir {
                *dirs += 1;
                self.measure(&join(dir, &e.name), files, dirs, bytes, depth + 1);
            } else {
                *files += 1;
                *bytes += e.size;
            }
        }
    }

    // ---- keyboard ----------------------------------------------------------

    fn move_cursor(&mut self, to: usize, shift: bool) {
        if self.view.is_empty() {
            return;
        }
        let to = to.min(self.view.len() - 1);
        if shift {
            if self.anchor.is_none() {
                self.anchor = self.cursor.or(Some(to));
            }
            self.select_range(to);
        } else {
            self.select_only(to);
        }
        self.reveal_cursor = true;
    }

    fn handle_keys(&mut self, ui: &mut Ui) {
        let now = ui.now();
        let keys_now = ui.input.keys.clone();
        let n = self.view.len();
        let step_v = if self.mode == ViewMode::Grid { self.grid_cols.max(1) } else { 1 };
        for k in keys_now {
            let ctrl = k.modifiers & modifiers::CTRL != 0;
            let shift = k.modifiers & modifiers::SHIFT != 0;
            let alt = k.modifiers & modifiers::ALT != 0;
            let cur = self.cursor;
            match k.code {
                keys::LEFT if alt => self.go_back(),
                keys::RIGHT if alt => self.go_forward(),
                keys::UP if alt => self.go_up(),
                keys::DOWN if alt => self.open_selection(),
                keys::DOWN => self.move_cursor(cur.map_or(0, |c| c + step_v), shift),
                keys::UP => self.move_cursor(cur.map_or(0, |c| c.saturating_sub(step_v)), shift),
                keys::RIGHT if self.mode == ViewMode::Grid => self.move_cursor(cur.map_or(0, |c| c + 1), shift),
                keys::LEFT if self.mode == ViewMode::Grid => {
                    self.move_cursor(cur.map_or(0, |c| c.saturating_sub(1)), shift)
                }
                keys::HOME => self.move_cursor(0, shift),
                keys::END => self.move_cursor(n.saturating_sub(1), shift),
                keys::PAGEDOWN => self.move_cursor(cur.map_or(0, |c| c + self.page_rows * step_v), shift),
                keys::PAGEUP => self.move_cursor(cur.map_or(0, |c| c.saturating_sub(self.page_rows * step_v)), shift),
                keys::ENTER | keys::KPENTER => self.open_selection(),
                keys::BACKSPACE => self.go_up(),
                keys::DELETE => self.delete_selection(shift),
                keys::F2 => self.start_rename(),
                keys::F5 => self.reload(),
                keys::ESC => {
                    if !self.search.is_empty() {
                        self.search.clear();
                        self.rebuild_view();
                    } else {
                        self.selected.clear();
                    }
                }
                keys::A if ctrl => self.select_all(),
                keys::Z if ctrl => self.undo_trash(),
                keys::C if ctrl => self.copy_selection(false),
                keys::X if ctrl => self.copy_selection(true),
                keys::V if ctrl => self.paste(),
                keys::N if ctrl && shift => self.new_folder(),
                keys::N if ctrl => self.new_document(),
                keys::F if ctrl => self.focus_search = true,
                keys::L if ctrl => self.path_edit = Some(self.cwd.clone()),
                keys::H if ctrl => {
                    self.show_hidden = !self.show_hidden;
                    self.rebuild_view();
                }
                keys::KEY_1 if ctrl => self.mode = ViewMode::List,
                keys::KEY_2 if ctrl => self.mode = ViewMode::Grid,
                keys::T if ctrl => {
                    let d = self.cwd.clone();
                    self.open_terminal(&d);
                }
                _ => {}
            }
        }
        // Type-ahead: jump to the first item starting with the typed text.
        let typed: String = ui.input.text.chars().filter(|c| !c.is_control()).collect();
        if !typed.is_empty() && ui.input.modifiers & (modifiers::CTRL | modifiers::ALT) == 0 {
            if now.saturating_sub(self.type_ahead.1) > 1_000_000_000 {
                self.type_ahead.0.clear();
            }
            self.type_ahead.0.push_str(&typed.to_lowercase());
            self.type_ahead.1 = now;
            let prefix = self.type_ahead.0.clone();
            if let Some(vi) = self.view.iter().position(|&i| self.entries[i].name.to_lowercase().starts_with(&prefix)) {
                self.select_only(vi);
                self.reveal_cursor = true;
            }
        }
    }

    /// A cheap fingerprint of the visible state, to notice changes made
    /// while drawing (which parts drawn earlier in the frame do not show).
    fn signature(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x100_0000_01b3);
        };
        for b in self.cwd.bytes().chain(self.search.bytes()) {
            mix(b as u64);
        }
        mix(self.entries.len() as u64);
        mix(self.view.len() as u64);
        mix(self.selected.len() as u64);
        for n in &self.selected {
            mix(n.len() as u64);
        }
        mix(self.cursor.map_or(u64::MAX, |c| c as u64));
        mix(self.mode as u64);
        mix(self.sort as u64);
        mix(self.ascending as u64 | (self.show_hidden as u64) << 1 | (self.rename.is_some() as u64) << 2);
        mix((self.path_edit.is_some() as u64)
            | (self.clipboard.is_some() as u64) << 1
            | (self.dialog.is_some() as u64) << 2
            | (self.trashed.is_some() as u64) << 3);
        mix(self.back.len() as u64 | (self.forward.len() as u64) << 32);
        h
    }

    // ---- dialogs -----------------------------------------------------------

    fn dialogs(&mut self, ui: &mut Ui) {
        let Some(dialog) = self.dialog.take() else { return };
        let keep = match &dialog {
            Dialog::ConfirmDelete(paths) => {
                let (title, msg) = if paths.len() == 1 {
                    let name = self.original_name(&paths[0]);
                    let what = if self.fs.is_dir(&paths[0]) { "folder and everything in it" } else { "file" };
                    (
                        format!("Delete “{name}” permanently?"),
                        format!("This permanently deletes the {what}. This can't be undone."),
                    )
                } else {
                    (
                        format!("Delete {} items permanently?", paths.len()),
                        "This permanently deletes the selected items. This can't be undone.".to_string(),
                    )
                };
                match ui.destructive_box(&title, &msg, &["Delete", "Cancel"]) {
                    Some(0) => {
                        self.delete(paths.clone());
                        false
                    }
                    Some(_) => false,
                    None => true,
                }
            }
            Dialog::ConfirmEmpty(n) => {
                let msg = if *n == 1 {
                    "The item in the Trash will be deleted for good. This can't be undone.".to_string()
                } else {
                    format!("All {n} items in the Trash will be deleted for good. This can't be undone.")
                };
                match ui.destructive_box("Empty the Trash?", &msg, &["Empty Trash", "Cancel"]) {
                    Some(0) => {
                        self.empty_trash();
                        false
                    }
                    Some(_) => false,
                    None => true,
                }
            }
            Dialog::NoRoomInTrash(paths) => {
                let msg = format!(
                    "There isn't enough free space to keep {} in the Trash. Delete permanently instead? \
                     This can't be undone.",
                    what(paths)
                );
                match ui.destructive_box("Not enough space in the Trash", &msg, &["Delete", "Cancel"]) {
                    Some(0) => {
                        self.delete(paths.clone());
                        false
                    }
                    Some(_) => false,
                    None => true,
                }
            }
            Dialog::Error { title, message } => ui.message_box(title, message, &["OK"]).is_none(),
            Dialog::OpenUnknown(path) => {
                let ext = extension(path);
                let what = if ext.is_empty() { "this file".to_string() } else { format!("“.{ext}” files") };
                match ui.message_box(
                    "No app for this file",
                    &format!("Veda doesn't have an app for {what}. Open “{}” in the Text Editor?", file_name(path)),
                    &["Open in Text Editor", "Cancel"],
                ) {
                    Some(0) => {
                        let p = path.clone();
                        self.launch(vfiles::kind::EDITOR.exe, vec![p]);
                        false
                    }
                    Some(_) => false,
                    None => true,
                }
            }
            Dialog::Properties(p) => !view::properties_dialog(ui, p),
        };
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}

impl App for Files {
    fn update(&mut self, ui: &mut Ui) {
        let now = ui.now();
        // Refresh when the window regains focus and every few seconds.
        let focused = ui.input.focused;
        if (focused && !self.was_focused) || now.saturating_sub(self.last_refresh) > 3_000_000_000 {
            if self.rename.is_none() && self.dialog.is_none() {
                self.refresh();
            }
            self.last_refresh = now;
        }
        self.was_focused = focused;
        ui.repaint_at(self.last_refresh + 3_000_000_000);
        // The notice of a move to the Trash goes away by itself.
        if let Some(t) = &self.trashed
            && now < t.at + TRASHED_NOTICE_NS
        {
            ui.repaint_at(t.at + TRASHED_NOTICE_NS);
        }
        if let Some(t) = &mut self.thumbs {
            t.collect();
        }
        let dialog_at_start = self.dialog.is_some();
        let before = self.signature();
        // Mouse input (handled while drawing) comes first, so that a click
        // and a key arriving in the same frame act in the right order.
        view::draw(self, ui);
        if self.signature() != before {
            ui.repaint();
        }
        // Keys belong to a dialog or a focused text field. (While a menu is
        // open, vui gives it the keyboard and widgets see no keys.)
        if !dialog_at_start
            && !self.typing
            && self.rename.is_none()
            && self.path_edit.is_none()
            && (!ui.input.keys.is_empty() || !ui.input.text.is_empty())
        {
            self.handle_keys(ui);
            ui.repaint();
        }
        // A dialog opened this frame is shown from the next one, so the key
        // or click that opened it does not also answer it.
        if dialog_at_start {
            self.dialogs(ui);
        } else if self.dialog.is_some() {
            ui.repaint();
        }
    }

    fn wait_handles(&self) -> Vec<(vabi::RawHandle, u32)> {
        match &self.thumbs {
            Some(t) => vec![(t.event_handle(), vabi::signals::SIGNALED)],
            None => Vec::new(),
        }
    }

    fn agent_info(&self) -> Option<vui::agent::AppAgentInfo> {
        Some(agent::info())
    }

    fn agent_state(&self) -> vui::agent::Value {
        agent::state(self)
    }

    fn agent_invoke(&mut self, action: &str, args: &vui::agent::Value) -> Result<vui::agent::Value, String> {
        Files::agent_invoke(self, action, args)
    }
}

fn main() -> i32 {
    let args = vrt::env::args();
    let empty_trash = args.get(1).is_some_and(|a| a == "--empty-trash");
    let start = match args.get(1) {
        _ if empty_trash => trash::FILES.to_string(),
        Some(path) => path.clone(),
        None => HOME.to_string(),
    };
    let mut files = Files::new(&start);
    if empty_trash {
        files.ask_empty_trash();
    }
    let mut spec = WindowSpec::new("Files", 940, 600);
    spec.app_id = "files".into();
    spec.min_width = 560;
    spec.min_height = 360;
    vui::run(spec, files)
}
