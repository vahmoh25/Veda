//! `devmgr` — the device manager.
//!
//! Enumerates the PCI bus, matches devices against the driver table, starts
//! each driver with a channel that speaks the `pcidev` protocol for exactly
//! that device, and serves those channels. Drivers therefore never see other
//! devices' configuration space or memory.
//!
//! It also reads the firmware's ACPI tables: a driver learns the devices
//! they describe below its PCI function (and only those), and the tables
//! that describe its function's hardware (an Intel audio controller's
//! NHLT), and devmgr drives the GPIO pins those devices are wired to on the
//! driver's behalf.
//!
//! Every other device goes to the driver VM, whose Linux drives it: all
//! but the disks Veda starts from, the platform's own functions, and the
//! devices Veda still has drivers for (see [`veda_keeps`]); the PC's
//! keyboard controller too, and the firmware's variables, which Linux's
//! drivers may read (`init` hands them over). The driver VM
//! gets their `pcidev` channels, through which it also gets the resource
//! that lets it give them to its guest. It starts when the machine can
//! give it devices: its processors run virtual machines, an IOMMU confines
//! the devices to the guest's memory, and the system image has its Linux.
//! `drivervm.devices=VID:DID,...` gives it devices Veda has drivers for
//! (a sound card), `drivervm` starts it even without devices (tests), and
//! `drivervm=off` keeps it off. When it ends without Linux having powered
//! it off (Linux crashed, or the monitor did), devmgr resets its devices
//! and starts it again, waiting longer each time it fails soon after
//! starting, and giving up after a few such failures.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod gpio;
mod pci;
mod placement;

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use vabi::{cache_policy, irq_flags, signals};
use vacpi::name::Path;
use vacpi::resource::Resource as FirmwareResource;
use vacpi::value::Value;
use vipc::WaitSet;
use vproto::pci::{
    AcpiDevice, AcpiResource, DeviceInfo, InterruptLine, IntxLine, MsiAddress, PciError, ReservedMemory, pcidev,
};
use vrt::object::{Channel, Interrupt, IoPorts, Process, Resource, Vmo};
use vrt::println;

use acpi::Acpi;
use gpio::Gpio;
use pci::{Address, ConfigSpace, Placement};

vrt::entry!(main);

/// Startup handle roles (must match `init`).
const IOPORT_RESOURCE: u32 = vabi::startup::role::USER + 1;
const IRQ_RESOURCE: u32 = vabi::startup::role::USER + 2;
const MMIO_RESOURCE: u32 = vabi::startup::role::USER + 3;
const DMA_RESOURCE: u32 = vabi::startup::role::USER + 4;
const HYPERVISOR_RESOURCE: u32 = vabi::startup::role::USER + 5;
const PCI_RESOURCE: u32 = vabi::startup::role::USER + 6;
/// Role of the `pcidev` channel handed to drivers.
pub const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
/// Role of the hypervisor resource handed to the driver VM.
const DRIVERVM_HYPERVISOR_ROLE: u32 = vabi::startup::role::USER + 1;
/// Role of the `pcidev` channels of the devices the driver VM gets (one
/// each).
const DRIVERVM_DEVICE_ROLE: u32 = vabi::startup::role::USER + 2;
/// Roles of the keyboard controller's data port, its command port, and its
/// keyboard's and mouse's interrupts, which the driver VM gets.
const DRIVERVM_I8042_ROLES: [u32; 4] = [
    vabi::startup::role::USER + 3,
    vabi::startup::role::USER + 4,
    vabi::startup::role::USER + 5,
    vabi::startup::role::USER + 6,
];
/// Role of the firmware's variables (`init` hands them over), which the
/// driver VM gets.
const DRIVERVM_VARIABLES_ROLE: u32 = vabi::startup::role::USER + 7;

/// Which driver handles which device.
struct DriverMatch {
    vendor: u16,
    devices: &'static [u16],
    driver: &'static str,
}

const DRIVERS: &[DriverMatch] = &[
    // virtio block (transitional and modern)
    DriverMatch { vendor: 0x1AF4, devices: &[0x1001, 0x1042], driver: "virtio-blk" },
];

/// Drivers for whole device classes: (class, subclass, programming
/// interface, driver). Consulted when no vendor/device entry matches.
const CLASS_DRIVERS: &[(u8, u8, u8, &str)] = &[
    // SATA controllers in AHCI mode (QEMU q35, most PCs)
    (0x01, 0x06, 0x01, "ahci"),
    // NVM Express controllers (the SSDs of most PCs since about 2016)
    (0x01, 0x08, 0x02, "nvme"),
];

/// Disk drivers. A live system (started with `live`) starts none of them:
/// it runs from memory and never touches the computer's disks.
const DISK_DRIVERS: [&str; 3] = ["virtio-blk", "ahci", "nvme"];

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

