//! The shell's built-in commands.
//!
//! Every command has the signature `fn(&mut Shell, &mut Io, &[String]) ->
//! i32` (arguments include the command name; the result is the exit
//! status) and is listed in [`BUILTINS`] with its aliases and help text.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vproto::fs::DirEntry;

use crate::screen::{Style, color};
use crate::shell::{Io, Shell, launch_error};
use vfiles::HOME;
use vfiles::format::{format_time, human_size};
use vfiles::kind::{EDITOR, FILES, FileKind, default_app, file_kind};
use vfiles::path::{SYSTEM, display_path, extension, file_name, glob_match, is_read_only, join};

/// A built-in command.
pub struct Builtin {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub group: Group,
    pub usage: &'static str,
    pub help: &'static str,
    pub run: fn(&mut Shell, &mut Io, &[String]) -> i32,
}

/// Sections of the `help` listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Files,
    System,
    Shell,
}

/// Every built-in command.
pub static BUILTINS: &[Builtin] = &[
    // Files and folders.
    Builtin {
        name: "ls",
        aliases: &["dir"],
        group: Group::Files,
        usage: "ls [-a] [-w] [PATH...]",
        help: "List a folder (-a: include hidden files, -w: names only)",
        run: cmd_ls,
    },
    Builtin {
        name: "cd",
        aliases: &[],
        group: Group::Files,
        usage: "cd [DIR | - | ~]",
        help: "Change the current folder",
        run: cmd_cd,
    },
    Builtin {
        name: "pwd",
        aliases: &[],
        group: Group::Files,
        usage: "pwd",
        help: "Print the current folder",
        run: cmd_pwd,
    },
    Builtin {
        name: "cat",
        aliases: &["type"],
        group: Group::Files,
        usage: "cat FILE...",
        help: "Print the contents of files",
        run: cmd_cat,
    },
    Builtin {
        name: "head",
        aliases: &[],
        group: Group::Files,
        usage: "head [-n N] [FILE]",
        help: "Print the first lines of a file (default 10)",
        run: cmd_head,
    },
    Builtin {
        name: "tail",
        aliases: &[],
        group: Group::Files,
        usage: "tail [-n N] [FILE]",
        help: "Print the last lines of a file (default 10)",
        run: cmd_tail,
    },
    Builtin {
        name: "wc",
        aliases: &[],
        group: Group::Files,
        usage: "wc [FILE...]",
        help: "Count lines, words and bytes",
        run: cmd_wc,
    },
    Builtin {
        name: "grep",
        aliases: &[],
        group: Group::Files,
        usage: "grep [-i] [-n] [-v] [-c] [-r] TEXT [FILE...]",
        help: "Print lines that contain TEXT",
        run: cmd_grep,
    },
    Builtin {
        name: "find",
        aliases: &[],
        group: Group::Files,
        usage: "find [DIR] [-name PATTERN] [-type f|d]",
        help: "Search for files below a folder",
        run: cmd_find,
    },
    Builtin {
        name: "tree",
        aliases: &[],
        group: Group::Files,
        usage: "tree [-a] [DIR]",
        help: "Show a folder hierarchy",
        run: cmd_tree,
    },
    Builtin {
        name: "mkdir",
        aliases: &["md"],
        group: Group::Files,
        usage: "mkdir [-p] DIR...",
        help: "Create folders (-p: with parents)",
        run: cmd_mkdir,
    },
    Builtin {
        name: "rmdir",
        aliases: &["rd"],
        group: Group::Files,
        usage: "rmdir DIR...",
        help: "Remove empty folders",
        run: cmd_rmdir,
    },
    Builtin {
        name: "rm",
        aliases: &["del"],
        group: Group::Files,
        usage: "rm [-r] [-f] PATH...",
        help: "Delete files (-r: folders with their contents)",
        run: cmd_rm,
    },
    Builtin {
        name: "cp",
        aliases: &["copy"],
        group: Group::Files,
        usage: "cp [-r] SOURCE... DEST",
        help: "Copy files (-r: folders too)",
        run: cmd_cp,
    },
    Builtin {
        name: "mv",
        aliases: &["move", "ren"],
        group: Group::Files,
        usage: "mv SOURCE... DEST",
        help: "Move or rename files and folders",
        run: cmd_mv,
    },
    Builtin {
        name: "touch",
        aliases: &[],
        group: Group::Files,
        usage: "touch FILE...",
        help: "Create empty files or update their time",
        run: cmd_touch,
    },
    Builtin {
        name: "stat",
        aliases: &[],
        group: Group::Files,
        usage: "stat PATH...",
        help: "Show details about files",
        run: cmd_stat,
    },
    Builtin {
        name: "edit",
        aliases: &["notepad"],
        group: Group::Files,
        usage: "edit [FILE]",
        help: "Open a file in the Text Editor",
        run: cmd_edit,
    },
    Builtin {
        name: "open",
        aliases: &["xdg-open"],
        group: Group::Files,
        usage: "open PATH...",
        help: "Open files or folders with their default app",
        run: cmd_open,
    },
    Builtin {
        name: "sync",
        aliases: &[],
        group: Group::Files,
        usage: "sync",
        help: "Write all changes to the disk now",
        run: cmd_sync,
    },
    Builtin {
        name: "df",
        aliases: &[],
        group: Group::Files,
        usage: "df [PATH...]",
        help: "Show how much space is used and free (home, /tmp and the system)",
        run: cmd_df,
    },
    // Processes and system information.
    Builtin {
        name: "ps",
        aliases: &["tasklist"],
        group: Group::System,
        usage: "ps [-a]",
        help: "List running processes",
        run: cmd_ps,
    },
    Builtin {
        name: "kill",
        aliases: &["taskkill"],
        group: Group::System,
        usage: "kill [-f] PID|NAME...",
        help: "End processes",
        run: cmd_kill,
    },
    Builtin {
        name: "run",
        aliases: &["start"],
        group: Group::System,
        usage: "run PROGRAM [ARGS...]",
        help: "Start a program or an installed app",
        run: cmd_run,
    },
    Builtin {
        name: "apps",
        aliases: &[],
        group: Group::System,
        usage: "apps",
        help: "List the installed applications",
        run: cmd_apps,
    },
    Builtin {
        name: "uptime",
        aliases: &[],
        group: Group::System,
        usage: "uptime",
        help: "Show how long the system has been running",
        run: cmd_uptime,
    },
    Builtin {
        name: "free",
        aliases: &["mem"],
        group: Group::System,
        usage: "free",
        help: "Show memory usage",
        run: cmd_free,
    },
    Builtin {
        name: "sysinfo",
        aliases: &["neofetch", "ver"],
        group: Group::System,
        usage: "sysinfo",
        help: "Show system information",
        run: cmd_sysinfo,
    },
    Builtin {
        name: "date",
        aliases: &["time"],
        group: Group::System,
        usage: "date",
        help: "Show the date and time",
        run: cmd_date,
    },
    Builtin {
        name: "dmesg",
        aliases: &["log"],
        group: Group::System,
        usage: "dmesg [-n N | -a] [TEXT]",
        help: "Show the system log (last 100 lines, optionally filtered)",
        run: cmd_dmesg,
    },
    Builtin {
        name: "logger",
        aliases: &[],
        group: Group::System,
        usage: "logger [MESSAGE...]",
        help: "Write a message (or the piped input) to the system log",
        run: cmd_logger,
    },
    Builtin {
        name: "whoami",
        aliases: &[],
        group: Group::System,
        usage: "whoami",
        help: "Print the user name",
        run: cmd_whoami,
    },
    Builtin {
        name: "hostname",
        aliases: &[],
        group: Group::System,
        usage: "hostname",
        help: "Print the computer name",
        run: cmd_hostname,
    },
    // The shell itself.
    Builtin {
        name: "help",
        aliases: &["?"],
        group: Group::Shell,
        usage: "help [COMMAND]",
        help: "Show this help, or details about a command",
        run: cmd_help,
    },
    Builtin {
        name: "echo",
        aliases: &[],
        group: Group::Shell,
        usage: "echo [-n] [-e] TEXT...",
        help: "Print text (-e: interpret \\n, \\t and \\e)",
        run: cmd_echo,
    },
    Builtin {
        name: "clear",
        aliases: &["cls"],
        group: Group::Shell,
        usage: "clear",
        help: "Clear the screen (Ctrl+L)",
        run: cmd_clear,
    },
    Builtin {
        name: "history",
        aliases: &[],
        group: Group::Shell,
        usage: "history [-c]",
        help: "Show (or clear) the command history",
        run: cmd_history,
    },
    Builtin {
        name: "env",
        aliases: &[],
        group: Group::Shell,
        usage: "env",
        help: "List environment variables",
        run: cmd_env,
    },
    Builtin {
        name: "export",
        aliases: &["set"],
        group: Group::Shell,
        usage: "export NAME=VALUE...",
        help: "Set environment variables",
        run: cmd_export,
    },
    Builtin {
        name: "unset",
        aliases: &[],
        group: Group::Shell,
        usage: "unset NAME...",
        help: "Remove environment variables",
        run: cmd_unset,
    },
    Builtin {
        name: "which",
        aliases: &["where"],
        group: Group::Shell,
        usage: "which NAME...",
        help: "Show what a command name refers to",
        run: cmd_which,
    },
    Builtin {
        name: "exit",
        aliases: &["quit"],
        group: Group::Shell,
        usage: "exit",
        help: "Close the terminal",
        run: cmd_exit,
    },
];

