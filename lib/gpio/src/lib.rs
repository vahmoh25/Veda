//! `vgpio` — GPIO controllers for Veda's device manager.
//!
//! Intel's chipsets put their GPIO pads in communities of registers whose
//! addresses the firmware gives (the controller's `_CRS`). Which ACPI pin
//! number is which pad depends on the chipset, as in Linux's pinctrl
//! drivers (`pinctrl-tigerlake.c`): the pins of each pad group are
//! numbered from a base that leaves room for 32 per group. This crate holds
//! those layouts and drives the pads through a [`Registers`] trait, so that
//! it runs in Veda (over mapped registers) and in host tests (over models)
//! alike.
//!
//! A pad that belongs to the firmware or another engine, or whose settings
//! are locked, is refused; one in another function becomes a GPIO only when
//! it has to; a level is set before the output is enabled (as Linux does).

#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

/// A community's registers.
pub trait Registers {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    /// Bytes of registers there are.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A pad group: its first pad (in the controller's numbering), its size
/// and the ACPI pin number of its first pad (`None`: not usable as GPIO).
struct Group(u16, u16, Option<u16>);

struct Community {
    groups: &'static [Group],
}

/// A controller's pads, and its registers' offsets in each community.
pub struct Layout {
    pub name: &'static str,
    ids: &'static [&'static str],
    communities: &'static [Community],
    pad_owner: usize,
    config_lock: usize,
}

/// Tiger Lake-LP, and the chipsets that kept its pins (Alder Lake-P).
const TIGER_LAKE_LP: Layout = Layout {
    name: "Tiger Lake-LP",
    ids: &["INT34C5", "INTC1055"],
    communities: &[
        Community { groups: &[Group(0, 26, Some(0)), Group(26, 16, Some(32)), Group(42, 25, Some(64))] },
        Community {
            groups: &[
                Group(67, 8, Some(96)),
                Group(75, 24, Some(128)),
                Group(99, 21, Some(160)),
                Group(120, 24, Some(192)),
                Group(144, 27, Some(224)),
            ],
        },
        Community {
            groups: &[
                Group(171, 24, Some(256)),
                Group(195, 25, Some(288)),
                Group(220, 6, None),
                Group(226, 25, Some(320)),
                Group(251, 9, None),
            ],
        },
        Community { groups: &[Group(260, 8, Some(352)), Group(268, 9, None)] },
    ],
    pad_owner: 0x20,
    config_lock: 0x80,
};

const LAYOUTS: &[&Layout] = &[&TIGER_LAKE_LP];

/// The layout of the controller with ACPI hardware id `hid`, if this
/// crate knows it.
pub fn layout(hid: &str) -> Option<&'static Layout> {
    LAYOUTS.iter().copied().find(|l| l.ids.contains(&hid))
}

// Community registers.
pub const REVID: usize = 0x000;
pub const PADBAR: usize = 0x00C;
// A pad's first configuration register.
pub const PADCFG0_TX_STATE: u32 = 1 << 0;
pub const PADCFG0_RX_STATE: u32 = 1 << 1;
pub const PADCFG0_TX_DISABLE: u32 = 1 << 8;
pub const PADCFG0_RX_DISABLE: u32 = 1 << 9;
pub const PADCFG0_MODE: u32 = 0xF << 10;
/// Routing of the pad's input to interrupts (I/O APIC, SCI, SMI, NMI).
pub const PADCFG0_ROUTES: u32 = 0xF << 17;

/// Where a pin's pad and its bits are in its community.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub community: usize,
    /// The pad within the community.
    pub pad: usize,
    /// The register owning the pad (offset), and the pad's nibble in it.
    pub owner: (usize, u32),
    /// The lock register (offset; the TX lock follows it), and the pad's bit.
    pub lock: (usize, u32),
}

impl Layout {
    /// How many register windows (communities) the controller has.
    pub fn communities(&self) -> usize {
        self.communities.len()
    }

    /// Where ACPI pin `pin` is.
    pub fn locate(&self, pin: u16) -> Option<Location> {
        for (index, community) in self.communities.iter().enumerate() {
            let first = community.groups[0].0;
            let mut owner_registers = 0usize;
            for (number, g) in community.groups.iter().enumerate() {
                let owners = (g.1 as usize).div_ceil(8);
                let Group(start, size, Some(base)) = *g else {
                    owner_registers += owners;
                    continue;
                };
                if !(base..base + size).contains(&pin) {
                    owner_registers += owners;
                    continue;
                }
                let in_group = (pin - base) as usize;
                return Some(Location {
                    community: index,
                    pad: (start - first) as usize + in_group,
                    owner: (self.pad_owner + (owner_registers + in_group / 8) * 4, (in_group % 8 * 4) as u32),
                    lock: (self.config_lock + number * 8, in_group as u32),
                });
            }
        }
        None
    }
}

