//! `cargo xtask` — the Veda developer tool.
//!
//! Run `cargo xtask help` for the list of commands.

mod agentsim;
mod airsim;
mod automate;
mod components;
mod image;
mod mic;
mod qemu;
mod qmp;
mod toolchain;
mod util;
mod vbox;
mod vboxctl;

use std::path::PathBuf;
use std::process::ExitCode;

use components::Profile;
use util::Result;

const HELP: &str = "\
Veda developer tool

USAGE:
    cargo xtask <COMMAND> [OPTIONS]

COMMANDS:
    build       Build every component and assemble target/veda/veda.img
    iso         Build every component and write target/veda/veda.iso: a live system
                that boots a PC from a USB stick (write it with Rufus, or as it is
                with any image writer); it runs from memory and leaves the PC's
                disks alone
    run         Build, then boot Veda in QEMU
    shot        Boot headless, wait, and save a screenshot (dev aid)
    script FILE Boot headless and run an automation script (see automate.rs)
    test        Run host unit tests, then the in-system integration tests
                (--ui also runs the GUI automation scripts in tests/ui
                and the agent's in tests/agent)
    toolchain   Build the C toolchain (GCC, binutils, musl) from ports/: a cross
                compiler for this machine and the native one the image installs in
                /system (needs build tools: MSYS2 on Windows; see docs/C.md; --jobs N)
    clean       Remove build outputs
    doctor      Check that the required tools are installed
    help        Show this message

BUILD OPTIONS:
    --debug             Build without optimisations (slow under emulation)
    --resolution WxH    Preferred screen resolution (default 1280x800; iso: 1920x1080, or
                        the largest the screen offers below it)
    --cmdline \"...\"     Extra kernel command line arguments
    --no-generate       Reuse the media in target/generated instead of regenerating it
    --skip PROGRAM      Leave a program out of the image (repeatable)

RUN OPTIONS:
    --vm HYPERVISOR     qemu (default) or virtualbox (run, shot, script, test)
    --scale FACTOR      VirtualBox: how much the window enlarges the screen, such as 2 or
                        250% (default on Windows: the whole part of the display scaling, as
                        QEMU's window has it, lowered if the window would not fit; else 1)
    --smp N             Number of virtual CPUs (default 4)
    --memory MiB        Guest RAM in MiB (default 1024)
    --headless          No display window (serial console only)
    --gpu, --no-gpu     Give QEMU's machine a 3D GPU (virtio-gpu with virgl, rendering on the
                        host's GPU) or not (default: if this QEMU has one)
    --no-audio          Do not attach a sound device
    --sound CARD        The sound card: virtio (QEMU's default), ac97 (VirtualBox's default) or
                        hda (Intel HD Audio, as most PCs have)
    --serial FILE       Write the serial console to FILE instead of the terminal
    --gdb               Wait for a debugger on localhost:1234
    --qemu-arg ARG      Pass ARG through to QEMU (repeatable)
    --fresh-home        Start with a new home directory (deletes target/veda/home.img)
    --net MODE          Network: ethernet (default, the hypervisor's NAT), wifi (QEMU: the
                        virtual Wi-Fi radio and the airsim access points), both (QEMU),
                        bridged (VirtualBox: the host's real network), or none
    --nic MODEL         Model of the wired card (QEMU: virtio-net-pci (default), e1000, e1000e;
                        VirtualBox: e1000 (82540EM, default), 82545EM, virtio)
    --bridge ADAPTER    VirtualBox host adapter for --net bridged (default: the first connected)
    --disk-bus BUS      How QEMU attaches the disks: virtio (default) or ahci (SATA)
    --input DEVICES     Keyboard and pointer: standard (default; QEMU: PS/2 keyboard and virtio
                        tablet, VirtualBox: PS/2) or usb (on the xHCI controller; QEMU: a
                        keyboard and a tablet behind a hub, and a mouse)
    --live              Boot the live system (iso) from a USB stick, as a PC would (QEMU)

SHOT OPTIONS:
    --wait SECS         Seconds to wait before the screenshot (default 10)
    --until TEXT        Instead, wait until the serial log contains TEXT
    --out FILE          Output PNG (default target/veda/screen.png)

ISO OPTIONS:
    --out FILE          The ISO image (default target/veda/veda.iso)
";

/// Options shared by the build-related commands.
#[derive(Clone)]
struct Options {
    profile: Profile,
    resolution: String,
    /// `--resolution` was given (otherwise the live system has its own).
    resolution_set: bool,
    cmdline: String,
    vm: qemu::VmConfig,
    wait: f64,
    until: Option<String>,
    out: Option<PathBuf>,
    /// `test`: also run the GUI scripts in `tests/ui` and `tests/agent`.
    ui: bool,
    /// Run the media generators (off: reuse what `target/generated` has).
    generate: bool,
    /// Programs left out of the image.
    skip: Vec<String>,
    /// `run`: start with a new, empty home directory.
    fresh_home: bool,
    /// The hypervisor for `run`, `shot`, `script` and `test`.
    hypervisor: Hypervisor,
    /// VirtualBox: the window's scale (`None`: from the host's display).
    scale: Option<f64>,
    /// `run`, `shot`, `script`: boot the live system from a USB stick.
    live: bool,
}

/// Which hypervisor runs Veda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hypervisor {
    Qemu,
    VirtualBox,
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut o = Options {
        profile: Profile::Release,
        resolution: "1280x800".into(),
        resolution_set: false,
        cmdline: String::new(),
        vm: qemu::VmConfig::default(),
        wait: 10.0,
        until: None,
        out: None,
        ui: false,
        generate: true,
        skip: Vec::new(),
        fresh_home: false,
        hypervisor: Hypervisor::Qemu,
        scale: None,
        live: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--debug" => o.profile = Profile::Debug,
            "--release" => o.profile = Profile::Release,
            "--resolution" => {
                let v = value(arg)?;
                let (w, h) =
                    resolution_size(&v).ok_or("--resolution expects WxH, at least 640x480 (such as 1920x1080)")?;
                o.resolution = format!("{w}x{h}");
                o.resolution_set = true;
            }
            "--live" => o.live = true,
            "--cmdline" => o.cmdline = value(arg)?,
            "--smp" => o.vm.cpus = value(arg)?.parse().map_err(|_| "--smp expects a number")?,
            "--memory" => o.vm.memory_mib = value(arg)?.parse().map_err(|_| "--memory expects MiB")?,
            "--headless" => o.vm.display = false,
            "--gpu" => o.vm.gpu = Some(true),
            "--no-gpu" => o.vm.gpu = Some(false),
            "--no-audio" => o.vm.audio = false,
            "--sound" => {
                let card = value(arg)?;
                if !matches!(card.as_str(), "virtio" | "ac97" | "hda") {
                    return Err(format!("--sound: unknown card '{card}' (virtio, ac97 or hda)"));
                }
                o.vm.sound = card;
            }
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
            "--vm" => {
                o.hypervisor = match value(arg)?.to_ascii_lowercase().as_str() {
                    "qemu" => Hypervisor::Qemu,
                    "virtualbox" | "vbox" => Hypervisor::VirtualBox,
                    v => return Err(format!("unknown hypervisor '{v}' (qemu, virtualbox)")),
                }
            }
            "--scale" => o.scale = parse_scale(&value(arg)?)?,
            "--bridge" => o.vm.bridge_adapter = Some(value(arg)?),
            "--net" => {
                let v = value(arg)?;
                o.vm.net = qemu::NetMode::parse(&v).ok_or(format!("unknown network mode '{v}'"))?;
            }
            "--nic" => o.vm.nic_model = Some(value(arg)?),
            "--disk-bus" => {
                let v = value(arg)?;
                o.vm.disk_bus = qemu::DiskBus::parse(&v).ok_or(format!("unknown disk bus '{v}' (virtio, ahci)"))?;
            }
            "--input" => {
                let v = value(arg)?;
                o.vm.input =
                    qemu::InputDevices::parse(&v).ok_or(format!("unknown input devices '{v}' (standard, usb)"))?;
            }
            other => return Err(format!("unknown option '{other}' (see `cargo xtask help`)")),
        }
    }
    Ok(o)
}

/// The width and height in a resolution such as "1920x1080".
fn resolution_size(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w >= 640 && h >= 480).then_some((w, h))
}