/// Looks up a built-in by name or alias.
pub fn find(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name || b.aliases.contains(&name))
}

/// All command names and aliases (for completion).
pub fn names() -> Vec<&'static str> {
    let mut v = Vec::new();
    for b in BUILTINS {
        v.push(b.name);
        v.extend_from_slice(b.aliases);
    }
    v
}

/// Core services, used to tell applications from system processes when the
/// launcher (which knows) cannot be asked.
const CORE_SERVICES: &[&str] = &["init", "vfs", "devmgr", "compositor", "audio", "shell"];

/// A test for "is this process a system process (not an application)?":
/// applications are the processes the launcher started on request;
/// everything else (services, drivers) is part of the system.
fn system_test(sh: &mut Shell) -> impl Fn(u64, &str) -> bool + use<> {
    let apps: Option<Vec<u64>> = match sh.launcher().map(|l| l.tasks()) {
        Some(Ok(tasks)) => Some(tasks.iter().filter(|t| t.is_app).map(|t| t.koid).collect()),
        _ => None,
    };
    move |koid, name| match &apps {
        Some(apps) => !apps.contains(&koid),
        None => CORE_SERVICES.contains(&name) || name.starts_with("virtio-") || name == "ps2",
    }
}

// ---- helpers ---------------------------------------------------------------

/// Splits `-abc` options from operands; `allowed` lists valid letters.
fn options(args: &[String], allowed: &str) -> Result<(Vec<char>, Vec<String>), String> {
    let mut opts = Vec::new();
    let mut rest = Vec::new();
    let mut operands_only = false;
    for a in &args[1..] {
        if !operands_only && a == "--" {
            operands_only = true;
        } else if !operands_only && a.len() > 1 && a.starts_with('-') {
            for c in a[1..].chars() {
                if !allowed.contains(c) {
                    return Err(format!("unknown option '-{c}'"));
                }
                opts.push(c);
            }
        } else {
            rest.push(a.clone());
        }
    }
    Ok((opts, rest))
}

/// Parses `-n N`, `-nN` or `-N` line counts; returns the count and the rest.
fn line_count(args: &[String], default: usize) -> Result<(usize, Vec<String>), String> {
    let mut n = default;
    let mut rest = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "-n" {
            let v = args.get(i + 1).ok_or("option -n needs a number")?;
            n = v.parse().map_err(|_| format!("'{v}' is not a number"))?;
            i += 1;
        } else if let Some(v) = a.strip_prefix("-n").filter(|v| !v.is_empty()) {
            n = v.parse().map_err(|_| format!("'{v}' is not a number"))?;
        } else if let Some(v) = a.strip_prefix('-').filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())) {
            n = v.parse().unwrap_or(default);
        } else if a == "-a" {
            n = usize::MAX;
        } else {
            rest.push(a.clone());
        }
        i += 1;
    }
    Ok((n, rest))
}

/// Prints a usage error.
fn usage(io: &mut Io, cmd: &str) -> i32 {
    let u = find(cmd).map(|b| b.usage).unwrap_or(cmd);
    io.error(cmd, &format!("usage: {u}"))
}

/// The colour of a directory entry name.
fn entry_style(name: &str, is_dir: bool) -> Style {
    if is_dir {
        return Style::fg(color::BRIGHT_BLUE).bold();
    }
    if name.starts_with('.') {
        return Style::DIM;
    }
    match file_kind(name) {
        FileKind::Program => Style::fg(color::BRIGHT_GREEN).bold(),
        FileKind::Image => Style::fg(color::BRIGHT_MAGENTA),
        FileKind::Audio => Style::fg(color::BRIGHT_CYAN),
        FileKind::Text | FileKind::Other => Style::PLAIN,
    }
}

/// True if data looks binary (contains NUL bytes near the start).
fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0)
}

/// Reads a text file argument; prints an error and returns `None` on failure.
fn read_text(sh: &Shell, io: &mut Io, cmd: &str, arg: &str) -> Option<String> {
    let path = sh.resolve(arg);
    match sh.fs.stat(&path) {
        Ok(st) if st.is_dir => {
            io.error(cmd, &format!("{arg}: is a folder"));
            return None;
        }
        Err(e) => {
            io.error(cmd, &format!("{arg}: {e}"));
            return None;
        }
        Ok(_) => {}
    }
    match sh.fs.read(&path) {
        Ok(data) if is_binary(&data) => {
            io.error(cmd, &format!("{arg}: binary file ({})", human_size(data.len() as u64)));
            None
        }
        Ok(data) => Some(String::from_utf8_lossy(&data).into_owned()),
        Err(e) => {
            io.error(cmd, &format!("{arg}: {e}"));
            None
        }
    }
}

/// Input text from file arguments or the pipe; `None` after an error.
fn inputs(sh: &Shell, io: &mut Io, cmd: &str, files: &[String]) -> Option<Vec<(String, String)>> {
    if files.is_empty() {
        return match io.take_stdin() {
            Some(s) => Some(vec![(String::new(), s)]),
            None => {
                usage(io, cmd);
                None
            }
        };
    }
    let mut out = Vec::new();
    for f in files {
        if let Some(t) = read_text(sh, io, cmd, f) {
            out.push((f.clone(), t));
        }
    }
    Some(out)
}

