//! Cross-building the individual Vindows components with cargo.
//!
//! * The UEFI loader and the kernel are built for `x86_64-unknown-uefi`
//!   (freestanding, soft-float, PE/COFF output).
//! * User-space programs are `no_std` PE executables built for
//!   `x86_64-pc-windows-msvc`, which gives us hard-float SSE code and lets us
//!   link C++ objects produced by MSVC. Each program's `build.rs` (via the
//!   `vbuild` helper crate) supplies the Vindows-specific linker options.

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
pub const PROGRAMS: &[Program] = &[Program { package: "init", binary: "init" }];

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
    util::workspace_root().join("target")
}

fn build_uefi(package: &str, profile: Profile) -> Result<PathBuf> {
    util::status("Building", format!("{package} ({UEFI_TARGET})"));
    util::run(util::cargo().args(["build", "--package", package, "--target", UEFI_TARGET]).args(profile.cargo_flag()))?;
    Ok(target_dir().join(UEFI_TARGET).join(profile.dir()).join(format!("{package}.efi")))
}

fn build_programs(profile: Profile) -> Result<Vec<(Program, PathBuf)>> {
    if PROGRAMS.is_empty() {
        return Ok(Vec::new());
    }
    util::status("Building", format!("{} user-space programs ({USER_TARGET})", PROGRAMS.len()));
    let mut cmd = util::cargo();
    cmd.args(["build", "--target", USER_TARGET]).args(profile.cargo_flag());
    for p in PROGRAMS {
        cmd.args(["--package", p.package]);
    }
    util::run(&mut cmd)?;
    let dir = target_dir().join(USER_TARGET).join(profile.dir());
    Ok(PROGRAMS.iter().map(|p| (*p, dir.join(format!("{}.exe", p.binary)))).collect())
}

/// Builds the loader, the kernel and all user-space programs.
pub fn build_all(profile: Profile) -> Result<Artifacts> {
    let bootloader = build_uefi("vboot", profile)?;
    let kernel = build_uefi("vkernel", profile)?;
    let programs = build_programs(profile)?;
    Ok(Artifacts { bootloader, kernel, programs })
}
