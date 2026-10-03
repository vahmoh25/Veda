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
    clean       Remove build outputs
    doctor      Check that the required tools are installed
    help        Show this message

BUILD OPTIONS:
    --debug             Build without optimisations (slow under emulation)
    --resolution WxH    Preferred screen resolution (default 1280x800)
    --cmdline \"...\"     Extra kernel command line arguments

RUN OPTIONS:
    --smp N             Number of virtual CPUs (default 4)
    --memory MiB        Guest RAM in MiB (default 1024)
    --headless          No display window (serial console only)
    --no-audio          Do not attach a sound device
    --serial FILE       Write the serial console to FILE instead of the terminal
    --gdb               Wait for a debugger on localhost:1234
    --qemu-arg ARG      Pass ARG through to QEMU (repeatable)

SHOT OPTIONS:
    --wait SECS         Seconds to wait before the screenshot (default 10)
    --until TEXT        Instead, wait until the serial log contains TEXT
    --out FILE          Output PNG (default target/vindows/screen.png)
";

/// Options shared by the build-related commands.
struct Options {
    profile: Profile,
    resolution: String,
    cmdline: String,
    vm: qemu::VmConfig,
    wait: f64,
    until: Option<String>,
    out: Option<PathBuf>,
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
            other => return Err(format!("unknown option '{other}' (see `cargo xtask help`)")),
        }
    }
    Ok(o)
}

/// Builds all components and writes the bootable disk image.
fn build(o: &Options) -> Result<PathBuf> {
    let started = std::time::Instant::now();
    let artifacts = components::build_all(o.profile)?;

    util::status("Packing", "initrd");
    let mut initrd = initrd::builder::Builder::new();
    for (program, path) in &artifacts.programs {
        initrd.add(&format!("bin/{}.exe", program.binary), util::read(path)?);
    }
    initrd.add("etc/version", format!("Vindows {}\n", env!("CARGO_PKG_VERSION")).into_bytes());
    let initrd = initrd.build();

    let boot_cfg = format!(
        "# Vindows boot configuration (read by the UEFI loader)\nresolution={}\ncmdline={}\n",
        o.resolution, o.cmdline
    );
    let bootloader = util::read(&artifacts.bootloader)?;
    let kernel = util::read(&artifacts.kernel)?;
    vpe::PeImage::parse(&kernel).map_err(|e| format!("kernel image is invalid: {e}"))?;

    let out = util::out_dir();
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let disk = out.join("vindows.img");
    util::status("Imaging", disk.display());
    let size = image::write_disk_image(
        &disk,
        &image::EspContents { bootloader: &bootloader, kernel: &kernel, initrd: &initrd, symbols: &[], boot_cfg: &boot_cfg },
    )?;
    util::status(
        "Finished",
        format!(
            "{} (kernel {}, initrd {}) in {:.1}s",
            util::human_size(size),
            util::human_size(kernel.len() as u64),
            util::human_size(initrd.len() as u64),
            started.elapsed().as_secs_f32()
        ),
    );
    Ok(disk)
}

fn run(o: &Options) -> Result {
    let install = qemu::QemuInstall::locate()?;
    let disk = build(o)?;
    let vars = qemu::vars_file(&install)?;
    let mut cmd = qemu::command(&install, &disk, &vars, &o.vm);
    util::status("Running", format!("{}", install.binary.display()));
    util::run(&mut cmd)
}

/// Boots headless and runs an automation script (see `automate.rs`).
fn script(o: &Options, script: &str) -> Result {
    let install = qemu::QemuInstall::locate()?;
    let disk = build(o)?;
    let mut vm = o.vm.clone();
    vm.audio_wav = Some(util::out_dir().join("audio.wav"));
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
