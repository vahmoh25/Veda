//! The Linux guest of the driver VM (see `docs/DRIVERVM.md`).
//!
//! * `cargo xtask linux` builds its kernel from `ports/linux` (first zlib
//!   and elfutils' libelf, from `ports/zlib` and `ports/elfutils`, for the
//!   kernel's build tool objtool): `target/linux/bzImage`.
//!   It also builds a cross toolchain for the guest's programs in C and C++
//!   (`target/linux/toolchain`, for Linux on musl) and configures Mesa's
//!   build with it for the renderer, and for this machine (`vgallium.so`,
//!   the renderer's decoder on softpipe, for the OpenGL ES tests).
//! * Once it is built, every image build also builds the guest's programs
//!   (`guest/`: Rust for Linux on musl, static; the renderer, Mesa's build
//!   around its Rust half) and packs them into the initial RAM file system
//!   the kernel starts with, with the firmware its drivers load
//!   (`ports/linux/firmware.txt`). Both go into the system
//!   image (`linux/bzImage`, `linux/initramfs.cpio`), where `drivervm`
//!   finds them.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::toolchain::{self, Port};
use crate::util::{self, Result};

/// The target of the guest's programs (`.cargo/config.toml` sets its
/// linker and `veda_guest`).
pub const GUEST_TARGET: &str = "x86_64-unknown-linux-musl";

/// The guest's programs: the package, its binary, and where it goes in the
/// initial RAM file system.
const GUEST_PROGRAMS: &[(&str, &str, &str)] = &[
    ("guest-init", "init", "init"),
    ("guest-bridgetest", "bridgetest", "bin/bridgetest"),
    ("guest-pcitest", "pcitest", "bin/pcitest"),
    ("guest-gpiotest", "gpiotest", "bin/gpiotest"),
    ("guest-efivartest", "efivartest", "bin/efivartest"),
    ("guest-kmspause", "kmspause", "bin/kmspause"),
    ("guest-alsa", "alsa", "bin/alsa"),
    ("guest-net", "net", "bin/net"),
    ("guest-wifi", "wifi", "bin/wifi"),
    ("guest-airlink", "airlink", "bin/airlink"),
    ("guest-kms", "kms", "bin/kms"),
    ("guest-input", "input", "bin/input"),
];

/// Tools the build runs: the kernel's, beyond a C compiler; GCC's (m4, and
/// the GMP, MPFR and MPC it links with); meson and ninja for Mesa.
const BUILD_TOOLS: [&str; 14] =
    ["gcc", "g++", "make", "m4", "flex", "bison", "bc", "perl", "tar", "xz", "patch", "curl", "meson", "ninja"];

/// Where the guest's kernel is built (`$VEDA_LINUX` overrides it).
pub fn root() -> PathBuf {
    match std::env::var_os("VEDA_LINUX") {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { util::workspace_root().join(p) }
        }
        None => util::workspace_root().join("target").join("linux"),
    }
}

/// Whether the guest's kernel has been built.
pub fn built() -> bool {
    root().join("bzImage").is_file()
}

/// Mesa's build directories, which `ports/linux/build.sh` configures: for
/// the guest (the renderer), and for this machine (the renderer's decoder
/// as a library, for the OpenGL ES tests).
fn mesa_build() -> PathBuf {
    root().join("build").join("mesa")
}

fn mesa_host_build() -> PathBuf {
    root().join("build").join("mesa-host")
}

/// The renderer's Rust half (`guest/renderer`), a static library that
/// Mesa's build links the program around, built.
fn renderer_lib() -> Result<PathBuf> {
    util::run(util::cargo().args(["build", "--release", "--target", GUEST_TARGET, "--package", "guest-renderer"]))?;
    Ok(util::target_dir().join(GUEST_TARGET).join("release").join("librenderer.a"))
}

/// The renderer's decoder on softpipe for this machine (`vgallium.so`), up
/// to date; None if Mesa has not been configured for it.
pub fn vgallium() -> Result<Option<PathBuf>> {
    let dir = mesa_host_build();
    if !dir.join("build.ninja").is_file() {
        return Ok(None);
    }
    util::status("Building", "the renderer on softpipe for the host (Mesa)");
    util::ninja(&dir, "src/gallium/targets/veda/vgallium.so")?;
    Ok(Some(dir.join("src").join("gallium").join("targets").join("veda").join("vgallium.so")))
}

/// Whether the driver VM can run in this machine's QEMU: its Linux is
/// built, and KVM gives guests VMX of their own (nested virtualization,
/// on Intel's processors: Veda's hypervisor uses VMX).
pub fn runnable() -> Result {
    if !built() {
        return Err("its Linux is not built (`cargo xtask linux`)".into());
    }
    let nested = std::fs::read_to_string("/sys/module/kvm_intel/parameters/nested").unwrap_or_default();
    if !matches!(nested.trim(), "Y" | "1") {
        return Err("KVM gives guests no VMX here (kvm_intel nested=1)".into());
    }
    Ok(())
}