/// A `--scale` value: `auto`, a factor (`2`, `2.5`) or a percentage (`250%`).
fn parse_scale(v: &str) -> Result<Option<f64>> {
    if v.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let (number, divisor) = match v.strip_suffix('%') {
        Some(percent) => (percent, 100.0),
        None => (v, 1.0),
    };
    match number.trim().parse::<f64>().map(|n| n / divisor) {
        Ok(scale) if (0.5..=4.0).contains(&scale) => Ok(Some(scale)),
        _ => Err(format!("--scale expects a factor from 0.5 to 4, such as 2 or 250%, or auto (not '{v}')")),
    }
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
    // C programs that test the POSIX layer (with a cross compiler only).
    for (name, exe) in toolchain::c_tests()? {
        initrd.add(&format!("tests/c/{name}"), exe);
    }
    // The renderer: OpenGL ES on Mesa's Gallium drivers (once Mesa is
    // configured, by `cargo xtask toolchain`).
    if let Some(exe) = toolchain::renderer()? {
        initrd.add("bin/renderer.exe", exe);
    }
    // The C toolchain (`cargo xtask toolchain`): gcc, as, ld, the C library
    // and its headers, as /system has them.
    toolchain::relink_native()?;
    let native = toolchain::native_files()?;
    if native.is_empty() {
        util::status("Note", "no C toolchain in the image (build it with `cargo xtask toolchain`)");
    }
    for (path, data) in native {
        initrd.add(&path, data);
    }
    initrd.add("etc/version", format!("Veda {}\n", env!("CARGO_PKG_VERSION")).into_bytes());
    // The licence, which About Veda refers to.
    initrd.add("LICENSE.txt", util::read(&util::workspace_root().join("LICENSE"))?);
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

/// The boot configuration of `o` (screen resolution and kernel command
/// line), for the disk image or for the live system.
fn boot_config(o: &Options, live: bool) -> String {
    let mut cmdline = o.cmdline.clone();
    let resolution = if live {
        // The live system runs from memory and leaves the PC's disks alone.
        // A PC's clock keeps local time under Windows, so it shows no time
        // zone; and it takes the screen's largest mode up to 1080p.
        cmdline = format!("live {cmdline}").trim().to_string();
        if o.resolution_set { o.resolution.as_str() } else { "1920x1080" }
    } else {
        // The machine's clock keeps UTC; local time is the host's, unless the
        // command line says otherwise.
        if !cmdline.split_whitespace().any(|a| a.starts_with("tz="))
            && let Some(tz) = util::host_utc_offset()
        {
            cmdline = format!("{cmdline} tz={tz}").trim().to_string();
        }
        o.resolution.as_str()
    };
    format!("# Veda boot configuration (read by the UEFI loader)\nresolution={resolution}\ncmdline={cmdline}\n")
}

/// The boot partition's files for `system` with `boot_cfg`.
fn esp_contents<'a>(system: &'a System, boot_cfg: &'a str) -> image::EspContents<'a> {
    image::EspContents {
        bootloader: &system.bootloader,
        kernel: &system.kernel,
        initrd: &system.initrd,
        symbols: &[],
        boot_cfg,
    }
}

/// Writes the disk image of `system` with the boot configuration of `o`
/// (resolution and kernel command line); returns its path and size.
fn write_image(o: &Options, system: &System) -> Result<(PathBuf, u64)> {
    let boot_cfg = boot_config(o, false);
    let out = util::out_dir();
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let disk = out.join("veda.img");
    util::status("Imaging", disk.display());
    let size = image::write_disk_image(&disk, &esp_contents(system, &boot_cfg))?;
    Ok((disk, size))
}

/// Where `run`, `shot` and `script` keep the live system's ISO image.
fn live_iso() -> PathBuf {
    util::out_dir().join("veda.iso")
}

/// Writes the live system's ISO image of `system` to `path` (see
/// `image::iso9660`); returns its size.
fn write_iso(o: &Options, system: &System, path: &std::path::Path) -> Result<u64> {
    let boot_cfg = boot_config(o, true);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    util::status("Imaging", path.display());
    image::write_iso_image(path, &esp_contents(system, &boot_cfg))
}

/// Builds all components and writes the live system's ISO image to `path`.
fn build_iso(o: &Options, path: &std::path::Path) -> Result {
    let started = std::time::Instant::now();
    let system = build_system(o)?;
    let size = write_iso(o, &system, path)?;
    util::status("Finished", format!("{} in {:.1}s", util::human_size(size), started.elapsed().as_secs_f32()));
    Ok(())
}

/// `iso`: the live system, for a USB stick.
fn iso(o: &Options) -> Result {
    let path = o.out.clone().unwrap_or_else(live_iso);
    build_iso(o, &path)?;
    println!(
        "\nWrite it to a USB stick with Rufus (partition scheme GPT, target system UEFI,\n\
         ISO mode), or as it is with any image writer. Start the PC from the stick\n\
         with UEFI, with Secure Boot off: Veda runs from memory and leaves the PC's\n\
         disks alone. `cargo xtask run --live` tries it in QEMU first."
    );
    Ok(())
}

fn run(o: &Options) -> Result {
    if o.hypervisor == Hypervisor::VirtualBox {
        if o.live {
            return Err("--live boots the live system in QEMU (--vm qemu)".into());
        }
        return run_vbox(o);
    }
    if o.vm.net == qemu::NetMode::Bridged {
        return Err("bridged networking needs VirtualBox (--vm virtualbox)".into());
    }
    if o.scale.is_some() {
        return Err("--scale is for VirtualBox (--vm virtualbox); QEMU's window zooms from its View menu".into());
    }
    let install = qemu::QemuInstall::locate()?;
    let mut vm = o.vm.clone();
    let disk = if o.live {
        // A stick with the ISO on it, and no home disk.
        let iso = live_iso();
        build_iso(o, &iso)?;
        vm.usb_stick = true;
        iso
    } else {
        let disk = build(o)?;
        // The home directory lives on its own disk, kept across builds and runs.
        let home = util::out_dir().join("home.img");
        qemu::prepare_home_disk(&home, o.fresh_home)?;
        vm.home_disk = Some(home);
        disk
    };
    let vars = qemu::vars_file(&install)?;
    // The simulated Wi-Fi environment runs while QEMU does.
    let _sim = if vm.net.wireless() {
        let sim = airsim::AirSim::start(&airsim::build()?, &util::out_dir().join("airsim.log"))?;
        vm.wifi = Some(sim.ports);
        util::status(
            "Wi-Fi",
            format!(
                "simulated networks (password \"{}\"); log {}; control: telnet 127.0.0.1 {}",
                airsim::PASSWORD,
                sim.log.display(),
                sim.control
            ),
        );
        Some(sim)
    } else {
        None
    };
    let mut cmd = qemu::command(&install, &disk, &vars, &vm);
    util::status("Running", format!("{}", install.binary.display()));
    util::run(&mut cmd)
}

/// `run` with VirtualBox: the same build, the same disks, a VirtualBox
/// machine configured from the options.
fn run_vbox(o: &Options) -> Result {
    let vbox = vbox::VBox::locate()?;
    util::status("Using", format!("VirtualBox {}", vbox.version()?));
    let disk = build(o)?;
    let home = util::out_dir().join("home.img");
    qemu::prepare_home_disk(&home, o.fresh_home)?;
    let serial = o.vm.serial_file.clone().unwrap_or_else(|| util::out_dir().join("serial-vbox.log"));
    let _ = std::fs::remove_file(&serial);
    // The window enlarges the screen as QEMU's does on a high-DPI display.
    let (width, height) = resolution_size(&o.resolution).unwrap_or((1280, 800));
    let scale = o.vm.display.then(|| o.scale.unwrap_or_else(|| vbox::auto_scale(width, height)));
    vbox.configure(&o.vm, &disk, Some(&home), &serial, &o.resolution, scale)?;
    if let Some(scale) = scale {
        util::status("Display", format!("{}, shown at {:.0}% (--resolution, --scale)", o.resolution, scale * 100.0));
    }
    vbox.start(!o.vm.display)?;
    util::status("Running", format!("VirtualBox machine \"{}\" (Ctrl+C powers it off)", vbox.name()));
    vbox::follow(&vbox, &serial, o.vm.serial_file.is_none())
}

/// Boots headless and runs an automation script (see `automate.rs`).
fn script(o: &Options, script: &str) -> Result {
    script_on(o, script, None)
}

/// Runs an automation script on `system`, or on a fresh build if `None`.
/// Only the boot configuration differs between scripts, so test runs build
/// the system once and just write a new disk image for each script.
fn script_on(o: &Options, script: &str, system: Option<&System>) -> Result {
    let install = match o.hypervisor {
        Hypervisor::Qemu => Some(qemu::QemuInstall::locate()?),
        Hypervisor::VirtualBox => None,
    };
    let mut o = o.clone();
    // The test microphone's address goes on the command line.
    let mic = if automate::needs_mic(script) { Some(mic::MicServer::start()?) } else { None };
    if let Some(m) = &mic {
        o.cmdline = format!("{} {}", o.cmdline, m.boot_arg()).trim().to_string();
    }
    let agent = if automate::needs_agentsim(script) { Some(agentsim::AgentSim::start()?) } else { None };
    if let Some(a) = &agent {
        // This computer is the gateway of the guest's network: QEMU's NAT,
        // or the Wi-Fi radio's own when that is the only network.
        let wifi_only = automate::net_mode(script)? == Some(qemu::NetMode::Wifi);
        let host = if wifi_only { "10.0.3.2" } else { "10.0.2.2" };
        o.cmdline = format!("{} {}", o.cmdline, a.boot_arg(host, !automate::agent_asleep(script))).trim().to_string();
    }
    for extra in automate::boot_cmdline(script) {
        o.cmdline = format!("{} {extra}", o.cmdline).trim().to_string();
    }
    let o = &o;
    let live = o.live || automate::live(script);
    if live && o.hypervisor == Hypervisor::VirtualBox {
        return Err("the live system boots in QEMU (--vm qemu)".into());
    }
    let disk = if live {
        let iso = live_iso();
        match system {
            Some(system) => {
                write_iso(o, system, &iso)?;
            }
            None => build_iso(o, &iso)?,
        }
        iso
    } else {
        match system {
            Some(system) => write_image(o, system)?.0,
            None => build(o)?,
        }
    };
    let mut vm = o.vm.clone();
    vm.audio_wav = (!automate::host_audio(script)).then(|| util::out_dir().join("audio.wav"));
    // Scripts that reboot the machine need QEMU to stay up across it.
    vm.allow_reboot = script.lines().any(|l| l.trim() == "reset");
    if let Some(net) = automate::net_mode(script)? {
        vm.net = net;
    }
    match o.hypervisor {
        Hypervisor::Qemu if vm.net == qemu::NetMode::Bridged || automate::needs_virtualbox(script) => {
            return Err("this script needs VirtualBox (--vm virtualbox)".into());
        }
        Hypervisor::VirtualBox if let Some(why) = automate::needs_qemu(script) => {
            return Err(format!("this script needs QEMU ({why})"));
        }
        _ => {}
    }
    if let Some(card) = automate::sound_card(script) {
        vm.sound = card;
    }
    if let Some(model) = automate::nic_model(script) {
        vm.nic_model = Some(model);
    }
    if let Some(input) = automate::input_devices(script)? {
        vm.input = input;
    }
    if let Some(gpu) = automate::gpu(script)? {
        vm.gpu = Some(gpu);
    }
    if live {
        // A stick with the ISO on it, and no home disk.
        vm.usb_stick = true;
    } else {
        // Every scripted run starts with a new, empty home directory.
        let home = util::out_dir().join("test-home.img");
        qemu::prepare_home_disk(&home, true)?;
        vm.home_disk = Some(home);
    }
    let hv = match &install {
        Some(install) => automate::Hypervisor::Qemu(install),
        None => automate::Hypervisor::VirtualBox { resolution: &o.resolution },
    };
    let log = automate::run_script(&hv, &disk, vm, script, mic, agent)?;
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
    ("velf", &[]),
    ("vposix", &[]),
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
    ("ventropy", &[]),
    ("vnetstack", &[]),
    ("vnet", &[]),
    ("vtls", &[]),
    ("vjson", &[]),
    ("vweb", &[]),
    ("vagent", &[]),
    ("vwlan", &[]),
    ("vradiolink", &[]),
    ("vusb", &[]),
    ("vhda", &[]),
    ("vacpi", &[]),
    ("vcs35l41", &[]),
    ("vgpio", &[]),
    ("vigpu", &[]),
    ("vboardsim", &[]),
    ("vsplash", &[]),
    ("vglsl", &[]),
    ("vgl", &[]),
    ("prism-scene", &[]),
    ("airsim", &[]),
    ("xtask", &[]),
];

fn test(o: &Options) -> Result {
    util::status("Testing", "library unit tests on the host");
    // Tests of OpenGL ES on the host's GPU drive the virglrenderer QEMU
    // runs, where it has one: on Windows the one QEMU's builds ship, which
    // they load from QEMU's directory; on Linux the system's, which QEMU's
    // 3D GPU is built on.
    let qemu = qemu::QemuInstall::locate().ok();
    let virgl_dir = qemu.as_ref().and_then(|q| q.virglrenderer_dir());
    let virgl = virgl_dir.is_some() || (cfg!(target_os = "linux") && qemu.as_ref().is_some_and(|q| q.has_gl_gpu()));
    for (package, features) in HOST_TESTED {
        let mut cmd = util::cargo();
        cmd.args(["test", "--quiet", "--package", package]);
        if !features.is_empty() {
            cmd.args(["--features", &features.join(",")]);
        }
        if let Some(dir) = &virgl_dir {
            cmd.env("VEDA_QEMU_DIR", dir);
        }
        util::run(&mut cmd)?;
    }
    // And through Veda's renderer, on softpipe, where it has been built.
    if let Some(dll) = toolchain::vgallium()? {
        // As OpenGL hosts keep depth, then as iris does (VR_DEPTH_LOW).
        for (depth, how) in [("0", ""), ("1", ", depth kept as on iris")] {
            util::status("Testing", format!("OpenGL ES through Veda's renderer (Mesa's softpipe{how})"));
            let mut cmd = util::cargo();
            cmd.args(["test", "--quiet", "--package", "vgl"])
                .env("VGL_TEST_BACKEND", "gallium")
                .env("VGL_GALLIUM_DLL", &dll)
                .env("VR_DEPTH_LOW", depth);
            util::run(&mut cmd)?;
        }
    }
    if virgl {
        // On Windows on ANGLE, as headless QEMU renders, and on the host's
        // desktop OpenGL, as QEMU's window does. On Linux QEMU renders on
        // desktop OpenGL either way, and on OpenGL ES with `gl=es`.
        let hosts: &[(&str, &str)] = if cfg!(windows) {
            &[("angle", "ANGLE"), ("desktop", "desktop OpenGL")]
        } else {
            &[("desktop", "desktop OpenGL"), ("gles", "OpenGL ES")]
        };
        for (host, what) in hosts {
            util::status("Testing", format!("OpenGL ES on the host's GPU (QEMU's virglrenderer on {what})"));
            let mut cmd = util::cargo();
            cmd.args(["test", "--quiet", "--package", "vgl"])
                .env("VGL_TEST_BACKEND", "virgl")
                .env("VGL_TEST_HOST", host);
            if let Some(dir) = &virgl_dir {
                cmd.env("VEDA_QEMU_DIR", dir);
            }
            util::run(&mut cmd)?;
        }
    }
    // One build serves every boot below.
    let started = std::time::Instant::now();
    let system = build_system(o)?;
    util::status("Built", format!("the system in {:.1}s", started.elapsed().as_secs_f32()));
    let hv_name = if o.hypervisor == Hypervisor::VirtualBox { "VirtualBox" } else { "QEMU" };
    util::status("Testing", format!("integration tests inside Veda ({hv_name})"));
    let mut o = o.clone();
    o.cmdline = format!("{} systest", o.cmdline).trim().to_string();
    // The last test restarts the window system; give the desktop a moment
    // to come back before the screenshot.
    let checks =
        "fail-on \"systest: FAIL\"\nwait-serial \"systest: PASS\" 240\nwait 3\nshot target/veda/test-desktop.png\n";
    script_on(&o, checks, Some(&system))?;
    if o.ui {
        // The GUI scripts, then the agent's (with the stand-in for Deepgram;
        // tests/real, which talks to the real one, is left out).
        let mut scripts: Vec<PathBuf> = Vec::new();
        for group in ["ui", "agent"] {
            let dir = util::workspace_root().join("tests").join(group);
            let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(|e| format!("reading {}: {e}", dir.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "vts"))
                .collect();
            found.sort();
            scripts.extend(found);
        }
        for path in scripts {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            if o.hypervisor == Hypervisor::VirtualBox
                && let Some(why) = automate::needs_qemu(&text)
            {
                util::status("Skipping", format!("GUI script {name} (needs QEMU: {why})"));
                continue;
            }
            if let Some(what) = automate::needs_toolchain(&text)
                && !toolchain::built(what)
            {
                util::status("Skipping", format!("GUI script {name} (needs the {what}: `cargo xtask toolchain`)"));
                continue;
            }
            util::status("Testing", format!("GUI script {name}"));
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
    println!("Checking the Veda build environment...");
    let rustc = std::process::Command::new("rustc").arg("--version").output();
    check("rustc", rustc.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).map_err(|e| e.to_string()));
    let targets = std::process::Command::new("rustup").args(["target", "list", "--installed"]).output();
    let targets = targets.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).map_err(|e| e.to_string());
    for (name, target) in [("uefi target", components::UEFI_TARGET), ("user target", components::USER_TARGET)] {
        check(
            name,
            targets.clone().and_then(|s| {
                if s.contains(target) {
                    Ok("installed".into())
                } else {
                    Err(format!("run `rustup target add {target}`"))
                }
            }),
        );
    }
    if cfg!(windows) {
        check("msvc linker", vbuild::find_msvc().map(|m| m.link.display().to_string()));
    } else {
        check("linker", rust_lld().map(|p| p.display().to_string()));
    }
    match qemu::QemuInstall::locate() {
        Ok(q) => {
            check("qemu", Ok(q.binary.display().to_string()));
            check("ovmf", Ok(q.ovmf_code.display().to_string()));
        }
        Err(e) => check("qemu", Err(e)),
    }
    // QEMU emulates the processor where it cannot use KVM, which works but
    // is several times slower.
    if cfg!(target_os = "linux") {
        match std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm") {
            Ok(_) => println!("  [ok]   kvm: /dev/kvm"),
            Err(e) => println!(
                "  [--]   kvm: /dev/kvm: {e} (QEMU emulates the processor instead, several times slower; \
                 members of the kvm group can use it)"
            ),
        }
    }
    match toolchain::status() {
        Ok(v) => println!("  [ok]   c toolchain: {v}"),
        Err(e) => println!("  [--]   c toolchain: {e}"),
    }
    if ok { Ok(()) } else { Err("some checks failed".into()) }
}

