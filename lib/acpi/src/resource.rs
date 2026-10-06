//! Resource templates: what `_CRS` returns, a list of descriptors for the
//! memory, I/O ports, interrupts, GPIO pins and serial bus connections a
//! device uses (ACPI 6.5, section 6.4).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// One resource a device uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// Interrupt lines (`IRQ`, `Interrupt`).
    Irq {
        irqs: Vec<u32>,
        edge: bool,
        active_low: bool,
        shared: bool,
        wake: bool,
    },
    /// A range of I/O ports.
    Io {
        base: u16,
        length: u16,
    },
    /// A range of memory (`Memory32Fixed`, `Memory32`, `Memory24`).
    Memory {
        base: u64,
        length: u64,
        writable: bool,
    },
    /// An address window a bridge decodes (`WordIO`, `DWordMemory`,
    /// `QWordMemory`...): kind 0 memory, 1 I/O ports, 2 bus numbers.
    Window {
        kind: u8,
        min: u64,
        max: u64,
        translation: u64,
        length: u64,
    },
    Gpio(Gpio),
    Spi(Spi),
    I2c(I2c),
    /// Anything else: the small descriptor type, or `0x80` plus the large
    /// one.
    Other {
        kind: u8,
    },
}

/// A `GpioIo` or `GpioInt` connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpio {
    /// An interrupt (`GpioInt`) rather than an input or output (`GpioIo`).
    pub interrupt: bool,
    pub pins: Vec<u16>,
    /// The GPIO controller, as the firmware names it (`\_SB.GPI0`).
    pub controller: String,
    /// 0 the controller's default, 1 pull-up, 2 pull-down, 3 none.
    pub pull: u8,
    /// `GpioIo`: 0 either way, 1 input only, 2 output only, 3 as it is.
    pub restriction: u8,
    /// `GpioInt`: edge rather than level triggered.
    pub edge: bool,
    /// `GpioInt`: 0 active high, 1 active low, 2 both edges.
    pub polarity: u8,
    pub shared: bool,
    pub wake: bool,
    /// Hundredths of a millisecond.
    pub debounce: u16,
}

/// An SPI device's connection (`SpiSerialBusV2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spi {
    /// The SPI controller (`\_SB.PC00.SPI1`).
    pub controller: String,
    pub chip_select: u16,
    pub speed_hz: u32,
    pub bits: u8,
    /// The clock idles high.
    pub cpol: bool,
    /// Data is sampled on the clock's second edge.
    pub cpha: bool,
    /// The chip select is active high (rather than low).
    pub cs_active_high: bool,
    pub three_wire: bool,
}

/// An I2C device's connection (`I2cSerialBusV2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I2c {
    pub controller: String,
    pub address: u16,
    pub speed_hz: u32,
    pub ten_bit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceError {
    /// A descriptor runs past the end of the template (at this offset).
    Truncated(usize),
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceError::Truncated(at) => write!(f, "a descriptor at {:#x} runs past the template's end", at),
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u16_at(b, at)? as u32 | (u16_at(b, at + 2)? as u32) << 16)
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u32_at(b, at)? as u64 | (u32_at(b, at + 4)? as u64) << 32)
}

/// A NUL-terminated name inside a descriptor.
fn name_at(d: &[u8], at: usize) -> String {
    let rest = d.get(at..).unwrap_or(&[]);
    let n = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..n]).into_owned()
}

/// Decodes a resource template, up to its end tag.
pub fn parse(t: &[u8]) -> Result<Vec<Resource>, ResourceError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < t.len() {
        let tag = t[at];
        let (header, len) = if tag & 0x80 == 0 {
            (1, (tag & 7) as usize)
        } else {
            (3, u16_at(t, at + 1).ok_or(ResourceError::Truncated(at))? as usize)
        };
        let d = t.get(at..at + header + len).ok_or(ResourceError::Truncated(at))?;
        let decoded = if tag & 0x80 == 0 { small(tag >> 3 & 0xF, d) } else { large(tag & 0x7F, d) };
        match decoded {
            Some(Decoded::End) => return Ok(out),
            Some(Decoded::Resource(r)) => out.push(r),
            None => return Err(ResourceError::Truncated(at)),
        }
        at += header + len;
    }
    // Some firmware leaves the end tag out.
    Ok(out)
}

enum Decoded {
    Resource(Resource),
    End,
}

fn small(kind: u8, d: &[u8]) -> Option<Decoded> {
    let r = match kind {
        0x0F => return Some(Decoded::End),
        // IRQ: a mask of lines; edge-triggered, active high unless flagged.
        0x04 => {
            let mask = u16_at(d, 1)?;
            let flags = d.get(3).copied().unwrap_or(0x01);
            Resource::Irq {
                irqs: (0..16).filter(|i| mask >> i & 1 != 0).collect(),
                edge: flags & 1 != 0,
                active_low: flags & 8 != 0,
                shared: flags & 0x10 != 0,
                wake: flags & 0x20 != 0,
            }
        }
        // I/O ports: the minimum base and the length.
        0x08 => Resource::Io { base: u16_at(d, 2)?, length: *d.get(7)? as u16 },
        // Fixed I/O ports (10-bit decoding).
        0x09 => Resource::Io { base: u16_at(d, 1)? & 0x3FF, length: *d.get(3)? as u16 },
        _ => Resource::Other { kind },
    };
    Some(Decoded::Resource(r))
}

