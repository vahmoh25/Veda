//! The firmware's description of the machine (ACPI): which devices it
//! places below which PCI functions, what they are and which resources
//! they use, for their drivers. A driver only learns about the devices
//! below its own PCI function.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ops::Range;

use vabi::{KernelBootInfo, cache_policy, map_flags};
use vacpi::aml::Namespace;
use vacpi::device::{self, Described, Identity, ResourcesError};
use vacpi::name::Path;
use vacpi::resource::Resource as AcpiResource;
use vacpi::tables;
use vacpi::{Memory, PciFunction};
use vproto::pci::DeviceInfo;
use vrt::object::{Resource, Vmo};
use vrt::println;

use crate::pci::{Address, ConfigSpace};

const PAGE: u64 = 4096;

/// Firmware memory, mapped as it is first read: the ACPI tables and the
/// firmware's variables. The tables, and what the kernel reported as the
/// firmware's ACPI memory, are RAM and are mapped cached; anything else
/// AML reads (device registers) uncached. And PCI configuration space, as
/// devmgr reads it (the host bridge's registers, which a root bridge's
/// `_CRS` may size its windows by).
pub struct FirmwareMemory {
    mmio: Resource,
    ram: Vec<(u64, u64)>,
    windows: RefCell<Vec<Window>>,
    config: Rc<ConfigSpace>,
}

struct Window {
    start: u64,
    end: u64,
    cached: bool,
    address: usize,
    _vmo: Vmo,
}

impl FirmwareMemory {
    fn new(mmio: Resource, boot: &KernelBootInfo, config: Rc<ConfigSpace>) -> FirmwareMemory {
        let n = (boot.acpi_memory_count as usize).min(boot.acpi_memory.len());
        let ram = boot.acpi_memory[..n].iter().map(|r| (r[0], r[1])).collect();
        FirmwareMemory { mmio, ram, windows: RefCell::new(Vec::new()), config }
    }

    fn is_ram(&self, start: u64, end: u64) -> bool {
        self.ram.iter().any(|&(s, e)| start >= s && end <= e)
    }

    /// Maps the pages covering `[start, end)`, or finds them mapped.
    fn window(&self, start: u64, end: u64, cached: bool) -> Option<usize> {
        let found = self
            .windows
            .borrow()
            .iter()
            .find(|w| w.cached == cached && start >= w.start && end <= w.end)
            .map(|w| w.address + (start - w.start) as usize);
        if found.is_some() {
            return found;
        }
        let first = start & !(PAGE - 1);
        let mut last = end.div_ceil(PAGE) * PAGE;
        if cached {
            // Tables are read in pieces: map 64 KiB at a time (inside the
            // firmware's range, when it is one the kernel reported).
            let limit = self.ram.iter().find(|&&(s, e)| first >= s && first < e).map_or(last, |&(_, e)| e);
            last = last.max((first + 16 * PAGE).min(limit));
        }
        let size = (last - first) as usize;
        let cache = if cached { cache_policy::WRITE_BACK } else { cache_policy::UNCACHED };
        let vmo = Vmo::create_physical(&self.mmio, first, size, cache).ok()?;
        let address = vmo.map(0, size, map_flags::READ).ok()?;
        self.windows.borrow_mut().push(Window { start: first, end: last, cached, address, _vmo: vmo });
        Some(address + (start - first) as usize)
    }

    fn read_as(&self, address: u64, buf: &mut [u8], cached: bool) -> bool {
        let Some(end) = address.checked_add(buf.len() as u64) else { return false };
        let Some(at) = self.window(address, end, cached) else { return false };
        // SAFETY: `at` maps `buf.len()` readable bytes for as long as the
        // window lives (the life of the process).
        unsafe {
            if cached {
                core::ptr::copy_nonoverlapping(at as *const u8, buf.as_mut_ptr(), buf.len());
            } else if at.is_multiple_of(4) && buf.len().is_multiple_of(4) {
                // Device registers: whole 32-bit reads.
                for (i, chunk) in buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let v = core::ptr::read_volatile((at as *const u32).add(i));
                    chunk.copy_from_slice(&v.to_le_bytes());
                }
            } else {
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = core::ptr::read_volatile((at as *const u8).add(i));
                }
            }
        }
        true
    }
}

