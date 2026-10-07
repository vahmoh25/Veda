//! Programs run in the terminal.
//!
//! A program typed at the prompt (found on `PATH`, or named by a path) runs
//! as a child of the terminal, and the shell waits for it. Its standard
//! streams are the terminal: one end of a socket pair whose other end the
//! terminal keeps, plus the terminal's state page (`vproto::tty`), which
//! makes the socket a terminal to the program (`isatty`, its size, its
//! modes). While it runs, its output comes to the screen and the keys typed
//! go to it — a line at a time, edited in the window, in canonical mode;
//! key by key otherwise. Ctrl+C ends it. Redirections give it files instead
//! of the terminal, and text piped from or to a built-in command travels
//! through a socket of its own.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vabi::signals::{PEER_CLOSED, PEER_WRITE_DISABLED, READABLE, SIGNALED, TERMINATED, WRITABLE};
use vabi::startup::role;
use vabi::{Error, map_flags};
use vipc::WaitSet;
use vproto::fs::vfs;
use vproto::tty::{self, Tty};
use vrt::object::{Channel, Event, Handle, Process, Socket};
use vrt::process::{Spawn, SpawnError, format_of};
use vrt::sync::Mutex;

use crate::job::Output;

/// The terminal side of the programs this window runs.
pub struct Console {
    /// The terminal's state, shared with its programs.
    pub tty: Tty,
    /// The window's end of the foreground program's terminal (where typed
    /// input goes), while one runs.
    foreground: Mutex<Option<Arc<Socket>>>,
    /// Wakes the shell waiting for a program (Ctrl+C was pressed).
    wake: Event,
    /// Input typed for a program that it did not read: the next program
    /// gets it, or else the shell, as from a Unix terminal's buffer.
    unread: Mutex<Vec<u8>>,
}

impl Console {
    pub fn new(rows: u16, cols: u16) -> Option<Arc<Console>> {
        Some(Arc::new(Console {
            tty: Tty::create(0, rows, cols)?,
            foreground: Mutex::new(None),
            wake: Event::create().ok()?,
            unread: Mutex::new(Vec::new()),
        }))
    }

    /// Takes the input no program read (for the shell, once the command
    /// line is done).
    pub fn take_unread(&self) -> Vec<u8> {
        core::mem::take(&mut *self.unread.lock())
    }

    /// Whether a program in the foreground reads the terminal.
    pub fn program_running(&self) -> bool {
        self.foreground.lock().is_some()
    }

    /// Sends what was typed to the foreground program.
    pub fn send(&self, bytes: &[u8]) {
        let fg = self.foreground.lock().clone();
        if let Some(s) = fg {
            // The program may have stopped reading: never block the window.
            let _ = s.write(bytes);
        }
    }

    /// Ends the foreground program's input (Ctrl+D): it reads the end of
    /// the file once it has read what is there.
    pub fn end_input(&self) {
        if let Some(s) = self.foreground.lock().as_ref() {
            let _ = s.shutdown();
        }
    }

    /// Wakes the shell waiting for a program (after setting the interrupt
    /// flag).
    pub fn wake(&self) {
        let _ = self.wake.signal();
    }

    /// Whether typed input goes a line at a time (else key by key).
    pub fn canonical(&self) -> bool {
        self.tty.lflag() & tty::lflag::ICANON != 0
    }

    /// Whether typed input is shown.
    pub fn echo(&self) -> bool {
        self.tty.lflag() & tty::lflag::ECHO != 0
    }

    /// Whether Ctrl+C ends the program (else it is sent to it).
    pub fn signals(&self) -> bool {
        self.tty.lflag() & tty::lflag::ISIG != 0
    }
}

/// A program's standard input.
pub enum Input {
    Terminal,
    /// Text from a built-in command or a pipe.
    Text(Vec<u8>),
    /// A file (`< FILE`).
    File(Channel),
}

/// A program's standard output.
pub enum Destination {
    Terminal,
    /// Kept for the next command of a pipe.
    Capture,
    /// A file (`> FILE`, `>> FILE`).
    File(Channel),
}

/// What to run.
pub struct Run<'a> {
    pub path: &'a str,
    /// Arguments, the first being the name the program was invoked by.
    pub args: &'a [String],
    pub env: Vec<String>,
    pub cwd: &'a str,
    pub stdin: Input,
    pub stdout: Destination,
}