fn large(kind: u8, d: &[u8]) -> Option<Decoded> {
    let r = match kind {
        // Memory24: addresses and length in 256-byte units.
        0x01 => Resource::Memory {
            base: (u16_at(d, 4)? as u64) << 8,
            length: (u16_at(d, 10)? as u64) << 8,
            writable: d.get(3)? & 1 != 0,
        },
        // Memory32: the minimum base and the length.
        0x05 => {
            Resource::Memory { base: u32_at(d, 4)? as u64, length: u32_at(d, 16)? as u64, writable: d.get(3)? & 1 != 0 }
        }
        // Memory32Fixed
        0x06 => {
            Resource::Memory { base: u32_at(d, 4)? as u64, length: u32_at(d, 8)? as u64, writable: d.get(3)? & 1 != 0 }
        }
        // DWord, Word, QWord and Extended address spaces.
        0x07 => window(d, 4, |at| u32_at(d, at).map(u64::from))?,
        0x08 => window(d, 2, |at| u16_at(d, at).map(u64::from))?,
        0x0A => window(d, 8, |at| u64_at(d, at))?,
        0x0B => Resource::Window {
            kind: *d.get(3)?,
            min: u64_at(d, 16)?,
            max: u64_at(d, 24)?,
            translation: u64_at(d, 32)?,
            length: u64_at(d, 40)?,
        },
        // Extended interrupt
        0x09 => {
            let flags = *d.get(3)?;
            let count = *d.get(4)? as usize;
            Resource::Irq {
                irqs: (0..count).map(|i| u32_at(d, 5 + i * 4)).collect::<Option<Vec<_>>>()?,
                edge: flags & 2 != 0,
                active_low: flags & 4 != 0,
                shared: flags & 8 != 0,
                wake: flags & 0x10 != 0,
            }
        }
        0x0C => Resource::Gpio(gpio(d)?),
        0x0E => serial_bus(d)?,
        _ => Resource::Other { kind: 0x80 | kind },
    };
    Some(Decoded::Resource(r))
}

/// An address space descriptor whose numbers are `size` bytes wide.
fn window(d: &[u8], size: usize, num: impl Fn(usize) -> Option<u64>) -> Option<Resource> {
    // Type, general and type-specific flags, then granularity, minimum,
    // maximum, translation offset and length.
    let at = |i: usize| 6 + size * i;
    Some(Resource::Window {
        kind: *d.get(3)?,
        min: num(at(1))?,
        max: num(at(2))?,
        translation: num(at(3))?,
        length: num(at(4))?,
    })
}

fn gpio(d: &[u8]) -> Option<Gpio> {
    let interrupt = *d.get(4)? == 0;
    let flags = u16_at(d, 7)?;
    let pins_at = u16_at(d, 14)? as usize;
    let name_at_ = u16_at(d, 17)? as usize;
    if pins_at > name_at_ || name_at_ > d.len() {
        return None;
    }
    let pins = (pins_at..name_at_).step_by(2).map(|at| u16_at(d, at)).collect::<Option<Vec<_>>>()?;
    Some(Gpio {
        interrupt,
        pins,
        controller: name_at(d, name_at_),
        pull: *d.get(9)?,
        restriction: if interrupt { 0 } else { (flags & 3) as u8 },
        edge: interrupt && flags & 1 != 0,
        polarity: if interrupt { (flags >> 1 & 3) as u8 } else { 0 },
        shared: flags & 8 != 0,
        wake: interrupt && flags & 0x10 != 0,
        debounce: u16_at(d, 12)?,
    })
}

fn serial_bus(d: &[u8]) -> Option<Resource> {
    let kind = *d.get(5)?;
    let specific = u16_at(d, 7)?;
    let data_len = u16_at(d, 10)? as usize;
    let controller = name_at(d, 12 + data_len);
    Some(match kind {
        1 => Resource::I2c(I2c {
            controller,
            speed_hz: u32_at(d, 12)?,
            address: u16_at(d, 16)?,
            ten_bit: specific & 1 != 0,
        }),
        2 => Resource::Spi(Spi {
            controller,
            speed_hz: u32_at(d, 12)?,
            bits: *d.get(16)?,
            cpha: *d.get(17)? != 0,
            cpol: *d.get(18)? != 0,
            chip_select: u16_at(d, 19)?,
            three_wire: specific & 1 != 0,
            cs_active_high: specific & 2 != 0,
        }),
        _ => Resource::Other { kind: 0x8E },
    })
}
