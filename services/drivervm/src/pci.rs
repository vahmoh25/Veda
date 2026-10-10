//! The PCI functions the guest gets. devmgr hands each over as a `pcidev`
//! channel, and each is given to the guest whole: its DMA reaches the
//! guest's memory and nothing else (the IOMMU), its memory BARs are mapped
//! into the guest's memory, its MSIs are bound to the guest's processors,
//! and the line its INTx is wired to is the guest's (`lines`; functions on
//! one line share it). Linux finds them on bus 0 of the guest's PCI, their
//! configuration space as `vhv::pci` makes it, through the platform's
//! hypercalls. What the PC's firmware describes below a function goes into
//! the guest's ACPI tables (`vhv::acpi`): the devices on its bus, with their
//! place on it (I2C or SPI), their interrupt lines (a touchpad's on an I2C
//! controller, whose line the guest gets too), the GPIO pins they are wired
//! to (`gpio`: a laptop's amplifiers on an SPI controller have some), and
//! the function's and their constant data; the firmware's tables that
//! describe a function's hardware go there whole (an Intel audio
//! controller's NHLT: the links of its DSP, and the microphones on them).
//!
//! A function reports no errors to the host, which a PC may make NMIs of:
//! its error reporting is turned off before the guest runs, and stays off.
//!
//! Memory the firmware keeps for a function (an RMRR: a GPU's stolen
//! memory, which it and the firmware's framebuffer use) the guest has where
//! the PC has it, in a free span of its address space: the function's DMA
//! reaches it there, as the firmware left it. A function the guest cannot
//! be given (its firmware's memory where the guest has its own, no room on
//! the bus or for its BARs) is said so and let go of: its channel closes,
//! which tells devmgr it stays the host's.
//!
//! Where a function is: on the host's bus 0, a function keeps its device
//! and function numbers if its device's function 0 comes too (drivers may
//! look for siblings where they are on the PC); other functions take a
//! free device number, as its function 0. Device 0 is the host bridge's,
//! as on a PC.
//!
//! An Intel integrated GPU's driver reads more of the PC's firmware: its
//! OpRegion (the display's description), which the guest has a copy of
//! (`vhv::igd`), and the PC's host bridge, which the guest has a stand-in
//! for at 00:00.0, read-only (`vhv::pci::HostBridge`). Its stolen memory is
//! the memory the firmware keeps for it, where the PC has it.
//!
//! The kernel's command line says where each function's memory BARs are on
//! the host ([`Devices::host_options`]): a driver in the guest names the
//! host's memory it drives so (a display, its firmware's framebuffer), to
//! services that know the host's addresses.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use vabi::map_flags;
use vhv::acpi::{self, Child, Function, Gpio, GpioController, GpioInterrupt, HID_OVER_I2C, Resource};
use vhv::pci::{Bar, ConfigSpace, HostBridge, Write};
use vhv::platform::{error, gpio};
use vproto::pci::{AcpiDevice, AcpiResource, DeviceInfo, IntxLine, pcidev};
use vrt::object::{Channel, Guest, Interrupt, Vcpu};
use vrt::println;
use vrt::sync::Mutex;

use crate::gpio::{Owner, Pins};
use crate::lines::Lines;

const PAGE: u64 = 4096;

/// A function the guest has.
struct Device {
    /// Where the guest has it on bus 0: device << 3 | function.
    devfn: u8,
    info: DeviceInfo,
    /// What the guest's ACPI tables say about it (its INTx among it).
    description: Function,
    state: Mutex<State>,
}

struct State {
    pci: pcidev::Client,
    config: ConfigSpace,
    /// Its MSIs, by index: the interrupt, and the message for it.
    msis: BTreeMap<u64, (Interrupt, u64)>,
}

/// The functions the guest has.
pub struct Devices {
    list: Vec<Device>,
    /// The GPIO pins their devices are wired to.
    pins: Pins,
    /// The firmware's tables that came with them (one of each).
    firmware_tables: Vec<Vec<u8>>,
    /// The memory the firmware keeps for them, where the guest has it.
    reserved: Vec<Range<u64>>,
    /// An Intel GPU's: the guest's copy of its OpRegion (for
    /// `vhv::igd::OPREGION_AT`), and the stand-in for the PC's host bridge.
    opregion: Option<Vec<u8>>,
    host_bridge: Option<HostBridge>,
}

