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
//!
//! A pad interrupts through its group's status register (its events, as
//! its configuration says: a level or an edge, inverted or not) when the
//! group's enable register lets it, and the controller raises its one
//! interrupt line while any is pending. Only a pad the firmware leaves in
//! GPIO driver mode can: one in ACPI mode raises the firmware's events
//! instead (Linux refuses those too).

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod sim;
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

impl<R: Registers + ?Sized> Registers for &R {
    fn read(&self, offset: usize) -> u32 {
        (**self).read(offset)
    }
    fn write(&self, offset: usize, value: u32) {
        (**self).write(offset, value)
    }
    fn len(&self) -> usize {
        (**self).len()
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
    /// Each group's register of pads in GPIO driver mode (a bit each).
    host_own: usize,
    /// Each group's interrupt status (write 1 to clear) and enables.
    interrupt_status: usize,
    interrupt_enable: usize,
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
    host_own: 0xB0,
    interrupt_status: 0x100,
    interrupt_enable: 0x120,
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
/// The pad's input inverted before the events (and its interrupt) see it.
pub const PADCFG0_RX_INVERT: u32 = 1 << 23;
/// What raises the pad's interrupt status: its level, an edge, both edges
/// or nothing.
pub const PADCFG0_RX_EVENT: u32 = 3 << 25;
pub const RX_EVENT_LEVEL: u32 = 0;
pub const RX_EVENT_EDGE: u32 = 1 << 25;
pub const RX_EVENT_NONE: u32 = 2 << 25;
pub const RX_EVENT_BOTH_EDGES: u32 = 3 << 25;

/// Where a pin's pad and its bits are in its community.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub community: usize,
    /// The pad within the community.
    pub pad: usize,
    /// Its group's place in the community (which of each kind of the
    /// group registers is its group's: locks, host ownership, interrupt
    /// status and enables), and its bit in them.
    pub group: usize,
    pub bit: u32,
    /// The register owning the pad (offset), and the pad's nibble in it.
    pub owner: (usize, u32),
}

/// How a pin interrupts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// While it is high, or low.
    Level { active_low: bool },
    /// When it rises, falls, or both.
    Edge { rising: bool, falling: bool },
}

impl Trigger {
    /// Its events, and whether the input is inverted for them.
    fn configuration(self) -> u32 {
        match self {
            Trigger::Level { active_low } => RX_EVENT_LEVEL | if active_low { PADCFG0_RX_INVERT } else { 0 },
            Trigger::Edge { rising: true, falling: true } => RX_EVENT_BOTH_EDGES,
            Trigger::Edge { rising: false, falling: true } => RX_EVENT_EDGE | PADCFG0_RX_INVERT,
            Trigger::Edge { rising: true, falling: false } => RX_EVENT_EDGE,
            Trigger::Edge { rising: false, falling: false } => RX_EVENT_NONE,
        }
    }
}

impl Layout {
    /// How many register windows (communities) the controller has.
    pub fn communities(&self) -> usize {
        self.communities.len()
    }

