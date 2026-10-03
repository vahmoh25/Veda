//! `cargo xtask` — the Vindows developer tool.
//!
//! Run `cargo xtask help` for the list of commands.

mod automate;
mod components;
mod image;
mod qemu;
mod qmp;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use components::Profile;
use util::Result;

const HELP: &str = "\
Vindows developer tool

USAGE:
    cargo xtask <COMMAND> [OPTIONS]

COMMANDS:
    build       Build every component and assemble target/vindows/vindows.img
    run         Build, then boot Vindows in QEMU
    shot        Boot headless, wait, and save a screenshot (dev aid)
    script FILE Boot headless and run an automation script (see automate.rs)
    test        Run host unit tests, then the in-system integration tests
                (--ui also runs the GUI automation scripts in tests/ui)
    clean       Remove build outputs
    doctor      Check that the required tools are installed
    help        Show this message

BUILD OPTIONS:
    --debug             Build without optimisations (slow under emulation)
    --resolution WxH    Preferred screen resolution (default 1280x800)
    --cmdline \"...\"     Extra kernel command line arguments
    --no-generate       Reuse the media in target/generated instead of regenerating it
    --skip PROGRAM      Leave a program out of the image (repeatable)

RUN OPTIONS:
    --smp N             Number of virtual CPUs (default 4)
    --memory MiB        Guest RAM in MiB (default 1024)
    --headless          No display window (serial console only)
    --no-audio          Do not attach a sound device
    --serial FILE       Write the serial console to FILE instead of the terminal
    --gdb               Wait for a debugger on localhost:1234
    --qemu-arg ARG      Pass ARG through to QEMU (repeatable)
    --fresh-home        Start with a new home directory (deletes target/vindows/home.img)

SHOT OPTIONS:
    --wait SECS         Seconds to wait before the screenshot (default 10)
    --until TEXT        Instead, wait until the serial log contains TEXT
    --out FILE          Output PNG (default target/vindows/screen.png)
";

/// Options shared by the build-related commands.
#[derive(Clone)]
struct Options {
    profile: Profile,
    resolution: String,
    cmdline: String,
    vm: qemu::VmConfig,
    wait: f64,
    until: Option<String>,
    out: Option<PathBuf>,
    /// `test`: also run the GUI scripts in `tests/ui`.
    ui: bool,
    /// Run the media generators (off: reuse what `target/generated` has).
    generate: bool,
    /// Programs left out of the image.
    skip: Vec<String>,
    /// `run`: start with a new, empty home directory.
    fresh_home: bool,
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut o = Options {
        profile: Profile::Release,
        resolution: "1280x800".into(),
        cmdline: String::new(),
        vm: qemu::VmConfig::default(),
        wait: 10.0,
        until: None,
        out: None,
        ui: false,
        generate: true,
        skip: Vec::new(),
        fresh_home: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--debug" => o.profile = Profile::Debug,
            "--release" => o.profile = Profile::Release,
            "--resolution" => o.resolution = value(arg)?,
            "--cmdline" => o.cmdline = value(arg)?,
            "--smp" => o.vm.cpus = value(arg)?.parse().map_err(|_| "--smp expects a number")?,
            "--memory" => o.vm.memory_mib = value(arg)?.parse().map_err(|_| "--memory expects MiB")?,
            "--headless" => o.vm.display = false,
            "--no-audio" => o.vm.audio = false,
            "--serial" => o.vm.serial_file = Some(PathBuf::from(value(arg)?)),
            "--gdb" => o.vm.gdb = true,
            "--qemu-arg" => o.vm.extra.push(value(arg)?),
            "--wait" => o.wait = value(arg)?.parse().map_err(|_| "--wait expects seconds")?,
            "--until" => o.until = Some(value(arg)?),
            "--out" => o.out = Some(PathBuf::from(value(arg)?)),
            "--ui" => o.ui = true,
            "--no-generate" => o.generate = false,
            "--skip" => o.skip.push(value(arg)?),
            "--fresh-home" => o.fresh_home = true,
            other => return Err(format!("unknown option '{other}' (see `cargo xtask help`)")),
        }
    }
    Ok(o)
}

/// Host tools that generate media into `target/generated` at build time,
/// with the source directories (the tool and the libraries it uses) whose
/// changes make it run again.
const GENERATORS: &[(&str, &[&str])] = &[
    ("assetgen", &["tools/assetgen", "lib/image"]),
    ("musicgen", &["tools/musicgen", "lib/audio", "lib/image", "lib/math"]),
];

/// Newest modification time of any file under `dir`.
fn newest_mtime(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                newest = Some(newest.map_or(t, |n: std::time::SystemTime| n.max(t)));
            }
        }
    }
    newest
}