/// Where in the guest's address space BARs go: below 4 GiB the ones that
/// must (32-bit) or fit, the rest above; and where the memory the firmware
/// keeps for functions may be (where nothing else of the guest's is).
pub struct Windows {
    pub low: Range<u64>,
    pub high: Range<u64>,
    pub free: Vec<Range<u64>>,
}

impl Windows {
    /// A naturally aligned place for `size` bytes: in the low window if
    /// `low_only` or it fits there, else the high one.
    fn place(&mut self, size: u64, low_only: bool) -> Option<u64> {
        let take = |w: &mut Range<u64>| {
            let at = w.start.next_multiple_of(size);
            (at.checked_add(size)? <= w.end).then(|| {
                w.start = at + size;
                at
            })
        };
        take(&mut self.low).or_else(|| if low_only { None } else { take(&mut self.high) })
    }
}

impl Devices {
    /// No functions.
    pub fn none() -> Devices {
        Devices {
            list: Vec::new(),
            pins: Pins::default(),
            firmware_tables: Vec::new(),
            reserved: Vec::new(),
            opregion: None,
            host_bridge: None,
        }
    }

    /// Gives the guest the functions of `channels` it can have: their DMA,
    /// the memory the firmware keeps for them, their BARs (in `windows`),
    /// their place on its bus 0, the lines their INTx are wired to (added
    /// to `lines`).
    pub fn attach(guest: &Guest, channels: Vec<Channel>, windows: &mut Windows, lines: &mut Lines) -> Devices {
        let mut given = Vec::new();
        for channel in channels {
            let pci = pcidev::Client::new(channel);
            match pci.info() {
                Ok(info) => given.push((pci, info)),
                Err(e) => println!("devmgr: {e}"),
            }
        }
        let places = places(&given.iter().map(|(_, i)| (i.bus, i.slot, i.function)).collect::<Vec<_>>());
        let mut list = Vec::new();
        let mut pins = Pins::default();
        let mut firmware_tables: Vec<Vec<u8>> = Vec::new();
        let mut reserved = Vec::new();
        let (mut opregion, mut host_bridge, mut intel_gpus) = (None, None, 0);
        for ((pci, info), place) in given.into_iter().zip(places) {
            let name = format!(
                "{:04x}:{:04x} at {:02x}:{:02x}.{}",
                info.vendor, info.device, info.bus, info.slot, info.function
            );
            let Some((devfn, multifunction)) = place else {
                println!("{name}: the guest's bus is full; it does not get it");
                continue;
            };
            let Placed { bars, pci_express, kept } = match give(guest, &pci, &info, windows, &reserved) {
                Ok(placed) => placed,
                Err(e) => {
                    println!("{name} cannot be given to the guest: {e}");
                    continue;
                }
            };
            // The line its INTx is wired to; other functions may have it
            // already.
            let intx = match pci.intx() {
                Ok(Ok((irq, line))) => (lines.has(line.gsi) || lines.add(line.gsi, irq, line.level)).then_some(line),
                _ => None,
            };
            let mut at: Vec<String> =
                bars.iter().map(|b| format!("BAR {} at {:#x} ({} KiB)", b.index, b.address, b.size / 1024)).collect();
            for k in &kept {
                at.push(format!("the memory the firmware keeps for it at {:#x}-{:#x}", k.start, k.end - 1));
            }
            reserved.extend(kept.into_iter().filter(|k| !reserved.contains(k)).collect::<Vec<_>>());
            if let Some(l) = intx {
                at.push(format!("INT{}# on GSI {}", (b'A' + l.pin - 1) as char, l.gsi));
            }
            println!("{} is the guest's 00:{:02x}.{}: {}", name, devfn >> 3, devfn & 7, at.join(", "));
            let mut given = Given { pci: &pci, name: &name, function: list.len(), lines, pins: &mut pins };
            let description = describe(&mut given, devfn, intx);
            for table in pci.acpi_tables().unwrap_or_default() {
                let signature = String::from_utf8_lossy(table.get(..4).unwrap_or_default()).into_owned();
                if !acpi::is_table(&table) {
                    println!("{name}: the firmware's {signature} is not a whole table: the guest does not get it");
                } else if !firmware_tables.iter().any(|t| t[..4] == table[..4]) {
                    println!("{name} comes with the firmware's {signature} ({} bytes)", table.len());
                    firmware_tables.push(table);
                }
            }
            let mut config = ConfigSpace::new(bars, multifunction, pci_express, intx.map(|l| (l.pin, l.gsi as u8)));
            // The first Intel GPU's OpRegion and host bridge are the guest's.
            if is_intel_gpu(&info) {
                if intel_gpus == 0 {
                    (opregion, host_bridge) = intel_gpu(&pci, &name);
                }
                let has_copy = intel_gpus == 0 && opregion.is_some();
                config.intel_graphics(if has_copy { vhv::igd::OPREGION_AT as u32 } else { 0 });
                intel_gpus += 1;
            }
            let state = Mutex::new(State { pci, config, msis: BTreeMap::new() });
            list.push(Device { devfn, info, description, state });
        }
        Devices { list, pins, firmware_tables, reserved, opregion, host_bridge }
    }

