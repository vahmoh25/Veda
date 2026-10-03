//! Files: the Vindows file manager.
//!
//! Browses the virtual file system with a places sidebar, a breadcrumb
//! path bar with back/forward/up, a sortable list view and an icon grid
//! (with image thumbnails), a search filter, and the usual operations: open
//! with the default app, new folder or document, rename (inline), delete
//! (with confirmation), cut/copy/paste, properties. Everything works from
//! the keyboard as well as the mouse; `/system` is shown read-only.
//!
//! Usage: `files [PATH]` opens a folder, or the folder containing a file
//! with that file selected.

#![no_std]
#![no_main]

extern crate alloc;

mod fsutil;
mod icons;
mod thumbs;
mod view;

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use vproto::display::modifiers;
use vproto::init::{LaunchError, launcher};
use vproto::input::keys;
use vui::{App, Ui, WindowSpec};

use fsutil::{Fs, HOME, file_name, join};
use thumbs::Thumbnailer;

vrt::entry!(main);

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: u64,
}

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
}

/// A dialog on top of the window.
enum Dialog {
    /// Delete these paths?
    ConfirmDelete(Vec<String>),
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
    free_memory: Option<u64>,
    /// The sidebar place a context menu was opened for.
    context_target: Option<String>,
    drag: Option<Drag>,
    /// The folder under the pointer while dragging.
    drop_target: Option<String>,
    /// A press on an already selected item: select only it on release
    /// unless the press turns into a drag.
    pending_single: Option<usize>,
}

/// "1 item", "3 items".
fn items(n: usize) -> String {
    if n == 1 { "1 item".to_string() } else { format!("{n} items") }
}

