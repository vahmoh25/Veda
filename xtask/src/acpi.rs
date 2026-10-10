//! `cargo xtask acpi DIR`: what devmgr makes of a PC's ACPI tables, worked
//! out on the host. DIR holds the tables as Linux has them (`sudo cp -r
//! /sys/firmware/acpi/tables DIR`); the same interpreter (`vacpi`) loads
//! the DSDT and the SSDTs and evaluates them by devmgr's rules: the PCI
//! root bridges' windows (where devmgr places the BARs a firmware leaves
//! unplaced), the memory the motherboard reserves, and for each function on
//! a root bus its ACPI device, where its INTx goes and the devices
//! described below it, with whatever could not be evaluated.
//!
//! The firmware's variables (its NVS memory, which Linux does not show)
//! read as zeros, but those given with `--set NAME=VALUE`. PCI
//! configuration space is read from files `DIR/pci/BB:DD.F` if there are
//! some, else from this machine's `/sys/bus/pci/devices` (64 bytes of each
//! function, all of it as root).

use std::collections::BTreeMap;
use std::path::Path;

use vacpi::aml::{Bounds, FieldUnit, Namespace, Object};
use vacpi::device;
use vacpi::name::Path as AcpiPath;
use vacpi::resource::Resource;
use vacpi::tables::Table;
use vacpi::{Memory, PciFunction};

use crate::util::Result;

const USAGE: &str = "usage: cargo xtask acpi DIR [--set NAME=VALUE]...";

/// The machine as the host knows it.
#[derive(Default)]
struct Host {
    /// Firmware memory given with `--set`; zeros elsewhere.
    memory: BTreeMap<u64, u8>,
    pci: BTreeMap<PciFunction, Vec<u8>>,
}