/// Pads `s` with spaces to `w` characters.
fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    let mut out = String::from(s);
    for _ in n..w {
        out.push(' ');
    }
    out
}

/// Right-aligns `s` in `w` characters.
fn rpad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    let mut out = String::new();
    for _ in n..w {
        out.push(' ');
    }
    out.push_str(s);
    out
}

fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).trim().to_string()
}

/// `1 h 05 min`, `12 min 30 s`, `45 s`.
fn format_duration(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    let (d, h, m, s) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60, s % 60);
    if d > 0 {
        format!("{d} d {h} h {m:02} min")
    } else if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// CPU time as `m:ss.cc`.
fn format_cpu(ns: u64) -> String {
    let cs = ns / 10_000_000;
    let (m, s, c) = (cs / 6000, (cs / 100) % 60, cs % 100);
    format!("{m}:{s:02}.{c:02}")
}

fn system_info(io: &mut Io, cmd: &str) -> Option<vabi::SystemInfo> {
    match vrt::object::system_info() {
        Ok(i) => Some(i),
        Err(e) => {
            io.error(cmd, &format!("cannot read system information: {e}"));
            None
        }
    }
}

/// A text progress bar `[████░░░░]`.
fn bar(fraction: f32, width: usize) -> (String, String) {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f32) + 0.5) as usize;
    let a: String = core::iter::repeat_n('█', filled.min(width)).collect();
    let b: String = core::iter::repeat_n('░', width - filled.min(width)).collect();
    (a, b)
}

/// Running processes from the kernel.
fn processes(all: bool) -> Vec<vabi::ProcessInfo> {
    let mut infos = vec![vabi::ProcessInfo::default(); 512];
    let n = vrt::object::process_list(&mut infos).unwrap_or(0).min(infos.len());
    infos.truncate(n);
    if !all {
        infos.retain(|p| p.state == vabi::process_state::RUNNING);
    }
    infos.sort_by_key(|p| p.koid);
    infos
}

// ---- files -----------------------------------------------------------------

fn print_entry_long(io: &mut Io, e: &DirEntry) {
    io.styled(&format_time(e.modified), Style::DIM);
    io.print("  ");
    if e.is_dir {
        let items = if e.size == 1 { "1 item".to_string() } else { format!("{} items", e.size) };
        io.styled(&rpad(&items, 10), Style::DIM);
    } else {
        io.print(&rpad(&human_size(e.size), 10));
    }
    io.print("  ");
    io.styled(&e.name, entry_style(&e.name, e.is_dir));
    if e.is_dir {
        io.styled("/", entry_style(&e.name, true));
    }
    io.print("\n");
}

fn print_wide(sh: &Shell, io: &mut Io, entries: &[DirEntry]) {
    if entries.is_empty() {
        return;
    }
    let names: Vec<String> =
        entries.iter().map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name.clone() }).collect();
    let colw = names.iter().map(|n| n.chars().count()).max().unwrap_or(1) + 2;
    let per_row = (sh.cols.max(colw) / colw).max(1);
    let rows = names.len().div_ceil(per_row);
    for r in 0..rows {
        for c in 0..per_row {
            let i = c * rows + r;
            let Some(n) = names.get(i) else { continue };
            let e = &entries[i];
            io.styled(n, entry_style(&e.name, e.is_dir));
            if (c + 1) * rows + r < names.len() {
                io.print(&pad("", colw - n.chars().count()));
            }
        }
        io.print("\n");
    }
}

fn cmd_ls(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, paths) = match options(args, "alw1h") {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    let all = opts.contains(&'a');
    let wide = opts.contains(&'w');
    let targets: Vec<String> = if paths.is_empty() { vec![".".to_string()] } else { paths };
    let multi = targets.len() > 1;
    let mut status = 0;
    for (i, arg) in targets.iter().enumerate() {
        let path = sh.resolve(arg);
        let st = match sh.fs.stat(&path) {
            Ok(s) => s,
            Err(e) => {
                status = io.error(&args[0], &format!("{arg}: {e}"));
                continue;
            }
        };
        if !st.is_dir {
            let e = DirEntry { name: arg.clone(), is_dir: false, size: st.size, modified: st.modified };
            print_entry_long(io, &e);
            continue;
        }
        let entries = match sh.fs.read_dir(&path) {
            Ok(v) => v,
            Err(e) => {
                status = io.error(&args[0], &format!("{arg}: {e}"));
                continue;
            }
        };
        let entries: Vec<DirEntry> = entries.into_iter().filter(|e| all || !e.name.starts_with('.')).collect();
        if multi {
            if i > 0 {
                io.print("\n");
            }
            io.styled(&format!("{}:\n", display_path(&path)), Style::BOLD);
        }
        if entries.is_empty() {
            io.styled("(empty folder)\n", Style::DIM);
            continue;
        }
        if wide {
            print_wide(sh, io, &entries);
            continue;
        }
        let (mut dirs, mut files, mut total) = (0, 0, 0u64);
        for e in &entries {
            print_entry_long(io, e);
            if e.is_dir {
                dirs += 1;
            } else {
                files += 1;
                total += e.size;
            }
        }
        let plural = |n: usize, s: &str| if n == 1 { format!("1 {s}") } else { format!("{n} {s}s") };
        io.styled(
            &format!(
                "{}, {}, {}{}\n",
                plural(dirs, "folder"),
                plural(files, "file"),
                human_size(total),
                if is_read_only(&path) { " (read-only)" } else { "" }
            ),
            Style::DIM,
        );
    }
    status
}

fn cmd_cd(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let target = match args.get(1).map(String::as_str) {
        None => HOME.to_string(),
        Some("-") => match &sh.prev_dir {
            Some(p) => {
                let p = p.clone();
                io.println(&display_path(&p));
                p
            }
            None => return io.error("cd", "no previous folder"),
        },
        Some(p) => sh.resolve(p),
    };
    if args.len() > 2 {
        return io.error("cd", "too many arguments");
    }
    let shown = args.get(1).map(String::as_str).unwrap_or("~");
    match sh.fs.stat(&target) {
        Ok(st) if st.is_dir => {
            sh.set_cwd(target);
            0
        }
        Ok(_) => io.error("cd", &format!("{shown}: not a folder")),
        Err(e) => io.error("cd", &format!("{shown}: {e}")),
    }
}

fn cmd_pwd(sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    io.println(&sh.cwd);
    0
}