    /// The kernel command line's options that say where the functions'
    /// memory BARs are on the host: ` veda.device=00:01.0,0:0x80000000,...`
    /// for each, its place in the guest then each BAR's index and host
    /// address.
    pub fn host_options(&self) -> String {
        let mut options = String::new();
        for d in &self.list {
            options.push_str(&format!(" veda.device=00:{:02x}.{}", d.devfn >> 3, d.devfn & 7));
            for b in d.info.bars.iter().filter(|b| !b.io) {
                options.push_str(&format!(",{}:{:#x}", b.index, b.address));
            }
        }
        options
    }

    /// The functions, as the ACPI tables describe them.
    pub fn functions(&self) -> Vec<Function> {
        self.list.iter().map(|d| d.description.clone()).collect()
    }

    /// The GPIO controllers of their devices' pins, as the ACPI tables
    /// describe them.
    pub fn gpio_controllers(&self) -> Vec<GpioController> {
        self.pins.controllers()
    }

    /// The firmware's tables that describe their hardware.
    pub fn firmware_tables(&self) -> &[Vec<u8>] {
        &self.firmware_tables
    }

    /// The memory the firmware keeps for them, where the guest has it.
    pub fn reserved(&self) -> &[Range<u64>] {
        &self.reserved
    }

    /// The guest's copy of an Intel GPU's OpRegion, for
    /// `vhv::igd::OPREGION_AT`.
    pub fn opregion(&self) -> Option<&[u8]> {
        self.opregion.as_deref()
    }

    /// Operation `op` ([`gpio`]) on pin `pin` of GPIO controller
    /// `controller`, with `value` for a level: through the channel of the
    /// function the pin's device is below.
    pub fn gpio(&self, controller: u64, pin: u64, op: u64, value: u64) -> u64 {
        let Some(Owner { function, device, index }) = self.pins.owner(controller, pin) else {
            return error::INVALID;
        };
        let Some(d) = self.list.get(function) else { return error::INVALID };
        let s = d.state.lock();
        let done = match op {
            gpio::READ => s.pci.gpio_read(device, index).map(|r| r.map(u64::from)),
            gpio::WRITE => s.pci.gpio_write(device, index, value != 0).map(|r| r.map(|_| 0)),
            gpio::INPUT => s.pci.gpio_input(device, index).map(|r| r.map(|_| 0)),
            gpio::DIRECTION => s.pci.gpio_is_output(device, index).map(|r| r.map(u64::from)),
            _ => return error::INVALID,
        };
        match done {
            Ok(Ok(v)) => v,
            _ => error::INVALID,
        }
    }

    fn device(&self, function: u64) -> Option<&Device> {
        (function >> 8 == 0).then(|| self.list.iter().find(|d| d.devfn as u64 == function))?
    }

    /// A read of the configuration space of guest function `function`.
    pub fn config_read(&self, function: u64, offset: u64, width: u64) -> u64 {
        let all_ones = u32::MAX >> (32 - 8 * width.clamp(1, 4) as u32);
        let (Ok(offset), Ok(width)) = (u16::try_from(offset), u8::try_from(width)) else {
            return all_ones as u64;
        };
        let Some(d) = self.device(function) else {
            return match &self.host_bridge {
                Some(b) if function == 0 => b.read(offset, width) as u64,
                _ => all_ones as u64,
            };
        };
        let s = d.state.lock();
        s.config.read(offset, width, |at| s.pci.config_read(at, 4).ok().and_then(|r| r.ok()).unwrap_or(u32::MAX)) as u64
    }

