//! The Terminal for the voice agent: what it shows (the latest output, the
//! folder it is in, the command running) and what it does (running a
//! command line as the user would type it, stopping it, clearing the
//! screen, the text size), through the same paths as the keyboard.
//!
//! Command lines read back by the agent have Wi-Fi passwords hidden, as in
//! the command history.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vui::agent::{self, Action, AppAgentInfo, Risk, Value, arg_f64, arg_opt_int, arg_str, clip, object, show_path};

use crate::{DEFAULT_FONT_SIZE, Terminal};

/// The longest command line the agent may run: the user's approval shows
/// 80 characters of it, so all of it.
const MAX_COMMAND: usize = 80;
/// How long `run_command` waits for the command (the agent gives an
/// application three seconds to answer).
const RUN_WAIT_NS: u64 = 2_000_000_000;
/// How long `interrupt` waits for the command to stop.
const STOP_WAIT_NS: u64 = 1_000_000_000;
/// The most output the agent gets at once (bytes).
const MAX_OUTPUT: usize = 6000;
/// The most output in the state (bytes).
const STATE_OUTPUT: usize = 2500;
/// The lines of output `read_output` returns unless asked for more, and the
/// most it returns.
const READ_LINES: i64 = 40;
const MAX_READ_LINES: i64 = 200;

pub fn info() -> AppAgentInfo {
    agent::info(
        "A terminal with vsh, the Vindows command shell: it runs commands for files, programs, the network and the \
         system, and shows what they print.",
        vec![
            Action::new("run_command", "Runs a command in the terminal")
                .param(
                    "command",
                    "string",
                    "The command line exactly as the user would type it: one line of at most 80 characters, such as \
                     ls ~/Documents. It runs in the terminal's current folder (cd changes it). Returns what it \
                     printed, waiting up to 2 seconds; a longer command keeps running.",
                    true,
                )
                .risk(Risk::Sensitive)
                .build(),
            Action::new("read_output", "Returns the terminal's latest output and whether a command is still running")
                .param("lines", "integer", "How many lines from the end (40 by default, at most 200)", false)
                .build(),
            Action::new("interrupt", "Stops the command that is running, like Ctrl+C").build(),
            Action::new("clear_screen", "Clears the terminal's screen, like Ctrl+L (the command history stays)")
                .build(),
            Action::new("set_text_size", "Changes the size of the terminal's text")
                .param("points", "number", "The size in points, 9 to 28 (14 is normal)", true)
                .build(),
        ],
    )
}

/// `text`, or its start and its end when it is longer than `max` bytes
/// (whole lines, with a note of how many were left out).
fn clip_middle(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let lines: Vec<&str> = text.lines().collect();
    // Whole lines from the start (about two thirds), then from the end.
    let (mut head, mut size) = (0, 0);
    while head < lines.len() && size + lines[head].len() < max * 2 / 3 {
        size += lines[head].len() + 1;
        head += 1;
    }
    let (mut tail, mut tail_size) = (lines.len(), 0);
    while tail > head && tail_size + lines[tail - 1].len() < max / 3 {
        tail_size += lines[tail - 1].len() + 1;
        tail -= 1;
    }
    let mut out = String::new();
    for l in &lines[..head] {
        out.push_str(l);
        out.push('\n');
    }
    if head == 0 {
        // A first line longer than all that: its start.
        out.push_str(clip(lines[0], max / 2).0);
        out.push_str("\u{2026}\n");
        head = 1;
    }
    if tail > head {
        out.push_str(&format!("[{} lines left out]\n", tail - head));
    }
    for l in &lines[tail.max(head)..] {
        out.push_str(l);
        out.push('\n');
    }
    (out, true)
}

pub fn state(t: &Terminal) -> Value {
    let (output, earlier) = t.tail(usize::MAX, STATE_OUTPUT);
    let mut v = object! { "folder" => t.folder(), "running" => t.running() };
    if t.job.is_none() {
        v.set("last_exit_status", t.last_status);
    }
    if !t.input.is_empty()
        && let Some(shell) = &t.shell
    {
        v.set("typed", shell.redact(&t.input));
    }
    v.set("output", output);
    v.set("earlier_lines", earlier);
    v.set("text_size", t.font_size);
    v
}

impl Terminal {
    /// The working folder as it reads aloud (as of the last command while
    /// one runs).
    fn folder(&self) -> String {
        show_path(self.shell.as_ref().map_or(self.cwd.as_str(), |s| s.cwd.as_str()))
    }

    /// The command running and for how long, or null.
    fn running(&self) -> Value {
        match &self.job {
            Some(job) if !job.finished() => object! {
                "command" => job.shown.as_str(),
                "seconds" => vrt::time::now_ns().saturating_sub(job.started) / 1_000_000_000,
            },
            _ => Value::Null,
        }
    }