fn cmd_cat(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    const LIMIT: usize = 1 << 20;
    if args.len() < 2 {
        return match io.take_stdin() {
            Some(s) => {
                io.raw(&s);
                0
            }
            None => usage(io, &args[0]),
        };
    }
    let mut status = 0;
    for arg in &args[1..] {
        let Some(mut text) = read_text(sh, io, &args[0], arg) else {
            status = 1;
            continue;
        };
        let truncated = text.len() > LIMIT;
        if truncated {
            let mut cut = LIMIT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        io.raw(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            io.print("\n");
        }
        if truncated {
            io.styled("… (output truncated at 1 MiB)\n", Style::DIM);
        }
    }
    status
}

fn cmd_head(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    head_tail(sh, io, args, true)
}

fn cmd_tail(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    head_tail(sh, io, args, false)
}

fn head_tail(sh: &mut Shell, io: &mut Io, args: &[String], head: bool) -> i32 {
    let (n, files) = match line_count(args, 10) {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    let Some(sources) = inputs(sh, io, &args[0], &files) else { return 1 };
    let multi = sources.len() > 1;
    for (i, (name, text)) in sources.iter().enumerate() {
        if multi {
            io.styled(&format!("{}==> {name} <==\n", if i > 0 { "\n" } else { "" }), Style::BOLD);
        }
        let lines: Vec<&str> = text.lines().collect();
        let range = if head { 0..n.min(lines.len()) } else { lines.len().saturating_sub(n)..lines.len() };
        for l in &lines[range] {
            io.raw(l);
            io.print("\n");
        }
    }
    if sources.len() < files.len() { 1 } else { 0 }
}

fn cmd_wc(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let Some(sources) = inputs(sh, io, &args[0], &args[1..]) else { return 1 };
    let mut total = (0, 0, 0);
    for (name, text) in &sources {
        let (l, w, b) = (text.lines().count(), text.split_whitespace().count(), text.len());
        total = (total.0 + l, total.1 + w, total.2 + b);
        io.println(&format!("{:>7} {:>7} {:>9} {name}", l, w, b));
    }
    if sources.len() > 1 {
        io.println(&format!("{:>7} {:>7} {:>9} total", total.0, total.1, total.2));
    }
    if sources.len() < args.len().saturating_sub(1) { 1 } else { 0 }
}

/// Collects the files below `dir` (for `grep -r`).
fn walk_files(sh: &Shell, dir: &str, out: &mut Vec<String>) {
    for e in sh.fs.read_dir(dir).unwrap_or_default() {
        let p = join(dir, &e.name);
        if e.is_dir {
            walk_files(sh, &p, out);
        } else {
            out.push(p);
        }
    }
}

fn cmd_grep(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, rest) = match options(args, "invcrH") {
        Ok(v) => v,
        Err(e) => return io.error("grep", &e),
    };
    let Some(pattern) = rest.first() else { return usage(io, "grep") };
    let (ci, numbers, invert, count, recursive) =
        (opts.contains(&'i'), opts.contains(&'n'), opts.contains(&'v'), opts.contains(&'c'), opts.contains(&'r'));
    let mut files: Vec<String> = Vec::new();
    for f in &rest[1..] {
        let abs = sh.resolve(f);
        if recursive && sh.fs.is_dir(&abs) {
            let mut found = Vec::new();
            walk_files(sh, &abs, &mut found);
            let prefix = abs.len();
            files.extend(found.into_iter().map(|p| format!("{}{}", f.trim_end_matches('/'), &p[prefix..])));
        } else {
            files.push(f.clone());
        }
    }
    if recursive && rest.len() == 1 {
        let mut found = Vec::new();
        walk_files(sh, &sh.cwd.clone(), &mut found);
        let prefix = sh.cwd.len() + if sh.cwd == "/" { 0 } else { 1 };
        files.extend(found.into_iter().map(|p| p[prefix..].to_string()));
    }
    let Some(sources) = inputs(sh, io, "grep", &files) else { return 2 };
    let names = sources.len() > 1 || recursive;
    let needle = if ci { pattern.to_ascii_lowercase() } else { pattern.clone() };
    let mut any = false;
    for (name, text) in &sources {
        let mut n = 0;
        for (ln, line) in text.lines().enumerate() {
            let hay = if ci { line.to_ascii_lowercase() } else { line.to_string() };
            if hay.contains(needle.as_str()) == invert {
                continue;
            }
            n += 1;
            any = true;
            if count {
                continue;
            }
            if names {
                io.styled(name, Style::fg(color::MAGENTA));
                io.styled(":", Style::DIM);
            }
            if numbers {
                io.styled(&format!("{}", ln + 1), Style::fg(color::GREEN));
                io.styled(":", Style::DIM);
            }
            if invert || needle.is_empty() || !io.is_terminal() {
                io.print(line);
            } else {
                let mut last = 0;
                for (pos, m) in hay.match_indices(needle.as_str()) {
                    io.print(&line[last..pos]);
                    io.styled(&line[pos..pos + m.len()], Style::fg(color::BRIGHT_RED).bold());
                    last = pos + m.len();
                }
                io.print(&line[last..]);
            }
            io.print("\n");
        }
        if count {
            if names {
                io.styled(&format!("{name}:"), Style::fg(color::MAGENTA));
            }
            io.println(&n.to_string());
        }
    }
    if any { 0 } else { 1 }
}

fn cmd_find(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let mut start = ".".to_string();
    let mut name: Option<(String, bool)> = None;
    let mut kind: Option<bool> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-name" | "-iname" => {
                let Some(p) = args.get(i + 1) else { return usage(io, "find") };
                name = Some((p.clone(), args[i] == "-iname"));
                i += 1;
            }
            "-type" => match args.get(i + 1).map(String::as_str) {
                Some("d") => {
                    kind = Some(true);
                    i += 1;
                }
                Some("f") => {
                    kind = Some(false);
                    i += 1;
                }
                _ => return io.error("find", "-type expects 'f' or 'd'"),
            },
            a if a.starts_with('-') => return io.error("find", &format!("unknown option '{a}'")),
            a => start = a.to_string(),
        }
        i += 1;
    }
    let abs = sh.resolve(&start);
    if !sh.fs.is_dir(&abs) {
        return io.error("find", &format!("{start}: not a folder"));
    }
    let shown = start.trim_end_matches('/').to_string();
    let mut found = 0;
    find_rec(sh, io, &abs, if shown.is_empty() { "/" } else { &shown }, &name, kind, &mut found);
    if found == 0 && io.is_terminal() {
        io.styled("No matches.\n", Style::DIM);
    }
    0
}

fn find_rec(
    sh: &Shell,
    io: &mut Io,
    dir: &str,
    shown: &str,
    name: &Option<(String, bool)>,
    kind: Option<bool>,
    found: &mut usize,
) {
    for e in sh.fs.read_dir(dir).unwrap_or_default() {
        let path = join(dir, &e.name);
        let display = join(shown, &e.name);
        let name_ok = match name {
            Some((p, true)) => glob_match(&p.to_lowercase(), &e.name.to_lowercase()),
            Some((p, false)) => glob_match(p, &e.name),
            None => true,
        };
        if name_ok && kind.is_none_or(|d| d == e.is_dir) {
            io.styled(&display, entry_style(&e.name, e.is_dir));
            io.print("\n");
            *found += 1;
        }
        if e.is_dir {
            find_rec(sh, io, &path, &display, name, kind, found);
        }
    }
}

fn cmd_tree(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, rest) = match options(args, "a") {
        Ok(v) => v,
        Err(e) => return io.error("tree", &e),
    };
    let arg = rest.first().cloned().unwrap_or_else(|| ".".into());
    let abs = sh.resolve(&arg);
    if !sh.fs.is_dir(&abs) {
        return io.error("tree", &format!("{arg}: not a folder"));
    }
    io.styled(&display_path(&abs), Style::fg(color::BRIGHT_BLUE).bold());
    io.print("\n");
    let mut counts = (0usize, 0usize);
    tree_rec(sh, io, &abs, "", opts.contains(&'a'), 0, &mut counts);
    let plural = |n: usize, s: &str| if n == 1 { format!("1 {s}") } else { format!("{n} {s}s") };
    io.styled(&format!("\n{}, {}\n", plural(counts.0, "folder"), plural(counts.1, "file")), Style::DIM);
    0
}