/// For `cargo xtask doctor`: whether the guest has been built (`Ok`), and if
/// not, whether it can be (`Err`).
pub fn status() -> std::result::Result<String, String> {
    if built() {
        return Ok(format!("built ({})", root().join("bzImage").display()));
    }
    let how = match tools() {
        Ok(()) => "`cargo xtask linux` builds it".to_string(),
        Err(e) => format!("`cargo xtask linux` builds it, but {e}"),
    };
    Err(format!("not built (the driver VM drives every device but the disks and sound); {how}"))
}

fn tools() -> Result {
    let missing: Vec<&str> = BUILD_TOOLS.into_iter().filter(|t| util::find_on_path(t).is_none()).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{} missing: install from the distribution's packages", missing.join(", ")))
    }
}

/// `cargo xtask linux [--jobs N]`.
pub fn command(args: &[String]) -> Result {
    let mut jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jobs" | "-j" => {
                jobs = it.next().and_then(|v| v.parse().ok()).ok_or("--jobs takes a number")?;
            }
            other => return Err(format!("unknown option '{other}' (linux takes --jobs N)")),
        }
    }
    tools()?;
    let started = std::time::Instant::now();
    let downloads = root().join("downloads");
    for name in ["zlib", "elfutils", "linux", "binutils", "gcc", "musl", "mesa"] {
        toolchain::fetch(&Port::named(name)?, &downloads)?;
    }
    firmware()?;
    let lib = renderer_lib()?;
    let ports = util::workspace_root().join("ports");
    util::run(
        Command::new("bash")
            .arg(ports.join("linux").join("build.sh"))
            .env("VEDA_PORTS", &ports)
            .env("VEDA_LINUX", root())
            .env("VEDA_RENDERER_LIB", &lib)
            .env("JOBS", jobs.to_string()),
    )?;
    util::status("Finished", format!("the driver VM's Linux in {:.0} s", started.elapsed().as_secs_f32()));
    Ok(())
}

/// Builds the guest the first time an image needs it, where this machine
/// has the tools for it (afterwards `cargo xtask linux` builds it again,
/// once its port changes).
pub fn build_once() -> Result {
    if built() {
        return Ok(());
    }
    match tools() {
        Ok(()) => command(&[]),
        Err(e) => {
            util::status("Note", format!("the driver VM's Linux cannot be built here: {e}"));
            Ok(())
        }
    }
}

/// The guest's kernel and initial RAM file system, for the system image,
/// if the kernel has been built.
pub fn guest() -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    if !built() {
        return Ok(None);
    }
    util::status("Building", format!("the driver VM's programs ({GUEST_TARGET})"));
    let mut cmd = util::cargo();
    cmd.args(["build", "--release", "--target", GUEST_TARGET, "--package", "guest-renderer"]);
    for (package, ..) in GUEST_PROGRAMS {
        cmd.args(["--package", package]);
    }
    util::run(&mut cmd)?;
    let bin = util::target_dir().join(GUEST_TARGET).join("release");
    let mut archive = cpio::Archive::new();
    archive.directory("dev");
    archive.directory("bin");
    // The console the kernel opens for the first program, before /dev is
    // mounted.
    archive.device("dev/console", 5, 1);
    for (_, binary, path) in GUEST_PROGRAMS {
        archive.file(path, 0o755, &util::read(&bin.join(binary))?);
    }
    // The renderer: Mesa's build links its decoder and drivers around the
    // Rust half just built (once `cargo xtask linux` has configured it).
    if mesa_build().join("build.ninja").is_file() {
        util::status("Building", "the driver VM's renderer (Mesa)");
        util::ninja(&mesa_build(), "src/gallium/targets/veda/renderer")?;
        let exe = mesa_build().join("src").join("gallium").join("targets").join("veda").join("renderer");
        let stripped = root().join("build").join("renderer");
        let strip = root().join("toolchain").join("bin").join("x86_64-linux-musl-strip");
        util::run(Command::new(strip).arg("-o").arg(&stripped).arg(&exe))?;
        archive.file("bin/renderer", 0o755, &util::read(&stripped)?);
    }
    let mut directories = std::collections::BTreeSet::new();
    for (name, file) in firmware()? {
        let path = format!("lib/firmware/{name}");
        // Its directories first, each once.
        for (at, _) in path.match_indices('/') {
            if directories.insert(path[..at].to_string()) {
                archive.directory(&path[..at]);
            }
        }
        archive.file(&path, 0o644, &util::read(&file)?);
    }
    Ok(Some((util::read(&root().join("bzImage"))?, archive.finish())))
}

