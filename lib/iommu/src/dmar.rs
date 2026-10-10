//! The DMAR table: where the firmware says the remapping hardware is, which
//! devices each unit translates, and which memory devices use before the
//! system takes over (Intel VT-d specification, chapter 8).

use alloc::vec::Vec;

/// The table's flags.
pub const INTR_REMAP: u8 = 1 << 0;
pub const X2APIC_OPT_OUT: u8 = 1 << 1;

/// What a device scope names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    /// A PCI endpoint.
    Endpoint,
    /// A PCI bridge and every device below it.
    Bridge,
    /// An I/O APIC, by its id.
    IoApic(u8),
    /// An HPET, by its number.
    Hpet(u8),
    /// A device of the ACPI namespace.
    Namespace(u8),
}

/// A device scope: a device, as a start bus and a path of (device,
/// function) hops through bridges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub bus: u8,
    pub path: Vec<(u8, u8)>,
}

impl Scope {
    /// The source id (bus, device, function) of a scope whose path has one
    /// hop: the device is on the start bus. Longer paths go through bridges
    /// whose bus numbers only the bridges' configuration says.
    pub fn source_id(&self) -> Option<u16> {
        match self.path.as_slice() {
            [(dev, func)] => Some(((self.bus as u16) << 8) | ((*dev as u16 & 0x1F) << 3) | (*func as u16 & 7)),
            _ => None,
        }
    }
}

/// A remapping unit (DRHD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub segment: u16,
    /// Where its registers are.
    pub base: u64,
    /// It translates every device of its segment that no other unit names.
    pub include_all: bool,
    pub scopes: Vec<Scope>,
}

/// Memory that devices use for the firmware (RMRR): it must stay reachable
/// for them as long as the firmware's work may go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserved {
    pub segment: u16,
    pub base: u64,
    /// The last byte.
    pub limit: u64,
    pub scopes: Vec<Scope>,
}

/// The table, read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dmar {
    /// The host address width (bits).
    pub address_width: u8,
    pub flags: u8,
    pub units: Vec<Unit>,
    pub reserved: Vec<Reserved>,
}

/// What is wrong with a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmarError {
    NotDmar,
    Truncated,
}

impl core::fmt::Display for DmarError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            DmarError::NotDmar => "not a DMAR table",
            DmarError::Truncated => "the table is cut short",
        })
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// The device scopes in `b`.
fn scopes(mut b: &[u8]) -> Result<Vec<Scope>, DmarError> {
    let mut out = Vec::new();
    while b.len() >= 6 {
        let (kind, len) = (b[0], b[1] as usize);
        if len < 6 || len > b.len() || !(len - 6).is_multiple_of(2) {
            return Err(DmarError::Truncated);
        }
        let id = b[4];
        let kind = match kind {
            1 => ScopeKind::Endpoint,
            2 => ScopeKind::Bridge,
            3 => ScopeKind::IoApic(id),
            4 => ScopeKind::Hpet(id),
            5 => ScopeKind::Namespace(id),
            _ => {
                b = &b[len..];
                continue;
            }
        };
        let path = b[6..len].as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect();
        out.push(Scope { kind, bus: b[5], path });
        b = &b[len..];
    }
    Ok(out)
}

/// Reads a DMAR table (all of it, from its header).
pub fn parse(table: &[u8]) -> Result<Dmar, DmarError> {
    if table.len() < 48 || &table[0..4] != b"DMAR" {
        return Err(DmarError::NotDmar);
    }
    let len = (u32::from_le_bytes([table[4], table[5], table[6], table[7]]) as usize).min(table.len());
    let mut dmar = Dmar { address_width: table[36] + 1, flags: table[37], ..Dmar::default() };
    let mut at = 48;
    while at + 4 <= len {
        let kind = u16_at(table, at);
        let size = u16_at(table, at + 2) as usize;
        if size < 4 || at + size > len {
            return Err(DmarError::Truncated);
        }
        let s = &table[at..at + size];
        match kind {
            0 if size >= 16 => dmar.units.push(Unit {
                include_all: s[4] & 1 != 0,
                segment: u16_at(s, 6),
                base: u64_at(s, 8),
                scopes: scopes(&s[16..])?,
            }),
            1 if size >= 24 => dmar.reserved.push(Reserved {
                segment: u16_at(s, 6),
                base: u64_at(s, 8),
                limit: u64_at(s, 16),
                scopes: scopes(&s[24..])?,
            }),
            _ => {}
        }
        at += size;
    }
    Ok(dmar)
}