/// How a program ended.
pub struct Ended {
    /// The shell's exit status: the program's (0-255), or 128 plus the
    /// signal that ended it.
    pub status: i32,
    /// What ended it, if it did not exit by itself.
    pub reason: Option<&'static str>,
    /// Captured standard output.
    pub captured: Vec<u8>,
}

/// The message for a program that could not start.
fn spawn_error(path: &str, e: SpawnError) -> String {
    match e {
        SpawnError::NotExecutable => format!("{path}: cannot run this file (it is not a program)"),
        SpawnError::TooLarge => format!("{path}: the argument list is too long"),
        e => format!("{path}: {e}"),
    }
}

/// Reads what a socket has without waiting: `Some` with the bytes, `None`
/// at the end of the stream.
fn drain(s: &Socket, into: &mut Vec<u8>) -> Option<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match s.read(&mut buf) {
            Ok(n) => into.extend_from_slice(&buf[..n]),
            Err(Error::ShouldWait) => return Some(()),
            Err(_) => return None,
        }
    }
}

/// Output for the screen, valid UTF-8 up to a character cut at the end
/// (kept in `carry` for the next read).
fn text_of(carry: &mut Vec<u8>) -> String {
    let valid = match core::str::from_utf8(carry) {
        Ok(_) => carry.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => carry.len(),
    };
    let text = String::from_utf8_lossy(&carry[..valid]).into_owned();
    carry.drain(..valid);
    text
}

/// What the exit code of a program means to the shell.
fn ended(code: i64) -> (i32, Option<&'static str>) {
    match vabi::exit_signal(code) {
        None => ((code & 0xff) as i32, None),
        Some(s) => {
            let what = match s {
                4 => "Illegal instruction",
                6 => "Aborted",
                8 => "Floating point exception",
                9 => "Killed",
                11 => "Segmentation fault",
                13 => "Broken pipe",
                15 => "Terminated",
                _ => "Ended by a signal",
            };
            (128 + s as i32, Some(what))
        }
    }
}

