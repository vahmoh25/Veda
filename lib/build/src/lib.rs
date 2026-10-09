//! Helpers for the `build.rs` scripts of Veda user-space crates.
//!
//! * [`user_program`] emits the linker options that turn a `no_std` binary
//!   into a Veda PE executable (fixed base, custom entry point, no CRT), in
//!   the Microsoft linker's syntax, which LLVM's linker (`rust-lld`, which
//!   links the programs on other hosts than Windows) takes too.
//! * [`find_msvc`] locates the Microsoft linker, which Rust's
//!   `x86_64-pc-windows-msvc` target links with on Windows (`cargo xtask
//!   doctor`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Image base of every Veda executable.
pub const IMAGE_BASE: &str = "0x140000000";

fn is_veda_target() -> bool {
    std::env::var("TARGET").is_ok_and(|t| t == "x86_64-pc-windows-msvc")
}

/// Emits the linker arguments for a Veda user-space executable. Call this
/// from the `build.rs` of every program crate.
pub fn user_program() {
    println!("cargo:rerun-if-changed=build.rs");
    if !is_veda_target() {
        return;
    }
    for arg in [
        "/NODEFAULTLIB",
        "/ENTRY:_vrt_start",
        "/SUBSYSTEM:NATIVE",
        &format!("/BASE:{IMAGE_BASE}"),
        "/FIXED",
        "/DYNAMICBASE:NO",
        "/INCREMENTAL:NO",
        "/MANIFEST:NO",
    ] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}

/// The MSVC tools Veda needs.
pub struct Msvc {
    /// The linker, `link.exe`.
    pub link: PathBuf,
}

/// Finds MSVC through `vswhere` (or the `VCToolsInstallDir` environment
/// variable inside a developer prompt).
pub fn find_msvc() -> Result<Msvc, String> {
    let tools_dir = if let Ok(dir) = std::env::var("VCToolsInstallDir") {
        PathBuf::from(dir)
    } else {
        let program_files_x86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
        let vswhere = Path::new(&program_files_x86).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
        let out = Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ])
            .output()
            .map_err(|e| format!("running {}: {e}", vswhere.display()))?;
        let install = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if install.is_empty() {
            return Err("Visual Studio (or its Build Tools) with the MSVC build tools was not found".into());
        }
        let version_file = Path::new(&install).join(r"VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt");
        let version = std::fs::read_to_string(&version_file)
            .map_err(|e| format!("reading {}: {e}", version_file.display()))?
            .trim()
            .to_string();
        Path::new(&install).join(r"VC\Tools\MSVC").join(version)
    };
    let link = tools_dir.join(r"bin\Hostx64\x64\link.exe");
    if !link.is_file() {
        return Err(format!("link.exe not found at {}", link.display()));
    }
    Ok(Msvc { link })
}
