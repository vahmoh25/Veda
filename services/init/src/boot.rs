//! The system start-up sequence: which services run, with which
//! capabilities.

use alloc::vec;
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

fn dup(h: &Option<vrt::Vmo>) -> Option<Handle> {
    h.as_ref().and_then(|v| v.0.duplicate(None).ok())
}

pub fn start_system(init: &mut Init) {
    // File systems first: everything else loads data through them.
    let initrd = init.initrd_vmo.0.duplicate(None).ok();
    start(init, "vfs", initrd.map(|h| vec![(role::INITRD, h)]).unwrap_or_default());

    // Device manager: enumerates PCI and starts device drivers. It gets
    // hardware resources to delegate, but not the root resource.
    let mut dev = Vec::new();
    for (r, kind, base, size) in [
        (roles::IOPORT_RESOURCE, resource_kind::IOPORT, 0u64, 0x1_0000u64),
        (roles::IRQ_RESOURCE, resource_kind::IRQ, 0, 256),
        (roles::MMIO_RESOURCE, resource_kind::MMIO, 0, 1 << 46),
        (roles::DMA_RESOURCE, resource_kind::DMA, 0, 0),
    ] {
        if let Some(h) = init_resource(init, kind, base, size) {
            dev.push((r, h));
        }
    }
    start(init, "devmgr", dev);

    // Legacy PS/2 keyboard and mouse.
    let mut ps2 = Vec::new();
    if let Some(h) = init_resource(init, resource_kind::IOPORT, 0x60, 5) {
        ps2.push((roles::IOPORT_RESOURCE, h));
    }
    if let Some(h) = init_resource(init, resource_kind::IRQ, 0, 24) {
        ps2.push((roles::IRQ_RESOURCE, h));
    }
    start(init, "ps2", ps2);

    // Window system.
    let mut display = Vec::new();
    if let Some(h) = dup(&init.framebuffer) {
        display.push((role::FRAMEBUFFER, h));
    }
    if let Some(h) = dup(&init.boot_info) {
        display.push((role::BOOT_INFO, h));
    }
    start(init, "compositor", display);
    start(init, "audio", Vec::new());
    start(init, "shell", Vec::new());
}

fn init_resource(init: &Init, kind: usize, base: u64, size: u64) -> Option<Handle> {
    init.root.create(kind, base, size).ok().map(|r| r.into_handle())
}

fn start(init: &mut Init, name: &str, handles: Vec<(u32, Handle)>) {
    let path = alloc::format!("bin/{name}.exe");
    if init.initrd.find(&path).is_none() {
        println!("init: {} is not installed; skipping", name);
        return;
    }
    match init.spawn(name, &path, &[], handles, true) {
        Ok(koid) => println!("init: started {} (process {})", name, koid),
        Err(e) => println!("init: could not start {}: {:?}", name, e),
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

/// Starts optional programs requested on the kernel command line.
pub fn start_requested(init: &mut Init) {
    let cmdline = cmdline(init);
    for arg in cmdline.split_whitespace() {
        if let Some(name) = arg.strip_prefix("run=") {
            let path = alloc::format!("bin/{name}.exe");
            match init.spawn(name, &path, &[], Vec::new(), false) {
                Ok(_) => println!("init: started {}", name),
                Err(e) => println!("init: cannot start {}: {:?}", name, e),
            }
        }
    }
    if cmdline.split_whitespace().any(|a| a == "systest") {
        match init.spawn("systest", "bin/systest.exe", &[], Vec::new(), false) {
            Ok(_) => println!("init: started systest"),
            Err(e) => println!("systest: FAIL (cannot start: {:?})", e),
        }
    }
}