    /// A write to the configuration space of guest function `function`.
    pub fn config_write(&self, function: u64, offset: u64, width: u64, value: u64) -> u64 {
        let (Ok(offset), Ok(width)) = (u16::try_from(offset), u8::try_from(width)) else {
            return vhv::platform::error::INVALID;
        };
        let Some(d) = self.device(function) else {
            // The host bridge's stand-in is read-only.
            return if function == 0 && self.host_bridge.is_some() { 0 } else { vhv::platform::error::INVALID };
        };
        let mut s = d.state.lock();
        if let Write::Device { offset, width, value } = s.config.write(offset, width, value as u32)
            && !matches!(s.pci.config_write(offset, width, value), Ok(Ok(())))
        {
            println!("{:04x}:{:04x}: devmgr refused a write at {:#x}", d.info.vendor, d.info.device, offset);
        }
        0
    }

    /// Routes MSI `index` of guest function `function` to `vector` of
    /// `vcpu`: the message the function sends for it. Vector 0: the MSI
    /// reaches nothing any more (the guest freed it).
    pub fn msi(&self, function: u64, index: u64, vcpu: &Vcpu, vector: u64) -> u64 {
        let (Some(d), Ok(vector)) = (self.device(function), u8::try_from(vector)) else {
            return vhv::platform::error::INVALID;
        };
        let mut s = d.state.lock();
        if vector == 0 {
            if let Some((irq, _)) = s.msis.get(&index) {
                let _ = vcpu.bind_interrupt(irq, 0);
            }
            return 0;
        }
        if !s.msis.contains_key(&index) {
            match s.pci.alloc_msi() {
                Ok(Ok((irq, msg))) => {
                    let message = vhv::platform::msi_result(msg.address, msg.data);
                    s.msis.insert(index, (irq, message));
                }
                _ => {
                    println!("{:04x}:{:04x}: no interrupt for MSI {}", d.info.vendor, d.info.device, index);
                    return vhv::platform::error::INVALID;
                }
            }
        }
        let (irq, message) = &s.msis[&index];
        match vcpu.bind_interrupt(irq, vector) {
            Ok(()) => *message,
            Err(e) => {
                println!("{:04x}:{:04x}: MSI {} cannot go to the guest: {}", d.info.vendor, d.info.device, index, e);
                vhv::platform::error::INVALID
            }
        }
    }
}

/// Whether the function is an Intel integrated GPU (a display controller of
/// Intel's on the root bus).
fn is_intel_gpu(info: &DeviceInfo) -> bool {
    info.vendor == 0x8086 && info.class == 0x03 && info.bus == 0
}

/// What the driver of Intel GPU `pci` (`name`) reads of the PC's firmware:
/// the guest's copy of its OpRegion (none if the firmware has none), and
/// the stand-in for the PC's host bridge.
fn intel_gpu(pci: &pcidev::Client, name: &str) -> (Option<Vec<u8>>, Option<HostBridge>) {
    let copy = match pci.opregion() {
        Ok(Ok(o)) => {
            let vbt = (!o.vbt.is_empty()).then_some(&o.vbt[..]);
            let copy = vhv::igd::for_guest(&o.opregion, vbt, vhv::igd::OPREGION_AT);
            let vbt = match vbt {
                Some(v) => format!("a VBT of {} bytes after it", v.len()),
                None => String::from("its VBT in it"),
            };
            match &copy {
                Some(_) => println!("{name}: its OpRegion is the guest's at {:#x}, {vbt}", vhv::igd::OPREGION_AT),
                None => println!("{name}: its OpRegion is not one the guest can have"),
            }
            copy
        }
        Ok(Err(e)) => {
            println!("{name}: no OpRegion ({e:?})");
            None
        }
        Err(e) => {
            println!("{name}: devmgr: {e}");
            None
        }
    };
    let bridge = match pci.host_bridge() {
        Ok(Ok(b)) => {
            println!(
                "{name}: the PC's host bridge ({:04x}:{:04x}) is at the guest's 00:00.0",
                b.ids & 0xFFFF,
                b.ids >> 16
            );
            Some(HostBridge {
                ids: b.ids,
                class: b.class,
                subsystem: b.subsystem,
                mchbar: b.mchbar,
                graphics_control: b.graphics_control,
            })
        }
        _ => None,
    };
    (copy, bridge)
}

/// Where a function given is in the guest: its BARs, its PCI Express
/// capability, and the memory the firmware keeps for it.
struct Placed {
    bars: Vec<Bar>,
    pci_express: Option<u16>,
    kept: Vec<Range<u64>>,
}

