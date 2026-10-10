//! GPIO pins, for the devices the firmware wires to them (a laptop's
//! amplifiers: their reset line, a chip select, their interrupt). devmgr
//! drives them on a driver's behalf — Veda's, or the driver VM's monitor
//! for Linux — and only the pins that driver's devices are described with.
//! The controllers are found and mapped here (their id and register windows
//! come from ACPI); their pads are `vgpio`'s.
//!
//! A pin's interrupt is an interrupt devmgr raises (`irq_raise`): a
//! controller has one line for all its pins, which devmgr waits on; when
//! it fires, each pin with an interrupt pending raises its own, an edge or
//! level-triggered as the pin's connection says. A level-triggered pin is
//! masked meanwhile: when the one it was given to ends the interrupt (its
//! acknowledgement, or the guest's end-of-interrupt), the event the
//! interrupt signals has devmgr unmask the pin, which fires again if its
//! level lasts. When the one it was given to goes, its pins' interrupts are
//! masked and dropped.
//!
//! A test's ACPI table may describe a simulated controller (`VTST0002`),
//! which devmgr drives as it does a PC's, over `vgpio::sim`'s registers;
//! its line is looked at after each change of its pins.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use vabi::{RawHandle, Rights, cache_policy, map_flags, signals};
use vacpi::name::Path;
use vacpi::resource::{Gpio as Connection, Resource as AcpiResource};
use vgpio::{Change, Layout, Pads, Registers, Trigger};
use vproto::pci::PciError;
use vrt::object::{Event, Interrupt, Resource, Vmo};
use vrt::println;

use crate::acpi::Acpi;

/// The hardware id of the simulated controller test tables describe.
const SIMULATED: &str = "VTST0002";

/// What devmgr waits on for GPIO is told by keys from here on.
pub const KEYS: u64 = 1 << 63;

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

enum Bank {
    Mapped(Vec<Window>),
    Simulated(vgpio::sim::Controller),
}

/// A controller with its communities mapped.
struct Controller {
    layout: &'static Layout,
    bank: Bank,
    /// Its line, once one of its pins interrupts (a simulated controller
    /// has none), and the key it is waited on with.
    line: Option<Interrupt>,
    key: u64,
}

impl Controller {
    fn with_pads<T>(&self, f: impl FnOnce(&Pads<'_, &dyn Registers>) -> T) -> T {
        match &self.bank {
            Bank::Mapped(windows) => {
                let regs: Vec<&dyn Registers> = windows.iter().map(|w| w as &dyn Registers).collect();
                f(&Pads::new(self.layout, &regs))
            }
            Bank::Simulated(sim) => {
                let communities = sim.communities();
                let regs: Vec<&dyn Registers> = communities.iter().map(|c| c as &dyn Registers).collect();
                f(&Pads::new(self.layout, &regs))
            }
        }
    }

    fn simulated(&self) -> Option<&vgpio::sim::Controller> {
        match &self.bank {
            Bank::Simulated(sim) => Some(sim),
            Bank::Mapped(_) => None,
        }
    }
}

/// A pin's interrupt, given to someone.
struct PinInterrupt {
    controller: Path,
    pin: u16,
    /// Raised here (the handle with `SIGNAL`).
    irq: Interrupt,
    /// What a level-triggered one's ending signals; `None` for an edge.
    ended: Option<Event>,
    /// Who has it: the session it goes with.
    owner: u64,
}

pub struct Gpio {
    /// What their registers are mapped with.
    mmio: Resource,
    controllers: BTreeMap<Path, Option<Controller>>,
    /// Pads already set up (logged once).
    set_up: Vec<(Path, u16)>,
    /// The pins' interrupts given out, by the key their ending is waited
    /// on with.
    interrupts: BTreeMap<u64, PinInterrupt>,
    next: u64,
}

fn error(e: vgpio::Error) -> PciError {
    match e {
        vgpio::Error::NoSuchPin => PciError::NotFound,
        vgpio::Error::Registers => PciError::Unsupported,
        vgpio::Error::Busy => PciError::Busy,
    }
}

/// How a connection's pin interrupts, as `vgpio` says it.
pub fn trigger(c: &Connection) -> Trigger {
    match (c.edge, c.polarity) {
        (false, p) => Trigger::Level { active_low: p == 1 },
        (true, 0) => Trigger::Edge { rising: true, falling: false },
        (true, 1) => Trigger::Edge { rising: false, falling: true },
        (true, _) => Trigger::Edge { rising: true, falling: true },
    }
}

impl Gpio {
    pub fn new(mmio: Resource) -> Gpio {
        Gpio { mmio, controllers: BTreeMap::new(), set_up: Vec::new(), interrupts: BTreeMap::new(), next: 0 }
    }

