//! The GPIO pins the guest has: the PC's pins that the devices described
//! below its functions are wired to (a laptop's amplifiers: a chip select,
//! their reset, their interrupt), on a GPIO controller of the platform's
//! for each of the PC's they are on (`VEDA0001`), by the PC's numbers for
//! them. A pin is driven through the channel of the function its device is
//! below — devmgr drives it, and only the pins that function's devices are
//! wired to; a pin that interrupts is a line of the guest's, a GSI the
//! platform makes (from `PLATFORM_GSIS` on), whose interrupt devmgr raises.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vhv::acpi::{GpioController, GpioInterrupt};

/// Where a pin is driven: the function (its place among the guest's), the
/// device the firmware describes below it (its index among them), and the
/// pin's index among that device's GPIO pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub function: usize,
    pub device: u32,
    pub index: u32,
}

struct Controller {
    /// The PC's controller.
    host: String,
    description: GpioController,
    pins: BTreeMap<u16, Owner>,
}

/// Names the guest's `\_SB` has already.
const TAKEN: [&str; 3] = ["PCI0", "PS2K", "PS2M"];

#[derive(Default)]
pub struct Pins {
    controllers: Vec<Controller>,
}

impl Pins {
    /// The guest's controller for the PC's controller `host`: its path in
    /// the guest's tables, and its number. Its name is the PC's
    /// controller's where it can be.
    pub fn controller(&mut self, host: &str) -> (String, u32) {
        let uid = match self.controllers.iter().position(|c| c.host == host) {
            Some(i) => i,
            None => {
                let uid = self.controllers.len();
                let own = host.rsplit(['.', '\\']).next().unwrap_or("");
                let taken = |n: &str| TAKEN.contains(&n) || self.controllers.iter().any(|c| c.description.name == n);
                let name = if own.len() == 4 && !taken(own) { String::from(own) } else { format!("GP{:02X}", uid) };
                let description = GpioController { name, uid: uid as u32, pins: Vec::new(), interrupts: Vec::new() };
                self.controllers.push(Controller { host: String::from(host), description, pins: BTreeMap::new() });
                uid
            }
        };
        (format!("\\_SB.{}", self.controllers[uid].description.name), uid as u32)
    }

    /// Gives the guest pin `pin` of controller `uid`, driven through
    /// `owner` (the first connection to name a pin drives it).
    pub fn add(&mut self, uid: u32, pin: u16, owner: Owner) {
        let Some(c) = self.controllers.get_mut(uid as usize) else { return };
        if c.pins.contains_key(&pin) {
            return;
        }
        c.pins.insert(pin, owner);
        c.description.pins = c.pins.keys().copied().collect();
    }

    /// A pin of controller `uid` interrupts on a line of the guest's.
    pub fn add_interrupt(&mut self, uid: u32, interrupt: GpioInterrupt) {
        if let Some(c) = self.controllers.get_mut(uid as usize)
            && !c.description.interrupts.iter().any(|i| i.pin == interrupt.pin)
        {
            c.description.interrupts.push(interrupt);
            c.description.interrupts.sort_by_key(|i| i.pin);
        }
    }

    /// What drives pin `pin` of controller `uid`.
    pub fn owner(&self, uid: u64, pin: u64) -> Option<Owner> {
        let c = self.controllers.get(usize::try_from(uid).ok()?)?;
        c.pins.get(&u16::try_from(pin).ok()?).copied()
    }

    /// The controllers that have pins, as the guest's tables describe
    /// them.
    pub fn controllers(&self) -> Vec<GpioController> {
        self.controllers.iter().filter(|c| !c.pins.is_empty()).map(|c| c.description.clone()).collect()
    }
}