/// Gives the guest function `info` (`pci`'s): the memory the firmware
/// keeps for it (but what `reserved` has, which a function given before
/// brought), its DMA, then its memory BARs, placed in `windows`.
fn give(
    guest: &Guest,
    pci: &pcidev::Client,
    info: &DeviceInfo,
    windows: &mut Windows,
    reserved: &[Range<u64>],
) -> Result<Placed, String> {
    let resource = pci.device_resource().map_err(|e| format!("devmgr: {e}"))?.map_err(|e| format!("{e:?}"))?;
    // The memory the firmware keeps for it, where the PC has it: in the
    // guest's address space, so in its IOMMU domain, before it joins (a
    // function given before may have had the same).
    let kept = pci.reserved_memory().map_err(|e| format!("devmgr: {e}"))?.map_err(|e| format!("{e:?}"))?;
    let mut mapped: Vec<Range<u64>> = Vec::new();
    let unmap = |mapped: &[Range<u64>]| {
        for r in mapped {
            let _ = guest.unmap(r.start, (r.end - r.start) as usize);
        }
    };
    for k in &kept {
        let range = k.base..k.base + k.size;
        if reserved.contains(&range) {
            continue;
        }
        let at = format!("the memory the firmware keeps for it ({:#x}-{:#x})", range.start, range.end - 1);
        let free = windows.free.iter().any(|f| f.start <= range.start && range.end <= f.end)
            && !reserved.iter().chain(&mapped).any(|r| r.start < range.end && range.start < r.end);
        let mapping = if free {
            guest
                .map(&k.memory, 0, k.size as usize, k.base, map_flags::READ | map_flags::WRITE)
                .map_err(|e| format!("{at} cannot be the guest's: {e}"))
        } else {
            Err(format!("{at} is where the guest has something else"))
        };
        if let Err(e) = mapping {
            unmap(&mapped);
            return Err(e);
        }
        mapped.push(range);
    }
    // Its DMA reaches the guest's memory from now on, before the guest can
    // enable it.
    let sid = (info.bus as u16) << 8 | (info.slot as u16) << 3 | info.function as u16;
    if let Err(e) = guest.attach_device(&resource, sid) {
        unmap(&mapped);
        return Err(format!("{e}"));
    }
    let pci_express = vproto::pci::find_capability(pci, vhv::pci::CAP_PCI_EXPRESS);
    quiet_errors(pci, pci_express);
    let bars = map_bars(guest, pci, info, windows)?;
    Ok(Placed { bars, pci_express, kept: kept.iter().map(|k| k.base..k.base + k.size).collect() })
}

/// The names of constant data an I2C controller's driver reads from its
/// device: the bus's timing at each speed.
const CONTROLLER_DATA: [&str; 4] = ["SSCN", "FMCN", "FPCN", "HSCN"];
/// HID over I2C's ids.
const HID_OVER_I2C_IDS: [&str; 2] = ["PNP0C50", "ACPI0C50"];

/// A function being given: its channel and name, its place among the
/// guest's functions, and the lines and GPIO pins its devices add to.
struct Given<'a> {
    pci: &'a pcidev::Client,
    name: &'a str,
    function: usize,
    lines: &'a mut Lines,
    pins: &'a mut Pins,
}

/// What the guest's tables say about function `f` at `devfn`: its INTx,
/// its constant data, and the devices the firmware describes below it,
/// whose interrupt lines and GPIO pins the guest gets. What a device uses
/// that the guest cannot have is left out, and said.
fn describe(f: &mut Given, devfn: u8, intx: Option<IntxLine>) -> Function {
    let pci = f.pci;
    let data_of = |device: u32, names: &[&str]| -> Vec<(String, Vec<u8>)> {
        names
            .iter()
            .filter_map(|n| match pci.acpi_data(device, String::from(*n)) {
                Ok(Ok(aml)) => Some((String::from(*n), aml)),
                _ => None,
            })
            .collect()
    };
    let children = pci
        .acpi_devices()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter(|(_, d)| d.status & 1 != 0)
        .filter_map(|(i, d)| child(f, i as u32, d))
        .collect::<Vec<Child>>();
    Function { devfn, intx: intx.map(|l| (l.pin, l.gsi)), data: data_of(u32::MAX, &CONTROLLER_DATA), children }
}

