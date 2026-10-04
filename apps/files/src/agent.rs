//! Files for the voice agent: what the window shows (the folder and its
//! items, the selection, how they are listed, open dialogs) and what it does
//! (showing folders, opening, selecting and filtering items, making folders,
//! and renaming, moving, copying and deleting), through the same operations
//! as the mouse and the keyboard.
//!
//! Renaming, moving, copying and deleting wait for the user's consent, so
//! they may run minutes after the agent asked for them. They name their
//! items by full path (a bare name would mean whichever folder is shown by
//! then), every item is looked up again when the action runs, and they only
//! change items in the home folder or in `/tmp`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vfiles::HOME;
use vfiles::format::{format_time, human_size};
use vfiles::kind::{EDITOR, default_app, kind_name};
use vfiles::path::{extension, file_name, file_stem, is_within, join, parent, resolve};
use vui::agent::{
    self, Action, AppAgentInfo, Risk, Value, arg_bool, arg_opt_str, arg_path, arg_str, clip, object, show_path,
};

use crate::{Dialog, Entry, Files, Props, Rename, SortKey, ViewMode, items, space_text};

/// The most items of a folder (or of a list in a result) the agent gets at
/// once.
const MAX_ITEMS: usize = 60;
/// The other place, besides the home folder, where the agent may change
/// items.
const TMP: &str = "/tmp";

pub fn info() -> AppAgentInfo {
    agent::info(
        "The file manager: it shows the folders and files of the computer and opens them, makes folders, and \
         renames, moves, copies and deletes files and folders.",
        vec![
            Action::new("open_folder", "Shows a folder (given a file, shows its folder with the file selected)")
                .param("path", "string", "The folder, such as ~/Pictures, ~ for the home folder, or /tmp", true)
                .build(),
            Action::new(
                "go",
                "Goes back or forward through the folders shown before, or up to the folder containing this one",
            )
            .choice("to", "Where to go", true, &["back", "forward", "up"])
            .build(),
            Action::new(
                "open",
                "Opens an item: a folder is shown, a file opens in its app (pictures in Photos, text in the Text \
                 Editor, music in Music)",
            )
            .param("item", "string", "A name in the folder shown, or a path", true)
            .param("in_text_editor", "boolean", "Open the file in the Text Editor, whatever its type", false)
            .build(),
            Action::new("select", "Selects items of the folder shown by name, or all of them, or none")
                .param("names", "string", "Names of items in the folder shown, one per line (none if not given)", false)
                .param("all", "boolean", "Select every item shown instead", false)
                .build(),
            Action::new(
                "search",
                "Shows only the items of the folder shown whose names contain a text (subfolders are not searched)",
            )
            .param("text", "string", "Part of a name; empty shows every item again", true)
            .build(),
            Action::new("view", "Changes how the items are listed")
                .choice("layout", "A detailed list, or icons with thumbnails of pictures", false, &["list", "icons"])
                .choice(
                    "sort_by",
                    "What to sort by: names and types go A to Z, sizes and dates largest and newest first, unless \
                     'order' says otherwise",
                    false,
                    &["name", "size", "type", "date"],
                )
                .choice("order", "The sort order", false, &["ascending", "descending"])
                .param("show_hidden", "boolean", "Also show hidden items (names starting with a dot)", false)
                .build(),
            Action::new("new_folder", "Makes a new folder and selects it")
                .param("name", "string", "The folder's name", true)
                .param("in", "string", "The folder to make it in (the folder shown by default)", false)
                .build(),
            Action::new(
                "properties",
                "Shows an item's properties and returns them: its type, size (for a folder, of everything in it) \
                 and when it was changed",
            )
            .param("item", "string", "A name in the folder shown, or a path (the folder shown if not given)", false)
            .build(),
            Action::new("rename", "Renames a file or folder")
                .param("path", "string", "The item's full path, such as ~/Documents/notes.txt", true)
                .param("new_name", "string", "The new name alone, such as ideas.txt (nothing is ever replaced)", true)
                .risk(Risk::Sensitive)
                .build(),
            Action::new("move", "Moves items into another folder")
                .param("paths", "string", "The items' full paths, one per line", true)
                .param(
                    "to",
                    "string",
                    "The folder, such as ~/Documents; an item whose name is taken there gets a new one, such as \
                     \"notes (2).txt\"",
                    true,
                )
                .risk(Risk::Sensitive)
                .build(),
            Action::new("copy", "Copies items into another folder")
                .param("paths", "string", "The items' full paths, one per line", true)
                .param(
                    "to",
                    "string",
                    "The folder, such as ~/Documents; a copy whose name is taken there gets a new one, such as \
                     \"notes (2).txt\"",
                    true,
                )
                .risk(Risk::Sensitive)
                .build(),
            Action::new("delete", "Deletes files or folders for good")
                .param(
                    "paths",
                    "string",
                    "The items' full paths, one per line. A folder goes with everything in it; there is no recycle \
                     bin to restore anything from.",
                    true,
                )
                .risk(Risk::Destructive)
                .build(),
        ],
    )
}