/// Runs each generator whose sources changed since its last run.
fn generate_assets() -> Result {
    let out = util::generated_dir();
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    for (g, sources) in GENERATORS {
        let stamp = out.join(format!(".{g}.stamp"));
        let stamp_time = std::fs::metadata(&stamp).and_then(|m| m.modified()).ok();
        let src_time = sources.iter().filter_map(|s| newest_mtime(&util::workspace_root().join(s))).max();
        if stamp_time.is_some() && src_time.is_some() && stamp_time >= src_time {
            continue;
        }
        util::status("Generating", format!("media with {g}"));
        util::run(util::cargo().args(["run", "--quiet", "--release", "--package", g, "--"]).arg(&out))?;
        std::fs::write(&stamp, b"ok").map_err(|e| e.to_string())?;
    }
    Ok(())
}
/// The contents of the boot disk apart from its configuration file.
struct System {
    bootloader: Vec<u8>,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
}

/// Builds all components and writes the bootable disk image.
fn build(o: &Options) -> Result<PathBuf> {
    let started = std::time::Instant::now();
    let system = build_system(o)?;
    let (disk, size) = write_image(o, &system)?;
    util::status(
        "Finished",
        format!(
            "{} (kernel {}, initrd {}) in {:.1}s",
            util::human_size(size),
            util::human_size(system.kernel.len() as u64),
            util::human_size(system.initrd.len() as u64),
            started.elapsed().as_secs_f32()
        ),
    );
    Ok(disk)
}

/// Builds all components and packs the initrd.
fn build_system(o: &Options) -> Result<System> {
    let artifacts = components::build_all(o.profile, &o.skip)?;
    if o.generate {
        generate_assets()?;
    }

    util::status("Packing", "initrd");
    let mut initrd = initrd::builder::Builder::new();
    for (program, path) in &artifacts.programs {
        initrd.add(&format!("bin/{}.exe", program.binary), util::read(path)?);
    }
    initrd.add("etc/version", format!("Vindows {}\n", env!("CARGO_PKG_VERSION")).into_bytes());
    // Everything under assets/ is installed at the same relative path
    // (assets/fonts/X -> /system/fonts/X); README files are documentation.
    for assets in [util::workspace_root().join("assets"), util::generated_dir()] {
        if !assets.is_dir() {
            continue;
        }
        let mut stack = vec![assets.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().is_some_and(|n| n != "README.md") {
                    let rel = path.strip_prefix(&assets).unwrap().to_string_lossy().replace('\\', "/");
                    initrd.add(&rel, util::read(&path)?);
                }
            }
        }
    }
    let initrd = initrd.build();
    let bootloader = util::read(&artifacts.bootloader)?;
    let kernel = util::read(&artifacts.kernel)?;
    vpe::PeImage::parse(&kernel).map_err(|e| format!("kernel image is invalid: {e}"))?;
    Ok(System { bootloader, kernel, initrd })
}

/// Writes the disk image of `system` with the boot configuration of `o`
/// (resolution and kernel command line); returns its path and size.
fn write_image(o: &Options, system: &System) -> Result<(PathBuf, u64)> {
    let boot_cfg = format!(
        "# Vindows boot configuration (read by the UEFI loader)\nresolution={}\ncmdline={}\n",
        o.resolution, o.cmdline
    );
    let out = util::out_dir();
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let disk = out.join("vindows.img");
    util::status("Imaging", disk.display());
    let size = image::write_disk_image(
        &disk,
        &image::EspContents {
            bootloader: &system.bootloader,
            kernel: &system.kernel,
            initrd: &system.initrd,
            symbols: &[],
            boot_cfg: &boot_cfg,
        },
    )?;
    Ok((disk, size))
}

fn run(o: &Options) -> Result {
    let install = qemu::QemuInstall::locate()?;
    let disk = build(o)?;
    let vars = qemu::vars_file(&install)?;
    // The home directory lives on its own disk, kept across builds and runs.
    let home = util::out_dir().join("home.img");
    qemu::prepare_home_disk(&home, o.fresh_home)?;
    let mut vm = o.vm.clone();
    vm.home_disk = Some(home);
    let mut cmd = qemu::command(&install, &disk, &vars, &vm);
    util::status("Running", format!("{}", install.binary.display()));
    util::run(&mut cmd)
}

/// Boots headless and runs an automation script (see `automate.rs`).
fn script(o: &Options, script: &str) -> Result {
    script_on(o, script, None)
}

/// Runs an automation script on `system`, or on a fresh build if `None`.
/// Only the boot configuration differs between scripts, so test runs build
/// the system once and just write a new disk image for each script.
fn script_on(o: &Options, script: &str, system: Option<&System>) -> Result {
    let install = qemu::QemuInstall::locate()?;
    let mut o = o.clone();
    for extra in automate::boot_cmdline(script) {
        o.cmdline = format!("{} {extra}", o.cmdline).trim().to_string();
    }
    let o = &o;
    let disk = match system {
        Some(system) => write_image(o, system)?.0,
        None => build(o)?,
    };
    let mut vm = o.vm.clone();
    vm.audio_wav = Some(util::out_dir().join("audio.wav"));
    // Scripts that reboot the machine need QEMU to stay up across it.
    vm.allow_reboot = script.lines().any(|l| l.trim() == "reset");
    // Every scripted run starts with a new, empty home directory.
    let home = util::out_dir().join("test-home.img");
    qemu::prepare_home_disk(&home, true)?;
    vm.home_disk = Some(home);
    let log = automate::run_script(&install, &disk, vm, script)?;
    println!("--- serial log (tail) ---\n{}", automate::tail(&log, 40));
    Ok(())
}

