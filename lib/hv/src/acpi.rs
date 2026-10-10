//! The platform's ACPI tables: the machine as they describe it to the
//! guest, as a PC's firmware describes a PC: hardware-reduced ACPI (no fixed hardware, no SCI) and no
//! MADT (the processors are the platform's, which Linux learns from its
//! CPUID leaves). The DSDT holds the PCI root (bus 0, the windows the BARs
//! are in), its functions and where their INTx go (`_PRT`: the GSIs the
//! guest has, level-triggered), and the keyboard controller's devices when
//! the guest has it, with the ports and lines a PC's are at. Below a
//! function are the devices the PC's firmware describes there that the
//! guest has — a touchpad on an I2C controller, amplifiers on an SPI
//! controller — with their ids, their place on the function's bus, the
//! lines their interrupts are on, the GPIO pins they are wired to, what
//! their `_DSM` answers HID over I2C, and constant data of theirs and the
//! function's (an I2C controller's timing). Those pins are on GPIO
//! controllers of the platform's (`VEDA0001`), one for each of the PC's
//! that the pins are on, with the PC's numbers for them: the pins it has
//! (`veda,pins` in its `_DSD`), and the lines the ones that interrupt are
//! on (its `_CRS`, in the order of `veda,interrupt-pins`). The FADT says
//! whether there is a keyboard controller, and that there is no VGA and no
//! CMOS clock.
//!
//! The tables go in the PC's firmware area below 1 MiB, which the memory
//! map reserves; the boot parameters say where the RSDP is.

use alloc::format;
use alloc::vec::Vec;
use core::ops::Range;

use alloc::string::String;
use vacpi::asm::{
    cat, device, dsm, dword_memory, eisa, gpio_descriptor_of, i2c_descriptor, int, interrupt_of, io, irq_no_flags,
    name, package, qword_memory, resource_template, scope, spi_descriptor_of, string, word_bus_number,
};
use vacpi::resource::Spi;

/// A GPIO connection, as [`Resource::Gpio`] holds it.
pub use vacpi::resource::Gpio;

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
#[derive(Debug, Clone)]
pub struct Function {
    /// Where on the guest's bus 0: device << 3 | function.
    pub devfn: u8,
    /// Its INTx, if the guest has the line: the pin (1 for INTA# to 4)
    /// and the GSI.
    pub intx: Option<(u8, u32)>,
    /// Constant objects of its own the firmware gives its driver (an I2C
    /// controller's timing, `FMCN`...): names and AML terms.
    pub data: Vec<(String, Vec<u8>)>,
    /// The devices the firmware describes below it that the guest has.
    pub children: Vec<Child>,
}

/// A device the PC's firmware describes below a function: on the bus it
/// controls.
#[derive(Debug, Clone)]
pub struct Child {
    /// Its name, the firmware's (`TPD0`).
    pub name: String,
    pub hid: Option<String>,
    pub cids: Vec<String>,
    pub uid: Option<String>,
    pub sub: Option<String>,
    pub resources: Vec<Resource>,
    /// What its `_DSM` answers HID over I2C: its HID descriptor's address.
    pub hid_descriptor: Option<u16>,
    /// Constant objects (`_DSD`): names and AML terms.
    pub data: Vec<(String, Vec<u8>)>,
}

/// A resource of a described device, as the guest has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// Its address on its function's I2C bus.
    I2c { address: u16, speed_hz: u32, ten_bit: bool },
    /// Its connection to its function's SPI bus.
    Spi { chip_select: u16, speed_hz: u32, bits: u8, cpol: bool, cpha: bool, cs_active_high: bool, three_wire: bool },
    /// An interrupt line the guest has (a GSI).
    Interrupt { gsi: u32, edge: bool, active_low: bool, shared: bool, wake: bool },
    /// GPIO pins it is wired to, on a controller of the guest's
    /// (`controller`: its path, `\_SB.GPI0`), by the PC's numbers.
    Gpio(Gpio),
}

/// The hardware id of the platform's GPIO controllers.
pub const GPIO_CONTROLLER: &str = "VEDA0001";

/// A GPIO controller of the guest's: the pins of one of the PC's that its
/// devices are wired to, by the PC's numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpioController {
    /// Its name in `\_SB` (the PC's controller's, `GPI0`).
    pub name: String,
    /// Its number for [`crate::platform::hypercall::GPIO`] (its `_UID`).
    pub uid: u32,
    /// Its pins, in order.
    pub pins: Vec<u16>,
    /// The pins that interrupt, and the lines they do on.
    pub interrupts: Vec<GpioInterrupt>,
}

