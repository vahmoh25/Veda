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
//! with `ports/build.sh`.

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
        Err(e) => format!("`cargo xtask toolchain` builds it, but {e}"),
    };
    Err(format!("not built (optional: C and GCC in the image); {how}"))
}

/// The cross compiler, if it has been built.
pub fn cross_gcc() -> Option<PathBuf> {
    let gcc = root().join("cross").join("bin").join(format!("{TARGET}-gcc"));
    gcc.is_file().then_some(gcc)
}

/// A tool of the cross toolchain (`ld`, `objcopy`, `ar`, ...).
fn cross_tool(name: &str) -> PathBuf {
    root().join("cross").join("bin").join(format!("{TARGET}-{name}"))
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
/// (`ports/build.sh` redoes only that). Without the build tools it says so
/// and leaves them as they are.
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
pub struct Port {
    name: String,
    url: String,
    sha256: String,
}

impl Port {
    /// The port in `ports/NAME`.
    pub fn named(name: &str) -> Result<Port> {
        Port::load(&util::workspace_root().join("ports").join(name))
    }

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

    /// Where its archive is downloaded to, in `downloads`.
    fn archive(&self, downloads: &Path) -> PathBuf {
        downloads.join(self.url.rsplit('/').next().unwrap_or(&self.name))
    }
}

/// The ports the toolchain is built from.
fn ports() -> Result<Vec<Port>> {
    ["binutils", "gcc", "gmp", "mpfr", "mpc", "musl"].iter().map(|p| Port::named(p)).collect()
}

pub fn sha256_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = util::read(path)?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads a port's source archive into `downloads` unless it is there,
/// and checks it against the SHA-256 the port pins. An archive the other
/// build (the C toolchain's, or the driver VM's Linux) has is copied.
pub fn fetch(port: &Port, downloads: &Path) -> Result {
    let archive = port.archive(downloads);
    if archive.is_file() && sha256_of(&archive)? == port.sha256 {
        return Ok(());
    }
    for other in [root().join("downloads"), crate::linux::root().join("downloads")] {
        let copy = port.archive(&other);
        if other != downloads && copy.is_file() && sha256_of(&copy)? == port.sha256 {
            std::fs::create_dir_all(downloads).map_err(|e| format!("{e}"))?;
            std::fs::copy(&copy, &archive).map_err(|e| format!("copying {}: {e}", copy.display()))?;
            return Ok(());
        }
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

/// The programs the build needs from the system: compilers and GNU tools,
/// and curl for the downloads (`docs/C.md` names the packages).
const BUILD_TOOLS: [&str; 8] = ["gcc", "g++", "make", "m4", "bison", "flex", "patch", "curl"];

/// The shell that runs `ports/build.sh`, once the build tools are there.
fn shell() -> Result<Command> {
    let missing: Vec<&str> = BUILD_TOOLS.into_iter().filter(|t| util::find_on_path(t).is_none()).collect();
    match missing.as_slice() {
        [] => Ok(Command::new("bash")),
        [tool] => Err(format!("{tool} is missing: install it from the distribution's packages (see docs/C.md)")),
        [tools @ .., last] => Err(format!(
            "{} and {last} are missing: install them from the distribution's packages (see docs/C.md)",
            tools.join(", ")
        )),
    }
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
    // The tools first: the downloads are large.
    let mut sh = shell()?;
    for port in ports()? {
        fetch(&port, &root().join("downloads"))?;
    }
    // The script makes the C library's object of the POSIX layer once its
    // cross linker exists.
    let library = posix_library()?;
    let script = util::workspace_root().join("ports").join("build.sh");
    sh.arg(&script)
        .env("VEDA_PORTS", util::workspace_root().join("ports"))
        .env("VEDA_TOOLCHAIN", root())
        .env("VEDA_POSIX_LIB", &library)
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
