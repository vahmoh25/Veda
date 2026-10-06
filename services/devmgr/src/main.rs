//! `devmgr` — the device manager.
//!
//! Enumerates the PCI bus, matches devices against the driver table, starts
//! each driver with a channel that speaks the `pcidev` protocol for exactly
//! that device, and serves those channels. Drivers therefore never see other
//! devices' configuration space or memory.

#![no_std]
#![no_main]

extern crate alloc;

mod pci;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{cache_policy, signals};
use vipc::WaitSet;
use vproto::pci::{DeviceInfo, MsiAddress, PciError, pcidev};
use vrt::object::{Channel, Interrupt, IoPorts, Resource, Vmo};
use vrt::println;

use pci::{Address, ConfigSpace};

vrt::entry!(main);

/// Startup handle roles (must match `init`).
const IOPORT_RESOURCE: u32 = vabi::startup::role::USER + 1;
const IRQ_RESOURCE: u32 = vabi::startup::role::USER + 2;
const MMIO_RESOURCE: u32 = vabi::startup::role::USER + 3;
const DMA_RESOURCE: u32 = vabi::startup::role::USER + 4;
/// Role of the `pcidev` channel handed to drivers.
pub const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;

/// Which driver handles which device.
struct DriverMatch {
    vendor: u16,
    devices: &'static [u16],
    driver: &'static str,
}

const DRIVERS: &[DriverMatch] = &[
    // virtio 1.0 input (tablet, keyboard, mouse)
    DriverMatch { vendor: 0x1AF4, devices: &[0x1052], driver: "virtio-input" },
    // virtio 1.0 sound
    DriverMatch { vendor: 0x1AF4, devices: &[0x1059], driver: "virtio-snd" },
    // Intel 82801AA AC'97 audio (QEMU's AC97, VirtualBox's ICH AC97).
    DriverMatch { vendor: 0x8086, devices: &[0x2415], driver: "ac97" },
    // Intel's HD Audio controllers since Skylake: with an audio DSP beside
    // them (laptops with digital microphones, mostly) they call themselves
    // audio devices (class 04/01) rather than HD Audio controllers.
    DriverMatch {
        vendor: 0x8086,
        devices: &[
            0xA170, 0x9D70, 0xA171, 0x9D71, 0xA2F0, 0xA348, 0x9DC8, 0x02C8, 0x06C8, 0xA3F0, 0xF0C8, 0xF1C8, 0x34C8,
            0x3DC8, 0x38C8, 0x4DC8, 0xA0C8, 0x43C8, 0x4B55, 0x4B58, 0x7AD0, 0x51C8, 0x51C9, 0x51CC, 0x51CD, 0x54C8,
            0x7A50, 0x51CA, 0x51CB, 0x51CE, 0x51CF, 0x7E28, 0x7728, 0x7F50, 0x5A98, 0x3198, 0xA828, 0xE428, 0xE328,
            0x4D28, 0xD328, 0x6E50,
        ],
        driver: "hda",
    },
    // virtio block (transitional and modern)
    DriverMatch { vendor: 0x1AF4, devices: &[0x1001, 0x1042], driver: "virtio-blk" },
    // virtio network card (transitional and modern)
    DriverMatch { vendor: 0x1AF4, devices: &[0x1000, 0x1041], driver: "virtio-net" },
    // virtio console (transitional and modern): under QEMU, the virtual
    // Wi-Fi radio is a named port of it
    DriverMatch { vendor: 0x1AF4, devices: &[0x1003, 0x1043], driver: "vwifi" },
    // Intel PRO/1000: 82540EM (QEMU e1000, VirtualBox), 82545EM (VMware),
    // 82574L (QEMU e1000e)
    DriverMatch { vendor: 0x8086, devices: &[0x100E, 0x100F, 0x10D3], driver: "e1000" },
];

/// Drivers for whole device classes: (class, subclass, programming
/// interface, driver). Consulted when no vendor/device entry matches.
const CLASS_DRIVERS: &[(u8, u8, u8, &str)] = &[
    // SATA controllers in AHCI mode (VirtualBox, QEMU q35, most PCs)
    (0x01, 0x06, 0x01, "ahci"),
    // USB 3 (xHCI) controllers: USB keyboards, mice and hubs
    (0x0C, 0x03, 0x30, "xhci"),
    // High Definition Audio controllers (most PCs' sound), also the ones
    // with an audio DSP beside them
    (0x04, 0x03, 0x00, "hda"),
    (0x04, 0x03, 0x80, "hda"),
];

