use core::cell::RefCell;
use std::collections::BTreeMap;
use std::vec::Vec;

use crate::*;

/// A community's registers: 64 KiB, revision 0x92 (four registers per
/// pad), pads from 0x700.
struct Community(RefCell<BTreeMap<usize, u32>>);

impl Community {
    fn new() -> Community {
        let c = Community(RefCell::new(BTreeMap::new()));
        c.write(REVID, 0x0092_0000);
        c.write(PADBAR, 0x700);
        c
    }
}

impl Registers for Community {
    fn read(&self, offset: usize) -> u32 {
        self.0.borrow().get(&offset).copied().unwrap_or(0)
    }
    fn write(&self, offset: usize, value: u32) {
        self.0.borrow_mut().insert(offset, value);
    }
    fn len(&self) -> usize {
        0x1_0000
    }
}

fn tiger_lake() -> &'static Layout {
    layout("INTC1055").unwrap()
}

#[test]
fn the_zenbooks_amplifier_pins() {
    let l = tiger_lake();
    assert_eq!(l.communities(), 4);
    // GPP_B23: the second amplifier's chip select.
    let b23 = l.locate(23).unwrap();
    assert_eq!(b23, Location { community: 0, pad: 23, group: 0, bit: 23, owner: (0x28, 28) });
    assert_eq!(l.lock(&b23), (0x80, 23));
    // GPP_F14, F15 and F17: speaker id, interrupt, reset. GPP_F's pads
    // follow GPP_C's 24 (PADCFG 0x880 on, as the firmware's own table has
    // it), its owner registers follow GPP_C's three (0x2C on), and its
    // group's registers GPP_C's (the second of each kind).
    assert_eq!(l.locate(302), Some(Location { community: 2, pad: 38, group: 1, bit: 14, owner: (0x30, 24) }));
    let f15 = l.locate(303).unwrap();
    assert_eq!(f15, Location { community: 2, pad: 39, group: 1, bit: 15, owner: (0x30, 28) });
    assert_eq!(l.lock(&f15), (0x88, 15));
    assert_eq!(l.locate(305), Some(Location { community: 2, pad: 41, group: 1, bit: 17, owner: (0x34, 4) }));
    assert_eq!(0x700 + 41 * 16, 0x880 + 17 * 16);
    // Pins between groups, and past the last, are no pads.
    assert_eq!(l.locate(26), None);
    assert_eq!(l.locate(400), None);
    assert!(layout("INT0000").is_none());
}

#[test]
fn driving_and_reading_pads() {
    let communities: Vec<Community> = (0..4).map(|_| Community::new()).collect();
    let pads = Pads::new(tiger_lake(), &communities);
    let config = |pin: u16| {
        let at = tiger_lake().locate(pin).unwrap();
        (at.community, 0x700 + at.pad * 16)
    };

    // A pad in a native function becomes a GPIO output, low.
    let (c, cfg) = config(23);
    communities[c].write(cfg, 1 << 10 | PADCFG0_ROUTES);
    let change = pads.write(23, false).unwrap().unwrap();
    assert_eq!(change.before, 1 << 10 | PADCFG0_ROUTES);
    assert_eq!(communities[c].read(cfg), PADCFG0_RX_DISABLE);
    // Already an output: only the level changes, nothing to log.
    assert_eq!(pads.write(23, true), Ok(None));
    assert_eq!(communities[c].read(cfg), PADCFG0_RX_DISABLE | PADCFG0_TX_STATE);
    assert_eq!(pads.read(23), Ok((true, None)));

    // An input reads its level.
    let (c, cfg) = config(302);
    communities[c].write(cfg, PADCFG0_TX_DISABLE | PADCFG0_RX_STATE);
    assert_eq!(pads.read(302), Ok((true, None)));

    // Another engine's pad, or a locked one that would have to change, is
    // refused; a locked output keeps working.
    let (c, cfg) = config(305);
    communities[c].write(0x34, 1 << 4);
    assert_eq!(pads.write(305, true), Err(Error::Busy));
    communities[c].write(0x34, 0);
    communities[c].write(cfg, PADCFG0_TX_DISABLE);
    communities[c].write(0x88, 1 << 17);
    assert_eq!(pads.write(305, true), Err(Error::Busy));
    communities[c].write(cfg, PADCFG0_RX_DISABLE);
    assert_eq!(pads.write(305, true), Ok(None));
    communities[c].write(0x8C, 1 << 17);
    assert_eq!(pads.write(305, false), Err(Error::Busy));

    // An output becomes an input again; each says which it is.
    assert_eq!(pads.is_output(23), Ok(true));
    let change = pads.input(23).unwrap().unwrap();
    assert_eq!(change.after, PADCFG0_TX_DISABLE | PADCFG0_TX_STATE);
    assert_eq!((pads.is_output(23), pads.input(23)), (Ok(false), Ok(None)));

    // Registers that make no sense are not followed.
    communities[0].write(REVID, u32::MAX);
    assert_eq!(pads.write(23, true), Err(Error::Registers));
    assert_eq!(pads.write(26, true), Err(Error::NoSuchPin));
}

