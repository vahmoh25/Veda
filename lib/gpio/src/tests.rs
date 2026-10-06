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
    assert_eq!(l.locate(23), Some(Location { community: 0, pad: 23, owner: (0x28, 28), lock: (0x80, 23) }));
    // GPP_F14, F15 and F17: speaker id, interrupt, reset. GPP_F's pads
    // follow GPP_C's 24 (PADCFG 0x880 on, as the firmware's own table has
    // it), its owner registers follow GPP_C's three (0x2C on).
    assert_eq!(l.locate(302), Some(Location { community: 2, pad: 38, owner: (0x30, 24), lock: (0x88, 14) }));
    assert_eq!(l.locate(303), Some(Location { community: 2, pad: 39, owner: (0x30, 28), lock: (0x88, 15) }));
    assert_eq!(l.locate(305), Some(Location { community: 2, pad: 41, owner: (0x34, 4), lock: (0x88, 17) }));
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

    // Registers that make no sense are not followed.
    communities[0].write(REVID, u32::MAX);
    assert_eq!(pads.write(23, true), Err(Error::Registers));
    assert_eq!(pads.write(26, true), Err(Error::NoSuchPin));
}
