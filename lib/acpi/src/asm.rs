//! A small AML assembler, so that the tables Veda writes (the driver VM's,
//! and those of tests and simulated boards) read like ASL:
//! `device("SPK1", &cat(&[&name("_HID", &string("CSC3551")), ...]))`.
//! Each function returns the bytes of one term; `cat` joins them. Names are
//! ASL text (`\_SB.PC00`, `^GPI0`, `SBUF`); the functions panic on a
//! malformed one (they are for names written into code).

use alloc::vec;
use alloc::vec::Vec;

use crate::name::NameString;

/// The bytes of `body` behind its PkgLength.
pub fn pkg(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let n = body.len();
    if n + 1 < 0x40 {
        out.push((n + 1) as u8);
    } else if n + 2 < 0x1000 {
        let total = n + 2;
        out.extend([0x40 | (total & 0xF) as u8, (total >> 4) as u8]);
    } else {
        let total = n + 3;
        out.extend([0x80 | (total & 0xF) as u8, (total >> 4) as u8, (total >> 12) as u8]);
    }
    out.extend_from_slice(body);
    out
}

/// A NameString.
pub fn nm(text: &str) -> Vec<u8> {
    let n = NameString::parse(text).expect("a malformed name");
    let mut out = Vec::new();
    if n.absolute {
        out.push(b'\\');
    }
    out.extend(core::iter::repeat_n(b'^', n.parents));
    match n.segs.len() {
        0 => out.push(0),
        1 => {}
        2 => out.push(0x2E),
        k => out.extend([0x2F, k as u8]),
    }
    for s in &n.segs {
        out.extend_from_slice(s);
    }
    out
}

pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// An integer, in its shortest encoding.
pub fn int(v: u64) -> Vec<u8> {
    match v {
        0 => vec![0x00],
        1 => vec![0x01],
        v if v <= 0xFF => vec![0x0A, v as u8],
        v if v <= 0xFFFF => cat(&[&[0x0B], &(v as u16).to_le_bytes()]),
        v if v <= 0xFFFF_FFFF => cat(&[&[0x0C], &(v as u32).to_le_bytes()]),
        v => cat(&[&[0x0E], &v.to_le_bytes()]),
    }
}

/// A compressed EISA id (`EisaId ("PNP0A08")`).
pub fn eisa(id: &str) -> Vec<u8> {
    let b = id.as_bytes();
    let letter = |c: u8| (c - b'@') as u32 & 0x1F;
    let hex = u32::from_str_radix(&id[3..7], 16).expect("an EISA id");
    let v = letter(b[0]) << 26 | letter(b[1]) << 21 | letter(b[2]) << 16 | hex;
    cat(&[&[0x0C], &v.swap_bytes().to_le_bytes()])
}

pub fn string(s: &str) -> Vec<u8> {
    cat(&[&[0x0D], s.as_bytes(), &[0]])
}

pub fn buffer(bytes: &[u8]) -> Vec<u8> {
    cat(&[&[0x11], &pkg(&cat(&[&int(bytes.len() as u64), bytes]))])
}

pub fn package(items: &[Vec<u8>]) -> Vec<u8> {
    cat(&[&[0x12], &pkg(&cat(&[&[items.len() as u8], &items.concat()]))])
}

pub fn scope(name: &str, body: &[u8]) -> Vec<u8> {
    cat(&[&[0x10], &pkg(&cat(&[&nm(name), body]))])
}

pub fn device(name: &str, body: &[u8]) -> Vec<u8> {
    cat(&[&[0x5B, 0x82], &pkg(&cat(&[&nm(name), body]))])
}

pub fn method(name: &str, args: u8, body: &[u8]) -> Vec<u8> {
    cat(&[&[0x14], &pkg(&cat(&[&nm(name), &[args], body]))])
}

pub fn name(n: &str, value: &[u8]) -> Vec<u8> {
    cat(&[&[0x08], &nm(n), value])
}

