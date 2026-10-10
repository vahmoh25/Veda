//! A VT-d remapping unit's registers, and the structures it reads from
//! memory: root and context entries (which domain a device is in), the
//! second-level page tables of a domain, interrupt remapping entries, and
//! the descriptors of the invalidation queue. All in the legacy format
//! (Intel VT-d specification, chapters 9-11), which every unit has.

/// Register offsets.
pub mod reg {
    pub const VER: usize = 0x00;
    pub const CAP: usize = 0x08;
    pub const ECAP: usize = 0x10;
    pub const GCMD: usize = 0x18;
    pub const GSTS: usize = 0x1C;
    pub const RTADDR: usize = 0x20;
    pub const FSTS: usize = 0x34;
    pub const FECTL: usize = 0x38;
    pub const FEDATA: usize = 0x3C;
    pub const FEADDR: usize = 0x40;
    pub const FEUADDR: usize = 0x44;
    pub const PMEN: usize = 0x64;
    pub const IQH: usize = 0x80;
    pub const IQT: usize = 0x88;
    pub const IQA: usize = 0x90;
    pub const ICS: usize = 0x9C;
    pub const IRTA: usize = 0xB8;
}

/// Bits of the global command and status registers.
pub mod gcmd {
    pub const TE: u32 = 1 << 31;
    pub const SRTP: u32 = 1 << 30;
    pub const QIE: u32 = 1 << 26;
    pub const IRE: u32 = 1 << 25;
    pub const SIRTP: u32 = 1 << 24;
    pub const CFI: u32 = 1 << 23;
    /// The commands that are states (kept when another command is given);
    /// the others are one-shot.
    pub const STATES: u32 = TE | QIE | IRE | CFI;
}

/// `PMEN`: protected memory (some firmware leaves it on).
pub const PMEN_EPM: u32 = 1 << 31;
pub const PMEN_PRS: u32 = 1 << 0;

/// Bits of the fault status register (`FSTS`); all but `PPF` are cleared
/// by writing them.
pub mod fsts {
    /// Faults came while every fault recording register was full.
    pub const PFO: u32 = 1 << 0;
    /// A fault recording register holds a fault.
    pub const PPF: u32 = 1 << 1;
    /// The invalidation queue met a descriptor it could not do.
    pub const IQE: u32 = 1 << 4;
    pub const ICE: u32 = 1 << 5;
    pub const ITE: u32 = 1 << 6;
    /// The errors, to clear.
    pub const ERRORS: u32 = PFO | IQE | ICE | ITE;

    /// The first fault recording register that holds a fault.
    pub fn first_record(fsts: u32) -> usize {
        ((fsts >> 8) & 0xFF) as usize
    }
}

/// `FECTL`: the fault event interrupt is masked.
pub const FECTL_IM: u32 = 1 << 31;

/// What the capability register says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap(pub u64);

impl Cap {
    /// How many domain ids the unit has.
    pub fn domains(&self) -> u32 {
        1 << (4 + 2 * (self.0 & 7) as u32)
    }

    /// Caching mode: the unit may cache entries that are not present, so
    /// adding a mapping needs an invalidation too (as under emulators).
    pub fn caching_mode(&self) -> bool {
        self.0 & (1 << 7) != 0
    }

    /// The page-table depths the unit walks (bit 1: three levels, 39-bit
    /// addresses; bit 2: four levels, 48-bit).
    pub fn sagaw(&self) -> u32 {
        ((self.0 >> 8) & 0x1F) as u32
    }

    /// The widest guest address the unit translates (bits).
    pub fn mgaw(&self) -> u32 {
        ((self.0 >> 16) & 0x3F) as u32 + 1
    }