/// Disk drivers. A live system (started with `live`) starts none of them:
/// it runs from memory and never touches the computer's disks.
const DISK_DRIVERS: [&str; 2] = ["virtio-blk", "ahci"];

/// The driver for a device, if any.
fn driver_for(info: &DeviceInfo) -> Option<&'static str> {
    DRIVERS.iter().find(|m| m.vendor == info.vendor && m.devices.contains(&info.device)).map(|m| m.driver).or_else(
        || {
            CLASS_DRIVERS
                .iter()
                .find(|(c, s, p, _)| (*c, *s, *p) == (info.class, info.subclass, info.prog_if))
                .map(|(_, _, _, d)| *d)
        },
    )
}

fn class_name(info: &DeviceInfo) -> &'static str {
    match (info.class, info.subclass) {
        (0x01, 0x06) => "SATA controller",
        (0x01, _) => "storage controller",
        (0x02, _) => "network controller",
        (0x03, _) => "display controller",
        (0x04, _) => "multimedia device",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x09, _) => "input device",
        (0x0C, 0x03) => "USB controller",
        (0x0C, 0x05) => "SMBus controller",
        _ => "device",
    }
}

/// A device that has a driver attached.
struct Bound {
    address: Address,
    info: DeviceInfo,
    channel: Channel,
}

struct Manager {
    config: ConfigSpace,
    io: Resource,
    mmio: Resource,
    dma: Resource,
}

struct DeviceSession<'a> {
    mgr: &'a Manager,
    dev: &'a Bound,
}

impl pcidev::Server for DeviceSession<'_> {
    fn info(&mut self) -> DeviceInfo {
        self.dev.info.clone()
    }

    fn config_read(&mut self, offset: u16, width: u8) -> Result<u32, PciError> {
        if offset >= 256 || !matches!(width, 1 | 2 | 4) || !offset.is_multiple_of(width as u16) {
            return Err(PciError::BadOffset);
        }
        Ok(self.mgr.config.read(self.dev.address, offset, width))
    }

    fn config_write(&mut self, offset: u16, width: u8, value: u32) -> Result<(), PciError> {
        // BARs are owned by the firmware/devmgr; drivers may not move them.
        if offset >= 256
            || !matches!(width, 1 | 2 | 4)
            || !offset.is_multiple_of(width as u16)
            || (0x10..0x28).contains(&offset)
        {
            return Err(PciError::BadOffset);
        }
        self.mgr.config.write(self.dev.address, offset, width, value);
        Ok(())
    }

    fn map_bar(&mut self, index: u8) -> Result<Vmo, PciError> {
        let bar = self.dev.info.bars.iter().find(|b| b.index == index && !b.io).ok_or(PciError::NoSuchBar)?;
        Vmo::create_physical(&self.mgr.mmio, bar.address, bar.size as usize, cache_policy::UNCACHED)
            .map_err(|_| PciError::Denied)
    }

    fn map_io_bar(&mut self, index: u8) -> Result<IoPorts, PciError> {
        let bar = self.dev.info.bars.iter().find(|b| b.index == index && b.io).ok_or(PciError::NoSuchBar)?;
        let (base, count) = (u16::try_from(bar.address), u16::try_from(bar.size));
        let (Ok(base), Ok(count)) = (base, count) else { return Err(PciError::NoSuchBar) };
        IoPorts::create(&self.mgr.io, base, count).map_err(|_| PciError::Denied)
    }

    fn enable(&mut self, bus_master: bool) -> Result<(), PciError> {
        let cmd = self.mgr.config.read(self.dev.address, 0x04, 2);
        // I/O and memory decoding, and bus mastering if asked for.
        let new = cmd | 0b11 | if bus_master { 0b100 } else { 0 };
        self.mgr.config.write(self.dev.address, 0x04, 2, new);
        Ok(())
    }

    fn alloc_msi(&mut self) -> Result<(Interrupt, MsiAddress), PciError> {
        let (irq, info) = Interrupt::create_msi(&self.mgr.dma).map_err(|_| PciError::NoResources)?;
        Ok((irq, MsiAddress { address: info.address, data: info.data }))
    }

    fn dma_resource(&mut self) -> Result<Resource, PciError> {
        self.mgr.dma.duplicate().map_err(|_| PciError::Denied)
    }
}

