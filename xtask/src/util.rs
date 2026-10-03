//! Small helpers shared by the xtask commands.

use std::path::{Path, PathBuf};
use std::process::Command;

pub type Result<T = ()> = std::result::Result<T, String>;

/// The repository root (the directory containing the workspace `Cargo.toml`).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives in the workspace").to_path_buf()
}

/// Directory that receives the assembled image and QEMU state
/// (`$VINDOWS_OUT` overrides it, so several builds can run side by side).
pub fn out_dir() -> PathBuf {
    match std::env::var_os("VINDOWS_OUT") {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { workspace_root().join(p) }
        }
        None => workspace_root().join("target").join("vindows"),
    }
}

/// Cargo's target directory (`$CARGO_TARGET_DIR` overrides it).
pub fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { workspace_root().join(p) }
        }
        None => workspace_root().join("target"),
    }
}

/// Where build-time generated assets (wallpapers, sample media) go; they
/// are installed into the system image like `assets/`.
pub fn generated_dir() -> PathBuf {
    workspace_root().join("target").join("generated")
}

/// Runs `cmd`, streaming its output, and fails if it does not succeed.
pub fn run(cmd: &mut Command) -> Result {
    let shown = format!("{cmd:?}");
    let status = cmd.status().map_err(|e| format!("failed to start {shown}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("command failed ({status}): {shown}")) }
}

/// The `cargo` executable that invoked us (falls back to `cargo` on PATH).
pub fn cargo() -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(workspace_root());
    cmd
}

pub fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Searches `PATH` for an executable.
/// The host's current offset from UTC as `+HH:MM` (daylight saving
/// included), for the guest's local time.
pub fn host_utc_offset() -> Option<String> {
    let minutes = host_offset_minutes()?;
    let sign = if minutes < 0 { '-' } else { '+' };
    Some(format!("{sign}{:02}:{:02}", minutes.abs() / 60, minutes.abs() % 60))
}

#[cfg(windows)]
fn host_offset_minutes() -> Option<i64> {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(t: *mut SystemTime);
        fn GetSystemTime(t: *mut SystemTime);
    }
    let (mut local, mut utc) = (SystemTime::default(), SystemTime::default());
    // SAFETY: both functions fill in the structure passed.
    unsafe {
        GetLocalTime(&mut local);
        GetSystemTime(&mut utc);
    }
    let minutes = |t: &SystemTime| {
        let (y, m, d) = (t.year as i64, t.month as i64, t.day as i64);
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
        let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy;
        days * 1440 + t.hour as i64 * 60 + t.minute as i64
    };
    // Time zones are whole quarter hours.
    let diff = minutes(&local) - minutes(&utc);
    Some((diff as f64 / 15.0).round() as i64 * 15)
}

#[cfg(not(windows))]
fn host_offset_minutes() -> Option<i64> {
    let out = Command::new("date").arg("+%z").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let digits = s.trim_start_matches(['+', '-']);
    let (h, m) = (digits.get(..2)?.parse::<i64>().ok()?, digits.get(2..4)?.parse::<i64>().ok()?);
    Some(sign * (h * 60 + m))
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) && !name.ends_with(".exe") { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Prints a status line in the style of cargo.
pub fn status(verb: &str, msg: impl std::fmt::Display) {
    eprintln!("\x1b[1;32m{verb:>12}\x1b[0m {msg}");
}
