//! An Intel GPIO controller (Tiger Lake-LP's and Alder Lake-P's pads), as
//! its registers behave: in each community a revision, the offset of the
//! pads' configuration (`PADBAR`), who owns each pad, its locks, and each
//! pad's configuration, whose bit 0 is the level it drives and bit 1 the
//! level on it.

use std::collections::BTreeMap;
use std::vec::Vec;

use vgpio::{
    Layout, Location, PADBAR, PADCFG0_MODE, PADCFG0_RX_DISABLE, PADCFG0_RX_STATE, PADCFG0_TX_DISABLE, PADCFG0_TX_STATE,
    REVID,
};

/// Where the pads' configuration starts, in every community.
pub const PADS_AT: u32 = 0x700;

pub struct PadsModel {
    pub layout: &'static Layout,
    communities: Vec<BTreeMap<usize, u32>>,
    /// Levels on input pins (what the board's wiring holds them at).
    inputs: BTreeMap<u16, bool>,
}

impl PadsModel {
    pub fn new(layout: &'static Layout) -> PadsModel {
        let communities =
            (0..layout.communities()).map(|_| BTreeMap::from([(REVID, 0x0092_0000), (PADBAR, PADS_AT)])).collect();
        PadsModel { layout, communities, inputs: BTreeMap::new() }
    }

    fn at(&self, pin: u16) -> Location {
        self.layout.locate(pin).expect("a pin of this controller")
    }

    fn config_offset(at: &Location) -> usize {
        PADS_AT as usize + at.pad * 16
    }

    pub fn read(&self, community: usize, offset: usize) -> u32 {
        let v = self.communities[community].get(&offset).copied().unwrap_or(0);
        // An input pad's configuration shows the level on the pin.
        let pin = self.inputs.keys().find(|&&p| {
            let at = self.at(p);
            at.community == community && Self::config_offset(&at) == offset
        });
        match pin {
            Some(p) if v & PADCFG0_RX_DISABLE == 0 => {
                if self.inputs[p] {
                    v | PADCFG0_RX_STATE
                } else {
                    v & !PADCFG0_RX_STATE
                }
            }
            _ => v,
        }
    }

    pub fn write(&mut self, community: usize, offset: usize, value: u32) {
        self.communities[community].insert(offset, value);
    }

    /// A pad's first configuration register.
    pub fn config(&self, pin: u16) -> u32 {
        let at = self.at(pin);
        self.read(at.community, Self::config_offset(&at))
    }

    pub fn set_config(&mut self, pin: u16, value: u32) {
        let at = self.at(pin);
        self.write(at.community, Self::config_offset(&at), value);
    }

    /// Gives a pad to another engine (the firmware's, say).
    pub fn give_away(&mut self, pin: u16) {
        let at = self.at(pin);
        let v = self.read(at.community, at.owner.0) | 1 << at.owner.1;
        self.write(at.community, at.owner.0, v);
    }

    /// Locks a pad's configuration and, with `tx`, its output level.
    pub fn lock(&mut self, pin: u16, tx: bool) {
        let at = self.at(pin);
        let v = self.read(at.community, at.lock.0) | 1 << at.lock.1;
        self.write(at.community, at.lock.0, v);
        if tx {
            let v = self.read(at.community, at.lock.0 + 4) | 1 << at.lock.1;
            self.write(at.community, at.lock.0 + 4, v);
        }
    }

    /// The board holds an input pin at `level`.
    pub fn hold(&mut self, pin: u16, level: bool) {
        self.inputs.insert(pin, level);
    }

    /// The level a pin is driven to, if it is a GPIO with its output on.
    pub fn output(&self, pin: u16) -> Option<bool> {
        let v = self.config(pin);
        (v & PADCFG0_MODE == 0 && v & PADCFG0_TX_DISABLE == 0).then_some(v & PADCFG0_TX_STATE != 0)
    }
}
