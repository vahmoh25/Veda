//! The C toolchain: GCC, binutils and musl for Veda.
//!
//! `cargo xtask toolchain` builds, from the sources in `ports/`:
//!
//! * a **cross toolchain** that runs on this machine and makes Veda
//!   programs (`x86_64-veda-gcc` and friends, with a sysroot laid out like
//!   Veda's `/system`), used to build the C tests and ports;
//! * the **native toolchain**, the same compiler built to run inside Veda,
//!   which the system image installs in `/system` (`gcc`, `as`, `ld`, the C
//!   library and its headers).
//!
//! Everything lives under `target/toolchain` (`$VEDA_TOOLCHAIN` overrides
//! it): `downloads/` (the verified source archives), `src/` (unpacked and
//! patched), `build/`, `cross/` and `native/`. The GNU packages are built
//! with `ports/build.sh` under a Unix shell: MSYS2 on Windows
//! (`$VEDA_MSYS2`, by default `C:\msys64`).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{self, Result};

/// The target triple of Veda's C programs.
pub const TARGET: &str = "x86_64-veda";

/// Where the toolchain is built.
pub fn root() -> PathBuf {
    match std::env::var_os("VEDA_TOOLCHAIN") {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { util::workspace_root().join(p) }
        }
        None => util::workspace_root().join("target").join("toolchain"),
    }
}

/// Whether the `cross toolchain` or the `native toolchain` has been built.
pub fn built(what: &str) -> bool {
    match what {
        "cross toolchain" => cross_gcc().is_some(),
        _ => root().join("native").join("system").join("bin").join("gcc").is_file(),
    }
}

/// For `cargo xtask doctor`: whether the toolchain, which is optional, has
/// been built (`Ok`), and if not, whether it can be (`Err`).
pub fn status() -> std::result::Result<String, String> {
    if built("native toolchain") && built("cross toolchain") {
        return Ok(format!("built ({})", root().display()));
    }
    let how = match shell() {
        Ok(_) => "`cargo xtask toolchain` builds it".to_string(),
        Err(e) => format!("`cargo xtask toolchain` builds it, with MSYS2: {e}"),
    };
    Err(format!("not built (optional: C and GCC in the image); {how}"))
}

/// The cross compiler, if it has been built.
pub fn cross_gcc() -> Option<PathBuf> {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    let gcc = root().join("cross").join("bin").join(format!("{TARGET}-gcc{exe}"));
    gcc.is_file().then_some(gcc)
}

/// A tool of the cross toolchain (`ld`, `objcopy`, `ar`, ...).
fn cross_tool(name: &str) -> PathBuf {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    root().join("cross").join("bin").join(format!("{TARGET}-{name}{exe}"))
}

/// The symbols the POSIX layer gives the C library (and, for the `veda_`
/// ones, C programs: `<veda/ipc.h>`); all else in it stays private.
const POSIX_LAYER_EXPORTS: [&str; 21] = [
    "__veda_syscall",
    "__veda_init",
    "__veda_spawn",
    "__veda_clone",
    "__veda_set_thread_area",
    "__veda_unmapself",
    "__veda_bootstrap",
    "veda_close",
    "veda_duplicate",
    "veda_service_register",
    "veda_service_accept",
    "veda_service_connect",
    "veda_channel_write",
    "veda_channel_read",
    "veda_wait",
    "veda_now_ns",
    "veda_vmo_create",
    "veda_vmo_map",
    "veda_vmo_unmap",
    "veda_event_create",
    "veda_object_signal",
];

/// Builds the POSIX layer (`lib/posix`) as a static library for
/// `x86_64-unknown-none`; returns its path.
pub fn posix_library() -> Result<PathBuf> {
    util::status("Building", "the POSIX layer (x86_64-unknown-none)");
    util::run(util::cargo().args([
        "rustc",
        "--package",
        "vposix",
        "--target",
        "x86_64-unknown-none",
        "--release",
        "--crate-type",
        "staticlib",
    ]))?;
    Ok(util::target_dir().join("x86_64-unknown-none").join("release").join("libvposix.a"))
}

