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
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) && !name.ends_with(".exe") { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Prints a status line in the style of cargo.
pub fn status(verb: &str, msg: impl std::fmt::Display) {
    eprintln!("\x1b[1;32m{verb:>12}\x1b[0m {msg}");
}
