//! The platform's ACPI tables: the machine as they describe it to the
//! guest, as a PC's firmware describes a PC: hardware-reduced ACPI (no fixed hardware, no SCI) and no
//! MADT (the processors are the platform's, which Linux learns from its
//! CPUID leaves). The DSDT holds the PCI root (bus 0, the windows the BARs
//! are in), its functions and where their INTx go (`_PRT`: the GSIs the
//! guest has, level-triggered), and the keyboard controller's devices when
//! the guest has it, with the ports and lines a PC's are at. The FADT says
//! whether there is a keyboard controller, and that there is no VGA and no
//! CMOS clock.
//!
//! The tables go in the PC's firmware area below 1 MiB, which the memory
//! map reserves; the boot parameters say where the RSDP is.

use alloc::format;
use alloc::vec::Vec;
use core::ops::Range;

use vacpi::asm::{
    cat, device, dword_memory, eisa, int, io, irq_no_flags, name, package, qword_memory, resource_template, scope,
    word_bus_number,
};

/// Where the tables are in the guest's memory, the RSDP first.
pub const AT: u64 = 0xE_0000;
/// The room they have there.
pub const ROOM: usize = 0x2_0000;

/// The PC's keyboard controller: its data port, its status and command
/// port, and the keyboard's and the mouse's interrupt lines (GSIs).
pub const I8042_DATA: u16 = 0x60;
pub const I8042_COMMAND: u16 = 0x64;
pub const KEYBOARD_GSI: u32 = 1;
pub const MOUSE_GSI: u32 = 12;

/// A PCI function as the guest has it.
pub struct Function {
    /// Where on the guest's bus 0: device << 3 | function.
    pub devfn: u8,
    /// Its INTx, if the guest has the line: the pin (1 for INTA# to 4)
    /// and the GSI.
    pub intx: Option<(u8, u32)>,
}

/// What the tables describe.
pub struct Machine<'a> {
    pub functions: &'a [Function],
    /// The guest has the keyboard controller.
    pub i8042: bool,
    /// Where the functions' BARs are, below and above 4 GiB.
    pub pci_low: Range<u64>,
    pub pci_high: Range<u64>,
}

const OEM_ID: &[u8; 6] = b"VEDA  ";
const OEM_TABLE_ID: &[u8; 8] = b"DRIVERVM";
const HEADER: usize = 36;
/// The FADT of ACPI 6.5, whole.
const FADT_LENGTH: usize = 276;

/// FADT flags: hardware-reduced ACPI.
const HW_REDUCED_ACPI: u32 = 1 << 20;
/// IA-PC boot architecture flags.
const BOOT_8042: u16 = 1 << 1;
const BOOT_VGA_NOT_PRESENT: u16 = 1 << 2;
const BOOT_CMOS_RTC_NOT_PRESENT: u16 = 1 << 5;

/// The tables, to be written at [`AT`]: the RSDP, the XSDT, the FADT and
/// the DSDT, one after the other, each at a multiple of 16.
pub fn tables(m: &Machine) -> Vec<u8> {
    let xsdt_at = AT + 48;
    let fadt_at = xsdt_at + 48;
    let dsdt_at = (fadt_at + FADT_LENGTH as u64).next_multiple_of(16);
    let mut out = Vec::new();
    out.extend_from_slice(&rsdp(xsdt_at));
    out.resize((xsdt_at - AT) as usize, 0);
    out.extend_from_slice(&table(b"XSDT", 1, &fadt_at.to_le_bytes()));
    out.resize((fadt_at - AT) as usize, 0);
    out.extend_from_slice(&table(b"FACP", 6, &fadt(m, dsdt_at)));
    out.resize((dsdt_at - AT) as usize, 0);
    out.extend_from_slice(&table(b"DSDT", 2, &dsdt(m)));
    out
}

/// A table: its header (with the checksum), then `body`.
fn table(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(HEADER + body.len());
    t.extend_from_slice(signature);
    t.extend_from_slice(&((HEADER + body.len()) as u32).to_le_bytes());
    t.push(revision);
    t.push(0);
    t.extend_from_slice(OEM_ID);
    t.extend_from_slice(OEM_TABLE_ID);
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(b"VEDA");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(body);
    t[9] = checksum(&t);
    t
}

/// What makes `bytes` sum to zero.
fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b)))
}

/// The RSDP of ACPI 2.0 and later, which points to the XSDT.
fn rsdp(xsdt_at: u64) -> [u8; 36] {
    let mut r = [0u8; 36];
    r[..8].copy_from_slice(b"RSD PTR ");
    r[9..15].copy_from_slice(OEM_ID);
    r[15] = 2;
    r[20..24].copy_from_slice(&36u32.to_le_bytes());
    r[24..32].copy_from_slice(&xsdt_at.to_le_bytes());
    r[8] = checksum(&r[..20]);
    r[32] = checksum(&r);
    r
}

/// The FADT after its header.
fn fadt(m: &Machine, dsdt_at: u64) -> Vec<u8> {
    let mut f = [0u8; FADT_LENGTH];
    // The DSDT, below 4 GiB, at both of its addresses.
    f[40..44].copy_from_slice(&(dsdt_at as u32).to_le_bytes());
    let boot = BOOT_VGA_NOT_PRESENT | BOOT_CMOS_RTC_NOT_PRESENT | if m.i8042 { BOOT_8042 } else { 0 };
    f[109..111].copy_from_slice(&boot.to_le_bytes());
    f[112..116].copy_from_slice(&HW_REDUCED_ACPI.to_le_bytes());
    // ACPI 6.5.
    f[131] = 5;
    f[140..148].copy_from_slice(&dsdt_at.to_le_bytes());
    f[268..276].copy_from_slice(b"VedaVeda");
    f[HEADER..].to_vec()
}

