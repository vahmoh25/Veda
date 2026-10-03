//! The Text Editor for the voice agent: what it shows (the open documents,
//! the current one's text, cursor and selection) and what it does
//! (writing, finding and replacing, undoing, saving), through the same
//! document operations as the keyboard and menus.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vfiles::path::file_name;
use vtext::highlight::Language;
use vtext::{Pos, Selection};
use vui::agent::{
    self, Action, AppAgentInfo, Risk, Value, arg_bool, arg_f64, arg_opt_int, arg_opt_str, arg_path, arg_str, clip,
    object, show_path,
};

use crate::{DEFAULT_FONT_SIZE, Editor, Modal, Tab, write_text};

/// The most text of a document the agent gets at once.
const MAX_TEXT: usize = 6000;

pub fn info() -> AppAgentInfo {
    agent::info(
        "A tabbed text editor for notes, letters and code: it writes, finds and replaces text, and opens and saves files.",
        vec![
            Action::new("new_document", "Opens a new empty document, optionally with text in it")
                .param("text", "string", "Text to start with", false)
                .build(),
            Action::new("open_file", "Opens a text file in a tab, or shows it if it is already open")
                .param("path", "string", "The file, such as ~/Documents/notes.txt", true)
                .build(),
            Action::new("write", "Writes text into the current document")
                .param("text", "string", "The text, with \\n between lines", true)
                .choice(
                    "where",
                    "Where it goes: at the cursor replacing any selection (the default), at the end, at the start, \
                     or instead of everything",
                    false,
                    &["cursor", "end", "start", "replace_all"],
                )
                .build(),
            Action::new("read", "Returns lines of the current document")
                .param("from_line", "integer", "The first line (1 by default)", false)
                .param("lines", "integer", "How many lines (as many as fit by default)", false)
                .build(),
            Action::new("find", "Selects where a text next appears after the cursor (wrapping around)")
                .param("text", "string", "What to look for", true)
                .param("case_sensitive", "boolean", "Match capital letters exactly", false)
                .build(),
            Action::new("replace", "Replaces a text in the current document, everywhere or just the next time")
                .param("find", "string", "The text to replace", true)
                .param("with", "string", "The new text", true)
                .param("all", "boolean", "Every occurrence (the default) or only the next one", false)
                .param("case_sensitive", "boolean", "Match capital letters exactly", false)
                .build(),
            Action::new("go_to_line", "Moves the cursor to the start of a line")
                .param("line", "integer", "The line number, from 1", true)
                .build(),
            Action::new("select", "Selects text: everything, or whole lines")
                .param("from_line", "integer", "The first line to select (everything if not given)", false)
                .param("to_line", "integer", "The last line to select (the first line by default)", false)
                .build(),
            Action::new("undo", "Undoes the latest changes")
                .param("times", "integer", "How many changes (1 by default)", false)
                .build(),
            Action::new("redo", "Redoes changes that were undone")
                .param("times", "integer", "How many changes (1 by default)", false)
                .build(),
            Action::new(
                "save",
                "Saves the current document to its file. An untitled document needs a path for a new file.",
            )
            .param("path", "string", "Where to save an untitled document (the file must not exist yet)", false)
            .build(),
            Action::new("save_as", "Saves the current document under another name, replacing any file there")
                .param("path", "string", "The new file", true)
                .risk(Risk::Sensitive)
                .build(),
            Action::new("switch_document", "Shows another open document")
                .param("name", "string", "Its title or file name", true)
                .build(),
            Action::new("close_document", "Closes a document that has no unsaved changes")
                .param("name", "string", "Its title or file name (the current document if not given)", false)
                .build(),
            Action::new("discard_changes", "Closes a document, throwing away its unsaved changes")
                .param("name", "string", "Its title or file name, as listed in the editor's documents", true)
                .risk(Risk::Destructive)
                .build(),
            Action::new("view", "Changes how documents look")
                .param("word_wrap", "boolean", "Wrap long lines", false)
                .param("line_numbers", "boolean", "Show line numbers", false)
                .param("font_size", "number", "Text size in points, 9 to 32 (14 is normal)", false)
                .build(),
        ],
    )
}

/// One document as the agent sees it in the list.
fn tab_summary(t: &Tab, current: bool) -> Value {
    object! {
        "title" => t.title.as_str(),
        "file" => t.path.as_deref().map(show_path),
        "unsaved_changes" => t.doc.is_modified(),
        "current" => current,
    }
}