fn tree_rec(sh: &Shell, io: &mut Io, dir: &str, prefix: &str, all: bool, depth: usize, counts: &mut (usize, usize)) {
    let entries: Vec<DirEntry> =
        sh.fs.read_dir(dir).unwrap_or_default().into_iter().filter(|e| all || !e.name.starts_with('.')).collect();
    for (i, e) in entries.iter().enumerate() {
        let last = i + 1 == entries.len();
        io.styled(prefix, Style::DIM);
        io.styled(if last { "└── " } else { "├── " }, Style::DIM);
        io.styled(&e.name, entry_style(&e.name, e.is_dir));
        io.print("\n");
        if e.is_dir {
            counts.0 += 1;
            if depth < 12 {
                let p = format!("{prefix}{}", if last { "    " } else { "│   " });
                tree_rec(sh, io, &join(dir, &e.name), &p, all, depth + 1, counts);
            }
        } else {
            counts.1 += 1;
        }
    }
}

fn cmd_mkdir(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, dirs) = match options(args, "p") {
        Ok(v) => v,
        Err(e) => return io.error("mkdir", &e),
    };
    if dirs.is_empty() {
        return usage(io, "mkdir");
    }
    let mut status = 0;
    for d in &dirs {
        let abs = sh.resolve(d);
        let r = if opts.contains(&'p') { sh.fs.mkdir_all(&abs) } else { sh.fs.mkdir(&abs) };
        if let Err(e) = r {
            status = io.fs_error("mkdir", d, &e);
        }
    }
    status
}

fn cmd_rmdir(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage(io, "rmdir");
    }
    let mut status = 0;
    for d in &args[1..] {
        let abs = sh.resolve(d);
        match sh.fs.stat(&abs) {
            Ok(st) if !st.is_dir => status = io.error("rmdir", &format!("{d}: not a folder")),
            Ok(_) => {
                if let Err(e) = sh.fs.remove(&abs) {
                    status = io.error("rmdir", &format!("{d}: {e}"));
                }
            }
            Err(e) => status = io.error("rmdir", &format!("{d}: {e}")),
        }
    }
    status
}

/// Paths that must never be deleted or overwritten wholesale.
fn protected(path: &str) -> bool {
    matches!(path, "/" | "/home" | HOME | "/system" | "/tmp")
}

fn cmd_rm(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, paths) = match options(args, "rRfdis") {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    if paths.is_empty() {
        return usage(io, "rm");
    }
    let recursive = opts.contains(&'r') || opts.contains(&'R') || opts.contains(&'s');
    let force = opts.contains(&'f');
    let mut status = 0;
    for p in &paths {
        let abs = sh.resolve(p);
        if protected(&abs) {
            status = io.error(&args[0], &format!("refusing to remove '{}'", display_path(&abs)));
            continue;
        }
        let st = match sh.fs.stat(&abs) {
            Ok(s) => s,
            Err(e) => {
                if !force {
                    status = io.error(&args[0], &format!("{p}: {e}"));
                }
                continue;
            }
        };
        if st.is_dir && !recursive {
            status = io.error(&args[0], &format!("{p}: is a folder (use 'rm -r' to delete it with its contents)"));
            continue;
        }
        let r = if st.is_dir { sh.fs.remove_all(&abs) } else { sh.fs.remove(&abs) };
        if let Err(e) = r {
            status = io.error(&args[0], &format!("{p}: {e}"));
        }
    }
    status
}

fn cmd_cp(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, paths) = match options(args, "rRfv") {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    if paths.len() < 2 {
        return usage(io, "cp");
    }
    let recursive = opts.contains(&'r') || opts.contains(&'R');
    let (sources, dest) = paths.split_at(paths.len() - 1);
    let dest_abs = sh.resolve(&dest[0]);
    let dest_is_dir = sh.fs.is_dir(&dest_abs);
    if sources.len() > 1 && !dest_is_dir {
        return io.error(&args[0], &format!("{}: not a folder", dest[0]));
    }
    let mut status = 0;
    for s in sources {
        let src = sh.resolve(s);
        let st = match sh.fs.stat(&src) {
            Ok(st) => st,
            Err(e) => {
                status = io.error(&args[0], &format!("{s}: {e}"));
                continue;
            }
        };
        if st.is_dir && !recursive {
            status = io.error(&args[0], &format!("{s}: is a folder (use 'cp -r' to copy it)"));
            continue;
        }
        let target = if dest_is_dir { join(&dest_abs, file_name(&src)) } else { dest_abs.clone() };
        if target == src {
            status = io.error(&args[0], &format!("{s}: source and destination are the same"));
            continue;
        }
        let r = match sh.fs.stat(&target) {
            Ok(t) if !t.is_dir && !st.is_dir => sh.fs.read(&src).and_then(|d| sh.fs.write(&target, &d)),
            _ => sh.fs.copy(&src, &target),
        };
        if let Err(e) = r {
            status = io.fs_error(&args[0], s, &e);
        }
    }
    status
}

fn cmd_mv(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (_, paths) = match options(args, "fv") {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    if paths.len() < 2 {
        return usage(io, "mv");
    }
    let (sources, dest) = paths.split_at(paths.len() - 1);
    let dest_abs = sh.resolve(&dest[0]);
    let dest_is_dir = sh.fs.is_dir(&dest_abs);
    if sources.len() > 1 && !dest_is_dir {
        return io.error(&args[0], &format!("{}: not a folder", dest[0]));
    }
    let mut status = 0;
    for s in sources {
        let src = sh.resolve(s);
        if protected(&src) {
            status = io.error(&args[0], &format!("refusing to move '{}'", display_path(&src)));
            continue;
        }
        let st = match sh.fs.stat(&src) {
            Ok(st) => st,
            Err(e) => {
                status = io.error(&args[0], &format!("{s}: {e}"));
                continue;
            }
        };
        let target = if dest_is_dir { join(&dest_abs, file_name(&src)) } else { dest_abs.clone() };
        if target == src {
            continue;
        }
        // Like Unix mv, replace an existing file.
        if let Ok(t) = sh.fs.stat(&target)
            && !t.is_dir
            && !st.is_dir
        {
            let _ = sh.fs.remove(&target);
        }
        if let Err(e) = sh.fs.move_to(&src, &target) {
            status = io.fs_error(&args[0], s, &e);
        }
    }
    status
}

fn cmd_touch(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage(io, "touch");
    }
    let mut status = 0;
    for p in &args[1..] {
        let abs = sh.resolve(p);
        if sh.fs.is_dir(&abs) {
            continue;
        }
        if let Err(e) = sh.fs.touch(&abs) {
            status = io.fs_error("touch", p, &e);
        }
    }
    status
}

fn cmd_stat(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage(io, "stat");
    }
    let mut status = 0;
    for p in &args[1..] {
        let abs = sh.resolve(p);
        match sh.fs.stat(&abs) {
            Ok(st) => {
                let label = |io: &mut Io, l: &str| io.styled(&format!("{l:>9}: "), Style::DIM);
                label(io, "Name");
                io.styled(file_name(&abs), entry_style(file_name(&abs), st.is_dir));
                io.print("\n");
                label(io, "Path");
                io.println(&abs);
                label(io, "Type");
                if st.is_dir {
                    io.println(&format!("folder ({} items)", st.size));
                } else {
                    let kind = match file_kind(&abs) {
                        FileKind::Text => "text document",
                        FileKind::Image => "image",
                        FileKind::Audio => "audio",
                        FileKind::Program => "program",
                        FileKind::Other => "file",
                    };
                    io.println(kind);
                    label(io, "Size");
                    io.println(&format!("{} ({} bytes)", human_size(st.size), st.size));
                }
                label(io, "Modified");
                io.println(&format_time(st.modified));
                label(io, "Access");
                io.println(if st.read_only { "read-only" } else { "read and write" });
                if let Some(app) = default_app(&abs).filter(|_| !st.is_dir) {
                    label(io, "Opens in");
                    io.println(app.id);
                }
            }
            Err(e) => status = io.error("stat", &format!("{p}: {e}")),
        }
    }
    status
}

