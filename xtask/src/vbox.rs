//! Running Vindows in VirtualBox with the same options as QEMU
//! (`cargo xtask run --vm virtualbox`).
//!
//! The virtual machine ("Vindows", its files under `target/vindows/vbox`) is
//! created on first use and brought in line with the options on every run.
//! Its disks are the very raw images QEMU uses, described by small VMDK
//! files ("monolithicFlat"), so a rebuild needs no conversion and the home
//! directory is shared between both hypervisors. The machine is closer to a
//! PC than QEMU's: an ICH9 chipset, SATA disks on an AHCI controller, an
//! Intel PRO/1000 network card, PS/2 keyboard and mouse, an xHCI USB
//! controller. The serial console goes to a file that xtask shows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::qemu::{NetMode, VmConfig};
use crate::util::{self, Result};

/// Name of the virtual machine.
pub const VM: &str = "Vindows";

/// The VirtualBox installation (`VBoxManage`).
#[derive(Clone)]
pub struct VBox {
    manage: PathBuf,
}

impl VBox {
    /// Finds `VBoxManage` via `$VBOX_MSI_INSTALL_PATH` / `$VBOX_INSTALL_PATH`
    /// (set by the installer), `PATH` or the usual install locations.
    pub fn locate() -> Result<VBox> {
        let exe = if cfg!(windows) { "VBoxManage.exe" } else { "VBoxManage" };
        let manage = ["VBOX_MSI_INSTALL_PATH", "VBOX_INSTALL_PATH"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|d| PathBuf::from(d).join(exe))
            .find(|p| p.is_file())
            .or_else(|| util::find_on_path("VBoxManage"))
            .or_else(|| {
                [
                    r"C:\Program Files\Oracle\VirtualBox\VBoxManage.exe",
                    "/usr/bin/VBoxManage",
                    "/usr/local/bin/VBoxManage",
                ]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
            })
            .ok_or("VirtualBox not found: install it or set VBOX_MSI_INSTALL_PATH")?;
        Ok(VBox { manage })
    }

    /// Runs `VBoxManage` and returns its standard output.
    pub fn manage(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.manage).args(args).output().map_err(|e| format!("running VBoxManage: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.lines().filter(|l| !l.trim().is_empty()).take(4).collect::<Vec<_>>().join(" / ");
            Err(format!("VBoxManage {}: {}", args.first().copied().unwrap_or(""), err))
        }
    }

    pub fn version(&self) -> Result<String> {
        Ok(self.manage(&["--version"])?.trim().to_string())
    }

