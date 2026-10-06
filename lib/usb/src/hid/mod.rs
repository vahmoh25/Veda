//! HID (HID 1.11): report descriptors, and the reports of keyboards and
//! pointing devices turned into Veda input events.
//!
//! [`ReportDescriptor::parse`] reads the items of a report descriptor
//! (HID 1.11 section 6.2.2) into a flat list of [`Field`]s: one per data
//! main item, with its report ID, position, size, usages and the
//! application collection it belongs to. [`Decoder`] picks out what
//! keyboards (the keyboard usage page) and mice and tablets (buttons, X, Y
//! and the wheels) report, and turns each input report into input events.

mod decoder;
mod keymap;

use alloc::vec::Vec;

pub use decoder::{Decoder, Leds, Pointer};
pub use keymap::key_code;

/// A usage: the usage page in the high 16 bits, the usage ID in the low 16.
pub type Usage = u32;

/// The usage `id` on usage page `page`.
pub const fn usage(page: u16, id: u16) -> Usage {
    (page as u32) << 16 | id as u32
}

/// The usage page of a usage.
pub const fn page_of(u: Usage) -> u16 {
    (u >> 16) as u16
}

/// The usage ID of a usage.
pub const fn id_of(u: Usage) -> u16 {
    u as u16
}

/// Usage pages (HID Usage Tables).
pub mod page {
    pub const GENERIC_DESKTOP: u16 = 0x01;
    pub const KEYBOARD: u16 = 0x07;
    pub const LED: u16 = 0x08;
    pub const BUTTON: u16 = 0x09;
    pub const CONSUMER: u16 = 0x0C;
}

/// The usages this module looks for.
pub mod usages {
    use super::{Usage, page, usage};

    pub const POINTER: Usage = usage(page::GENERIC_DESKTOP, 0x01);
    pub const MOUSE: Usage = usage(page::GENERIC_DESKTOP, 0x02);
    pub const KEYBOARD: Usage = usage(page::GENERIC_DESKTOP, 0x06);
    pub const KEYPAD: Usage = usage(page::GENERIC_DESKTOP, 0x07);
    pub const X: Usage = usage(page::GENERIC_DESKTOP, 0x30);
    pub const Y: Usage = usage(page::GENERIC_DESKTOP, 0x31);
    pub const WHEEL: Usage = usage(page::GENERIC_DESKTOP, 0x38);
    /// Horizontal scrolling.
    pub const AC_PAN: Usage = usage(page::CONSUMER, 0x238);
    pub const NUM_LOCK: Usage = usage(page::LED, 0x01);
    pub const CAPS_LOCK: Usage = usage(page::LED, 0x02);
    pub const SCROLL_LOCK: Usage = usage(page::LED, 0x03);
}

/// The report descriptors of the boot protocol (HID 1.11 appendix B), for
/// keyboards and mice whose own report descriptor cannot be used.
pub mod boot {
    /// Keyboard: 8 modifier bits, a reserved byte and an array of 6 keys;
    /// 5 lights.
    pub const KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x75, 0x01, 0x95, 0x08, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00,
        0x25, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01,
        0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0xFF,
        0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00, 0xC0,
    ];

    /// Mouse: 3 buttons, then X and Y movements of a byte each.
    pub const MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
        0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
        0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xC0, 0xC0,
    ];
}

/// The report a field belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// From the device (key and pointer state).
    Input,
    /// To the device (keyboard lights).
    Output,
    /// Configuration, both ways.
    Feature,
}

/// Main item flags (HID 1.11 section 6.2.2.5).
pub mod flags {
    /// Constant (padding) rather than data.
    pub const CONSTANT: u32 = 1 << 0;
    /// Each value has its own usage (a variable), rather than holding the
    /// index of a usage (an array).
    pub const VARIABLE: u32 = 1 << 1;
    /// Values are changes since the last report rather than absolute.
    pub const RELATIVE: u32 = 1 << 2;
}