impl Memory for FirmwareMemory {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        let cached = address.checked_add(buf.len() as u64).is_some_and(|end| self.is_ram(address, end));
        self.read_as(address, buf, cached)
    }

    /// Through the configuration ports: segment 0, the first 256 bytes.
    fn read_pci(&self, f: PciFunction, offset: u16, buf: &mut [u8]) -> bool {
        if f.segment != 0 || f.device > 31 || f.function > 7 || offset as usize + buf.len() > 256 {
            return false;
        }
        let a = Address { bus: f.bus, slot: f.device, function: f.function };
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.config.read(a, offset + i as u16, 1) as u8;
        }
        true
    }
}

/// The tables themselves, which are always in RAM.
struct Tables<'a>(&'a FirmwareMemory);

impl Memory for Tables<'_> {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        self.0.read_as(address, buf, true)
    }
}

/// A table that describes a device's hardware rather than the machine's,
/// which goes with the device to its driver (`pcidev::acpi_tables`): its
/// signature, and the devices it describes.
struct DeviceTable {
    signature: &'static [u8; 4],
    describes: fn(&DeviceInfo) -> bool,
}

/// An Intel audio controller's NHLT: the links of its DSP, and the
/// microphones and ports on them.
const DEVICE_TABLES: [DeviceTable; 1] = [DeviceTable { signature: b"NHLT", describes: intel_audio }];

fn intel_audio(info: &DeviceInfo) -> bool {
    info.vendor == 0x8086 && info.class == 0x04 && matches!(info.subclass, 0x01 | 0x03)
}

pub struct Acpi {
    pub ns: Namespace,
    pub memory: FirmwareMemory,
    /// PCI root bridges: their bus number and path.
    roots: Vec<(u8, Path)>,
    /// The firmware's [`DEVICE_TABLES`].
    device_tables: Vec<tables::Table>,
    /// The memory the firmware keeps for devices (the DMAR table's RMRRs).
    reserved: Vec<viommu::dmar::Reserved>,
}

impl Acpi {
    /// Reads and loads the firmware's tables. Logs what it found.
    pub fn load(mmio: &Resource, boot: &KernelBootInfo, config: Rc<ConfigSpace>) -> Option<Acpi> {
        if boot.acpi_rsdp == 0 {
            println!("acpi: the firmware has no ACPI tables");
            return None;
        }
        let memory = FirmwareMemory::new(mmio.duplicate().ok()?, boot, config);
        let tables = match tables::load(&Tables(&memory), boot.acpi_rsdp) {
            Ok(t) => t,
            Err(e) => {
                println!("acpi: cannot read the tables: {:?}", e);
                return None;
            }
        };
        let mut ns = Namespace::new();
        let (mut loaded, mut blocks) = (0, 0);
        let aml = tables.iter().filter(|t| t.is(b"DSDT")).chain(tables.iter().filter(|t| t.is(b"SSDT")));
        for t in aml {
            blocks += 1;
            match ns.load_table(t, &memory) {
                Ok(()) => loaded += 1,
                Err(e) => {
                    let s = t.signature();
                    let name = String::from_utf8_lossy(&s);
                    println!("acpi: {} '{}' loaded only in part: {}", name, t.oem_table_id(), e);
                }
            }
        }
        let roots = device::pci_roots(&ns, &memory);
        let devices = ns.devices().count();
        println!(
            "acpi: {} tables, {} of {} with device definitions loaded: {} devices, {} PCI root bridge(s){}",
            tables.len(),
            loaded,
            blocks,
            devices,
            roots.len(),
            if ns.skipped > 0 {
                alloc::format!(" ({} conditional definitions left out)", ns.skipped)
            } else {
                String::new()
            }
        );
        let reserved = match tables.iter().find(|t| t.is(b"DMAR")).map(|t| viommu::dmar::parse(&t.data)) {
            Some(Ok(dmar)) => dmar.reserved,
            Some(Err(e)) => {
                println!("acpi: the DMAR table: {:?}", e);
                Vec::new()
            }
            None => Vec::new(),
        };
        let device_tables = tables.into_iter().filter(|t| DEVICE_TABLES.iter().any(|d| t.is(d.signature))).collect();
        Some(Acpi { ns, memory, roots, device_tables, reserved })
    }

