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
}

impl ConfigSpace {
    pub fn new(bars: Vec<Bar>, multifunction: bool, pci_express: Option<u16>, intx: Option<(u8, u8)>) -> ConfigSpace {
        ConfigSpace { bars, multifunction, pci_express, sizing: 0, intx }
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
            _ => device(at),
        };
        let shift = 8 * (offset & 3) as u32;
        let mask = if width == 4 { u32::MAX } else { (1 << (8 * width as u32)) - 1 };
        (dword >> shift) & mask
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
        match at {
            ROM => Write::Absorbed,
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

fn aligned(offset: u16, width: u8) -> bool {
    matches!(width, 1 | 2 | 4) && offset.is_multiple_of(width as u16)
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
}
