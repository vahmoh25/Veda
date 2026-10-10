//! What a device in the namespace is, and what it uses: its identification
//! objects (`_HID`, `_CID`, `_UID`, `_ADR`, `_SUB`, `_STA`) and its
//! current resources (`_CRS`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::Memory;
use crate::aml::{self, Namespace};
use crate::name::Path;
use crate::resource::{self, Resource, ResourceError};
use crate::value::Value;

/// `_STA` bits: present, enabled, shown, functioning.
pub const STA_PRESENT: u64 = 1;
/// What a device without `_STA` is.
pub const STA_DEFAULT: u64 = 0xF;

/// A device's identification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// Hardware id: `CSC3551`, `INTC1055`, `PNP0A08`.
    pub hid: Option<String>,
    /// Compatible ids.
    pub cids: Vec<String>,
    /// Unique id, as text.
    pub uid: Option<String>,
    /// Address on its parent bus (PCI: device << 16 | function).
    pub adr: Option<u64>,
    /// Subsystem id: `10431F62`.
    pub sub: Option<String>,
    /// `_STA` ([`STA_DEFAULT`] if the device has none or it fails).
    pub status: u64,
}

impl Identity {
    /// Whether the device has `id` as its hardware or a compatible id.
    pub fn is(&self, id: &str) -> bool {
        self.hid.as_deref() == Some(id) || self.cids.iter().any(|c| c == id)
    }

    pub fn present(&self) -> bool {
        self.status & STA_PRESENT != 0
    }
}

/// Decodes a compressed EISA id (`PNP0A08`) as AML holds it.
pub fn eisa_id(value: u64) -> String {
    let v = (value as u32).swap_bytes();
    let letter = |shift: u32| (b'@' + (v >> shift & 0x1F) as u8) as char;
    format!("{}{}{}{:04X}", letter(26), letter(21), letter(16), v & 0xFFFF)
}

/// An id: a string, or a compressed EISA id.
fn id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Integer(i) => Some(eisa_id(*i)),
        _ => None,
    }
}

/// A device's hardware id and compatible ids only (evaluating nothing
/// else), for finding devices of a kind.
pub fn ids(ns: &Namespace, device: &Path, memory: &dyn Memory) -> Vec<String> {
    let get = |name: &str| ns.evaluate_child(device, name, memory).and_then(Result::ok);
    let mut ids: Vec<String> = get("_HID").as_ref().and_then(id).into_iter().collect();
    match get("_CID") {
        Some(Value::Package(items)) => ids.extend(items.iter().filter_map(id)),
        Some(v) => ids.extend(id(&v)),
        None => {}
    }
    ids
}

/// Reads a device's identification objects. Ones that are missing or fail
/// are left out.
pub fn identify(ns: &Namespace, device: &Path, memory: &dyn Memory) -> Identity {
    let get = |name: &str| ns.evaluate_child(device, name, memory).and_then(Result::ok);
    let cids = match get("_CID") {
        Some(Value::Package(items)) => items.iter().filter_map(id).collect(),
        Some(v) => id(&v).into_iter().collect(),
        None => Vec::new(),
    };
    Identity {
        hid: get("_HID").as_ref().and_then(id),
        cids,
        uid: get("_UID").and_then(|v| match v {
            Value::Integer(i) => Some(format!("{}", i)),
            Value::String(s) => Some(s),
            _ => None,
        }),
        adr: get("_ADR").and_then(|v| v.as_integer()),
        sub: get("_SUB").and_then(|v| v.as_str().map(String::from)),
        status: match ns.evaluate_child(device, "_STA", memory) {
            Some(Ok(v)) => v.as_integer().unwrap_or(STA_DEFAULT),
            _ => STA_DEFAULT,
        },
    }
}

/// Why a device's resources could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcesError {
    /// The device has no `_CRS`.
    None,
    Aml(aml::Error),
    /// `_CRS` returned something other than a buffer.
    NotABuffer,
    Template(ResourceError),
}

impl fmt::Display for ResourcesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourcesError::None => f.write_str("the device has no _CRS"),
            ResourcesError::Aml(e) => write!(f, "_CRS: {}", e),
            ResourcesError::NotABuffer => f.write_str("_CRS returned no resource template"),
            ResourcesError::Template(e) => write!(f, "_CRS: {}", e),
        }
    }
}

/// The resources a device currently uses (`_CRS`).
pub fn resources(ns: &Namespace, device: &Path, memory: &dyn Memory) -> Result<Vec<Resource>, ResourcesError> {
    let value = ns.evaluate_child(device, "_CRS", memory).ok_or(ResourcesError::None)?.map_err(ResourcesError::Aml)?;
    let template = value.as_buffer().ok_or(ResourcesError::NotABuffer)?;
    resource::parse(template).map_err(ResourcesError::Template)
}

/// The devices directly below `path` (in the namespace, not only on a
/// bus).
pub fn children(ns: &Namespace, path: &Path) -> Vec<Path> {
    ns.children(path).filter(|(_, o)| matches!(o, aml::Object::Device)).map(|(p, _)| p.clone()).collect()
}