    fn key(&mut self) -> u64 {
        self.next += 1;
        KEYS | self.next
    }

    /// The controller at `path`, mapped on first use; `None` if devmgr
    /// cannot drive it.
    fn controller(&mut self, acpi: &Acpi, path: &Path) -> Option<&mut Controller> {
        if !self.controllers.contains_key(path) {
            let key = self.key();
            let c = Self::open(acpi, &self.mmio, path, key);
            self.controllers.insert(path.clone(), c);
        }
        self.controllers.get_mut(path)?.as_mut()
    }

    fn open(acpi: &Acpi, mmio: &Resource, path: &Path, key: u64) -> Option<Controller> {
        let hid = acpi.identify(path).hid.unwrap_or_default();
        if hid == SIMULATED {
            let layout = vgpio::layout("INTC1055")?;
            println!("gpio: {} ({}) is a simulated controller with {}'s pads", path, hid, layout.name);
            let bank = Bank::Simulated(vgpio::sim::Controller::new(layout));
            return Some(Controller { layout, bank, line: None, key });
        }
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
        Some(Controller { layout, bank: Bank::Mapped(communities), line: None, key })
    }

    /// Reads a pin: the level it drives if it is an output, else the level
    /// on it.
    pub fn read(&mut self, acpi: &Acpi, controller: &Path, pin: u16) -> Result<bool, PciError> {
        let c = self.controller(acpi, controller).ok_or(PciError::Unsupported)?;
        let (level, change) = c.with_pads(|p| p.read(pin)).map_err(error)?;
        self.note(controller, pin, change);
        Ok(level)
    }

    /// Drives a pin high or low, making it a GPIO output if it is not one.
    pub fn write(&mut self, acpi: &Acpi, controller: &Path, pin: u16, high: bool) -> Result<(), PciError> {
        let c = self.controller(acpi, controller).ok_or(PciError::Unsupported)?;
        let change = c.with_pads(|p| p.write(pin, high)).map_err(error)?;
        self.note(controller, pin, change);
        self.look_at_simulated(controller);
        Ok(())
    }

    /// Makes a pin an input.
    pub fn input(&mut self, acpi: &Acpi, controller: &Path, pin: u16) -> Result<(), PciError> {
        let c = self.controller(acpi, controller).ok_or(PciError::Unsupported)?;
        let change = c.with_pads(|p| p.input(pin)).map_err(error)?;
        self.note(controller, pin, change);
        Ok(())
    }

    /// Whether a pin is an output.
    pub fn is_output(&mut self, acpi: &Acpi, controller: &Path, pin: u16) -> Result<bool, PciError> {
        let c = self.controller(acpi, controller).ok_or(PciError::Unsupported)?;
        c.with_pads(|p| p.is_output(pin)).map_err(error)
    }