/// The firmware of `ports/linux/firmware.txt`: each file's name, and where
/// it is (fetched now if it is not, or is not the file pinned).
fn firmware() -> Result<Vec<(String, PathBuf)>> {
    let list = util::workspace_root().join("ports").join("linux").join("firmware.txt");
    let text = std::fs::read_to_string(&list).map_err(|e| format!("{}: {e}", list.display()))?;
    let mut sources = std::collections::BTreeMap::new();
    let mut files = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let bad = || format!("{}:{}: not a source or a file: {line}", list.display(), n + 1);
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            [first, ..] if first.starts_with('#') => {}
            ["source", name, url] if url.contains("{}") => {
                sources.insert(name.to_string(), url.to_string());
            }
            [name, source, path, sha256] if sha256.len() == 64 && !name.contains("..") => {
                let url = sources.get(*source).ok_or_else(bad)?.replace("{}", path);
                let file = root().join("firmware").join(name);
                fetch_firmware(&file, &url, sha256)?;
                files.push((name.to_string(), file));
            }
            _ => return Err(bad()),
        }
    }
    Ok(files)
}

/// Makes `file` the one of `url` whose SHA-256 is `sha256`.
fn fetch_firmware(file: &Path, url: &str, sha256: &str) -> Result {
    if file.is_file() && toolchain::sha256_of(file)? == sha256 {
        return Ok(());
    }
    util::status("Downloading", url);
    std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| format!("{e}"))?;
    let partial = file.with_extension("part");
    util::run(
        Command::new("curl").args(["-fL", "--retry", "3", "--silent", "--show-error", "-o"]).arg(&partial).arg(url),
    )?;
    let got = toolchain::sha256_of(&partial)?;
    if got != sha256 {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("{url}: SHA-256 {got}, expected {sha256} (ports/linux/firmware.txt)"));
    }
    std::fs::rename(&partial, file).map_err(|e| format!("{e}"))
}

/// Archives in the `newc` format of cpio, as Linux unpacks its initial RAM
/// file system.
pub mod cpio {
    /// An archive being written.
    pub struct Archive {
        bytes: Vec<u8>,
        inode: u32,
    }

    const DIRECTORY: u32 = 0o040_000;
    const FILE: u32 = 0o100_000;
    const CHARACTER_DEVICE: u32 = 0o020_000;

    impl Archive {
        pub fn new() -> Archive {
            Archive { bytes: Vec::new(), inode: 1 }
        }

        pub fn directory(&mut self, path: &str) {
            self.entry(path, DIRECTORY | 0o755, &[], (0, 0));
        }

        pub fn file(&mut self, path: &str, permissions: u32, data: &[u8]) {
            self.entry(path, FILE | permissions, data, (0, 0));
        }

        pub fn device(&mut self, path: &str, major: u32, minor: u32) {
            self.entry(path, CHARACTER_DEVICE | 0o600, &[], (major, minor));
        }

        fn entry(&mut self, path: &str, mode: u32, data: &[u8], (major, minor): (u32, u32)) {
            let fields = [
                self.inode,
                mode,
                0, // uid
                0, // gid
                1, // links
                0, // mtime
                data.len() as u32,
                0, // the device it is on
                0,
                major,
                minor,
                path.len() as u32 + 1,
                0, // checksum
            ];
            self.inode += 1;
            self.bytes.extend_from_slice(b"070701");
            for f in fields {
                self.bytes.extend_from_slice(format!("{f:08X}").as_bytes());
            }
            self.bytes.extend_from_slice(path.as_bytes());
            self.bytes.push(0);
            self.pad();
            self.bytes.extend_from_slice(data);
            self.pad();
        }

        fn pad(&mut self) {
            while !self.bytes.len().is_multiple_of(4) {
                self.bytes.push(0);
            }
        }

        /// The archive, with its trailer.
        pub fn finish(mut self) -> Vec<u8> {
            self.entry("TRAILER!!!", 0, &[], (0, 0));
            self.bytes
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn entries_are_aligned() {
            let mut a = Archive::new();
            a.directory("dev");
            a.device("dev/console", 5, 1);
            a.file("init", 0o755, b"#!/bin/sh\n");
            let bytes = a.finish();
            assert!(bytes.len().is_multiple_of(4));
            // The first header, and the directory's name after it.
            assert_eq!(&bytes[..6], b"070701");
            assert_eq!(&bytes[14..22], b"000041ED");
            assert_eq!(&bytes[110..114], b"dev\0");
            // The console's header, after the directory's (110 bytes and
            // its name, to a multiple of four), with its device numbers.
            let header = &bytes[116..];
            assert_eq!(&header[..6], b"070701");
            assert_eq!(&header[78..86], b"00000005");
            assert_eq!(&header[86..94], b"00000001");
            assert!(bytes.windows(10).any(|w| w == b"TRAILER!!!"));
        }
    }
}
