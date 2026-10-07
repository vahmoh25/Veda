//! Terminal: a terminal window hosting the built-in command shell (`vsh`).
//!
//! The window shows a scrollback buffer of styled text in JetBrains Mono
//! with a live prompt line at the bottom. It supports line editing, command
//! history, Tab completion, mouse selection with clipboard copy and paste,
//! scrolling with the wheel, keyboard or scrollbar, a blinking cursor and
//! zooming. Commands are interpreted inside this process (see `shell.rs` and
//! `commands.rs`), each on a thread of its own (`job.rs`): the window stays
//! responsive while one runs, keys typed meanwhile wait for it, and Ctrl+C
//! stops it. Programs and applications are started through the launcher.
//! [`agent`] lets the voice agent run commands and read what they print.
//!
//! Usage: `terminal [DIR]` starts in DIR; `terminal -c COMMAND` runs a
//! command first.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod commands;
mod job;
mod netcmds;
mod program;
mod screen;
mod shell;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vabi::{RawHandle, signals};
use vfiles::HOME;
use vfiles::path::display_path;
use vproto::display::modifiers;
use vproto::input::keys;
use vui::ui::KeyPress;
use vui::{App, Color, Cursor, Font, MenuItem, Rect, Ui, WindowSpec};

use job::{Job, Output};
use program::Console;
use screen::{Cell, Pos, Screen, Style, color};
use shell::Shell;

vrt::entry!(main);

/// Terminal background and default text colour.
const BG: Color = Color::hex(0x121216);
const FG: Color = Color::hex(0xDCDCE4);
/// The 16-colour palette (normal, then bright).
const PALETTE: [Color; 16] = [
    Color::hex(0x3B3F51),
    Color::hex(0xF7768E),
    Color::hex(0x9ECE6A),
    Color::hex(0xE0AF68),
    Color::hex(0x7AA2F7),
    Color::hex(0xBB9AF7),
    Color::hex(0x7DCFFF),
    Color::hex(0xC0C6D8),
    Color::hex(0x6E7391),
    Color::hex(0xFF8FA3),
    Color::hex(0xB5E88A),
    Color::hex(0xF5CB84),
    Color::hex(0x8FB4FF),
    Color::hex(0xCDB2FF),
    Color::hex(0x9EDCFF),
    Color::hex(0xF2F3F7),
];
const SELECTION: Color = Color::rgba(91, 140, 255, 96);
const CURSOR: Color = Color::hex(0xE8E8F0);

const PAD_X: i32 = 12;
const PAD_Y: i32 = 10;
const SCROLLBAR_W: i32 = 10;
const BLINK_NS: u64 = 530_000_000;
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// Rows that Page Up and Page Down scroll.
const PAGE: usize = 10;
/// How long the window waits for a command it has just started before it
/// draws again: most commands are done by then, and the window looks as if
/// it had run them itself.
const QUICK_NS: u64 = 50_000_000;
/// How many command lines are remembered as the agent may read them.
const MAX_ECHOES: usize = 1000;

/// An incremental search backwards through the history (Ctrl+R).
struct HistorySearch {
    query: String,
    /// Index of the current match in the history.
    found: Option<usize>,
}

/// The prefix shown while searching the history.
const SEARCH_PREFIX: &str = "(reverse-i-search)`";
const SEARCH_FAILED_PREFIX: &str = "(failed reverse-i-search)`";

/// Something typed while a command ran, handled once it is done.
enum Pending {
    Key(KeyPress),
    /// Pasted text (or what is left of it after a line that started a
    /// command).
    Paste(String),
}

/// One visible row of text: a slice `start..end` of logical line `line`.
#[derive(Debug, Clone, Copy)]
struct VisRow {
    line: u64,
    start: usize,
    end: usize,
}

/// Text geometry for the current frame.
struct Layout {
    area: Rect,
    cell_w: f32,
    row_h: i32,
    cols: usize,
    rows: usize,
    total: usize,
    first: usize,
    vis: Vec<VisRow>,
}

struct Terminal {
    screen: Screen,
    /// The shell, while no command runs (a running command has it).
    shell: Option<Shell>,
    /// The command that is running.
    job: Option<Job>,
    /// Output of commands on its way to the screen.
    output: Arc<Output>,
    /// The shell's interrupt flag (Ctrl+C), also while a command has the
    /// shell.
    interrupt: Arc<AtomicBool>,
    /// Keys and pastes waiting for the running command to finish.
    pending: VecDeque<Pending>,
    /// The working directory as of the last command (also while one runs).
    cwd: String,
    /// The exit status of the last command.
    last_status: i32,
    /// Command lines as the agent may read them (Wi-Fi passwords hidden), by
    /// their line on the screen.
    echoes: VecDeque<(u64, String)>,
    /// The command being typed and the cursor (byte offset) in it.
    input: String,
    cursor: usize,
    /// Position while browsing the history, and the line typed before.
    history_pos: Option<usize>,
    saved_input: String,
    /// Rows scrolled back from the bottom.
    scroll: usize,
    selection: Option<(Pos, Pos)>,
    /// Where a mouse selection started (button held).
    drag_anchor: Option<Pos>,
    drag_moved: bool,
    /// Grabbed scrollbar thumb: pointer offset inside it.
    thumb_grab: Option<i32>,
    blink_start: u64,
    font_size: f32,
    /// Consecutive Tab presses without progress.
    tabs: u32,
    title: String,
    /// A Ctrl+R history search in progress.
    hsearch: Option<HistorySearch>,
    /// The terminal the programs it runs see.
    console: Arc<Console>,
}

/// Number of visual rows a line of `len` cells takes.
fn line_rows(len: usize, cols: usize) -> usize {
    if len == 0 { 1 } else { len.div_ceil(cols) }
}

/// Draws block-element and box-drawing characters as exact rectangles so
/// that neighbouring cells join without seams. Returns `false` for other
/// characters.
fn draw_special(canvas: &mut vui::Canvas, c: char, x0: i32, x1: i32, y: i32, h: i32, color: Color) -> bool {
    let w = x1 - x0;
    let t = if h >= 26 { 2 } else { 1 };
    let cx = x0 + (w - t) / 2;
    let cy = y + (h - t) / 2;
    let mut fill = |x: i32, yy: i32, ww: i32, hh: i32| canvas.fill_rect(Rect::new(x, yy, ww, hh), color);
    // (left, right, up, down) arms of light box-drawing characters.
    let arms = match c {
        '─' => Some((true, true, false, false)),
        '│' => Some((false, false, true, true)),
        '┌' | '╭' => Some((false, true, false, true)),
        '┐' | '╮' => Some((true, false, false, true)),
        '└' | '╰' => Some((false, true, true, false)),
        '┘' | '╯' => Some((true, false, true, false)),
        '├' => Some((false, true, true, true)),
        '┤' => Some((true, false, true, true)),
        '┬' => Some((true, true, false, true)),
        '┴' => Some((true, true, true, false)),
        '┼' => Some((true, true, true, true)),
        _ => None,
    };
    if let Some((l, r, u, d)) = arms {
        if l {
            fill(x0, cy, cx - x0 + t, t);
        }
        if r {
            fill(cx, cy, x1 - cx, t);
        }
        if u {
            fill(cx, y, t, cy - y + t);
        }
        if d {
            fill(cx, cy, t, y + h - cy);
        }
        return true;
    }
    match c {
        '█' => fill(x0, y, w, h),
        '▀' => fill(x0, y, w, h / 2),
        '▄' => fill(x0, y + h / 2, w, h - h / 2),
        '▌' => fill(x0, y, w / 2, h),
        '▐' => fill(x0 + w / 2, y, w - w / 2, h),
        '▁'..='▇' => {
            let eighths = c as i32 - '▀' as i32;
            let bh = h * eighths / 8;
            fill(x0, y + h - bh, w, bh);
        }
        '░' | '▒' | '▓' => {
            let a = match c {
                '░' => 0.28,
                '▒' => 0.5,
                _ => 0.75,
            };
            canvas.fill_rect(Rect::new(x0, y, w, h), color.fade(a));
        }
        _ => return false,
    }
    true
}