    /// The register locking a pad's configuration (its TX state's lock
    /// follows it), and the pad's bit in it.
    pub fn lock(&self, at: &Location) -> (usize, u32) {
        (self.config_lock + at.group * 8, at.bit)
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
                    group: number,
                    bit: in_group as u32,
                    owner: (self.pad_owner + (owner_registers + in_group / 8) * 4, (in_group % 8 * 4) as u32),
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
    layout: &'static Layout,
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

    /// The pad's bit in its group's register of a kind (at `base`).
    fn group_bit(&self, base: usize) -> bool {
        self.regs.read(base + self.at.group * 4) >> self.at.bit & 1 != 0
    }

    fn set_group_bit(&self, base: usize, on: bool) {
        let at = base + self.at.group * 4;
        let v = self.regs.read(at);
        self.regs.write(at, if on { v | 1 << self.at.bit } else { v & !(1 << self.at.bit) });
    }

    /// Clears the pad's interrupt status (written 1 to clear: the others'
    /// stay).
    fn clear_status(&self) {
        self.regs.write(self.layout.interrupt_status + self.at.group * 4, 1 << self.at.bit);
    }

    /// A GPIO input: the configuration `v` becomes, from any other.
    fn as_input(v: u32) -> u32 {
        (v & !(PADCFG0_MODE | PADCFG0_RX_DISABLE | PADCFG0_ROUTES)) | PADCFG0_TX_DISABLE
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
        let l = self.layout;
        let (lock, _) = l.lock(&at);
        let group_registers = [lock + 8, l.host_own + at.group * 4 + 4, l.interrupt_enable + at.group * 4 + 4];
        if revision == u32::MAX || end.is_none_or(|e| e > regs.len()) || group_registers.iter().any(|&e| e > regs.len())
        {
            return Err(Error::Registers);
        }
        Ok(Pad {
            regs,
            layout: l,
            config: padbar + at.pad * per_pad,
            owned_by_host: regs.read(at.owner.0) >> at.owner.1 & 0xF == 0,
            locked: regs.read(lock) >> at.bit & 1 != 0,
            locked_tx: regs.read(lock + 4) >> at.bit & 1 != 0,
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
            let v = Pad::<R>::as_input(before);
            pad.set(v);
            change = Some(pad.change(before, v));
        }
        Ok((pad.get() & PADCFG0_RX_STATE != 0, change))
    }

    /// Makes a pin a GPIO input, from an output or another function.
    pub fn input(&self, pin: u16) -> Result<Option<Change>, Error> {
        let pad = self.pad(pin)?;
        if !pad.owned_by_host {
            return Err(Error::Busy);
        }
        let before = pad.get();
        let v = Pad::<R>::as_input(before);
        if v == before {
            return Ok(None);
        }
        if pad.locked {
            return Err(Error::Busy);
        }
        pad.set(v);
        Ok(Some(pad.change(before, v)))
    }

    /// Whether a pin drives its level (an output) rather than reading it.
    pub fn is_output(&self, pin: u16) -> Result<bool, Error> {
        let pad = self.pad(pin)?;
        let v = pad.get();
        Ok(v & PADCFG0_MODE == 0 && v & PADCFG0_TX_DISABLE == 0)
    }

    /// Sets a pin up to interrupt as `trigger` says: a GPIO input whose
    /// events reach its group's status register (none of the I/O APIC,
    /// the SCI, SMIs and NMIs), its interrupt masked and nothing pending.
    /// A pad the firmware keeps in ACPI mode is refused: its events are
    /// the firmware's.
    pub fn set_interrupt(&self, pin: u16, trigger: Trigger) -> Result<Option<Change>, Error> {
        let pad = self.pad(pin)?;
        if !pad.owned_by_host || !pad.group_bit(self.layout.host_own) {
            return Err(Error::Busy);
        }
        let before = pad.get();
        let v = (Pad::<R>::as_input(before) & !(PADCFG0_RX_EVENT | PADCFG0_RX_INVERT)) | trigger.configuration();
        if v != before && pad.locked {
            return Err(Error::Busy);
        }
        pad.set_group_bit(self.layout.interrupt_enable, false);
        if v != before {
            pad.set(v);
        }
        pad.clear_status();
        Ok((v != before).then(|| pad.change(before, v)))
    }

    /// Unmasks (or masks) a pin's interrupt.
    pub fn enable_interrupt(&self, pin: u16, on: bool) -> Result<(), Error> {
        let pad = self.pad(pin)?;
        pad.set_group_bit(self.layout.interrupt_enable, on);
        Ok(())
    }

    /// Whether a pin's interrupt is pending, and unmasked.
    pub fn interrupt_pending(&self, pin: u16) -> Result<bool, Error> {
        let pad = self.pad(pin)?;
        Ok(pad.group_bit(self.layout.interrupt_status) && pad.group_bit(self.layout.interrupt_enable))
    }

    /// Clears a pin's pending interrupt (a level-triggered one that is
    /// still asserted is pending again at once).
    pub fn clear_interrupt(&self, pin: u16) -> Result<(), Error> {
        self.pad(pin)?.clear_status();
        Ok(())
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
            v = Pad::<R>::as_input(v);
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