/// Whether Veda keeps a function rather than give it to the driver VM: the
/// disks it starts from (storage controllers, whether it drives them yet
/// or not: they are none of Linux's business), the platform's own (memory
/// controllers, bridges, system peripherals, processors, encryption and
/// signal processing controllers, the SMBus, and the controller of the
/// firmware's flash: the serial bus controller in device 31 of an Intel
/// PCH), and the devices it has drivers of its own for. Sound (HD Audio
/// controllers, with an audio DSP beside them or not) and the other
/// serial bus controllers (a laptop's I2C controllers with its touchpad,
/// its SPI controller with its speaker amplifiers) are Linux's.
fn veda_keeps(info: &DeviceInfo) -> bool {
    let flash = info.class == 0x0C && info.subclass == 0x80 && info.bus == 0 && info.slot == 0x1F;
    matches!(info.class, 0x01 | 0x05 | 0x06 | 0x08 | 0x0B | 0x10 | 0x11)
        || (info.class == 0x0C && info.subclass == 0x05)
        || flash
        || driver_for(info).is_some()
}

fn class_name(info: &DeviceInfo) -> &'static str {
    match (info.class, info.subclass) {
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x08) => "NVM Express controller",
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
    /// Where the firmware placed it.
    placement: Placement,
    /// Its ACPI companion, and the devices the firmware describes below
    /// it.
    companion: Option<Path>,
    acpi: Vec<vacpi::device::Described>,
    /// The driver VM has it.
    guest: bool,
}

/// A function the driver VM gets, with what the firmware describes below
/// it.
struct GuestDevice {
    address: Address,
    info: DeviceInfo,
    placement: Placement,
    companion: Option<Path>,
    acpi: Vec<vacpi::device::Described>,
}

struct Manager {
    /// Shared with the ACPI interpreter, which reads it for the firmware.
    config: Rc<ConfigSpace>,
    io: Resource,
    irq: Resource,
    mmio: Resource,
    dma: Resource,
    pci: Resource,
    acpi: Option<Acpi>,
    gpio: RefCell<Gpio>,
    /// The lines functions' INTx are wired to, by GSI: one interrupt each,
    /// which every function on the line shares.
    lines: RefCell<BTreeMap<u32, Interrupt>>,
}

/// `A` for INTA# (pin 1) to `D`.
fn intx_letter(pin: u8) -> char {
    (b'A' + pin.saturating_sub(1)) as char
}

/// A resource of the device at `scope`, as the `pcidev` protocol carries
/// it: the controllers of its connections resolved where they can be.
fn wire(r: &FirmwareResource, acpi: &Acpi, scope: &Path) -> AcpiResource {
    let controller = |name: &str| {
        let resolved = vacpi::name::NameString::parse(name).and_then(|n| acpi.ns.resolve(scope, &n));
        resolved.map_or_else(|| String::from(name), |p| alloc::format!("{}", p))
    };
    match r {
        FirmwareResource::Memory { base, length, .. } => AcpiResource::Memory { base: *base, length: *length },
        FirmwareResource::Io { base, length } => AcpiResource::Io { base: *base, length: *length },
        FirmwareResource::Irq { irqs, edge, active_low, shared, .. } => {
            AcpiResource::Irq { irqs: irqs.clone(), edge: *edge, active_low: *active_low, shared: *shared }
        }
        FirmwareResource::Gpio(g) => AcpiResource::Gpio {
            interrupt: g.interrupt,
            pins: g.pins.clone(),
            controller: controller(&g.controller),
            pull: g.pull,
            restriction: g.restriction,
            shared: g.shared,
            edge: g.edge,
            polarity: g.polarity,
            wake: g.wake,
            debounce: g.debounce,
        },
        FirmwareResource::Spi(s) => AcpiResource::Spi {
            controller: controller(&s.controller),
            chip_select: s.chip_select,
            speed_hz: s.speed_hz,
            bits: s.bits,
            cpol: s.cpol,
            cpha: s.cpha,
            cs_active_high: s.cs_active_high,
        },
        FirmwareResource::I2c(i) => AcpiResource::I2c {
            controller: controller(&i.controller),
            address: i.address,
            speed_hz: i.speed_hz,
            ten_bit: i.ten_bit,
        },
        FirmwareResource::Window { .. } => AcpiResource::Other { kind: 0x87 },
        FirmwareResource::Other { kind } => AcpiResource::Other { kind: *kind },
    }
}

struct DeviceSession<'a> {
    mgr: &'a Manager,
    dev: &'a Bound,
    /// The key the session's channel is waited on with, which owns what
    /// it is given that goes when it does (its pins' interrupts).
    key: u64,
}