    /// The machine's settings (`showvminfo --machinereadable`), or `None` if
    /// it does not exist or cannot be read.
    fn info(&self) -> Option<BTreeMap<String, String>> {
        let text = self.manage(&["showvminfo", VM, "--machinereadable"]).ok()?;
        Some(
            text.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim_matches('"').to_string(), v.trim_matches('"').to_string()))
                .collect(),
        )
    }

    /// The machine's state (`running`, `poweroff`, ...).
    pub fn state(&self) -> Option<String> {
        self.info()?.get("VMState").cloned()
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state().as_deref(), Some("running" | "starting" | "paused" | "stopping" | "restoring"))
    }

    /// Creates the machine if needed (re-creating it if its files are gone).
    fn ensure_vm(&self) -> Result {
        if self.info().is_some() {
            return Ok(());
        }
        // Registered but inaccessible (its folder was cleaned): unregister it.
        if self.manage(&["list", "vms"])?.lines().any(|l| l.starts_with(&format!("\"{VM}\""))) {
            let _ = self.manage(&["unregistervm", VM]);
        }
        let folder = util::out_dir().join("vbox");
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let stale = folder.join(VM);
        if stale.exists() {
            let _ = std::fs::remove_dir_all(&stale);
        }
        let folder = folder.to_string_lossy().to_string();
        self.manage(&["createvm", "--name", VM, "--ostype", "Other_64", "--basefolder", &folder, "--register"])?;
        util::status("Created", format!("VirtualBox machine \"{VM}\""));
        Ok(())
    }

    /// Brings the machine in line with `cfg`: hardware, network, disks and
    /// the serial console (written to `serial`).
    pub fn configure(
        &self,
        cfg: &VmConfig,
        boot: &Path,
        home: Option<&Path>,
        serial: &Path,
        resolution: &str,
    ) -> Result {
        if self.is_running() {
            return Err(format!("the VirtualBox machine \"{VM}\" is already running; close it first"));
        }
        self.ensure_vm()?;
        let cpus = cfg.cpus.to_string();
        let memory = cfg.memory_mib.to_string();
        let serial_path = serial.to_string_lossy().to_string();
        let mut args: Vec<String> = [
            "modifyvm",
            VM,
            "--firmware",
            "efi64",
            "--chipset",
            "ich9",
            "--cpus",
            &cpus,
            "--memory",
            &memory,
            "--ioapic",
            "on",
            "--x86-x2apic",
            "on",
            "--x86-hpet",
            "on",
            "--rtc-use-utc",
            "on",
            "--paravirt-provider",
            "none",
            "--graphicscontroller",
            "vmsvga",
            "--vram",
            "64",
            "--audio-enabled",
            "off",
            "--usb-xhci",
            "on",
            "--mouse",
            "ps2",
            "--keyboard",
            "ps2",
            "--clipboard-mode",
            "disabled",
            "--vrde",
            "off",
            "--boot1",
            "disk",
            "--boot2",
            "none",
            "--boot3",
            "none",
            "--boot4",
            "none",
            "--uart1",
            "0x3F8",
            "4",
            "--uart-type1",
            "16550A",
            "--uart-mode1",
            "file",
            &serial_path,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(self.network_args(cfg)?);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.manage(&refs)?;
        // The firmware's display mode, which the boot loader then uses.
        self.manage(&["setextradata", VM, "VBoxInternal2/EfiGraphicsResolution", resolution])?;
        self.attach_disks(boot, home)
    }

    fn network_args(&self, cfg: &VmConfig) -> Result<Vec<String>> {
        let nic_type = match cfg.nic_model.as_deref() {
            None | Some("e1000" | "82540EM" | "82540em") => "82540EM",
            Some("82545EM" | "82545em") => "82545EM",
            Some("virtio" | "virtio-net" | "virtio-net-pci") => "virtio",
            Some(other) => {
                return Err(format!("VirtualBox has no network card model '{other}' (e1000, 82545EM, virtio)"));
            }
        };
        let mut a: Vec<String> = Vec::new();
        let mut push = |s: &[&str]| a.extend(s.iter().map(|s| s.to_string()));
        match cfg.net {
            NetMode::None => push(&["--nic1", "none"]),
            NetMode::Ethernet => push(&["--nic1", "nat", "--nic-type1", nic_type, "--cable-connected1", "on"]),
            NetMode::Bridged => {
                let adapter = match &cfg.bridge_adapter {
                    Some(a) => a.clone(),
                    None => self.default_bridge_adapter()?,
                };
                util::status("Bridging", format!("to the host's \"{adapter}\""));
                push(&["--nic1", "bridged", "--nic-type1", nic_type, "--cable-connected1", "on"]);
                push(&["--bridge-adapter1", &adapter]);
            }
            NetMode::Wifi | NetMode::Both => {
                return Err("the simulated Wi-Fi (--net wifi/both) needs QEMU's virtio-serial port; \
                            VirtualBox runs support --net ethernet, bridged or none"
                    .into());
            }
        }
        // Only one card.
        push(&["--nic2", "none", "--nic3", "none", "--nic4", "none"]);
        Ok(a)
    }

    /// The host adapter to bridge to: the first one that is up and has an
    /// IPv4 address, preferring a wired one.
    pub fn default_bridge_adapter(&self) -> Result<String> {
        let text = self.manage(&["list", "bridgedifs"])?;
        let mut adapters: Vec<(String, bool)> = Vec::new();
        for block in text.split("\n\n").chain(text.split("\r\n\r\n")) {
            let field = |k: &str| {
                block.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string())).unwrap_or_default()
            };
            let (name, ip, status) = (field("Name:"), field("IPAddress:"), field("Status:"));
            if !name.is_empty() && status == "Up" && !ip.is_empty() && ip != "0.0.0.0" {
                let wireless = field("Wireless:") == "Yes";
                if !adapters.iter().any(|(n, _)| *n == name) {
                    adapters.push((name, wireless));
                }
            }
        }
        adapters.sort_by_key(|(_, wireless)| *wireless);
        adapters
            .into_iter()
            .next()
            .map(|(n, _)| n)
            .ok_or_else(|| "no host network adapter is connected (see `VBoxManage list bridgedifs`)".into())
    }

    /// Attaches the boot disk (port 0) and the home disk (port 1) to the SATA
    /// controller, with the serial numbers Vindows looks for.
    fn attach_disks(&self, boot: &Path, home: Option<&Path>) -> Result {
        let info = self.info().ok_or("cannot read the machine's settings")?;
        if !info.keys().any(|k| k.starts_with("storagecontrollername") && info[k] == "SATA") {
            self.manage(&[
                "storagectl",
                VM,
                "--name",
                "SATA",
                "--add",
                "sata",
                "--controller",
                "IntelAhci",
                "--portcount",
                "2",
                "--bootable",
                "on",
            ])?;
        }
        for (port, image, serial) in [(0, Some(boot), "vindows-boot"), (1, home, "vindows-home")] {
            let key = format!("VBoxInternal/Devices/ahci/0/Config/Port{port}/SerialNumber");
            let p = port.to_string();
            match image {
                Some(image) => {
                    let vmdk = flat_vmdk(image)?;
                    self.attach(&p, &vmdk)?;
                    self.manage(&["setextradata", VM, &key, serial])?;
                }
                None => {
                    let _ = self.manage(&[
                        "storageattach",
                        VM,
                        "--storagectl",
                        "SATA",
                        "--port",
                        &p,
                        "--device",
                        "0",
                        "--medium",
                        "none",
                    ]);
                }
            }
        }
        Ok(())
    }

    /// Attaches a VMDK to a SATA port, re-registering it if VirtualBox still
    /// knows an older version (for example with a different size).
    fn attach(&self, port: &str, vmdk: &Path) -> Result {
        let path = vmdk.to_string_lossy().to_string();
        let args = [
            "storageattach",
            VM,
            "--storagectl",
            "SATA",
            "--port",
            port,
            "--device",
            "0",
            "--type",
            "hdd",
            "--medium",
            &path,
        ];
        if self.manage(&args).is_ok() {
            return Ok(());
        }
        let _ = self.manage(&[
            "storageattach",
            VM,
            "--storagectl",
            "SATA",
            "--port",
            port,
            "--device",
            "0",
            "--medium",
            "none",
        ]);
        let _ = self.manage(&["closemedium", "disk", &path]);
        self.manage(&args).map(|_| ())
    }

    pub fn start(&self, headless: bool) -> Result {
        self.manage(&["startvm", VM, "--type", if headless { "headless" } else { "gui" }]).map(|_| ())
    }

    pub fn poweroff(&self) {
        let _ = self.manage(&["controlvm", VM, "poweroff"]);
        let start = Instant::now();
        while self.is_running() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn reset(&self) -> Result {
        self.manage(&["controlvm", VM, "reset"]).map(|_| ())
    }

    pub fn screenshot(&self, path: &Path) -> Result {
        self.manage(&["controlvm", VM, "screenshotpng", &path.to_string_lossy()]).map(|_| ())
    }

    /// Sends raw PS/2 set-1 scan codes.
    pub fn scancodes(&self, codes: &[u8]) -> Result {
        if codes.is_empty() {
            return Ok(());
        }
        let hex: Vec<String> = codes.iter().map(|c| format!("{c:02x}")).collect();
        let mut args = vec!["controlvm", VM, "keyboardputscancode"];
        args.extend(hex.iter().map(String::as_str));
        self.manage(&args).map(|_| ())
    }

    pub fn set_link(&self, up: bool) -> Result {
        self.manage(&["controlvm", VM, "setlinkstate1", if up { "on" } else { "off" }]).map(|_| ())
    }
}