/// Starts `driver` for a device, returning the server end of its channel.
/// Driver images come straight from the system image (not through the file
/// system, which may itself be waiting for a disk driver).
fn start_driver(boot: &initrd::Archive<'static>, driver: &str, info: &DeviceInfo) -> Option<Channel> {
    let path = alloc::format!("bin/{driver}.exe");
    let Some(image) = boot.find(&path).map(|f| f.data) else {
        println!("driver {} is not installed", driver);
        return None;
    };
    let registry = vproto::with_registry(|r| r.clone_registry()).ok()?.ok()?.ok()?;
    let (ours, theirs) = Channel::create().ok()?;
    let name = String::from(driver);
    let result = vrt::process::Spawn::new(&name)
        .handle(vabi::startup::role::REGISTRY, registry.into_handle())
        .handle(PCIDEV_ROLE, theirs.into_handle())
        .start(image);
    match result {
        Ok(_) => {
            println!(
                "started {} for {:04x}:{:04x} at {:02x}:{:02x}.{}",
                driver, info.vendor, info.device, info.bus, info.slot, info.function
            );
            Some(ours)
        }
        Err(e) => {
            println!("failed to start {}: {}", driver, e);
            None
        }
    }
}

/// Maps the system image (the initrd) that `init` hands over.
fn boot_image() -> Option<initrd::Archive<'static>> {
    let vmo = vrt::object::Vmo::from_handle(vrt::env::take_handle(vabi::startup::role::INITRD)?);
    let size = vmo.size().ok()?;
    let addr = vmo.map(0, size, vabi::map_flags::READ).ok()?;
    // SAFETY: a read-only mapping that lives as long as the process.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    initrd::Archive::open(bytes).ok()
}

fn main() -> i32 {
    let take = |role| vrt::env::take_handle(role).map(Resource::from_handle);
    let (Some(io), Some(_irq), Some(mmio), Some(dma)) =
        (take(IOPORT_RESOURCE), take(IRQ_RESOURCE), take(MMIO_RESOURCE), take(DMA_RESOURCE))
    else {
        println!("missing hardware resources");
        return 1;
    };
    let Ok(ports) = IoPorts::create(&io, 0xCF8, 8) else {
        println!("cannot access PCI configuration ports");
        return 1;
    };
    let Some(boot) = boot_image() else {
        println!("no system image to load drivers from");
        return 1;
    };
    let mgr = Manager { config: ConfigSpace::new(ports), io, mmio, dma };
    let live = vrt::env::args().iter().any(|a| a == "live");

    let mut bound: BTreeMap<u64, Bound> = BTreeMap::new();
    let mut next = 1u64;
    for a in mgr.config.scan() {
        let info = mgr.config.info(a);
        println!(
            "pci {:02x}:{:02x}.{} {:04x}:{:04x} {}",
            a.bus,
            a.slot,
            a.function,
            info.vendor,
            info.device,
            class_name(&info)
        );
        let Some(driver) = driver_for(&info) else {
            continue;
        };
        if live && DISK_DRIVERS.contains(&driver) {
            println!(
                "live system: {} not started for {:04x}:{:04x} at {:02x}:{:02x}.{}",
                driver, info.vendor, info.device, a.bus, a.slot, a.function
            );
            continue;
        }
        if let Some(ch) = start_driver(&boot, driver, &info) {
            bound.insert(next, Bound { address: a, info, channel: ch });
            next += 1;
        }
    }

    loop {
        let mut ws = WaitSet::new();
        for (&k, b) in &bound {
            ws.add(b.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        if ws.is_empty() {
            vrt::time::sleep(vrt::time::Duration::from_secs(3600));
            continue;
        }
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { continue };
        let mut closed: Vec<u64> = Vec::new();
        for (k, observed) in ready {
            let Some(dev) = bound.get(&k) else { continue };
            if observed & signals::READABLE != 0 {
                while let Ok(msg) = dev.channel.read() {
                    let mut s = DeviceSession { mgr: &mgr, dev };
                    if let Ok(reply) = pcidev::dispatch(&mut s, msg) {
                        let _ = reply.send(&dev.channel);
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                closed.push(k);
            }
        }
        for k in closed {
            if let Some(b) = bound.remove(&k) {
                println!("driver for {:04x}:{:04x} exited", b.info.vendor, b.info.device);
            }
        }
    }
}