/// `External (name, kind, args)` (kind 6 a device, 8 a method).
pub fn external(n: &str, kind: u8, args: u8) -> Vec<u8> {
    cat(&[&[0x15], &nm(n), &[kind, args]])
}

pub fn if_(predicate: &[u8], body: &[u8]) -> Vec<u8> {
    cat(&[&[0xA0], &pkg(&cat(&[predicate, body]))])
}

pub fn else_(body: &[u8]) -> Vec<u8> {
    cat(&[&[0xA1], &pkg(body)])
}

pub fn while_(predicate: &[u8], body: &[u8]) -> Vec<u8> {
    cat(&[&[0xA2], &pkg(&cat(&[predicate, body]))])
}

pub fn ret(v: &[u8]) -> Vec<u8> {
    cat(&[&[0xA4], v])
}

pub fn store(v: &[u8], target: &[u8]) -> Vec<u8> {
    cat(&[&[0x70], v, target])
}

/// A two-operand operator with a target (`Add`, `And`, `ShiftLeft`...).
pub fn binary(op: u8, a: &[u8], b: &[u8], target: &[u8]) -> Vec<u8> {
    cat(&[&[op], a, b, target])
}

pub fn add(a: &[u8], b: &[u8]) -> Vec<u8> {
    binary(0x72, a, b, &[0])
}

pub fn lequal(a: &[u8], b: &[u8]) -> Vec<u8> {
    cat(&[&[0x93], a, b])
}

pub fn lor(a: &[u8], b: &[u8]) -> Vec<u8> {
    cat(&[&[0x91], a, b])
}

/// `CreateDWordField (buffer, offset, name)`.
pub fn create_dword_field(buffer: &str, offset: u64, field: &str) -> Vec<u8> {
    cat(&[&[0x8A], &nm(buffer), &int(offset), &nm(field)])
}

pub fn region(n: &str, space: u8, offset: &[u8], length: &[u8]) -> Vec<u8> {
    cat(&[&[0x5B, 0x80], &nm(n), &[space], offset, length])
}

/// A field list: named fields (`"SM01", 8`) and reserved bits (`"", 8`).
pub fn field(region: &str, flags: u8, fields: &[(&str, usize)]) -> Vec<u8> {
    let mut list = Vec::new();
    for &(n, bits) in fields {
        if n.is_empty() {
            list.push(0);
        } else {
            list.extend_from_slice(&nm(n));
        }
        // A field's length as a PkgLength value.
        if bits < 0x40 {
            list.push(bits as u8);
        } else if bits < 0x1000 {
            list.extend([0x40 | (bits & 0xF) as u8, (bits >> 4) as u8]);
        } else {
            list.extend([0x80 | (bits & 0xF) as u8, (bits >> 4) as u8, (bits >> 12) as u8]);
        }
    }
    cat(&[&[0x5B, 0x81], &pkg(&cat(&[&nm(region), &[flags], &list]))])
}

pub const LOCAL0: u8 = 0x60;
pub const LOCAL1: u8 = 0x61;
pub const ARG0: u8 = 0x68;
pub const ARG1: u8 = 0x69;

// --- Resource descriptors ---------------------------------------------------

/// A large descriptor: its type, then the length and `body`.
fn large(kind: u8, body: &[u8]) -> Vec<u8> {
    cat(&[&[0x80 | kind], &(body.len() as u16).to_le_bytes(), body])
}

/// `SpiSerialBusV2 (cs, PolarityLow, FourWireMode, 8, ControllerInitiated,
/// speed, ClockPolarityLow, ClockPhaseFirst, controller)`.
pub fn spi_descriptor(cs: u16, speed: u32, controller: &str) -> Vec<u8> {
    let mut d = vec![2, 0, 2, 2, 0, 0, 1, 9, 0];
    d.extend_from_slice(&speed.to_le_bytes());
    d.extend([8, 0, 0]);
    d.extend_from_slice(&cs.to_le_bytes());
    d.extend_from_slice(controller.as_bytes());
    d.push(0);
    large(0x0E, &d)
}

