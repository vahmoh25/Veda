//! A PCI function given to a guest, as the guest sees its configuration
//! space: the function's own, but for what the platform owns. The memory
//! BARs hold the guest-physical addresses the platform mapped them at (and
//! answer sizing as BARs do; other writes leave them where they are);
//! there is no expansion ROM or I/O BAR; the header type says what the
//! function's place in the guest's topology is. The interrupt pin is the
//! function's when the guest has the line its INTx is wired to (the
//! interrupt line register says which GSI that is, as a PC's firmware
//! leaves it), none otherwise.
//!
//! And the function reports no errors to the host: the errors a function
//! signals become the host's system errors, which a PC may turn into NMIs
//! (as Xen's XSA-59 found). Its SERR# enable and PCI Express error
//! reporting stay off, whatever the guest writes.
//!
//! An Intel integrated GPU has registers that are the platform's too
//! ([`ConfigSpace::intel_graphics`]); its driver also reads the PC's host
//! bridge, which the guest has a stand-in for ([`HostBridge`]).

use alloc::vec::Vec;

/// A memory BAR, where the guest has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// Its register (0 to 5; a 64-bit BAR takes the next one too).
    pub index: u8,
    /// The guest-physical address.
    pub address: u64,
    /// A power of two, at least a page.
    pub size: u64,
    pub is64: bool,
    pub prefetchable: bool,
}

/// What the guest's access to the configuration space becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// The function's own register: write it.
    Device { offset: u16, width: u8, value: u32 },
    /// The platform's: nothing to do on the device.
    Absorbed,
}

/// Configuration space beyond the header and capabilities' first 256
/// bytes is not given (extended configuration space).
pub const SIZE: u16 = 256;

/// The command register's SERR# enable: the function may signal system
/// errors.
pub const COMMAND_SERR: u16 = 1 << 8;
/// The error reporting enables of the PCI Express capability's device
/// control register (correctable, non-fatal, fatal, unsupported request).
pub const DEVCTL_ERROR_REPORTING: u16 = 0xF;
/// Where device control is in the PCI Express capability.
pub const DEVCTL: u16 = 8;
/// The PCI Express capability's id.
pub const CAP_PCI_EXPRESS: u8 = 0x10;

/// An Intel integrated GPU's registers that name the PC's memory: its
/// graphics control (GGC: the size of its stolen memory), the base of its
/// stolen memory (BDSM; from Ice Lake on BDSM's 64 bits), and its
/// OpRegion's address (ASLS); and its software SCI (SWSCI), which a write
/// sends to the PC's firmware (an SMI).
pub mod intel_graphics {
    pub const GGC: u16 = 0x50;
    pub const BDSM: u16 = 0x5C;
    pub const BDSM_64: u16 = 0xC0;
    pub const SWSCI: u16 = 0xE8;
    pub const ASLS: u16 = 0xFC;
}

const COMMAND_DWORD: u16 = 0x04;
const HEADER_TYPE_DWORD: u16 = 0x0C;
const BARS: core::ops::Range<u16> = 0x10..0x28;
const ROM: u16 = 0x30;
const INTERRUPT_DWORD: u16 = 0x3C;

/// The configuration space of a function given to a guest.
#[derive(Debug, Clone)]
pub struct ConfigSpace {
    bars: Vec<Bar>,
    /// What the header type's multi-function bit says.
    multifunction: bool,
    /// Where the function's PCI Express capability is, if it has one.
    pci_express: Option<u16>,
    /// The BAR registers the guest wrote all ones to (and reads their size
    /// from).
    sizing: u8,
    /// The function's INTx, if the guest has its line: the pin (1 for
    /// INTA# to 4) and the GSI.
    intx: Option<(u8, u8)>,
    /// An Intel integrated GPU's: where the guest's copy of its OpRegion
    /// is (0: it has none).
    intel_graphics: Option<u32>,
}