/// How an item's size reads in the list ("1.2 MiB", "3 items").
fn size_text(e: &Entry) -> String {
    match (e.is_dir, e.size) {
        (true, 0) => "empty".into(),
        (true, n) => items(n as usize),
        (false, n) => human_size(n),
    }
}

/// The names of the selected items, in the order shown.
fn selected_names(f: &Files) -> Vec<&str> {
    let mut names: Vec<&str> =
        f.view.iter().map(|&i| f.entries[i].name.as_str()).filter(|n| f.selected.contains(*n)).collect();
    // Selected items the search hides come last.
    for n in &f.selected {
        if !names.contains(&n.as_str()) {
            names.push(n);
        }
    }
    names.truncate(MAX_ITEMS);
    names
}

fn sort_name(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Kind => "type",
        SortKey::Modified => "date",
    }
}

/// What the Properties dialog says.
fn properties(p: &Props) -> Value {
    object! {
        "name" => p.name.as_str(),
        "path" => show_path(&p.path),
        "kind" => p.kind.as_str(),
        "size" => p.size.as_str(),
        "contains" => p.contents.as_deref(),
        "modified" => p.modified.as_str(),
        "read_only" => p.read_only,
    }
}

/// The dialog in front of the window.
fn dialog(d: &Dialog) -> Value {
    match d {
        Dialog::ConfirmDelete(paths) => object! {
            "asks_to_delete" => paths.iter().take(MAX_ITEMS).map(|p| show_path(p)).collect::<Vec<_>>(),
        },
        Dialog::Error { title, message } => object! { "error" => title.as_str(), "message" => clip(message, 600).0 },
        Dialog::Properties(p) => object! { "properties" => properties(p) },
        Dialog::OpenUnknown(path) => object! { "no_app_for" => show_path(path) },
    }
}

pub fn state(f: &Files) -> Value {
    let listed: Vec<Value> = f
        .view
        .iter()
        .take(MAX_ITEMS)
        .map(|&i| {
            let e = &f.entries[i];
            object! {
                "name" => e.name.as_str(),
                "kind" => kind_name(&e.name, e.is_dir),
                "size" => size_text(e),
                "modified" => format_time(e.modified),
            }
        })
        .collect();
    let mut v = object! {
        "folder" => show_path(&f.cwd),
        "items" => listed,
        "item_count" => f.view.len(),
        "selected" => selected_names(f),
        "layout" => if f.mode == ViewMode::List { "list" } else { "icons" },
        "sort_by" => sort_name(f.sort),
        "order" => if f.ascending { "ascending" } else { "descending" },
        "show_hidden" => f.show_hidden,
        "can_go_back" => !f.back.is_empty(),
        "can_go_forward" => !f.forward.is_empty(),
    };
    if f.view.len() > MAX_ITEMS {
        v.set("items_cut_short", true);
    }
    if !f.search.is_empty() {
        v.set("search", f.search.as_str());
    }
    if let Some(e) = &f.list_error {
        v.set("error", e.as_str());
    }
    if f.read_only() {
        v.set("read_only", true);
    }
    if let Some(s) = &f.space {
        v.set("free_space", space_text(s));
    }
    if let Some((paths, cut)) = &f.clipboard {
        let paths: Vec<String> = paths.iter().take(MAX_ITEMS).map(|p| show_path(p)).collect();
        v.set("clipboard", object! { "paths" => paths, "cut" => *cut });
    }
    if let Some(r) = &f.rename {
        v.set("renaming", r.original.as_str());
    }
    if let Some(d) = &f.dialog {
        v.set("dialog", dialog(d));
    }
    v
}