/// Links the POSIX layer's static library into the one relocatable object
/// the C library holds, which exports [`POSIX_LAYER_EXPORTS`] and nothing
/// else (no Rust or compiler-builtins symbol can clash with the C
/// library's). `ports/build.sh` does the same for the first build.
pub fn posix_object(lib: &Path, out: &Path) -> Result {
    let whole = out.with_extension("all.o");
    let mut ld = Command::new(cross_tool("ld"));
    ld.args(["-r", "--gc-sections"]);
    for s in POSIX_LAYER_EXPORTS {
        ld.args(["-u", s]);
    }
    util::run(ld.arg("-o").arg(&whole).arg(lib))?;
    let mut objcopy = Command::new(cross_tool("objcopy"));
    objcopy.args(["--strip-debug", "--remove-section=.llvmbc", "--remove-section=.llvmcmd"]);
    for s in POSIX_LAYER_EXPORTS {
        objcopy.arg(format!("--keep-global-symbol={s}"));
    }
    util::run(objcopy.arg(&whole).arg(out))?;
    let _ = std::fs::remove_file(&whole);
    Ok(())
}

/// The C library's object of the POSIX layer, as last made.
fn posix_layer_object() -> PathBuf {
    root().join("build").join("veda.o")
}

/// Puts the current POSIX layer into the cross toolchain's C library (and
/// the native toolchain's, if built), so that C programs built from now
/// on get it.
pub fn refresh_posix_layer() -> Result {
    let object = posix_layer_object();
    let library = posix_library()?;
    if !is_newer(&object, &[&library]) {
        std::fs::create_dir_all(object.parent().unwrap()).map_err(|e| format!("{e}"))?;
        posix_object(&library, &object)?;
    }
    let headers = util::workspace_root().join("lib").join("posix").join("include").join("veda");
    for sysroot in [root().join("cross").join("sysroot"), root().join("native")] {
        let libc = sysroot.join("system").join("lib").join("libc.a");
        if libc.is_file() && !is_newer(&libc, &[&object]) {
            util::run(Command::new(cross_tool("ar")).arg("rcs").arg(&libc).arg(&object))?;
        }
        // Its own header, `<veda/ipc.h>`.
        if libc.is_file() {
            let dir = sysroot.join("system").join("include").join("veda");
            std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            for entry in
                std::fs::read_dir(&headers).map_err(|e| format!("reading {}: {e}", headers.display()))?.flatten()
            {
                let to = dir.join(entry.file_name());
                if !is_newer(&to, &[&entry.path()]) {
                    std::fs::copy(entry.path(), &to).map_err(|e| format!("copying to {}: {e}", to.display()))?;
                }
            }
        }
    }
    Ok(())
}

/// Mesa's build directories, which `ports/build.sh` configures: for Veda,
/// and for this machine (Windows only: the renderer as a library, for the
/// OpenGL ES tests).
fn mesa_build() -> PathBuf {
    root().join("build").join("mesa")
}

fn mesa_host_build() -> PathBuf {
    root().join("build").join("mesa-host")
}

/// Runs ninja for `target` in a build directory of Mesa's (showing its
/// output only if it fails). The generators it runs are meson's, which is
/// MSYS2's own and says so unless MSYSTEM agrees.
fn ninja(dir: &Path, target: &str) -> Result {
    let log = dir.join("ninja.log.txt");
    let mut sh = shell()?;
    sh.arg("-c").arg(format!(
        "MSYSTEM=MSYS ninja -C '{}' '{}' > '{}' 2>&1 || {{ tail -n 40 '{}'; exit 1; }}",
        mixed(dir),
        target,
        mixed(&log),
        mixed(&log)
    ));
    util::run(&mut sh)
}

/// Veda's renderer service (`services/renderer` on Mesa's Gallium), up to
/// date and stripped; None if Mesa has not been configured
/// (`cargo xtask toolchain`).
pub fn renderer() -> Result<Option<Vec<u8>>> {
    let dir = mesa_build();
    if !dir.join("build.ninja").is_file() {
        return Ok(None);
    }
    refresh_posix_layer()?;
    let exe = dir.join("src").join("gallium").join("targets").join("veda").join("renderer");
    // ninja does not see the C library: the program is linked again when
    // it has changed.
    let libc = root().join("cross").join("sysroot").join("system").join("lib").join("libc.a");
    if exe.is_file() && !is_newer(&exe, &[&libc]) {
        std::fs::remove_file(&exe).map_err(|e| format!("removing {}: {e}", exe.display()))?;
    }
    util::status("Building", "the renderer (Mesa)");
    ninja(&dir, "src/gallium/targets/veda/renderer")?;
    let stripped = root().join("build").join("renderer");
    if !is_newer(&stripped, &[&exe]) {
        util::run(Command::new(cross_tool("strip")).arg("-o").arg(&stripped).arg(&exe))?;
    }
    Ok(Some(util::read(&stripped)?))
}