/// The PCI root bridges (`PNP0A08`, `PNP0A03`): their bus number (`_BBN`,
/// 0 without) and path. Only the devices' ids are evaluated.
pub fn pci_roots(ns: &Namespace, memory: &dyn Memory) -> Vec<(u8, Path)> {
    let mut roots = Vec::new();
    for d in ns.devices() {
        if ids(ns, d, memory).iter().any(|i| i == "PNP0A08" || i == "PNP0A03") {
            let bus = ns.evaluate_child(d, "_BBN", memory).and_then(Result::ok).and_then(|v| v.as_integer());
            roots.push((bus.unwrap_or(0) as u8, d.clone()));
        }
    }
    roots
}

/// The device that describes PCI function `bus:slot.function`: the root
/// bridge's child whose `_ADR` is the function's (or any function of the
/// slot's). Devices on a root bus only.
pub fn pci_companion(
    ns: &Namespace,
    roots: &[(u8, Path)],
    (bus, slot, function): (u8, u8, u8),
    memory: &dyn Memory,
) -> Option<Path> {
    let (_, root) = roots.iter().find(|(b, _)| *b == bus)?;
    let exact = (slot as u64) << 16 | function as u64;
    let any_function = (slot as u64) << 16 | 0xFFFF;
    children(ns, root).into_iter().find(|d| {
        let adr = ns.evaluate_child(d, "_ADR", memory).and_then(Result::ok).and_then(|v| v.as_integer());
        adr == Some(exact) || adr == Some(any_function)
    })
}

/// Where a PCI function's INTx goes: an I/O APIC input (a GSI), and how
/// it signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntxRoute {
    pub gsi: u32,
    pub level: bool,
    pub active_low: bool,
}

/// Where INTx `pin` (1 for INTA# to 4) of the functions in `slot` of a
/// root bus goes, as the root bridge's `_PRT` says with the interrupt
/// model set to the I/O APICs' (`\_PIC (1)`, as an OS that uses them
/// calls first): a GSI it names (level-triggered, active low), or the one
/// an interrupt link device's `_CRS` holds. Devices on a root bus only.
pub fn pci_interrupt(
    ns: &Namespace,
    roots: &[(u8, Path)],
    (bus, slot): (u8, u8),
    pin: u8,
    memory: &dyn Memory,
) -> Option<IntxRoute> {
    let (_, root) = roots.iter().find(|(b, _)| *b == bus)?;
    let pic = Path::parse("\\_PIC")?;
    let prt = root.join("_PRT")?;
    let Value::Package(routes) = ns.evaluate_after(&pic, &[Value::Integer(1)], &prt, &[], memory).ok()? else {
        return None;
    };
    for route in routes {
        let Value::Package(r) = route else { continue };
        let [address, route_pin, source, index] = r.as_slice() else { continue };
        let (Some(address), Some(route_pin)) = (address.as_integer(), route_pin.as_integer()) else { continue };
        if address >> 16 != slot as u64 || route_pin + 1 != pin as u64 {
            continue;
        }
        let index = index.as_integer().unwrap_or(0);
        return match source {
            Value::Reference(link) => link_interrupt(ns, link, index as usize, memory),
            v if v.as_integer() == Some(0) => Some(IntxRoute { gsi: index as u32, level: true, active_low: true }),
            _ => None,
        };
    }
    None
}

/// The `index`-th interrupt an interrupt link device's `_CRS` holds.
fn link_interrupt(ns: &Namespace, link: &Path, index: usize, memory: &dyn Memory) -> Option<IntxRoute> {
    resources(ns, link, memory).ok()?.into_iter().find_map(|r| match r {
        Resource::Irq { irqs, edge, active_low, .. } => {
            Some(IntxRoute { gsi: *irqs.get(index)?, level: !edge, active_low })
        }
        _ => None,
    })
}

/// A device as its driver sees it: what it is and what it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    pub path: Path,
    pub identity: Identity,
    pub resources: Result<Vec<Resource>, ResourcesError>,
}

impl Described {
    /// Its `index`-th GPIO connection (counting `GpioIo` and `GpioInt` in
    /// order): the controller, resolved in the namespace, and the pin.
    pub fn gpio(&self, ns: &Namespace, index: usize) -> Option<(Path, u16)> {
        let gpio = self
            .resources
            .as_ref()
            .ok()?
            .iter()
            .filter_map(|r| match r {
                Resource::Gpio(g) => Some(g),
                _ => None,
            })
            .nth(index)?;
        let name = crate::name::NameString::parse(&gpio.controller)?;
        Some((ns.resolve(&self.path, &name)?, *gpio.pins.first()?))
    }
}

/// The present devices directly below `path`, described. A device without
/// `_CRS` simply uses nothing.
pub fn describe_children(ns: &Namespace, path: &Path, memory: &dyn Memory) -> Vec<Described> {
    children(ns, path)
        .into_iter()
        .map(|p| Described {
            identity: identify(ns, &p, memory),
            resources: match resources(ns, &p, memory) {
                Err(ResourcesError::None) => Ok(Vec::new()),
                r => r,
            },
            path: p,
        })
        .filter(|d| d.identity.present())
        .collect()
}