    /// Line `abs` of the screen as text (a command line as the agent may
    /// read it).
    fn line_text(&self, abs: u64) -> String {
        if let Ok(i) = self.echoes.binary_search_by_key(&abs, |(line, _)| *line) {
            return self.echoes[i].1.clone();
        }
        let text: String = self.screen.line(abs).unwrap_or(&[]).iter().map(|c| c.ch).collect();
        text.trim_end().to_string()
    }

    /// Lines `from..to` of the screen as text.
    fn text(&self, from: u64, to: u64) -> String {
        let mut out = String::new();
        for abs in from.max(self.screen.base())..to.min(self.screen.end()) {
            out.push_str(&self.line_text(abs));
            out.push('\n');
        }
        out
    }

    /// The last `lines` lines of the screen, at most `max` bytes of them,
    /// and how many lines come before them.
    fn tail(&self, lines: usize, max: usize) -> (String, u64) {
        let (base, end) = (self.screen.base(), self.screen.end());
        let (mut start, mut size) = (end, 0);
        while start > base && ((end - start) as usize) < lines {
            let len = self.line_text(start - 1).len() + 1;
            if size + len > max {
                break;
            }
            size += len;
            start -= 1;
        }
        if start == end && end > base && lines > 0 {
            // A last line longer than all that: its end.
            let line = self.line_text(end - 1);
            let mut cut = line.len() - max.min(line.len());
            while !line.is_char_boundary(cut) {
                cut += 1;
            }
            return (format!("\u{2026}{}\n", &line[cut..]), end - 1 - base);
        }
        (self.text(start, end), start - base)
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        self.collect();
        match action {
            "run_command" => {
                let command = arg_str(args, "command")?.trim();
                if command.is_empty() {
                    return Err("which command? Give the command line to run".into());
                }
                if command.chars().any(char::is_control) {
                    return Err("the command must be a single line".into());
                }
                if command.chars().count() > MAX_COMMAND {
                    return Err(format!(
                        "the command is longer than {MAX_COMMAND} characters; run it in shorter steps"
                    ));
                }
                if let Some(job) = &self.job {
                    return Err(format!(
                        "\u{201c}{}\u{201d} is still running: read_output shows what it prints, interrupt stops it",
                        job.shown
                    ));
                }
                let shown = self.shell.as_ref().map_or_else(|| command.to_string(), |s| s.redact(command));
                // What the user was typing waits while the agent's command
                // runs.
                let typed = core::mem::take(&mut self.input);
                let cursor = self.cursor;
                self.hsearch = None;
                self.input = command.to_string();
                self.cursor = self.input.len();
                // As Enter does: the line is echoed and starts.
                self.submit();
                let from = self.echoes.back().map_or(self.screen.end(), |(line, _)| line + 1);
                let finished = self.wait_for_job(RUN_WAIT_NS);
                self.input = typed;
                self.cursor = cursor.min(self.input.len());
                let (output, cut) = clip_middle(&self.text(from, self.screen.end()), MAX_OUTPUT);
                let mut v = object! { "command" => shown, "finished" => finished, "output" => output };
                if cut {
                    v.set("output_cut_short", true);
                }
                if finished {
                    v.set("exit_status", self.last_status);
                    v.set("folder", self.folder());
                } else {
                    v.set("note", "it is still running: read_output shows what it prints next, interrupt stops it");
                }
                Ok(v)
            }
            "read_output" => {
                let lines = arg_opt_int(args, "lines")?.unwrap_or(READ_LINES).clamp(1, MAX_READ_LINES) as usize;
                let (output, earlier) = self.tail(lines, MAX_OUTPUT);
                let mut v = object! {
                    "output" => output,
                    "earlier_lines" => earlier,
                    "folder" => self.folder(),
                    "running" => self.running(),
                };
                if self.job.is_none() {
                    v.set("last_exit_status", self.last_status);
                }
                Ok(v)
            }
            "interrupt" => {
                let Some(shown) = self.job.as_ref().map(|j| j.shown.clone()) else {
                    return Err("no command is running in the terminal".into());
                };
                self.interrupt();
                let stopped = self.wait_for_job(STOP_WAIT_NS);
                let mut v = object! { "command" => shown, "stopped" => stopped };
                if !stopped {
                    v.set(
                        "note",
                        "it is waiting for something it cannot leave at once (such as the network) and stops when \
                         that ends",
                    );
                }
                Ok(v)
            }
            "clear_screen" => {
                self.clear_screen();
                Ok(object! { "cleared" => true })
            }
            "set_text_size" => {
                let points = arg_f64(args, "points")? as f32;
                self.font_size = if points <= 0.0 { DEFAULT_FONT_SIZE } else { points.clamp(9.0, 28.0) };
                Ok(object! { "text_size" => self.font_size })
            }
            other => Err(format!("the Terminal has no action called {other}")),
        }
    }
}