/// `GpioIo` (or `GpioInt`) with one pin. `flags`: the I/O restriction and
/// sharing (`GpioIo`) or the interrupt's mode, polarity and sharing.
pub fn gpio_descriptor(interrupt: bool, flags: u16, pull: u8, debounce: u16, pin: u16, controller: &str) -> Vec<u8> {
    let mut d = vec![1, if interrupt { 0 } else { 1 }, 1, 0];
    d.extend_from_slice(&flags.to_le_bytes());
    d.push(pull);
    d.extend_from_slice(&0u16.to_le_bytes());
    d.extend_from_slice(&debounce.to_le_bytes());
    // Pin table right after the fixed part (23 bytes in), then the name.
    let name_at = 23 + 2;
    d.extend_from_slice(&23u16.to_le_bytes());
    d.push(0);
    d.extend_from_slice(&(name_at as u16).to_le_bytes());
    let vendor_at = name_at + controller.len() + 1;
    d.extend_from_slice(&(vendor_at as u16).to_le_bytes());
    d.extend_from_slice(&0u16.to_le_bytes());
    d.extend_from_slice(&pin.to_le_bytes());
    d.extend_from_slice(controller.as_bytes());
    d.push(0);
    large(0x0C, &d)
}

/// `Memory32Fixed (ReadWrite, base, length)`.
pub fn memory32_fixed(base: u32, length: u32) -> Vec<u8> {
    large(0x06, &cat(&[&[1], &base.to_le_bytes(), &length.to_le_bytes()]))
}

/// `Interrupt (ResourceConsumer, Level, ActiveLow, Shared) { irq }`.
pub fn interrupt(irq: u32) -> Vec<u8> {
    large(0x09, &cat(&[&[0x0D, 1], &irq.to_le_bytes()]))
}

/// `IO (Decode16, base, base, 1, length)`.
pub fn io(base: u16, length: u8) -> Vec<u8> {
    cat(&[&[0x47, 1], &base.to_le_bytes(), &base.to_le_bytes(), &[1, length]])
}

/// `IRQNoFlags () { irq }`: an ISA interrupt (an edge, active high).
pub fn irq_no_flags(irq: u8) -> Vec<u8> {
    cat(&[&[0x22], &(1u16 << (irq & 15)).to_le_bytes()])
}

/// `WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode, 0,
/// min, max, 0, max - min + 1)`: the buses a bridge decodes.
pub fn word_bus_number(min: u16, max: u16) -> Vec<u8> {
    let fields = [0, min, max, 0, max - min + 1];
    large(0x08, &cat(&[&[2, 0x0C, 0], &fields.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()]))
}

/// `DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed,
/// NonCacheable, ReadWrite, 0, min, max, 0, max - min + 1)`: memory a
/// bridge decodes, below 4 GiB.
pub fn dword_memory(min: u32, max: u32) -> Vec<u8> {
    let fields = [0, min, max, 0, max - min + 1];
    large(0x07, &cat(&[&[0, 0x0C, 1], &fields.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()]))
}

/// `QWordMemory (...)`, as [`dword_memory`] for 64-bit addresses.
pub fn qword_memory(min: u64, max: u64) -> Vec<u8> {
    let fields = [0, min, max, 0, max - min + 1];
    large(0x0A, &cat(&[&[0, 0x0C, 1], &fields.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>()]))
}

/// A `ResourceTemplate` of `descriptors`: a buffer, with its end tag.
pub fn resource_template(descriptors: &[u8]) -> Vec<u8> {
    buffer(&cat(&[descriptors, &END_TAG]))
}

pub const END_TAG: [u8; 2] = [0x79, 0x00];