/// Described device `index` (`d`) of function `f`, as the guest has it:
/// its connection to the function's bus, its interrupts (whose lines the
/// guest gets), the GPIO pins it is wired to, what its `_DSM` answers HID
/// over I2C, its `_DSD`.
fn child(f: &mut Given, index: u32, d: &AcpiDevice) -> Option<Child> {
    let (pci, name) = (f.pci, f.name);
    let (parent, own) = d.path.rsplit_once('.')?;
    let mut resources = Vec::new();
    let mut interrupts = 0;
    // The device's GPIO pins, counted in order.
    let mut pin_index = 0;
    for r in &d.resources {
        match r {
            AcpiResource::I2c { controller, address, speed_hz, ten_bit } if controller == parent => {
                resources.push(Resource::I2c { address: *address, speed_hz: *speed_hz, ten_bit: *ten_bit });
            }
            &AcpiResource::Spi { ref controller, chip_select, speed_hz, bits, cpol, cpha, cs_active_high }
                if controller == parent =>
            {
                let three_wire = false;
                resources.push(Resource::Spi { chip_select, speed_hz, bits, cpol, cpha, cs_active_high, three_wire });
            }
            AcpiResource::Gpio { .. } => {
                if let Some(g) = gpio_connection(f, index, d, r, &mut pin_index) {
                    resources.push(Resource::Gpio(g));
                }
            }
            AcpiResource::Irq { irqs, edge, active_low, shared } => {
                for _ in irqs {
                    let given = match pci.acpi_interrupt(index, interrupts) {
                        Ok(Ok((irq, line))) => {
                            (f.lines.has(line.gsi) || f.lines.add(line.gsi, irq, line.level)).then_some(line)
                        }
                        _ => None,
                    };
                    match given {
                        Some(line) => resources.push(Resource::Interrupt {
                            gsi: line.gsi,
                            edge: *edge,
                            active_low: *active_low,
                            shared: *shared,
                            wake: false,
                        }),
                        None => println!("{name}: {}'s interrupt {} cannot be given to the guest", d.path, interrupts),
                    }
                    interrupts += 1;
                }
            }
            other => println!("{name}: {} uses what the guest does not get: {:?}", d.path, other),
        }
    }
    let hid_over_i2c = HID_OVER_I2C_IDS.iter().any(|id| d.hid == *id || d.cids.iter().any(|c| c == id));
    let hid_descriptor = acpi::uuid(HID_OVER_I2C).filter(|_| hid_over_i2c).and_then(|uuid| {
        match pci.acpi_dsm(index, uuid.to_vec(), 1, 1) {
            Ok(Ok(address)) => u16::try_from(address).ok(),
            _ => None,
        }
    });
    let data = match pci.acpi_data(index, String::from("_DSD")) {
        Ok(Ok(aml)) => alloc::vec![(String::from("_DSD"), aml)],
        _ => Vec::new(),
    };
    let text = |s: &str| (!s.is_empty()).then(|| String::from(s));
    println!(
        "{name}: the guest has {} ({}) below it{}",
        d.path,
        if d.hid.is_empty() { "no id" } else { &d.hid },
        hid_descriptor.map(|a| format!(", its HID descriptor at {a:#x}")).unwrap_or_default()
    );
    Some(Child {
        name: String::from(own),
        hid: text(&d.hid),
        cids: d.cids.clone(),
        uid: text(&d.uid),
        sub: text(&d.sub),
        resources,
        hid_descriptor,
        data,
    })
}

/// GPIO connection `r` of described device `index` (`d`) of function `f`,
/// as the guest has it: its pins on the guest's controller for the PC's,
/// each driven through the function's channel (`pin_index` counts the
/// device's pins), an interrupt's pin raising a line of the platform's.
/// `None` if the guest gets none of its pins.
fn gpio_connection(f: &mut Given, index: u32, d: &AcpiDevice, r: &AcpiResource, pin_index: &mut u32) -> Option<Gpio> {
    let &AcpiResource::Gpio {
        interrupt,
        ref pins,
        ref controller,
        pull,
        restriction,
        shared,
        edge,
        polarity,
        wake,
        debounce,
    } = r
    else {
        return None;
    };
    let (path, uid) = f.pins.controller(controller);
    let mut given = Vec::new();
    for &pin in pins {
        let at = *pin_index;
        *pin_index += 1;
        if interrupt {
            let irq = match f.pci.gpio_interrupt(index, at) {
                Ok(Ok(irq)) => irq,
                e => {
                    println!(
                        "{}: {}'s interrupt on GPIO pin {} cannot be given to the guest: {:?}",
                        f.name, d.path, pin, e
                    );
                    continue;
                }
            };
            let Some(gsi) = f.lines.add_platform(irq, !edge) else {
                println!("{}: no line is left for {}'s GPIO pin {}", f.name, d.path, pin);
                continue;
            };
            f.pins.add_interrupt(uid, GpioInterrupt { pin, gsi, edge, active_low: polarity == 1 });
            println!("{}: {}'s GPIO pin {} interrupts on the guest's GSI {}", f.name, d.path, pin, gsi);
        }
        f.pins.add(uid, pin, Owner { function: f.function, device: index, index: at });
        given.push(pin);
    }
    if given.is_empty() {
        return None;
    }
    Some(Gpio { interrupt, pins: given, controller: path, pull, restriction, edge, polarity, shared, wake, debounce })
}