/// One data main item: `count` values of `size` bits each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub kind: ReportKind,
    /// The report ID (0 when the device does not number its reports).
    pub report_id: u8,
    /// Position of the first value, in bits from the start of the report
    /// without its ID byte.
    pub offset: u32,
    /// Bits per value (1 to 32).
    pub size: u32,
    pub count: u32,
    /// [`flags`] of the main item.
    pub flags: u32,
    pub logical_min: i64,
    pub logical_max: i64,
    /// The usages in declaration order, as inclusive ranges (a single
    /// usage is a range of one).
    pub usages: Vec<(Usage, Usage)>,
    /// The usage of the application collection around the field.
    pub application: Usage,
}

impl Field {
    /// Each value has its own usage (otherwise values name usages).
    pub fn is_variable(&self) -> bool {
        self.flags & flags::VARIABLE != 0
    }

    /// Values are changes rather than positions.
    pub fn is_relative(&self) -> bool {
        self.flags & flags::RELATIVE != 0
    }

    /// Whether one of the field's usages is `u`.
    pub fn has_usage(&self, u: Usage) -> bool {
        self.usages.iter().any(|&(min, max)| (min..=max).contains(&u))
    }

    /// Whether one of the field's usages is on usage page `p`.
    pub fn has_page(&self, p: u16) -> bool {
        self.usages.iter().any(|&(min, max)| (page_of(min)..=page_of(max)).contains(&p))
    }

    /// The usage of value `index` of a variable field. When there are fewer
    /// usages than values, the last usage applies to the rest.
    pub fn usage_at(&self, index: u32) -> Option<Usage> {
        let mut left = index as u64;
        for &(min, max) in &self.usages {
            let n = (max - min) as u64 + 1;
            if left < n {
                return Some(min + left as u32);
            }
            left -= n;
        }
        self.usages.last().map(|&(_, max)| max)
    }

    /// The usage that a value of an array field selects (`None`: no usage,
    /// such as an empty slot).
    pub fn array_usage(&self, value: i64) -> Option<Usage> {
        if value < self.logical_min || value > self.logical_max {
            return None;
        }
        let mut left = (value - self.logical_min) as u64;
        for &(min, max) in &self.usages {
            let n = (max - min) as u64 + 1;
            if left < n {
                return Some(min + left as u32);
            }
            left -= n;
        }
        None
    }

    /// Whether `data` (a report without its ID byte) holds every value.
    pub fn fits(&self, data: &[u8]) -> bool {
        self.offset as u64 + self.size as u64 * self.count as u64 <= data.len() as u64 * 8
    }

    /// Value `index` from `data` (a report without its ID byte),
    /// sign-extended when the logical minimum is negative.
    pub fn value(&self, data: &[u8], index: u32) -> Option<i64> {
        if index >= self.count {
            return None;
        }
        let raw = read_bits(data, self.offset as u64 + index as u64 * self.size as u64, self.size)?;
        Some(if self.logical_min < 0 { sign_extend(raw, self.size) } else { raw as i64 })
    }
}

/// `size` (1 to 32) bits at bit `start` of `data`, least significant first.
fn read_bits(data: &[u8], start: u64, size: u32) -> Option<u64> {
    let end = start + size as u64;
    if size == 0 || size > 32 || end > data.len() as u64 * 8 {
        return None;
    }
    let (first, last) = ((start / 8) as usize, ((end - 1) / 8) as usize);
    let word = data[first..=last].iter().rev().fold(0u64, |v, &b| v << 8 | b as u64);
    Some((word >> (start % 8)) & ((1u64 << size) - 1))
}

/// Sets `size` (1 to 32) bits at bit `start` of `data` to `value`.
fn write_bits(data: &mut [u8], start: u64, size: u32, value: u64) {
    for bit in 0..size as u64 {
        let at = start + bit;
        if let Some(byte) = data.get_mut((at / 8) as usize) {
            let mask = 1u8 << (at % 8);
            if value >> bit & 1 != 0 {
                *byte |= mask;
            } else {
                *byte &= !mask;
            }
        }
    }
}

fn sign_extend(v: u64, size: u32) -> i64 {
    let shift = 64 - size;
    ((v << shift) as i64) >> shift
}