/// A list argument: one item per line (or a JSON array of strings).
fn list_arg(args: &Value, key: &str) -> Vec<String> {
    let lines: Vec<&str> = match args.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => s.lines().collect(),
        _ => Vec::new(),
    };
    lines.into_iter().map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// A path in an action the user approves. Bare names are refused: they
/// would mean whichever folder is shown when the user answers.
fn explicit_path(p: &str, shown: &str) -> Result<String, String> {
    let p = p.trim();
    if !p.contains('/') && !p.starts_with('~') {
        return Err(format!("give the full path of \u{201c}{p}\u{201d}, such as {}", show_path(&join(shown, p))));
    }
    let path = resolve(HOME, p);
    if is_within(&path, "/home/.private") {
        return Err("that location is private".into());
    }
    Ok(path)
}

/// Whether the agent may rename, move or delete `path`: items in the home
/// folder or in `/tmp`, not those folders themselves.
fn changeable(path: &str) -> bool {
    (is_within(path, HOME) && path != HOME) || (is_within(path, TMP) && path != TMP)
}

/// Whether the agent may move or copy items into the folder `dir`.
fn destination_allowed(dir: &str) -> bool {
    is_within(dir, HOME) || is_within(dir, TMP)
}

/// Refuses what cannot be the name of one item.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the name is empty".into());
    }
    if name.contains('/') || name == "." || name == ".." {
        return Err(format!("\u{201c}{name}\u{201d} is not a valid name: names cannot contain \u{201c}/\u{201d}"));
    }
    Ok(())
}

impl Files {
    /// The item of the current folder called `name`: exactly, else ignoring
    /// case, else ignoring case and extension (when only one item matches).
    fn find_name(&self, name: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        if let Some(e) = self.entries.iter().find(|e| e.name == name) {
            return Some(e.name.clone());
        }
        let lower = name.to_lowercase();
        let only = |found: Vec<&Entry>| if found.len() == 1 { Some(found[0].name.clone()) } else { None };
        only(self.entries.iter().filter(|e| e.name.to_lowercase() == lower).collect())
            .or_else(|| only(self.entries.iter().filter(|e| file_stem(&e.name).to_lowercase() == lower).collect()))
    }

    /// The item an action names: a name in the current folder, or a path. It
    /// must exist.
    fn item_arg(&self, args: &Value, key: &str) -> Result<String, String> {
        let s = arg_str(args, key)?.trim();
        if s.contains('/') || s.starts_with('~') {
            let path = arg_path(args, key)?;
            return if self.fs.exists(&path) { Ok(path) } else { Err(format!("there is no {}", show_path(&path))) };
        }
        self.find_name(s)
            .map(|n| self.path_of(&n))
            .ok_or_else(|| format!("there is nothing called \u{201c}{s}\u{201d} in {}", show_path(&self.cwd)))
    }

    /// An item an approved action works on: named by full path and still
    /// there; an item the action `changes` must also be the agent's to
    /// change.
    fn target(&self, p: &str, changes: bool) -> Result<String, String> {
        let path = explicit_path(p, &self.cwd)?;
        if changes && !changeable(&path) {
            return Err(format!(
                "{} cannot be changed: only items in the home folder or in /tmp can",
                show_path(&path)
            ));
        }
        if !self.fs.exists(&path) {
            return Err(format!("there is no {} (nothing was changed)", show_path(&path)));
        }
        Ok(path)
    }