/// Turns off the errors the function reports to the host (its SERR# and
/// PCI Express error reporting; see `vhv::pci`), before the guest runs.
fn quiet_errors(pci: &pcidev::Client, pci_express: Option<u16>) {
    let read = |at| pci.config_read(at, 2).ok().and_then(|r| r.ok());
    if let Some(command) = read(0x04) {
        let _ = pci.config_write(0x04, 2, command & !(vhv::pci::COMMAND_SERR as u32));
    }
    if let Some(at) = pci_express.map(|p| p + vhv::pci::DEVCTL)
        && let Some(control) = read(at)
    {
        let _ = pci.config_write(at, 2, control & !(vhv::pci::DEVCTL_ERROR_REPORTING as u32));
    }
}

/// Maps the function's memory BARs into the guest's memory: where it sees
/// them. A BAR the guest cannot have whole pages of (smaller than a page,
/// or not on one) stays out.
fn map_bars(guest: &Guest, pci: &pcidev::Client, info: &DeviceInfo, windows: &mut Windows) -> Result<Vec<Bar>, String> {
    let mut bars = Vec::new();
    for b in info.bars.iter().filter(|b| !b.io) {
        if b.size < PAGE || !b.address.is_multiple_of(PAGE) || !b.size.is_power_of_two() {
            println!(
                "BAR {} ({} bytes at {:#x}) is not whole pages; the guest does not get it",
                b.index, b.size, b.address
            );
            continue;
        }
        let is64 =
            pci.config_read(0x10 + 4 * b.index as u16, 4).ok().and_then(|r| r.ok()).is_some_and(|v| v & 0b110 == 0b100);
        let address = windows
            .place(b.size, !is64)
            .ok_or_else(|| format!("no room for BAR {} ({} KiB)", b.index, b.size / 1024))?;
        let vmo =
            pci.map_bar(b.index).map_err(|e| format!("devmgr: {e}"))?.map_err(|e| format!("BAR {}: {e:?}", b.index))?;
        guest
            .map(&vmo, 0, b.size as usize, address, map_flags::READ | map_flags::WRITE)
            .map_err(|e| format!("BAR {} cannot be mapped: {e}", b.index))?;
        bars.push(Bar { index: b.index, address, size: b.size, is64, prefetchable: b.prefetchable });
    }
    Ok(bars)
}

/// Where each function of `functions` (bus, device, function on the host)
/// goes on the guest's bus 0: device << 3 | function, and whether its
/// device has other functions there; `None` once the bus is full. Device 0
/// stays the host bridge's.
fn places(functions: &[(u8, u8, u8)]) -> Vec<Option<(u8, bool)>> {
    let keeps = |&(bus, dev, func): &(u8, u8, u8)| {
        bus == 0 && (func == 0 || functions.iter().any(|&(b, d, f)| (b, d, f) == (0, dev, 0)))
    };
    let mut taken = [false; 32];
    for f in functions.iter().filter(|f| keeps(f)) {
        taken[f.1 as usize] = true;
    }
    // Device 0 is the host bridge's.
    let mut free = (1..32u8).filter(|&d| !taken[d as usize]).collect::<Vec<_>>().into_iter();
    functions
        .iter()
        .map(|f| {
            if keeps(f) {
                let siblings = functions.iter().filter(|g| keeps(g) && g.1 == f.1).count();
                Some(((f.1 << 3) | f.2, siblings > 1))
            } else {
                free.next().map(|d| (d << 3, false))
            }
        })
        .collect()
}