pub fn state(ed: &Editor) -> Value {
    let tab = &ed.tabs[ed.active];
    let text = tab.doc.text();
    let (shown, truncated) = clip(&text, MAX_TEXT);
    let selected = tab.doc.selected_text();
    let (selected, _) = clip(&selected, 500);
    let cursor = tab.doc.cursor();
    let dialog = ed.modal.as_ref().map(|m| match m {
        Modal::Open(_) => "Open file",
        Modal::SaveAs { .. } => "Save as",
        Modal::ConfirmClose { .. } => "Save changes?",
        Modal::GotoLine(_) => "Go to line",
        Modal::Message { .. } => "Message",
        Modal::Shortcuts => "Keyboard shortcuts",
        Modal::About => "About",
    });
    object! {
        "documents" => Value::Array(ed.tabs.iter().enumerate().map(|(i, t)| tab_summary(t, i == ed.active)).collect()),
        "current" => object! {
            "title" => tab.title.as_str(),
            "file" => tab.path.as_deref().map(show_path),
            "language" => tab.lang.name(),
            "read_only" => tab.read_only,
            "unsaved_changes" => tab.doc.is_modified(),
            "lines" => tab.doc.buffer().line_count(),
            "cursor" => object! { "line" => cursor.line + 1, "column" => cursor.col + 1 },
            "selection" => selected,
            "text" => shown,
            "text_cut_short" => truncated,
        },
        "dialog" => dialog,
        "word_wrap" => ed.opts.wrap,
        "font_size" => ed.opts.font_size,
    }
}

/// How many times to repeat (1 to 1000).
fn times(args: &Value) -> Result<usize, String> {
    Ok(arg_opt_int(args, "times")?.unwrap_or(1).clamp(1, 1000) as usize)
}

/// A 1-based line number argument.
fn line_arg(ed: &Editor, args: &Value, name: &str) -> Result<Option<usize>, String> {
    let lines = ed.tabs[ed.active].doc.buffer().line_count() as i64;
    match arg_opt_int(args, name)? {
        None => Ok(None),
        Some(n) if n < 1 || n > lines => Err(format!("the document has lines 1 to {lines}")),
        Some(n) => Ok(Some(n as usize)),
    }
}

impl Editor {
    fn writable(&self) -> Result<(), String> {
        let tab = &self.tabs[self.active];
        if tab.read_only {
            return Err(format!("{} is read-only; save a copy with save_as to change it", tab.title));
        }
        Ok(())
    }

    /// Saves tab `i` to `path` (its own file, or a new one).
    fn save_to(&mut self, i: usize, path: &str) -> Result<(), String> {
        let text = self.tabs[i].doc.text();
        write_text(self.vfs.as_ref(), path, &text)?;
        let tab = &mut self.tabs[i];
        if tab.path.as_deref() != Some(path) {
            tab.path = Some(path.into());
            tab.title = file_name(path).into();
            tab.lang = Language::for_path(path);
            tab.read_only = false;
        }
        tab.doc.mark_saved();
        tab.lossy = false;
        let msg = format!("Saved {}", tab.title);
        self.show_toast(msg, vrt::time::now_ns());
        Ok(())
    }

    /// The open document called `name` (its title or file name; with
    /// `fuzzy`, also a title containing it).
    fn find_tab(&self, name: &str, fuzzy: bool) -> Result<usize, String> {
        let name = name.trim().to_lowercase();
        let by_file = |t: &Tab| t.path.as_deref().is_some_and(|p| file_name(p).to_lowercase() == name);
        self.tabs
            .iter()
            .position(|t| t.title.to_lowercase() == name)
            .or_else(|| self.tabs.iter().position(by_file))
            .or_else(|| self.tabs.iter().position(|t| fuzzy && t.title.to_lowercase().contains(&name)))
            .ok_or_else(|| {
                let open: Vec<&str> = self.tabs.iter().map(|t| t.title.as_str()).collect();
                format!("no open document is called {name}; open: {}", open.join(", "))
            })
    }