    /// Sets a pin up to interrupt as `trigger` says, for session `owner`:
    /// the interrupt to give it (without the right to raise it). `line`
    /// makes the controller's own line, the first time: its GSI, and
    /// whether it is level-triggered and active low.
    pub fn interrupt(
        &mut self,
        acpi: &Acpi,
        controller: &Path,
        pin: u16,
        trigger: Trigger,
        owner: u64,
        line: impl FnOnce(u32, bool, bool) -> Result<Interrupt, PciError>,
    ) -> Result<Interrupt, PciError> {
        if self.interrupts.values().any(|i| i.controller == *controller && i.pin == pin) {
            return Err(PciError::Busy);
        }
        let key = self.key();
        let c = self.controller(acpi, controller).ok_or(PciError::Unsupported)?;
        if c.line.is_none() && c.simulated().is_none() {
            let crs = acpi.resources(controller).unwrap_or_default();
            let Some((gsi, level, active_low)) = crs.iter().find_map(|r| match r {
                AcpiResource::Irq { irqs, edge, active_low, .. } => Some((*irqs.first()?, !*edge, *active_low)),
                _ => None,
            }) else {
                println!("gpio: {}: the firmware gives the controller no interrupt", controller);
                return Err(PciError::NotFound);
            };
            c.line = Some(line(gsi, level, active_low)?);
            println!("gpio: {}: its pins interrupt on GSI {}", controller, gsi);
        }
        let change = c.with_pads(|p| p.set_interrupt(pin, trigger)).map_err(error)?;
        let level = matches!(trigger, Trigger::Level { .. });
        let ended = if level { Some(Event::create().map_err(|_| PciError::NoResources)?) } else { None };
        let irq = Interrupt::create_software(level, ended.as_ref()).map_err(|_| PciError::NoResources)?;
        let given = irq
            .0
            .duplicate(Some(Rights(Rights::BASIC.0 | Rights::WRITE.0)))
            .map(Interrupt)
            .map_err(|_| PciError::NoResources)?;
        c.with_pads(|p| p.enable_interrupt(pin, true)).map_err(error)?;
        self.note(controller, pin, change);
        println!("gpio: {} pin {} interrupts ({:?})", controller, pin, trigger);
        self.interrupts.insert(key, PinInterrupt { controller: controller.clone(), pin, irq, ended, owner });
        self.look_at_simulated(controller);
        Ok(given)
    }

    /// What devmgr waits on for the pins: each controller's line, each
    /// level-triggered pin's ending. Handles, signals and keys.
    pub fn waits(&self) -> Vec<(RawHandle, u32, u64)> {
        let mut w = Vec::new();
        for c in self.controllers.values().flatten() {
            if let Some(line) = &c.line {
                w.push((line.raw(), signals::SIGNALED, c.key));
            }
        }
        for (&key, i) in &self.interrupts {
            if let Some(e) = &i.ended {
                w.push((e.raw(), signals::SIGNALED, key));
            }
        }
        w
    }

    /// What a key of [`Gpio::waits`] was signalled for: a controller's line
    /// fired, or a pin's interrupt ended.
    pub fn signalled(&mut self, key: u64) {
        let fired = self.controllers.iter().find(|(_, c)| c.as_ref().is_some_and(|c| c.key == key));
        if let Some(path) = fired.map(|(p, _)| p.clone()) {
            self.pending(&path);
            if let Some(Some(c)) = self.controllers.get(&path)
                && let Some(line) = &c.line
            {
                let _ = line.ack();
            }
            return;
        }
        let Some(i) = self.interrupts.get(&key) else { return };
        if let Some(e) = &i.ended {
            let _ = e.clear();
        }
        let (path, pin) = (i.controller.clone(), i.pin);
        if let Some(Some(c)) = self.controllers.get(&path) {
            // Pending again at once if the level is still there.
            let _ = c.with_pads(|p| p.clear_interrupt(pin).and_then(|_| p.enable_interrupt(pin, true)));
        }
        self.look_at_simulated(&path);
    }

    /// Raises the interrupts of the pins of controller `path` that are
    /// pending; a level-triggered one is masked until it is ended.
    fn pending(&mut self, path: &Path) {
        let Some(Some(c)) = self.controllers.get(path) else { return };
        for i in self.interrupts.values().filter(|i| i.controller == *path) {
            let fired = c.with_pads(|p| {
                if p.interrupt_pending(i.pin) != Ok(true) {
                    return false;
                }
                if i.ended.is_some() {
                    let _ = p.enable_interrupt(i.pin, false);
                }
                p.clear_interrupt(i.pin).is_ok()
            });
            if fired {
                let _ = i.irq.raise();
            }
        }
    }

    /// A simulated controller has no line: whether it would raise it is
    /// looked at after its pins change.
    fn look_at_simulated(&mut self, path: &Path) {
        let raised = matches!(self.controllers.get(path), Some(Some(c)) if c.simulated().is_some_and(|s| s.line()));
        if raised {
            self.pending(path);
        }
    }

    /// Session `owner` went: its pins' interrupts are masked and dropped.
    pub fn release(&mut self, owner: u64) {
        let keys: Vec<u64> = self.interrupts.iter().filter(|(_, i)| i.owner == owner).map(|(&k, _)| k).collect();
        for key in keys {
            if let Some(i) = self.interrupts.remove(&key)
                && let Some(Some(c)) = self.controllers.get(&i.controller)
            {
                let _ = c.with_pads(|p| p.enable_interrupt(i.pin, false));
            }
        }
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