impl Memory for Host {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.memory.get(&address.wrapping_add(i as u64)).copied().unwrap_or(0);
        }
        true
    }

    fn read_pci(&self, function: PciFunction, offset: u16, buf: &mut [u8]) -> bool {
        let Some(space) = self.pci.get(&function) else {
            // No function there: what the bus answers.
            buf.fill(0xFF);
            return true;
        };
        let at = offset as usize;
        match space.get(at..at + buf.len()) {
            Some(bytes) => {
                buf.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    }
}

pub fn command(args: &[String]) -> Result {
    let mut dir = None;
    let mut sets = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--set" => sets.push(it.next().ok_or(USAGE)?.as_str()),
            a if dir.is_none() && !a.starts_with('-') => dir = Some(Path::new(a)),
            _ => return Err(USAGE.into()),
        }
    }
    let dir = dir.ok_or(USAGE)?;
    let tables = definition_blocks(dir)?;
    let mut host = Host::default();
    let source = match spaces(&dir.join("pci")) {
        Some(spaces) if !spaces.is_empty() => {
            host.pci = spaces;
            format!("{}", dir.join("pci").display())
        }
        _ => {
            host.pci = spaces(Path::new("/sys/bus/pci/devices")).unwrap_or_default();
            "this machine's /sys/bus/pci/devices".into()
        }
    };
    // The variables go where their fields are, which loading the tables
    // tells; then they load again, with them.
    if !sets.is_empty() {
        let (ns, _) = load(&tables, &host);
        for s in sets {
            set(&ns, &mut host, s)?;
        }
    }
    let (ns, failed) = load(&tables, &host);
    let names: Vec<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
    println!("tables: {}", names.join(", "));
    for (name, e) in failed {
        println!("  {name} loaded only in part: {e}");
    }
    println!("{} devices; {} conditional definitions left out", ns.devices().count(), ns.skipped);
    for (scope, e) in &ns.skip_reasons {
        println!("  in {scope}: {e}");
    }
    println!("PCI configuration space from {source}");

    let roots = device::pci_roots(&ns, &host);
    for (bus, root) in &roots {
        match device::pci_root_windows(&ns, root, &host) {
            Ok(w) if w.is_empty() => println!("{root} (bus {bus:02x}): no memory windows"),
            Ok(w) => println!("{root} (bus {bus:02x}): windows {}", ranges(&w)),
            Err(e) => println!("{root} (bus {bus:02x}): {e}: its windows are not known"),
        }
    }
    println!("the motherboard reserves:");
    for r in device::motherboard_memory(&ns, &host) {
        match r.memory {
            Ok(m) if m.is_empty() => println!("  {}: no memory", r.device),
            Ok(m) => println!("  {}: {}", r.device, ranges(&m)),
            Err(e) => println!("  {}: {e}: what it reserves is not known", r.device),
        }
    }
    println!("functions on root buses:");
    for (f, space) in &host.pci {
        if f.segment != 0 || !roots.iter().any(|(bus, _)| *bus == f.bus) || space.len() < 0x40 {
            continue;
        }
        let word = |at: usize| u16::from_le_bytes([space[at], space[at + 1]]);
        let mut line = format!(
            "  {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}{:02x}",
            f.bus,
            f.device,
            f.function,
            word(0),
            word(2),
            space[0x0B],
            space[0x0A],
            space[0x09]
        );
        let pin = space[0x3D];
        if (1..=4).contains(&pin) {
            let route = device::pci_interrupt(&ns, &roots, (f.bus, f.device), pin, &host);
            line += &match route {
                Some(r) => format!(
                    ", INT{}# on GSI {} ({}, active {})",
                    (b'A' + pin - 1) as char,
                    r.gsi,
                    if r.level { "level" } else { "edge" },
                    if r.active_low { "low" } else { "high" }
                ),
                None => format!(", INT{}# not routed", (b'A' + pin - 1) as char),
            };
        }
        let companion = device::pci_companion(&ns, &roots, (f.bus, f.device, f.function), &host);
        match &companion {
            Some(c) => println!("{line}: {c}"),
            None => println!("{line}"),
        }
        for d in companion.iter().flat_map(|c| device::describe_children(&ns, c, &host)) {
            let id = d.identity.hid.clone().or_else(|| d.identity.cids.first().cloned());
            let used = match &d.resources {
                Ok(r) if r.is_empty() => "uses nothing".to_string(),
                Ok(r) => r.iter().map(resource).collect::<Vec<_>>().join("; "),
                Err(e) => format!("{e}"),
            };
            println!("      {} ({}): {used}", d.path, id.as_deref().unwrap_or("no id"));
        }
    }
    Ok(())
}

/// The definition blocks in `dir`, in the firmware's order: the DSDT, then
/// the SSDTs as Linux numbers them.
fn definition_blocks(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut blocks = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let order = match name.strip_prefix("SSDT") {
            Some(n) => 1 + n.trim_end_matches(".dat").parse::<u32>().unwrap_or(0),
            None if name.starts_with("DSDT") => 0,
            None => continue,
        };
        let data = std::fs::read(e.path()).map_err(|e| format!("{name}: {e} (the tables are root's: copy them)"))?;
        if data.len() < 36 {
            return Err(format!("{name} is not an ACPI table"));
        }
        blocks.push((order, name, data));
    }
    if !blocks.iter().any(|b| b.0 == 0) {
        return Err(format!("no DSDT in {}", dir.display()));
    }
    blocks.sort();
    Ok(blocks.into_iter().map(|(_, n, d)| (n, d)).collect())
}

/// The namespace the tables make, and the tables that loaded only in part.
fn load(tables: &[(String, Vec<u8>)], host: &Host) -> (Namespace, Vec<(String, vacpi::aml::Error)>) {
    let mut ns = Namespace::new();
    let mut failed = Vec::new();
    for (name, data) in tables {
        let table = Table { address: 0, data: data.clone() };
        if let Err(e) = ns.load_table(&table, host) {
            failed.push((name.clone(), e));
        }
    }
    (ns, failed)
}