fn shot(o: &Options) -> Result {
    let out = o.out.clone().unwrap_or_else(|| util::out_dir().join("screen.png"));
    let wait = match &o.until {
        Some(text) => format!("wait-serial \"{text}\" 120\nwait 1"),
        None => format!("wait {}", o.wait),
    };
    script(o, &format!("{wait}\nshot \"{}\"\n", out.display()))
}

/// Library crates with host unit tests (features in parentheses).
const HOST_TESTED: &[(&str, &[&str])] = &[
    ("vabi", &[]),
    ("vpe", &[]),
    ("initrd", &["std"]),
    ("vheap", &[]),
    ("vipc", &[]),
    ("vmath", &[]),
    ("vraster", &[]),
    ("vfont", &[]),
    ("vimage", &[]),
    ("vaudio", &[]),
    ("vproto", &[]),
    ("vgfx", &[]),
    ("vtext", &[]),
    ("vfiles", &["thumbnails"]),
    ("xtask", &[]),
];

fn test(o: &Options) -> Result {
    util::status("Testing", "library unit tests on the host");
    for (package, features) in HOST_TESTED {
        let mut cmd = util::cargo();
        cmd.args(["test", "--quiet", "--package", package]);
        if !features.is_empty() {
            cmd.args(["--features", &features.join(",")]);
        }
        util::run(&mut cmd)?;
    }
    // One build serves every boot below.
    let started = std::time::Instant::now();
    let system = build_system(o)?;
    util::status("Built", format!("the system in {:.1}s", started.elapsed().as_secs_f32()));
    util::status("Testing", "integration tests inside Vindows (QEMU)");
    let mut o = o.clone();
    o.cmdline = format!("{} systest", o.cmdline).trim().to_string();
    // The last test restarts the window system; give the desktop a moment
    // to come back before the screenshot.
    let checks =
        "fail-on \"systest: FAIL\"\nwait-serial \"systest: PASS\" 240\nwait 3\nshot target/vindows/test-desktop.png\n";
    script_on(&o, checks, Some(&system))?;
    if o.ui {
        let dir = util::workspace_root().join("tests").join("ui");
        let mut scripts: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "vts"))
            .collect();
        scripts.sort();
        for path in scripts {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            util::status("Testing", format!("GUI script {name}"));
            let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            let mut ui = o.clone();
            ui.cmdline.clear();
            script_on(&ui, &text, Some(&system)).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    util::status("Passed", "all tests");
    Ok(())
}

fn doctor() -> Result {
    let mut ok = true;
    let mut check = |name: &str, r: std::result::Result<String, String>| match r {
        Ok(v) => println!("  [ok]   {name}: {v}"),
        Err(e) => {
            ok = false;
            println!("  [FAIL] {name}: {e}")
        }
    };
    println!("Checking the Vindows build environment...");
    let rustc = std::process::Command::new("rustc").arg("--version").output();
    check("rustc", rustc.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).map_err(|e| e.to_string()));
    let targets = std::process::Command::new("rustup").args(["target", "list", "--installed"]).output();
    check(
        "uefi target",
        targets.map_err(|e| e.to_string()).and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            if s.contains(components::UEFI_TARGET) {
                Ok("installed".into())
            } else {
                Err(format!("run `rustup target add {}`", components::UEFI_TARGET))
            }
        }),
    );
    check("msvc linker", vbuild::find_msvc().map(|m| m.link.display().to_string()));
    match qemu::QemuInstall::locate() {
        Ok(q) => {
            check("qemu", Ok(q.binary.display().to_string()));
            check("ovmf", Ok(q.ovmf_code.display().to_string()));
        }
        Err(e) => check("qemu", Err(e)),
    }
    if ok { Ok(()) } else { Err("some checks failed".into()) }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => ("help", &[][..]),
    };
    let result = match cmd {
        "build" => parse_options(rest).and_then(|o| build(&o).map(|_| ())),
        "run" => parse_options(rest).and_then(|o| run(&o)),
        "shot" => parse_options(rest).and_then(|o| shot(&o)),
        "test" => parse_options(rest).and_then(|o| test(&o)),
        "script" => match rest.split_first() {
            Some((file, opts)) => std::fs::read_to_string(file)
                .map_err(|e| format!("reading {file}: {e}"))
                .and_then(|text| parse_options(opts).and_then(|o| script(&o, &text))),
            None => Err("usage: cargo xtask script FILE [options]".into()),
        },
        "clean" => util::run(util::cargo().arg("clean")),
        "doctor" => doctor(),
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{HELP}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\x1b[1;31merror:\x1b[0m {e}");
            ExitCode::FAILURE
        }
    }
}
