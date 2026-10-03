//! Cross-building the individual Vindows components with cargo.
//!
//! * The UEFI loader and the kernel are built for `x86_64-unknown-uefi`
//!   (freestanding, soft-float, PE/COFF output).
//! * User-space programs are `no_std` PE executables built for
//!   `x86_64-pc-windows-msvc`, which gives us hard-float SSE code on stable
//!   Rust, linked by the Microsoft linker. Each program's `build.rs` (via
//!   the `vbuild` helper crate) supplies the Vindows-specific linker options.

use std::path::PathBuf;

use crate::util::{self, Result};

pub const UEFI_TARGET: &str = "x86_64-unknown-uefi";
pub const USER_TARGET: &str = "x86_64-pc-windows-msvc";

/// A user-space program and where it is installed in the initrd.
#[derive(Debug, Clone, Copy)]
pub struct Program {
    /// Cargo package name.
    pub package: &'static str,
    /// Binary (executable) name produced by the package.
    pub binary: &'static str,
}

/// Every user-space program that ships in the initrd, installed as
/// `bin/<binary>.exe`.
pub const PROGRAMS: &[Program] = &[
    Program { package: "init", binary: "init" },
    Program { package: "vfs", binary: "vfs" },
    Program { package: "systest", binary: "systest" },
    Program { package: "devmgr", binary: "devmgr" },
    Program { package: "virtio-input", binary: "virtio-input" },
    Program { package: "virtio-blk", binary: "virtio-blk" },
    Program { package: "ps2", binary: "ps2" },
    Program { package: "compositor", binary: "compositor" },
    Program { package: "about", binary: "about" },
    Program { package: "racer", binary: "racer" },
    Program { package: "starfall", binary: "starfall" },
    Program { package: "virtio-snd", binary: "virtio-snd" },
    Program { package: "audio", binary: "audio" },
    Program { package: "music", binary: "music" },
    Program { package: "photos", binary: "photos" },
    Program { package: "terminal", binary: "terminal" },
    Program { package: "taskmgr", binary: "taskmgr" },
    Program { package: "files", binary: "files" },
    Program { package: "settings", binary: "settings" },
    Program { package: "editor", binary: "editor" },
    Program { package: "shell", binary: "shell" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

impl Profile {
    fn cargo_flag(self) -> &'static [&'static str] {
        match self {
            Profile::Debug => &[],
            Profile::Release => &["--release"],
        }
    }

    pub fn dir(self) -> &'static str {
        match self {
            Profile::Debug => "debug",
            Profile::Release => "release",
        }
    }
}

/// Paths of everything produced by [`build_all`].
pub struct Artifacts {
    pub bootloader: PathBuf,
    pub kernel: PathBuf,
    pub programs: Vec<(Program, PathBuf)>,
}

fn target_dir() -> PathBuf {
    util::target_dir()
}

fn build_uefi(package: &str, profile: Profile) -> Result<PathBuf> {
    util::status("Building", format!("{package} ({UEFI_TARGET})"));
    util::run(util::cargo().args(["build", "--package", package, "--target", UEFI_TARGET]).args(profile.cargo_flag()))?;
    Ok(target_dir().join(UEFI_TARGET).join(profile.dir()).join(format!("{package}.efi")))
}

/// Builds every program except those named in `skip`.
fn build_programs(profile: Profile, skip: &[String]) -> Result<Vec<(Program, PathBuf)>> {
    let programs: Vec<Program> = PROGRAMS.iter().copied().filter(|p| !skip.iter().any(|s| s == p.package)).collect();
    if programs.is_empty() {
        return Ok(Vec::new());
    }
    util::status("Building", format!("{} user-space programs ({USER_TARGET})", programs.len()));
    let mut cmd = util::cargo();
    cmd.args(["build", "--target", USER_TARGET]).args(profile.cargo_flag());
    for p in &programs {
        cmd.args(["--package", p.package]);
    }
    util::run(&mut cmd)?;
    let dir = target_dir().join(USER_TARGET).join(profile.dir());
    Ok(programs.iter().map(|p| (*p, dir.join(format!("{}.exe", p.binary)))).collect())
}

/// Builds the loader, the kernel and the user-space programs (all but `skip`).
pub fn build_all(profile: Profile, skip: &[String]) -> Result<Artifacts> {
    let bootloader = build_uefi("vboot", profile)?;
    let kernel = build_uefi("vkernel", profile)?;
    let programs = build_programs(profile, skip)?;
    Ok(Artifacts { bootloader, kernel, programs })
}