fn launch_with(sh: &mut Shell, io: &mut Io, cmd: &str, exe: &str, args: Vec<String>) -> i32 {
    let Some(l) = sh.launcher() else { return io.error(cmd, "the launcher is unavailable") };
    match l.launch(exe.into(), args) {
        Ok(Ok(_)) => 0,
        Ok(Err(e)) => io.error(cmd, &format!("{}: {}", file_name(exe), launch_error(e))),
        Err(_) => io.error(cmd, "the launcher is not responding"),
    }
}

fn cmd_edit(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return launch_with(sh, io, "edit", EDITOR.exe, Vec::new());
    }
    let mut status = 0;
    for p in &args[1..] {
        let abs = sh.resolve(p);
        match sh.fs.stat(&abs) {
            Ok(st) if st.is_dir => {
                status = io.error("edit", &format!("{p}: is a folder"));
                continue;
            }
            Ok(_) => {}
            Err(_) if is_read_only(&abs) => {
                status = io.error("edit", &format!("{p}: no such file in the read-only system folder"));
                continue;
            }
            Err(_) => {
                if let Err(e) = sh.fs.touch(&abs) {
                    status = io.fs_error("edit", p, &e);
                    continue;
                }
                io.styled(&format!("Created {}\n", display_path(&abs)), Style::DIM);
            }
        }
        status |= launch_with(sh, io, "edit", EDITOR.exe, vec![abs]);
    }
    status
}

fn cmd_open(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage(io, &args[0]);
    }
    let mut status = 0;
    for p in &args[1..] {
        let abs = sh.resolve(p);
        let st = match sh.fs.stat(&abs) {
            Ok(st) => st,
            Err(e) => {
                status = io.error(&args[0], &format!("{p}: {e}"));
                continue;
            }
        };
        if st.is_dir {
            status |= launch_with(sh, io, &args[0], FILES.exe, vec![abs]);
        } else if let Some(app) = default_app(&abs) {
            status |= launch_with(sh, io, &args[0], app.exe, vec![abs]);
        } else if file_kind(&abs) == FileKind::Program {
            match sh.launch(&abs, Vec::new()) {
                Ok((name, koid)) => io.styled(&format!("Started {name} (PID {koid})\n"), Style::DIM),
                Err(e) => status = io.error(&args[0], &e),
            }
        } else {
            let ext = extension(&abs);
            let what =
                if ext.is_empty() { "files without an extension".to_string() } else { format!("'.{ext}' files") };
            status = io.error(&args[0], &format!("{p}: no app is set up to open {what}"));
        }
    }
    status
}

fn cmd_sync(sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    match sh.fs.sync() {
        Ok(()) => {
            io.styled("All changes are saved.\n", Style::DIM);
            0
        }
        Err(e) => io.error("sync", &e.to_string()),
    }
}

fn cmd_df(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let targets: Vec<String> = if args.len() > 1 {
        args[1..].iter().map(|a| sh.resolve(a)).collect()
    } else {
        vec![HOME.to_string(), "/tmp".to_string(), SYSTEM.to_string()]
    };
    let shown: Vec<String> = targets.iter().map(|p| display_path(p)).collect();
    let w = shown.iter().map(|s| s.chars().count()).max().unwrap_or(6).max(6) + 2;
    io.styled(
        &format!("{}{:>11}{:>11}{:>11}{:>6}  {}\n", pad("FOLDER", w), "SIZE", "USED", "FREE", "USE", "KEPT ON"),
        Style::BOLD,
    );
    let mut status = 0;
    for (path, name) in targets.iter().zip(&shown) {
        let s = match sh.fs.space(path) {
            Ok(s) => s,
            Err(e) => {
                status = io.fs_error("df", name, &e);
                continue;
            }
        };
        let free = s.total.saturating_sub(s.used);
        let pct = if s.total > 0 { (s.used.min(s.total) as u128 * 100 / s.total as u128) as u64 } else { 0 };
        let kept = if is_read_only(path) {
            "system image (read-only)"
        } else if s.persistent {
            "home disk"
        } else {
            "memory (lost at restart)"
        };
        io.print(&pad(name, w));
        io.print(&format!("{:>11}{:>11}", human_size(s.total), human_size(s.used)));
        let low = !is_read_only(path) && free < s.total / 10;
        io.styled(&format!("{:>11}", human_size(free)), if low { Style::fg(color::BRIGHT_RED) } else { Style::PLAIN });
        let pct_style = match pct {
            90.. if !is_read_only(path) => Style::fg(color::BRIGHT_RED).bold(),
            75.. if !is_read_only(path) => Style::fg(color::BRIGHT_YELLOW),
            _ => Style::PLAIN,
        };
        io.styled(&format!("{:>5}%", pct), pct_style);
        io.styled(&format!("  {kept}\n"), Style::DIM);
    }
    status
}

// ---- processes and system --------------------------------------------------

fn cmd_ps(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, _) = match options(args, "aef") {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    let all = opts.contains(&'a');
    let procs = processes(all);
    let is_system = system_test(sh);
    io.styled(
        &format!(
            "{:>6}  {}  {:>7}  {:>10}  {:>9}{}\n",
            "PID",
            pad("NAME", 16),
            "THREADS",
            "MEMORY",
            "CPU TIME",
            if all { "  STATE" } else { "" }
        ),
        Style::BOLD,
    );
    let mut mem = 0;
    for p in &procs {
        io.styled(&format!("{:>6}  ", p.koid), Style::DIM);
        let style = if is_system(p.koid, p.name()) { Style::fg(color::CYAN) } else { Style::fg(color::BRIGHT_GREEN) };
        io.styled(&pad(p.name(), 16), style);
        io.print(&format!("  {:>7}  {:>10}  {:>9}", p.threads, human_size(p.memory_bytes), format_cpu(p.cpu_time_ns)));
        if all {
            let state = match p.state {
                vabi::process_state::RUNNING => "running",
                vabi::process_state::EXITED => "exited",
                vabi::process_state::KILLED => "killed",
                vabi::process_state::CRASHED => "crashed",
                _ => "?",
            };
            io.print(&format!("  {state}"));
        }
        io.print("\n");
        mem += p.memory_bytes;
    }
    io.styled(&format!("{} processes, {} in use\n", procs.len(), human_size(mem)), Style::DIM);
    0
}

