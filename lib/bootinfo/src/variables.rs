//! The firmware's variables, as the loader hands them over.
//!
//! A PC's UEFI firmware keeps variables, some of which an operating system
//! may read while it runs (those with runtime access): the boot options,
//! and what the PC's maker keeps there for its drivers, such as a laptop's
//! speaker amplifiers' calibration. Veda runs no firmware code once it has
//! started: the loader reads those variables before it leaves the
//! firmware's boot services, into memory of their own
//! ([`BootInfo::firmware_variables`](crate::BootInfo::firmware_variables)),
//! and the kernel hands them to user space as they are, read-only.
//!
//! Layout (little-endian; each record starts 8-byte aligned):
//!
//! ```text
//! u32 magic = "EVAR", u32 count, u64 size (bytes, this header's included)
//! count x record:
//!     [u8; 16] its vendor (a GUID, as UEFI lays it out), u32 attributes,
//!     u32 its name's length (UTF-16 code units, no NUL), u32 its data's
//!     size, u32 0, the name (UTF-16LE), the data, zeros to the next record
//! ```

use core::fmt;

/// `"EVAR"`
pub const MAGIC: u32 = u32::from_le_bytes(*b"EVAR");
/// Bytes of the header before the records.
pub const HEADER_SIZE: usize = 16;
/// Bytes of a record before its name.
const RECORD_HEADER_SIZE: usize = 32;

/// UEFI's attributes of a variable.
pub mod attributes {
    pub const NON_VOLATILE: u32 = 1 << 0;
    pub const BOOTSERVICE_ACCESS: u32 = 1 << 1;
    /// An operating system may read it while it runs.
    pub const RUNTIME_ACCESS: u32 = 1 << 2;
}

/// The bytes the record of a variable takes, its name `name_len` UTF-16
/// code units long and its data `data_len` bytes (`None`: more than memory
/// holds).
pub const fn record_size(name_len: usize, data_len: usize) -> Option<usize> {
    let Some(name) = name_len.checked_mul(2) else { return None };
    let Some(size) = RECORD_HEADER_SIZE.checked_add(name) else { return None };
    let Some(size) = size.checked_add(data_len) else { return None };
    size.checked_next_multiple_of(8)
}

/// Writes variables' records into memory (the loader's side).
pub struct Writer<'a> {
    out: &'a mut [u8],
    len: usize,
    count: u32,
}

impl<'a> Writer<'a> {
    /// Writes into `out` (`None`: it cannot hold the header).
    pub fn new(out: &'a mut [u8]) -> Option<Writer<'a>> {
        (out.len() >= HEADER_SIZE).then_some(Writer { out, len: HEADER_SIZE, count: 0 })
    }

    /// Adds a variable: its vendor, its attributes, its name (without the
    /// NUL) and its data. `false` if there is no room for it.
    pub fn push(&mut self, guid: &[u8; 16], attributes: u32, name: &[u16], data: &[u8]) -> bool {
        let (Ok(name_len), Ok(data_len)) = (u32::try_from(name.len()), u32::try_from(data.len())) else {
            return false;
        };
        let Some(end) = record_size(name.len(), data.len()).and_then(|size| self.len.checked_add(size)) else {
            return false;
        };
        let Some(record) = self.out.get_mut(self.len..end) else { return false };
        let (header, rest) = record.split_at_mut(RECORD_HEADER_SIZE);
        header[..16].copy_from_slice(guid);
        for (i, v) in [attributes, name_len, data_len, 0].into_iter().enumerate() {
            header[16 + i * 4..20 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        let (name_bytes, rest) = rest.split_at_mut(name.len() * 2);
        for (b, unit) in name_bytes.as_chunks_mut::<2>().0.iter_mut().zip(name) {
            *b = unit.to_le_bytes();
        }
        let (data_bytes, padding) = rest.split_at_mut(data.len());
        data_bytes.copy_from_slice(data);
        padding.fill(0);
        self.len = end;
        self.count += 1;
        true
    }

    /// Writes the header; returns the bytes written.
    pub fn finish(self) -> usize {
        self.out[..4].copy_from_slice(&MAGIC.to_le_bytes());
        self.out[4..8].copy_from_slice(&self.count.to_le_bytes());
        self.out[8..16].copy_from_slice(&(self.len as u64).to_le_bytes());
        self.len
    }
}

/// Variables' records, checked (user space's side).
#[derive(Clone, Copy, Debug, Default)]
pub struct Variables<'a> {
    records: &'a [u8],
    count: u32,
}

impl<'a> Variables<'a> {
    /// The variables `bytes` holds (memory may follow them); `None` if they
    /// are not records of this layout, every one whole.
    pub fn parse(bytes: &'a [u8]) -> Option<Variables<'a>> {
        if u32_at(bytes, 0)? != MAGIC {
            return None;
        }
        let count = u32_at(bytes, 4)?;
        let size = usize::try_from(u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?)).ok()?;
        let records = bytes.get(HEADER_SIZE..size)?;
        let mut at = 0;
        for _ in 0..count {
            at = record_at(records, at)?.1;
        }
        (at == records.len()).then_some(Variables { records, count })
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The variables, in the order the firmware listed them.
    pub fn iter(&self) -> impl Iterator<Item = Variable<'a>> + 'a {
        let records = self.records;
        let mut at = 0;
        core::iter::from_fn(move || {
            let (v, next) = record_at(records, at)?;
            at = next;
            Some(v)
        })
    }

    /// The variable named `name` (UTF-16LE, without the NUL) of vendor
    /// `guid`.
    pub fn find(&self, guid: &[u8; 16], name: &[u8]) -> Option<Variable<'a>> {
        self.iter().find(|v| v.is(guid, name))
    }
}

/// One of the firmware's variables.
#[derive(Clone, Copy, Debug)]
pub struct Variable<'a> {
    /// Its vendor (a GUID, as UEFI lays it out).
    pub guid: [u8; 16],
    /// Its attributes ([`attributes`]).
    pub attributes: u32,
    /// Its name, UTF-16LE, without the NUL.
    pub name: &'a [u8],
    pub data: &'a [u8],
}

impl Variable<'_> {
    /// Whether it is the variable named `name` (UTF-16LE, without the NUL)
    /// of vendor `guid`.
    pub fn is(&self, guid: &[u8; 16], name: &[u8]) -> bool {
        self.guid == *guid && self.name == name
    }
}