/// A GPIO pin's interrupt: the line (a GSI the platform makes), as the
/// pin's connection describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpioInterrupt {
    pub pin: u16,
    pub gsi: u32,
    pub edge: bool,
    pub active_low: bool,
}

/// Device properties' UUID (`_DSD`).
const DEVICE_PROPERTIES: &str = "daffd814-6eba-4d8c-8a91-bc9bbf4aa301";

/// HID over I2C's `_DSM` (function 1: the HID descriptor's address).
pub const HID_OVER_I2C: &str = "3cdff6f7-4267-4555-ad05-b30a3d8938de";

/// The bytes `ToUUID` makes of a UUID's text.
pub use vacpi::asm::uuid;

/// What the tables describe.
pub struct Machine<'a> {
    pub functions: &'a [Function],
    pub gpio: &'a [GpioController],
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
    for c in m.gpio {
        sb.extend_from_slice(&gpio_controller(c));
    }
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
        let path = format!("\\_SB.PCI0.{}", function_name(f.devfn));
        let address = (f.devfn as u64 >> 3) << 16 | (f.devfn as u64 & 7);
        let mut fd = name("_ADR", &int(address));
        for (n, value) in &f.data {
            fd.extend_from_slice(&name(n, value));
        }
        for c in &f.children {
            fd.extend_from_slice(&child(c, &path));
        }
        body.extend_from_slice(&device(&function_name(f.devfn), &fd));
    }
    device("PCI0", &body)
}

/// A GPIO controller of the platform's.
fn gpio_controller(c: &GpioController) -> Vec<u8> {
    let lines: Vec<u8> =
        c.interrupts.iter().flat_map(|i| interrupt_of(i.gsi, i.edge, i.active_low, false, false)).collect();
    let numbers = |pins: &mut dyn Iterator<Item = u16>| package(&pins.map(|p| int(p as u64)).collect::<Vec<_>>());
    let properties = package(&[
        package(&[string("veda,pins"), numbers(&mut c.pins.iter().copied())]),
        package(&[string("veda,interrupt-pins"), numbers(&mut c.interrupts.iter().map(|i| i.pin))]),
    ]);
    let dsd = package(&[vacpi::asm::buffer(&uuid(DEVICE_PROPERTIES).unwrap_or_default()), properties]);
    device(
        &c.name,
        &cat(&[
            &name("_HID", &string(GPIO_CONTROLLER)),
            &name("_UID", &int(c.uid as u64)),
            &name("_CRS", &resource_template(&lines)),
            &name("_DSD", &dsd),
        ]),
    )
}

/// The name of the device of function `devfn` (`S18` for 00:03.0).
fn function_name(devfn: u8) -> String {
    format!("S{:02X}", devfn)
}

/// An id: compressed when it is an EISA one (`PNP0C50`), else a string.
fn id(text: &str) -> Vec<u8> {
    let b = text.as_bytes();
    let eisa_form =
        b.len() == 7 && b[..3].iter().all(u8::is_ascii_uppercase) && b[3..].iter().all(u8::is_ascii_hexdigit);
    if eisa_form { eisa(text) } else { string(text) }
}