fn cmd_kill(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (opts, targets) = match options(args, "f9") {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    if targets.is_empty() {
        return usage(io, "kill");
    }
    let force = opts.contains(&'f') || opts.contains(&'9');
    let procs = processes(false);
    let is_system = system_test(sh);
    let mut status = 0;
    for t in &targets {
        let matches: Vec<(u64, String)> = match t.parse::<u64>() {
            Ok(pid) => procs.iter().filter(|p| p.koid == pid).map(|p| (p.koid, p.name().to_string())).collect(),
            Err(_) => {
                let name = t.trim_end_matches(".exe");
                procs
                    .iter()
                    .filter(|p| p.name().eq_ignore_ascii_case(name))
                    .map(|p| (p.koid, p.name().to_string()))
                    .collect()
            }
        };
        if matches.is_empty() {
            status = io.error("kill", &format!("{t}: no such process"));
            continue;
        }
        for (koid, name) in matches {
            if is_system(koid, &name) && !force {
                status = io
                    .error("kill", &format!("{name} (PID {koid}) is a system process; use 'kill -f' to end it anyway"));
                continue;
            }
            let Some(l) = sh.launcher() else { return io.error("kill", "the launcher is unavailable") };
            match l.kill(koid) {
                Ok(Ok(())) => io.styled(&format!("Ended {name} (PID {koid})\n"), Style::DIM),
                Ok(Err(e)) => status = io.error("kill", &format!("{name} (PID {koid}): {}", launch_error(e))),
                Err(_) => status = io.error("kill", "the launcher is not responding"),
            }
        }
    }
    status
}

fn cmd_run(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage(io, &args[0]);
    }
    let rest: Vec<String> = args[2..].iter().map(|a| sh.absolute_arg(a)).collect();
    match sh.launch(&args[1], rest) {
        Ok((name, koid)) => {
            io.styled(&format!("Started {name} (PID {koid})\n"), Style::DIM);
            0
        }
        Err(e) => io.error(&args[0], &e),
    }
}

fn cmd_apps(sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    let apps = sh.apps();
    if apps.is_empty() {
        return io.error("apps", "no applications found (is the launcher running?)");
    }
    let idw = apps.iter().map(|a| a.id.chars().count()).max().unwrap_or(2).max(2);
    let namew = apps.iter().map(|a| a.name.chars().count()).max().unwrap_or(4).max(4);
    io.styled(
        &format!("{}  {}  {}  {}\n", pad("ID", idw), pad("NAME", namew), pad("CATEGORY", 12), "DESCRIPTION"),
        Style::BOLD,
    );
    for a in &apps {
        io.styled(&pad(&a.id, idw), Style::fg(color::BRIGHT_GREEN).bold());
        io.print("  ");
        io.print(&pad(&a.name, namew));
        io.print("  ");
        io.styled(&pad(&a.category, 12), Style::fg(color::CYAN));
        io.print("  ");
        io.styled(&a.description, Style::DIM);
        io.print("\n");
    }
    io.styled("Start one with 'run ID' (or just type its ID).\n", Style::DIM);
    0
}

fn cmd_uptime(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    let Some(i) = system_info(io, "uptime") else { return 1 };
    io.print("up ");
    io.styled(&format_duration(i.uptime_ns), Style::BOLD);
    io.println(&format!(", {} processes, {} threads, {} CPUs", i.process_count, i.thread_count, i.cpu_count));
    0
}

fn cmd_free(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    let Some(i) = system_info(io, "free") else { return 1 };
    let used = i.total_memory.saturating_sub(i.free_memory);
    let mib = |b: u64| format!("{} MiB", b >> 20);
    io.styled(&format!("{:>9} {:>12} {:>12} {:>12}\n", "", "total", "used", "free"), Style::BOLD);
    io.println(&format!("{:>9} {:>12} {:>12} {:>12}", "Memory:", mib(i.total_memory), mib(used), mib(i.free_memory)));
    let f = if i.total_memory > 0 { used as f32 / i.total_memory as f32 } else { 0.0 };
    let (a, b) = bar(f, 32);
    io.print(&format!("{:>9} ", ""));
    io.styled(&a, Style::fg(if f > 0.85 { color::BRIGHT_RED } else { color::BRIGHT_GREEN }));
    io.styled(&b, Style::DIM);
    io.println(&format!(" {:.1}% in use", f * 100.0));
    0
}

fn cmd_sysinfo(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    let Some(i) = system_info(io, "sysinfo") else { return 1 };
    let screen = vproto::connect(vproto::display::display::NAME)
        .ok()
        .and_then(|ch| vproto::display::display::Client::new(ch).screen_info().ok());
    let version = cstr(&i.version);
    let used = i.total_memory.saturating_sub(i.free_memory);
    let mut info: Vec<(&str, String)> = vec![
        ("OS", format!("Vindows {} x86_64", version.trim_start_matches("Vindows").trim())),
        ("Kernel", format!("vkernel {} (microkernel)", version.trim_start_matches("Vindows").trim())),
        ("Uptime", format_duration(i.uptime_ns)),
        ("CPU", format!("{} ({} cores)", cstr(&i.cpu_brand), i.cpu_count)),
        ("Memory", format!("{} MiB / {} MiB", used >> 20, i.total_memory >> 20)),
        ("Processes", format!("{} ({} threads)", i.process_count, i.thread_count)),
    ];
    if let Some(s) = screen {
        info.push(("Display", format!("{}x{}", s.width, s.height)));
    }
    info.push(("Shell", "vsh 0.1".to_string()));
    info.push(("Terminal", "Vindows Terminal".to_string()));
    info.push(("Font", "JetBrains Mono".to_string()));
    // The logo: four tiles, like the About window.
    let tiles = [color::BRIGHT_CYAN, color::BRIGHT_BLUE, color::BLUE, color::BRIGHT_MAGENTA];
    let logo_rows = 7;
    let rows = (info.len() + 2).max(logo_rows);
    for r in 0..rows {
        io.print("  ");
        if r < logo_rows && r != 3 {
            let top = r < 3;
            for (k, gap) in [(0, "  "), (1, "")] {
                let t = tiles[if top { k } else { 2 + k }];
                io.styled("██████", Style::fg(t));
                io.print(gap);
            }
        } else {
            io.print(&pad("", 14));
        }
        io.print("    ");
        match r {
            0 => {
                io.styled("user", Style::fg(color::BRIGHT_GREEN).bold());
                io.print("@");
                io.styled("vindows", Style::fg(color::BRIGHT_GREEN).bold());
            }
            1 => io.styled("────────────", Style::DIM),
            r if r - 2 < info.len() => {
                let (k, v) = &info[r - 2];
                io.styled(&format!("{k}: "), Style::fg(color::BRIGHT_BLUE).bold());
                io.print(v);
            }
            _ => {}
        }
        io.print("\n");
    }
    io.print("\n  ");
    for c in 0..16u8 {
        io.styled("███", Style::fg(c));
        if c == 7 {
            io.print("\n  ");
        }
    }
    io.print("\n");
    0
}

fn cmd_date(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    let d = vrt::time::DateTime::now();
    io.println(&format!(
        "{}, {} {} {}, {:02}:{:02}:{:02}",
        d.weekday_name(),
        d.day,
        d.month_name(),
        d.year,
        d.hour,
        d.minute,
        d.second
    ));
    0
}

