//! Locating QEMU and the OVMF firmware, and assembling the command line.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{self, Result};

/// Where to find QEMU and its UEFI firmware.
pub struct QemuInstall {
    pub binary: PathBuf,
    pub ovmf_code: PathBuf,
    pub ovmf_vars_template: PathBuf,
}

impl QemuInstall {
    /// Finds QEMU via `$QEMU`, `PATH` or well-known install locations, and the
    /// OVMF firmware via `$OVMF_CODE`/`$OVMF_VARS`, QEMU's data directory or
    /// where Linux distributions install it.
    pub fn locate() -> Result<Self> {
        let binary = std::env::var_os("QEMU")
            .map(PathBuf::from)
            .or_else(|| util::find_on_path("qemu-system-x86_64"))
            .or_else(|| {
                [
                    r"C:\Program Files\qemu\qemu-system-x86_64.exe",
                    "/usr/bin/qemu-system-x86_64",
                    "/opt/homebrew/bin/qemu-system-x86_64",
                ]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
            })
            .ok_or("QEMU not found: install it or set the QEMU environment variable")?;

        let qemu_dir = binary.parent().unwrap_or(Path::new("."));
        let dirs = [
            qemu_dir.join("share"),
            qemu_dir.join("../share/qemu"),
            PathBuf::from("/usr/share/qemu"),
            PathBuf::from("/usr/share/OVMF"),
            PathBuf::from("/usr/share/edk2/ovmf"),
            PathBuf::from("/usr/share/edk2/x64"),
        ];
        // The firmware and its variable store, as QEMU's own builds name
        // them and as Linux distributions do (Debian and Ubuntu, Fedora,
        // Arch): both from one place, as their sizes must match.
        let pairs = [
            ("edk2-x86_64-code.fd", "edk2-i386-vars.fd"),
            ("OVMF_CODE.fd", "OVMF_VARS.fd"),
            ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"),
            ("OVMF_CODE.4m.fd", "OVMF_VARS.4m.fd"),
        ];
        let found = dirs
            .iter()
            .flat_map(|d| pairs.iter().map(move |(code, vars)| (d.join(code), d.join(vars))))
            .find(|(code, vars)| code.is_file() && vars.is_file());
        let missing = "the OVMF firmware was not found: install it (QEMU for Windows includes it; on Linux \
                       it is the ovmf or edk2-ovmf package) or set OVMF_CODE and OVMF_VARS";
        let ovmf_code = std::env::var_os("OVMF_CODE")
            .map(PathBuf::from)
            .or_else(|| found.as_ref().map(|(code, _)| code.clone()))
            .ok_or(missing)?;
        let ovmf_vars_template =
            std::env::var_os("OVMF_VARS").map(PathBuf::from).or_else(|| found.map(|(_, vars)| vars)).ok_or(missing)?;
        Ok(QemuInstall { binary, ovmf_code, ovmf_vars_template })
    }

    /// The directory of the virglrenderer library this QEMU runs, which the
    /// OpenGL ES tests can drive directly (Windows builds, which ship it
    /// with ANGLE).
    pub fn virglrenderer_dir(&self) -> Option<PathBuf> {
        let dir = self.binary.parent()?;
        (cfg!(windows) && dir.join("libvirglrenderer-1.dll").is_file() && dir.join("libEGL.dll").is_file())
            .then(|| dir.to_path_buf())
    }

    /// Whether this QEMU has the 3D virtio-gpu (built with virglrenderer):
    /// it lists `virtio-vga-gl` among its devices.
    pub fn has_gl_gpu(&self) -> bool {
        static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *HAS.get_or_init(|| {
            std::process::Command::new(&self.binary)
                .args(["-device", "help"])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("\"virtio-vga-gl\""))
        })
    }
}

/// How the virtual machine reaches the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// No network card.
    None,
    /// A wired card on QEMU's NAT (DHCP 10.0.2.15, gateway 10.0.2.2, DNS
    /// 10.0.2.3; the host is reachable at 10.0.2.2).
    Ethernet,
    /// The virtual Wi-Fi radio only: a virtio-serial port connected to the
    /// `airsim` simulator, whose access points bridge to a second NAT
    /// (10.0.3.0/24: gateway 10.0.3.2, DNS 10.0.3.3).
    Wifi,
    /// Both the wired card and the Wi-Fi radio.
    Both,
    /// The wired card bridged to a host network adapter (VirtualBox only):
    /// Veda joins the host's real network (its router's DHCP, DNS and
    /// Internet).
    Bridged,
}