    /// The memory the firmware keeps for function `bus:slot.function`, as
    /// `[start, end)` ranges.
    pub fn reserved_memory(&self, bus: u8, slot: u8, function: u8) -> Vec<Range<u64>> {
        let sid = (bus as u16) << 8 | (slot as u16) << 3 | function as u16;
        self.reserved
            .iter()
            .filter(|r| r.segment == 0 && r.scopes.iter().any(|s| s.source_id() == Some(sid)))
            .map(|r| r.base..r.limit + 1)
            .collect()
    }

    /// The firmware's tables that describe the hardware of the function
    /// `info` is ([`DEVICE_TABLES`]), whole.
    pub fn device_tables(&self, info: &DeviceInfo) -> Vec<Vec<u8>> {
        let described = DEVICE_TABLES.iter().filter(|d| (d.describes)(info));
        described
            .filter_map(|d| self.device_tables.iter().find(|t| t.is(d.signature)))
            .map(|t| t.data.clone())
            .collect()
    }

    /// The ACPI device that describes PCI function `bus:slot.function`
    /// (devices on a root bus only).
    pub fn pci_companion(&self, bus: u8, slot: u8, function: u8) -> Option<Path> {
        device::pci_companion(&self.ns, &self.roots, (bus, slot, function), &self.memory)
    }

    /// The memory the firmware reserves for the motherboard (that its
    /// `PNP0C01` and `PNP0C02` devices use), which no BAR may be placed
    /// over. Logs what of it is not known.
    pub fn motherboard_memory(&self) -> Vec<Range<u64>> {
        let mut reserved = Vec::new();
        for r in device::motherboard_memory(&self.ns, &self.memory) {
            match r.memory {
                Ok(m) => reserved.extend(m),
                Err(e) => println!("acpi: {}: {}: what it reserves for the motherboard is not known", r.device, e),
            }
        }
        reserved
    }

    /// The memory windows of the PCI root bridge of bus `bus` (see
    /// [`device::pci_root_windows`]); none if no root bridge has that bus,
    /// or its `_CRS` fails (which it logs).
    pub fn pci_root_windows(&self, bus: u8) -> Vec<Range<u64>> {
        let Some((_, root)) = self.roots.iter().find(|(b, _)| *b == bus) else { return Vec::new() };
        device::pci_root_windows(&self.ns, root, &self.memory).unwrap_or_else(|e| {
            println!("acpi: {}: {}: its windows are not known", root, e);
            Vec::new()
        })
    }

    /// Where INTx `pin` (1 to 4) of the functions in `slot` of root bus
    /// `bus` goes (see [`device::pci_interrupt`]).
    pub fn pci_interrupt(&self, bus: u8, slot: u8, pin: u8) -> Option<device::IntxRoute> {
        device::pci_interrupt(&self.ns, &self.roots, (bus, slot), pin, &self.memory)
    }

    /// The devices directly below `path`, as their drivers see them.
    pub fn children(&self, path: &Path) -> Vec<Described> {
        device::describe_children(&self.ns, path, &self.memory)
    }

    /// A device's identity.
    pub fn identify(&self, path: &Path) -> Identity {
        device::identify(&self.ns, path, &self.memory)
    }

    /// A device's resources.
    pub fn resources(&self, path: &Path) -> Result<Vec<AcpiResource>, ResourcesError> {
        device::resources(&self.ns, path, &self.memory)
    }
}
