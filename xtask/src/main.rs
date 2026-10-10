//! `cargo xtask` — the Veda developer tool.
//!
//! Run `cargo xtask help` for the list of commands.

mod acpitest;
mod agentsim;
mod airsim;
mod automate;
mod components;
mod image;
mod linux;
mod mic;
mod qemu;
mod qmp;
mod toolchain;
mod util;

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
                /system (needs build tools; see docs/C.md; --jobs N)
    linux       Build the Linux kernel of the driver VM from ports/linux (see
                docs/DRIVERVM.md; --jobs N); later builds put it in the image, with the
                guest's programs
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
    --smp N             Number of virtual CPUs (default 4)
    --memory MiB        Guest RAM in MiB (default 1024)
    --headless          No display window (serial console only)
    --no-iommu          Leave the IOMMU out of QEMU's machine (it has one, intel-iommu
                        remapping interrupts, as the driver VM's devices need)
    --gpu, --no-gpu     Give QEMU's machine a 3D GPU (virtio-gpu with virgl, rendering on the
                        host's GPU) or not (default: if this QEMU has one)
    --no-audio          Do not attach a sound device
    --sound CARD        The sound card: virtio (default) or hda (Intel HD Audio, as most PCs
                        have)
    --serial FILE       Write the serial console to FILE instead of the terminal
    --gdb               Wait for a debugger on localhost:1234
    --qemu-arg ARG      Pass ARG through to QEMU (repeatable)
    --fresh-home        Start with a new home directory (deletes target/veda/home.img)
    --net MODE          Network: ethernet (default, QEMU's NAT), wifi (the virtual Wi-Fi radio
                        and the airsim access points), both, or none
    --nic MODEL         Model of the wired card: virtio-net-pci (default), e1000e, igb
    --disk-bus BUS      How QEMU attaches the disks: virtio (default), ahci (SATA) or nvme
    --input DEVICES     Keyboard and pointer: standard (default: PS/2 keyboard and virtio
                        tablet) or usb (on the xHCI controller: a keyboard and a tablet behind
                        a hub, and a mouse)
    --live              Boot the live system (iso) from a USB stick, as a PC would

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
    /// `run`, `shot`, `script`: boot the live system from a USB stick.
    live: bool,
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
            "--no-iommu" => o.vm.iommu = false,
            "--gpu" => o.vm.gpu = Some(true),
            "--no-gpu" => o.vm.gpu = Some(false),
            "--no-audio" => o.vm.audio = false,
            "--sound" => {
                let card = value(arg)?;
                if !matches!(card.as_str(), "virtio" | "hda") {
                    return Err(format!("--sound: unknown card '{card}' (virtio or hda)"));
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
            "--net" => {
                let v = value(arg)?;
                o.vm.net = qemu::NetMode::parse(&v).ok_or(format!("unknown network mode '{v}'"))?;
            }
            "--nic" => o.vm.nic_model = Some(value(arg)?),
            "--disk-bus" => {
                let v = value(arg)?;
                o.vm.disk_bus =
                    qemu::DiskBus::parse(&v).ok_or(format!("unknown disk bus '{v}' (virtio, ahci, nvme)"))?;
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
    // The driver VM's Linux (`cargo xtask linux`, or the first time an
    // image needs it) and its programs.
    linux::build_once()?;
    match linux::guest()? {
        Some((kernel, initramfs)) => {
            initrd.add("linux/bzImage", kernel);
            initrd.add("linux/initramfs.cpio", initramfs);
        }
        None => util::status(
            "Note",
            "no driver VM in the image: nothing but the disks and sound is driven \
             (`cargo xtask linux` builds its Linux)",
        ),
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
    if automate::silent_audio(script) {
        vm.audio_wav = None;
        vm.audio_silent = true;
    }
    // Scripts that reboot the machine need QEMU to stay up across it.
    vm.allow_reboot = script.lines().any(|l| l.trim() == "reset");
    if let Some(net) = automate::net_mode(script)? {
        vm.net = net;
    }
    if let Some(card) = automate::sound_card(script) {
        vm.sound = card;
    }
    if let Some(model) = automate::nic_model(script) {
        vm.nic_model = Some(model);
    }
    if let Some(bus) = automate::disk_bus(script)? {
        vm.disk_bus = bus;
    }
    vm.acpi_tables = automate::acpi_tables(script)?;
    if let Some(input) = automate::input_devices(script)? {
        vm.input = input;
    }
    if let Some(gpu) = automate::gpu(script)? {
        vm.gpu = Some(gpu);
    }
    if automate::usb_net(script) {
        vm.usb_net = true;
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
    let log = automate::run_script(&install, &disk, vm, script, mic, agent)?;
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
    ("vhda", &[]),
    ("vacpi", &[]),
    ("vcs35l41", &[]),
    ("vgpio", &[]),
    ("vboardsim", &[]),
    ("vsplash", &[]),
    ("vglsl", &[]),
    ("vgl", &[]),
    ("prism-scene", &[]),
    ("airsim", &[]),
    ("xtask", &[]),
];

fn test(o: &Options) -> Result {
    // The GUI scripts' pointers and networks are the driver VM's.
    if o.ui
        && let Err(why) = linux::runnable()
    {
        return Err(format!("the GUI scripts need the driver VM: {why}"));
    }
    util::status("Testing", "library unit tests on the host");
    // Tests of OpenGL ES on the host's GPU drive the system's virglrenderer,
    // which QEMU's 3D GPU is built on, where QEMU has one.
    let virgl = qemu::QemuInstall::locate().is_ok_and(|q| q.has_gl_gpu());
    // On QEMU's GPU, unless `VGL_TEST_RENDERNODE` names another.
    let node = std::env::var("VGL_TEST_RENDERNODE").ok().filter(|n| !n.is_empty()).or_else(qemu::render_node);
    for (package, features) in HOST_TESTED {
        let mut cmd = util::cargo();
        cmd.args(["test", "--quiet", "--package", package]);
        if !features.is_empty() {
            cmd.args(["--features", &features.join(",")]);
        }
        util::run(&mut cmd)?;
    }
    // The driver VM's programs' logic (touchpads), on Linux as they run.
    util::run(util::cargo().args(["test", "--quiet", "--package", "guest-input", "--target", linux::GUEST_TARGET]))?;
    // And through Veda's renderer, where it has been built: on softpipe,
    // and on virgl over virglrenderer's test server, as the driver VM's
    // renderer renders under QEMU.
    if let Some(dll) = linux::vgallium()? {
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
        if let Some(server) = util::find_on_path("virgl_test_server") {
            util::status("Testing", "OpenGL ES through Veda's renderer (Mesa's virgl, on virglrenderer)");
            let socket = util::out_dir().join("vtest.sock");
            let _ = std::fs::remove_file(&socket);
            std::fs::create_dir_all(util::out_dir()).map_err(|e| e.to_string())?;
            let mut run = std::process::Command::new(server);
            run.args(["--multi-clients", "--socket-path"]).arg(&socket);
            if let Some(node) = &node {
                run.arg("--rendernode").arg(node);
            }
            let mut server = run
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("starting virgl_test_server: {e}"))?;
            let started = std::time::Instant::now();
            while !socket.exists() && started.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let mut cmd = util::cargo();
            cmd.args(["test", "--quiet", "--package", "vgl"])
                .env("VGL_TEST_BACKEND", "gallium")
                .env("VGL_GALLIUM_DEVICE", "virgl")
                .env("VGL_GALLIUM_DLL", &dll)
                .env("VTEST_SOCKET_NAME", &socket);
            let result = util::run(&mut cmd);
            let _ = server.kill();
            let _ = server.wait();
            result?;
        }
    }
    if virgl {
        // QEMU renders on desktop OpenGL, and on OpenGL ES with `gl=es`.
        for (host, what) in [("desktop", "desktop OpenGL"), ("gles", "OpenGL ES")] {
            util::status("Testing", format!("OpenGL ES on the host's GPU (QEMU's virglrenderer on {what})"));
            let mut cmd = util::cargo();
            cmd.args(["test", "--quiet", "--package", "vgl"])
                .env("VGL_TEST_BACKEND", "virgl")
                .env("VGL_TEST_HOST", host);
            if let Some(node) = &node {
                cmd.env("VGL_TEST_RENDERNODE", node);
            }
            util::run(&mut cmd)?;
        }
    }
    // One build serves every boot below.
    let started = std::time::Instant::now();
    let system = build_system(o)?;
    util::status("Built", format!("the system in {:.1}s", started.elapsed().as_secs_f32()));
    util::status("Testing", "integration tests inside Veda");
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
    check("linker", rust_lld().map(|p| p.display().to_string()));
    match qemu::QemuInstall::locate() {
        Ok(q) => {
            check("qemu", Ok(q.binary.display().to_string()));
            check("ovmf", Ok(q.ovmf_code.display().to_string()));
        }
        Err(e) => check("qemu", Err(e)),
    }
    // QEMU emulates the processor where it cannot use KVM, which works but
    // is several times slower.
    match std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm") {
        Ok(_) => println!("  [ok]   kvm: /dev/kvm"),
        Err(e) => println!(
            "  [--]   kvm: /dev/kvm: {e} (QEMU emulates the processor instead, several times slower; \
             members of the kvm group can use it)"
        ),
    }
    match toolchain::status() {
        Ok(v) => println!("  [ok]   c toolchain: {v}"),
        Err(e) => println!("  [--]   c toolchain: {e}"),
    }
    match linux::status() {
        Ok(v) => println!("  [ok]   driver vm: {v}"),
        Err(e) => println!("  [--]   driver vm: {e}"),
    }
    if ok { Ok(()) } else { Err("some checks failed".into()) }
}

/// The linker of user-space programs (`components::USER_LINKER`): LLVM's,
/// which comes with Rust's toolchain, beside the host's libraries.
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
        "linux" => linux::command(rest),
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
