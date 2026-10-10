//! The interrupt lines the guest has: the PC's global system interrupts
//! (GSIs) its devices use — the keyboard controller's (1 and 12), the lines
//! the PCI functions' INTx are wired to — by the numbers the guest's ACPI
//! tables give them, which are the PC's; and lines the platform makes, GSIs
//! from `PLATFORM_GSIS` on: the interrupts of GPIO pins. devmgr hands over
//! an interrupt for each; functions that share a line share it.
//!
//! The guest routes a line to one of its processors with a hypercall
//! ([`vhv::platform::hypercall::GSI`]): the line's interrupt is bound to the
//! processor, at whose local APIC the kernel raises the vector — an edge,
//! or a level-triggered interrupt with the line masked until the
//! processor's end-of-interrupt. Routed to nothing (vector 0), the line is
//! masked: its interrupt is bound to nothing, a level-triggered line stays
//! masked once it fires, and an edge that comes meanwhile is raised when the
//! line is routed again.

use alloc::collections::BTreeMap;

use vhv::platform::{GSIS, PLATFORM_GSIS};
use vrt::object::{Interrupt, Vcpu};

struct Line {
    irq: Interrupt,
    level: bool,
}

#[derive(Default)]
pub struct Lines {
    lines: BTreeMap<u32, Line>,
}

impl Lines {
    /// Gives the guest line `gsi`, which `irq` is; false if it has the line
    /// already (or cannot route one of that number).
    pub fn add(&mut self, gsi: u32, irq: Interrupt, level: bool) -> bool {
        if gsi >= PLATFORM_GSIS || self.lines.contains_key(&gsi) {
            return false;
        }
        self.lines.insert(gsi, Line { irq, level });
        true
    }

    /// Gives the guest a line of the platform's, which `irq` is: its GSI,
    /// or `None` once there are no more.
    pub fn add_platform(&mut self, irq: Interrupt, level: bool) -> Option<u32> {
        let gsi = (PLATFORM_GSIS..GSIS).find(|g| !self.lines.contains_key(g))?;
        self.lines.insert(gsi, Line { irq, level });
        Some(gsi)
    }

    pub fn has(&self, gsi: u32) -> bool {
        self.lines.contains_key(&gsi)
    }

    /// Routes line `gsi` to `vector` of `vcpu` (0: to nothing).
    pub fn route(&self, gsi: u64, vcpu: &Vcpu, vector: u64) -> bool {
        let (Some(line), Ok(vector)) = (u32::try_from(gsi).ok().and_then(|g| self.lines.get(&g)), u8::try_from(vector))
        else {
            return false;
        };
        if vcpu.bind_interrupt(&line.irq, vector).is_err() {
            return false;
        }
        if vector != 0 {
            // An edge that came while the line was masked signalled the
            // interrupt; a level-triggered line that fired then is masked,
            // and fires again on the ack if it is still asserted.
            if !line.level && line.irq.wait_irq(0).is_ok() {
                let _ = vcpu.interrupt(vector);
            }
            let _ = line.irq.ack();
        }
        true
    }
}