impl ConfigSpace {
    pub fn new(bars: Vec<Bar>, multifunction: bool, pci_express: Option<u16>, intx: Option<(u8, u8)>) -> ConfigSpace {
        ConfigSpace { bars, multifunction, pci_express, sizing: 0, intx, intel_graphics: None }
    }

    /// Makes it an Intel integrated GPU's, whose OpRegion the guest has a
    /// copy of at `opregion` (0: none). Its registers that name the PC's
    /// memory are the platform's: its stolen memory's, which the guest has
    /// where the PC has it, read as they are; its OpRegion's address
    /// (ASLS), the copy's. Writes to them, and to its software SCI, which
    /// would call the PC's firmware, do nothing.
    pub fn intel_graphics(&mut self, opregion: u32) {
        self.intel_graphics = Some(opregion);
    }

    /// The memory BARs.
    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    /// The value of BAR register `r` (0 to 5).
    fn bar_register(&self, r: u8) -> u32 {
        let sizing = self.sizing & (1 << r) != 0;
        if let Some(b) = self.bars.iter().find(|b| b.index == r) {
            let flags = if b.is64 { 0b100 } else { 0 } | if b.prefetchable { 0b1000 } else { 0 };
            let base = if sizing { !(b.size - 1) } else { b.address };
            return (base as u32 & 0xFFFF_FFF0) | flags;
        }
        if let Some(b) = self.bars.iter().find(|b| b.is64 && b.index + 1 == r) {
            return if sizing { (!(b.size - 1) >> 32) as u32 } else { (b.address >> 32) as u32 };
        }
        0
    }

    /// Reads `width` bytes (1, 2 or 4, aligned) at `offset`; `device`
    /// reads the function's own dword at an offset (aligned).
    pub fn read(&self, offset: u16, width: u8, device: impl FnOnce(u16) -> u32) -> u32 {
        if !aligned(offset, width) || offset >= SIZE {
            return u32::MAX >> (32 - 8 * width.clamp(1, 4) as u32);
        }
        let at = offset & !3;
        let dword = match at {
            HEADER_TYPE_DWORD => {
                let v = device(at);
                let header = if self.multifunction { 0x80 } else { 0 };
                (v & !0x00FF_0000) | (header << 16)
            }
            _ if BARS.contains(&at) => self.bar_register(((at - BARS.start) / 4) as u8),
            ROM => 0,
            // The pin and the line, or none (the line 0xFF, the pin 0).
            INTERRUPT_DWORD => {
                let (pin, line) = self.intx.unwrap_or((0, 0xFF));
                (device(at) & 0xFFFF_0000) | (pin as u32) << 8 | line as u32
            }
            intel_graphics::ASLS if let Some(opregion) = self.intel_graphics => opregion,
            _ => device(at),
        };
        part(dword, offset, width)
    }

    /// The guest writes `value` (`width` bytes) at `offset`.
    pub fn write(&mut self, offset: u16, width: u8, value: u32) -> Write {
        if !aligned(offset, width) || offset >= SIZE {
            return Write::Absorbed;
        }
        let at = offset & !3;
        if BARS.contains(&at) {
            // All ones asks for the size; anything else ends that, and
            // leaves the BAR where the platform put it.
            let r = ((at - BARS.start) / 4) as u8;
            if width == 4 && value == u32::MAX {
                self.sizing |= 1 << r;
            } else {
                self.sizing &= !(1 << r);
            }
            return Write::Absorbed;
        }
        let devctl = self.pci_express.map(|p| (p + DEVCTL) & !3);
        let platforms = [
            intel_graphics::GGC,
            intel_graphics::BDSM,
            intel_graphics::BDSM_64,
            intel_graphics::BDSM_64 + 4,
            intel_graphics::SWSCI,
            intel_graphics::ASLS,
        ];
        match at {
            ROM => Write::Absorbed,
            _ if self.intel_graphics.is_some() && platforms.contains(&at) => Write::Absorbed,
            // The header type is read-only; the interrupt line is the
            // platform's.
            HEADER_TYPE_DWORD if offset == 0x0E => Write::Absorbed,
            INTERRUPT_DWORD if offset < 0x3E => Write::Absorbed,
            // No system errors, no error messages.
            COMMAND_DWORD => Write::Device { offset, width, value: without(offset, value, COMMAND_SERR as u32) },
            _ if Some(at) == devctl => {
                let bits = (DEVCTL_ERROR_REPORTING as u32) << (8 * ((self.pci_express.unwrap_or(0) + DEVCTL) & 3));
                Write::Device { offset, width, value: without(offset, value, bits) }
            }
            _ => Write::Device { offset, width, value },
        }
    }
}

