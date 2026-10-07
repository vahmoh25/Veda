//! The command interpreter ("vsh").
//!
//! A command line is tokenised (quotes, backslash escapes, `$VAR` and `~`
//! expansion, `*`/`?` globbing), split into lists (`;`, `&&`, `||`) of
//! pipelines (`|`) of commands with redirections (`>`, `>>`, `<`), and run.
//! Every command is built in (see `commands.rs`) except programs and
//! installed applications, which are started through the launcher.
//!
//! Commands write their standard output through [`Io`]: either straight to
//! the terminal (with colours, through the [`Output`] queue, as commands run
//! on a thread of their own) or into a capture buffer for a pipe or a
//! redirection. Error messages always go to the terminal. Ctrl+C sets the
//! shell's interrupt flag: the rest of the command line is skipped, and
//! commands that take long check it as they go.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vproto::init::{AppInfo, LaunchError, launcher};

use crate::commands;
use crate::job::Output;
use crate::program::{self, Console, Destination, Input, Run};
use crate::screen::{Cell, Style, cells, color};
use vfiles::path::{display_path, file_name, glob_match, has_wildcards, is_read_only, normalize, resolve};
use vfiles::{Fs, HOME};

/// Shown after an operation failed because the disk is full.
const NO_SPACE_HINT: &str = "The disk is full: delete files you no longer need ('df' shows how much space is left).";

/// The command history file (kept across sessions).
const HISTORY_FILE: &str = "/home/user/.vsh_history";
/// Commands kept in the history.
const MAX_HISTORY: usize = 500;
/// The exit status of a command stopped with Ctrl+C (as in Unix shells).
pub const INTERRUPTED: i32 = 130;

/// Where a command's standard output goes.
pub struct Io<'a> {
    /// The terminal (also receives error messages).
    pub out: &'a Output,
    /// `Some` when the output is captured for a pipe or redirection.
    pub capture: Option<String>,
    /// Standard input from a pipe or `<` redirection.
    pub stdin: Option<String>,
}

impl Io<'_> {
    /// True if output goes to the terminal (so colours are visible).
    pub fn is_terminal(&self) -> bool {
        self.capture.is_none()
    }

    /// Writes text in a style.
    pub fn styled(&mut self, s: &str, style: Style) {
        match &mut self.capture {
            Some(buf) => buf.push_str(s),
            None => self.out.styled(s, style),
        }
    }

    /// Writes plain text.
    pub fn print(&mut self, s: &str) {
        self.styled(s, Style::PLAIN);
    }

    /// Writes text and a newline.
    pub fn println(&mut self, s: &str) {
        self.styled(s, Style::PLAIN);
        self.styled("\n", Style::PLAIN);
    }

    /// Writes text that may contain ANSI colour sequences.
    pub fn raw(&mut self, s: &str) {
        match &mut self.capture {
            Some(buf) => buf.push_str(s),
            None => self.out.raw(s),
        }
    }

    /// Prints `cmd: message` in red on the terminal; returns exit status 1.
    pub fn error(&mut self, cmd: &str, msg: &str) -> i32 {
        self.out.finish_line();
        self.out.styled(&format!("{cmd}: {msg}\n"), Style::ERROR);
        1
    }

    /// Prints a hint in grey on the terminal.
    pub fn hint(&mut self, msg: &str) {
        self.out.styled(&format!("{msg}\n"), Style::DIM);
    }

    /// Prints `cmd: what: error` for a failed file operation (with a hint
    /// when the disk is full); returns exit status 1.
    pub fn fs_error(&mut self, cmd: &str, what: &str, e: &vfiles::Error) -> i32 {
        let status = self.error(cmd, &format!("{what}: {e}"));
        if e.is_no_space() {
            self.hint(NO_SPACE_HINT);
        }
        status
    }

    /// Takes standard input (if any).
    pub fn take_stdin(&mut self) -> Option<String> {
        self.stdin.take()
    }
}

/// A token of the command language.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A word and whether it contains unquoted wildcards.
    Word(String, bool),
    Pipe,
    Semi,
    And,
    Or,
    Out,
    Append,
    In,
}

/// Where an item of a command list starts (a character index) and how it
/// is connected to the item before it.
type ListBreak = (usize, Connector);