/// Compares names in natural order ("file2" < "file10"), ignoring case.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = 0u64;
                while let Some(d) = ai.peek().and_then(|c| c.to_digit(10)) {
                    na = na.saturating_mul(10).saturating_add(d as u64);
                    ai.next();
                }
                let mut nb = 0u64;
                while let Some(d) = bi.peek().and_then(|c| c.to_digit(10)) {
                    nb = nb.saturating_mul(10).saturating_add(d as u64);
                    bi.next();
                }
                if na != nb {
                    return na.cmp(&nb);
                }
            }
            (Some(x), Some(y)) => {
                let (lx, ly) = (x.to_lowercase().next().unwrap_or(x), y.to_lowercase().next().unwrap_or(y));
                if lx != ly {
                    return lx.cmp(&ly);
                }
                ai.next();
                bi.next();
            }
        }
    }
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
            free_memory: None,
            context_target: None,
            drag: None,
            drop_target: None,
            pending_single: None,
        };
        let start = fsutil::resolve(HOME, start);
        match f.fs.stat(&start) {
            Ok(st) if st.is_dir => f.cwd = start,
            Ok(_) => {
                f.cwd = fsutil::parent(&start);
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
                self.entries = list
                    .into_iter()
                    .map(|e| Entry { name: e.name, is_dir: e.is_dir, size: e.size, modified: e.modified })
                    .collect();
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
        if let Ok(i) = vrt::object::system_info() {
            self.free_memory = Some(i.free_memory);
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
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let n = &self.entries[i].name;
                (self.show_hidden || !n.starts_with('.')) && (q.is_empty() || n.to_lowercase().contains(&q))
            })
            .collect();
        let (sort, asc, entries) = (self.sort, self.ascending, &self.entries);
        v.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            // Folders always come first.
            if ea.is_dir != eb.is_dir {
                return eb.is_dir.cmp(&ea.is_dir);
            }
            let ord = match sort {
                SortKey::Name => Ordering::Equal,
                SortKey::Size => ea.size.cmp(&eb.size),
                SortKey::Kind => fsutil::kind_name(&ea.name, ea.is_dir).cmp(&fsutil::kind_name(&eb.name, eb.is_dir)),
                SortKey::Modified => ea.modified.cmp(&eb.modified),
            }
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
        fsutil::is_read_only(&self.cwd)
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
        let path = fsutil::normalize(path);
        match self.fs.stat(&path) {
            Ok(st) if st.is_dir => {}
            Ok(_) => {
                // A file: show its folder with the file selected.
                let dir = fsutil::parent(&path);
                let name = file_name(&path).to_string();
                if self.navigate(&dir, record) {
                    self.select_name(&name);
                }
                return true;
            }
            Err(e) => {
                self.error("Can't open folder", format!("{}: {e}.", fsutil::display_path(&path)));
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
        self.reload();
        // Going up selects the folder we came from.
        if let Some(prev) = came_from
            && fsutil::parent(&prev) == self.cwd
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
        if self.cwd != "/" {
            let child = self.cwd.clone();
            let parent = fsutil::parent(&self.cwd);
            if self.navigate(&parent, true) {
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
        let ext = fsutil::extension(path);
        if let Some(app) = fsutil::default_app(path) {
            self.launch(app, vec![path.to_string()]);
        } else if ext == "exe" {
            if fsutil::is_read_only(path) {
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
            self.error(what, format!("{} is part of the read-only system image.", fsutil::display_path(&self.cwd)));
            return false;
        }
        true
    }

    fn new_folder(&mut self) {
        if !self.ensure_writable("Can't create folder") {
            return;
        }
        let name = fsutil::unique_name(&self.fs, &self.cwd, "New folder");
        match self.fs.mkdir(&self.path_of(&name)) {
            Ok(()) => {
                vrt::println!("created folder {}", self.path_of(&name));
                self.search.clear();
                self.reload();
                self.select_name(&name);
                self.start_rename();
            }
            Err(e) => self.error("Can't create folder", e.to_string()),
        }
    }

    fn new_document(&mut self) {
        if !self.ensure_writable("Can't create document") {
            return;
        }
        let name = fsutil::unique_name(&self.fs, &self.cwd, "New document.txt");
        match self.fs.write(&self.path_of(&name), b"") {
            Ok(()) => {
                vrt::println!("created document {}", self.path_of(&name));
                self.search.clear();
                self.reload();
                self.select_name(&name);
                self.start_rename();
            }
            Err(e) => self.error("Can't create document", e.to_string()),
        }
    }

    fn start_rename(&mut self) {
        if self.read_only() {
            self.error(
                "Can't rename",
                format!("{} is part of the read-only system image.", fsutil::display_path(&self.cwd)),
            );
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
            Err(e) => self.error("Can't rename", format!("{}: {e}.", r.original)),
        }
    }

    fn ask_delete(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        if !self.ensure_writable("Can't delete") {
            return;
        }
        self.dialog = Some(Dialog::ConfirmDelete(paths));
    }

    fn delete(&mut self, paths: Vec<String>) {
        let mut errors = Vec::new();
        for p in &paths {
            if let Err(e) = self.fs.remove_all(p) {
                errors.push(format!("{}: {e}", file_name(p)));
            }
            if let Some(t) = &mut self.thumbs {
                t.invalidate(p);
            }
        }
        vrt::println!("deleted {} from {}", items(paths.len() - errors.len()), self.cwd);
        let next = self.cursor;
        self.selected.clear();
        self.reload();
        if let Some(c) = next.filter(|_| !self.view.is_empty()) {
            self.select_only(c.min(self.view.len() - 1));
        }
        if !errors.is_empty() {
            self.error("Some items could not be deleted", errors.join("\n"));
        }
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
        // Also put the paths on the text clipboard (for the Terminal).
        let _ = vproto::connect(vproto::display::display::NAME)
            .ok()
            .map(|ch| vproto::display::display::Client::new(ch).set_clipboard(paths.join("\n")));
        self.clipboard = Some((paths, cut));
    }

    fn paste(&mut self) {
        let Some((paths, cut)) = self.clipboard.clone() else { return };
        if !self.ensure_writable("Can't paste") {
            return;
        }
        let mut errors = Vec::new();
        let mut pasted = Vec::new();
        for src in &paths {
            let name = file_name(src).to_string();
            if cut && fsutil::parent(src) == self.cwd {
                pasted.push(name);
                continue;
            }
            if fsutil::is_within(&self.cwd, src) {
                errors.push(format!("{name}: a folder can't be pasted into itself"));
                continue;
            }
            let target_name = fsutil::unique_name(&self.fs, &self.cwd, &name);
            let target = self.path_of(&target_name);
            let r = if cut { self.fs.move_to(src, &target) } else { self.fs.copy(src, &target) };
            match r {
                Ok(()) => pasted.push(target_name),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        if cut && errors.is_empty() {
            self.clipboard = None;
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
        if !errors.is_empty() {
            self.error("Some items could not be pasted", errors.join("\n"));
        }
    }

    /// Moves (or copies) dragged items into folder `target`.
    fn drop_into(&mut self, paths: Vec<String>, target: &str, copy: bool) {
        if fsutil::is_read_only(target) {
            self.error(
                "Can't drop here",
                format!("{} is part of the read-only system image.", fsutil::display_path(target)),
            );
            return;
        }
        let mut errors = Vec::new();
        let (mut moved, mut copied) = (0, 0);
        for src in &paths {
            let name = file_name(src).to_string();
            if src == target || fsutil::is_within(target, src) {
                errors.push(format!("{name}: a folder can't be moved into itself"));
                continue;
            }
            // Items from the read-only image can only be copied.
            let copy = copy || fsutil::is_read_only(src);
            if !copy && fsutil::parent(src) == target {
                continue;
            }
            let dst = join(target, &fsutil::unique_name(&self.fs, target, &name));
            let r = if copy { self.fs.copy(src, &dst) } else { self.fs.move_to(src, &dst) };
            match r {
                Ok(()) if copy => copied += 1,
                Ok(()) => moved += 1,
                Err(e) => errors.push(format!("{name}: {e}")),
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
        if !errors.is_empty() {
            self.error("Some items could not be moved", errors.join("\n"));
        }
    }

    fn show_properties(&mut self, path: &str) {
        let Ok(st) = self.fs.stat(path) else { return };
        let name = if path == "/" { "Computer".to_string() } else { file_name(path).to_string() };
        let (size, contents) = if st.is_dir {
            let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
            self.measure(path, &mut files, &mut dirs, &mut bytes, 0);
            (fsutil::human_size(bytes), Some(format!("{files} files, {dirs} folders")))
        } else {
            (format!("{} ({} bytes)", fsutil::human_size(st.size), st.size), None)
        };
        self.dialog = Some(Dialog::Properties(Props {
            kind: fsutil::kind_name(&name, st.is_dir),
            name,
            path: path.to_string(),
            size,
            contents,
            modified: fsutil::friendly_time(st.modified),
            read_only: st.read_only,
            is_dir: st.is_dir,
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
                keys::DELETE => self.ask_delete(),
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
            | (self.dialog.is_some() as u64) << 2);
        mix(self.back.len() as u64 | (self.forward.len() as u64) << 32);
        h
    }

    // ---- dialogs -----------------------------------------------------------

    fn dialogs(&mut self, ui: &mut Ui) {
        let Some(dialog) = self.dialog.take() else { return };
        let keep = match &dialog {
            Dialog::ConfirmDelete(paths) => {
                let (title, msg) = if paths.len() == 1 {
                    let name = file_name(&paths[0]);
                    let what = if self.fs.is_dir(&paths[0]) { "folder and everything in it" } else { "file" };
                    (format!("Delete “{name}”?"), format!("This permanently deletes the {what}. This can't be undone."))
                } else {
                    (
                        format!("Delete {} items?", paths.len()),
                        "This permanently deletes the selected items. This can't be undone.".to_string(),
                    )
                };
                match ui.message_box(&title, &msg, &["Delete", "Cancel"]) {
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
                let ext = fsutil::extension(path);
                let what = if ext.is_empty() { "this file".to_string() } else { format!("“.{ext}” files") };
                match ui.message_box(
                    "No app for this file",
                    &format!("Vindows doesn't have an app for {what}. Open “{}” in the Text Editor?", file_name(path)),
                    &["Open in Text Editor", "Cancel"],
                ) {
                    Some(0) => {
                        let p = path.clone();
                        self.launch("/system/bin/editor.exe", vec![p]);
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
                let before = self.entries.clone();
                self.reload();
                // Changed or new files may need a new thumbnail.
                if before != self.entries
                    && let Some(t) = &mut self.thumbs
                {
                    for e in self.entries.iter().filter(|e| !before.contains(e)) {
                        t.invalidate(&join(&self.cwd, &e.name));
                    }
                }
            }
            self.last_refresh = now;
        }
        self.was_focused = focused;
        ui.repaint_at(self.last_refresh + 3_000_000_000);
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
}

fn main() -> i32 {
    let args = vrt::env::args();
    let start = args.get(1).cloned().unwrap_or_else(|| HOME.to_string());
    let files = Files::new(&start);
    let mut spec = WindowSpec::new("Files", 940, 600);
    spec.app_id = "files".into();
    spec.min_width = 560;
    spec.min_height = 360;
    vui::run(spec, files)
}