/// Resolves a style to (foreground, optional background).
fn colors(s: Style) -> (Color, Option<Color>) {
    let mut fg = if s.fg == color::DEFAULT { FG } else { PALETTE[(s.fg & 15) as usize] };
    let mut bg = if s.bg == color::DEFAULT { None } else { Some(PALETTE[(s.bg & 15) as usize]) };
    if s.inverse {
        let old_fg = fg;
        fg = bg.unwrap_or(BG);
        bg = Some(old_fg);
    }
    if s.dim {
        fg = fg.lerp(BG, 0.45);
    }
    (fg, bg)
}

impl Terminal {
    fn new(cwd: &str) -> Option<Terminal> {
        let output = Output::new()?;
        let mut screen = Screen::new();
        let version = vrt::object::system_info()
            .map(|i| {
                let n = i.version.iter().position(|&c| c == 0).unwrap_or(i.version.len());
                String::from_utf8_lossy(&i.version[..n]).trim().into()
            })
            .unwrap_or_else(|_| String::from("Veda"));
        screen.write_styled("Welcome to ", Style::PLAIN);
        screen.write_styled(&version, Style::fg(color::BRIGHT_BLUE).bold());
        screen.write_styled(" — vsh, the Veda shell\n", Style::PLAIN);
        screen.write_styled("Type ", Style::DIM);
        screen.write_styled("help", Style::fg(color::BRIGHT_GREEN));
        screen.write_styled(" to list the commands, or ", Style::DIM);
        screen.write_styled("apps", Style::fg(color::BRIGHT_GREEN));
        screen.write_styled(" to see the installed applications.\n\n", Style::DIM);
        let console = Console::new(24, 80)?;
        let mut shell = Shell::new(cwd);
        shell.load_history();
        shell.console = Some(console.clone());
        Some(Terminal {
            console,
            screen,
            interrupt: shell.interrupt.clone(),
            cwd: shell.cwd.clone(),
            shell: Some(shell),
            job: None,
            output,
            pending: VecDeque::new(),
            last_status: 0,
            echoes: VecDeque::new(),
            input: String::new(),
            cursor: 0,
            history_pos: None,
            saved_input: String::new(),
            scroll: 0,
            selection: None,
            drag_anchor: None,
            drag_moved: false,
            thumb_grab: None,
            blink_start: 0,
            font_size: DEFAULT_FONT_SIZE,
            tabs: 0,
            title: String::new(),
            hsearch: None,
        })
    }

    /// The command history (empty while a command runs).
    fn history(&self) -> &[String] {
        self.shell.as_ref().map_or(&[][..], |s| s.history.as_slice())
    }

    /// The `exit` command ran (or Ctrl+D was pressed).
    fn exit_requested(&self) -> bool {
        self.shell.as_ref().is_some_and(|s| s.exit_requested)
    }

    /// The prompt plus the input as cells (or the history search line).
    fn live_cells(&self) -> (Vec<Cell>, usize) {
        if let Some(s) = &self.hsearch {
            let failed = !s.query.is_empty() && s.found.is_none();
            let mut cells = screen::cells(if failed { SEARCH_FAILED_PREFIX } else { SEARCH_PREFIX }, Style::DIM);
            cells.extend(screen::cells(&s.query, Style::fg(color::BRIGHT_YELLOW).bold()));
            cells.extend(screen::cells("': ", Style::DIM));
            let prompt_len = cells.len();
            if let Some(line) = s.found.and_then(|i| self.history().get(i)) {
                cells.extend(screen::cells(line, Style::PLAIN));
            }
            return (cells, prompt_len);
        }
        // A program reading the terminal a line at a time: what it wrote of
        // the line so far (its prompt), then what is being typed for it.
        if self.program_mode() {
            let mut cells = self.screen.partial().to_vec();
            let prompt_len = cells.len();
            if self.console.canonical() && self.console.echo() {
                cells.extend(screen::cells(&self.input, Style::PLAIN));
            }
            return (cells, prompt_len);
        }
        // While a command runs, the line below its output stays empty; the
        // prompt comes back when it is done.
        let Some(shell) = &self.shell else { return (Vec::new(), 0) };
        let mut cells = shell.prompt();
        let prompt_len = cells.len();
        cells.extend(screen::cells(&self.input, Style::PLAIN));
        (cells, prompt_len)
    }

    /// Column of the cursor in the live line.
    fn cursor_col(&self, prompt_len: usize) -> usize {
        match &self.hsearch {
            Some(s) => {
                let failed = !s.query.is_empty() && s.found.is_none();
                let prefix = if failed { SEARCH_FAILED_PREFIX } else { SEARCH_PREFIX };
                prefix.chars().count() + s.query.chars().count()
            }
            None if self.program_mode() => {
                let typed = self.console.canonical() && self.console.echo();
                prompt_len + if typed { self.input[..self.cursor].chars().count() } else { 0 }
            }
            None if self.shell.is_none() => 0,
            None => prompt_len + self.input[..self.cursor].chars().count(),
        }
    }