/// The PC's host bridge (00:00.0) as the guest sees it, read-only: its ids,
/// class and subsystem, and the registers an Intel integrated GPU's driver
/// reads of it (whether its MCHBAR is on, its graphics control: whether
/// VGA is decoded), as the PC has them; the rest zero, which says it has
/// no BARs and no capabilities, and decodes nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostBridge {
    /// Its vendor and device ids, class code and revision, and subsystem
    /// (the dwords at 0x00, 0x08 and 0x2C).
    pub ids: u32,
    pub class: u32,
    pub subsystem: u32,
    /// Its MCHBAR register (0x48).
    pub mchbar: u64,
    /// Its graphics control register's dword (0x50).
    pub graphics_control: u32,
}

impl HostBridge {
    /// Reads `width` bytes (1, 2 or 4, aligned) at `offset`. Writes do
    /// nothing.
    pub fn read(&self, offset: u16, width: u8) -> u32 {
        if !aligned(offset, width) || offset >= SIZE {
            return u32::MAX >> (32 - 8 * width.clamp(1, 4) as u32);
        }
        let dword = match offset & !3 {
            0x00 => self.ids,
            0x08 => self.class,
            0x2C => self.subsystem,
            0x48 => self.mchbar as u32,
            0x4C => (self.mchbar >> 32) as u32,
            intel_graphics::GGC => self.graphics_control,
            _ => 0,
        };
        part(dword, offset, width)
    }
}

fn aligned(offset: u16, width: u8) -> bool {
    matches!(width, 1 | 2 | 4) && offset.is_multiple_of(width as u16)
}

/// The `width` bytes at `offset` of the dword it falls in.
fn part(dword: u32, offset: u16, width: u8) -> u32 {
    let shift = 8 * (offset & 3) as u32;
    let mask = if width == 4 { u32::MAX } else { (1 << (8 * width as u32)) - 1 };
    (dword >> shift) & mask
}

