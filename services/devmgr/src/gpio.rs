//! GPIO pins, for the devices the firmware wires to them (a laptop's
//! amplifiers: their reset line, a chip select). devmgr drives them on a
//! driver's behalf, and only the pins that driver's devices are described
//! with. The controllers are found and mapped here (their id and register
//! windows come from ACPI); their pads are `vgpio`'s.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use vabi::{cache_policy, map_flags};
use vacpi::name::Path;
use vacpi::resource::Resource as AcpiResource;
use vgpio::{Change, Layout, Pads, Registers};
use vproto::pci::PciError;
use vrt::object::{Resource, Vmo};
use vrt::println;

use crate::acpi::Acpi;

/// A community's registers, mapped (uncached).
struct Window {
    address: usize,
    len: usize,
    _vmo: Vmo,
}

impl Registers for Window {
    fn read(&self, offset: usize) -> u32 {
        if offset.is_multiple_of(4) && offset + 4 <= self.len {
            // SAFETY: an aligned register inside the mapping.
            unsafe { read_volatile((self.address + offset) as *const u32) }
        } else {
            u32::MAX
        }
    }

    fn write(&self, offset: usize, value: u32) {
        if offset.is_multiple_of(4) && offset + 4 <= self.len {
            // SAFETY: as above.
            unsafe { write_volatile((self.address + offset) as *mut u32, value) }
        }
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// A controller with its communities mapped.
struct Controller {
    layout: &'static Layout,
    communities: Vec<Window>,
}

#[derive(Default)]
pub struct Gpio {
    controllers: BTreeMap<Path, Option<Controller>>,
    /// Pads already set up (logged once).
    set_up: Vec<(Path, u16)>,
}

fn error(e: vgpio::Error) -> PciError {
    match e {
        vgpio::Error::NoSuchPin => PciError::NotFound,
        vgpio::Error::Registers => PciError::Unsupported,
        vgpio::Error::Busy => PciError::Busy,
    }
}

impl Gpio {
    /// The controller at `path`, mapped on first use; `None` if devmgr
    /// cannot drive it.
    fn controller(&mut self, acpi: &Acpi, mmio: &Resource, path: &Path) -> Option<&Controller> {
        if !self.controllers.contains_key(path) {
            let c = Self::open(acpi, mmio, path);
            self.controllers.insert(path.clone(), c);
        }
        self.controllers.get(path)?.as_ref()
    }

    fn open(acpi: &Acpi, mmio: &Resource, path: &Path) -> Option<Controller> {
        let hid = acpi.identify(path).hid.unwrap_or_default();
        let Some(layout) = vgpio::layout(&hid) else {
            println!("gpio: {} ({}) is a controller devmgr does not drive", path, hid);
            return None;
        };
        let resources = match acpi.resources(path) {
            Ok(r) => r,
            Err(e) => {
                println!("gpio: {} ({}): {}", path, hid, e);
                return None;
            }
        };
        let windows: Vec<(u64, u64)> = resources
            .iter()
            .filter_map(|r| match r {
                AcpiResource::Memory { base, length, .. } => Some((*base, *length)),
                _ => None,
            })
            .collect();
        if windows.len() < layout.communities() {
            println!("gpio: {} ({}) has {} register windows, not {}", path, hid, windows.len(), layout.communities());
            return None;
        }
        let mut communities = Vec::new();
        for &(base, length) in &windows[..layout.communities()] {
            let len = length as usize;
            let mapped = Vmo::create_physical(mmio, base, len, cache_policy::UNCACHED)
                .ok()
                .and_then(|vmo| vmo.map(0, len, map_flags::READ | map_flags::WRITE).ok().map(|a| (a, vmo)));
            let Some((address, vmo)) = mapped else {
                println!("gpio: {} ({}): cannot map registers at {:#x}", path, hid, base);
                return None;
            };
            communities.push(Window { address, len, _vmo: vmo });
        }
        let bases: Vec<alloc::string::String> = windows.iter().map(|(b, _)| alloc::format!("{:#x}", b)).collect();
        println!("gpio: {} ({}, {}): communities at {}", path, hid, layout.name, bases.join(", "));
        Some(Controller { layout, communities })
    }

    /// Reads a pin: the level it drives if it is an output, else the level
    /// on it.
    pub fn read(&mut self, acpi: &Acpi, mmio: &Resource, controller: &Path, pin: u16) -> Result<bool, PciError> {
        let c = self.controller(acpi, mmio, controller).ok_or(PciError::Unsupported)?;
        let (level, change) = Pads::new(c.layout, &c.communities).read(pin).map_err(error)?;
        self.note(controller, pin, change);
        Ok(level)
    }

    /// Drives a pin high or low, making it a GPIO output if it is not one.
    pub fn write(
        &mut self,
        acpi: &Acpi,
        mmio: &Resource,
        controller: &Path,
        pin: u16,
        high: bool,
    ) -> Result<(), PciError> {
        let c = self.controller(acpi, mmio, controller).ok_or(PciError::Unsupported)?;
        let change = Pads::new(c.layout, &c.communities).write(pin, high).map_err(error)?;
        self.note(controller, pin, change);
        Ok(())
    }

    /// Logs a pad's set-up the first time it changes.
    fn note(&mut self, controller: &Path, pin: u16, change: Option<Change>) {
        let Some(c) = change else { return };
        let key = (controller.clone(), pin);
        if self.set_up.contains(&key) {
            return;
        }
        self.set_up.push(key);
        println!(
            "gpio: {} pin {} (community {}, pad {}): {:#010x} -> {:#010x}",
            controller, pin, c.community, c.pad, c.before, c.after
        );
    }
}