    /// Writes to the tables reach the unit only after a write-buffer
    /// flush (early implementations).
    pub fn rwbf(&self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Where the fault recording registers are (an offset into the
    /// registers) and how many there are.
    pub fn fault_records(&self) -> (usize, usize) {
        ((((self.0 >> 24) & 0x3FF) * 16) as usize, ((self.0 >> 40) & 0xFF) as usize + 1)
    }
}

/// What the extended capability register says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ecap(pub u64);

impl Ecap {
    /// The unit's walks see the processor's caches (else they must be
    /// flushed for it).
    pub fn coherent(&self) -> bool {
        self.0 & (1 << 0) != 0
    }

    pub fn queued_invalidation(&self) -> bool {
        self.0 & (1 << 1) != 0
    }

    pub fn interrupt_remapping(&self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// 32-bit (x2APIC) destinations in interrupt remapping entries.
    pub fn extended_interrupt_mode(&self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Context entries may pass a device's requests through untranslated.
    pub fn pass_through(&self) -> bool {
        self.0 & (1 << 6) != 0
    }
}

/// The depth of second-level page tables to walk, given what a unit can
/// walk ([`Cap::sagaw`], or what several units have in common): 4 levels
/// (48-bit guest addresses) if it can, else 3 (39-bit); and the address
/// width field of context entries for it.
pub fn levels(sagaw: u32) -> Option<(u32, u64)> {
    if sagaw & 4 != 0 {
        Some((4, 2))
    } else if sagaw & 2 != 0 {
        Some((3, 1))
    } else {
        None
    }
}

/// A root entry: the context table of a bus.
pub fn root_entry(context_table: u64) -> [u64; 2] {
    [context_table | 1, 0]
}

/// How a context entry translates a device's requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translation {
    /// Through the second-level page tables at the address.
    Tables(u64),
    /// Not at all: the device reaches physical memory as it is.
    PassThrough,
}

/// A context entry: a device in domain `domain`, translated so.
pub fn context_entry(translation: Translation, domain: u16, address_width: u64) -> [u64; 2] {
    let low = match translation {
        Translation::Tables(root) => (root & !0xFFF) | 1,
        Translation::PassThrough => (2 << 2) | 1,
    };
    [low, address_width | ((domain as u64) << 8)]
}

/// Whether a context entry is present, and whether it passes requests
/// through (the host's domain) rather than translating them (a guest's).
pub fn context_state(entry: [u64; 2]) -> Option<Translation> {
    match (entry[0] & 1, (entry[0] >> 2) & 3) {
        (0, _) => None,
        (_, 2) => Some(Translation::PassThrough),
        _ => Some(Translation::Tables(entry[0] & !0xFFF)),
    }
}

/// Bits of a second-level page-table entry.
pub const SL_READ: u64 = 1 << 0;
pub const SL_WRITE: u64 = 1 << 1;
/// A leaf that maps a large page (2 MiB at the directory level).
pub const SL_LARGE: u64 = 1 << 7;
pub const SL_ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;

/// An interrupt remapping entry (remapped, not posted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Irte {
    pub vector: u8,
    /// The destination's APIC id (x2APIC: all 32 bits).
    pub destination: u32,
    pub level: bool,
    /// The requester that may raise it (source validation), if known.
    pub source: Option<u16>,
}

impl Irte {
    /// The entry's two quadwords; `x2apic`: the unit's table is in
    /// extended interrupt mode.
    pub fn encode(&self, x2apic: bool) -> [u64; 2] {
        let destination = if x2apic { self.destination as u64 } else { ((self.destination & 0xFF) as u64) << 8 };
        let low = 1 | ((self.level as u64) << 4) | ((self.vector as u64) << 16) | (destination << 32);
        // Source validation: the whole requester id, no qualifier.
        let high = match self.source {
            Some(sid) => sid as u64 | (1 << 18),
            None => 0,
        };
        [low, high]
    }
}

/// The address and data a device writes to raise remapping entry `index`
/// (remappable format, no subhandle).
pub fn msi_message(index: u16) -> (u64, u32) {
    let address = 0xFEE0_0000 | (((index & 0x7FFF) as u64) << 5) | (1 << 4) | (((index >> 15) as u64) << 2);
    (address, 0)
}

/// An I/O APIC redirection entry that raises remapping entry `index`
/// (remappable format), `vector` as the entry has it (for the EOIs of
/// level-triggered lines).
pub fn ioapic_entry(index: u16, vector: u8, level: bool, active_low: bool, masked: bool) -> u64 {
    vector as u64
        | (((index >> 15) as u64) << 11)
        | ((active_low as u64) << 13)
        | ((level as u64) << 15)
        | ((masked as u64) << 16)
        | (1 << 48)
        | (((index & 0x7FFF) as u64) << 49)
}

/// `IRTA`: a table of `2^(size+1)` entries at `table`.
pub fn irta(table: u64, size: u32, x2apic: bool) -> u64 {
    table | ((x2apic as u64) << 11) | (size as u64 & 0xF)
}

/// A fault a unit recorded: a device's request that it refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    /// The requester (bus, device, function).
    pub source: u16,
    /// Why ([`fault_reason`]).
    pub reason: u8,
    /// A read (else a write); for interrupt requests, a write.
    pub read: bool,
    /// The page the request was for; for an interrupt request, the
    /// remapping entry's index.
    pub address: u64,
}

impl Fault {
    /// The fault in a fault recording register, if it holds one.
    pub fn from_record(record: [u64; 2]) -> Option<Fault> {
        let [low, high] = record;
        if high & (1 << 63) == 0 {
            return None;
        }
        let reason = (high >> 32) as u8;
        let interrupt = (0x20..0x30).contains(&reason);
        Some(Fault {
            source: high as u16,
            reason,
            read: !interrupt && high & (1 << 62) != 0,
            address: if interrupt { low >> 48 } else { low & !0xFFF },
        })
    }