/// The renderer as a library on softpipe for this machine
/// (`vgallium.dll`), up to date; None if Mesa has not been configured for
/// it (Windows only).
pub fn vgallium() -> Result<Option<PathBuf>> {
    let dir = mesa_host_build();
    if !dir.join("build.ninja").is_file() {
        return Ok(None);
    }
    util::status("Building", "the renderer on softpipe for the host (Mesa)");
    ninja(&dir, "src/gallium/targets/veda/vgallium.dll")?;
    Ok(Some(dir.join("src").join("gallium").join("targets").join("veda").join("vgallium.dll")))
}

/// How each C test program is linked: once at a fixed address, as GCC links
/// by default, and once position-independent. The suffix of the
/// executable's name, and the flags.
const C_TEST_VARIANTS: &[(&str, &[&str])] = &[("", &[]), ("-pie", &["-fPIE", "-static-pie"])];

/// Compiles the test programs in C and C++ (`tests/c/*.c` and `*.cc`, each
/// in every one of [`C_TEST_VARIANTS`]) for the system image: `(name,
/// executable)`. None without a cross compiler.
pub fn c_tests() -> Result<Vec<(String, Vec<u8>)>> {
    let Some(gcc) = cross_gcc() else { return Ok(Vec::new()) };
    let gxx = cross_tool("g++");
    refresh_posix_layer()?;
    let libc = root().join("cross").join("sysroot").join("system").join("lib").join("libc.a");
    let src = util::workspace_root().join("tests").join("c");
    let out = root().join("tests");
    std::fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&src)
        .map_err(|e| format!("reading {}: {e}", src.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "c" || x == "cc"))
        .collect();
    sources.sort();
    let mut programs = Vec::new();
    for source in sources {
        let stem = source.file_stem().unwrap().to_string_lossy().into_owned();
        let compiler = if source.extension().is_some_and(|x| x == "cc") { &gxx } else { &gcc };
        for (suffix, flags) in C_TEST_VARIANTS {
            let name = format!("{stem}{suffix}");
            let exe = out.join(&name);
            if !is_newer(&exe, &[&source, compiler, &libc]) {
                util::run(
                    Command::new(compiler)
                        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-g0", "-s"])
                        .args(*flags)
                        .arg("-o")
                        .arg(&exe)
                        .arg(&source),
                )?;
            }
            programs.push((name, util::read(&exe)?));
        }
    }
    Ok(programs)
}

/// Where [`command`] notes the POSIX layer the native programs were linked
/// with: the SHA-256 of its object.
fn native_layer_note() -> PathBuf {
    root().join("build").join("native-posix-layer")
}

/// The native toolchain's programs are C programs too, with the POSIX layer
/// linked in: when it has changed since they were linked, links them again
/// (`ports/build.sh` redoes only that). Without MSYS2 it says so and leaves
/// them as they are.
pub fn relink_native() -> Result {
    let object = posix_layer_object();
    if !built("native toolchain") || !object.is_file() {
        return Ok(());
    }
    let current = sha256_of(&object)?;
    if std::fs::read_to_string(native_layer_note()).is_ok_and(|linked| linked.trim() == current) {
        return Ok(());
    }
    if let Err(e) = shell() {
        util::status("Note", format!("the C toolchain in the image has an older POSIX layer. {e}"));
        return Ok(());
    }
    util::status("Linking", "the C toolchain's programs with the current POSIX layer");
    command(&[])
}