/// A described device below the function at `parent`.
fn child(c: &Child, parent: &str) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(hid) = &c.hid {
        body.extend_from_slice(&name("_HID", &id(hid)));
    }
    match c.cids.as_slice() {
        [] => {}
        [one] => body.extend_from_slice(&name("_CID", &id(one))),
        many => body.extend_from_slice(&name("_CID", &package(&many.iter().map(|c| id(c)).collect::<Vec<_>>()))),
    }
    if let Some(uid) = &c.uid {
        let value = uid.parse::<u64>().map(int).unwrap_or_else(|_| string(uid));
        body.extend_from_slice(&name("_UID", &value));
    }
    if let Some(sub) = &c.sub {
        body.extend_from_slice(&name("_SUB", &string(sub)));
    }
    let descriptors: Vec<u8> = c
        .resources
        .iter()
        .flat_map(|r| match r {
            &Resource::I2c { address, speed_hz, ten_bit } => i2c_descriptor(address, speed_hz, ten_bit, parent),
            &Resource::Spi { chip_select, speed_hz, bits, cpol, cpha, cs_active_high, three_wire } => {
                let controller = parent.into();
                spi_descriptor_of(&Spi {
                    controller,
                    chip_select,
                    speed_hz,
                    bits,
                    cpol,
                    cpha,
                    cs_active_high,
                    three_wire,
                })
            }
            &Resource::Interrupt { gsi, edge, active_low, shared, wake } => {
                interrupt_of(gsi, edge, active_low, shared, wake)
            }
            Resource::Gpio(g) => gpio_descriptor_of(g),
        })
        .collect();
    body.extend_from_slice(&name("_CRS", &resource_template(&descriptors)));
    if let (Some(address), Some(hid)) = (c.hid_descriptor, uuid(HID_OVER_I2C)) {
        body.extend_from_slice(&dsm(&hid, 1, &int(address as u64)));
    }
    for (n, value) in &c.data {
        body.extend_from_slice(&name(n, value));
    }
    device(&c.name, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vacpi::NoMemory;
    use vacpi::aml::Namespace;
    use vacpi::device;
    use vacpi::name::Path;
    use vacpi::resource::Resource as Resource2;

    fn function(devfn: u8, intx: Option<(u8, u32)>) -> Function {
        Function { devfn, intx, data: Vec::new(), children: Vec::new() }
    }

    #[test]
    fn the_tables_add_up() {
        let functions = [function(0x18, Some((1, 22))), function(0x80, None)];
        let m = Machine {
            functions: &functions,
            gpio: &[],
            i8042: true,
            pci_low: 0xC000_0000..0xFEC0_0000,
            pci_high: 0..1,
        };
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
        assert_eq!(keyboard[..2], [Resource2::Io { base: 0x60, length: 1 }, Resource2::Io { base: 0x64, length: 1 }]);
        assert!(matches!(&keyboard[2], Resource2::Irq { irqs, edge: true, .. } if irqs == &[1]));
        assert!(device::identify(&ns, &at("\\_SB.PS2M"), &NoMemory).is("PNP0F13"));
    }

    #[test]
    fn a_touchpad_below_its_i2c_controller() {
        // As the Zenbook's: an I2C HID touchpad at 0x15 on I2C1, its
        // interrupt GSI 40; the controller's fast-mode timing.
        let touchpad = Child {
            name: String::from("ETPD"),
            hid: Some(String::from("ASUP1413")),
            cids: alloc::vec![String::from("PNP0C50")],
            uid: Some(String::from("1")),
            sub: None,
            resources: alloc::vec![
                Resource::I2c { address: 0x15, speed_hz: 400_000, ten_bit: false },
                Resource::Interrupt { gsi: 40, edge: false, active_low: true, shared: false, wake: true },
            ],
            hid_descriptor: Some(0x20),
            data: Vec::new(),
        };
        let fmcn = package(&[int(0x64), int(0xD6), int(0x1C)]);
        let i2c = Function {
            devfn: 0xA9,
            intx: Some((2, 52)),
            data: alloc::vec![(String::from("FMCN"), fmcn)],
            children: alloc::vec![touchpad],
        };
        let functions = [i2c];
        let m = Machine {
            functions: &functions,
            gpio: &[],
            i8042: false,
            pci_low: 0xC000_0000..0xFEC0_0000,
            pci_high: 0..1,
        };
        let t = tables(&m);
        let dsdt = (u64::from_le_bytes(t[96 + 140..96 + 148].try_into().unwrap()) - AT) as usize;
        let len = u32::from_le_bytes(t[dsdt + 4..dsdt + 8].try_into().unwrap()) as usize;
        let mut ns = Namespace::new();
        ns.load(&t[dsdt + HEADER..dsdt + len], &NoMemory).unwrap();
        let at = |p| Path::parse(p).unwrap();
        let tp = at("\\_SB.PCI0.SA9.ETPD");
        let id = device::identify(&ns, &tp, &NoMemory);
        assert!(id.is("ASUP1413") && id.is("PNP0C50"));
        assert_eq!(id.uid.as_deref(), Some("1"));
        let r = device::resources(&ns, &tp, &NoMemory).unwrap();
        assert!(matches!(&r[0], Resource2::I2c(i) if i.address == 0x15 && i.controller == "\\_SB.PCI0.SA9"));
        assert!(
            matches!(&r[1], Resource2::Irq { irqs, edge: false, active_low: true, wake: true, .. } if irqs == &[40])
        );
        let hid = vacpi::value::Value::Buffer(uuid(HID_OVER_I2C).unwrap().to_vec());
        let args = [
            hid,
            vacpi::value::Value::Integer(1),
            vacpi::value::Value::Integer(1),
            vacpi::value::Value::Package(Vec::new()),
        ];
        assert_eq!(
            ns.evaluate(&at("\\_SB.PCI0.SA9.ETPD._DSM"), &args, &NoMemory),
            Ok(vacpi::value::Value::Integer(0x20))
        );
        let timing = ns.evaluate(&at("\\_SB.PCI0.SA9.FMCN"), &[], &NoMemory).unwrap();
        assert!(matches!(timing, vacpi::value::Value::Package(p) if p.len() == 3));
    }

    #[test]
    fn amplifiers_below_their_spi_controller_and_their_pins() {
        // As the Zenbook's: two CS35L41 on SPI1 (00:1e.3), the second's chip
        // select, their reset and speaker id on GPIO pins, their
        // interrupt a pin's, on the platform's line 256.
        let pin = |interrupt: bool, pin: u16| Gpio {
            interrupt,
            pins: alloc::vec![pin],
            controller: String::from("\\_SB.GPI0"),
            pull: 0,
            restriction: 0,
            edge: false,
            polarity: if interrupt { 1 } else { 0 },
            shared: false,
            wake: false,
            debounce: 0,
        };
        let spi = |cs| Resource::Spi {
            chip_select: cs,
            speed_hz: 4_000_000,
            bits: 8,
            cpol: false,
            cpha: false,
            cs_active_high: false,
            three_wire: false,
        };
        let amps = Child {
            name: String::from("SPK1"),
            hid: Some(String::from("CSC3551")),
            cids: Vec::new(),
            uid: Some(String::from("1")),
            sub: Some(String::from("10431F62")),
            resources: alloc::vec![
                spi(0),
                spi(1),
                Resource::Gpio(pin(false, 23)),
                Resource::Gpio(pin(false, 305)),
                Resource::Gpio(pin(false, 302)),
                Resource::Gpio(pin(true, 303)),
            ],
            hid_descriptor: None,
            data: Vec::new(),
        };
        let functions = [Function { devfn: 0xF3, intx: Some((4, 37)), data: Vec::new(), children: alloc::vec![amps] }];
        let gpio = [GpioController {
            name: String::from("GPI0"),
            uid: 0,
            pins: alloc::vec![23, 302, 303, 305],
            interrupts: alloc::vec![GpioInterrupt { pin: 303, gsi: 256, edge: false, active_low: true }],
        }];
        let m = Machine {
            functions: &functions,
            gpio: &gpio,
            i8042: false,
            pci_low: 0xC000_0000..0xFEC0_0000,
            pci_high: 0..1,
        };
        let t = tables(&m);
        let dsdt = (u64::from_le_bytes(t[96 + 140..96 + 148].try_into().unwrap()) - AT) as usize;
        let len = u32::from_le_bytes(t[dsdt + 4..dsdt + 8].try_into().unwrap()) as usize;
        let mut ns = Namespace::new();
        ns.load(&t[dsdt + HEADER..dsdt + len], &NoMemory).unwrap();
        let at = |p| Path::parse(p).unwrap();
        let r = device::resources(&ns, &at("\\_SB.PCI0.SF3.SPK1"), &NoMemory).unwrap();
        assert!(matches!(&r[1], Resource2::Spi(s) if s.chip_select == 1 && s.controller == "\\_SB.PCI0.SF3"));
        let gpios: Vec<_> =
            r[2..].iter().filter_map(|r| if let Resource2::Gpio(g) = r { Some(g.clone()) } else { None }).collect();
        assert_eq!(gpios, [pin(false, 23), pin(false, 305), pin(false, 302), pin(true, 303)]);
        assert_eq!(device::identify(&ns, &at("\\_SB.PCI0.SF3.SPK1"), &NoMemory).sub.as_deref(), Some("10431F62"));
        // The controller: its id and number, its pins, its line.
        let controller = at("\\_SB.GPI0");
        let id = device::identify(&ns, &controller, &NoMemory);
        assert_eq!((id.hid.as_deref(), id.uid.as_deref()), (Some(GPIO_CONTROLLER), Some("0")));
        let lines = device::resources(&ns, &controller, &NoMemory).unwrap();
        assert!(matches!(&lines[..], [Resource2::Irq { irqs, edge: false, active_low: true, .. }] if irqs == &[256]));
        use vacpi::value::Value;
        let Ok(Value::Package(dsd)) = ns.evaluate(&at("\\_SB.GPI0._DSD"), &[], &NoMemory) else { panic!() };
        assert_eq!(dsd[0], Value::Buffer(uuid(DEVICE_PROPERTIES).unwrap().to_vec()));
        let numbers = |v: &[u16]| Value::Package(v.iter().map(|&p| Value::Integer(p as u64)).collect());
        assert_eq!(
            dsd[1],
            Value::Package(alloc::vec![
                Value::Package(alloc::vec![Value::String("veda,pins".into()), numbers(&[23, 302, 303, 305])]),
                Value::Package(alloc::vec![Value::String("veda,interrupt-pins".into()), numbers(&[303])]),
            ])
        );
    }
}