/// Why a report descriptor was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// An item runs past the end of the descriptor.
    Truncated,
    /// Pop without Push, End Collection without Collection, or nesting
    /// deeper than this parser follows.
    Nesting,
    /// A report longer than this parser accepts.
    TooLong,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            ParseError::Truncated => "the report descriptor is cut short",
            ParseError::Nesting => "the report descriptor's collections or push/pop do not match",
            ParseError::TooLong => "the report descriptor describes reports that are too long",
        })
    }
}

/// How deep collections and the global item stack may go.
const MAX_DEPTH: usize = 32;
/// The longest report accepted, in bytes.
const MAX_REPORT_LEN: u32 = 8192;

/// A parsed report descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportDescriptor {
    /// The data fields (padding is left out).
    pub fields: Vec<Field>,
    /// Whether every report starts with a report ID byte.
    pub numbered: bool,
    /// The length of each report in bits, padding included.
    lengths: Vec<(ReportKind, u8, u32)>,
}

/// The global items (HID 1.11 section 6.2.2.7) that matter here.
#[derive(Debug, Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    logical_min: i64,
    logical_max: i64,
    report_size: u32,
    report_count: u32,
    report_id: u8,
}

/// A usage as declared. A 16-bit usage takes the usage page in effect at
/// the main item it belongs to; a 32-bit one carries its own.
#[derive(Debug, Clone, Copy)]
struct Declared {
    value: u32,
    extended: bool,
}

impl Declared {
    fn resolve(self, page: u16) -> Usage {
        if self.extended { self.value } else { usage(page, self.value as u16) }
    }
}

/// The local items (HID 1.11 section 6.2.2.8) of the next main item.
#[derive(Debug, Default)]
struct Locals {
    usages: Vec<(Declared, Declared)>,
    minimum: Option<Declared>,
    maximum: Option<Declared>,
}

impl Locals {
    /// A Usage Minimum and Usage Maximum pair make a range.
    fn close_range(&mut self) {
        if let (Some(min), Some(max)) = (self.minimum, self.maximum) {
            self.usages.push((min, max));
            self.minimum = None;
            self.maximum = None;
        }
    }

    fn ranges(&self, page: u16) -> Vec<(Usage, Usage)> {
        self.usages
            .iter()
            .map(|&(min, max)| (min.resolve(page), max.resolve(page)))
            .filter(|(min, max)| min <= max)
            .collect()
    }
}

