//! The PCI functions the guest gets. devmgr hands each over as a `pcidev`
//! channel, and each is given to the guest whole: its DMA reaches the
//! guest's memory and nothing else (the IOMMU), its memory BARs are mapped
//! into the guest's memory, its MSIs are bound to the guest's processors.
//! Linux finds them on bus 0 of the guest's PCI, their configuration space
//! as `vhv::pci` makes it, through the platform's hypercalls.
//!
//! A function reports no errors to the host, which a PC may make NMIs of:
//! its error reporting is turned off before the guest runs, and stays off.
//!
//! Where a function is: on the host's bus 0, a function keeps its device
//! and function numbers if its device's function 0 comes too (drivers may
//! look for siblings where they are on the PC); other functions take a
//! free device number, as its function 0.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use vabi::map_flags;
use vhv::pci::{Bar, ConfigSpace, Write};
use vproto::pci::{DeviceInfo, pcidev};
use vrt::object::{Channel, Guest, Interrupt, Vcpu};
use vrt::println;
use vrt::sync::Mutex;

const PAGE: u64 = 4096;

/// A function the guest has.
struct Device {
    /// Where the guest has it on bus 0: device << 3 | function.
    devfn: u8,
    info: DeviceInfo,
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
}

/// Where in the guest's memory BARs go: below 4 GiB the ones that must
/// (32-bit) or fit, the rest above.
pub struct Windows {
    pub low: Range<u64>,
    pub high: Range<u64>,
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
        Devices { list: Vec::new() }
    }

    /// Gives the guest the functions of `channels`: their DMA, their BARs
    /// (in `windows`), their place on its bus 0.
    pub fn attach(guest: &Guest, channels: Vec<Channel>, windows: &mut Windows) -> Result<Devices, String> {
        let mut given = Vec::new();
        for channel in channels {
            let pci = pcidev::Client::new(channel);
            let info = pci.info().map_err(|e| format!("devmgr: {e}"))?;
            given.push((pci, info));
        }
        let places = places(&given.iter().map(|(_, i)| (i.bus, i.slot, i.function)).collect::<Vec<_>>());
        let mut list = Vec::new();
        for ((pci, info), place) in given.into_iter().zip(places) {
            let name = format!(
                "{:04x}:{:04x} at {:02x}:{:02x}.{}",
                info.vendor, info.device, info.bus, info.slot, info.function
            );
            let Some((devfn, multifunction)) = place else {
                println!("{name}: the guest's bus is full; it does not get it");
                continue;
            };
            // Its DMA reaches the guest's memory from now on, before the
            // guest can enable it.
            let resource =
                pci.device_resource().map_err(|e| format!("devmgr: {e}"))?.map_err(|e| format!("{name}: {e:?}"))?;
            let sid = (info.bus as u16) << 8 | (info.slot as u16) << 3 | info.function as u16;
            guest.attach_device(&resource, sid).map_err(|e| format!("{name} cannot be given to the guest: {e}"))?;
            let pci_express = vproto::pci::find_capability(&pci, vhv::pci::CAP_PCI_EXPRESS);
            quiet_errors(&pci, pci_express);
            let bars = map_bars(guest, &pci, &info, windows).map_err(|e| format!("{name}: {e}"))?;
            let at: Vec<String> =
                bars.iter().map(|b| format!("BAR {} at {:#x} ({} KiB)", b.index, b.address, b.size / 1024)).collect();
            println!("{} is the guest's 00:{:02x}.{}: {}", name, devfn >> 3, devfn & 7, at.join(", "));
            let config = ConfigSpace::new(bars, multifunction, pci_express);
            list.push(Device { devfn, info, state: Mutex::new(State { pci, config, msis: BTreeMap::new() }) });
        }
        Ok(Devices { list })
    }

    fn device(&self, function: u64) -> Option<&Device> {
        (function >> 8 == 0).then(|| self.list.iter().find(|d| d.devfn as u64 == function))?
    }

    /// A read of the configuration space of guest function `function`.
    pub fn config_read(&self, function: u64, offset: u64, width: u64) -> u64 {
        let all_ones = u32::MAX >> (32 - 8 * width.clamp(1, 4) as u32);
        let (Some(d), Ok(offset), Ok(width)) = (self.device(function), u16::try_from(offset), u8::try_from(width))
        else {
            return all_ones as u64;
        };
        let s = d.state.lock();
        s.config.read(offset, width, |at| s.pci.config_read(at, 4).ok().and_then(|r| r.ok()).unwrap_or(u32::MAX)) as u64
    }

    /// A write to the configuration space of guest function `function`.
    pub fn config_write(&self, function: u64, offset: u64, width: u64, value: u64) -> u64 {
        let (Some(d), Ok(offset), Ok(width)) = (self.device(function), u16::try_from(offset), u8::try_from(width))
        else {
            return vhv::platform::error::INVALID;
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
    /// `vcpu`: the message the function sends for it.
    pub fn msi(&self, function: u64, index: u64, vcpu: &Vcpu, vector: u64) -> u64 {
        let (Some(d), Ok(vector)) = (self.device(function), u8::try_from(vector)) else {
            return vhv::platform::error::INVALID;
        };
        let mut s = d.state.lock();
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
/// device has other functions there; `None` once the bus is full.
fn places(functions: &[(u8, u8, u8)]) -> Vec<Option<(u8, bool)>> {
    let keeps = |&(bus, dev, func): &(u8, u8, u8)| {
        bus == 0 && (func == 0 || functions.iter().any(|&(b, d, f)| (b, d, f) == (0, dev, 0)))
    };
    let mut taken = [false; 32];
    for f in functions.iter().filter(|f| keeps(f)) {
        taken[f.1 as usize] = true;
    }
    let mut free = (0..32u8).filter(|&d| !taken[d as usize]).collect::<Vec<_>>().into_iter();
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