    /// Whether the request was an interrupt (the address is an index).
    pub fn is_interrupt(&self) -> bool {
        (0x20..0x30).contains(&self.reason)
    }
}

/// What a fault reason means (chapter 7 of the specification).
/// The fault reason of a request from a device with no context entry (one
/// that is in no domain, as one is once its guest has ended).
pub const NO_CONTEXT_ENTRY: u8 = 0x2;

pub fn fault_reason(reason: u8) -> &'static str {
    match reason {
        0x1 => "the bus has no root entry",
        0x2 => "the device has no context entry",
        0x3 => "the context entry is invalid",
        0x4 => "the address is beyond what the domain translates",
        0x5 => "the page is not writable",
        0x6 => "the page is not readable",
        0x7 => "a page table could not be read",
        0x8 => "the root table could not be read",
        0x9 => "the context table could not be read",
        0xA | 0xB => "a reserved field of an entry is set",
        0xC => "a page-table entry is invalid",
        0xD => "the request is of a kind the context entry refuses",
        0x20 => "a reserved field of the interrupt request is set",
        0x21 => "the interrupt's index is beyond the table",
        0x22 => "the interrupt's entry is not present",
        0x23 => "the interrupt table could not be read",
        0x24 => "a reserved field of the interrupt's entry is set",
        0x25 => "the interrupt request is in the blocked compatibility format",
        0x26 => "the device may not raise this interrupt",
        _ => "an unknown reason",
    }
}

/// Invalidation descriptors of the queue.
pub mod desc {
    /// Forget every cached context entry.
    pub fn context_global() -> [u64; 2] {
        [0x1 | (1 << 4), 0]
    }

    /// Forget the context entry of a device (in domain `domain`).
    pub fn context_device(domain: u16, sid: u16) -> [u64; 2] {
        [0x1 | (3 << 4) | ((domain as u64) << 16) | ((sid as u64) << 32), 0]
    }

    /// Forget every cached translation.
    pub fn iotlb_global() -> [u64; 2] {
        [0x2 | (1 << 4) | (1 << 6) | (1 << 7), 0]
    }

    /// Forget the translations of domain `domain`, writes and reads drained.
    pub fn iotlb_domain(domain: u16) -> [u64; 2] {
        [0x2 | (2 << 4) | (1 << 6) | (1 << 7) | ((domain as u64) << 16), 0]
    }

    /// Forget every cached interrupt remapping entry.
    pub fn interrupt_global() -> [u64; 2] {
        [0x4, 0]
    }

    /// Forget interrupt remapping entry `index`.
    pub fn interrupt_entry(index: u16) -> [u64; 2] {
        [0x4 | (1 << 4) | ((index as u64) << 32), 0]
    }