/// Ctrl+C in the terminal: noticed instead of ending xtask, so that it can
/// power the machine off first (as QEMU stops when xtask is interrupted).
pub mod ctrl_c {
    use std::sync::atomic::{AtomicBool, Ordering};

    static PRESSED: AtomicBool = AtomicBool::new(false);

    #[cfg(windows)]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    #[cfg(windows)]
    unsafe extern "system" fn handler(_event: u32) -> i32 {
        PRESSED.store(true, Ordering::SeqCst);
        1
    }

    pub fn install() {
        #[cfg(windows)]
        // SAFETY: registers a handler that only stores to an atomic.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }

    pub fn pressed() -> bool {
        PRESSED.load(Ordering::SeqCst)
    }
}

/// Shows the serial console (`echo`) while the machine runs; powers it off
/// on Ctrl+C. Returns when the machine has stopped.
pub fn follow(vbox: &VBox, serial: &Path, echo: bool) -> Result {
    use std::io::{Read, Seek, SeekFrom, Write};
    ctrl_c::install();
    let mut offset = 0u64;
    let mut last_check = Instant::now();
    let mut stdout = std::io::stdout();
    loop {
        if ctrl_c::pressed() {
            util::status("Stopping", format!("VirtualBox machine \"{VM}\""));
            vbox.poweroff();
            return Ok(());
        }
        if echo && let Ok(mut f) = std::fs::File::open(serial) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < offset {
                offset = 0;
            }
            if len > offset && f.seek(SeekFrom::Start(offset)).is_ok() {
                let mut buf = Vec::new();
                if f.take(len - offset).read_to_end(&mut buf).is_ok() {
                    offset += buf.len() as u64;
                    let _ = stdout.write_all(&buf);
                    let _ = stdout.flush();
                }
            }
        }
        if last_check.elapsed() >= Duration::from_millis(500) {
            last_check = Instant::now();
            if !vbox.is_running() {
                util::status("Stopped", format!("VirtualBox machine \"{VM}\""));
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A stable UUID for a file path (VirtualBox identifies media by UUID).
fn path_uuid(path: &Path) -> String {
    let s = path.to_string_lossy().to_lowercase();
    let mut h = [0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64];
    for b in s.bytes() {
        h[0] = (h[0] ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
        h[1] = (h[1] ^ (b as u64).rotate_left(3)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    let x = (h[0] as u128) << 64 | h[1] as u128;
    let hex = format!("{x:032x}");
    // Version 4 / variant 1 bits, so it is a well-formed UUID.
    let mut c: Vec<char> = hex.chars().collect();
    c[12] = '4';
    c[16] = ['8', '9', 'a', 'b'][(c[16].to_digit(16).unwrap_or(0) & 3) as usize];
    let h: String = c.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Writes (or refreshes) a VMDK descriptor next to a raw disk image that
/// lets VirtualBox use the image in place; returns its path.
pub fn flat_vmdk(raw: &Path) -> Result<PathBuf> {
    let raw = std::fs::canonicalize(raw).map_err(|e| format!("{}: {e}", raw.display()))?;
    let size = std::fs::metadata(&raw).map_err(|e| e.to_string())?.len();
    if size == 0 || !size.is_multiple_of(512) {
        return Err(format!("{} is not a whole number of sectors", raw.display()));
    }
    let sectors = size / 512;
    let cylinders = (sectors / (16 * 63)).clamp(1, 16383);
    let vmdk = raw.with_extension("vmdk");
    let file_name = raw.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let text = format!(
        "# Disk DescriptorFile\n\
         # Written by cargo xtask: lets VirtualBox use {file_name} in place.\n\
         version=1\n\
         CID=fffffffe\n\
         parentCID=ffffffff\n\
         createType=\"monolithicFlat\"\n\
         \n\
         RW {sectors} FLAT \"{file_name}\" 0\n\
         \n\
         ddb.virtualHWVersion = \"4\"\n\
         ddb.adapterType = \"ide\"\n\
         ddb.geometry.cylinders = \"{cylinders}\"\n\
         ddb.geometry.heads = \"16\"\n\
         ddb.geometry.sectors = \"63\"\n\
         ddb.uuid.image = \"{}\"\n\
         ddb.uuid.parent = \"00000000-0000-0000-0000-000000000000\"\n\
         ddb.uuid.modification = \"00000000-0000-0000-0000-000000000000\"\n\
         ddb.uuid.parentmodification = \"00000000-0000-0000-0000-000000000000\"\n",
        path_uuid(&raw)
    );
    if std::fs::read_to_string(&vmdk).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&vmdk, text).map_err(|e| format!("writing {}: {e}", vmdk.display()))?;
    }
    Ok(vmdk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_stable_and_well_formed() {
        let a = path_uuid(Path::new(r"C:\x\target\vindows\vindows.img"));
        assert_eq!(a, path_uuid(Path::new(r"c:\X\TARGET\vindows\vindows.img")));
        assert_ne!(a, path_uuid(Path::new(r"C:\x\target\vindows\home.img")));
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!("89ab".contains(&a[19..20]));
    }
}
