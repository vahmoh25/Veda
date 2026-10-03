//! Helpers for the `build.rs` scripts of Vindows user-space crates.
//!
//! * [`user_program`] emits the linker options that turn a `no_std` binary
//!   into a Vindows PE executable (fixed base, custom entry point, no CRT).
//! * [`compile_cpp`] compiles freestanding C++ sources with MSVC and links
//!   them into the crate.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Image base of every Vindows executable.
pub const IMAGE_BASE: &str = "0x140000000";

fn is_vindows_target() -> bool {
    std::env::var("TARGET").is_ok_and(|t| t == "x86_64-pc-windows-msvc")
}

/// Emits the linker arguments for a Vindows user-space executable. Call this
/// from the `build.rs` of every program crate.
pub fn user_program() {
    println!("cargo:rerun-if-changed=build.rs");
    if !is_vindows_target() {
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

/// Locations of the MSVC tools.
pub struct Msvc {
    pub cl: PathBuf,
    pub lib: PathBuf,
    pub include: PathBuf,
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
            return Err("Visual Studio with the C++ tools was not found".into());
        }
        let version_file = Path::new(&install).join(r"VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt");
        let version = std::fs::read_to_string(&version_file)
            .map_err(|e| format!("reading {}: {e}", version_file.display()))?
            .trim()
            .to_string();
        Path::new(&install).join(r"VC\Tools\MSVC").join(version)
    };
    let bin = tools_dir.join(r"bin\Hostx64\x64");
    let msvc = Msvc { cl: bin.join("cl.exe"), lib: bin.join("lib.exe"), include: tools_dir.join("include") };
    if !msvc.cl.is_file() {
        return Err(format!("cl.exe not found at {}", msvc.cl.display()));
    }
    Ok(msvc)
}

/// Compiles freestanding C++20 `sources` (paths relative to the crate) into
/// a static library named `name` and links it into the crate.
///
/// The code is compiled without exceptions, RTTI, security cookies or the
/// C runtime; the Vindows runtime (`vrt`) supplies the few symbols the
/// compiler may reference (`memcpy`, `memset`, `_fltused`, ...).
pub fn compile_cpp(name: &str, sources: &[&str], include_dirs: &[&str]) {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let msvc = find_msvc().unwrap_or_else(|e| panic!("C++ compiler unavailable: {e}"));
    let profile_release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
    let mut objects = Vec::new();
    for src in sources {
        let path = manifest.join(src);
        println!("cargo:rerun-if-changed={}", path.display());
        let obj = out_dir.join(Path::new(src).file_stem().unwrap()).with_extension("obj");
        let mut cmd = Command::new(&msvc.cl);
        cmd.args(["/nologo", "/c", "/std:c++20", "/GS-", "/GR-", "/EHs-c-", "/Zl", "/X", "/fp:fast", "/Gy", "/W4"]);
        cmd.arg(if profile_release { "/O2" } else { "/Od" });
        cmd.arg(format!("/I{}", msvc.include.display()));
        for inc in include_dirs {
            let dir = manifest.join(inc);
            println!("cargo:rerun-if-changed={}", dir.display());
            cmd.arg(format!("/I{}", dir.display()));
        }
        cmd.arg(format!("/Fo{}", obj.display())).arg(&path);
        let output = cmd.output().unwrap_or_else(|e| panic!("running cl.exe: {e}"));
        if !output.status.success() {
            panic!(
                "cl.exe failed on {}:\n{}{}",
                src,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        objects.push(obj);
    }
    let lib_path = out_dir.join(format!("{name}.lib"));
    let status = Command::new(&msvc.lib)
        .arg("/nologo")
        .arg(format!("/OUT:{}", lib_path.display()))
        .args(&objects)
        .status()
        .unwrap_or_else(|e| panic!("running lib.exe: {e}"));
    assert!(status.success(), "lib.exe failed");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static={name}");
}
