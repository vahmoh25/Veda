//! A small AML assembler, so that the tables Veda writes (the driver VM's,
//! and those of tests and simulated boards) read like ASL:
//! `device("SPK1", &cat(&[&name("_HID", &string("CSC3551")), ...]))`.
//! Each function returns the bytes of one term; `cat` joins them. Names are
//! ASL text (`\_SB.PC00`, `^GPI0`, `SBUF`); the functions panic on a
//! malformed one (they are for names written into code).

use alloc::vec;
use alloc::vec::Vec;

use crate::name::NameString;
use crate::value::Value;

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
pub const ARG2: u8 = 0x6A;

/// The AML of a constant: an integer, a string, a buffer, or a package of
/// them (`None` for anything else, a reference say).
pub fn constant(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Integer(i) => Some(int(*i)),
        Value::String(s) => Some(string(s)),
        Value::Buffer(b) => Some(buffer(b)),
        Value::Package(items) if items.len() <= 0xFF => {
            Some(package(&items.iter().map(constant).collect::<Option<Vec<_>>>()?))
        }
        _ => None,
    }
}

/// The bytes `ToUUID ("...")` makes of a UUID's text: the first three
/// fields little-endian, the rest as written. `None` if it is not one.
pub fn uuid(text: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = text
        .split('-')
        .flat_map(|g| g.as_bytes().chunks(2))
        .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<_>>()?;
    let groups: Vec<usize> = text.split('-').map(str::len).collect();
    if hex.len() != 16 || groups != [8, 4, 4, 4, 12] {
        return None;
    }
    let mut u = [0u8; 16];
    u[..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    u[4..8].copy_from_slice(&[hex[5], hex[4], hex[7], hex[6]]);
    u[8..].copy_from_slice(&hex[8..]);
    Some(u)
}

/// A `_DSM` that answers function `function` of `uuid` (at any revision)
/// with `answer` (an AML term), and function 0 with the functions there
/// are; anything else with nothing.
pub fn dsm(uuid: &[u8; 16], function: u8, answer: &[u8]) -> Vec<u8> {
    let functions = buffer(&[1 | 1 << function]);
    method(
        "_DSM",
        4,
        &cat(&[
            &if_(
                &lequal(&[ARG0], &buffer(uuid)),
                &cat(&[
                    &if_(&lequal(&[ARG2], &int(0)), &ret(&functions)),
                    &if_(&lequal(&[ARG2], &int(function as u64)), &ret(answer)),
                ]),
            ),
            &ret(&buffer(&[0])),
        ]),
    )
}

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
    interrupt_of(irq, false, true, true, false)
}

/// `Interrupt (ResourceConsumer, ...) { irq }` as given: edge- or
/// level-triggered, active low or high, shared or not, a wake source or
/// not.
pub fn interrupt_of(irq: u32, edge: bool, active_low: bool, shared: bool, wake: bool) -> Vec<u8> {
    let flags = 1 | (edge as u8) << 1 | (active_low as u8) << 2 | (shared as u8) << 3 | (wake as u8) << 4;
    large(0x09, &cat(&[&[flags, 1], &irq.to_le_bytes()]))
}

/// `I2cSerialBusV2 (address, ControllerInitiated, speed, 7- or 10-bit
/// addressing, controller)`.
pub fn i2c_descriptor(address: u16, speed: u32, ten_bit: bool, controller: &str) -> Vec<u8> {
    let mut d = vec![2, 0, 1, 2];
    d.extend_from_slice(&(ten_bit as u16).to_le_bytes());
    d.push(1);
    d.extend_from_slice(&6u16.to_le_bytes());
    d.extend_from_slice(&speed.to_le_bytes());
    d.extend_from_slice(&address.to_le_bytes());
    d.extend_from_slice(controller.as_bytes());
    d.push(0);
    large(0x0E, &d)
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