impl DeviceSession<'_> {
    /// The interrupt of line `gsi`: the one the lines' table has, or a
    /// new one, signalling as given.
    fn line(&self, gsi: u32, level: bool, active_low: bool) -> Result<Interrupt, PciError> {
        let mut lines = self.mgr.lines.borrow_mut();
        let irq = match lines.entry(gsi) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let flags =
                    if level { irq_flags::LEVEL } else { 0 } | if active_low { irq_flags::ACTIVE_LOW } else { 0 };
                e.insert(Interrupt::create(&self.mgr.irq, gsi as usize, flags).map_err(|_| PciError::NoResources)?)
            }
        };
        irq.0.duplicate(None).map(Interrupt).map_err(|_| PciError::NoResources)
    }
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
        // BARs (and the expansion ROM's) are owned by the firmware/devmgr;
        // drivers may not move them.
        if offset >= 256
            || !matches!(width, 1 | 2 | 4)
            || !offset.is_multiple_of(width as u16)
            || (0x10..0x28).contains(&offset)
            || (0x30..0x34).contains(&offset)
        {
            return Err(PciError::BadOffset);
        }
        let a = self.dev.address;
        if self.mgr.config.driver_write(a, &self.dev.placement, offset, width, value) {
            println!("{:02x}:{:02x}.{} woke from D3hot without its BARs: put back", a.bus, a.slot, a.function);
        }
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
        let (irq, info) =
            Interrupt::create_msi(&self.mgr.pci, self.dev.address.requester_id()).map_err(|_| PciError::NoResources)?;
        Ok((irq, MsiAddress { address: info.address, data: info.data }))
    }

    fn dma_resource(&mut self) -> Result<Resource, PciError> {
        self.mgr.dma.duplicate().map_err(|_| PciError::Denied)
    }

    fn intx(&mut self) -> Result<(Interrupt, IntxLine), PciError> {
        let (a, pin) = (self.dev.address, self.dev.info.irq_pin);
        if !(1..=4).contains(&pin) {
            return Err(PciError::NotFound);
        }
        let route = self.mgr.acpi.as_ref().and_then(|acpi| acpi.pci_interrupt(a.bus, a.slot, pin));
        let Some(route) = route else {
            println!(
                "{:02x}:{:02x}.{}: the firmware does not say where INT{}# goes",
                a.bus,
                a.slot,
                a.function,
                intx_letter(pin)
            );
            return Err(PciError::NotFound);
        };
        let line = IntxLine { pin, gsi: route.gsi, level: route.level, active_low: route.active_low };
        let irq = self.line(route.gsi, route.level, route.active_low)?;
        println!(
            "{:02x}:{:02x}.{}: INT{}# on GSI {} ({}, active {})",
            a.bus,
            a.slot,
            a.function,
            intx_letter(pin),
            route.gsi,
            if route.level { "level" } else { "edge" },
            if route.active_low { "low" } else { "high" }
        );
        Ok((irq, line))
    }

    fn device_resource(&mut self) -> Result<Resource, PciError> {
        if !self.dev.guest {
            return Err(PciError::Denied);
        }
        let sid = self.dev.address.requester_id() as u64;
        self.mgr.pci.create(vabi::resource_kind::PCI, sid, 1).map_err(|_| PciError::Denied)
    }

    fn reserved_memory(&mut self) -> Result<Vec<ReservedMemory>, PciError> {
        if !self.dev.guest {
            return Err(PciError::Denied);
        }
        let Some(acpi) = self.mgr.acpi.as_ref() else { return Ok(Vec::new()) };
        let a = self.dev.address;
        acpi.reserved_memory(a.bus, a.slot, a.function)
            .into_iter()
            .map(|r| {
                let size = r.end - r.start;
                let memory = Vmo::create_physical(&self.mgr.mmio, r.start, size as usize, cache_policy::UNCACHED)
                    .map_err(|_| PciError::NoResources)?;
                Ok(ReservedMemory { base: r.start, size, memory })
            })
            .collect()
    }

    fn acpi_tables(&mut self) -> Vec<Vec<u8>> {
        self.mgr.acpi.as_ref().map(|a| a.device_tables(&self.dev.info)).unwrap_or_default()
    }

    fn acpi_devices(&mut self) -> Vec<AcpiDevice> {
        let Some(acpi) = self.mgr.acpi.as_ref() else { return Vec::new() };
        self.dev
            .acpi
            .iter()
            .map(|d| AcpiDevice {
                path: alloc::format!("{}", d.path),
                hid: d.identity.hid.clone().unwrap_or_default(),
                cids: d.identity.cids.clone(),
                uid: d.identity.uid.clone().unwrap_or_default(),
                sub: d.identity.sub.clone().unwrap_or_default(),
                status: d.identity.status as u32,
                resources: d
                    .resources
                    .as_ref()
                    .map(|r| r.iter().map(|r| wire(r, acpi, &d.path)).collect())
                    .unwrap_or_default(),
                resource_error: d.resources.as_ref().err().map(|e| alloc::format!("{}", e)).unwrap_or_default(),
            })
            .collect()
    }

    fn acpi_dsm(&mut self, device: u32, uuid: Vec<u8>, revision: u64, function: u64) -> Result<u64, PciError> {
        let acpi = self.mgr.acpi.as_ref().ok_or(PciError::NotFound)?;
        let d = self.dev.acpi.get(device as usize).ok_or(PciError::NotFound)?;
        let dsm = d.path.join("_DSM").ok_or(PciError::NotFound)?;
        let args =
            [Value::Buffer(uuid), Value::Integer(revision), Value::Integer(function), Value::Package(Vec::new())];
        let answer = acpi.ns.evaluate(&dsm, &args, &acpi.memory).map_err(|_| PciError::NotFound)?;
        match answer {
            Value::Integer(v) => Ok(v),
            _ => Err(PciError::NotFound),
        }
    }

    fn acpi_data(&mut self, device: u32, name: String) -> Result<Vec<u8>, PciError> {
        let acpi = self.mgr.acpi.as_ref().ok_or(PciError::NotFound)?;
        let owner = match device {
            u32::MAX => self.dev.companion.as_ref(),
            d => self.dev.acpi.get(d as usize).map(|d| &d.path),
        };
        let owner = owner.ok_or(PciError::NotFound)?;
        let value = acpi.ns.evaluate_child(owner, &name, &acpi.memory).ok_or(PciError::NotFound)?;
        let value = value.map_err(|_| PciError::Unsupported)?;
        vacpi::asm::constant(&value).ok_or(PciError::Unsupported)
    }

    fn acpi_interrupt(&mut self, device: u32, index: u32) -> Result<(Interrupt, InterruptLine), PciError> {
        let d = self.dev.acpi.get(device as usize).ok_or(PciError::NotFound)?;
        let line = d
            .resources
            .as_ref()
            .ok()
            .and_then(|r| {
                r.iter()
                    .flat_map(|r| match r {
                        FirmwareResource::Irq { irqs, edge, active_low, .. } => irqs
                            .iter()
                            .map(|&gsi| InterruptLine { gsi, level: !*edge, active_low: *active_low })
                            .collect(),
                        _ => Vec::new(),
                    })
                    .nth(index as usize)
            })
            .ok_or(PciError::NotFound)?;
        println!(
            "{}: interrupt {} on GSI {} ({}, active {})",
            d.path,
            index,
            line.gsi,
            if line.level { "level" } else { "edge" },
            if line.active_low { "low" } else { "high" }
        );
        Ok((self.line(line.gsi, line.level, line.active_low)?, line))
    }

    fn gpio_read(&mut self, device: u32, index: u32) -> Result<bool, PciError> {
        let (acpi, controller, pin, _) = self.gpio_pin(device, index)?;
        self.mgr.gpio.borrow_mut().read(acpi, &controller, pin)
    }

    fn gpio_write(&mut self, device: u32, index: u32, high: bool) -> Result<(), PciError> {
        let (acpi, controller, pin, _) = self.gpio_pin(device, index)?;
        self.mgr.gpio.borrow_mut().write(acpi, &controller, pin, high)
    }

    fn gpio_input(&mut self, device: u32, index: u32) -> Result<(), PciError> {
        let (acpi, controller, pin, _) = self.gpio_pin(device, index)?;
        self.mgr.gpio.borrow_mut().input(acpi, &controller, pin)
    }

    fn gpio_is_output(&mut self, device: u32, index: u32) -> Result<bool, PciError> {
        let (acpi, controller, pin, _) = self.gpio_pin(device, index)?;
        self.mgr.gpio.borrow_mut().is_output(acpi, &controller, pin)
    }

    fn gpio_interrupt(&mut self, device: u32, index: u32) -> Result<Interrupt, PciError> {
        let (acpi, controller, pin, connection) = self.gpio_pin(device, index)?;
        if !connection.interrupt {
            return Err(PciError::NotFound);
        }
        let trigger = gpio::trigger(&connection);
        let line = |gsi, level, active_low| self.line(gsi, level, active_low);
        self.mgr.gpio.borrow_mut().interrupt(acpi, &controller, pin, trigger, self.key, line)
    }
}