/// Starts `run` and waits for it, showing its output in `out`. Ctrl+C
/// (`interrupt`, with [`Console::wake`]) ends it.
pub fn run(console: &Console, run: Run, out: &Output, interrupt: &AtomicBool) -> Result<Ended, String> {
    // The program's image.
    let fs = vproto::connect(vfs::NAME).map(vfs::Client::new).map_err(|_| "the file system is unavailable")?;
    let (vmo, size) = match fs.read_file(run.path.into()) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(format!("{}: {e}", run.path)),
        Err(_) => return Err("the file system is not responding".into()),
    };
    let image = vrt::vm::Mapping::new(vmo, (size as usize).max(1), map_flags::READ)
        .map_err(|e| format!("{}: {e}", run.path))?;
    // SAFETY: a read-only mapping of a VMO made for us alone.
    let bytes = unsafe { &image.as_slice()[..size as usize] };
    if format_of(bytes).is_none() {
        return Err(spawn_error(run.path, SpawnError::NotExecutable));
    }

    // The terminal: a fresh pair for each program, so that the end of one
    // program's input (Ctrl+D) is not the next one's.
    let (term, program_side) = Socket::create().map_err(|e| format!("{e}"))?;
    console.tty.set_socket(program_side.0.koid());
    let dup = |s: &Socket| s.duplicate(None).map_err(|e| format!("{e}"));
    // Our own handle to the program's side, to take back what it does not
    // read of the input typed for it; and the input earlier programs left.
    let input_side = dup(&program_side)?;
    let left = core::mem::take(&mut *console.unread.lock());
    if !left.is_empty() {
        let _ = term.write(&left);
    }
    let mut handles: Vec<(u32, Handle)> = Vec::new();
    let mut feed: Option<(Socket, Vec<u8>)> = None;
    match run.stdin {
        Input::Terminal => handles.push((role::FD, dup(&program_side)?.into_handle())),
        Input::File(ch) => handles.push((role::FD, ch.into_handle())),
        Input::Text(text) => {
            let (ours, theirs) = Socket::create().map_err(|e| format!("{e}"))?;
            handles.push((role::FD, theirs.into_handle()));
            feed = Some((ours, text));
        }
    }
    let mut capture: Option<Socket> = None;
    match run.stdout {
        Destination::Terminal => handles.push((role::FD + 1, dup(&program_side)?.into_handle())),
        Destination::File(ch) => handles.push((role::FD + 1, ch.into_handle())),
        Destination::Capture => {
            let (ours, theirs) = Socket::create().map_err(|e| format!("{e}"))?;
            handles.push((role::FD + 1, theirs.into_handle()));
            capture = Some(ours);
        }
    }
    handles.push((role::FD + 2, program_side.into_handle()));
    let registry = vproto::with_registry(|r| r.clone_registry());
    if let Ok(Ok(Ok(ch))) = registry {
        handles.push((role::REGISTRY, ch.into_handle()));
    }
    if let Ok(h) = console.tty.vmo().0.duplicate(None) {
        handles.push((role::TERMINAL, h));
    }

    let name = run.path.rsplit('/').next().unwrap_or(run.path);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let mut spawn = Spawn::new(name).path(run.path).cwd(run.cwd);
    spawn.args = run.args.iter().map(|a| a.as_bytes()).collect();
    spawn.env = run.env.iter().map(|e| e.as_bytes()).collect();
    spawn.handles = handles;
    let process: Process = spawn.start(bytes).map_err(|e| spawn_error(run.path, e))?;
    drop(image);

    let term = Arc::new(term);
    *console.foreground.lock() = Some(term.clone());
    let _ = console.wake.clear();
    let mut captured = Vec::new();
    let mut carry = Vec::new();
    let mut term_open = true;
    let mut killed = false;
    const TERM: u64 = 1;
    const PROCESS: u64 = 2;
    const WAKE: u64 = 3;
    const CAPTURE: u64 = 4;
    const FEED: u64 = 5;
    loop {
        let mut ws = WaitSet::new();
        ws.add(process.raw(), TERMINATED, PROCESS);
        ws.add(console.wake.raw(), SIGNALED, WAKE);
        if term_open {
            ws.add(term.raw(), READABLE | PEER_CLOSED | PEER_WRITE_DISABLED, TERM);
        }
        if let Some(c) = &capture {
            ws.add(c.raw(), READABLE | PEER_CLOSED | PEER_WRITE_DISABLED, CAPTURE);
        }
        if let Some((s, _)) = &feed {
            ws.add(s.raw(), WRITABLE | PEER_CLOSED, FEED);
        }
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { break };
        let mut exited = false;
        for (key, _) in ready {
            match key {
                TERM => {
                    term_open = drain(&term, &mut carry).is_some();
                    out.raw(&text_of(&mut carry));
                }
                CAPTURE => {
                    if drain(capture.as_ref().unwrap(), &mut captured).is_none() {
                        capture = None;
                    }
                }
                FEED => {
                    let (s, text) = feed.as_mut().unwrap();
                    match s.write(text) {
                        Ok(n) => {
                            text.drain(..n);
                        }
                        Err(Error::ShouldWait) => {}
                        Err(_) => text.clear(),
                    }
                    if text.is_empty() {
                        // The end of the input.
                        feed = None;
                    }
                }
                WAKE => {
                    let _ = console.wake.clear();
                    if interrupt.load(Ordering::Relaxed) && !killed {
                        // The whole job, as on a Unix terminal: `gcc` with the
                        // `cc1` and `as` it is running.
                        let _ = process.kill_tree();
                        killed = true;
                    }
                }
                PROCESS => exited = true,
                _ => {}
            }
        }
        if exited {
            break;
        }
    }
    *console.foreground.lock() = None;
    // What it wrote last.
    drain(&term, &mut carry);
    // What was typed for it and not read stays for the next reader.
    let mut unread = Vec::new();
    drain(&input_side, &mut unread);
    console.unread.lock().extend_from_slice(&unread);
    let text = text_of(&mut carry);
    out.raw(&text);
    if !carry.is_empty() {
        out.raw(&String::from_utf8_lossy(&carry));
    }
    if let Some(c) = &capture {
        drain(c, &mut captured);
    }
    let code = process.info().map(|i| i.exit_code).unwrap_or(vabi::EXIT_CODE_CRASHED);
    let (status, reason) = if killed { (crate::shell::INTERRUPTED, None) } else { ended(code) };
    Ok(Ended { status, reason, captured })
}