/// `NAME-GUID`, as Linux's efivarfs names it.
impl fmt::Display for Variable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Name { name: self.name, guid: &self.guid }.fmt(f)
    }
}

/// A variable's name (UTF-16LE, without the NUL) and vendor, written as
/// Linux's efivarfs names the variable: `NAME-GUID`.
pub struct Name<'a> {
    pub name: &'a [u8],
    pub guid: &'a [u8; 16],
}

impl fmt::Display for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units = self.name.as_chunks::<2>().0.iter().map(|&b| u16::from_le_bytes(b));
        for c in char::decode_utf16(units) {
            write!(f, "{}", c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        write!(f, "-{}", Guid(self.guid))
    }
}

/// A GUID as UEFI lays it out (its first three fields little-endian),
/// written as one is: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
pub struct Guid<'a>(pub &'a [u8; 16]);

impl fmt::Display for Guid<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let g = self.0;
        let a = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
        let b = u16::from_le_bytes([g[4], g[5]]);
        let c = u16::from_le_bytes([g[6], g[7]]);
        write!(f, "{a:08x}-{b:04x}-{c:04x}-{:02x}{:02x}-", g[8], g[9])?;
        g[10..].iter().try_for_each(|x| write!(f, "{x:02x}"))
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// The record at `at` of `records`, and where the next starts.
fn record_at(records: &[u8], at: usize) -> Option<(Variable<'_>, usize)> {
    let header = records.get(at..at.checked_add(RECORD_HEADER_SIZE)?)?;
    let (name_len, data_len) = (u32_at(header, 20)? as usize, u32_at(header, 24)? as usize);
    let end = at.checked_add(record_size(name_len, data_len)?)?;
    let record = records.get(at..end)?;
    let name_end = RECORD_HEADER_SIZE + name_len * 2;
    let variable = Variable {
        guid: header[..16].try_into().ok()?,
        attributes: u32_at(header, 16)?,
        name: &record[RECORD_HEADER_SIZE..name_end],
        data: &record[name_end..name_end + data_len],
    };
    Some((variable, end))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::ToString;
    use std::vec::Vec;

    use super::*;

    /// 02f9af02-7734-4233-b43d-93fe5aa35db3, as UEFI lays it out.
    const CIRRUS: [u8; 16] =
        [0x02, 0xaf, 0xf9, 0x02, 0x34, 0x77, 0x33, 0x42, 0xb4, 0x3d, 0x93, 0xfe, 0x5a, 0xa3, 0x5d, 0xb3];

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn written(variables: &[(&[u8; 16], u32, &str, &[u8])]) -> Vec<u8> {
        let mut out = std::vec![0xAA; 4096];
        let mut w = Writer::new(&mut out).unwrap();
        for &(guid, attributes, name, data) in variables {
            assert!(w.push(guid, attributes, &utf16(name), data));
        }
        let n = w.finish();
        out.truncate(n);
        out
    }

    #[test]
    fn variables_come_back_as_written() {
        let rt = attributes::NON_VOLATILE | attributes::BOOTSERVICE_ACCESS | attributes::RUNTIME_ACCESS;
        let bytes = written(&[
            (&CIRRUS, rt, "CirrusSmartAmpCalibrationData", &[1, 2, 3]),
            (&[7; 16], attributes::RUNTIME_ACCESS, "Lang", b"eng"),
            (&[0; 16], rt, "Empty", &[]),
        ]);
        let vars = Variables::parse(&bytes).unwrap();
        assert_eq!(vars.len(), 3);
        let all: Vec<_> = vars.iter().collect();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].attributes, rt);
        assert_eq!(all[0].data, &[1, 2, 3]);
        assert_eq!(all[1].name, utf16le("Lang"));
        assert_eq!(all[2].data, &[] as &[u8]);
        let found = vars.find(&CIRRUS, &utf16le("CirrusSmartAmpCalibrationData")).unwrap();
        assert_eq!(found.data, &[1, 2, 3]);
        assert!(vars.find(&[7; 16], &utf16le("CirrusSmartAmpCalibrationData")).is_none());
        assert!(vars.find(&[7; 16], &utf16le("Lan")).is_none());
        // Records start 8-byte aligned, the padding zeroed.
        assert_eq!(bytes.len() % 8, 0);
        assert!(!bytes.contains(&0xAA));
    }

    #[test]
    fn variables_are_named_as_efivarfs_names_them() {
        let bytes = written(&[(&CIRRUS, 7, "CirrusSmartAmpCalibrationData", &[0])]);
        let v = Variables::parse(&bytes).unwrap().iter().next().unwrap();
        assert_eq!(v.to_string(), "CirrusSmartAmpCalibrationData-02f9af02-7734-4233-b43d-93fe5aa35db3");
    }

    #[test]
    fn memory_after_the_records_is_not_theirs() {
        let mut bytes = written(&[(&CIRRUS, 7, "A", &[1])]);
        bytes.resize(4096, 0);
        assert_eq!(Variables::parse(&bytes).unwrap().iter().count(), 1);
    }

    #[test]
    fn broken_records_are_refused() {
        let bytes = written(&[(&CIRRUS, 7, "Name", &[1, 2, 3, 4, 5]), (&CIRRUS, 7, "Other", &[6])]);
        assert!(Variables::parse(&bytes).is_some());
        // Cut short, anywhere.
        for n in 0..bytes.len() {
            assert!(Variables::parse(&bytes[..n]).is_none(), "{n} bytes");
        }
        // A size that claims more than there is, or a name longer than its record.
        let mut long = bytes.clone();
        long[8] = long[8].wrapping_add(8);
        assert!(Variables::parse(&long).is_none());
        let mut name = bytes.clone();
        name[HEADER_SIZE + 20] = 200;
        assert!(Variables::parse(&name).is_none());
        // More records than the size holds, or fewer.
        let mut more = bytes.clone();
        more[4] = 3;
        assert!(Variables::parse(&more).is_none());
        let mut fewer = bytes.clone();
        fewer[4] = 1;
        assert!(Variables::parse(&fewer).is_none());
        let mut magic = bytes;
        magic[0] = b'X';
        assert!(Variables::parse(&magic).is_none());
    }

    #[test]
    fn a_full_writer_takes_no_more() {
        let mut out = [0u8; 64];
        let mut w = Writer::new(&mut out).unwrap();
        assert!(w.push(&CIRRUS, 7, &utf16("ab"), &[1, 2]));
        assert!(!w.push(&CIRRUS, 7, &utf16("cd"), &[3]));
        let n = w.finish();
        assert_eq!(n, HEADER_SIZE + 40);
        assert_eq!(Variables::parse(&out[..n]).unwrap().len(), 1);
        assert!(Writer::new(&mut [0u8; 15]).is_none());
        assert_eq!(record_size(usize::MAX, 0), None);
    }
}