impl DeviceSession<'_> {
    /// GPIO pin `index` of ACPI device `device` below this driver's PCI
    /// function: its controller, the pin, and the connection it is of.
    fn gpio_pin(&self, device: u32, index: u32) -> Result<(&Acpi, Path, u16, vacpi::resource::Gpio), PciError> {
        let acpi = self.mgr.acpi.as_ref().ok_or(PciError::NotFound)?;
        let d = self.dev.acpi.get(device as usize).ok_or(PciError::NotFound)?;
        let (controller, pin, connection) = d.gpio(&acpi.ns, index as usize).ok_or(PciError::NotFound)?;
        Ok((acpi, controller, pin, connection.clone()))
    }
}

/// Starts `driver` for a device, returning the server end of its channel.
/// Driver images come straight from the system image (not through the file
/// system, which may itself be waiting for a disk driver).
fn start_driver(boot: &initrd::Archive<'static>, driver: &str, info: &DeviceInfo) -> Option<Channel> {
    let (ours, theirs) = Channel::create().ok()?;
    start_program(boot, driver, &[], alloc::vec![(PCIDEV_ROLE, theirs.into_handle())])?;
    println!(
        "started {} for {:04x}:{:04x} at {:02x}:{:02x}.{}",
        driver, info.vendor, info.device, info.bus, info.slot, info.function
    );
    Some(ours)
}

