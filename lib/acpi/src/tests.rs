use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use std::collections::BTreeMap;

use crate::aml::{Error, Namespace, Object};
use crate::asm::*;
use crate::device::{self, ResourcesError, eisa_id};
use crate::name::{NameString, Path};
use crate::resource::{self, Gpio, Resource, Spi};
use crate::value::Value;
use crate::{Memory, PciFunction};

/// Firmware memory: bytes at given addresses, nothing elsewhere; and the
/// configuration space of given PCI functions (zeros but what is put).
#[derive(Default)]
struct Ram {
    memory: BTreeMap<u64, u8>,
    pci: BTreeMap<PciFunction, [u8; 256]>,
}

impl Ram {
    fn put(&mut self, address: u64, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.memory.insert(address + i as u64, b);
        }
    }

    /// `bytes` at `offset` in the configuration space of function
    /// `bus:device.function` of segment 0.
    fn put_pci(&mut self, (bus, device, function): (u8, u8, u8), offset: usize, bytes: &[u8]) {
        let space = self.pci.entry(PciFunction { segment: 0, bus, device, function }).or_insert([0; 256]);
        space[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
}

impl Memory for Ram {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        for (i, b) in buf.iter_mut().enumerate() {
            match self.memory.get(&(address + i as u64)) {
                Some(&v) => *b = v,
                None => return false,
            }
        }
        true
    }

    fn read_pci(&self, function: PciFunction, offset: u16, buf: &mut [u8]) -> bool {
        let at = offset as usize;
        match self.pci.get(&function).and_then(|space| space.get(at..at + buf.len())) {
            Some(bytes) => {
                buf.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

fn path(text: &str) -> Path {
    Path::parse(text).unwrap()
}

fn eval(ns: &Namespace, p: &str, memory: &dyn Memory) -> Result<Value, Error> {
    ns.evaluate(&path(p), &[], memory)
}

// --- Resource templates ----------------------------------------------------

#[test]
fn amplifier_resources() {
    let template = cat(&[
        &spi_descriptor(0, 4_000_000, "\\_SB.PC00.SPI1"),
        &spi_descriptor(1, 4_000_000, "\\_SB.PC00.SPI1"),
        &gpio_descriptor(false, 2, 1, 0, 23, "\\_SB.GPI0"),
        &gpio_descriptor(false, 2, 2, 0, 305, "\\_SB.GPI0"),
        &gpio_descriptor(true, 0x0D, 1, 100, 303, "\\_SB.GPI0"),
        &END_TAG,
    ]);
    let r = resource::parse(&template).unwrap();
    assert_eq!(r.len(), 5);
    assert_eq!(
        r[1],
        Resource::Spi(Spi {
            controller: "\\_SB.PC00.SPI1".into(),
            chip_select: 1,
            speed_hz: 4_000_000,
            bits: 8,
            cpol: false,
            cpha: false,
            cs_active_high: false,
            three_wire: false,
        })
    );
    let Resource::Gpio(cs) = &r[2] else { panic!("{:?}", r[2]) };
    assert_eq!((cs.interrupt, cs.pins.as_slice(), cs.restriction, cs.pull), (false, &[23u16][..], 2, 1));
    assert_eq!(cs.controller, "\\_SB.GPI0");
    let Resource::Gpio(irq) = &r[4] else { panic!() };
    assert_eq!(
        *irq,
        Gpio {
            interrupt: true,
            pins: vec![303],
            controller: "\\_SB.GPI0".into(),
            pull: 1,
            restriction: 0,
            edge: true,
            polarity: 2,
            shared: true,
            wake: false,
            debounce: 100,
        }
    );
    // A template cut short is an error, not a panic.
    assert!(resource::parse(&template[..30]).is_err());
}

#[test]
fn memory_interrupt_and_window_resources() {
    let template = [
        // Interrupt (ResourceConsumer, Level, ActiveLow, Shared) {14}
        0x89, 0x06, 0x00, 0x0D, 0x01, 0x0E, 0x00, 0x00, 0x00,
        // Memory32Fixed (ReadWrite, 0xFD6E0000, 0x10000)
        0x86, 0x09, 0x00, 0x01, 0x00, 0x00, 0x6E, 0xFD, 0x00, 0x00, 0x01, 0x00,
        // IO (Decode16, 0x60, 0x60, 1, 1)
        0x47, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x01, // IRQNoFlags () {1}
        0x22, 0x02, 0x00, // WordBusNumber (0..0xFF)
        0x88, 0x0D, 0x00, 0x02, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x01, 0x79, 0x00,
    ];
    let r = resource::parse(&template).unwrap();
    assert_eq!(r[0], Resource::Irq { irqs: vec![14], edge: false, active_low: true, shared: true, wake: false });
    assert_eq!(r[1], Resource::Memory { base: 0xFD6E_0000, length: 0x1_0000, writable: true });
    assert_eq!(r[2], Resource::Io { base: 0x60, length: 1 });
    assert_eq!(r[3], Resource::Irq { irqs: vec![1], edge: true, active_low: false, shared: false, wake: false });
    assert_eq!(r[4], Resource::Window { kind: 2, min: 0, max: 0xFF, translation: 0, length: 0x100 });
}

// --- Names -----------------------------------------------------------------

#[test]
fn paths_and_names() {
    let p = path("\\_SB.PC00.SPI1");
    assert_eq!(alloc::format!("{}", p), "\\_SB.PC00.SPI1");
    assert_eq!(p.0[0], *b"_SB_");
    assert_eq!(p.parent().unwrap(), path("\\_SB.PC00"));
    assert_eq!(alloc::format!("{}", Path::root()), "\\");
    let n = NameString::parse("^^GPI0").unwrap();
    assert_eq!(n.in_scope(&path("\\_SB.PC00.SPI1")), path("\\_SB.GPI0"));
    assert!(NameString::parse("TOOLONG").is_none());
    assert_eq!(eisa_id(0x080A_D041), "PNP0A08");
    assert_eq!(eisa_id(0x0C0C_D041), "PNP0C0C");
}

// --- Loading and evaluating -------------------------------------------------

/// A board like the laptop this was written for: firmware settings in a
/// memory region decide which devices exist and where the GPIO
/// controller's registers are.
fn board() -> (Namespace, Ram) {
    let gpio_crs = method(
        "_CRS",
        0,
        &cat(&[
            &name(
                "RBFL",
                &buffer(&[
                    0x86, 0x09, 0x00, 0x01, 0, 0, 0, 0, 0x00, 0x00, 0x01, 0x00, //
                    0x86, 0x09, 0x00, 0x01, 0, 0, 0, 0, 0x00, 0x00, 0x01, 0x00, //
                    0x79, 0x00,
                ]),
            ),
            &cat(&[&[0x8A], &nm("RBFL"), &int(4), &nm("CML0")]),
            &store(&add(&nm("SBRG"), &int(0x006E_0000)), &nm("CML0")),
            &cat(&[&[0x8A], &nm("RBFL"), &int(16), &nm("CML4")]),
            &store(&add(&nm("SBRG"), &int(0x006A_0000)), &nm("CML4")),
            &ret(&nm("RBFL")),
        ]),
    );
    let gpio = device(
        "GPI0",
        &cat(&[
            &method(
                "_HID",
                0,
                &cat(&[&if_(&lequal(&nm("GPHD"), &int(1)), &ret(&string("PNP0C02"))), &ret(&string("INTC1055"))]),
            ),
            &gpio_crs,
        ]),
    );
    let spi = device(
        "SPI1",
        &cat(&[
            &if_(&lequal(&nm("SM01"), &int(2)), &name("_STA", &int(8))),
            &if_(&lequal(&nm("SM01"), &int(1)), &method("_ADR", 0, &ret(&int(0x001E_0003)))),
        ]),
    );
    // A device whose condition reads memory nobody can read: dropped.
    let unknown = if_(&lequal(&nm("\\NOPE"), &int(1)), &device("GONE", &name("_ADR", &int(1))));
    let dsdt = cat(&[
        &region("GNVS", 0, &int(0x5000), &int(0x10)),
        &field("GNVS", 0x10, &[("SM01", 8), ("GPHD", 8), ("", 16), ("SBRG", 32)]),
        &region("BAD", 0, &int(0x9000), &int(4)),
        &field("BAD", 0x10, &[("NOPE", 8)]),
        &scope(
            "\\_SB",
            &cat(&[
                &device("PC00", &cat(&[&name("_HID", &[0x0C, 0x41, 0xD0, 0x0A, 0x08]), &name("_ADR", &int(0)), &spi])),
                &gpio,
                &unknown,
            ]),
        ),
    ]);
    let ssdt = cat(&[
        // If (Zero) { External (...) }, as some compilers emit.
        &if_(&[0x00], &cat(&[&[0x15], &nm("\\_SB.GPI0"), &[6, 0]])),
        &scope(
            "\\_SB.PC00.SPI1",
            &device(
                "SPK1",
                &cat(&[
                    &name("_HID", &string("CSC3551")),
                    &name("_SUB", &string("10431F62")),
                    &name("_UID", &int(1)),
                    &method(
                        "_CRS",
                        0,
                        &cat(&[
                            &name(
                                "SBUF",
                                &buffer(&cat(&[
                                    &spi_descriptor(0, 4_000_000, "\\_SB.PC00.SPI1"),
                                    &gpio_descriptor(false, 2, 2, 0, 305, "\\_SB.GPI0"),
                                    &END_TAG,
                                ])),
                            ),
                            &ret(&nm("SBUF")),
                        ]),
                    ),
                    &method("_STA", 0, &ret(&int(0x0F))),
                ]),
            ),
        ),
    ]);
    let mut ram = Ram::default();
    // SM01 = 1 (the SPI controller on PCI), GPHD = 0, SBRG = 0xFD000000.
    ram.put(0x5000, &[1, 0, 0, 0, 0x00, 0x00, 0x00, 0xFD, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut ns = Namespace::new();
    ns.load(&dsdt, &ram).unwrap();
    ns.load(&ssdt, &ram).unwrap();
    (ns, ram)
}

#[test]
fn devices_of_a_board() {
    let (ns, ram) = board();
    let devices: Vec<String> = ns.devices().map(|p| alloc::format!("{}", p)).collect();
    assert_eq!(devices, ["\\_SB.GPI0", "\\_SB.PC00", "\\_SB.PC00.SPI1", "\\_SB.PC00.SPI1.SPK1"]);
    assert_eq!(ns.skipped, 1);

    let root = device::identify(&ns, &path("\\_SB.PC00"), &ram);
    assert_eq!((root.hid.as_deref(), root.adr), (Some("PNP0A08"), Some(0)));
    // _ADR exists because SM01 is 1; _STA does not (SM01 is not 2).
    let spi = device::identify(&ns, &path("\\_SB.PC00.SPI1"), &ram);
    assert_eq!((spi.adr, spi.status), (Some(0x001E_0003), 0xF));

    let amp = device::identify(&ns, &path("\\_SB.PC00.SPI1.SPK1"), &ram);
    assert_eq!(amp.hid.as_deref(), Some("CSC3551"));
    assert_eq!((amp.sub.as_deref(), amp.uid.as_deref()), (Some("10431F62"), Some("1")));
    assert!(amp.present());
    let r = device::resources(&ns, &path("\\_SB.PC00.SPI1.SPK1"), &ram).unwrap();
    assert!(matches!(&r[1], Resource::Gpio(g) if g.pins == [305] && g.controller == "\\_SB.GPI0"));
    assert_eq!(device::children(&ns, &path("\\_SB.PC00.SPI1")), [path("\\_SB.PC00.SPI1.SPK1")]);

    // The GPIO controller: its id and registers come from the settings.
    let gpio = device::identify(&ns, &path("\\_SB.GPI0"), &ram);
    assert_eq!(gpio.hid.as_deref(), Some("INTC1055"));
    let r = device::resources(&ns, &path("\\_SB.GPI0"), &ram).unwrap();
    assert_eq!(r[0], Resource::Memory { base: 0xFD6E_0000, length: 0x1_0000, writable: true });
    assert_eq!(r[1], Resource::Memory { base: 0xFD6A_0000, length: 0x1_0000, writable: true });
    // The firmware's own buffer is not changed by evaluating _CRS.
    assert!(matches!(ns.get(&path("\\_SB.GPI0._CRS")), Some(Object::Method { .. })));
    // Names resolve the way a resource source names its controller.
    let n = NameString::parse("\\_SB.GPI0").unwrap();
    assert_eq!(ns.resolve(&path("\\_SB.PC00.SPI1.SPK1"), &n), Some(path("\\_SB.GPI0")));
}

#[test]
fn arithmetic_loops_and_calls() {
    // Method (SUM, 2) { Local0 = 0; Local1 = Arg0; While (Local1 <= Arg1) { Local0 += Local1; Local1++ }; Return (Local0) }
    let sum = method(
        "SUM",
        2,
        &cat(&[
            &store(&int(0), &[LOCAL0]),
            &store(&[ARG0], &[LOCAL1]),
            &while_(
                &[0x92, 0x94, LOCAL1, ARG1],
                &cat(&[&binary(0x72, &[LOCAL0], &[LOCAL1], &[LOCAL0]), &[0x75, LOCAL1]]),
            ),
            &ret(&[LOCAL0]),
        ]),
    );
    // Method (TEST) { Return (SUM (1, 10) * 2 + (0xF0 >> 4)) }
    let test = method(
        "TEST",
        0,
        &ret(&add(
            &binary(0x77, &cat(&[&nm("SUM"), &int(1), &int(10)]), &int(2), &[0]),
            &binary(0x7A, &int(0xF0), &int(4), &[0]),
        )),
    );
    // Method (OSIW) { If (_OSI ("Windows 2015")) { If (_OSI ("Linux")) { Return (2) } Return (1) } Return (0) }
    let osi = method(
        "OSIW",
        0,
        &cat(&[
            &if_(
                &cat(&[&nm("_OSI"), &string("Windows 2015")]),
                &cat(&[&if_(&cat(&[&nm("_OSI"), &string("Linux")]), &ret(&int(2))), &ret(&int(1))]),
            ),
            &ret(&int(0)),
        ]),
    );
    // Method (LOOP) { While (One) { } }
    let forever = method("LOOP", 0, &while_(&[0x01], &[]));
    // Method (PKG) { Name (P, Package () { 5, "x", Buffer () {1, 2} }); Store (7, Index (P, 0)); Return (DerefOf (Index (P, 0)) + SizeOf (P)) }
    let pkg_test = method(
        "PKG",
        0,
        &cat(&[
            &name("P___", &package(&[int(5), string("x"), buffer(&[1, 2])])),
            &store(&int(7), &cat(&[&[0x88], &nm("P___"), &int(0), &[0]])),
            &ret(&add(&cat(&[&[0x83, 0x88], &nm("P___"), &int(0), &[0]]), &cat(&[&[0x87], &nm("P___")]))),
        ]),
    );
    // Method (ELSE, 1) { If (Arg0 == 3) { Return ("three") } Else { Return ("other") } }
    let else_test = method(
        "ELSE",
        1,
        &cat(&[&if_(&lequal(&[ARG0], &int(3)), &ret(&string("three"))), &else_(&ret(&string("other")))]),
    );
    let code = cat(&[&sum, &test, &osi, &forever, &pkg_test, &else_test]);
    let mut ns = Namespace::new();
    ns.load(&code, &crate::NoMemory).unwrap();
    let m = &crate::NoMemory;
    assert_eq!(eval(&ns, "\\TEST", m), Ok(Value::Integer(55 * 2 + 15)));
    assert_eq!(eval(&ns, "\\OSIW", m), Ok(Value::Integer(1)));
    assert_eq!(eval(&ns, "\\LOOP", m), Err(Error::Limit));
    assert_eq!(eval(&ns, "\\PKG_", m), Ok(Value::Integer(7 + 3)));
    assert_eq!(ns.evaluate(&path("\\ELSE"), &[Value::Integer(3)], m), Ok(Value::String("three".into())));
    assert_eq!(ns.evaluate(&path("\\ELSE"), &[Value::Integer(4)], m), Ok(Value::String("other".into())));
}

#[test]
fn stores_stay_inside_an_evaluation() {
    // Name (CNT, 5); Method (BUMP) { CNT++; Return (CNT) }
    let code = cat(&[&name("CNT_", &int(5)), &method("BUMP", 0, &cat(&[&[0x75], &nm("CNT_"), &ret(&nm("CNT_"))]))]);
    let mut ns = Namespace::new();
    ns.load(&code, &crate::NoMemory).unwrap();
    assert_eq!(eval(&ns, "\\BUMP", &crate::NoMemory), Ok(Value::Integer(6)));
    assert_eq!(eval(&ns, "\\BUMP", &crate::NoMemory), Ok(Value::Integer(6)));
    assert_eq!(eval(&ns, "\\CNT_", &crate::NoMemory), Ok(Value::Integer(5)));
}

#[test]
fn spi_and_gpio_connections_read_back() {
    // The amplifiers' connections, as a guest's tables describe them.
    let spi = Spi {
        controller: "\\_SB.PCI0.SF3".into(),
        chip_select: 1,
        speed_hz: 4_000_000,
        bits: 8,
        cpol: true,
        cpha: true,
        cs_active_high: true,
        three_wire: false,
    };
    let reset = Gpio {
        interrupt: false,
        pins: alloc::vec![305, 23],
        controller: "\\_SB.GPI0".into(),
        pull: 2,
        restriction: 2,
        edge: false,
        polarity: 0,
        shared: true,
        wake: false,
        debounce: 0,
    };
    let irq = Gpio {
        interrupt: true,
        pins: alloc::vec![303],
        controller: "\\_SB.GPI0".into(),
        pull: 1,
        restriction: 0,
        edge: true,
        polarity: 2,
        shared: false,
        wake: true,
        debounce: 100,
    };
    let template = cat(&[&spi_descriptor_of(&spi), &gpio_descriptor_of(&reset), &gpio_descriptor_of(&irq), &END_TAG]);
    let r = resource::parse(&template).unwrap();
    assert_eq!(r, [Resource::Spi(spi), Resource::Gpio(reset), Resource::Gpio(irq)]);
}

#[test]
fn the_descriptors_veda_writes_read_back() {
    let template = cat(&[
        &io(0x60, 1),
        &irq_no_flags(12),
        &word_bus_number(0, 0),
        &dword_memory(0xC000_0000, 0xFEBF_FFFF),
        &qword_memory(0x20_0000_0000, 0x3F_FFFF_FFFF),
        &END_TAG,
    ]);
    let r = resource::parse(&template).unwrap();
    assert_eq!(r[0], Resource::Io { base: 0x60, length: 1 });
    assert!(matches!(&r[1], Resource::Irq { irqs, edge: true, active_low: false, .. } if irqs == &[12]));
    assert_eq!(r[2], Resource::Window { kind: 2, min: 0, max: 0, translation: 0, length: 1 });
    assert_eq!(
        r[3],
        Resource::Window { kind: 0, min: 0xC000_0000, max: 0xFEBF_FFFF, translation: 0, length: 0x3EC0_0000 }
    );
    assert_eq!(
        r[4],
        Resource::Window { kind: 0, min: 0x20_0000_0000, max: 0x3F_FFFF_FFFF, translation: 0, length: 0x20_0000_0000 }
    );
}

#[test]
fn connections_and_interrupts_veda_writes_read_back() {
    let template = cat(&[
        &i2c_descriptor(0x15, 400_000, false, "\\_SB.PCI0.S29"),
        &interrupt_of(40, false, true, false, true),
        &END_TAG,
    ]);
    let r = resource::parse(&template).unwrap();
    assert_eq!(
        r[0],
        Resource::I2c(crate::resource::I2c {
            controller: String::from("\\_SB.PCI0.S29"),
            speed_hz: 400_000,
            address: 0x15,
            ten_bit: false,
        })
    );
    assert!(
        matches!(&r[1], Resource::Irq { irqs, edge: false, active_low: true, shared: false, wake: true } if irqs == &[40])
    );
}

#[test]
fn a_dsm_answers_what_hid_over_i2c_asks() {
    let hid = uuid("3cdff6f7-4267-4555-ad05-b30a3d8938de").unwrap();
    assert_eq!(hid[..4], [0xF7, 0xF6, 0xDF, 0x3C]);
    assert_eq!(hid[8..10], [0xAD, 0x05]);
    assert_eq!(uuid("3cdff6f7-4267-4555-ad05"), None);
    let aml = device("TPD0", &dsm(&hid, 1, &int(0x20)));
    let mut ns = Namespace::new();
    ns.load(&aml, &crate::NoMemory).unwrap();
    let call = |u: &[u8; 16], function: u64| {
        let args = [Value::Buffer(u.to_vec()), Value::Integer(1), Value::Integer(function), Value::Package(vec![])];
        ns.evaluate(&path("\\TPD0._DSM"), &args, &crate::NoMemory)
    };
    assert_eq!(call(&hid, 1), Ok(Value::Integer(0x20)));
    assert_eq!(call(&hid, 0), Ok(Value::Buffer(vec![3])));
    assert_eq!(call(&[0; 16], 1), Ok(Value::Buffer(vec![0])));
}

#[test]
fn constants_are_written_as_they_evaluate() {
    let v = Value::Package(vec![
        Value::Integer(0x264),
        Value::String(String::from("cirrus,dev-index")),
        Value::Buffer(vec![1, 2]),
    ]);
    let mut ns = Namespace::new();
    ns.load(&name("DATA", &constant(&v).unwrap()), &crate::NoMemory).unwrap();
    assert_eq!(eval(&ns, "\\DATA", &crate::NoMemory), Ok(v));
    assert_eq!(constant(&Value::Reference(path("\\_SB"))), None);
}

#[test]
fn pci_interrupts_in_apic_mode() {
    // As QEMU's q35 has it: `_PRT` answers for the model `_PIC` sets, its
    // routes through interrupt link devices defined after it (in APIC
    // mode, to the I/O APIC's inputs from 16, level-triggered and active
    // high); a route can name a GSI itself too.
    let gsi_link = |irq: u32| cat(&[&[0x89, 6, 0, 0x09, 1], &irq.to_le_bytes(), &END_TAG]);
    let aml = cat(&[
        &name("PICF", &int(0)),
        &method("_PIC", 1, &store(&[ARG0], &nm("PICF"))),
        &scope(
            "\\_SB",
            &cat(&[
                &device(
                    "PCI0",
                    &cat(&[
                        &name("_HID", &eisa("PNP0A08")),
                        &name("PRTP", &package(&[package(&[int(0x0002_FFFF), int(0), nm("LNKA"), int(0)])])),
                        &name(
                            "PRTA",
                            &package(&[
                                package(&[int(0x0002_FFFF), int(0), nm("GSIA"), int(0)]),
                                package(&[int(0x0003_FFFF), int(1), int(0), int(0x11)]),
                            ]),
                        ),
                        &method("_PRT", 0, &cat(&[&if_(&nm("PICF"), &ret(&nm("PRTA"))), &ret(&nm("PRTP"))])),
                    ]),
                ),
                &device("GSIA", &cat(&[&name("_HID", &eisa("PNP0C0F")), &name("_CRS", &buffer(&gsi_link(16)))])),
                // IRQ (Level, ActiveLow, Shared) {11}: the PIC's.
                &device(
                    "LNKA",
                    &cat(&[
                        &name("_HID", &eisa("PNP0C0F")),
                        &name("_CRS", &buffer(&[0x23, 0x00, 0x08, 0x18, 0x79, 0])),
                    ]),
                ),
            ]),
        ),
    ]);
    let mut ns = Namespace::new();
    ns.load(&aml, &crate::NoMemory).unwrap();
    let roots = device::pci_roots(&ns, &crate::NoMemory);
    let route = |slot, pin| device::pci_interrupt(&ns, &roots, (0, slot), pin, &crate::NoMemory);
    assert_eq!(route(2, 1), Some(device::IntxRoute { gsi: 16, level: true, active_low: false }));
    assert_eq!(route(3, 2), Some(device::IntxRoute { gsi: 0x11, level: true, active_low: true }));
    assert_eq!(route(3, 1), None);
    // Nothing the evaluation stored stays: the namespace is in PIC mode.
    assert_eq!(eval(&ns, "\\PICF", &crate::NoMemory), Ok(Value::Integer(0)));
}

#[test]
fn a_root_bridges_windows_from_the_host_bridge_and_the_firmwares_variables() {
    // As an Alder Lake laptop's firmware has them: the root bridge's _CRS
    // sizes its bus range by the host bridge's PCIEXBAR, read through a
    // PCI_Config region of the device at _ADR 0 below it (00:00.0), and
    // takes its memory windows from the firmware's variables, emptying the
    // 64-bit one when there is none.
    let template = cat(&[&word_bus_number(0, 0xFF), &dword_memory(0, 0xDFFF_FFFF), &qword_memory(0x1_0000, 0x1_FFFF)]);
    let create = |op: u8, offset: u64, field: &str| cat(&[&[op], &nm("BUF0"), &int(offset), &nm(field)]);
    let (word, dword, qword) = (0x8B, 0x8A, 0x8F);
    let shift_right = |a: &[u8], b: &[u8], target: &[u8]| binary(0x7A, a, b, target);
    let minus = |a: &[u8], n: u64| binary(0x74, a, &int(n), &[0]);
    let buses = shift_right(&[LOCAL0], &int(20), &[0]);
    let crs = method(
        "_CRS",
        0,
        &cat(&[
            &shift_right(&int(0x1000_0000), &nm("^MC.PXSZ"), &[LOCAL0]),
            &create(word, 10, "PBMX"),
            &store(&minus(&buses, 2), &nm("PBMX")),
            &create(word, 14, "PBLN"),
            &store(&minus(&buses, 1), &nm("PBLN")),
            &create(dword, 26, "M1MN"),
            &create(dword, 30, "M1MX"),
            &create(dword, 38, "M1LN"),
            &store(&nm("M32L"), &nm("M1LN")),
            &store(&nm("M32B"), &nm("M1MN")),
            &store(&minus(&add(&nm("M1MN"), &nm("M1LN")), 1), &nm("M1MX")),
            &if_(&lequal(&nm("M64L"), &int(0)), &cat(&[&create(qword, 80, "MSLN"), &store(&int(0), &nm("MSLN"))])),
            &else_(&cat(&[
                &create(qword, 56, "M2MN"),
                &create(qword, 64, "M2MX"),
                &create(qword, 80, "M2LN"),
                &store(&nm("M64L"), &nm("M2LN")),
                &store(&nm("M64B"), &nm("M2MN")),
                &store(&minus(&add(&nm("M2MN"), &nm("M2LN")), 1), &nm("M2MX")),
            ])),
            &ret(&nm("BUF0")),
        ]),
    );
    let dsdt = cat(&[
        &region("SANV", 0, &int(0x5000), &int(0x18)),
        &field("SANV", 0, &[("M32B", 32), ("M32L", 32), ("M64B", 64), ("M64L", 64)]),
        &scope(
            "\\_SB",
            &device(
                "PC00",
                &cat(&[
                    &name("_HID", &eisa("PNP0A08")),
                    &name("_CID", &eisa("PNP0A03")),
                    &name("_ADR", &int(0)),
                    &device(
                        "MC",
                        &cat(&[
                            &name("_ADR", &int(0)),
                            &region("HBUS", 2, &int(0), &int(0x100)),
                            &field("HBUS", 3, &[("", 0x60 * 8), ("PXEN", 1), ("PXSZ", 3)]),
                        ]),
                    ),
                    &name("BUF0", &resource_template(&template)),
                    &crs,
                ]),
            ),
        ),
    ]);
    let mut ram = Ram::default();
    // The windows Linux finds on the laptop, where the firmware keeps them.
    ram.put(0x5000, &0x6880_0000u32.to_le_bytes());
    ram.put(0x5004, &0x5780_0000u32.to_le_bytes());
    ram.put(0x5008, &0x40_0000_0000u64.to_le_bytes());
    ram.put(0x5010, &0x40_0000_0000u64.to_le_bytes());
    // PCIEXBAR on, 128 MiB: 128 buses.
    ram.put_pci((0, 0, 0), 0x60, &[0x03]);
    let mut ns = Namespace::new();
    ns.load(&dsdt, &ram).unwrap();
    let root = path("\\_SB.PC00");
    assert_eq!(
        device::pci_root_windows(&ns, &root, &ram),
        Ok(vec![0x6880_0000..0xC000_0000, 0x40_0000_0000..0x80_0000_0000])
    );
    let r = device::resources(&ns, &root, &ram).unwrap();
    assert_eq!(r[0], Resource::Window { kind: 2, min: 0, max: 0x7E, translation: 0, length: 0x7F });
    // No 64-bit window: the firmware empties it, which leaves it out.
    ram.put(0x5010, &0u64.to_le_bytes());
    let low = 0x6880_0000..0xC000_0000;
    assert_eq!(device::pci_root_windows(&ns, &root, &ram), Ok(Vec::from([low])));
    // Without the host bridge's registers, the windows are not known.
    assert_eq!(
        device::pci_root_windows(&ns, &root, &crate::NoMemory),
        Err(ResourcesError::Aml(Error::PciConfig(PciFunction::default(), 0x60)))
    );
}

#[test]
fn configuration_space_below_bridges() {
    // A PCI_Config region is the configuration space of the function at
    // the _ADR of the device it is in, on its root bridge's bus (_BBN), or
    // on the secondary bus of the bridge it is below; one in the root
    // bridge's own scope is the root's. The tables' conditions read them
    // as they load.
    let id = |r: &str, f: &str| cat(&[&region(r, 2, &int(0), &int(0x100)), &field(r, 2, &[(f, 16)])]);
    let dsdt = scope(
        "\\_SB",
        &device(
            "PC10",
            &cat(&[
                &name("_HID", &eisa("PNP0A08")),
                &name("_BBN", &int(0x10)),
                &id("OWNC", "RVID"),
                &device(
                    "RP01",
                    &cat(&[
                        &name("_ADR", &int(0x001C_0000)),
                        &id("RPCS", "BVID"),
                        &device(
                            "PXSX",
                            &cat(&[&name("_ADR", &int(0)), &id("PCFG", "DVID"), &method("VEND", 0, &ret(&nm("DVID")))]),
                        ),
                    ]),
                ),
                &if_(&lequal(&nm("\\_SB.PC10.RP01.PXSX.DVID"), &int(0x3333)), &device("SEEN", &name("_ADR", &int(1)))),
                &device("RP02", &cat(&[&name("_ADR", &int(0x001D_0000)), &id("RPCS", "BVID")])),
            ]),
        ),
    );
    let mut ram = Ram::default();
    ram.put_pci((0x10, 0, 0), 0, &[0x11, 0x11]);
    // The root port: a bridge to bus 0x13.
    ram.put_pci((0x10, 0x1C, 0), 0, &[0x22, 0x22]);
    ram.put_pci((0x10, 0x1C, 0), 0x0E, &[0x01]);
    ram.put_pci((0x10, 0x1C, 0), 0x19, &[0x13]);
    ram.put_pci((0x13, 0, 0), 0, &[0x33, 0x33]);
    let mut ns = Namespace::new();
    ns.load(&dsdt, &ram).unwrap();
    assert_eq!(eval(&ns, "\\_SB.PC10.RVID", &ram), Ok(Value::Integer(0x1111)));
    assert_eq!(eval(&ns, "\\_SB.PC10.RP01.BVID", &ram), Ok(Value::Integer(0x2222)));
    assert_eq!(eval(&ns, "\\_SB.PC10.RP01.PXSX.VEND", &ram), Ok(Value::Integer(0x3333)));
    assert!(ns.get(&path("\\_SB.PC10.SEEN")).is_some());
    // A function the machine does not have.
    let rp02 = PciFunction { segment: 0, bus: 0x10, device: 0x1D, function: 0 };
    assert_eq!(eval(&ns, "\\_SB.PC10.RP02.BVID", &ram), Err(Error::PciConfig(rp02, 0)));
}

#[test]
fn integers_are_32_bits_under_a_revision_1_dsdt() {
    let aml = method("ONES", 0, &ret(&[0xFF]));
    let mut data = vec![0u8; 36];
    data[..4].copy_from_slice(b"DSDT");
    data[8] = 1;
    data.extend_from_slice(&aml);
    let len = data.len() as u32;
    data[4..8].copy_from_slice(&len.to_le_bytes());
    let table = crate::tables::Table { address: 0, data };
    let mut ns = Namespace::new();
    ns.load_table(&table, &crate::NoMemory).unwrap();
    assert_eq!(eval(&ns, "\\ONES", &crate::NoMemory), Ok(Value::Integer(0xFFFF_FFFF)));
}

#[test]
fn malformed_code_is_an_error() {
    let good = device("DEV0", &name("_ADR", &int(1)));
    let mut ns = Namespace::new();
    // A package length pointing past the end.
    let mut bad = good.clone();
    bad[2] = 0x3F;
    assert!(ns.load(&bad, &crate::NoMemory).is_err());
    // A method calling one that does not exist fails when it runs.
    let code = method("CALL", 0, &ret(&cat(&[&nm("MISS"), &int(1)])));
    let mut ns = Namespace::new();
    ns.load(&code, &crate::NoMemory).unwrap();
    assert!(eval(&ns, "\\CALL", &crate::NoMemory).is_err());
}

#[test]
fn tables_from_the_rsdp() {
    let mut ram = Ram::default();
    let table = |signature: &[u8; 4], body: &[u8]| -> Vec<u8> {
        let mut t = vec![0u8; 36];
        t[..4].copy_from_slice(signature);
        t[8] = 2;
        t.extend_from_slice(body);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        let sum = t.iter().fold(0u8, |s, &b| s.wrapping_add(b));
        t[9] = 0u8.wrapping_sub(sum);
        t
    };
    let dsdt = table(b"DSDT", &device("DEV0", &[]));
    let mut fadt_body = vec![0u8; 148 - 36];
    fadt_body[140 - 36..148 - 36].copy_from_slice(&0x3000u64.to_le_bytes());
    let fadt = table(b"FACP", &fadt_body);
    let ssdt = table(b"SSDT", &[]);
    let xsdt = table(b"XSDT", &cat(&[&0x2000u64.to_le_bytes(), &0x4000u64.to_le_bytes()]));
    let mut rsdp = b"RSD PTR ".to_vec();
    rsdp.extend([0, 0, 0, 0, 0, 0, 0, 2]);
    rsdp.extend_from_slice(&0u32.to_le_bytes());
    rsdp.extend_from_slice(&36u32.to_le_bytes());
    rsdp.extend_from_slice(&0x1000u64.to_le_bytes());
    rsdp.extend([0; 4]);
    ram.put(0x100, &rsdp);
    ram.put(0x1000, &xsdt);
    ram.put(0x2000, &fadt);
    ram.put(0x3000, &dsdt);
    ram.put(0x4000, &ssdt);
    let tables = crate::tables::load(&ram, 0x100).unwrap();
    let names: Vec<[u8; 4]> = tables.iter().map(|t| t.signature()).collect();
    assert_eq!(names, [*b"FACP", *b"SSDT", *b"DSDT"]);
    assert!(tables.iter().all(|t| t.checksum_ok()));
    assert_eq!(crate::tables::load(&ram, 0x200).unwrap_err(), crate::tables::TableError::NoRsdp);
}
