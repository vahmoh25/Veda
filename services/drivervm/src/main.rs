//! `drivervm` — the driver VM.
//!
//! Runs Linux in a virtual machine, for the drivers Veda takes from it
//! (see `docs/DRIVERVM.md`). The machine is Veda's paravirtual platform
//! (`vhv::platform`): processors with an x2APIC, memory, and hypercalls;
//! no emulated hardware. This program is its monitor: it lays out the
//! guest's memory, loads the kernel and its initial RAM file system from
//! the system image (`linux/bzImage`, `linux/initramfs.cpio`), runs the
//! virtual processors, one thread each, and serves the hypercalls.
//!
//! Programs in the guest reach Veda's objects through the bridge
//! (`vhv::bridge`): this program keeps the guest's handles and carries out
//! its operations on them, and gives it a registry narrowed to the
//! services drivers attach to.
//!
//! It gives the guest the PCI functions devmgr hands over (`pci`): their
//! DMA into the guest's memory only, their BARs, their interrupts.
//!
//! The guest's life is this process's: when Linux powers the machine off,
//! restarts it, crashes or does something the platform does not allow,
//! the process ends, and whoever started it decides what comes next.
//!
//! Arguments: `memory=MIB` (default 256), `cpus=N` (default 2),
//! `cmdline=...` (added to the kernel's command line, commas for spaces),
//! and for tests `run=PROGRAM,...` (programs of the guest's `/bin` its
//! `init` runs), `poweroff` (once they are done) and `crash=SECONDS`
//! (Linux crashes that long after it starts: devmgr passes it to the first
//! run only), `renderer=softpipe` (the guest's renderer renders on
//! softpipe, whatever the devices). With `usb=...` (the USB devices lent to
//! the guest), the guest's USB/IP host takes them.

#![no_std]
#![no_main]

extern crate alloc;

mod bridge;
mod machine;
mod memory;
mod pci;
mod registry;

use alloc::string::String;
use alloc::vec::Vec;

use vrt::object::{Channel, Resource, Vmo};
use vrt::println;

use machine::{Config, Machine};

vrt::entry!(main);

/// Startup handle role of the hypervisor resource (devmgr hands it over).
const HYPERVISOR_RESOURCE: u32 = vabi::startup::role::USER + 1;
/// Role of the `pcidev` channels of the guest's PCI functions (one each).
const DEVICE_ROLE: u32 = vabi::startup::role::USER + 2;

/// Maps the system image (the initrd), where the guest's kernel is.
fn system_image() -> Option<initrd::Archive<'static>> {
    let vmo = Vmo::from_handle(vrt::env::take_handle(vabi::startup::role::INITRD)?);
    let size = vmo.size().ok()?;
    let addr = vmo.map(0, size, vabi::map_flags::READ).ok()?;
    // SAFETY: a read-only mapping that lives as long as the process.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    initrd::Archive::open(bytes).ok()
}

fn config() -> Config {
    let mut c = Config::default();
    for arg in vrt::env::args() {
        match arg.split_once('=') {
            Some(("memory", v)) => c.memory_mib = v.parse().unwrap_or(c.memory_mib),
            Some(("cpus", v)) => c.cpus = v.parse().unwrap_or(c.cpus),
            Some(("cmdline", v)) => c.cmdline.push_str(&alloc::format!(" {}", v.replace(',', " "))),
            Some(("run", v)) => c.cmdline.push_str(&alloc::format!(" veda.run={v}")),
            Some(("crash", v)) => c.cmdline.push_str(&alloc::format!(" veda.crash={v}")),
            Some(("renderer", v)) => c.cmdline.push_str(&alloc::format!(" veda.renderer={v}")),
            Some(("usb", _)) => c.cmdline.push_str(" veda.usb"),
            None if arg == "poweroff" => c.cmdline.push_str(" veda.poweroff"),
            _ => {}
        }
    }
    c
}

fn main() -> i32 {
    let Some(hypervisor) = vrt::env::take_handle(HYPERVISOR_RESOURCE).map(Resource::from_handle) else {
        println!("no hypervisor resource");
        return 1;
    };
    let Some(image) = system_image() else {
        println!("no system image");
        return 1;
    };
    let (Some(kernel), Some(initramfs)) = (image.find("linux/bzImage"), image.find("linux/initramfs.cpio")) else {
        println!("the system image has no Linux guest");
        return 1;
    };
    let config = config();
    let devices: Vec<Channel> =
        core::iter::from_fn(|| vrt::env::take_handle(DEVICE_ROLE)).map(Channel::from_handle).collect();
    match Machine::start(&hypervisor, kernel.data, initramfs.data, devices, &config) {
        Ok(machine) => machine.wait(),
        Err(e) => {
            let e: String = e;
            println!("{}", e);
            1
        }
    }
}
