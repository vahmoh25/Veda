//! A controller as its registers behave, for tests: devmgr drives one in
//! place of a PC's when a test's ACPI table describes it, and host tests
//! use it too. Every pad starts host-owned, unlocked, in GPIO driver mode,
//! a GPIO input with nothing on its pin. The pins are wired in pairs: an
//! even pad of a group, when it is an output, drives the pin of the odd
//! pad after it, as a test board's loopback wire would. A pad's events
//! (its level, or an edge, inverted as its configuration says) set its
//! group's interrupt status, which writing 1 clears; the controller raises
//! its line while any status is set that its group's enables let through.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::{
    Layout, PADBAR, PADCFG0_MODE, PADCFG0_RX_EVENT, PADCFG0_RX_INVERT, PADCFG0_RX_STATE, PADCFG0_TX_DISABLE,
    PADCFG0_TX_STATE, REVID, RX_EVENT_BOTH_EDGES, RX_EVENT_EDGE, RX_EVENT_LEVEL, Registers,
};

/// Where the pads' configuration starts, in every community (four
/// registers a pad, from revision 0x92 on).
const PADS_AT: usize = 0x700;
const PAD_REGISTERS: usize = 16;
/// A community's registers.
const WINDOW: usize = 0x1_0000;

pub struct Controller {
    layout: &'static Layout,
    communities: RefCell<Vec<BTreeMap<usize, u32>>>,
}

/// One community of a [`Controller`]'s registers.
pub struct Community<'a> {
    controller: &'a Controller,
    index: usize,
}

impl Controller {
    pub fn new(layout: &'static Layout) -> Controller {
        let c = Controller { layout, communities: RefCell::new(Vec::new()) };
        {
            let mut communities = c.communities.borrow_mut();
            for community in layout.communities {
                let mut regs = BTreeMap::from([(REVID, 0x0092_0000), (PADBAR, PADS_AT as u32)]);
                // GPIO driver mode for every group.
                for group in 0..community.groups.len() {
                    regs.insert(layout.host_own + group * 4, u32::MAX);
                }
                communities.push(regs);
            }
        }
        // Every pad a GPIO input to begin with.
        for (community, pads) in c.pads().into_iter().enumerate() {
            for pad in 0..pads {
                c.set(community, PADS_AT + pad * PAD_REGISTERS, PADCFG0_TX_DISABLE);
            }
        }
        c
    }

    /// The communities, to drive the pads over.
    pub fn communities(&self) -> Vec<Community<'_>> {
        (0..self.layout.communities()).map(|index| Community { controller: self, index }).collect()
    }

    /// Whether the controller raises its line: some pad's interrupt is
    /// pending and enabled.
    pub fn line(&self) -> bool {
        let communities = self.communities.borrow();
        communities.iter().zip(self.layout.communities).any(|(regs, community)| {
            (0..community.groups.len()).any(|group| {
                let at = |base: usize| regs.get(&(base + group * 4)).copied().unwrap_or(0);
                at(self.layout.interrupt_status) & at(self.layout.interrupt_enable) != 0
            })
        })
    }

    /// How many pads each community has.
    fn pads(&self) -> Vec<usize> {
        self.layout.communities.iter().map(|c| c.groups.iter().map(|g| g.1 as usize).sum()).collect()
    }

    fn get(&self, community: usize, offset: usize) -> u32 {
        self.communities.borrow()[community].get(&offset).copied().unwrap_or(0)
    }

    fn set(&self, community: usize, offset: usize, value: u32) {
        self.communities.borrow_mut()[community].insert(offset, value);
    }

    /// The group of pad `pad` of a community, and its bit in the group's
    /// registers.
    fn group_of(&self, community: usize, pad: usize) -> Option<(usize, u32)> {
        let mut first = 0;
        for (number, g) in self.layout.communities[community].groups.iter().enumerate() {
            let size = g.1 as usize;
            if pad < first + size {
                return Some((number, (pad - first) as u32));
            }
            first += size;
        }
        None
    }

    /// A pad's configuration register was written: what it drives reaches
    /// the pin it is wired to; what its pin carries, its events.
    fn configured(&self, community: usize, pad: usize) {
        let config = self.get(community, PADS_AT + pad * PAD_REGISTERS);
        if let Some((_, bit)) = self.group_of(community, pad)
            && bit % 2 == 0
        {
            let output = config & PADCFG0_MODE == 0 && config & PADCFG0_TX_DISABLE == 0;
            if output {
                self.drive(community, pad + 1, config & PADCFG0_TX_STATE != 0);
            }
        }
        self.events(community, pad, None);
    }

    /// The pin of `pad` carries `high`: its input, and its events.
    fn drive(&self, community: usize, pad: usize, high: bool) {
        let at = PADS_AT + pad * PAD_REGISTERS;
        let before = self.get(community, at);
        let after = if high { before | PADCFG0_RX_STATE } else { before & !PADCFG0_RX_STATE };
        self.set(community, at, after);
        self.events(community, pad, Some(before & PADCFG0_RX_STATE != 0));
    }

    /// Sets a pad's interrupt status if its input makes an event: its
    /// level (inverted as configured) now, or an edge from `was`.
    fn events(&self, community: usize, pad: usize, was: Option<bool>) {
        let Some((group, bit)) = self.group_of(community, pad) else { return };
        let config = self.get(community, PADS_AT + pad * PAD_REGISTERS);
        let invert = config & PADCFG0_RX_INVERT != 0;
        let now = (config & PADCFG0_RX_STATE != 0) != invert;
        let before = was.map(|w| w != invert);
        let event = match config & PADCFG0_RX_EVENT {
            RX_EVENT_LEVEL => now,
            RX_EVENT_EDGE => before == Some(false) && now,
            RX_EVENT_BOTH_EDGES => before.is_some_and(|b| b != now),
            _ => false,
        };
        if event {
            let status = self.layout.interrupt_status + group * 4;
            self.set(community, status, self.get(community, status) | 1 << bit);
        }
    }
}

impl Registers for Community<'_> {
    fn read(&self, offset: usize) -> u32 {
        self.controller.get(self.index, offset)
    }

    fn write(&self, offset: usize, value: u32) {
        let c = self.controller;
        let groups = c.layout.communities[self.index].groups;
        let status = c.layout.interrupt_status..c.layout.interrupt_status + groups.len() * 4;
        if status.contains(&offset) {
            // Written 1 to clear; a level still there is pending again.
            c.set(self.index, offset, c.get(self.index, offset) & !value);
            let group = (offset - status.start) / 4;
            if let Some(g) = groups.get(group) {
                let first: usize = groups[..group].iter().map(|g| g.1 as usize).sum();
                for bit in (0..g.1 as usize).filter(|b| value >> b & 1 != 0) {
                    c.events(self.index, first + bit, None);
                }
            }
            return;
        }
        // The input state is the pin's, not the register's.
        if offset >= PADS_AT && (offset - PADS_AT).is_multiple_of(PAD_REGISTERS) {
            let pad = (offset - PADS_AT) / PAD_REGISTERS;
            let input = c.get(self.index, offset) & PADCFG0_RX_STATE;
            c.set(self.index, offset, (value & !PADCFG0_RX_STATE) | input);
            c.configured(self.index, pad);
            return;
        }
        c.set(self.index, offset, value);
    }

    fn len(&self) -> usize {
        WINDOW
    }
}