    /// Write `value` to `status` (a 4-byte-aligned physical address) once
    /// the descriptors before are done.
    pub fn wait(status: u64, value: u32) -> [u64; 2] {
        [0x5 | (1 << 5) | (1 << 6) | ((value as u64) << 32), status]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities() {
        // Domains 2^16, four-level tables, 39-bit guest addresses.
        let cap = Cap(0x0026_0406);
        assert_eq!(cap.domains(), 65536);
        assert!(!cap.caching_mode());
        assert_eq!(cap.sagaw(), 4);
        assert_eq!(cap.mgaw(), 39);
        assert_eq!(levels(cap.sagaw()), Some((4, 2)));
        assert_eq!(levels(Cap(0x2 << 8).sagaw()), Some((3, 1)));
        assert_eq!(levels(0), None);
        let ecap = Ecap(0xF0_0F5E);
        assert!(!ecap.coherent());
        assert!(ecap.queued_invalidation());
        assert!(ecap.interrupt_remapping());
        assert!(ecap.extended_interrupt_mode());
        assert!(ecap.pass_through());
    }

    #[test]
    fn entries() {
        assert_eq!(root_entry(0x5000), [0x5001, 0]);
        assert_eq!(context_entry(Translation::PassThrough, 1, 2), [0b1001, 0x102]);
        assert_eq!(context_entry(Translation::Tables(0x7000), 3, 1), [0x7001, 0x301]);
    }

    #[test]
    fn interrupt_remapping() {
        let e = Irte { vector: 0x51, destination: 0x103, level: false, source: Some(0x0218) };
        assert_eq!(e.encode(true), [0x0000_0103_0051_0001, 0x4_0218]);
        assert_eq!(e.encode(false)[0], 0x0000_0300_0051_0001);
        let level = Irte { level: true, source: None, ..e };
        assert_eq!(level.encode(true), [0x0000_0103_0051_0011, 0]);
        // Entry 0x8005: the 16th bit of the handle goes in bit 2.
        assert_eq!(msi_message(5), (0xFEE0_00B0, 0));
        assert_eq!(msi_message(0x8005), (0xFEE0_00B4, 0));
        let rte = ioapic_entry(0x8003, 0x31, true, true, false);
        assert_eq!(rte, 0x31 | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 48) | (3 << 49));
        assert_eq!(irta(0x9000, 7, true), 0x9807);
    }

    #[test]
    fn context_states() {
        assert_eq!(context_state([0, 0]), None);
        assert_eq!(context_state(context_entry(Translation::PassThrough, 1, 2)), Some(Translation::PassThrough));
        let guest = context_entry(Translation::Tables(0x7000), 3, 1);
        assert_eq!(context_state(guest), Some(Translation::Tables(0x7000)));
    }

    #[test]
    fn faults() {
        // Four fault recording registers, at 0x220 (offset 0x22 in 16s).
        let cap = Cap((0x22 << 24) | (3 << 40));
        assert_eq!(cap.fault_records(), (0x220, 4));
        assert!(!cap.rwbf());
        assert_eq!(Fault::from_record([0x1234_5000, 0x0000_0005_0000_0018]), None);
        // A write to 0x12345000 by 00:03.0 that the domain does not allow.
        let write = Fault::from_record([0x1234_5678, (1 << 63) | (0x5 << 32) | 0x0018]).unwrap();
        assert_eq!(write, Fault { source: 0x18, reason: 5, read: false, address: 0x1234_5000 });
        assert!(!write.is_interrupt());
        let read = Fault::from_record([0x9000, (1 << 63) | (1 << 62) | (0x6 << 32) | 0x0018]).unwrap();
        assert!(read.read);
        // An interrupt from 00:03.0 through entry 0x51, which is another's.
        let irq = Fault::from_record([0x51 << 48, (1 << 63) | (0x26 << 32) | 0x0018]).unwrap();
        assert!(irq.is_interrupt());
        assert_eq!((irq.address, irq.read), (0x51, false));
        assert_eq!(fault_reason(0x26), "the device may not raise this interrupt");
        assert_eq!(fsts::first_record(0x0000_0302), 3);
    }

    #[test]
    fn descriptors() {
        assert_eq!(desc::context_device(2, 0x0218), [0x0218_0002_0031, 0]);
        assert_eq!(desc::iotlb_domain(2), [0x0002_00E2, 0]);
        assert_eq!(desc::interrupt_entry(9), [0x9_0000_0014, 0]);
        assert_eq!(desc::wait(0x1000, 1), [0x1_0000_0065, 0x1000]);
    }
}