/// The native toolchain's files for the system image: paths relative to
/// `/system` and their contents. None if it has not been built.
pub fn native_files() -> Result<Vec<(String, Vec<u8>)>> {
    let base = root().join("native").join("system");
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let mut stack = vec![base.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(&base).unwrap().to_string_lossy().replace('\\', "/");
                files.push((rel, util::read(&path)?));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// A source package of `ports/` (its `port.toml`).
struct Port {
    name: String,
    url: String,
    sha256: String,
}

impl Port {
    fn load(dir: &Path) -> Result<Port> {
        let file = dir.join("port.toml");
        let text = std::fs::read_to_string(&file).map_err(|e| format!("reading {}: {e}", file.display()))?;
        let value = |key: &str| -> Result<String> {
            text.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().trim_matches('"').to_string())
                .ok_or_else(|| format!("{}: no {key}", file.display()))
        };
        Ok(Port { name: value("name")?, url: value("url")?, sha256: value("sha256")? })
    }

    fn archive(&self) -> PathBuf {
        root().join("downloads").join(self.url.rsplit('/').next().unwrap_or(&self.name))
    }
}

/// The ports the toolchain is built from.
fn ports() -> Result<Vec<Port>> {
    ["binutils", "gcc", "gmp", "mpfr", "mpc", "musl"]
        .iter()
        .map(|p| Port::load(&util::workspace_root().join("ports").join(p)))
        .collect()
}

fn sha256_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = util::read(path)?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads a port's source archive unless it is there, and checks it
/// against the SHA-256 the port pins.
fn fetch(port: &Port) -> Result {
    let archive = port.archive();
    if archive.is_file() && sha256_of(&archive)? == port.sha256 {
        return Ok(());
    }
    util::status("Downloading", &port.url);
    std::fs::create_dir_all(archive.parent().unwrap()).map_err(|e| format!("{e}"))?;
    let partial = archive.with_extension("part");
    util::run(
        Command::new("curl")
            .args(["-fL", "--retry", "3", "--silent", "--show-error", "-o"])
            .arg(&partial)
            .arg(&port.url),
    )?;
    let got = sha256_of(&partial)?;
    if got != port.sha256 {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("{}: SHA-256 {got}, expected {} (ports/{}/port.toml)", port.url, port.sha256, port.name));
    }
    std::fs::rename(&partial, &archive).map_err(|e| format!("{e}"))
}

/// The Unix shell that runs `ports/build.sh`: MSYS2's on Windows.
fn shell() -> Result<Command> {
    if !cfg!(windows) {
        return Ok(Command::new("bash"));
    }
    let msys = std::env::var_os("VEDA_MSYS2").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\msys64"));
    let bash = msys.join("usr").join("bin").join("bash.exe");
    if !bash.is_file() {
        return Err(format!(
            "MSYS2 was not found at {} (set VEDA_MSYS2). Install it from https://www.msys2.org, then in its \
             UCRT64 shell: pacman -S make m4 bison flex texinfo diffutils patch mingw-w64-ucrt-x86_64-gcc",
            msys.display()
        ));
    }
    let mut cmd = Command::new(bash);
    // A login shell of the UCRT64 environment, in the current directory.
    cmd.env("MSYSTEM", "UCRT64").env("CHERE_INVOKING", "1").arg("-l");
    Ok(cmd)
}

/// A path as both Windows and MSYS2 programs read it (`C:/x/y`).
fn mixed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `cargo xtask toolchain [--jobs N]`.
pub fn command(args: &[String]) -> Result {
    let mut jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jobs" | "-j" => {
                jobs = it.next().and_then(|v| v.parse().ok()).ok_or("--jobs takes a number")?;
            }
            other => return Err(format!("unknown option '{other}' (toolchain takes --jobs N)")),
        }
    }
    let started = std::time::Instant::now();
    for port in ports()? {
        fetch(&port)?;
    }
    // The script makes the C library's object of the POSIX layer once its
    // cross linker exists.
    let library = posix_library()?;
    let script = util::workspace_root().join("ports").join("build.sh");
    let mut sh = shell()?;
    sh.arg(mixed(&script))
        .env("VEDA_PORTS", mixed(&util::workspace_root().join("ports")))
        .env("VEDA_TOOLCHAIN", mixed(&root()))
        .env("VEDA_POSIX_LIB", mixed(&library))
        .env("VEDA_POSIX_EXPORTS", POSIX_LAYER_EXPORTS.join(" "))
        .env("JOBS", jobs.to_string());
    util::run(&mut sh)?;
    // The native programs now have the POSIX layer's object as it is.
    let object = posix_layer_object();
    let note = native_layer_note();
    std::fs::write(&note, sha256_of(&object)?).map_err(|e| format!("writing {}: {e}", note.display()))?;
    util::status("Finished", format!("the C toolchain in {:.0} min", started.elapsed().as_secs_f32() / 60.0));
    Ok(())
}

/// Whether `target` exists and is newer than every input.
fn is_newer(target: &Path, inputs: &[&Path]) -> bool {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let Some(t) = modified(target) else { return false };
    inputs.iter().all(|i| modified(i).is_some_and(|m| m <= t))
}