    /// The items of an approved action (see [`Files::target`]), one per
    /// line.
    fn targets(&self, args: &Value, key: &str, changes: bool) -> Result<Vec<String>, String> {
        let list = list_arg(args, key);
        if list.is_empty() {
            return Err(format!("which items? give their full paths in '{key}', one per line"));
        }
        let mut paths = Vec::new();
        for p in &list {
            let path = self.target(p, changes)?;
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// An error the last operation reported in a dialog becomes the agent's
    /// answer instead (the agent tells the user).
    fn failed(&mut self) -> Result<(), String> {
        match self.dialog.take() {
            Some(Dialog::Error { title, message }) => Err(format!("{title}: {}", message.replace('\n', "; "))),
            other => {
                self.dialog = other;
                Ok(())
            }
        }
    }

    /// Shows the folder `path` (a file: its folder, with the file selected).
    fn show_folder(&mut self, path: &str) -> Result<(), String> {
        if self.navigate(path, true) {
            return Ok(());
        }
        self.failed()?;
        Err(format!("{} could not be shown", show_path(path)))
    }

    /// Where the window is now, for results.
    fn location(&self) -> Value {
        let mut v = object! { "showing" => show_path(&self.cwd), "items" => self.view.len() };
        let selected = selected_names(self);
        if !selected.is_empty() {
            v.set("selected", selected);
        }
        v
    }

    /// Selects the items called `names` with the cursor on the first, first
    /// showing hidden items or clearing the search if they hide any of them.
    fn select_names(&mut self, names: &[String]) {
        if names.iter().any(|n| n.starts_with('.')) {
            self.show_hidden = true;
        }
        let q = self.search.to_lowercase();
        if names.iter().any(|n| !n.to_lowercase().contains(&q)) {
            self.search.clear();
        }
        self.rebuild_view();
        self.selected = names.iter().cloned().collect();
        self.cursor = names.first().and_then(|n| self.view_index(n));
        self.anchor = self.cursor;
        self.reveal_cursor = true;
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        // Messages the agent's change would leave behind are closed first
        // (the agent acts for the user, who asked for something else).
        if matches!(self.dialog, Some(Dialog::Error { .. } | Dialog::Properties(_) | Dialog::OpenUnknown(_))) {
            self.dialog = None;
        }
        // The agent works with the folder as it is now, not as last listed.
        self.refresh();
        match action {
            "open_folder" => {
                let path = arg_path(args, "path")?;
                if !self.fs.exists(&path) {
                    return Err(format!("there is no {}", show_path(&path)));
                }
                self.show_folder(&path)?;
                Ok(self.location())
            }
            "go" => {
                match arg_str(args, "to")? {
                    "back" if self.back.is_empty() => return Err("there is no folder to go back to".into()),
                    "back" => self.go_back(),
                    "forward" if self.forward.is_empty() => return Err("there is no folder to go forward to".into()),
                    "forward" => self.go_forward(),
                    "up" if self.cwd == "/" => {
                        return Err("this is the top of the computer: no folder contains it".into());
                    }
                    "up" => self.go_up(),
                    other => return Err(format!("'to' cannot be {other}: say back, forward or up")),
                }
                self.failed()?;
                Ok(self.location())
            }
            "open" => {
                let path = self.item_arg(args, "item")?;
                let in_editor = arg_bool(args, "in_text_editor").unwrap_or(false);
                // An item of the folder shown is selected first, as by a
                // double click.
                if parent(&path) == self.cwd {
                    self.select_name(file_name(&path));
                }
                if self.fs.is_dir(&path) {
                    if in_editor {
                        return Err(format!("{} is a folder", show_path(&path)));
                    }
                    self.show_folder(&path)?;
                    return Ok(self.location());
                }
                if in_editor {
                    self.launch(EDITOR.exe, vec![path.clone()]);
                    self.failed()?;
                    return Ok(object! { "opened" => show_path(&path), "with" => EDITOR.id });
                }
                self.open_file(&path);
                if matches!(self.dialog, Some(Dialog::OpenUnknown(_))) {
                    self.dialog = None;
                    let ext = extension(&path);
                    let what = if ext.is_empty() { "this file".to_string() } else { format!(".{ext} files") };
                    return Err(format!(
                        "Veda has no app for {what}; if it is text, it can be opened with in_text_editor"
                    ));
                }
                self.failed()?;
                Ok(match default_app(&path) {
                    Some(app) => object! { "opened" => show_path(&path), "with" => app.id },
                    None => object! { "started" => show_path(&path) },
                })
            }
            "select" => {
                if arg_bool(args, "all") == Some(true) {
                    self.select_all();
                    return Ok(object! { "selected" => self.selected.len() });
                }
                let mut names: Vec<String> = Vec::new();
                for wanted in list_arg(args, "names") {
                    let found = match self.find_name(&wanted) {
                        Some(n) => vec![n],
                        None => {
                            // Several names on one line, separated by commas.
                            let parts: Vec<Option<String>> =
                                wanted.split(',').map(|p| self.find_name(p.trim())).collect();
                            if parts.len() < 2 || parts.iter().any(Option::is_none) {
                                return Err(format!(
                                    "there is nothing called \u{201c}{wanted}\u{201d} in {}",
                                    show_path(&self.cwd)
                                ));
                            }
                            parts.into_iter().flatten().collect()
                        }
                    };
                    for n in found {
                        if !names.contains(&n) {
                            names.push(n);
                        }
                    }
                }
                self.select_names(&names);
                Ok(object! { "selected" => names })
            }
            "search" => {
                self.search = arg_opt_str(args, "text").unwrap_or("").trim().to_string();
                self.rebuild_view();
                self.reset_scroll = true;
                let names: Vec<&str> =
                    self.view.iter().take(MAX_ITEMS).map(|&i| self.entries[i].name.as_str()).collect();
                Ok(object! { "search" => self.search.as_str(), "matches" => self.view.len(), "names" => names })
            }
            "view" => {
                if let Some(layout) = arg_opt_str(args, "layout") {
                    self.mode = match layout {
                        "list" => ViewMode::List,
                        "icons" | "grid" => ViewMode::Grid,
                        other => return Err(format!("'layout' cannot be {other}: say list or icons")),
                    };
                    self.reveal_cursor = true;
                }
                if let Some(by) = arg_opt_str(args, "sort_by") {
                    let key = match by {
                        "name" => SortKey::Name,
                        "size" => SortKey::Size,
                        "type" | "kind" => SortKey::Kind,
                        "date" | "modified" => SortKey::Modified,
                        other => return Err(format!("'sort_by' cannot be {other}: say name, size, type or date")),
                    };
                    // As a click on a column heading does.
                    self.sort = key;
                    self.ascending = matches!(key, SortKey::Name | SortKey::Kind);
                }
                if let Some(order) = arg_opt_str(args, "order") {
                    self.ascending = match order {
                        "ascending" => true,
                        "descending" => false,
                        other => return Err(format!("'order' cannot be {other}: say ascending or descending")),
                    };
                }
                if let Some(hidden) = arg_bool(args, "show_hidden") {
                    self.show_hidden = hidden;
                }
                self.rebuild_view();
                Ok(object! {
                    "layout" => if self.mode == ViewMode::List { "list" } else { "icons" },
                    "sort_by" => sort_name(self.sort),
                    "order" => if self.ascending { "ascending" } else { "descending" },
                    "show_hidden" => self.show_hidden,
                })
            }
            "new_folder" => {
                let name = arg_str(args, "name")?.trim();
                check_name(name)?;
                if arg_opt_str(args, "in").is_some() {
                    let dir = arg_path(args, "in")?;
                    if !self.fs.is_dir(&dir) {
                        return Err(format!("there is no folder {}", show_path(&dir)));
                    }
                    self.show_folder(&dir)?;
                }
                if self.read_only() {
                    return Err(format!("{} is part of the read-only system image", show_path(&self.cwd)));
                }
                let path = self.path_of(name);
                if self.fs.exists(&path) {
                    return Err(format!(
                        "there is already an item called \u{201c}{name}\u{201d} in {}",
                        show_path(&self.cwd)
                    ));
                }
                self.rename = None;
                if !self.make_folder(name) {
                    self.failed()?;
                    return Err(format!("{} could not be made", show_path(&path)));
                }
                Ok(object! { "created" => show_path(&path) })
            }
            "properties" => {
                let path = match arg_opt_str(args, "item") {
                    Some(_) => self.item_arg(args, "item")?,
                    None => self.cwd.clone(),
                };
                self.show_properties(&path);
                match &self.dialog {
                    Some(Dialog::Properties(p)) => Ok(properties(p)),
                    _ => Err(format!("the properties of {} could not be read", show_path(&path))),
                }
            }
            "rename" => {
                let path = self.target(arg_str(args, "path")?, true)?;
                let new = arg_str(args, "new_name")?.trim();
                check_name(new)?;
                let (dir, old) = (parent(&path).to_string(), file_name(&path).to_string());
                if new == old {
                    return Err(format!("it is already called {new}"));
                }
                let renamed = join(&dir, new);
                if self.fs.exists(&renamed) {
                    return Err(format!(
                        "there is already an item called \u{201c}{new}\u{201d} in {}",
                        show_path(&dir)
                    ));
                }
                // Renamed where it is, as the user would: its folder is shown.
                if dir != self.cwd {
                    self.show_folder(&dir)?;
                }
                self.rename = Some(Rename { original: old, text: new.to_string(), focused: true });
                self.commit_rename();
                self.failed()?;
                Ok(object! { "renamed" => show_path(&path), "now" => show_path(&renamed) })
            }
            "move" | "copy" => {
                let cut = action == "move";
                let verb = if cut { "moved" } else { "copied" };
                let paths = self.targets(args, "paths", cut)?;
                let dest = arg_path(args, "to")?;
                if !self.fs.is_dir(&dest) {
                    return Err(format!("there is no folder {}", show_path(&dest)));
                }
                if !destination_allowed(&dest) {
                    return Err(format!("items can only be {verb} into folders in the home folder or in /tmp"));
                }
                if let Some(p) = paths.iter().find(|p| is_within(&dest, p)) {
                    return Err(format!("{} cannot be {verb} into itself", show_path(p)));
                }
                if cut
                    && matches!(&self.dialog, Some(Dialog::ConfirmDelete(asked)) if asked.iter().any(|p| paths.contains(p)))
                {
                    self.dialog = None;
                }
                // As the user would: the folder is shown and the items are
                // pasted into it.
                if dest != self.cwd {
                    self.show_folder(&dest)?;
                }
                let (arrived, _) = self.paste_paths(&paths, cut);
                let problems = self.failed().err();
                if arrived.is_empty() {
                    return Err(problems.unwrap_or_else(|| format!("nothing could be {verb}")));
                }
                let arrived: Vec<String> =
                    arrived.iter().take(MAX_ITEMS).map(|n| show_path(&self.path_of(n))).collect();
                let mut v = object! { verb => arrived };
                if let Some(p) = problems {
                    v.set("problems", p);
                }
                Ok(v)
            }
            "delete" => {
                let paths = self.targets(args, "paths", true)?;
                // Folder by folder, as the user would: each folder is shown
                // and its items deleted there.
                let mut groups: Vec<(String, Vec<String>)> = Vec::new();
                for p in paths {
                    let dir = parent(&p).to_string();
                    match groups.iter_mut().find(|(d, _)| *d == dir) {
                        Some((_, group)) => group.push(p),
                        None => groups.push((dir, vec![p])),
                    }
                }
                let (mut deleted, mut problems) = (Vec::new(), Vec::new());
                for (dir, group) in groups {
                    if dir != self.cwd
                        && let Err(e) = self.show_folder(&dir)
                    {
                        problems.push(e);
                        continue;
                    }
                    // A confirmation the user has open for these items is
                    // answered by this.
                    if matches!(&self.dialog, Some(Dialog::ConfirmDelete(asked)) if asked.iter().any(|p| group.contains(p)))
                    {
                        self.dialog = None;
                    }
                    self.delete(group.clone());
                    if let Err(e) = self.failed() {
                        problems.push(e);
                    }
                    deleted.extend(group.iter().filter(|p| !self.fs.exists(p)).map(|p| show_path(p)));
                }
                if deleted.is_empty() {
                    return Err(problems.join("; "));
                }
                let mut v = object! { "deleted" => deleted };
                if !problems.is_empty() {
                    v.set("problems", problems.join("; "));
                }
                Ok(v)
            }
            other => Err(format!("Files has no action called {other}")),
        }
    }
}