/// The linker of user-space programs off Windows (`components::USER_LINKER`):
/// LLVM's, which comes with Rust's toolchain, beside the host's libraries.
fn rust_lld() -> Result<PathBuf> {
    let out = std::process::Command::new("rustc")
        .args(["--print", "target-libdir"])
        .output()
        .map_err(|e| format!("running rustc: {e}"))?;
    let libdir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let lld = libdir.parent().unwrap_or(&libdir).join("bin").join(components::USER_LINKER);
    if lld.is_file() {
        Ok(lld)
    } else {
        Err(format!("{} not found (it comes with Rust's toolchains from rustup)", lld.display()))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => ("help", &[][..]),
    };
    let result = match cmd {
        "build" => parse_options(rest).and_then(|o| build(&o).map(|_| ())),
        "iso" => parse_options(rest).and_then(|o| iso(&o)),
        "run" => parse_options(rest).and_then(|o| run(&o)),
        "shot" => parse_options(rest).and_then(|o| shot(&o)),
        "test" => parse_options(rest).and_then(|o| test(&o)),
        "script" => match rest.split_first() {
            Some((file, opts)) => std::fs::read_to_string(file)
                .map_err(|e| format!("reading {file}: {e}"))
                .and_then(|text| parse_options(opts).and_then(|o| script(&o, &text))),
            None => Err("usage: cargo xtask script FILE [options]".into()),
        },
        "toolchain" => toolchain::command(rest),
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