/// One command of a pipeline.
#[derive(Debug, Default)]
struct Command {
    words: Vec<(String, bool)>,
    stdin: Option<String>,
    stdout: Option<(String, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connector {
    Always,
    And,
    Or,
}

/// The result of Tab completion.
#[derive(Debug, Default)]
pub struct Completion {
    /// Byte offset of the word being completed.
    pub start: usize,
    /// Replacement for the input between `start` and the cursor.
    pub replacement: Option<String>,
    /// Every match (shown when the completion is ambiguous).
    pub candidates: Vec<String>,
}

/// The interpreter state.
pub struct Shell {
    pub cwd: String,
    pub prev_dir: Option<String>,
    pub env: Vec<(String, String)>,
    pub history: Vec<String>,
    /// Exit status of the last command.
    pub status: i32,
    pub fs: Fs,
    launcher: Option<launcher::Client>,
    apps: Option<Vec<AppInfo>>,
    /// The `exit` command ran.
    pub exit_requested: bool,
    /// Terminal width in columns (for formatting tables).
    pub cols: usize,
    /// Set (by Ctrl+C) to stop the command that is running.
    pub interrupt: Arc<AtomicBool>,
    /// The terminal programs run in (none without a window).
    pub console: Option<Arc<Console>>,
}

impl Shell {
    pub fn new(cwd: &str) -> Shell {
        let fs = Fs::connect();
        let cwd = if fs.is_dir(cwd) { normalize(cwd) } else { HOME.to_string() };
        let env = alloc::vec![
            ("HOME".to_string(), HOME.to_string()),
            ("USER".to_string(), "user".to_string()),
            ("HOSTNAME".to_string(), "veda".to_string()),
            ("SHELL".to_string(), "vsh".to_string()),
            ("PATH".to_string(), "/system/bin".to_string()),
            ("PWD".to_string(), cwd.clone()),
            ("TERM".to_string(), "veda-terminal".to_string()),
        ];
        Shell {
            cwd,
            prev_dir: None,
            env,
            history: Vec::new(),
            status: 0,
            fs,
            launcher: None,
            apps: None,
            exit_requested: false,
            cols: 80,
            interrupt: Arc::new(AtomicBool::new(false)),
            console: None,
        }
    }

    /// The command that is running should stop (Ctrl+C was pressed).
    pub fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::Relaxed)
    }

    /// The prompt: `user@veda:~/Documents$ `.
    pub fn prompt(&self) -> Vec<Cell> {
        let mut c = cells("user@veda", Style::fg(color::BRIGHT_GREEN).bold());
        c.extend(cells(":", Style::PLAIN));
        c.extend(cells(&display_path(&self.cwd), Style::fg(color::BRIGHT_BLUE).bold()));
        c.extend(cells("$ ", Style::PLAIN));
        c
    }

    pub fn var(&self, key: &str) -> Option<String> {
        if key == "?" {
            return Some(self.status.to_string());
        }
        if key == "PWD" {
            return Some(self.cwd.clone());
        }
        self.env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    pub fn set_var(&mut self, key: &str, value: &str) {
        match self.env.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value.to_string(),
            None => self.env.push((key.to_string(), value.to_string())),
        }
    }

    pub fn unset_var(&mut self, key: &str) {
        self.env.retain(|(k, _)| k != key);
    }

    /// Changes the working directory.
    pub fn set_cwd(&mut self, dir: String) {
        if dir != self.cwd {
            self.prev_dir = Some(core::mem::replace(&mut self.cwd, dir));
            let cwd = self.cwd.clone();
            self.set_var("PWD", &cwd);
        }
    }

    /// Absolute form of a path argument.
    pub fn resolve(&self, p: &str) -> String {
        resolve(&self.cwd, p)
    }

