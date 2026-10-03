//! The system start-up sequence: which services run, with which
//! capabilities, and which of them are restarted if they fail.

use alloc::vec::Vec;

use vabi::resource_kind;
use vabi::startup::role;
use vrt::object::Handle;
use vrt::println;

use crate::Init;

/// Handle roles used by system services (beyond the registry).
pub mod roles {
    use vabi::startup::role::USER;
    /// I/O port resource for a driver.
    pub const IOPORT_RESOURCE: u32 = USER + 1;
    /// IRQ resource for a driver.
    pub const IRQ_RESOURCE: u32 = USER + 2;
    /// MMIO resource for a driver.
    pub const MMIO_RESOURCE: u32 = USER + 3;
    /// DMA resource for a driver.
    pub const DMA_RESOURCE: u32 = USER + 4;
}

/// System services in start order. File systems come first (everything
/// else loads data through them), then the network and Wi-Fi services
/// (drivers attach to them as they start), device management and drivers,
/// the window system, audio, the voice agent and the desktop shell.
const SERVICES: [&str; 9] = ["vfs", "netd", "wlan", "devmgr", "ps2", "compositor", "audio", "agent", "shell"];

/// Services that are started again if they exit. They hold no state other
/// processes cannot recover: drivers reconnect to a restarted compositor,
/// network or Wi-Fi service, and the shell rebuilds its windows. (The file
/// system and the device manager are not restarted: one holds the user's
/// files, the other owns the running drivers.)
pub const RESTARTABLE: [&str; 7] = ["compositor", "shell", "audio", "ps2", "netd", "wlan", "agent"];

fn dup(h: &Option<vrt::Vmo>) -> Option<Handle> {
    h.as_ref().and_then(|v| v.0.duplicate(None).ok())
}

fn init_resource(init: &Init, kind: usize, base: u64, size: u64) -> Option<Handle> {
    init.root.create(kind, base, size).ok().map(|r| r.into_handle())
}

/// The capabilities a system service is started with.
fn handles_for(init: &Init, name: &str) -> Vec<(u32, Handle)> {
    let mut out = Vec::new();
    match name {
        "vfs" => out.extend(init.initrd_vmo.0.duplicate(None).ok().map(|h| (role::INITRD, h))),
        // Hardware resources to delegate to drivers (but not the root
        // resource), and the system image to load the drivers from.
        "devmgr" => {
            out.extend(init.initrd_vmo.0.duplicate(None).ok().map(|h| (role::INITRD, h)));
            for (r, kind, base, size) in [
                (roles::IOPORT_RESOURCE, resource_kind::IOPORT, 0u64, 0x1_0000u64),
                (roles::IRQ_RESOURCE, resource_kind::IRQ, 0, 256),
                (roles::MMIO_RESOURCE, resource_kind::MMIO, 0, 1 << 46),
                (roles::DMA_RESOURCE, resource_kind::DMA, 0, 0),
            ] {
                out.extend(init_resource(init, kind, base, size).map(|h| (r, h)));
            }
        }
        // The legacy keyboard controller's ports and IRQ lines.
        "ps2" => {
            out.extend(init_resource(init, resource_kind::IOPORT, 0x60, 5).map(|h| (roles::IOPORT_RESOURCE, h)));
            out.extend(init_resource(init, resource_kind::IRQ, 0, 24).map(|h| (roles::IRQ_RESOURCE, h)));
        }
        "compositor" => {
            out.extend(dup(&init.framebuffer).map(|h| (role::FRAMEBUFFER, h)));
            out.extend(dup(&init.boot_info).map(|h| (role::BOOT_INFO, h)));
        }
        _ => {}
    }
    out
}

/// Starts all system services.
pub fn start_system(init: &mut Init) {
    for name in SERVICES {
        start_service(init, name);
    }
}

/// Starts (or restarts) one system service.
pub fn start_service(init: &mut Init, name: &str) {
    let path = alloc::format!("bin/{name}.exe");
    if init.initrd.find(&path).is_none() {
        println!("{} is not installed; skipping", name);
        return;
    }
    let handles = handles_for(init, name);
    // The agent takes its test options (`agent.endpoint=...`) from the
    // kernel command line.
    let args: Vec<alloc::string::String> = if name == "agent" {
        cmdline(init).split_whitespace().filter(|a| a.starts_with("agent.")).map(Into::into).collect()
    } else {
        Vec::new()
    };
    if let Err(e) = init.spawn(name, &path, &args, handles, true) {
        println!("could not start {}: {:?}", name, e);
    }
}

/// The kernel command line (from the boot information VMO).
pub fn cmdline(init: &Init) -> alloc::string::String {
    let Some(vmo) = &init.boot_info else { return alloc::string::String::new() };
    let mut buf = [0u8; core::mem::size_of::<vabi::KernelBootInfo>()];
    if vmo.read(0, &mut buf).is_err() {
        return alloc::string::String::new();
    }
    // SAFETY: KernelBootInfo is plain old data written by the kernel.
    let info: vabi::KernelBootInfo = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const vabi::KernelBootInfo) };
    let len = (info.cmdline_len as usize).min(info.cmdline.len());
    alloc::string::String::from_utf8_lossy(&info.cmdline[..len]).into_owned()
}

/// Starts optional programs requested on the kernel command line:
/// `run=NAME` or `run=NAME:ARG1,ARG2,...`.
pub fn start_requested(init: &mut Init) {
    let cmdline = cmdline(init);
    for arg in cmdline.split_whitespace() {
        if let Some(spec) = arg.strip_prefix("run=") {
            let (name, args): (&str, Vec<alloc::string::String>) = match spec.split_once(':') {
                Some((name, args)) => (name, args.split(',').filter(|a| !a.is_empty()).map(Into::into).collect()),
                None => (spec, Vec::new()),
            };
            let path = alloc::format!("bin/{name}.exe");
            match init.spawn(name, &path, &args, Vec::new(), false) {
                Ok(_) => {}
                Err(e) => println!("cannot start {}: {:?}", name, e),
            }
        }
    }
    if cmdline.split_whitespace().any(|a| a == "systest") {
        match init.spawn("systest", "bin/systest.exe", &[], Vec::new(), false) {
            Ok(_) => {}
            Err(e) => println!("systest: FAIL (cannot start: {:?})", e),
        }
    }
}