impl NetMode {
    pub fn parse(s: &str) -> Option<NetMode> {
        match s {
            "none" | "off" => Some(NetMode::None),
            "ethernet" | "wired" | "user" => Some(NetMode::Ethernet),
            "wifi" | "wi-fi" | "wireless" => Some(NetMode::Wifi),
            "both" => Some(NetMode::Both),
            "bridged" | "bridge" => Some(NetMode::Bridged),
            _ => None,
        }
    }

    pub fn wired(self) -> bool {
        matches!(self, NetMode::Ethernet | NetMode::Both | NetMode::Bridged)
    }

    pub fn wireless(self) -> bool {
        matches!(self, NetMode::Wifi | NetMode::Both)
    }
}

/// The local ports joining QEMU and `airsim` (see `crate::airsim`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WifiPorts {
    /// QEMU listens here for the simulator's radio connection.
    pub radio: u16,
    /// QEMU's end of the bridged NAT link (UDP).
    pub qemu_udp: u16,
    /// The simulator's end of it (UDP).
    pub sim_udp: u16,
}

/// How the disks are attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskBus {
    /// virtio-blk (QEMU's default here).
    Virtio,
    /// SATA disks on the machine's AHCI controller (as in VirtualBox and most
    /// PCs).
    Ahci,
}

impl DiskBus {
    pub fn parse(s: &str) -> Option<DiskBus> {
        match s {
            "virtio" => Some(DiskBus::Virtio),
            "ahci" | "sata" => Some(DiskBus::Ahci),
            _ => None,
        }
    }
}

/// The keyboard and pointing devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDevices {
    /// QEMU: a PS/2 keyboard and the virtio tablet; VirtualBox: a PS/2
    /// keyboard and mouse.
    Standard,
    /// USB devices on the xHCI controller, as most PCs have. QEMU: a
    /// keyboard and a tablet behind a hub, and a mouse; VirtualBox: a
    /// keyboard and a tablet.
    Usb,
}

impl InputDevices {
    pub fn parse(s: &str) -> Option<InputDevices> {
        match s {
            "standard" => Some(InputDevices::Standard),
            "usb" => Some(InputDevices::Usb),
            _ => None,
        }
    }
}

/// User-tunable virtual machine settings.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub cpus: u32,
    pub memory_mib: u32,
    /// Show a window (`false` = headless).
    pub display: bool,
    pub audio: bool,
    /// The sound card: `virtio` (QEMU's default here), `ac97` (VirtualBox's
    /// default) or `hda` (Intel HD Audio).
    pub sound: String,
    /// Record guest audio to this WAV file instead of playing it.
    pub audio_wav: Option<PathBuf>,
    /// QEMU's silent sound system instead (`none`): nothing records the
    /// output; the input records silence, at the card's pace.
    pub audio_silent: bool,
    /// Serial output destination: `None` = this terminal.
    pub serial_file: Option<PathBuf>,
    /// QMP control socket (TCP port on localhost), used by automated tests.
    pub qmp_port: Option<u16>,
    /// Wait for a GDB connection on port 1234.
    pub gdb: bool,
    /// Allow the guest to terminate QEMU with an exit code (tests).
    pub debug_exit: bool,
    /// Disk image holding the user's home directory (serial `veda-home`).
    pub home_disk: Option<PathBuf>,
    /// The boot image is the live system's ISO on a USB stick (read-only),
    /// not the boot disk.
    pub usb_stick: bool,
    /// How the boot and home disks are attached.
    pub disk_bus: DiskBus,
    /// The keyboard and pointing devices.
    pub input: InputDevices,
    /// Let the guest reboot (otherwise a reset, e.g. after a triple fault,
    /// stops QEMU).
    pub allow_reboot: bool,
    /// The network connection.
    pub net: NetMode,
    /// Model of the wired card (`None`: the hypervisor's default, virtio-net
    /// for QEMU and the Intel 82540EM for VirtualBox).
    pub nic_model: Option<String>,
    /// VirtualBox: the host adapter to bridge to (`None`: the first one
    /// connected).
    pub bridge_adapter: Option<String>,
    /// Ports for the Wi-Fi radio and its NAT link (required when `net`
    /// includes Wi-Fi).
    pub wifi: Option<WifiPorts>,
    /// A display that is also a 3D GPU (virtio-gpu with virgl): `None` uses
    /// one if QEMU has it.
    pub gpu: Option<bool>,
    /// An IOMMU (QEMU's intel-iommu, remapping interrupts), for the
    /// driver VM's devices.
    pub iommu: bool,
    /// QEMU's USB network adapter (`usb-net`: CDC Ethernet, which Veda has
    /// no driver for) on the xHCI controller, on a NAT of its own.
    pub usb_net: bool,
    /// Extra raw QEMU arguments.
    pub extra: Vec<String>,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig {
            cpus: 4,
            memory_mib: 1024,
            display: true,
            audio: true,
            sound: "virtio".into(),
            audio_wav: None,
            audio_silent: false,
            serial_file: None,
            qmp_port: None,
            gdb: false,
            debug_exit: false,
            home_disk: None,
            usb_stick: false,
            disk_bus: DiskBus::Virtio,
            input: InputDevices::Standard,
            allow_reboot: false,
            net: NetMode::Ethernet,
            nic_model: None,
            bridge_adapter: None,
            wifi: None,
            gpu: None,
            iommu: false,
            usb_net: false,
            extra: Vec::new(),
        }
    }
}