    /// The newest history entry before `before` that contains `query`.
    fn search_history(&self, query: &str, before: usize) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let lower = query.to_lowercase();
        let history = self.history();
        (0..before.min(history.len())).rev().find(|&i| history[i].to_lowercase().contains(&lower))
    }

    /// Ends the history search, putting the match in the input line.
    fn accept_search(&mut self) {
        if let Some(line) = self.hsearch.take().and_then(|s| s.found).and_then(|i| self.history().get(i).cloned()) {
            self.input = line;
            self.cursor = self.input.len();
        }
    }

    /// Handles a key during a history search; returns `true` if consumed.
    fn search_key(&mut self, code: u16, ctrl: bool, ch: Option<char>) -> bool {
        let Some(mut s) = self.hsearch.take() else { return false };
        let n = self.history().len();
        if let Some(c) = ch {
            s.query.push(c);
            s.found = self.search_history(&s.query, s.found.map_or(n, |f| f + 1));
            self.hsearch = Some(s);
            return true;
        }
        match code {
            keys::R if ctrl => {
                let before = s.found.unwrap_or(n);
                s.found = self.search_history(&s.query, before).or(s.found);
            }
            keys::BACKSPACE => {
                s.query.pop();
                s.found = self.search_history(&s.query, n);
            }
            keys::ESC => return true,
            keys::G | keys::C if ctrl => return true,
            keys::ENTER | keys::KPENTER => {
                self.hsearch = Some(s);
                self.accept_search();
                self.submit();
                return true;
            }
            keys::LEFTCTRL | keys::RIGHTCTRL | keys::LEFTSHIFT | keys::RIGHTSHIFT | keys::LEFTALT | keys::RIGHTALT => {}
            _ => {
                // Any other key ends the search and then acts normally.
                self.hsearch = Some(s);
                self.accept_search();
                return false;
            }
        }
        self.hsearch = Some(s);
        true
    }

    fn cells_of(&self, abs: u64, live: &[Cell]) -> Vec<Cell> {
        if abs == self.screen.end() {
            live.to_vec()
        } else {
            self.screen.line(abs).map(|l| l.to_vec()).unwrap_or_default()
        }
    }

    // ---- editing -----------------------------------------------------------

    fn reset_view(&mut self, now: u64) {
        self.scroll = 0;
        self.blink_start = now;
    }

    fn insert(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        self.input.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.history_pos = None;
    }

    fn prev_char(&self, i: usize) -> usize {
        self.input[..i].char_indices().next_back().map(|(p, _)| p).unwrap_or(0)
    }

    fn next_char(&self, i: usize) -> usize {
        self.input[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
    }

    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && self.input[..i].ends_with(' ') {
            i = self.prev_char(i);
        }
        while i > 0 && !self.input[..i].ends_with(' ') && !self.input[..i].ends_with('/') {
            i = self.prev_char(i);
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        while i < self.input.len() && self.input[i..].starts_with(' ') {
            i = self.next_char(i);
        }
        while i < self.input.len() && !self.input[i..].starts_with(' ') && !self.input[i..].starts_with('/') {
            i = self.next_char(i);
        }
        if i < self.input.len() && self.input[i..].starts_with('/') {
            i += 1;
        }
        i
    }

    /// Runs the typed line: echoes it and starts it (see
    /// [`Terminal::start`]).
    fn submit(&mut self) {
        // Keys wait while a command runs, so the shell is here.
        let Some(shell) = &mut self.shell else { return };
        let line = core::mem::take(&mut self.input);
        self.cursor = 0;
        let prompt = shell.prompt();
        let redacted = shell.redact(&line);
        let shown = format!("{}{redacted}", prompt.iter().map(|c| c.ch).collect::<String>());
        let mut echoed = prompt;
        echoed.extend(screen::cells(&line, Style::PLAIN));
        shell.remember(&line);
        self.screen.push_cells(echoed);
        self.echoes.push_back((self.screen.end() - 1, shown));
        if self.echoes.len() > MAX_ECHOES {
            self.echoes.pop_front();
        }
        self.history_pos = None;
        self.saved_input.clear();
        self.selection = None;
        self.scroll = 0;
        if !line.trim().is_empty() {
            self.start(&line, redacted);
        }
    }

    /// Starts `line` on a thread of its own and waits a moment for it.
    /// `shown` is the line as it may be shown (Wi-Fi passwords hidden).
    fn start(&mut self, line: &str, shown: String) {
        self.interrupt.store(false, Ordering::Relaxed);
        match Job::start(&mut self.shell, line.into(), shown, &self.output) {
            Some(job) => {
                self.job = Some(job);
                self.wait_for_job(QUICK_NS);
            }
            None => {
                // No thread for it: it runs here, the window waiting.
                if let Some(shell) = &mut self.shell {
                    shell.execute(line, &self.output);
                    self.last_status = shell.status;
                    self.cwd = shell.cwd.clone();
                }
                self.output.drain_into(&mut self.screen);
                self.screen.finish_line();
            }
        }
    }

    /// Moves the running command's output to the screen and, once it has
    /// finished, takes the shell back. Returns whether a command finished.
    fn collect(&mut self) -> bool {
        self.output.drain_into(&mut self.screen);
        if !self.job.as_ref().is_some_and(Job::finished) {
            return false;
        }
        let Some(job) = self.job.take() else { return false };
        let shell = match job.finish() {
            Some(shell) => shell,
            // The command's thread went away with the shell: a new one.
            None => {
                let mut shell = Shell::new(&self.cwd);
                shell.load_history();
                shell.interrupt = self.interrupt.clone();
                shell.console = Some(self.console.clone());
                shell
            }
        };
        self.output.drain_into(&mut self.screen);
        self.last_status = shell.status;
        self.cwd = shell.cwd.clone();
        self.shell = Some(shell);
        self.screen.finish_line();
        // Lines typed ahead that no program read are the shell's.
        let unread = self.console.take_unread();
        if !unread.is_empty() {
            self.pending.push_front(Pending::Paste(String::from_utf8_lossy(&unread).into_owned()));
        }
        true
    }

    /// Waits up to `timeout` ns for the running command, moving its output
    /// to the screen. Returns whether none runs any more.
    fn wait_for_job(&mut self, timeout: u64) -> bool {
        let deadline = vrt::time::now_ns().saturating_add(timeout);
        loop {
            self.collect();
            if self.job.is_none() {
                return true;
            }
            if vrt::time::now_ns() >= deadline {
                return false;
            }
            self.output.wait(deadline);
        }
    }

    /// Stops the running command (Ctrl+C): it stops at its next step, ^C
    /// shows at once, and what was typed meanwhile is dropped, as in other
    /// terminals. Returns whether a command was running.
    fn interrupt(&mut self) -> bool {
        if self.job.is_none() {
            return false;
        }
        self.interrupt.store(true, Ordering::Relaxed);
        // A program the shell waits for ends at once.
        self.console.wake();
        self.output.drain_into(&mut self.screen);
        self.screen.write_styled("^C", Style::DIM);
        self.screen.newline();
        self.input.clear();
        self.cursor = 0;
        self.pending.clear();
        self.scroll = 0;
        true
    }

    /// Clears the screen (Ctrl+L).
    fn clear_screen(&mut self) {
        self.screen.clear();
        self.selection = None;
        self.scroll = 0;
    }

    /// Handles what was typed while a command ran, until a line starts
    /// another command.
    fn replay(&mut self, ui: &mut Ui) {
        while self.job.is_none() && !self.exit_requested() {
            match self.pending.pop_front() {
                Some(Pending::Key(k)) => self.handle_key(ui, k),
                Some(Pending::Paste(text)) => self.paste(&text),
                None => break,
            }
        }
    }

    /// Pastes text: complete lines are run, the rest is inserted. Lines
    /// after one that started a command wait for it.
    fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\t', "    ");
        let mut rest = text.as_str();
        while let Some((line, more)) = rest.split_once('\n') {
            self.insert(line);
            self.submit();
            if self.exit_requested() {
                return;
            }
            rest = more;
            if self.job.is_some() {
                if !rest.is_empty() {
                    self.pending.push_front(Pending::Paste(rest.into()));
                }
                return;
            }
        }
        self.insert(rest);
    }

    fn history_up(&mut self) {
        let len = self.history().len();
        if len == 0 {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.saved_input = self.input.clone();
                len - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.history_pos = Some(pos);
        self.input = self.history()[pos].clone();
        self.cursor = self.input.len();
    }

    fn history_down(&mut self) {
        let Some(p) = self.history_pos else { return };
        if p + 1 < self.history().len() {
            self.history_pos = Some(p + 1);
            self.input = self.history()[p + 1].clone();
        } else {
            self.history_pos = None;
            self.input = core::mem::take(&mut self.saved_input);
        }
        self.cursor = self.input.len();
    }

    fn complete(&mut self) {
        let Some(shell) = &mut self.shell else { return };
        let c = shell.complete(&self.input, self.cursor);
        let cols = shell.cols;
        if let Some(rep) = c.replacement {
            self.input.replace_range(c.start..self.cursor, &rep);
            self.cursor = c.start + rep.len();
            self.tabs = 0;
            return;
        }
        if c.candidates.len() <= 1 {
            return;
        }
        self.tabs += 1;
        if self.tabs < 2 {
            return;
        }
        self.tabs = 0;
        // Show the candidates below a copy of the prompt line, like bash.
        let (live, _) = self.live_cells();
        self.screen.push_cells(live);
        let colw = c.candidates.iter().map(|s| s.chars().count()).max().unwrap_or(1) + 2;
        let per_row = (cols.max(colw) / colw).max(1);
        let rows = c.candidates.len().div_ceil(per_row);
        for r in 0..rows {
            for k in 0..per_row {
                let Some(name) = c.candidates.get(k * rows + r) else { continue };
                let style = if name.ends_with('/') { Style::fg(color::BRIGHT_BLUE).bold() } else { Style::PLAIN };
                self.screen.write_styled(name, style);
                if (k + 1) * rows + r < c.candidates.len() {
                    let pad = colw - name.chars().count();
                    self.screen.write_styled(&" ".repeat(pad), Style::PLAIN);
                }
            }
            self.screen.newline();
        }
        self.scroll = 0;
    }

    fn copy_selection(&mut self, ui: &mut Ui) -> bool {
        let Some((a, b)) = self.selection else { return false };
        let (live, _) = self.live_cells();
        let text = self.screen.text_between(a, b, &live);
        if text.is_empty() {
            return false;
        }
        ui.set_clipboard(&text);
        true
    }

    fn select_all(&mut self) {
        let (live, _) = self.live_cells();
        self.selection =
            Some((Pos { line: self.screen.base(), col: 0 }, Pos { line: self.screen.end(), col: live.len() }));
    }

    fn zoom(&mut self, delta: f32) {
        self.font_size = if delta == 0.0 { DEFAULT_FONT_SIZE } else { (self.font_size + delta).clamp(9.0, 28.0) };
    }

    /// Handles the keyboard in the order the keys were pressed. While a
    /// command runs, the keys that can wait for it (typing, editing, Enter)
    /// do.
    fn handle_keys(&mut self, ui: &mut Ui) {
        for k in ui.input.keys.clone() {
            if self.program_mode() {
                // Keys typed before the program started are its input too.
                while let Some(p) = self.pending.pop_front() {
                    match p {
                        Pending::Key(k) => self.program_key(ui, &k),
                        Pending::Paste(text) => self.program_paste(&text),
                    }
                }
                self.program_key(ui, &k);
                continue;
            }
            if self.job.is_some() || !self.pending.is_empty() {
                if !self.key_while_running(ui, &k) {
                    self.pending.push_back(Pending::Key(k));
                }
                continue;
            }
            self.handle_key(ui, k);
            if self.exit_requested() {
                return;
            }
        }
    }

    /// A program is running in the foreground and reads the terminal.
    fn program_mode(&self) -> bool {
        self.job.is_some() && self.console.program_running()
    }

    /// Sends text to the program: in canonical mode it is edited here first
    /// (each complete line goes when its newline is pasted).
    fn program_paste(&mut self, text: &str) {
        if !self.console.canonical() {
            self.console.send(text.as_bytes());
            return;
        }
        let text = text.replace("\r\n", "\n");
        let mut rest = text.as_str();
        while let Some((line, more)) = rest.split_once('\n') {
            self.input.insert_str(self.cursor, line);
            self.cursor = self.input.len();
            self.program_enter();
            rest = more;
        }
        self.input.insert_str(self.cursor, rest);
        self.cursor += rest.len();
    }

    /// Ends the line being typed for the program (Enter in canonical mode).
    fn program_enter(&mut self) {
        let line = core::mem::take(&mut self.input);
        self.cursor = 0;
        if self.console.echo() {
            self.screen.write_styled(&line, Style::PLAIN);
        }
        self.screen.newline();
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        self.console.send(&bytes);
    }

    /// A key while a program runs in the foreground. In canonical mode the
    /// line is edited here and sent with Enter; otherwise each key goes to
    /// the program as the bytes a terminal sends for it.
    fn program_key(&mut self, ui: &mut Ui, k: &KeyPress) {
        let ctrl = k.modifiers & modifiers::CTRL != 0;
        let shift = k.modifiers & modifiers::SHIFT != 0;
        self.reset_view(ui.now());
        // What the window does with these whatever the program wants.
        match k.code {
            keys::C if ctrl && shift => {
                self.copy_selection(ui);
                return;
            }
            keys::V if ctrl && shift => {
                let clip = ui.clipboard();
                self.program_paste(&clip);
                return;
            }
            keys::A if ctrl && shift => return self.select_all(),
            keys::C if ctrl && self.console.signals() => {
                if !self.copy_selection(ui) {
                    self.interrupt();
                }
                self.selection = None;
                return;
            }
            keys::UP | keys::DOWN if shift => {
                self.scroll =
                    if k.code == keys::UP { self.scroll.saturating_add(1) } else { self.scroll.saturating_sub(1) };
                return;
            }
            keys::PAGEUP if shift => {
                self.scroll = self.scroll.saturating_add(PAGE);
                return;
            }
            keys::PAGEDOWN if shift => {
                self.scroll = self.scroll.saturating_sub(PAGE);
                return;
            }
            keys::EQUAL | keys::KPPLUS if ctrl => return self.zoom(1.0),
            keys::MINUS | keys::KPMINUS if ctrl => return self.zoom(-1.0),
            keys::KEY_0 if ctrl => return self.zoom(0.0),
            _ => {}
        }
        if !self.console.canonical() {
            if let Some(bytes) = key_bytes(k) {
                if self.console.echo()
                    && !ctrl
                    && let Some(c) = k.ch
                {
                    let mut buf = [0u8; 4];
                    self.screen.write_styled(c.encode_utf8(&mut buf), Style::PLAIN);
                }
                self.console.send(&bytes);
            }
            return;
        }
        if let Some(c) = k.ch.filter(|_| !ctrl) {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            self.input.insert_str(self.cursor, s);
            self.cursor += s.len();
            return;
        }
        match k.code {
            keys::ENTER | keys::KPENTER => self.program_enter(),
            keys::BACKSPACE if self.cursor > 0 => {
                let p = if ctrl { self.word_left(self.cursor) } else { self.prev_char(self.cursor) };
                self.input.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            keys::DELETE if self.cursor < self.input.len() => {
                let n = self.next_char(self.cursor);
                self.input.replace_range(self.cursor..n, "");
            }
            keys::LEFT => self.cursor = self.prev_char(self.cursor),
            keys::RIGHT => self.cursor = self.next_char(self.cursor),
            keys::HOME => self.cursor = 0,
            keys::END => self.cursor = self.input.len(),
            keys::A if ctrl => self.cursor = 0,
            keys::E if ctrl => self.cursor = self.input.len(),
            keys::U if ctrl => {
                self.input.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            keys::K if ctrl => self.input.truncate(self.cursor),
            keys::W if ctrl => {
                let p = self.word_left(self.cursor);
                self.input.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            keys::L if ctrl => self.clear_screen(),
            keys::C if ctrl => self.console.send(b"\x03"),
            // The end of the input: what was typed goes without a newline;
            // on an empty line the program reads the end of the file.
            keys::D if ctrl => {
                if self.input.is_empty() {
                    self.console.end_input();
                } else {
                    let line = core::mem::take(&mut self.input);
                    self.cursor = 0;
                    if self.console.echo() {
                        self.screen.write_styled(&line, Style::PLAIN);
                    }
                    self.console.send(line.as_bytes());
                }
            }
            _ => {}
        }
    }

    /// The keys that act while a command runs: Ctrl+C stops it (or copies
    /// the selection), and copying, selecting everything, scrolling,
    /// clearing the screen and zooming work as always. Returns whether `k`
    /// was one of them.
    fn key_while_running(&mut self, ui: &mut Ui, k: &KeyPress) -> bool {
        let ctrl = k.modifiers & modifiers::CTRL != 0;
        let shift = k.modifiers & modifiers::SHIFT != 0;
        match k.code {
            keys::C if ctrl && shift => {
                self.copy_selection(ui);
            }
            keys::C if ctrl => {
                if !self.copy_selection(ui) && !self.interrupt() {
                    return false;
                }
                self.selection = None;
            }
            keys::A if ctrl && shift => self.select_all(),
            keys::L if ctrl => self.clear_screen(),
            keys::UP if ctrl || shift => self.scroll = self.scroll.saturating_add(1),
            keys::DOWN if ctrl || shift => self.scroll = self.scroll.saturating_sub(1),
            keys::PAGEUP => self.scroll = self.scroll.saturating_add(PAGE),
            keys::PAGEDOWN => self.scroll = self.scroll.saturating_sub(PAGE),
            keys::HOME if ctrl => self.scroll = usize::MAX,
            keys::END if ctrl => self.scroll = 0,
            keys::EQUAL | keys::KPPLUS if ctrl => self.zoom(1.0),
            keys::MINUS | keys::KPMINUS if ctrl => self.zoom(-1.0),
            keys::KEY_0 if ctrl => self.zoom(0.0),
            keys::ESC if self.selection.is_some() => self.selection = None,
            _ => return false,
        }
        true
    }

    /// Handles one key: typed characters, editing keys and shortcuts.
    fn handle_key(&mut self, ui: &mut Ui, k: KeyPress) {
        let now = ui.now();
        let ctrl = k.modifiers & modifiers::CTRL != 0;
        let shift = k.modifiers & modifiers::SHIFT != 0;
        if k.code != keys::TAB {
            self.tabs = 0;
        }
        // The character the key typed (none with Ctrl, Alt or Super).
        let ch = k.ch.filter(|c| *c != '\t');
        if self.hsearch.is_some() && self.search_key(k.code, ctrl, ch) {
            self.reset_view(now);
            return;
        }
        if k.code == keys::TAB && !ctrl {
            self.complete();
            self.selection = None;
            self.reset_view(now);
            return;
        }
        if let Some(c) = ch {
            let mut buf = [0u8; 4];
            self.insert(c.encode_utf8(&mut buf));
            self.selection = None;
            self.reset_view(now);
            return;
        }
        match k.code {
            keys::ENTER | keys::KPENTER => {
                self.submit();
                self.reset_view(now);
            }
            keys::BACKSPACE => {
                if self.cursor > 0 {
                    let p = if ctrl { self.word_left(self.cursor) } else { self.prev_char(self.cursor) };
                    self.input.replace_range(p..self.cursor, "");
                    self.cursor = p;
                }
                self.reset_view(now);
            }
            keys::DELETE => {
                if self.cursor < self.input.len() {
                    let n = if ctrl { self.word_right(self.cursor) } else { self.next_char(self.cursor) };
                    self.input.replace_range(self.cursor..n, "");
                }
                self.reset_view(now);
            }
            keys::LEFT => {
                self.cursor = if ctrl { self.word_left(self.cursor) } else { self.prev_char(self.cursor) };
                self.reset_view(now);
            }
            keys::RIGHT => {
                self.cursor = if ctrl { self.word_right(self.cursor) } else { self.next_char(self.cursor) };
                self.reset_view(now);
            }
            keys::HOME if ctrl => self.scroll = usize::MAX,
            keys::END if ctrl => self.scroll = 0,
            keys::HOME => {
                self.cursor = 0;
                self.reset_view(now);
            }
            keys::END => {
                self.cursor = self.input.len();
                self.reset_view(now);
            }
            keys::UP if ctrl || shift => self.scroll = self.scroll.saturating_add(1),
            keys::DOWN if ctrl || shift => self.scroll = self.scroll.saturating_sub(1),
            keys::UP => {
                self.history_up();
                self.reset_view(now);
            }
            keys::DOWN => {
                self.history_down();
                self.reset_view(now);
            }
            keys::PAGEUP => self.scroll = self.scroll.saturating_add(PAGE),
            keys::PAGEDOWN => self.scroll = self.scroll.saturating_sub(PAGE),
            keys::ESC => {
                if self.selection.is_some() {
                    self.selection = None;
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    self.history_pos = None;
                }
            }
            keys::INSERT if shift => {
                let clip = ui.clipboard();
                self.paste(&clip);
                self.reset_view(now);
            }
            keys::C if ctrl && shift => {
                self.copy_selection(ui);
            }
            keys::C if ctrl => {
                if !self.copy_selection(ui) {
                    // Cancel the line, like ^C in a Unix shell.
                    let (mut live, _) = self.live_cells();
                    live.extend(screen::cells("^C", Style::DIM));
                    self.screen.push_cells(live);
                    self.input.clear();
                    self.cursor = 0;
                    self.history_pos = None;
                    self.reset_view(now);
                }
                self.selection = None;
            }
            keys::V if ctrl => {
                let clip = ui.clipboard();
                self.paste(&clip);
                self.reset_view(now);
            }
            keys::R if ctrl => {
                self.hsearch = Some(HistorySearch { query: String::new(), found: None });
                self.reset_view(now);
            }
            keys::A if ctrl && shift => self.select_all(),
            keys::A if ctrl => self.cursor = 0,
            keys::E if ctrl => self.cursor = self.input.len(),
            keys::U if ctrl => {
                self.input.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            keys::K if ctrl => self.input.truncate(self.cursor),
            keys::W if ctrl => {
                let p = self.word_left(self.cursor);
                self.input.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            keys::L if ctrl => self.clear_screen(),
            keys::D if ctrl && self.input.is_empty() => {
                if let Some(shell) = &mut self.shell {
                    shell.exit_requested = true;
                }
            }
            keys::EQUAL | keys::KPPLUS if ctrl => self.zoom(1.0),
            keys::MINUS | keys::KPMINUS if ctrl => self.zoom(-1.0),
            keys::KEY_0 if ctrl => self.zoom(0.0),
            _ => {}
        }
    }

    // ---- layout ------------------------------------------------------------

    fn layout(&self, ui: &Ui, live_len: usize, cursor_col: usize) -> Layout {
        let mono = ui.ctx.font(Font::Mono);
        let cell_w = ui.ctx.text.measure(mono, self.font_size, "M").max(1.0);
        let m = ui.ctx.text.metrics(mono, self.font_size);
        let row_h = (m.line_height + 1.0) as i32;
        let area = ui.rect().inset(PAD_X, PAD_Y, PAD_X + SCROLLBAR_W, PAD_Y);
        let cols = ((area.w as f32 / cell_w) as usize).max(1);
        let rows = (area.h / row_h.max(1)).max(1) as usize;
        let live_rows = line_rows(live_len, cols).max(cursor_col / cols + 1);
        let (base, end) = (self.screen.base(), self.screen.end());
        let mut total = live_rows;
        for abs in base..end {
            total += line_rows(self.screen.line(abs).map_or(0, |l| l.len()), cols);
        }
        let max_scroll = total.saturating_sub(rows);
        let scroll = self.scroll.min(max_scroll);
        let first = total.saturating_sub(rows + scroll);
        // Collect the visible rows, walking backwards from the input line.
        let mut vis = Vec::with_capacity(rows);
        let last = first + rows;
        let mut row_end = total;
        let mut abs = end;
        loop {
            let (len, r) = if abs == end {
                (live_len, live_rows)
            } else {
                let len = self.screen.line(abs).map_or(0, |l| l.len());
                (len, line_rows(len, cols))
            };
            let row_start = row_end - r;
            if row_start < last && row_end > first {
                for k in (0..r).rev() {
                    let ri = row_start + k;
                    if ri >= first && ri < last {
                        vis.push(VisRow { line: abs, start: k * cols, end: ((k + 1) * cols).min(len) });
                    }
                }
            }
            row_end = row_start;
            if row_end <= first || abs == base {
                break;
            }
            abs -= 1;
        }
        vis.reverse();
        Layout { area, cell_w, row_h, cols, rows, total, first, vis }
    }

    /// The buffer position under a window point (clamped to the text).
    fn hit(&self, l: &Layout, x: i32, y: i32, live: &[Cell]) -> Option<Pos> {
        if l.vis.is_empty() {
            return None;
        }
        let row = if y < l.area.y { 0 } else { (((y - l.area.y) / l.row_h) as usize).min(l.vis.len() - 1) };
        let vr = l.vis[row];
        let cx = (((x - l.area.x) as f32 / l.cell_w) + 0.5).max(0.0) as usize;
        let len =
            if vr.line == self.screen.end() { live.len() } else { self.screen.line(vr.line).map_or(0, |c| c.len()) };
        let col = (vr.start + cx).min(vr.start.max(vr.end)).min(len.max(vr.start));
        Some(Pos { line: vr.line, col })
    }

    // ---- mouse -------------------------------------------------------------

    fn handle_mouse(&mut self, ui: &mut Ui, l: &mut Layout, live: &[Cell], prompt_len: usize) {
        let max_scroll = l.total.saturating_sub(l.rows);
        // Wheel.
        if ui.hovered(ui.rect()) && ui.input.scroll.1 != 0 {
            let d = ui.input.scroll.1 * 3;
            self.scroll =
                if d > 0 { self.scroll.saturating_add(d as usize) } else { self.scroll.saturating_sub((-d) as usize) };
            self.scroll = self.scroll.min(max_scroll);
        }
        // Scrollbar.
        let track = Rect::new(ui.width - SCROLLBAR_W - 4, 4, SCROLLBAR_W, ui.height - 8);
        if max_scroll > 0 {
            let thumb_h = ((l.rows as f32 / l.total as f32) * track.h as f32).max(28.0) as i32;
            let span = (track.h - thumb_h).max(1);
            let pos = |scroll: usize| {
                track.y + (((max_scroll - scroll.min(max_scroll)) as f32 / max_scroll as f32) * span as f32) as i32
            };
            let thumb_y = pos(self.scroll);
            let id = ui.id("scrollbar");
            let resp = ui.interact(id, track);
            if resp.pressed
                && let Some((_, py)) = ui.input.pointer
            {
                let grab = if py >= thumb_y && py < thumb_y + thumb_h { py - thumb_y } else { thumb_h / 2 };
                self.thumb_grab = Some(grab);
            }
            if !ui.input.down[0] {
                self.thumb_grab = None;
            }
            if let (Some(grab), Some((_, py))) = (self.thumb_grab, ui.input.pointer) {
                let f = ((py - grab - track.y) as f32 / span as f32).clamp(0.0, 1.0);
                self.scroll = max_scroll - (f * max_scroll as f32 + 0.5) as usize;
            }
        } else {
            self.thumb_grab = None;
        }
        // Recompute the visible rows if the scroll position changed.
        let wanted_first = l.total.saturating_sub(l.rows + self.scroll.min(max_scroll));
        if wanted_first != l.first {
            let cursor_col = self.cursor_col(prompt_len);
            *l = self.layout(ui, live.len(), cursor_col);
        }
        if self.thumb_grab.is_some() {
            return;
        }
        // Selection.
        let text_rect = Rect::new(0, 0, ui.width - SCROLLBAR_W - 6, ui.height);
        let id = ui.id("text");
        let resp = ui.interact(id, text_rect);
        if resp.hovered {
            ui.set_cursor(Cursor::Text);
        }
        if resp.right_clicked
            && let Some((x, y)) = ui.input.pointer
        {
            ui.open_context_menu("term-menu", x, y);
        }
        if resp.pressed
            && let Some((x, y)) = ui.input.pointer
            && let Some(p) = self.hit(l, x, y, live)
        {
            let clicks = ui.input.clicks;
            let cells = self.cells_of(p.line, live);
            if clicks >= 3 {
                self.selection = Some((Pos { line: p.line, col: 0 }, Pos { line: p.line, col: cells.len() }));
                self.drag_anchor = None;
            } else if clicks == 2 {
                let (a, b) = screen::word_at(&cells, p.col.min(cells.len().saturating_sub(1)));
                self.selection = Some((Pos { line: p.line, col: a }, Pos { line: p.line, col: b }));
                self.drag_anchor = None;
            } else {
                self.drag_anchor = Some(p);
                self.drag_moved = false;
            }
        }
        if let Some(anchor) = self.drag_anchor {
            if ui.input.down[0] {
                if let Some((x, y)) = ui.input.pointer {
                    // Auto-scroll when dragging past the edges.
                    if y < l.area.y && self.scroll < max_scroll {
                        self.scroll += 1;
                        ui.repaint();
                    } else if y > l.area.bottom() && self.scroll > 0 {
                        self.scroll -= 1;
                        ui.repaint();
                    }
                    if let Some(p) = self.hit(l, x, y, live) {
                        if p != anchor {
                            self.drag_moved = true;
                        }
                        if self.drag_moved {
                            self.selection = Some((anchor, p));
                        }
                    }
                }
            } else {
                // Released: a plain click clears the selection and may move
                // the cursor within the input.
                if !self.drag_moved {
                    self.selection = None;
                    if anchor.line == self.screen.end() && anchor.col >= prompt_len && self.hsearch.is_none() {
                        let idx = anchor.col - prompt_len;
                        self.cursor = self.input.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(self.input.len());
                        self.blink_start = ui.now();
                    }
                }
                self.drag_anchor = None;
            }
        }
    }

    // ---- drawing -----------------------------------------------------------

    fn draw(&mut self, ui: &mut Ui, l: &Layout, live: &[Cell], cursor_col: usize) {
        let now = ui.now();
        let full = ui.rect();
        ui.canvas.fill_rect(full, BG);
        let mono = ui.ctx.font(Font::Mono);
        let mono_bold = ui.ctx.font(Font::MonoBold);
        let size = self.font_size;
        let m = ui.ctx.text.metrics(mono, size);
        let sel = self.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        let end = self.screen.end();
        ui.canvas.save();
        ui.canvas.clip_to(Rect::new(0, 0, full.w - SCROLLBAR_W - 4, full.h));
        let mut run = String::new();
        for (i, vr) in l.vis.iter().enumerate() {
            let y = l.area.y + i as i32 * l.row_h;
            let line: &[Cell] = if vr.line == end { live } else { self.screen.line(vr.line).unwrap_or(&[]) };
            let seg = &line[vr.start.min(line.len())..vr.end.min(line.len())];
            let x_of = |col: usize| l.area.x as f32 + col as f32 * l.cell_w;
            // Backgrounds.
            for (k, c) in seg.iter().enumerate() {
                if let (_, Some(bg)) = colors(c.style) {
                    let x0 = x_of(k) as i32;
                    let x1 = x_of(k + 1) as i32;
                    ui.canvas.fill_rect(Rect::new(x0, y, x1 - x0, l.row_h), bg);
                }
            }
            // Selection.
            if let Some((a, b)) = sel
                && vr.line >= a.line
                && vr.line <= b.line
            {
                let c0 = if vr.line == a.line { a.col.max(vr.start) } else { vr.start };
                let row_end = if vr.end > vr.start || line.is_empty() { vr.start + l.cols } else { vr.end };
                let c1 = if vr.line == b.line { b.col.min(row_end) } else { row_end.min(vr.start.max(line.len()) + 1) };
                if c1 > c0 {
                    let x0 = x_of(c0 - vr.start) as i32;
                    let x1 = x_of(c1 - vr.start) as i32;
                    ui.canvas.fill_rect(Rect::new(x0, y, x1 - x0, l.row_h), SELECTION);
                }
            }
            // Text, in runs of equally styled ASCII characters.
            let baseline = (y as f32 + (l.row_h as f32 - (m.ascent + m.descent)) / 2.0 + m.ascent + 0.5) as i32 as f32;
            let mut k = 0;
            while k < seg.len() {
                let st = seg[k].style;
                let (fg, _) = colors(st);
                let font = if st.bold { mono_bold } else { mono };
                if !seg[k].ch.is_ascii() {
                    let (x0, x1) = (x_of(k) as i32, x_of(k + 1) as i32);
                    if !draw_special(&mut ui.canvas, seg[k].ch, x0, x1, y, l.row_h, fg) {
                        let mut buf = [0u8; 4];
                        let s = seg[k].ch.encode_utf8(&mut buf);
                        ui.ctx.text.draw(&mut ui.canvas, font, size, x_of(k), baseline, s, fg);
                    }
                    if st.underline {
                        ui.canvas.fill_rect(Rect::new(x_of(k) as i32, baseline as i32 + 2, l.cell_w as i32 + 1, 1), fg);
                    }
                    k += 1;
                    continue;
                }
                run.clear();
                let start = k;
                while k < seg.len() && seg[k].style == st && seg[k].ch.is_ascii() {
                    run.push(seg[k].ch);
                    k += 1;
                }
                if !run.trim().is_empty() {
                    ui.ctx.text.draw(&mut ui.canvas, font, size, x_of(start), baseline, &run, fg);
                }
                if st.underline {
                    let x0 = x_of(start) as i32;
                    ui.canvas.fill_rect(Rect::new(x0, baseline as i32 + 2, x_of(k) as i32 - x0, 1), fg);
                }
            }
            // Cursor.
            if vr.line == end && self.scroll_follows(l) {
                let row_of_cursor = cursor_col / l.cols;
                if vr.start == row_of_cursor * l.cols {
                    let cx = x_of(cursor_col % l.cols) as i32;
                    let cw = (l.cell_w + 0.5) as i32;
                    let r = Rect::new(cx, y + 1, cw, l.row_h - 2);
                    if ui.input.focused {
                        let phase = ((now.saturating_sub(self.blink_start)) / BLINK_NS) % 2;
                        if phase == 0 {
                            ui.canvas.fill_rect(r, CURSOR);
                            if let Some(c) = live.get(cursor_col).filter(|c| c.ch != ' ') {
                                let mut buf = [0u8; 4];
                                let s = c.ch.encode_utf8(&mut buf);
                                ui.ctx.text.draw(&mut ui.canvas, mono, size, cx as f32, baseline, s, BG);
                            }
                        }
                        let elapsed = now.saturating_sub(self.blink_start);
                        ui.repaint_at(self.blink_start + (elapsed / BLINK_NS + 1) * BLINK_NS);
                    } else {
                        ui.canvas.stroke_rounded_rect(r, 1.0, 1.0, CURSOR.with_alpha(160));
                    }
                }
            }
        }
        ui.canvas.restore();
        // Scrollbar.
        let max_scroll = l.total.saturating_sub(l.rows);
        if max_scroll > 0 {
            let track = Rect::new(ui.width - SCROLLBAR_W - 4, 4, SCROLLBAR_W, ui.height - 8);
            let thumb_h = ((l.rows as f32 / l.total as f32) * track.h as f32).max(28.0) as i32;
            let span = (track.h - thumb_h).max(1);
            let scroll = self.scroll.min(max_scroll);
            let thumb_y = track.y + (((max_scroll - scroll) as f32 / max_scroll as f32) * span as f32) as i32;
            let hot = ui.hovered(track) || self.thumb_grab.is_some();
            let id = ui.id("scrollbar");
            let k = ui.animate(id, if hot { 1.0 } else { 0.0 }, 6.0);
            let alpha = (50.0 + k * 90.0) as u8;
            let w = (4.0 + k * 3.0) as i32;
            let thumb = Rect::new(track.right() - w - 1, thumb_y, w, thumb_h);
            ui.canvas.fill_rounded_rect(thumb, w as f32 / 2.0, Color::rgba(255, 255, 255, alpha));
            // "More below" hint when scrolled back.
            if scroll > 0 {
                let t = ui.theme().clone();
                let badge = Rect::new(full.w - SCROLLBAR_W - 150, full.h - 34, 132, 26);
                ui.canvas.fill_rounded_rect(badge, 13.0, Color::rgba(40, 42, 54, 230));
                ui.canvas.stroke_rounded_rect(badge, 13.0, 1.0, t.border_strong);
                ui.icon(Rect::new(badge.x + 8, badge.y, 18, badge.h), vui::Icon::ChevronDown, 14.0, t.text_dim);
                ui.label(
                    badge.inset(28, 0, 8, 0),
                    &format!("{scroll} lines below"),
                    Font::Regular,
                    t.small_size,
                    t.text_dim,
                    vui::Align::Left,
                );
            }
        }
    }

    /// The cursor is drawn only on the live line; it is always visible
    /// when that line is on screen.
    fn scroll_follows(&self, _l: &Layout) -> bool {
        true
    }

    fn menu_items(&self) -> [MenuItem; 9] {
        [
            MenuItem::new("Copy").shortcut("Ctrl+Shift+C").enabled(self.selection.is_some()),
            MenuItem::new("Paste").shortcut("Ctrl+Shift+V"),
            MenuItem::new("Select all").shortcut("Ctrl+Shift+A"),
            MenuItem::separator(),
            MenuItem::new("Clear screen").shortcut("Ctrl+L"),
            MenuItem::separator(),
            MenuItem::new("Zoom in").shortcut("Ctrl+="),
            MenuItem::new("Zoom out").shortcut("Ctrl+-"),
            MenuItem::new("Reset zoom").shortcut("Ctrl+0"),
        ]
    }

    fn context_menu(&mut self, ui: &mut Ui) {
        let items = self.menu_items();
        match ui.context_menu("term-menu", &items) {
            Some(0) => {
                self.copy_selection(ui);
            }
            Some(1) => {
                let clip = ui.clipboard();
                if self.job.is_some() {
                    self.pending.push_back(Pending::Paste(clip));
                } else {
                    self.paste(&clip);
                }
                self.scroll = 0;
            }
            Some(2) => self.select_all(),
            Some(4) => self.clear_screen(),
            Some(6) => self.zoom(1.0),
            Some(7) => self.zoom(-1.0),
            Some(8) => self.zoom(0.0),
            _ => {}
        }
    }
}

impl App for Terminal {
    fn agent_info(&self) -> Option<vui::agent::AppAgentInfo> {
        Some(agent::info())
    }

    fn agent_state(&self) -> vui::agent::Value {
        agent::state(self)
    }

    fn agent_invoke(&mut self, action: &str, args: &vui::agent::Value) -> Result<vui::agent::Value, String> {
        Terminal::agent_invoke(self, action, args)
    }

    fn wait_handles(&self) -> Vec<(RawHandle, u32)> {
        alloc::vec![(self.output.handle(), signals::SIGNALED)]
    }

    fn handle_signaled(&mut self, _index: usize, _observed: u32) {
        // Output arrived or the command finished; a frame follows.
        self.collect();
    }

    fn update(&mut self, ui: &mut Ui) {
        // The running command's output, and the shell back once it is done;
        // then what was typed meanwhile.
        self.collect();
        if self.job.is_none() {
            self.replay(ui);
        }
        if self.exit_requested() {
            ui.close_window();
            return;
        }
        // The mouse first (it may select text), then the keyboard (which may
        // copy it), so that both arriving in one frame act in order. While
        // the context menu is open, vui gives it the keyboard: no keys here.
        let (live, prompt_len) = self.live_cells();
        let cursor_col = self.cursor_col(prompt_len);
        let mut layout = self.layout(ui, live.len(), cursor_col);
        if let Some(shell) = &mut self.shell {
            shell.cols = layout.cols;
        }
        // Programs ask the terminal how large it is.
        let clamp = |n: usize| n.clamp(1, u16::MAX as usize) as u16;
        self.console.tty.set_size(clamp(layout.rows), clamp(layout.cols));
        self.console.tty.set_pixels(clamp(layout.area.w.max(0) as usize), clamp(layout.area.h.max(0) as usize));
        self.handle_mouse(ui, &mut layout, &live, prompt_len);
        if !ui.input.keys.is_empty() {
            self.handle_keys(ui);
            if self.exit_requested() {
                ui.close_window();
                return;
            }
            let (live, prompt_len) = self.live_cells();
            let cursor_col = self.cursor_col(prompt_len);
            layout = self.layout(ui, live.len(), cursor_col);
            if let Some(shell) = &mut self.shell {
                shell.cols = layout.cols;
            }
            self.finish_frame(ui, &layout, &live, cursor_col);
            return;
        }
        self.finish_frame(ui, &layout, &live, cursor_col);
    }
}

impl Terminal {
    /// Draws the frame and the context menu, and updates the title.
    fn finish_frame(&mut self, ui: &mut Ui, layout: &Layout, live: &[Cell], cursor_col: usize) {
        // The selection may refer to lines that were cleared.
        if let Some((a, b)) = self.selection
            && a.line.min(b.line) < self.screen.base()
        {
            self.selection = None;
        }
        self.draw(ui, layout, live, cursor_col);
        self.context_menu(ui);
        // Keep the window title in sync with the working directory.
        let cwd = self.shell.as_ref().map_or(self.cwd.as_str(), |s| s.cwd.as_str());
        let title = format!("Terminal — {}", display_path(cwd));
        if title != self.title {
            let _ = ui.ctx.display.set_title(ui.ctx.window_id, title.clone());
            self.title = title;
        }
    }
}

/// The letter a key code types on a US keyboard (for Ctrl+letter).
fn letter(code: u16) -> Option<u8> {
    const ROWS: [(u16, &[u8]); 3] = [(keys::Q, b"qwertyuiop"), (keys::A, b"asdfghjkl"), (keys::Z, b"zxcvbnm")];
    ROWS.iter().find_map(|&(first, letters)| letters.get(code.checked_sub(first)? as usize).copied())
}

/// The bytes a terminal sends a program for a key when the program reads
/// keys as they are typed (as xterm sends them).
fn key_bytes(k: &KeyPress) -> Option<Vec<u8>> {
    if k.modifiers & modifiers::CTRL != 0
        && let Some(c) = letter(k.code)
    {
        return Some(alloc::vec![c - b'a' + 1]);
    }
    if let Some(c) = k.ch {
        let mut buf = [0u8; 4];
        return Some(c.encode_utf8(&mut buf).as_bytes().to_vec());
    }
    let seq: &[u8] = match k.code {
        keys::ENTER | keys::KPENTER => b"\r",
        keys::BACKSPACE => b"\x7f",
        keys::TAB => b"\t",
        keys::ESC => b"\x1b",
        keys::UP => b"\x1b[A",
        keys::DOWN => b"\x1b[B",
        keys::RIGHT => b"\x1b[C",
        keys::LEFT => b"\x1b[D",
        keys::HOME => b"\x1b[H",
        keys::END => b"\x1b[F",
        keys::INSERT => b"\x1b[2~",
        keys::DELETE => b"\x1b[3~",
        keys::PAGEUP => b"\x1b[5~",
        keys::PAGEDOWN => b"\x1b[6~",
        _ => return None,
    };
    Some(seq.to_vec())
}

fn main() -> i32 {
    let args = vrt::env::args();
    let mut cwd = String::from(HOME);
    let mut command: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "-c" {
            command = Some(args[i + 1..].join(" "));
            break;
        }
        cwd = args[i].clone();
        i += 1;
    }
    let Some(mut term) = Terminal::new(&cwd) else {
        vrt::println!("cannot create events");
        return 1;
    };
    if let Some(cmd) = command {
        term.input = cmd;
        term.submit();
    }
    let mut spec = WindowSpec::new("Terminal", 880, 560);
    spec.app_id = "terminal".into();
    spec.min_width = 420;
    spec.min_height = 240;
    vui::run(spec, term)
}
