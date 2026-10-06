use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use std::collections::BTreeMap;

use crate::Memory;
use crate::aml::{Error, Namespace, Object};
use crate::asm::*;
use crate::device::{self, eisa_id};
use crate::name::{NameString, Path};
use crate::resource::{self, Gpio, Resource, Spi};
use crate::value::Value;

/// Firmware memory: bytes at given addresses, nothing elsewhere.
#[derive(Default)]
struct Ram(BTreeMap<u64, u8>);

impl Ram {
    fn put(&mut self, address: u64, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.0.insert(address + i as u64, b);
        }
    }
}

impl Memory for Ram {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        for (i, b) in buf.iter_mut().enumerate() {
            match self.0.get(&(address + i as u64)) {
                Some(&v) => *b = v,
                None => return false,
            }
        }
        true
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

/// Loads a machine's own tables, dumped as `<signature>[-n].dat` files in
/// the directory `VEDA_ACPI_DUMP` names, and evaluates every device. Run
/// with `cargo test -p vacpi -- --ignored`.
#[test]
#[ignore]
fn a_machines_tables() {
    let Ok(dir) = std::env::var("VEDA_ACPI_DUMP") else { return };
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let mut ns = Namespace::new();
    // Firmware variables read as zeros.
    struct Zeros;
    impl Memory for Zeros {
        fn read(&self, _address: u64, buf: &mut [u8]) -> bool {
            buf.fill(0);
            true
        }
    }
    let load = |ns: &mut Namespace, f: &std::path::Path| {
        let data = std::fs::read(f).unwrap();
        let table = crate::tables::Table { address: 0, data };
        let r = ns.load_table(&table, &Zeros);
        std::println!("{}: {:?}", f.display(), r.err());
    };
    for f in files.iter().filter(|f| f.file_name().unwrap().to_string_lossy().starts_with("DSDT")) {
        load(&mut ns, f);
    }
    for f in files.iter().filter(|f| {
        f.file_name().unwrap().to_string_lossy().starts_with("SSD")
            && !f.file_name().unwrap().to_string_lossy().starts_with("SSDT")
    }) {
        load(&mut ns, f);
    }
    let devices: Vec<Path> = ns.devices().cloned().collect();
    let (mut ok, mut failed) = (0, 0);
    for d in &devices {
        let id = device::identify(&ns, d, &Zeros);
        match device::resources(&ns, d, &Zeros) {
            Ok(_) | Err(device::ResourcesError::None) => ok += 1,
            Err(e) => {
                failed += 1;
                std::println!("{} ({:?}): {}", d, id.hid, e);
            }
        }
    }
    for (s, e) in &ns.skip_reasons {
        std::println!("skipped in {}: {}", s, e);
    }
    std::println!(
        "{} devices: {} fine, {} with _CRS failing; {} conditional blocks skipped",
        devices.len(),
        ok,
        failed,
        ns.skipped
    );
    // As devmgr finds the SPI controller at 00:1e.3: the root bridge by
    // its ids, then the child whose _ADR is the function's.
    let roots: Vec<Path> = devices
        .iter()
        .filter(|d| device::ids(&ns, d, &Zeros).iter().any(|i| i == "PNP0A08" || i == "PNP0A03"))
        .cloned()
        .collect();
    let bus = ns.evaluate_child(&roots[0], "_BBN", &Zeros).and_then(Result::ok);
    let companion = device::children(&ns, &roots[0]).into_iter().find(|d| {
        ns.evaluate_child(d, "_ADR", &Zeros).and_then(Result::ok).and_then(|v| v.as_integer()) == Some(0x001E_0003)
    });
    std::println!("roots {:?} (bus {:?}); 00:1e.3 is {:?}", roots, bus, companion);
    for p in ["\\_SB.PC00.SPI1", "\\_SB.PC00.SPI1.SPK1", "\\_SB.GPI0", "\\_SB.PC00.HDAS"] {
        let p = path(p);
        std::println!("{}: {:?} {:?}", p, device::identify(&ns, &p, &Zeros), device::resources(&ns, &p, &Zeros));
    }
}