#[test]
fn a_pads_interrupt() {
    // GPP_F15, the amplifiers' interrupt: level-triggered, active low.
    let communities: Vec<Community> = (0..4).map(|_| Community::new()).collect();
    let pads = Pads::new(tiger_lake(), &communities);
    let f = &communities[2];
    let cfg = 0x700 + 39 * 16;
    let (status, enable, host_own) = (0x104, 0x124, 0xB4);
    f.write(cfg, 1 << 10 | PADCFG0_ROUTES);
    // In ACPI mode its events are the firmware's.
    assert_eq!(pads.set_interrupt(303, Trigger::Level { active_low: true }), Err(Error::Busy));
    f.write(host_own, 1 << 15);
    // Another pad's interrupt stays enabled, and its status pending.
    f.write(enable, 1 << 15 | 1 << 2);
    f.write(status, 1 << 2);
    let change = pads.set_interrupt(303, Trigger::Level { active_low: true }).unwrap().unwrap();
    assert_eq!(change.after, PADCFG0_TX_DISABLE | PADCFG0_RX_INVERT | RX_EVENT_LEVEL);
    assert_eq!(f.read(enable), 1 << 2);
    // The status register is written 1 to clear: the pad's bit only.
    assert_eq!(f.read(status), 1 << 15);
    f.write(status, 1 << 15);
    assert_eq!(pads.interrupt_pending(303), Ok(false));
    pads.enable_interrupt(303, true).unwrap();
    assert_eq!(f.read(enable), 1 << 15 | 1 << 2);
    assert_eq!(pads.interrupt_pending(303), Ok(true));
    pads.enable_interrupt(303, false).unwrap();
    assert_eq!(pads.interrupt_pending(303), Ok(false));
    pads.clear_interrupt(303).unwrap();
    assert_eq!(f.read(status), 1 << 15);
    // Edges: rising, falling (inverted) or both.
    let edge = |rising, falling| {
        pads.set_interrupt(303, Trigger::Edge { rising, falling }).unwrap();
        f.read(cfg) & (PADCFG0_RX_EVENT | PADCFG0_RX_INVERT)
    };
    assert_eq!(edge(true, false), RX_EVENT_EDGE);
    assert_eq!(edge(false, true), RX_EVENT_EDGE | PADCFG0_RX_INVERT);
    assert_eq!(edge(true, true), RX_EVENT_BOTH_EDGES);
    // Locked as it is, it still interrupts; locked otherwise, it cannot.
    f.write(0x88, 1 << 15);
    assert_eq!(pads.set_interrupt(303, Trigger::Edge { rising: true, falling: true }), Ok(None));
    assert_eq!(pads.set_interrupt(303, Trigger::Level { active_low: false }), Err(Error::Busy));
}

#[test]
fn a_simulated_controller() {
    // GPP_B0 drives GPP_B1's pin.
    let c = sim::Controller::new(tiger_lake());
    let communities = c.communities();
    let pads = Pads::new(tiger_lake(), &communities);
    assert_eq!(pads.read(1), Ok((false, None)));
    pads.write(0, true).unwrap();
    assert_eq!(pads.read(1), Ok((true, None)));
    // Both edges of B1 interrupt, once enabled.
    pads.set_interrupt(1, Trigger::Edge { rising: true, falling: true }).unwrap();
    pads.write(0, false).unwrap();
    assert!(!c.line());
    pads.enable_interrupt(1, true).unwrap();
    assert!(c.line() && pads.interrupt_pending(1) == Ok(true));
    pads.clear_interrupt(1).unwrap();
    assert!(!c.line());
    pads.write(0, true).unwrap();
    assert!(c.line());
    pads.clear_interrupt(1).unwrap();
    // A level stays pending while it lasts, cleared or not; masked, it
    // raises nothing.
    pads.set_interrupt(1, Trigger::Level { active_low: false }).unwrap();
    pads.enable_interrupt(1, true).unwrap();
    assert!(c.line());
    pads.clear_interrupt(1).unwrap();
    assert!(c.line());
    pads.enable_interrupt(1, false).unwrap();
    assert!(!c.line());
    pads.write(0, false).unwrap();
    pads.clear_interrupt(1).unwrap();
    pads.enable_interrupt(1, true).unwrap();
    assert!(!c.line());
    // Active low: pending while the pin is low.
    pads.set_interrupt(1, Trigger::Level { active_low: true }).unwrap();
    pads.enable_interrupt(1, true).unwrap();
    assert!(c.line());
    pads.write(0, true).unwrap();
    pads.clear_interrupt(1).unwrap();
    assert!(!c.line());
}