/// The configuration spaces in `dir`, named as `/sys/bus/pci/devices` names
/// them (`0000:00:1f.3`, with a `config` file) or as `00:1f.3` files.
fn spaces(dir: &Path) -> Option<BTreeMap<PciFunction, Vec<u8>>> {
    let mut spaces = BTreeMap::new();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(f) = pci_function(&name) else { continue };
        let file = if e.path().is_dir() { e.path().join("config") } else { e.path() };
        if let Ok(bytes) = std::fs::read(file) {
            spaces.insert(f, bytes);
        }
    }
    Some(spaces)
}

/// `0000:00:1f.3` or `00:1f.3`.
fn pci_function(name: &str) -> Option<PciFunction> {
    let parts: Vec<&str> = name.split(':').collect();
    let (segment, bus, rest) = match parts.as_slice() {
        [s, b, r] => (u16::from_str_radix(s, 16).ok()?, *b, *r),
        [b, r] => (0, *b, *r),
        _ => return None,
    };
    let (device, function) = rest.split_once('.')?;
    Some(PciFunction {
        segment,
        bus: u8::from_str_radix(bus, 16).ok()?,
        device: u8::from_str_radix(device, 16).ok()?,
        function: function.parse().ok()?,
    })
}

/// Puts `NAME=VALUE` where the firmware keeps the variable: its field of a
/// `SystemMemory` region.
fn set(ns: &Namespace, host: &mut Host, spec: &str) -> Result {
    let bad = |why: &str| format!("--set {spec}: {why}");
    let (name, value) = spec.split_once('=').ok_or_else(|| bad("NAME=VALUE expected"))?;
    let name = if name.starts_with('\\') { name.to_string() } else { format!("\\{name}") };
    let path = AcpiPath::parse(&name).ok_or_else(|| bad("not a name"))?;
    let value = match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    }
    .map_err(|_| bad("not a number"))?;
    let Some(Object::Field(field)) = ns.get(&path) else { return Err(bad("not a field")) };
    let FieldUnit::Region(region) = &field.unit else { return Err(bad("not a region's field")) };
    let Some(Object::Region { space: 0, bounds: Bounds::Known { offset, .. } }) = ns.get(region) else {
        return Err(bad("not in firmware memory at an address the tables give"));
    };
    if field.bit_offset % 8 != 0 || field.bit_width % 8 != 0 || field.bit_width > 64 {
        return Err(bad("not whole bytes"));
    }
    let at = offset + field.bit_offset / 8;
    for i in 0..field.bit_width / 8 {
        host.memory.insert(at + i, (value >> (8 * i)) as u8);
    }
    println!("{path} = {value:#x} (at {at:#x})");
    Ok(())
}

fn ranges(r: &[std::ops::Range<u64>]) -> String {
    r.iter().map(|r| format!("{:#x}-{:#x}", r.start, r.end - 1)).collect::<Vec<_>>().join(", ")
}

fn resource(r: &Resource) -> String {
    let list = |v: &[u32]| v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    match r {
        Resource::Irq { irqs, edge, active_low, .. } => format!(
            "interrupt {} ({}, active {})",
            list(irqs),
            if *edge { "edge" } else { "level" },
            if *active_low { "low" } else { "high" }
        ),
        Resource::Io { base, length } => format!("I/O {base:#x} ({length})"),
        Resource::Memory { base, length, .. } => format!("memory {base:#x} ({length:#x})"),
        Resource::Window { kind, min, max, length, .. } => {
            let kind = ["memory", "I/O", "bus"].get(*kind as usize).unwrap_or(&"other");
            format!("{kind} window {min:#x}-{max:#x}{}", if *length == 0 { " (empty)" } else { "" })
        }
        Resource::Gpio(g) => {
            let pins = g.pins.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
            format!("GPIO{} {pins} on {}", if g.interrupt { " interrupt" } else { "" }, g.controller)
        }
        Resource::Spi(s) => format!("SPI chip select {} at {} Hz on {}", s.chip_select, s.speed_hz, s.controller),
        Resource::I2c(i) => format!("I2C {:#x} at {} Hz on {}", i.address, i.speed_hz, i.controller),
        Resource::Other { kind } => format!("descriptor {kind:#x}"),
    }
}
