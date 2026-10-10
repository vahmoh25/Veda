//! A PCI function given to a guest, as the guest sees its configuration
//! space: the function's own, but for what the platform owns. The memory
//! BARs hold the guest-physical addresses the platform mapped them at (and
//! answer sizing as BARs do; other writes leave them where they are);
//! there is no expansion ROM, I/O BAR or INTx; the header type says what
//! the function's place in the guest's topology is.

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
    /// The BAR registers the guest wrote all ones to (and reads their size
    /// from).
    sizing: u8,
}

impl ConfigSpace {
    pub fn new(bars: Vec<Bar>, multifunction: bool) -> ConfigSpace {
        ConfigSpace { bars, multifunction, sizing: 0 }
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
            // No INTx: the interrupt line says none, the pin is 0.
            INTERRUPT_DWORD => (device(at) & 0xFFFF_0000) | 0xFF,
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
        match at {
            ROM => Write::Absorbed,
            // The header type is read-only; the interrupt line is the
            // platform's (there is none).
            HEADER_TYPE_DWORD if offset == 0x0E => Write::Absorbed,
            INTERRUPT_DWORD if offset < 0x3E => Write::Absorbed,
            _ => Write::Device { offset, width, value },
        }
    }
}

fn aligned(offset: u16, width: u8) -> bool {
    matches!(width, 1 | 2 | 4) && offset.is_multiple_of(width as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A device as Intel's HD Audio controllers are: one 64-bit BAR.
    fn hda() -> ConfigSpace {
        let bar = Bar { index: 0, address: 0xC000_4000, size: 0x4000, is64: true, prefetchable: false };
        ConfigSpace::new(vec![bar], false)
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
        let mut c = ConfigSpace::new(vec![bar], true);
        assert_eq!(c.read(0x18, 4, device), 0x0000_000C);
        assert_eq!(c.read(0x1C, 4, device), 0x20);
        assert_eq!(c.read(0x0E, 1, device), 0x80);
        c.write(0x18, 4, u32::MAX);
        c.write(0x1C, 4, u32::MAX);
        assert_eq!(c.read(0x18, 4, device), 0xF000_000C);
        assert_eq!(c.read(0x1C, 4, device), u32::MAX);
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