/// `value`, written at `offset`, without `bits` (of the dword it falls in).
fn without(offset: u16, value: u32, bits: u32) -> u32 {
    value & !(bits >> (8 * (offset & 3) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A device as Intel's HD Audio controllers are: one 64-bit BAR.
    fn hda() -> ConfigSpace {
        let bar = Bar { index: 0, address: 0xC000_4000, size: 0x4000, is64: true, prefetchable: false };
        ConfigSpace::new(vec![bar], false, Some(0x60), None)
    }

    /// The function's own registers: a header that says multi-function,
    /// a BAR the device has elsewhere, an INTx pin.
    fn device(at: u16) -> u32 {
        match at {
            0x00 => 0x293E_8086,
            0x0C => 0x0080_0010,
            0x10 => 0xFEB4_0004,
            0x3C => 0x0000_010B,
            _ => 0x1234_5678,
        }
    }

    #[test]
    fn the_header_is_the_functions_but_for_the_platforms_parts() {
        let c = hda();
        assert_eq!(c.read(0x00, 4, device), 0x293E_8086);
        assert_eq!(c.read(0x02, 2, device), 0x293E);
        // Not multi-function, here.
        assert_eq!(c.read(0x0E, 1, device), 0x00);
        assert_eq!(c.read(0x0C, 4, device), 0x0000_0010);
        // The BAR where the platform put it, not where the device has it.
        assert_eq!(c.read(0x10, 4, device), 0xC000_4004);
        assert_eq!(c.read(0x14, 4, device), 0);
        assert_eq!(c.read(0x18, 4, device), 0);
        assert_eq!(c.read(0x30, 4, device), 0);
        // No INTx.
        assert_eq!(c.read(0x3D, 1, device), 0);
        assert_eq!(c.read(0x3C, 1, device), 0xFF);
        assert_eq!(c.read(0x3E, 2, device), 0);
        // INTB# on GSI 22, when the guest has its line.
        let c = ConfigSpace::new(vec![], false, None, Some((2, 22)));
        assert_eq!(c.read(0x3C, 4, device), 0x0000_0216);
        assert_eq!(c.read(0x3D, 1, device), 2);
        assert_eq!(c.read(0x40, 4, device), 0x1234_5678);
        // Beyond what is given, or unaligned: nothing.
        assert_eq!(c.read(0x100, 4, device), u32::MAX);
        assert_eq!(c.read(0x11, 2, device), 0xFFFF);
    }

    #[test]
    fn bars_answer_sizing_and_stay() {
        let mut c = hda();
        assert_eq!(c.write(0x10, 4, u32::MAX), Write::Absorbed);
        assert_eq!(c.read(0x10, 4, device), 0xFFFF_C004);
        assert_eq!(c.write(0x14, 4, u32::MAX), Write::Absorbed);
        assert_eq!(c.read(0x14, 4, device), u32::MAX);
        // Linux puts the old values back.
        c.write(0x10, 4, 0xC000_4004);
        c.write(0x14, 4, 0);
        assert_eq!(c.read(0x10, 4, device), 0xC000_4004);
        assert_eq!(c.read(0x14, 4, device), 0);
        // Moving it moves nothing.
        c.write(0x10, 4, 0xD000_0004);
        assert_eq!(c.read(0x10, 4, device), 0xC000_4004);
        // An absent BAR sizes to nothing.
        c.write(0x18, 4, u32::MAX);
        assert_eq!(c.read(0x18, 4, device), 0);
    }

    #[test]
    fn high_bars_and_prefetchable_ones() {
        let bar = Bar { index: 2, address: 0x20_0000_0000, size: 1 << 28, is64: true, prefetchable: true };
        let mut c = ConfigSpace::new(vec![bar], true, None, None);
        assert_eq!(c.read(0x18, 4, device), 0x0000_000C);
        assert_eq!(c.read(0x1C, 4, device), 0x20);
        assert_eq!(c.read(0x0E, 1, device), 0x80);
        c.write(0x18, 4, u32::MAX);
        c.write(0x1C, 4, u32::MAX);
        assert_eq!(c.read(0x18, 4, device), 0xF000_000C);
        assert_eq!(c.read(0x1C, 4, device), u32::MAX);
    }

    #[test]
    fn the_function_reports_no_errors() {
        let mut c = hda();
        // SERR# goes, the rest of the command stays; whatever the width.
        assert_eq!(c.write(0x04, 2, 0x0506), Write::Device { offset: 0x04, width: 2, value: 0x0406 });
        assert_eq!(c.write(0x04, 4, 0xF900_0146), Write::Device { offset: 0x04, width: 4, value: 0xF900_0046 });
        assert_eq!(c.write(0x05, 1, 0x05), Write::Device { offset: 0x05, width: 1, value: 0x04 });
        assert_eq!(c.write(0x04, 1, 0x06), Write::Device { offset: 0x04, width: 1, value: 0x06 });
        // Device control (the PCI Express capability at 0x60): no error
        // reporting, its other settings as written; device status as is.
        assert_eq!(c.write(0x68, 2, 0x281F), Write::Device { offset: 0x68, width: 2, value: 0x2810 });
        assert_eq!(c.write(0x68, 4, 0x000F_201F), Write::Device { offset: 0x68, width: 4, value: 0x000F_2010 });
        assert_eq!(c.write(0x6A, 2, 0x000F), Write::Device { offset: 0x6A, width: 2, value: 0x000F });
        // Without the capability, nothing there is the platform's.
        let mut plain = ConfigSpace::new(vec![], false, None, None);
        assert_eq!(plain.write(0x68, 2, 0x281F), Write::Device { offset: 0x68, width: 2, value: 0x281F });
    }

    #[test]
    fn writes_go_to_the_device_but_the_platforms_registers() {
        let mut c = hda();
        assert_eq!(c.write(0x04, 2, 0x0406), Write::Device { offset: 0x04, width: 2, value: 0x0406 });
        assert_eq!(c.write(0x0C, 1, 0x10), Write::Device { offset: 0x0C, width: 1, value: 0x10 });
        assert_eq!(c.write(0x0E, 1, 0x80), Write::Absorbed);
        assert_eq!(c.write(0x30, 4, u32::MAX), Write::Absorbed);
        assert_eq!(c.write(0x3C, 1, 11), Write::Absorbed);
        assert_eq!(c.write(0x62, 2, 0x81), Write::Device { offset: 0x62, width: 2, value: 0x81 });
        assert_eq!(c.write(0x100, 4, 1), Write::Absorbed);
        assert_eq!(c.write(0x63, 2, 1), Write::Absorbed);
    }

    #[test]
    fn an_intel_gpus_registers_that_name_the_pcs_memory_are_the_platforms() {
        // Alder Lake-P's: its stolen memory at 0x64000000, its OpRegion's
        // copy at 0xC0000.
        let gpu = |at: u16| match at {
            0x50 => 0x0000_02C3,
            0xC0 => 0x6400_0001,
            0xC4 => 0,
            0xFC => 0x5A2D_7018,
            _ => device(at),
        };
        let mut c = ConfigSpace::new(vec![], false, None, None);
        c.intel_graphics(0xC_0000);
        assert_eq!(c.read(0xFC, 4, gpu), 0xC_0000);
        assert_eq!(c.read(0xFE, 2, gpu), 0x000C);
        assert_eq!((c.read(0x50, 2, gpu), c.read(0xC0, 4, gpu)), (0x02C3, 0x6400_0001));
        for (at, width) in [(0x50, 2), (0x5C, 4), (0xC0, 4), (0xC4, 4), (0xE8, 2), (0xFC, 4), (0xFD, 1)] {
            assert_eq!(c.write(at, width, 1), Write::Absorbed, "{at:#x}");
        }
        assert_eq!(c.write(0xE4, 4, 1), Write::Device { offset: 0xE4, width: 4, value: 1 });
        // Another function's are its own.
        let mut other = ConfigSpace::new(vec![], false, None, None);
        assert_eq!(other.read(0xFC, 4, gpu), 0x5A2D_7018);
        assert_eq!(other.write(0xE8, 2, 1), Write::Device { offset: 0xE8, width: 2, value: 1 });
    }

    #[test]
    fn the_host_bridge_says_what_the_pcs_does_and_no_more() {
        let b = HostBridge {
            ids: 0x4621_8086,
            class: 0x0600_0002,
            subsystem: 0x1F62_1043,
            mchbar: 0xFEDC_0001,
            graphics_control: 0x02C3,
        };
        assert_eq!((b.read(0x00, 2), b.read(0x02, 2), b.read(0x0B, 1)), (0x8086, 0x4621, 0x06));
        assert_eq!((b.read(0x48, 4), b.read(0x4C, 4), b.read(0x50, 2)), (0xFEDC_0001, 0, 0x02C3));
        assert_eq!(b.read(0x2C, 4), 0x1F62_1043);
        // No BARs, no capabilities, single-function, decoding nothing.
        assert_eq!((b.read(0x04, 4), b.read(0x0E, 1), b.read(0x10, 4), b.read(0x34, 1)), (0, 0, 0, 0));
        assert_eq!((b.read(0xA0, 4), b.read(0x49, 2), b.read(0x100, 4)), (0, u32::MAX >> 16, u32::MAX));
    }
}