    fn file_exists(&self, path: &str) -> bool {
        self.vfs.as_ref().is_some_and(|v| v.stat(path.into()).ok().and_then(|r| r.ok()).is_some())
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        // A dialog the agent's change would leave behind is closed first
        // (the agent acts for the user, who asked for something else).
        if matches!(self.modal, Some(Modal::Message { .. } | Modal::Shortcuts | Modal::About | Modal::GotoLine(_))) {
            self.modal = None;
        }
        let i = self.active;
        match action {
            "new_document" => {
                if !self.tabs[i].is_pristine() {
                    self.new_tab();
                }
                if let Some(text) = arg_opt_str(args, "text") {
                    self.tab().doc.paste(text);
                }
                self.tab().view.reveal_cursor();
                Ok(object! { "opened" => self.tabs[self.active].title.as_str() })
            }
            "open_file" => {
                let path = arg_path(args, "path")?;
                self.open_path(&path, false);
                if let Some(Modal::Message { text, .. }) = &self.modal {
                    let e = text.clone();
                    self.modal = None;
                    return Err(e);
                }
                let tab = &self.tabs[self.active];
                Ok(object! { "opened" => show_path(&path), "lines" => tab.doc.buffer().line_count() })
            }
            "write" => {
                self.writable()?;
                let text = arg_str(args, "text")?;
                let doc = &mut self.tabs[i].doc;
                match arg_opt_str(args, "where").unwrap_or("cursor") {
                    "cursor" => {}
                    "end" => {
                        let end = doc.buffer().end();
                        doc.set_cursor(end, false);
                    }
                    "start" => doc.set_cursor(Pos::new(0, 0), false),
                    "replace_all" => doc.select_all(),
                    other => return Err(format!("'where' cannot be {other}")),
                }
                doc.paste(text);
                self.tabs[i].view.reveal_cursor();
                let c = self.tabs[i].doc.cursor();
                Ok(object! { "written" => text.chars().count(), "cursor_line" => c.line + 1 })
            }
            "read" => {
                let from = line_arg(self, args, "from_line")?.unwrap_or(1);
                let doc = &self.tabs[i].doc;
                let total = doc.buffer().line_count();
                let want = arg_opt_int(args, "lines")?.map(|n| n.max(1) as usize).unwrap_or(total);
                let mut out = String::new();
                let mut last = from - 1;
                for (k, line) in doc.buffer().lines().enumerate().skip(from - 1).take(want) {
                    if out.len() + line.len() > MAX_TEXT && k >= from {
                        break;
                    }
                    out.push_str(line);
                    out.push('\n');
                    last = k + 1;
                }
                Ok(object! { "from_line" => from, "to_line" => last, "of_lines" => total, "text" => out })
            }
            "find" => {
                let text = arg_str(args, "text")?;
                let case = arg_bool(args, "case_sensitive").unwrap_or(false);
                let tab = &mut self.tabs[i];
                if !tab.doc.find_next(text, case) {
                    return Err(format!("\u{201c}{text}\u{201d} is not in {}", tab.title));
                }
                tab.view.reveal_cursor();
                let c = tab.doc.selection().start();
                Ok(object! { "found_at_line" => c.line + 1, "column" => c.col + 1 })
            }
            "replace" => {
                self.writable()?;
                let find = arg_str(args, "find")?;
                let with = arg_str(args, "with")?;
                let case = arg_bool(args, "case_sensitive").unwrap_or(false);
                let tab = &mut self.tabs[i];
                let n = if arg_bool(args, "all").unwrap_or(true) {
                    tab.doc.replace_all(find, with, case)
                } else if tab.doc.find_next(find, case) {
                    tab.doc.paste(with);
                    1
                } else {
                    0
                };
                if n == 0 {
                    return Err(format!("\u{201c}{find}\u{201d} is not in {}", tab.title));
                }
                tab.view.reveal_cursor();
                Ok(object! { "replaced" => n })
            }
            "go_to_line" => {
                let line = line_arg(self, args, "line")?.ok_or("which line?")?;
                let tab = &mut self.tabs[i];
                tab.doc.goto_line(line);
                tab.view.reveal_cursor();
                Ok(object! { "line" => line })
            }
            "select" => {
                let from = line_arg(self, args, "from_line")?;
                let to = line_arg(self, args, "to_line")?;
                let tab = &mut self.tabs[i];
                match from {
                    None => tab.doc.select_all(),
                    Some(a) => {
                        let b = to.unwrap_or(a).max(a);
                        let end =
                            if b < tab.doc.buffer().line_count() { Pos::new(b, 0) } else { tab.doc.buffer().end() };
                        tab.doc.set_selection(Selection { anchor: Pos::new(a - 1, 0), cursor: end });
                    }
                }
                tab.view.reveal_cursor();
                let selected = tab.doc.selected_text();
                Ok(object! { "selected" => clip(&selected, 500).0 })
            }
            "undo" | "redo" => {
                let n = times(args)?;
                let tab = &mut self.tabs[i];
                let mut done = 0;
                for _ in 0..n {
                    let ok = if action == "undo" { tab.doc.undo() } else { tab.doc.redo() };
                    if !ok {
                        break;
                    }
                    done += 1;
                }
                if done == 0 {
                    return Err(format!("there is nothing to {action}"));
                }
                tab.view.reveal_cursor();
                Ok(object! { action => done })
            }
            "save" => {
                let tab = &self.tabs[i];
                let own = tab.path.clone().filter(|_| !tab.read_only);
                let path = match (own, arg_opt_str(args, "path")) {
                    (Some(p), None) => p,
                    (Some(p), Some(_)) if tab.path.as_deref() == Some(p.as_str()) => p,
                    (_, Some(_)) => {
                        let p = arg_path(args, "path")?;
                        if tab.path.as_deref() != Some(p.as_str()) && self.file_exists(&p) {
                            return Err(format!(
                                "{} already exists; use save_as to replace it, or choose another name",
                                show_path(&p)
                            ));
                        }
                        p
                    }
                    (None, None) if tab.read_only => {
                        return Err(format!("{} is read-only; give a path for a copy", tab.title));
                    }
                    (None, None) => {
                        return Err("this document has no file yet: give a path such as ~/Documents/notes.txt".into());
                    }
                };
                self.save_to(i, &path)?;
                Ok(object! { "saved" => show_path(&path) })
            }
            "save_as" => {
                let path = arg_path(args, "path")?;
                if vfiles::path::is_read_only(&path) {
                    return Err(format!("{} is in the read-only system folder", show_path(&path)));
                }
                let replaced = self.tabs[i].path.as_deref() != Some(path.as_str()) && self.file_exists(&path);
                self.save_to(i, &path)?;
                Ok(object! { "saved" => show_path(&path), "replaced_a_file" => replaced })
            }
            "switch_document" => {
                let k = self.find_tab(arg_str(args, "name")?, true)?;
                self.active = k;
                Ok(object! { "showing" => self.tabs[k].title.as_str() })
            }
            "close_document" => {
                let i = match arg_opt_str(args, "name") {
                    Some(name) => self.find_tab(name, true)?,
                    None => i,
                };
                let tab = &self.tabs[i];
                if tab.doc.is_modified() {
                    return Err(format!(
                        "{} has unsaved changes: save it first, or discard the changes (which needs the user's OK)",
                        tab.title
                    ));
                }
                let title = tab.title.clone();
                if matches!(self.modal, Some(Modal::ConfirmClose { .. })) {
                    self.modal = None;
                }
                self.remove_tab(i);
                Ok(object! { "closed" => title })
            }
            "discard_changes" => {
                // By name: the user approved discarding this document,
                // whichever is in front by now.
                let i = self.find_tab(arg_str(args, "name")?, false)?;
                let title = self.tabs[i].title.clone();
                if matches!(self.modal, Some(Modal::ConfirmClose { tab, .. } | Modal::SaveAs { tab, .. }) if tab == i) {
                    self.modal = None;
                }
                self.remove_tab(i);
                Ok(object! { "closed_without_saving" => title })
            }
            "view" => {
                if let Some(w) = arg_bool(args, "word_wrap") {
                    self.opts.wrap = w;
                }
                if let Some(n) = arg_bool(args, "line_numbers") {
                    self.opts.line_numbers = n;
                }
                if args.get("font_size").is_some() {
                    let size = arg_f64(args, "font_size")? as f32;
                    self.opts.font_size = if size <= 0.0 { DEFAULT_FONT_SIZE } else { size.clamp(9.0, 32.0) };
                }
                Ok(object! {
                    "word_wrap" => self.opts.wrap,
                    "line_numbers" => self.opts.line_numbers,
                    "font_size" => self.opts.font_size,
                })
            }
            other => Err(format!("the Text Editor has no action called {other}")),
        }
    }
}