/// Reads the whole kernel log ring.
fn read_log() -> String {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    while let Ok((n, next)) = vrt::object::log_read(offset, &mut buf) {
        if n == 0 || out.len() > 4 << 20 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        offset = next;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn message_style(msg: &str) -> Style {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("panic") || lower.contains("error") || lower.contains("fail") || lower.contains("crash") {
        Style::fg(color::BRIGHT_RED)
    } else if lower.contains("warn") {
        Style::fg(color::BRIGHT_YELLOW)
    } else {
        Style::PLAIN
    }
}

fn print_log_line(io: &mut Io, line: &str) {
    if !io.is_terminal() || !line.starts_with('[') {
        io.println(line);
        return;
    }
    let Some(close) = line.find(']') else {
        io.println(line);
        return;
    };
    io.styled(&line[..=close], Style::DIM);
    let rest = &line[close + 1..];
    let trimmed = rest.trim_start();
    if let Some(after_cpu) = trimmed.strip_prefix("cpu") {
        // Kernel record: "cpuN level message".
        let mut parts = after_cpu.splitn(2, ' ');
        let cpu = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim_start();
        let (level, msg) = rest.split_once(' ').unwrap_or((rest, ""));
        io.styled(&format!(" cpu{cpu} "), Style::DIM);
        let ls = match level {
            "ERROR" => Style::fg(color::BRIGHT_RED).bold(),
            "WARN" => Style::fg(color::BRIGHT_YELLOW).bold(),
            "debug" => Style::DIM,
            _ => Style::fg(color::GREEN),
        };
        io.styled(&pad(level, 5), ls);
        io.print(" ");
        let msg = msg.trim_start();
        io.styled(msg, message_style(msg));
    } else if let Some((name, msg)) = rest.split_once(": ") {
        io.styled(&format!("{name}:"), Style::fg(color::CYAN));
        io.print(" ");
        io.styled(msg, message_style(msg));
    } else {
        io.print(rest);
    }
    io.print("\n");
}

fn cmd_dmesg(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let (n, filters) = match line_count(args, 100) {
        Ok(v) => v,
        Err(e) => return io.error(&args[0], &e),
    };
    let filter = filters.join(" ").to_ascii_lowercase();
    let log = read_log();
    let lines: Vec<&str> =
        log.lines().filter(|l| filter.is_empty() || l.to_ascii_lowercase().contains(&filter)).collect();
    let start = lines.len().saturating_sub(n);
    if start > 0 && io.is_terminal() {
        io.styled(&format!("… {} earlier lines (use '{} -a' to see everything)\n", start, args[0]), Style::DIM);
    }
    for l in &lines[start..] {
        print_log_line(io, l);
    }
    if lines.is_empty() && io.is_terminal() {
        io.styled("No matching log lines.\n", Style::DIM);
    }
    0
}

fn cmd_logger(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let text = if args.len() > 1 { args[1..].join(" ") } else { io.take_stdin().unwrap_or_default() };
    let mut any = false;
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        // The kernel prefixes each line with the process name ("terminal").
        vrt::println!("{line}");
        any = true;
    }
    if any { 0 } else { usage(io, "logger") }
}

fn cmd_whoami(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    io.println("user");
    0
}

fn cmd_hostname(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    io.println("vindows");
    0
}

// ---- shell -----------------------------------------------------------------

fn cmd_help(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if let Some(name) = args.get(1) {
        let Some(b) = find(name) else { return io.error("help", &format!("no such command: {name}")) };
        io.styled(b.usage, Style::BOLD);
        io.print("\n  ");
        io.println(b.help);
        if !b.aliases.is_empty() {
            io.styled(&format!("  Also available as: {}\n", b.aliases.join(", ")), Style::DIM);
        }
        return 0;
    }
    io.styled("Vindows Terminal", Style::fg(color::BRIGHT_BLUE).bold());
    io.styled(" — built-in commands\n", Style::DIM);
    let label = |b: &Builtin| {
        let mut names = String::from(b.name);
        for a in b.aliases {
            names.push_str(", ");
            names.push_str(a);
        }
        names
    };
    let width = BUILTINS.iter().map(|b| label(b).chars().count()).max().unwrap_or(10) + 3;
    for (group, title) in
        [(Group::Files, "Files and folders"), (Group::System, "Programs and system"), (Group::Shell, "Shell")]
    {
        io.print("\n");
        io.styled(&format!("{title}\n"), Style::BOLD);
        for b in BUILTINS.iter().filter(|b| b.group == group) {
            io.print("  ");
            io.styled(&pad(&label(b), width), Style::fg(color::BRIGHT_GREEN));
            io.println(b.help);
        }
    }
    io.print("\n");
    io.styled("Tips\n", Style::BOLD);
    for tip in [
        "Tab completes commands and paths; Up/Down browse the history, Ctrl+R searches it.",
        "Use | to pass output to another command, > or >> to save it in a file.",
        "Chain commands with ; or &&. Wildcards: rm *.txt, ls ~/Pictures/*.png",
        "Type an app's id (see 'apps') to start it. 'help COMMAND' shows details.",
        "Select text with the mouse; Ctrl+Shift+C copies, Ctrl+Shift+V pastes.",
    ] {
        io.styled("  • ", Style::DIM);
        io.println(tip);
    }
    0
}

fn unescape_echo(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some('0') => {
                // \033 style octal escape.
                let mut v = 0u32;
                for _ in 0..3 {
                    match chars.peek() {
                        Some(d @ '0'..='7') => {
                            v = v * 8 + d.to_digit(8).unwrap_or(0);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if let Some(ch) = char::from_u32(v) {
                    out.push(ch);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn cmd_echo(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let mut newline = true;
    let mut escapes = false;
    let mut i = 1;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-n" => newline = false,
            "-e" => escapes = true,
            "-ne" | "-en" => {
                newline = false;
                escapes = true;
            }
            _ => break,
        }
        i += 1;
    }
    let text = args[i..].join(" ");
    let text = if escapes { unescape_echo(&text) } else { text };
    io.raw(&text);
    if newline {
        io.raw("\n");
    }
    0
}

fn cmd_clear(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    io.screen.clear();
    0
}

fn cmd_history(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.get(1).is_some_and(|a| a == "-c") {
        sh.clear_history();
        return 0;
    }
    for (i, h) in sh.history.iter().enumerate() {
        io.styled(&format!("{:>5}  ", i + 1), Style::DIM);
        io.println(h);
    }
    0
}

fn cmd_env(sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    let mut vars = sh.env.clone();
    vars.sort();
    for (k, v) in vars {
        io.styled(&k, Style::fg(color::BRIGHT_BLUE));
        io.print("=");
        io.println(&v);
    }
    0
}

fn cmd_export(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return cmd_env(sh, io, args);
    }
    let mut status = 0;
    for a in &args[1..] {
        match a.split_once('=') {
            Some((k, v)) if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                sh.set_var(k, v)
            }
            _ => status = io.error(&args[0], &format!("'{a}': expected NAME=VALUE")),
        }
    }
    status
}

fn cmd_unset(sh: &mut Shell, _io: &mut Io, args: &[String]) -> i32 {
    for a in &args[1..] {
        sh.unset_var(a);
    }
    0
}

fn cmd_which(sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage(io, "which");
    }
    let mut status = 0;
    for name in &args[1..] {
        if let Some(b) = find(name) {
            io.println(&format!("{name}: shell built-in ({})", b.help.to_lowercase()));
        } else if let Some(app) = sh.apps().into_iter().find(|a| &a.id == name) {
            io.println(&format!("{name}: application \"{}\" ({})", app.name, app.exe));
        } else if sh.fs.exists(&format!("/system/bin/{name}.exe")) {
            io.println(&format!("/system/bin/{name}.exe"));
        } else {
            status = io.error("which", &format!("{name}: not found"));
        }
    }
    status
}

fn cmd_exit(sh: &mut Shell, _io: &mut Io, _args: &[String]) -> i32 {
    sh.exit_requested = true;
    0
}