impl ReportDescriptor {
    /// Parses a report descriptor.
    pub fn parse(b: &[u8]) -> Result<ReportDescriptor, ParseError> {
        let mut out = ReportDescriptor::default();
        let mut g = Globals::default();
        let mut stack: Vec<Globals> = Vec::new();
        let mut l = Locals::default();
        // (collection type, usage) from the outermost collection in.
        let mut collections: Vec<(u8, Usage)> = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let prefix = b[i];
            i += 1;
            if prefix == 0xFE {
                // A long item: data size, tag, data. None are defined.
                let size = *b.get(i).ok_or(ParseError::Truncated)? as usize;
                i += 2 + size;
                if i > b.len() {
                    return Err(ParseError::Truncated);
                }
                continue;
            }
            let size = [0, 1, 2, 4][(prefix & 3) as usize];
            let data = b.get(i..i + size).ok_or(ParseError::Truncated)?;
            i += size;
            let unsigned = data.iter().rev().fold(0u32, |v, &x| v << 8 | x as u32);
            let signed = match size {
                1 => unsigned as u8 as i8 as i64,
                2 => unsigned as u16 as i16 as i64,
                4 => unsigned as i32 as i64,
                _ => 0,
            };
            match ((prefix >> 2) & 3, prefix >> 4) {
                // Main items: Input, Output, Feature.
                (0, tag @ (0x8 | 0x9 | 0xB)) => {
                    let kind = match tag {
                        0x8 => ReportKind::Input,
                        0x9 => ReportKind::Output,
                        _ => ReportKind::Feature,
                    };
                    let bits = g.report_size.checked_mul(g.report_count).ok_or(ParseError::TooLong)?;
                    let offset = out.extend_report(kind, g.report_id, bits)?;
                    let usages = l.ranges(g.usage_page);
                    let data = unsigned & flags::CONSTANT == 0 && !usages.is_empty();
                    if data && (1..=32).contains(&g.report_size) && g.report_count > 0 {
                        out.fields.push(Field {
                            kind,
                            report_id: g.report_id,
                            offset,
                            size: g.report_size,
                            count: g.report_count,
                            flags: unsigned,
                            logical_min: g.logical_min,
                            logical_max: g.logical_max,
                            usages,
                            application: application(&collections),
                        });
                    }
                    l = Locals::default();
                }
                // Collection: its usage is the first local usage.
                (0, 0xA) => {
                    if collections.len() >= MAX_DEPTH {
                        return Err(ParseError::Nesting);
                    }
                    let u = l.ranges(g.usage_page).first().map_or(0, |r| r.0);
                    collections.push((unsigned as u8, u));
                    l = Locals::default();
                }
                // End Collection.
                (0, 0xC) => {
                    collections.pop().ok_or(ParseError::Nesting)?;
                    l = Locals::default();
                }
                (0, _) => l = Locals::default(),
                // Global items.
                (1, 0x0) => g.usage_page = unsigned as u16,
                (1, 0x1) => g.logical_min = signed,
                // Many devices write a maximum such as 255 in one byte; it
                // is only negative when the minimum is.
                (1, 0x2) => g.logical_max = if g.logical_min < 0 { signed } else { unsigned as i64 },
                (1, 0x7) => g.report_size = unsigned,
                (1, 0x8) => {
                    g.report_id = unsigned as u8;
                    out.numbered = true;
                }
                (1, 0x9) => g.report_count = unsigned,
                (1, 0xA) => {
                    if stack.len() >= MAX_DEPTH {
                        return Err(ParseError::Nesting);
                    }
                    stack.push(g);
                }
                (1, 0xB) => g = stack.pop().ok_or(ParseError::Nesting)?,
                // Local items: Usage, Usage Minimum, Usage Maximum.
                (2, 0x0) => {
                    let d = Declared { value: unsigned, extended: size == 4 };
                    l.usages.push((d, d));
                }
                (2, 0x1) => {
                    l.minimum = Some(Declared { value: unsigned, extended: size == 4 });
                    l.close_range();
                }
                (2, 0x2) => {
                    l.maximum = Some(Declared { value: unsigned, extended: size == 4 });
                    l.close_range();
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Appends `bits` to a report; returns where they start.
    fn extend_report(&mut self, kind: ReportKind, id: u8, bits: u32) -> Result<u32, ParseError> {
        let at = match self.lengths.iter().position(|&(k, i, _)| k == kind && i == id) {
            Some(at) => at,
            None => {
                self.lengths.push((kind, id, 0));
                self.lengths.len() - 1
            }
        };
        let start = self.lengths[at].2;
        let end = start.checked_add(bits).filter(|&end| end <= MAX_REPORT_LEN * 8).ok_or(ParseError::TooLong)?;
        self.lengths[at].2 = end;
        Ok(start)
    }

    /// The length of a report in bytes, without the report ID byte.
    pub fn report_len(&self, kind: ReportKind, id: u8) -> usize {
        self.lengths
            .iter()
            .find(|&&(k, i, _)| k == kind && i == id)
            .map_or(0, |&(_, _, bits)| bits.div_ceil(8) as usize)
    }

    /// The length of the longest input report in bytes, with the report ID
    /// byte.
    pub fn max_input_len(&self) -> usize {
        let longest = self
            .lengths
            .iter()
            .filter(|&&(k, _, _)| k == ReportKind::Input)
            .map(|&(_, _, bits)| bits.div_ceil(8) as usize)
            .max()
            .unwrap_or(0);
        longest + self.numbered as usize
    }
}

/// The usage of the innermost application collection.
fn application(collections: &[(u8, Usage)]) -> Usage {
    const APPLICATION: u8 = 1;
    collections.iter().rev().find(|&&(kind, _)| kind == APPLICATION).map_or(0, |&(_, u)| u)
}

#[cfg(test)]
mod tests;