/// QEMU id of the wired network card (for QMP `set_link`).
pub const WIRED_NIC_ID: &str = "nic0";

/// QEMU id of the display device.
const DISPLAY_ID: &str = "video0";

/// Size of a new home disk.
const HOME_DISK_BYTES: u64 = 64 << 20;

/// Creates an empty home disk at `path` (replacing any existing one when
/// `fresh`). An empty disk makes Veda start a new home directory.
pub fn prepare_home_disk(path: &Path, fresh: bool) -> Result {
    if fresh || !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let f = std::fs::File::create(path).map_err(|e| format!("creating {}: {e}", path.display()))?;
        f.set_len(HOME_DISK_BYTES).map_err(|e| format!("sizing {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Builds the QEMU command line for booting `disk`.
pub fn command(install: &QemuInstall, disk: &Path, vars: &Path, cfg: &VmConfig) -> Command {
    let mut cmd = Command::new(&install.binary);
    let flash = |file: &Path, ro: bool| {
        format!(
            "if=pflash,format=raw,unit={},{}file={}",
            if ro { 0 } else { 1 },
            if ro { "readonly=on," } else { "" },
            file.display()
        )
    };
    cmd.args(["-name", "Veda"]);
    // With USB input there is no PS/2 controller, as on many PCs: keys can
    // only come through USB.
    let mut machine = String::from("q35");
    if cfg.input == InputDevices::Usb {
        machine.push_str(",i8042=off");
    }
    // Interrupt remapping needs the I/O APIC in QEMU, not in KVM.
    if cfg.iommu {
        machine.push_str(",kernel-irqchip=split");
    }
    cmd.args(["-machine", &machine]);
    // The IOMMU comes before the devices it translates.
    if cfg.iommu {
        cmd.args(["-device", "intel-iommu,intremap=on,eim=on"]);
    }
    // Prefer hardware virtualisation when the host offers it; fall back to
    // the TCG emulator (multi-threaded, one host thread per vCPU) otherwise.
    if cfg!(windows) {
        cmd.args(["-accel", "whpx,kernel-irqchip=off", "-accel", "tcg,thread=multi"]);
    } else if cfg!(target_os = "linux") {
        cmd.args(["-accel", "kvm", "-accel", "tcg,thread=multi"]);
    } else {
        cmd.args(["-accel", "tcg,thread=multi"]);
    }
    cmd.args(["-cpu", "max"]);
    cmd.args(["-smp", &cfg.cpus.to_string()]);
    cmd.args(["-m", &format!("{}M", cfg.memory_mib)]);
    cmd.args(["-drive", &flash(&install.ovmf_code, true)]);
    cmd.args(["-drive", &flash(vars, false)]);
    // The display, at 00:01.0 (where `-vga std` puts it, before any other
    // device takes it), named so that the tablet can be bound to it (see
    // `-display` below). With a GPU it is virtio-vga-gl: a standard VGA
    // that is also virtio-gpu with virgl, whose virglrenderer runs on the
    // host's OpenGL (ANGLE on Windows), and needs an OpenGL display
    // backend, a window or an offscreen one. The firmware and Veda show the
    // picture through its VGA side, as with plain VGA; the `virtio-gpu`
    // driver uses only the 3D side and never takes the scanout. Not VGA
    // beside a separate virtio-gpu-gl-pci: the firmware drives that one
    // itself, as a second display, and resets it at boot's end, and such a
    // reset (from a vCPU thread, which waits for QEMU's main loop) now and
    // then deadlocks QEMU when the device has OpenGL.
    let gpu = cfg.gpu.unwrap_or_else(|| install.has_gl_gpu());
    let display = format!("id={DISPLAY_ID},addr=0x1");
    let display = if gpu { virtio(cfg, &format!("virtio-vga-gl,{display}")) } else { format!("VGA,{display}") };
    cmd.args(["-vga", "none", "-device", &display]);
    // In a window, the tablet is bound to the display: the window's pointer
    // is then absolute from power-on, not only once the guest's driver has
    // started (see `-display` below).
    let bound = if cfg.display { format!(",display={DISPLAY_ID}") } else { String::new() };
    if cfg.usb_stick || cfg.input == InputDevices::Usb || cfg.usb_net {
        cmd.args(["-device", "qemu-xhci,id=xhci"]);
    }
    if cfg.usb_net {
        cmd.args(["-netdev", "user,id=usbnet0,net=10.0.4.0/24"]);
        cmd.args(["-device", "usb-net,netdev=usbnet0,bus=xhci.0,mac=52:54:00:12:34:58"]);
    }
    if cfg.usb_stick {
        // As a PC sees a stick the ISO was written to.
        cmd.args(["-drive", &format!("id=disk0,if=none,format=raw,readonly=on,file={}", disk.display())]);
        cmd.args(["-device", "usb-storage,bus=xhci.0,drive=disk0,bootindex=0,removable=on"]);
    } else {
        cmd.args(["-drive", &format!("id=disk0,if=none,format=raw,file={}", disk.display())]);
        // q35's built-in AHCI controller has six ports, ide.0 to ide.5.
        match cfg.disk_bus {
            DiskBus::Virtio => {
                cmd.args(["-device", &virtio(cfg, "virtio-blk-pci,drive=disk0,bootindex=0,serial=veda-boot")])
            }
            DiskBus::Ahci => cmd.args(["-device", "ide-hd,bus=ide.0,drive=disk0,bootindex=0,serial=veda-boot"]),
        };
    }
    if let Some(home) = &cfg.home_disk {
        cmd.args(["-drive", &format!("id=home,if=none,format=raw,file={}", home.display())]);
        match cfg.disk_bus {
            DiskBus::Virtio => cmd.args(["-device", &virtio(cfg, "virtio-blk-pci,drive=home,serial=veda-home")]),
            DiskBus::Ahci => cmd.args(["-device", "ide-hd,bus=ide.1,drive=home,serial=veda-home"]),
        };
    }
    match cfg.input {
        InputDevices::Standard => {
            cmd.args(["-device", &virtio(cfg, &format!("virtio-tablet-pci{bound}"))]);
        }
        InputDevices::Usb => {
            // A keyboard and a tablet behind a hub, and a mouse on a root
            // port (after the live system's stick, if any). QMP's absolute
            // pointer events go to the tablet.
            cmd.args(["-device", "usb-hub,bus=xhci.0,port=2"]);
            cmd.args(["-device", "usb-kbd,bus=xhci.0,port=2.1"]);
            cmd.args(["-device", &format!("usb-tablet,bus=xhci.0,port=2.2{bound}")]);
            cmd.args(["-device", "usb-mouse,bus=xhci.0,port=3"]);
        }
    }
    // Host entropy for the firmware's EFI_RNG_PROTOCOL, which seeds the
    // kernel's random number generator.
    cmd.args(["-device", &virtio(cfg, "virtio-rng-pci")]);
    if cfg.audio {
        match &cfg.audio_wav {
            _ if cfg.audio_silent => cmd.args(["-audiodev", "none,id=audio0"]),
            Some(wav) => cmd.args(["-audiodev", &format!("wav,id=audio0,path={}", wav.display())]),
            None => cmd.args(["-audiodev", if cfg!(windows) { "dsound,id=audio0" } else { "sdl,id=audio0" }]),
        };
        // With the host's sound system the card also has an input stream:
        // the host's microphone (with the silent one, silence). Recorded
        // runs (WAV) have none, so a test microphone can take its place.
        let streams = if cfg.audio_wav.is_some() { 1 } else { 2 };
        match cfg.sound.as_str() {
            "ac97" => cmd.args(["-device", "AC97,audiodev=audio0"]),
            // The ICH9's HD Audio controller with a codec that has a line
            // output and, with the host's sound system, a line input.
            "hda" => {
                let codec = if streams == 2 { "hda-duplex" } else { "hda-output" };
                cmd.args(["-device", "ich9-intel-hda,id=hda"]);
                cmd.args(["-device", &format!("{codec},bus=hda.0,audiodev=audio0")])
            }
            _ => cmd.args(["-device", &virtio(cfg, &format!("virtio-sound-pci,audiodev=audio0,streams={streams}"))]),
        };
    }
    if cfg.net.wired() {
        let model = cfg.nic_model.as_deref().unwrap_or("virtio-net-pci");
        let nic = format!("{model},id={WIRED_NIC_ID},netdev=net0,mac=52:54:00:12:34:56");
        cmd.args(["-netdev", "user,id=net0"]);
        cmd.args(["-device", &if model.starts_with("virtio") { virtio(cfg, &nic) } else { nic }]);
    } else {
        // Without this QEMU adds a default card.
        cmd.args(["-nic", "none"]);
    }
    if let (true, Some(p)) = (cfg.net.wireless(), cfg.wifi) {
        cmd.args(wifi_args(cfg, &p));
    }
    if cfg.display {
        // When the guest's tablet driver starts, QEMU tells its window the
        // pointer has become absolute, on the vCPU thread. On Windows a
        // window's grab of the mouse, set or released from that thread,
        // waits for the window's own thread, which waits for the vCPU: QEMU
        // deadlocks early in Veda's boot (seen with QEMU 11.1). SDL grabs
        // the mouse then if the pointer is over the window, so not SDL. GTK
        // releases a grab then, and grabs at a click in the window while the
        // pointer is relative, as it is from power-on until that driver
        // starts; which is why the tablet is bound to the display: the
        // window's pointer is absolute from the start, and a click before
        // the driver runs grabs nothing.
        cmd.args(["-display", if gpu { "gtk,gl=on" } else { "gtk" }]);
    } else {
        cmd.args(["-display", if gpu { "egl-headless" } else { "none" }]);
    }
    match &cfg.serial_file {
        Some(path) => cmd.args(["-serial", &format!("file:{}", path.display())]),
        // The terminal, and a copy in the output directory (for looking at
        // a session afterwards).
        None => cmd.args([
            "-chardev",
            &format!("stdio,id=serial0,logfile={}", util::out_dir().join("serial.log").display()),
            "-serial",
            "chardev:serial0",
        ]),
    };
    if let Some(port) = cfg.qmp_port {
        cmd.args(["-qmp", &format!("tcp:127.0.0.1:{port},server=on,wait=off")]);
    }
    if cfg.debug_exit {
        cmd.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    }
    if cfg.gdb {
        cmd.args(["-s", "-S"]);
    }
    if !cfg.allow_reboot {
        cmd.args(["-no-reboot"]);
    }
    cmd.args(&cfg.extra);
    cmd
}

/// A virtio device of `spec`'s (`virtio-blk-pci,drive=...`): behind the
/// machine's IOMMU when it has one, as devices are on a PC that has one, so
/// that a device can go to a virtual machine of Veda's (whose addresses
/// only the IOMMU turns into the machine's). Translated devices are modern
/// ones only.
fn virtio(cfg: &VmConfig, spec: &str) -> String {
    if cfg.iommu { format!("{spec},disable-legacy=on,iommu_platform=on") } else { spec.to_string() }
}

/// QEMU arguments for the virtual Wi-Fi radio: a virtio-serial port named
/// `org.veda.wlan.0` that `airsim` connects to, and a NAT (`user`
/// network) joined through a hub to a UDP link with `airsim`, whose access
/// points bridge their stations onto it.
pub fn wifi_args(cfg: &VmConfig, p: &WifiPorts) -> Vec<String> {
    [
        "-device",
        &virtio(cfg, "virtio-serial-pci,id=vser0,max_ports=2"),
        "-chardev",
        &format!("socket,id=wlanradio,host=127.0.0.1,port={},server=on,wait=off", p.radio),
        "-device",
        "virtserialport,bus=vser0.0,nr=1,chardev=wlanradio,name=org.veda.wlan.0",
        "-netdev",
        "user,id=wlanwan,net=10.0.3.0/24",
        "-netdev",
        "hubport,id=wlanhub0,hubid=7,netdev=wlanwan",
        "-netdev",
        &format!(
            "dgram,id=wlanair,local.type=inet,local.host=127.0.0.1,local.port={},\
             remote.type=inet,remote.host=127.0.0.1,remote.port={}",
            p.qemu_udp, p.sim_udp
        ),
        "-netdev",
        "hubport,id=wlanhub1,hubid=7,netdev=wlanair",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Returns the path of this machine's writable UEFI variable store, creating
/// it from the firmware template on first use.
pub fn vars_file(install: &QemuInstall) -> Result<PathBuf> {
    let path = util::out_dir().join("ovmf-vars.fd");
    if !path.exists() {
        std::fs::create_dir_all(util::out_dir()).map_err(|e| e.to_string())?;
        std::fs::copy(&install.ovmf_vars_template, &path)
            .map_err(|e| format!("copying {}: {e}", install.ovmf_vars_template.display()))?;
    }
    Ok(path)
}