/// Why a pin cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No pad has that pin number.
    NoSuchPin,
    /// The registers make no sense (no controller there).
    Registers,
    /// The pad belongs to the firmware or another engine, or is locked.
    Busy,
}

/// A pad's configuration changed: for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub community: usize,
    pub pad: usize,
    pub before: u32,
    pub after: u32,
}

/// A pad, located and checked.
struct Pad<'a, R> {
    regs: &'a R,
    config: usize,
    owned_by_host: bool,
    locked: bool,
    locked_tx: bool,
    at: Location,
}

impl<R: Registers> Pad<'_, R> {
    fn get(&self) -> u32 {
        self.regs.read(self.config)
    }

    fn set(&self, v: u32) {
        self.regs.write(self.config, v)
    }

    fn change(&self, before: u32, after: u32) -> Change {
        Change { community: self.at.community, pad: self.at.pad, before, after }
    }
}

/// A controller's pads, over its communities' registers (in the order of
/// its `_CRS`).
pub struct Pads<'a, R> {
    layout: &'static Layout,
    communities: &'a [R],
}

impl<'a, R: Registers> Pads<'a, R> {
    pub fn new(layout: &'static Layout, communities: &'a [R]) -> Pads<'a, R> {
        Pads { layout, communities }
    }

    fn pad(&self, pin: u16) -> Result<Pad<'a, R>, Error> {
        let at = self.layout.locate(pin).ok_or(Error::NoSuchPin)?;
        let regs = self.communities.get(at.community).ok_or(Error::Registers)?;
        // Four registers per pad from revision 0x92 on, two before.
        let revision = regs.read(REVID);
        let per_pad = if revision >> 16 >= 0x92 { 16 } else { 8 };
        // Registers that make no sense (no controller there) are not
        // followed outside the window.
        let padbar = regs.read(PADBAR) as usize;
        let end = padbar.checked_add((at.pad + 1) * per_pad);
        if revision == u32::MAX || end.is_none_or(|e| e > regs.len()) || at.lock.0 + 8 > regs.len() {
            return Err(Error::Registers);
        }
        Ok(Pad {
            regs,
            config: padbar + at.pad * per_pad,
            owned_by_host: regs.read(at.owner.0) >> at.owner.1 & 0xF == 0,
            locked: regs.read(at.lock.0) >> at.lock.1 & 1 != 0,
            locked_tx: regs.read(at.lock.0 + 4) >> at.lock.1 & 1 != 0,
            at,
        })
    }

    /// Reads a pin: the level it drives if it is an output, else the level
    /// on it (making it an input first if it is neither).
    pub fn read(&self, pin: u16) -> Result<(bool, Option<Change>), Error> {
        let pad = self.pad(pin)?;
        if !pad.owned_by_host {
            return Err(Error::Busy);
        }
        let before = pad.get();
        if before & PADCFG0_TX_DISABLE == 0 && before & PADCFG0_RX_DISABLE != 0 {
            return Ok((before & PADCFG0_TX_STATE != 0, None));
        }
        let mut change = None;
        if before & (PADCFG0_MODE | PADCFG0_RX_DISABLE) != 0 {
            if pad.locked {
                return Err(Error::Busy);
            }
            let v = (before & !(PADCFG0_MODE | PADCFG0_RX_DISABLE | PADCFG0_ROUTES)) | PADCFG0_TX_DISABLE;
            pad.set(v);
            change = Some(pad.change(before, v));
        }
        Ok((pad.get() & PADCFG0_RX_STATE != 0, change))
    }

    /// Drives a pin high or low, making it a GPIO output first if it is not
    /// one (the level set before the output is enabled, as Linux does).
    pub fn write(&self, pin: u16, high: bool) -> Result<Option<Change>, Error> {
        let pad = self.pad(pin)?;
        if !pad.owned_by_host || pad.locked_tx {
            return Err(Error::Busy);
        }
        let before = pad.get();
        let reconfigure = before & (PADCFG0_MODE | PADCFG0_TX_DISABLE) != 0;
        if reconfigure && pad.locked {
            return Err(Error::Busy);
        }
        let mut v = before;
        if v & PADCFG0_MODE != 0 {
            // From a native function to GPIO, as an input to begin with.
            v = (v & !(PADCFG0_MODE | PADCFG0_RX_DISABLE | PADCFG0_ROUTES)) | PADCFG0_TX_DISABLE;
            pad.set(v);
        }
        v = if high { v | PADCFG0_TX_STATE } else { v & !PADCFG0_TX_STATE };
        pad.set(v);
        if v & PADCFG0_TX_DISABLE != 0 {
            v = (v & !PADCFG0_TX_DISABLE) | PADCFG0_RX_DISABLE;
            pad.set(v);
        }
        Ok(reconfigure.then(|| pad.change(before, v)))
    }
}