    /// Adds a line to the history (skipping blanks and repeats) and to the
    /// history file.
    pub fn remember(&mut self, line: &str) {
        let redacted = self.redact(line.trim());
        let line = redacted.as_str();
        if line.is_empty() || self.history.last().is_some_and(|l| l == line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        let _ = self.fs.append(HISTORY_FILE, format!("{line}\n").as_bytes());
    }

    /// A command line as it may be kept in the history (or read by the
    /// agent): Wi-Fi passwords (`wifi connect SSID PASSWORD`) are replaced by
    /// stars.
    pub fn redact(&self, line: &str) -> String {
        if let Ok(toks) = self.tokenize(line) {
            let words: Vec<&str> = toks
                .iter()
                .map_while(|t| match t {
                    Tok::Word(w, _) => Some(w.as_str()),
                    _ => None,
                })
                .collect();
            if words.len() >= 4 && words[0] == "wifi" && words[1] == "connect" && words.len() == toks.len() {
                return format!("wifi connect {} ********", escape(words[2]));
            }
        }
        line.to_string()
    }

    /// Loads the history saved by earlier sessions (keeping the file short).
    pub fn load_history(&mut self) {
        let Ok(data) = self.fs.read(HISTORY_FILE) else { return };
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(MAX_HISTORY);
        self.history = lines[start..].iter().map(|l| l.to_string()).collect();
        if start > 0 {
            let mut trimmed = self.history.join("\n");
            trimmed.push('\n');
            let _ = self.fs.write(HISTORY_FILE, trimmed.as_bytes());
        }
    }

    /// Forgets the history, also on disk.
    pub fn clear_history(&mut self) {
        self.history.clear();
        let _ = self.fs.remove(HISTORY_FILE);
    }

    // ---- services ----------------------------------------------------------

    pub fn launcher(&mut self) -> Option<&launcher::Client> {
        if self.launcher.is_none() {
            self.launcher = vproto::connect(launcher::NAME).ok().map(launcher::Client::new);
        }
        self.launcher.as_ref()
    }

    /// Installed applications (cached).
    pub fn apps(&mut self) -> Vec<AppInfo> {
        if self.apps.is_none() {
            let list = self.launcher().and_then(|l| l.apps().ok());
            if let Some(list) = list {
                self.apps = Some(list);
            }
        }
        self.apps.clone().unwrap_or_default()
    }

    /// Names of the programs in `/system/bin` (without `.exe`).
    pub fn programs(&self) -> Vec<String> {
        self.fs
            .read_dir("/system/bin")
            .map(|v| v.into_iter().filter_map(|e| e.name.strip_suffix(".exe").map(|s| s.to_string())).collect())
            .unwrap_or_default()
    }

    /// Starts a program or application. `prog` is an application id, a
    /// program name from `/system/bin` or a path to an `.exe`. Returns a
    /// display name and the process id.
    pub fn launch(&mut self, prog: &str, args: Vec<String>) -> Result<(String, u64), String> {
        let as_path = prog.contains('/') || prog.ends_with(".exe");
        if !as_path {
            let apps = self.apps();
            let lower = prog.to_ascii_lowercase();
            if let Some(app) = apps.iter().find(|a| a.id == lower || a.name.to_ascii_lowercase() == lower) {
                let (id, name) = (app.id.clone(), app.name.clone());
                let r = self.launcher().ok_or("the launcher is unavailable")?.launch_app(id, args);
                return launch_result(r).map(|koid| (name, koid));
            }
        }
        let mut path = if as_path { self.resolve(prog) } else { format!("/system/bin/{prog}.exe") };
        if !path.ends_with(".exe") && !self.fs.exists(&path) {
            path.push_str(".exe");
        }
        match self.fs.stat(&path) {
            Ok(st) if !st.is_dir => {}
            _ => return Err(format!("{prog}: command not found")),
        }
        if !is_read_only(&path) {
            return Err(format!("{prog}: only programs installed in /system/bin can be started"));
        }
        let name = file_name(&path).trim_end_matches(".exe").to_string();
        let r = self.launcher().ok_or("the launcher is unavailable")?.launch(path, args);
        launch_result(r).map(|koid| (name, koid))
    }

    /// The program a command name runs as a child of the terminal: a path
    /// (with a `/`) to a file, or a file of that name (or that name plus
    /// `.exe`) in a directory of `PATH`. Built-in commands and installed
    /// applications come first: they are not programs here.
    pub fn program_for(&mut self, name: &str) -> Option<String> {
        if commands::find(name).is_some() {
            return None;
        }
        let is_file = |fs: &Fs, p: &str| fs.stat(p).is_ok_and(|s| !s.is_dir);
        if name.contains('/') {
            let path = self.resolve(name);
            return is_file(&self.fs, &path).then_some(path);
        }
        let lower = name.to_ascii_lowercase();
        if self.apps().iter().any(|a| a.id == lower || a.name.to_ascii_lowercase() == lower) {
            return None;
        }
        let dirs = self.var("PATH").unwrap_or_default();
        for dir in dirs.split(':').filter(|d| d.starts_with('/')) {
            for candidate in [format!("{dir}/{name}"), format!("{dir}/{name}.exe")] {
                if is_file(&self.fs, &candidate) {
                    return Some(normalize(&candidate));
                }
            }
        }
        None
    }

    /// Runs the program at `path` for `cmd` (one command of a pipeline) and
    /// waits for it. Its input is the terminal, `piped` (the output of the
    /// command before) or a file; its output the terminal, a file, or (if it
    /// is not the `last` command) kept for the next one. Returns its exit
    /// status and the output kept, or the status of a failure to start it.
    fn run_program(
        &mut self,
        path: &str,
        args: &[String],
        cmd: &Command,
        piped: Option<Vec<u8>>,
        last: bool,
        out: &Output,
    ) -> Result<(i32, Option<Vec<u8>>), i32> {
        let Some(console) = self.console.clone() else {
            out.styled("vsh: programs cannot run here (no terminal)\n", Style::ERROR);
            return Err(1);
        };
        let open = |sh: &Self, p: &str, flags: u32| -> Result<vrt::object::Channel, i32> {
            let abs = sh.resolve(p);
            let r = sh.fs.client().map(|c| c.open_file(abs, flags));
            match r {
                Some(Ok(Ok((ch, _)))) => Ok(ch),
                Some(Ok(Err(e))) => {
                    out.styled(&format!("vsh: {p}: {e}\n"), Style::ERROR);
                    Err(1)
                }
                _ => {
                    out.styled("vsh: the file system is not responding\n", Style::ERROR);
                    Err(1)
                }
            }
        };
        use vproto::fs::open_flags as of;
        let stdin = match (&cmd.stdin, piped) {
            (Some(p), _) => Input::File(open(self, p, of::READ)?),
            (None, Some(text)) => Input::Text(text),
            (None, None) => Input::Terminal,
        };
        let stdout = match &cmd.stdout {
            Some((p, false)) => Destination::File(open(self, p, of::WRITE | of::CREATE | of::TRUNCATE)?),
            Some((p, true)) => Destination::File(open(self, p, of::WRITE | of::CREATE | of::APPEND)?),
            None if !last => Destination::Capture,
            None => Destination::Terminal,
        };
        let capturing = matches!(stdout, Destination::Capture);
        let mut env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        env.retain(|e| !e.starts_with("PWD="));
        env.push(format!("PWD={}", self.cwd));
        let run = Run { path, args, env, cwd: &self.cwd, stdin, stdout };
        match program::run(&console, run, out, &self.interrupt) {
            Ok(ended) => {
                if let Some(reason) = ended.reason {
                    out.finish_line();
                    out.styled(&format!("{reason}\n"), Style::ERROR);
                }
                Ok((ended.status, capturing.then_some(ended.captured)))
            }
            Err(msg) => {
                out.finish_line();
                out.styled(&format!("vsh: {msg}\n"), Style::ERROR);
                Err(126)
            }
        }
    }

    // ---- execution ---------------------------------------------------------

    /// Runs a command line, writing output to `out`. After Ctrl+C the rest
    /// of the line is skipped.
    pub fn execute(&mut self, line: &str, out: &Output) {
        // The whole line is checked first; then each item of the list is
        // expanded when its turn comes, so that it sees what the items
        // before it did (`prog; echo $?`).
        let breaks = match self.tokenize_list(line).and_then(|(toks, breaks)| parse(toks).map(|_| breaks)) {
            Ok(b) => b,
            Err(e) => {
                out.styled(&format!("vsh: {e}\n"), Style::ERROR);
                self.status = 2;
                return;
            }
        };
        let chars: Vec<char> = line.chars().collect();
        let mut items = Vec::with_capacity(breaks.len() + 1);
        let mut start = (0, Connector::Always);
        for &b in &breaks {
            items.push((start.1, chars[start.0..b.0].iter().collect::<String>()));
            start = b;
        }
        items.push((start.1, chars[start.0..].iter().collect::<String>()));
        for (conn, text) in items {
            if self.interrupted() {
                break;
            }
            match conn {
                Connector::And if self.status != 0 => continue,
                Connector::Or if self.status == 0 => continue,
                _ => {}
            }
            // The item without its separator, expanded now.
            let toks = match self.tokenize(&text) {
                Ok(t) => t.into_iter().filter(|t| !matches!(t, Tok::Semi | Tok::And | Tok::Or)).collect(),
                Err(e) => {
                    out.styled(&format!("vsh: {e}\n"), Style::ERROR);
                    self.status = 2;
                    return;
                }
            };
            let Some((_, pipeline)) = parse(toks).ok().and_then(|l| l.into_iter().next()) else { continue };
            self.status = self.run_pipeline(pipeline, out);
            out.finish_line();
            if self.exit_requested {
                break;
            }
        }
        if self.interrupted() {
            self.status = INTERRUPTED;
        }
    }

    fn run_pipeline(&mut self, cmds: Vec<Command>, out: &Output) -> i32 {
        let n = cmds.len();
        let mut piped: Option<Vec<u8>> = None;
        let mut status = 0;
        for (i, cmd) in cmds.into_iter().enumerate() {
            if self.interrupted() {
                return INTERRUPTED;
            }
            let last = i + 1 == n;
            let args = self.expand(&cmd.words);
            // Programs get their redirections as files and pipes.
            if let Some(name) = args.first()
                && let Some(path) = self.program_for(name)
            {
                match self.run_program(&path, &args, &cmd, piped.take(), last, out) {
                    Ok((s, captured)) => {
                        status = s;
                        piped = captured;
                    }
                    Err(s) => return s,
                }
                if self.exit_requested {
                    break;
                }
                continue;
            }
            let mut stdin = piped.take().map(|b| String::from_utf8_lossy(&b).into_owned());
            if let Some(path) = &cmd.stdin {
                let abs = self.resolve(path);
                match self.fs.read(&abs) {
                    Ok(data) => stdin = Some(String::from_utf8_lossy(&data).into_owned()),
                    Err(e) => {
                        out.styled(&format!("vsh: {path}: {e}\n"), Style::ERROR);
                        return 1;
                    }
                }
            }
            let capture = !last || cmd.stdout.is_some();
            let mut io = Io { out, capture: if capture { Some(String::new()) } else { None }, stdin };
            status = if args.is_empty() { 0 } else { self.run_command(&args, &mut io) };
            let captured = io.capture.take();
            if let Some((path, append)) = &cmd.stdout {
                let abs = self.resolve(path);
                let data = captured.unwrap_or_default();
                let r =
                    if *append { self.fs.append(&abs, data.as_bytes()) } else { self.fs.write(&abs, data.as_bytes()) };
                if let Err(e) = r {
                    out.finish_line();
                    out.styled(&format!("vsh: {path}: {e}\n"), Style::ERROR);
                    if e.is_no_space() {
                        out.styled(&format!("{NO_SPACE_HINT}\n"), Style::DIM);
                    }
                    status = 1;
                }
            } else if !last {
                piped = captured.map(String::into_bytes);
            }
            if self.exit_requested {
                break;
            }
        }
        status
    }

    fn run_command(&mut self, args: &[String], io: &mut Io) -> i32 {
        let name = args[0].as_str();
        if let Some(b) = commands::find(name) {
            return (b.run)(self, io, args);
        }
        // Not a built-in: maybe an application or program.
        let rest: Vec<String> = args[1..].iter().map(|a| self.absolute_arg(a)).collect();
        match self.launch(name, rest) {
            Ok((app, koid)) => {
                io.styled(&format!("Started {app} (PID {koid})\n"), Style::DIM);
                0
            }
            Err(e) if e.ends_with("command not found") => {
                io.error("vsh", &format!("command not found: {name}"));
                io.hint("Type 'help' to see the available commands, or 'apps' for the installed applications.");
                127
            }
            Err(e) => io.error("vsh", &e),
        }
    }

    /// Turns an argument that names an existing file into an absolute path
    /// (programs do not share the terminal's working directory).
    pub fn absolute_arg(&self, a: &str) -> String {
        if a.starts_with('-') {
            return a.to_string();
        }
        let abs = self.resolve(a);
        if !a.starts_with('/') && self.fs.exists(&abs) { abs } else { a.to_string() }
    }

    /// Expands wildcards in words (patterns without matches stay as typed).
    fn expand(&self, words: &[(String, bool)]) -> Vec<String> {
        let mut out = Vec::new();
        for (w, glob) in words {
            if !*glob || !has_wildcards(w) {
                out.push(w.clone());
                continue;
            }
            let (dir_typed, pattern) = match w.rfind('/') {
                Some(i) => (&w[..=i], &w[i + 1..]),
                None => ("", w.as_str()),
            };
            if has_wildcards(dir_typed) {
                out.push(w.clone());
                continue;
            }
            let dir = if dir_typed.is_empty() { self.cwd.clone() } else { self.resolve(dir_typed) };
            let mut matches: Vec<String> = self
                .fs
                .read_dir(&dir)
                .unwrap_or_default()
                .into_iter()
                .filter(|e| (!e.name.starts_with('.') || pattern.starts_with('.')) && glob_match(pattern, &e.name))
                .map(|e| format!("{dir_typed}{}", e.name))
                .collect();
            if matches.is_empty() {
                out.push(w.clone());
            } else {
                matches.sort();
                out.extend(matches);
            }
        }
        out
    }

    /// Splits a command line into tokens.
    fn tokenize(&self, line: &str) -> Result<Vec<Tok>, String> {
        self.tokenize_list(line).map(|(toks, _)| toks)
    }

    /// Splits a command line into tokens, and also returns where each list
    /// separator (`;`, `&&`, `||`) ends: the character index of the next
    /// list item and how it is connected.
    fn tokenize_list(&self, line: &str) -> Result<(Vec<Tok>, Vec<ListBreak>), String> {
        let mut toks = Vec::new();
        let mut breaks = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut word = String::new();
        let mut in_word = false;
        let mut glob = false;
        macro_rules! end_word {
            () => {
                if in_word {
                    toks.push(Tok::Word(core::mem::take(&mut word), glob));
                    in_word = false;
                    glob = false;
                }
            };
        }
        while i < chars.len() {
            let c = chars[i];
            match c {
                ' ' | '\t' => end_word!(),
                '#' if !in_word => break,
                '|' => {
                    end_word!();
                    if chars.get(i + 1) == Some(&'|') {
                        i += 1;
                        toks.push(Tok::Or);
                        breaks.push((i + 1, Connector::Or));
                    } else {
                        toks.push(Tok::Pipe);
                    }
                }
                '&' if chars.get(i + 1) == Some(&'&') => {
                    end_word!();
                    i += 1;
                    toks.push(Tok::And);
                    breaks.push((i + 1, Connector::And));
                }
                ';' => {
                    end_word!();
                    toks.push(Tok::Semi);
                    breaks.push((i + 1, Connector::Always));
                }
                '>' => {
                    end_word!();
                    if chars.get(i + 1) == Some(&'>') {
                        i += 1;
                        toks.push(Tok::Append);
                    } else {
                        toks.push(Tok::Out);
                    }
                }
                '<' => {
                    end_word!();
                    toks.push(Tok::In);
                }
                '\'' => {
                    in_word = true;
                    i += 1;
                    while i < chars.len() && chars[i] != '\'' {
                        word.push(chars[i]);
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err("unterminated quote (')".into());
                    }
                }
                '"' => {
                    in_word = true;
                    i += 1;
                    loop {
                        let Some(&c) = chars.get(i) else { return Err("unterminated quote (\")".into()) };
                        match c {
                            '"' => break,
                            '\\' if matches!(chars.get(i + 1), Some('"' | '\\' | '$' | '`')) => {
                                word.push(chars[i + 1]);
                                i += 2;
                            }
                            '$' => i = self.expand_var(&chars, i, &mut word),
                            c => {
                                word.push(c);
                                i += 1;
                            }
                        }
                    }
                }
                '\\' => {
                    in_word = true;
                    if let Some(&n) = chars.get(i + 1) {
                        word.push(n);
                        i += 1;
                    }
                }
                '$' => {
                    in_word = true;
                    i = self.expand_var(&chars, i, &mut word);
                    continue;
                }
                '~' if !in_word && matches!(chars.get(i + 1), None | Some('/' | ' ' | ';' | '|')) => {
                    in_word = true;
                    word.push_str(HOME);
                }
                c => {
                    if c == '*' || c == '?' {
                        glob = true;
                    }
                    in_word = true;
                    word.push(c);
                }
            }
            i += 1;
        }
        if in_word {
            toks.push(Tok::Word(word, glob));
        }
        Ok((toks, breaks))
    }

    /// Expands `$NAME`, `${NAME}` or `$?` starting at `chars[i]` (the `$`);
    /// returns the index after it.
    fn expand_var(&self, chars: &[char], i: usize, out: &mut String) -> usize {
        let mut j = i + 1;
        let name: String = if chars.get(j) == Some(&'{') {
            let start = j + 1;
            let mut k = start;
            while k < chars.len() && chars[k] != '}' {
                k += 1;
            }
            j = (k + 1).min(chars.len());
            chars[start..k.min(chars.len())].iter().collect()
        } else if chars.get(j) == Some(&'?') {
            j += 1;
            "?".into()
        } else {
            let start = j;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            chars[start..j].iter().collect()
        };
        if name.is_empty() {
            out.push('$');
        } else if let Some(v) = self.var(&name) {
            out.push_str(&v);
        }
        j
    }

    // ---- completion --------------------------------------------------------

    /// Completes the word before byte offset `cursor` of `input`.
    pub fn complete(&mut self, input: &str, cursor: usize) -> Completion {
        let before = &input[..cursor];
        // Find the start of the current word (unquoted whitespace or operator).
        let mut start = 0;
        let mut quote: Option<char> = None;
        let mut escaped = false;
        for (i, c) in before.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match (quote, c) {
                (_, '\\') if quote != Some('\'') => escaped = true,
                (None, '\'' | '"') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, ' ' | '\t' | '|' | ';' | '&' | '<' | '>') => start = i + c.len_utf8(),
                _ => {}
            }
        }
        let typed = unescape(&before[start..]);
        let prefix_text = before[..start].trim_end();
        let command_position = prefix_text.is_empty() || prefix_text.ends_with(['|', ';', '&']);
        let mut out = Completion { start, ..Default::default() };

