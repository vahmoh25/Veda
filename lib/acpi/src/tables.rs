//! The ACPI tables: from the RSDP to the root table (the XSDT, or the RSDT
//! of older firmware), to every table it lists, and the DSDT the FADT
//! points to.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::Memory;

/// Length of the header every table starts with.
pub const HEADER_LEN: usize = 36;
/// Tables larger than this are not believed (a DSDT is under 1 MiB).
const MAX_TABLE: usize = 8 << 20;

/// One table, header included.
#[derive(Debug, Clone)]
pub struct Table {
    pub address: u64,
    pub data: Vec<u8>,
}

impl Table {
    pub fn signature(&self) -> [u8; 4] {
        [self.data[0], self.data[1], self.data[2], self.data[3]]
    }

    /// The OEM's name for the table (`SPKRAMPS`, `CpuSsdt`), trimmed.
    pub fn oem_table_id(&self) -> String {
        String::from_utf8_lossy(&self.data[16..24]).trim_end_matches([' ', '\0']).into()
    }

    /// The AML of a DSDT or SSDT.
    pub fn aml(&self) -> &[u8] {
        &self.data[HEADER_LEN..]
    }

    /// Whether the bytes add up to zero, as they should.
    pub fn checksum_ok(&self) -> bool {
        self.data.iter().fold(0u8, |s, &b| s.wrapping_add(b)) == 0
    }

    pub fn is(&self, signature: &[u8; 4]) -> bool {
        &self.signature() == signature
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    /// No RSDP at the address given.
    NoRsdp,
    /// The root table is missing or is not an XSDT or RSDT.
    BadRoot,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u32_at(b, at) as u64 | (u32_at(b, at + 4) as u64) << 32
}

/// Reads the table at `address`, or `None` if it cannot be read or its
/// length is absurd.
pub fn read(memory: &dyn Memory, address: u64) -> Option<Table> {
    let mut header = [0u8; HEADER_LEN];
    if address == 0 || !memory.read(address, &mut header) {
        return None;
    }
    let len = u32_at(&header, 4) as usize;
    if !(HEADER_LEN..=MAX_TABLE).contains(&len) {
        return None;
    }
    let mut data = vec![0u8; len];
    memory.read(address, &mut data).then_some(Table { address, data })
}

/// Every table reachable from the RSDP at `rsdp`: those the root table
/// lists, then the DSDT. Tables that cannot be read are left out.
pub fn load(memory: &dyn Memory, rsdp: u64) -> Result<Vec<Table>, TableError> {
    let mut r = [0u8; 36];
    if rsdp == 0 || !memory.read(rsdp, &mut r[..20]) || &r[..8] != b"RSD PTR " {
        return Err(TableError::NoRsdp);
    }
    // ACPI 2.0 and later: a 64-bit XSDT address after the RSDT's.
    let xsdt = if r[15] >= 2 && memory.read(rsdp, &mut r) { u64_at(&r, 24) } else { 0 };
    let (root, entry) = if xsdt != 0 { (xsdt, 8) } else { (u32_at(&r, 16) as u64, 4) };
    let root = read(memory, root).ok_or(TableError::BadRoot)?;
    if !root.is(b"XSDT") && !root.is(b"RSDT") {
        return Err(TableError::BadRoot);
    }
    let mut tables: Vec<Table> = Vec::new();
    let entries = &root.data[HEADER_LEN..];
    for i in 0..entries.len() / entry {
        let at = i * entry;
        let address = if entry == 8 { u64_at(entries, at) } else { u32_at(entries, at) as u64 };
        if let Some(t) = read(memory, address) {
            tables.push(t);
        }
    }
    // The DSDT, from the FADT: its 64-bit address when there is one.
    let dsdt = tables.iter().find(|t| t.is(b"FACP")).map(|fadt| {
        let d = &fadt.data;
        if d.len() >= 148 && u64_at(d, 140) != 0 { u64_at(d, 140) } else { u32_at(d, 40) as u64 }
    });
    if let Some(t) = dsdt.and_then(|a| read(memory, a)).filter(|t| t.is(b"DSDT")) {
        tables.push(t);
    }
    Ok(tables)
}