/// The DSDT's definitions.
fn dsdt(m: &Machine) -> Vec<u8> {
    let mut sb = pci_root(m);
    if m.i8042 {
        let keyboard = cat(&[&io(I8042_DATA, 1), &io(I8042_COMMAND, 1), &irq_no_flags(KEYBOARD_GSI as u8)]);
        let mouse = irq_no_flags(MOUSE_GSI as u8);
        sb.extend_from_slice(&device(
            "PS2K",
            &cat(&[&name("_HID", &eisa("PNP0303")), &name("_CRS", &resource_template(&keyboard))]),
        ));
        sb.extend_from_slice(&device(
            "PS2M",
            &cat(&[&name("_HID", &eisa("PNP0F13")), &name("_CRS", &resource_template(&mouse))]),
        ));
    }
    scope("\\_SB", &sb)
}

/// The PCI root: bus 0, the windows the BARs are in, the functions and
/// where their INTx go.
fn pci_root(m: &Machine) -> Vec<u8> {
    let crs = cat(&[
        &word_bus_number(0, 0),
        &dword_memory(m.pci_low.start as u32, (m.pci_low.end - 1) as u32),
        &qword_memory(m.pci_high.start, m.pci_high.end - 1),
    ]);
    let mut body = cat(&[
        &name("_HID", &eisa("PNP0A03")),
        &name("_UID", &int(0)),
        &name("_SEG", &int(0)),
        &name("_BBN", &int(0)),
        &name("_CRS", &resource_template(&crs)),
    ]);
    let routes: Vec<Vec<u8>> = m
        .functions
        .iter()
        .filter_map(|f| {
            let (pin, gsi) = f.intx?;
            let all_functions = (f.devfn as u64 >> 3) << 16 | 0xFFFF;
            Some(package(&[int(all_functions), int(pin as u64 - 1), int(0), int(gsi as u64)]))
        })
        .collect();
    if !routes.is_empty() {
        body.extend_from_slice(&name("_PRT", &package(&routes)));
    }
    for f in m.functions {
        let address = (f.devfn as u64 >> 3) << 16 | (f.devfn as u64 & 7);
        body.extend_from_slice(&device(&format!("S{:02X}", f.devfn), &name("_ADR", &int(address))));
    }
    device("PCI0", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vacpi::NoMemory;
    use vacpi::aml::Namespace;
    use vacpi::device;
    use vacpi::name::Path;
    use vacpi::resource::Resource;

    #[test]
    fn the_tables_add_up() {
        let functions = [Function { devfn: 0x18, intx: Some((1, 22)) }, Function { devfn: 0x80, intx: None }];
        let m = Machine { functions: &functions, i8042: true, pci_low: 0xC000_0000..0xFEC0_0000, pci_high: 0..1 };
        let t = tables(&m);
        assert!(t.len() <= ROOM);
        let sum = |b: &[u8]| b.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        assert_eq!((&t[..8], sum(&t[..20]), sum(&t[..36])), (&b"RSD PTR "[..], 0, 0));
        // Each table sums to zero over its length.
        for at in [48, 96] {
            let len = u32::from_le_bytes(t[at + 4..at + 8].try_into().unwrap()) as usize;
            assert_eq!(sum(&t[at..at + len]), 0);
        }
        assert_eq!(&t[96..100], b"FACP");
        let dsdt = (u64::from_le_bytes(t[96 + 140..96 + 148].try_into().unwrap()) - AT) as usize;
        assert_eq!(&t[dsdt..dsdt + 4], b"DSDT");

        // What an OS finds in the DSDT.
        let len = u32::from_le_bytes(t[dsdt + 4..dsdt + 8].try_into().unwrap()) as usize;
        let mut ns = Namespace::new();
        ns.load(&t[dsdt + HEADER..dsdt + len], &NoMemory).unwrap();
        let roots = device::pci_roots(&ns, &NoMemory);
        assert_eq!(roots.len(), 1);
        let intx = |slot| device::pci_interrupt(&ns, &roots, (0, slot), 1, &NoMemory);
        assert_eq!(intx(3), Some(device::IntxRoute { gsi: 22, level: true, active_low: true }));
        assert_eq!(intx(16), None);
        let at = |p| Path::parse(p).unwrap();
        assert_eq!(device::pci_companion(&ns, &roots, (0, 16, 0), &NoMemory), Some(at("\\_SB.PCI0.S80")));
        let keyboard = device::resources(&ns, &at("\\_SB.PS2K"), &NoMemory).unwrap();
        assert_eq!(keyboard[..2], [Resource::Io { base: 0x60, length: 1 }, Resource::Io { base: 0x64, length: 1 }]);
        assert!(matches!(&keyboard[2], Resource::Irq { irqs, edge: true, .. } if irqs == &[1]));
        assert!(device::identify(&ns, &at("\\_SB.PS2M"), &NoMemory).is("PNP0F13"));
    }
}