/// Starts `bin/NAME.exe` from the system image with the registry and
/// `handles` (by role): the process, if it started.
fn start_program(
    boot: &initrd::Archive<'static>,
    name: &str,
    args: &[String],
    handles: Vec<(u32, vrt::object::Handle)>,
) -> Option<Process> {
    let path = alloc::format!("bin/{name}.exe");
    let Some(image) = boot.find(&path).map(|f| f.data) else {
        println!("{} is not installed", name);
        return None;
    };
    let registry = vproto::with_registry(|r| r.clone_registry()).ok()?.ok()?.ok()?;
    let mut spawn = vrt::process::Spawn::new(name).handle(vabi::startup::role::REGISTRY, registry.into_handle());
    for (role, h) in handles {
        spawn = spawn.handle(role, h);
    }
    for a in args {
        spawn = spawn.arg(a);
    }
    match spawn.start(image) {
        Ok(process) => Some(process),
        Err(e) => {
            println!("failed to start {}: {}", name, e);
            None
        }
    }
}

/// The boot information `init` hands over (where the ACPI tables are).
fn boot_info() -> Option<vabi::KernelBootInfo> {
    let vmo = Vmo::from_handle(vrt::env::take_handle(vabi::startup::role::BOOT_INFO)?);
    let mut buf = [0u8; core::mem::size_of::<vabi::KernelBootInfo>()];
    vmo.read(0, &mut buf).ok()?;
    // SAFETY: KernelBootInfo is plain old data written by the kernel.
    Some(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const vabi::KernelBootInfo) })
}

/// The devices the firmware describes below PCI function `a`; logs them.
fn described_below(acpi: Option<&Acpi>, a: Address) -> (Option<Path>, Vec<vacpi::device::Described>) {
    let Some(acpi) = acpi else { return (None, Vec::new()) };
    let Some(companion) = acpi.pci_companion(a.bus, a.slot, a.function) else { return (None, Vec::new()) };
    let below = acpi.children(&companion);
    if !below.is_empty() {
        let names: Vec<String> = below
            .iter()
            .map(|d| alloc::format!("{} ({})", d.path, d.identity.hid.as_deref().unwrap_or("no id")))
            .collect();
        println!("acpi: {:02x}:{:02x}.{} is {}, with {}", a.bus, a.slot, a.function, companion, names.join(", "));
    }
    (Some(companion), below)
}

/// Maps the system image (the initrd) that `init` hands over; also
/// returns it, for the driver VM.
fn boot_image() -> Option<(initrd::Archive<'static>, Vmo)> {
    let vmo = vrt::object::Vmo::from_handle(vrt::env::take_handle(vabi::startup::role::INITRD)?);
    let size = vmo.size().ok()?;
    let addr = vmo.map(0, size, vabi::map_flags::READ).ok()?;
    // SAFETY: a read-only mapping that lives as long as the process.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    Some((initrd::Archive::open(bytes).ok()?, vmo))
}

/// The PC's keyboard controller (the i8042), which the driver VM gets: its
/// data port, its command port (the ports around them stay Veda's), and
/// its keyboard's and mouse's interrupts (ISA 1 and 12). It does no DMA,
/// so it needs no IOMMU; the driver VM's monitor keeps it from resetting
/// the machine.
struct I8042 {
    data: IoPorts,
    command: IoPorts,
    keyboard: Interrupt,
    mouse: Interrupt,
}

impl I8042 {
    fn claim(io: &Resource, irq: &Resource) -> Option<I8042> {
        Some(I8042 {
            data: IoPorts::create(io, 0x60, 1).ok()?,
            command: IoPorts::create(io, 0x64, 1).ok()?,
            keyboard: Interrupt::create(irq, 1, irq_flags::ISA).ok()?,
            mouse: Interrupt::create(irq, 12, irq_flags::ISA).ok()?,
        })
    }

    /// Copies of its handles, by role, for a run of the driver VM.
    fn handles(&self) -> Option<Vec<(u32, vrt::object::Handle)>> {
        let handles = [
            self.data.0.duplicate(None),
            self.command.0.duplicate(None),
            self.keyboard.0.duplicate(None),
            self.mouse.0.duplicate(None),
        ];
        DRIVERVM_I8042_ROLES.into_iter().zip(handles).map(|(role, h)| Some((role, h.ok()?))).collect()
    }
}

/// A run of the driver VM that ends sooner than this after it started is
/// a failure; the next start waits longer after each in a row, and stops
/// after a few.
const DRIVERVM_STABLE_NS: u64 = 60_000_000_000;
const DRIVERVM_MAX_FAILURES: u32 = 5;
/// drivervm's exit code when Linux powered the machine off.
const DRIVERVM_POWERED_OFF: i64 = 0;

/// The driver VM: the devices it gets, and its process, which is started
/// again when it ends without Linux having powered the machine off.
struct DriverVm {
    /// Its options: `drivervm.NAME=VALUE` (`drivervm.memory=512`) but
    /// `devices`, which is devmgr's.
    args: Vec<String>,
    devices: Vec<GuestDevice>,
    i8042: Option<I8042>,
    /// The firmware's variables, for Linux's drivers that read them.
    variables: Option<Vmo>,
    hypervisor: Resource,
    process: Option<Process>,
    /// How many times it started, when it last did, the failures in a row
    /// (runs that ended soon), and when to start it next.
    starts: u32,
    started_ns: u64,
    failures: u32,
    next_start_ns: Option<u64>,
}