        if command_position && !typed.contains('/') && !typed.starts_with('.') && !typed.starts_with('~') {
            let mut names: Vec<String> = commands::names().into_iter().map(|s| s.to_string()).collect();
            names.extend(self.apps().into_iter().map(|a| a.id));
            names.extend(self.programs());
            names.sort();
            names.dedup();
            let matches: Vec<String> = names.into_iter().filter(|n| n.starts_with(typed.as_str())).collect();
            match matches.len() {
                0 => {}
                1 => out.replacement = Some(format!("{} ", escape(&matches[0]))),
                _ => {
                    let lcp = common_prefix(&matches);
                    if lcp.len() > typed.len() {
                        out.replacement = Some(escape(&lcp));
                    }
                }
            }
            out.candidates = matches;
            return out;
        }

        // Path completion.
        let (dir_typed, prefix) = match typed.rfind('/') {
            Some(i) => (typed[..=i].to_string(), typed[i + 1..].to_string()),
            None => (String::new(), typed.clone()),
        };
        let dir = if dir_typed.is_empty() { self.cwd.clone() } else { self.resolve(&dir_typed) };
        let entries = self.fs.read_dir(&dir).unwrap_or_default();
        let visible = |name: &str| !name.starts_with('.') || prefix.starts_with('.');
        let mut matches: Vec<(String, bool)> = entries
            .iter()
            .filter(|e| visible(&e.name) && e.name.starts_with(prefix.as_str()))
            .map(|e| (e.name.clone(), e.is_dir))
            .collect();
        if matches.is_empty() {
            // Fall back to a case-insensitive match.
            let lower = prefix.to_lowercase();
            matches = entries
                .iter()
                .filter(|e| visible(&e.name) && e.name.to_lowercase().starts_with(&lower))
                .map(|e| (e.name.clone(), e.is_dir))
                .collect();
        }
        out.candidates = matches.iter().map(|(n, d)| if *d { format!("{n}/") } else { n.clone() }).collect();
        match matches.len() {
            0 => {}
            1 => {
                let (name, is_dir) = &matches[0];
                let suffix = if *is_dir { "/" } else { " " };
                out.replacement = Some(format!("{}{}", escape(&format!("{dir_typed}{name}")), suffix));
            }
            _ => {
                let names: Vec<String> = matches.iter().map(|(n, _)| n.clone()).collect();
                let lcp = common_prefix(&names);
                if lcp.chars().count() > prefix.chars().count() || (lcp.len() == prefix.len() && lcp != prefix) {
                    out.replacement = Some(escape(&format!("{dir_typed}{lcp}")));
                }
            }
        }
        out
    }
}

