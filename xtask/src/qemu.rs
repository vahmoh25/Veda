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
    /// OVMF firmware via `$OVMF_CODE`/`$OVMF_VARS` or QEMU's data directory.
    pub fn locate() -> Result<Self> {
        let binary = std::env::var_os("QEMU")
            .map(PathBuf::from)
            .or_else(|| util::find_on_path("qemu-system-x86_64"))
            .or_else(|| {
                [r"C:\Program Files\qemu\qemu-system-x86_64.exe", "/usr/bin/qemu-system-x86_64", "/opt/homebrew/bin/qemu-system-x86_64"]
                    .iter()
                    .map(PathBuf::from)
                    .find(|p| p.is_file())
            })
            .ok_or("QEMU not found: install it or set the QEMU environment variable")?;

        let qemu_dir = binary.parent().unwrap_or(Path::new("."));
        let candidates = |names: &[&str]| -> Option<PathBuf> {
            let dirs = [
                qemu_dir.join("share"),
                qemu_dir.join("../share/qemu"),
                PathBuf::from("/usr/share/qemu"),
                PathBuf::from("/usr/share/OVMF"),
                PathBuf::from("/usr/share/edk2/x64"),
            ];
            dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
        };
        let ovmf_code = std::env::var_os("OVMF_CODE")
            .map(PathBuf::from)
            .or_else(|| candidates(&["edk2-x86_64-code.fd", "OVMF_CODE.fd", "OVMF_CODE_4M.fd"]))
            .ok_or("OVMF firmware (edk2-x86_64-code.fd) not found; set OVMF_CODE")?;
        let ovmf_vars_template = std::env::var_os("OVMF_VARS")
            .map(PathBuf::from)
            .or_else(|| candidates(&["edk2-i386-vars.fd", "OVMF_VARS.fd", "OVMF_VARS_4M.fd"]))
            .ok_or("OVMF variable store (edk2-i386-vars.fd) not found; set OVMF_VARS")?;
        Ok(QemuInstall { binary, ovmf_code, ovmf_vars_template })
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
    /// Record guest audio to this WAV file instead of playing it.
    pub audio_wav: Option<PathBuf>,
    /// Serial output destination: `None` = this terminal.
    pub serial_file: Option<PathBuf>,
    /// QMP control socket (TCP port on localhost), used by automated tests.
    pub qmp_port: Option<u16>,
    /// Wait for a GDB connection on port 1234.
    pub gdb: bool,
    /// Allow the guest to terminate QEMU with an exit code (tests).
    pub debug_exit: bool,
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
            audio_wav: None,
            serial_file: None,
            qmp_port: None,
            gdb: false,
            debug_exit: false,
            extra: Vec::new(),
        }
    }
}

/// Builds the QEMU command line for booting `disk`.
pub fn command(install: &QemuInstall, disk: &Path, vars: &Path, cfg: &VmConfig) -> Command {
    let mut cmd = Command::new(&install.binary);
    let flash = |file: &Path, ro: bool| {
        format!("if=pflash,format=raw,unit={},{}file={}", if ro { 0 } else { 1 }, if ro { "readonly=on," } else { "" }, file.display())
    };
    cmd.args(["-name", "Vindows"]);
    cmd.args(["-machine", "q35"]);
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
    cmd.args(["-drive", &format!("id=disk0,if=none,format=raw,file={}", disk.display())]);
    cmd.args(["-device", "virtio-blk-pci,drive=disk0,bootindex=0"]);
    cmd.args(["-device", "virtio-tablet-pci"]);
    cmd.args(["-vga", "std"]);
    if cfg.audio {
        match &cfg.audio_wav {
            Some(wav) => cmd.args(["-audiodev", &format!("wav,id=audio0,path={}", wav.display())]),
            None => cmd.args(["-audiodev", if cfg!(windows) { "dsound,id=audio0" } else { "sdl,id=audio0" }]),
        };
        cmd.args(["-device", "virtio-sound-pci,audiodev=audio0,streams=1"]);
    }
    if cfg.display {
        cmd.args(["-display", "sdl"]);
    } else {
        cmd.args(["-display", "none"]);
    }
    match &cfg.serial_file {
        Some(path) => cmd.args(["-serial", &format!("file:{}", path.display())]),
        None => cmd.args(["-serial", "stdio"]),
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
    cmd.args(["-no-reboot"]);
    cmd.args(&cfg.extra);
    cmd
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