impl DriverVm {
    fn new(
        options: &[String],
        devices: Vec<GuestDevice>,
        i8042: Option<I8042>,
        variables: Option<Vmo>,
        hypervisor: Resource,
    ) -> DriverVm {
        let args = options
            .iter()
            .filter_map(|a| a.strip_prefix("drivervm."))
            .filter(|a| !a.starts_with("devices="))
            .map(Into::into)
            .collect();
        DriverVm {
            args,
            devices,
            i8042,
            variables,
            hypervisor,
            process: None,
            starts: 0,
            started_ns: 0,
            failures: 0,
            next_start_ns: Some(0),
        }
    }

    /// Starts it, with what it runs on: the hypervisor resource, the system
    /// image, which holds its Linux, new channels of its devices (which
    /// devmgr serves as `bound`'s), the keyboard controller, and the
    /// firmware's variables.
    fn start(
        &mut self,
        boot: &initrd::Archive<'static>,
        image: &Vmo,
        bound: &mut BTreeMap<u64, Bound>,
        next: &mut u64,
    ) {
        self.next_start_ns = None;
        let (Ok(h), Ok(i)) = (self.hypervisor.duplicate(), image.0.duplicate(None)) else { return };
        let mut handles = alloc::vec![(DRIVERVM_HYPERVISOR_ROLE, h.into_handle()), (vabi::startup::role::INITRD, i)];
        handles.extend(self.i8042.as_ref().and_then(I8042::handles).unwrap_or_default());
        if let Some(variables) = self.variables.as_ref().and_then(|v| v.0.duplicate(None).ok()) {
            handles.push((DRIVERVM_VARIABLES_ROLE, variables));
        }
        for d in &self.devices {
            let Ok((ours, theirs)) = Channel::create() else { continue };
            handles.push((DRIVERVM_DEVICE_ROLE, theirs.into_handle()));
            let device = Bound {
                address: d.address,
                info: d.info.clone(),
                placement: d.placement,
                channel: ours,
                companion: d.companion.clone(),
                acpi: d.acpi.clone(),
                guest: true,
            };
            bound.insert(*next, device);
            *next += 1;
        }
        // A crash the tests ask for (`drivervm.crash=SECONDS`) is the first
        // run's.
        let args: Vec<String> =
            self.args.iter().filter(|a| self.starts == 0 || !a.starts_with("crash=")).cloned().collect();
        self.process = start_program(boot, "drivervm", &args, handles);
        if self.process.is_some() {
            self.starts += 1;
            self.started_ns = vrt::time::now_ns();
            let mut what = String::from(if self.starts > 1 { "started drivervm again" } else { "started drivervm" });
            for a in &args {
                what.push(' ');
                what.push_str(a);
            }
            match self.devices.len() {
                0 => println!("{}", what),
                n => println!("{} with {} device(s)", what, n),
            }
        }
    }

    /// Whether its monitor runs: the kernel says a process has ended
    /// before it closes its handles, the channels of its devices among
    /// them.
    fn running(&self) -> bool {
        self.process.as_ref().and_then(|p| p.info().ok()).is_some_and(|i| i.state == vabi::process_state::RUNNING)
    }

    /// Its monitor did not take the device at `address` (and said why): it
    /// stays the host's, neither reset when the driver VM ends nor given to
    /// the next.
    fn turned_down(&mut self, address: Address) {
        self.devices.retain(|d| d.address != address);
    }

    /// Its process ended: unless Linux powered it off, its devices are
    /// reset and it starts again (later, the sooner it failed).
    fn ended(&mut self, config: &ConfigSpace) {
        let Some(process) = self.process.take() else { return };
        let info = process.info().ok();
        let why = match info.map(|i| (i.state, i.exit_code)) {
            Some((vabi::process_state::EXITED, DRIVERVM_POWERED_OFF)) => {
                println!("Linux powered the driver VM off");
                return;
            }
            Some((vabi::process_state::EXITED, 2)) => String::from("Linux restarted it"),
            Some((vabi::process_state::EXITED, 3)) => String::from("Linux crashed"),
            Some((vabi::process_state::EXITED, code)) => alloc::format!("the monitor stopped it ({code})"),
            Some((vabi::process_state::KILLED, _)) => String::from("the monitor was killed"),
            _ => String::from("the monitor crashed"),
        };
        let ran_ns = vrt::time::now_ns().saturating_sub(self.started_ns);
        self.failures = if ran_ns < DRIVERVM_STABLE_NS { self.failures + 1 } else { 0 };
        let mut resets = Vec::new();
        for d in &self.devices {
            let how = config.reset(d.address).unwrap_or("no reset");
            resets.push(alloc::format!("{:04x}:{:04x} {}", d.info.vendor, d.info.device, how));
        }
        if resets.is_empty() {
            println!("the driver VM ended: {}", why);
        } else {
            println!("the driver VM ended: {}; its devices: {}", why, resets.join(", "));
        }
        if self.failures > DRIVERVM_MAX_FAILURES {
            println!("the driver VM keeps failing: its devices stay off");
            return;
        }
        // At once the first time, then 1, 2, 4... seconds.
        let wait_ns = match self.failures {
            0 | 1 => 0,
            n => 1_000_000_000u64 << (n - 2).min(5),
        };
        self.next_start_ns = Some(vrt::time::now_ns() + wait_ns);
    }
}