fn launch_result(r: Result<Result<u64, LaunchError>, vipc::IpcError>) -> Result<u64, String> {
    match r {
        Ok(Ok(koid)) => Ok(koid),
        Ok(Err(e)) => Err(launch_error(e).to_string()),
        Err(_) => Err("the launcher is not responding".to_string()),
    }
}

/// A readable message for a launcher error.
pub fn launch_error(e: LaunchError) -> &'static str {
    match e {
        LaunchError::NotFound => "program not found",
        LaunchError::BadImage => "not a valid Veda program",
        LaunchError::NoMemory => "not enough memory",
        LaunchError::Denied => "permission denied",
        LaunchError::Failed => "the program could not be started",
    }
}

/// Splits tokens into connected pipelines of commands.
fn parse(toks: Vec<Tok>) -> Result<Vec<(Connector, Vec<Command>)>, String> {
    let mut list = Vec::new();
    let mut conn = Connector::Always;
    let mut pipeline: Vec<Command> = Vec::new();
    let mut cmd = Command::default();
    let mut it = toks.into_iter();
    fn finish_cmd(cmd: &mut Command, pipeline: &mut Vec<Command>, sep: &str) -> Result<(), String> {
        if cmd.words.is_empty() {
            if cmd.stdout.is_some() || cmd.stdin.is_some() {
                return Err("missing command before redirection".into());
            }
            return Err(format!("syntax error near '{sep}'"));
        }
        pipeline.push(core::mem::take(cmd));
        Ok(())
    }
    while let Some(t) = it.next() {
        match t {
            Tok::Word(w, g) => cmd.words.push((w, g)),
            Tok::Out | Tok::Append | Tok::In => {
                let Some(Tok::Word(target, _)) = it.next() else {
                    return Err("missing file name after redirection".into());
                };
                match t {
                    Tok::Out => cmd.stdout = Some((target, false)),
                    Tok::Append => cmd.stdout = Some((target, true)),
                    _ => cmd.stdin = Some(target),
                }
            }
            Tok::Pipe => finish_cmd(&mut cmd, &mut pipeline, "|")?,
            Tok::Semi | Tok::And | Tok::Or => {
                let sep = match t {
                    Tok::Semi => ";",
                    Tok::And => "&&",
                    _ => "||",
                };
                if cmd.words.is_empty() && pipeline.is_empty() && t == Tok::Semi {
                    continue; // empty statement
                }
                finish_cmd(&mut cmd, &mut pipeline, sep)?;
                list.push((conn, core::mem::take(&mut pipeline)));
                conn = match t {
                    Tok::And => Connector::And,
                    Tok::Or => Connector::Or,
                    _ => Connector::Always,
                };
            }
        }
    }
    if !cmd.words.is_empty() || cmd.stdout.is_some() || cmd.stdin.is_some() {
        finish_cmd(&mut cmd, &mut pipeline, "end of line")?;
    } else if !pipeline.is_empty() || conn != Connector::Always {
        return Err("unexpected end of line".into());
    }
    if !pipeline.is_empty() {
        list.push((conn, pipeline));
    }
    Ok(list)
}

/// Escapes characters that the tokenizer treats specially.
pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, ' ' | '\'' | '"' | '\\' | '|' | ';' | '&' | '<' | '>' | '(' | ')' | '$' | '*' | '?' | '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Removes quotes and backslash escapes from a partial word.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (q, '\\') if q != Some('\'') => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Longest common prefix of several strings.
fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else { return String::new() };
    let mut len = first.len();
    for s in &items[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    while !first.is_char_boundary(len) {
        len -= 1;
    }
    first[..len].to_owned()
}