impl Dmar {
    /// The unit that translates the device with source id `sid` of
    /// `segment`: the one whose scopes name it, else the one that takes
    /// the rest of the segment. A unit that names a bridge translates the
    /// devices below it, whose buses only the bridge's configuration says:
    /// while one does, a device it does not name has no unit here.
    pub fn unit_for(&self, segment: u16, sid: u16) -> Option<usize> {
        let units = || self.units.iter().enumerate().filter(|(_, u)| u.segment == segment);
        let named = units().find(|(_, u)| {
            u.scopes.iter().any(|s| matches!(s.kind, ScopeKind::Endpoint) && s.source_id() == Some(sid))
        });
        if let Some((i, _)) = named {
            return Some(i);
        }
        let bridges = units().any(|(_, u)| !u.include_all && u.scopes.iter().any(|s| s.kind == ScopeKind::Bridge));
        if bridges {
            return None;
        }
        units().find(|(_, u)| u.include_all).map(|(i, _)| i)
    }

    /// The source id of I/O APIC `id`, if the table names it.
    pub fn ioapic_source(&self, id: u8) -> Option<u16> {
        self.units.iter().flat_map(|u| &u.scopes).find(|s| s.kind == ScopeKind::IoApic(id))?.source_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    /// A table as QEMU's intel-iommu makes it: one unit for everything,
    /// naming the I/O APIC; and an RMRR for a USB controller.
    pub fn qemu_table() -> Vec<u8> {
        let mut t = vec![0u8; 48];
        t[0..4].copy_from_slice(b"DMAR");
        t[36] = 38; // 39-bit host addresses
        t[37] = INTR_REMAP;
        // DRHD: INCLUDE_PCI_ALL, segment 0, at 0xFED90000, with an I/O
        // APIC scope (id 0, at 00:1f.7... as QEMU names it: bus 0xf0, 1f.0).
        let mut drhd = vec![0u8; 16];
        drhd[0..2].copy_from_slice(&0u16.to_le_bytes());
        drhd[4] = 1;
        drhd[8..16].copy_from_slice(&0xFED9_0000u64.to_le_bytes());
        drhd.extend_from_slice(&[3, 8, 0, 0, 0, 0xF0, 0x1F, 0x00]);
        let size = drhd.len() as u16;
        drhd[2..4].copy_from_slice(&size.to_le_bytes());
        t.extend_from_slice(&drhd);
        // RMRR for 00:14.0, 0x3E000000 to 0x3E0FFFFF.
        let mut rmrr = vec![0u8; 24];
        rmrr[0..2].copy_from_slice(&1u16.to_le_bytes());
        rmrr[8..16].copy_from_slice(&0x3E00_0000u64.to_le_bytes());
        rmrr[16..24].copy_from_slice(&0x3E0F_FFFFu64.to_le_bytes());
        rmrr.extend_from_slice(&[1, 8, 0, 0, 0, 0, 0x14, 0]);
        let size = rmrr.len() as u16;
        rmrr[2..4].copy_from_slice(&size.to_le_bytes());
        t.extend_from_slice(&rmrr);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        t
    }

    #[test]
    fn reads_qemus_table() {
        let d = parse(&qemu_table()).unwrap();
        assert_eq!(d.address_width, 39);
        assert_eq!(d.flags, INTR_REMAP);
        assert_eq!(d.units.len(), 1);
        let u = &d.units[0];
        assert!(u.include_all);
        assert_eq!(u.base, 0xFED9_0000);
        assert_eq!(u.scopes, vec![Scope { kind: ScopeKind::IoApic(0), bus: 0xF0, path: vec![(0x1F, 0)] }]);
        assert_eq!(d.ioapic_source(0), Some(0xF0F8));
        assert_eq!(d.ioapic_source(1), None);
        assert_eq!(d.reserved.len(), 1);
        assert_eq!(d.reserved[0].limit, 0x3E0F_FFFF);
        assert_eq!(d.reserved[0].scopes[0].source_id(), Some(0xA0));
        assert_eq!(d.unit_for(0, 0x0200), Some(0));
        assert_eq!(d.unit_for(1, 0x0200), None);
    }

    #[test]
    fn named_devices_go_to_their_unit() {
        let mut d = parse(&qemu_table()).unwrap();
        d.units.insert(
            0,
            Unit {
                segment: 0,
                base: 0xFED9_1000,
                include_all: false,
                scopes: vec![Scope { kind: ScopeKind::Endpoint, bus: 0, path: vec![(2, 0)] }],
            },
        );
        assert_eq!(d.unit_for(0, 0x0010), Some(0));
        assert_eq!(d.unit_for(0, 0x0018), Some(1));
        // A unit that names a bridge: what is below it is not known here.
        d.units[0].scopes.push(Scope { kind: ScopeKind::Bridge, bus: 0, path: vec![(0x1C, 0)] });
        assert_eq!(d.unit_for(0, 0x0010), Some(0));
        assert_eq!(d.unit_for(0, 0x0300), None);
    }

    #[test]
    fn refuses_what_is_not_a_table() {
        assert_eq!(parse(b"APIC"), Err(DmarError::NotDmar));
        let mut t = qemu_table();
        t[50] = 0xFF;
        t[51] = 0xFF;
        assert_eq!(parse(&t), Err(DmarError::Truncated));
    }
}