/// The devices an option lists (`option` as `drivervm.devices=`, then
/// `VID:DID,...`).
fn listed_devices(options: &[String], option: &str) -> Vec<(u16, u16)> {
    let id = |s: &str| u16::from_str_radix(s, 16).ok();
    options
        .iter()
        .filter_map(|a| a.strip_prefix(option))
        .flat_map(|list| list.split(','))
        .filter_map(|d| d.split_once(':').and_then(|(v, d)| Some((id(v)?, id(d)?))))
        .collect()
}

fn main() -> i32 {
    let take = |role| vrt::env::take_handle(role).map(Resource::from_handle);
    let (Some(io), Some(irq), Some(mmio), Some(dma), Some(pci)) =
        (take(IOPORT_RESOURCE), take(IRQ_RESOURCE), take(MMIO_RESOURCE), take(DMA_RESOURCE), take(PCI_RESOURCE))
    else {
        println!("missing hardware resources");
        return 1;
    };
    let Ok(ports) = IoPorts::create(&io, 0xCF8, 8) else {
        println!("cannot access PCI configuration ports");
        return 1;
    };
    let Some((boot, image)) = boot_image() else {
        println!("no system image to load drivers from");
        return 1;
    };
    let config = Rc::new(ConfigSpace::new(ports));
    let machine = boot_info();
    let acpi = machine.as_ref().and_then(|m| Acpi::load(&mmio, m, config.clone()));
    let platform = machine.map_or(0, |m| m.platform);
    let Ok(gpio_mmio) = mmio.duplicate() else {
        println!("cannot keep the MMIO resource for GPIO");
        return 1;
    };
    let mgr = Manager {
        config,
        io,
        irq,
        mmio,
        dma,
        pci,
        acpi,
        gpio: RefCell::new(Gpio::new(gpio_mmio)),
        lines: RefCell::new(BTreeMap::new()),
    };
    let args = vrt::env::args();
    let live = args.iter().any(|a| a == "live");
    // The driver VM's options: `drivervm`, `drivervm=off`, and its own as
    // `drivervm.NAME=VALUE`.
    let drivervm: Vec<String> = args
        .iter()
        .filter(|a| *a == "drivervm" || a.starts_with("drivervm.") || a.starts_with("drivervm="))
        .cloned()
        .collect();
    let linux = boot.find("linux/bzImage").is_some() && boot.find("linux/initramfs.cpio").is_some();
    let runs = if drivervm.iter().any(|a| a == "drivervm=off") {
        Err("drivervm=off")
    } else if platform & vabi::platform::VIRTUALIZATION == 0 {
        Err("the processors cannot run virtual machines")
    } else if !linux {
        Err("the system image has no Linux for it")
    } else {
        Ok(())
    };
    // Devices go to it only where an IOMMU confines them to its memory.
    let gives = runs.is_ok() && platform & vabi::platform::IOMMU != 0;
    match runs {
        Err(why) => println!("no driver VM ({}): the devices Veda has no driver for stay off", why),
        Ok(()) if !gives => println!("no IOMMU: no device can go to the driver VM"),
        Ok(()) => {}
    }
    let listed = listed_devices(&drivervm, "drivervm.devices=");
    let mut guest = Vec::new();

    let functions = mgr.config.scan();
    // Tests: these functions' BARs are taken as a firmware that leaves them
    // unplaced does (`devmgr.unplaced=VID:DID,...`).
    let unplaced = listed_devices(&args, "devmgr.unplaced=");
    for &a in &functions {
        let info = mgr.config.info(a);
        if unplaced.contains(&(info.vendor, info.device)) {
            mgr.config.forget_placement(a);
        }
    }
    placement::place(&mgr.config, &functions, mgr.acpi.as_ref());

    let mut bound: BTreeMap<u64, Bound> = BTreeMap::new();
    let mut next = 1u64;
    for a in functions {
        let info = mgr.config.info(a);
        let companion = mgr.acpi.as_ref().and_then(|acpi| acpi.pci_companion(a.bus, a.slot, a.function));
        println!(
            "pci {:02x}:{:02x}.{} {:04x}:{:04x} {}{}",
            a.bus,
            a.slot,
            a.function,
            info.vendor,
            info.device,
            class_name(&info),
            companion.map(|c| alloc::format!(" ({})", c)).unwrap_or_default()
        );
        if gives && (!veda_keeps(&info) || listed.contains(&(info.vendor, info.device))) {
            // An endpoint only: a bridge would give the guest what is
            // behind it. (Its I/O BARs the guest does not get.)
            if mgr.config.read(a, 0x0E, 1) & 0x7F != 0 {
                println!("{:04x}:{:04x} cannot go to the driver VM: not an endpoint", info.vendor, info.device);
                continue;
            }
            println!(
                "{:04x}:{:04x} at {:02x}:{:02x}.{} goes to the driver VM",
                info.vendor, info.device, a.bus, a.slot, a.function
            );
            if let Some(state) = mgr.config.power_state(a).filter(|&s| s != 0) {
                println!("{:02x}:{:02x}.{} is in D{}: the guest wakes it", a.bus, a.slot, a.function, state);
            }
            let (companion, acpi) = described_below(mgr.acpi.as_ref(), a);
            let placement = mgr.config.placement(a);
            guest.push(GuestDevice { address: a, info, placement, companion, acpi });
            continue;
        }
        let Some(driver) = driver_for(&info) else { continue };
        if live && DISK_DRIVERS.contains(&driver) {
            println!(
                "live system: {} not started for {:04x}:{:04x} at {:02x}:{:02x}.{}",
                driver, info.vendor, info.device, a.bus, a.slot, a.function
            );
            continue;
        }
        let (companion, below) = described_below(mgr.acpi.as_ref(), a);
        if let Some(ch) = start_driver(&boot, driver, &info) {
            let placement = mgr.config.placement(a);
            bound
                .insert(next, Bound { address: a, info, placement, channel: ch, companion, acpi: below, guest: false });
            next += 1;
        }
    }

    // The keyboard controller goes too, whose ports and interrupts are
    // the PC's whether or not anything answers there.
    let i8042 = if runs.is_ok() { I8042::claim(&mgr.io, &mgr.irq) } else { None };
    // It starts for its devices, or because the boot options ask for it.
    let mut vm = None;
    if runs.is_ok() && (!guest.is_empty() || i8042.is_some() || drivervm.iter().any(|a| a == "drivervm")) {
        match take(HYPERVISOR_RESOURCE) {
            Some(hypervisor) => {
                let variables = vrt::env::take_handle(vabi::startup::role::FIRMWARE_VARIABLES).map(Vmo::from_handle);
                vm = Some(DriverVm::new(&drivervm, guest, i8042, variables, hypervisor));
            }
            None => println!("no hypervisor resource for the driver VM"),
        }
    }

    /// The driver VM's process, among the channels.
    const DRIVERVM: u64 = u64::MAX;
    loop {
        if let Some(vm) = vm.as_mut()
            && vm.next_start_ns.is_some_and(|at| vrt::time::now_ns() >= at)
        {
            vm.start(&boot, &image, &mut bound, &mut next);
        }
        let mut ws = WaitSet::new();
        for (&k, b) in &bound {
            ws.add(b.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        for (handle, wanted, k) in mgr.gpio.borrow().waits() {
            ws.add(handle, wanted, k);
        }
        let mut deadline = vabi::DEADLINE_INFINITE;
        if let Some(vm) = &vm {
            if let Some(p) = &vm.process {
                ws.add(p.raw(), signals::TERMINATED, DRIVERVM);
            }
            if let Some(at) = vm.next_start_ns {
                deadline = at;
            }
        }
        if ws.is_empty() && deadline == vabi::DEADLINE_INFINITE {
            vrt::time::sleep(vrt::time::Duration::from_secs(3600));
            continue;
        }
        let Ok(ready) = ws.wait(deadline) else { continue };
        let mut closed: Vec<u64> = Vec::new();
        for (k, observed) in ready {
            if k == DRIVERVM {
                if let Some(vm) = vm.as_mut() {
                    vm.ended(&mgr.config);
                }
                continue;
            }
            if k & gpio::KEYS != 0 {
                mgr.gpio.borrow_mut().signalled(k);
                continue;
            }
            let Some(dev) = bound.get(&k) else { continue };
            if observed & signals::READABLE != 0 {
                while let Ok(msg) = dev.channel.read() {
                    let mut s = DeviceSession { mgr: &mgr, dev, key: k };
                    if let Ok(reply) = pcidev::dispatch(&mut s, msg) {
                        let _ = reply.send(&dev.channel);
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                closed.push(k);
            }
        }
        for k in closed {
            mgr.gpio.borrow_mut().release(k);
            if let Some(b) = bound.remove(&k) {
                if b.guest {
                    match vm.as_mut() {
                        Some(vm) if vm.running() => {
                            vm.turned_down(b.address);
                            println!(
                                "the driver VM did not take {:04x}:{:04x}; it stays the host's",
                                b.info.vendor, b.info.device
                            );
                        }
                        _ => println!(
                            "the driver VM let go of {:04x}:{:04x}; it reaches nothing",
                            b.info.vendor, b.info.device
                        ),
                    }
                } else {
                    println!("driver for {:04x}:{:04x} exited", b.info.vendor, b.info.device);
                }
            }
        }
    }
}
