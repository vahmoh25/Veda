//! The firmware's description of the machine (ACPI): which devices it
//! places below which PCI functions, what they are and which resources
//! they use, for their drivers. A driver only learns about the devices
//! below its own PCI function.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use vabi::{KernelBootInfo, cache_policy, map_flags};
use vacpi::Memory;
use vacpi::aml::Namespace;
use vacpi::device::{self, Described, Identity, ResourcesError};
use vacpi::name::Path;
use vacpi::resource::Resource as AcpiResource;
use vacpi::tables;
use vrt::object::{Resource, Vmo};
use vrt::println;

const PAGE: u64 = 4096;

/// Firmware memory, mapped as it is first read: the ACPI tables and the
/// firmware's variables. The tables, and what the kernel reported as the
/// firmware's ACPI memory, are RAM and are mapped cached; anything else
/// AML reads (device registers) uncached.
pub struct FirmwareMemory {
    mmio: Resource,
    ram: Vec<(u64, u64)>,
    windows: RefCell<Vec<Window>>,
}

struct Window {
    start: u64,
    end: u64,
    cached: bool,
    address: usize,
    _vmo: Vmo,
}

impl FirmwareMemory {
    fn new(mmio: Resource, boot: &KernelBootInfo) -> FirmwareMemory {
        let n = (boot.acpi_memory_count as usize).min(boot.acpi_memory.len());
        let ram = boot.acpi_memory[..n].iter().map(|r| (r[0], r[1])).collect();
        FirmwareMemory { mmio, ram, windows: RefCell::new(Vec::new()) }
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
}

/// The tables themselves, which are always in RAM.
struct Tables<'a>(&'a FirmwareMemory);

impl Memory for Tables<'_> {
    fn read(&self, address: u64, buf: &mut [u8]) -> bool {
        self.0.read_as(address, buf, true)
    }
}

pub struct Acpi {
    pub ns: Namespace,
    pub memory: FirmwareMemory,
    /// PCI root bridges: their bus number and path.
    roots: Vec<(u8, Path)>,
}

impl Acpi {
    /// Reads and loads the firmware's tables. Logs what it found.
    pub fn load(mmio: &Resource, boot: &KernelBootInfo) -> Option<Acpi> {
        if boot.acpi_rsdp == 0 {
            println!("acpi: the firmware has no ACPI tables");
            return None;
        }
        let memory = FirmwareMemory::new(mmio.duplicate().ok()?, boot);
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
        Some(Acpi { ns, memory, roots })
    }

    /// The ACPI device that describes PCI function `bus:slot.function`
    /// (devices on a root bus only).
    pub fn pci_companion(&self, bus: u8, slot: u8, function: u8) -> Option<Path> {
        device::pci_companion(&self.ns, &self.roots, (bus, slot, function), &self.memory)
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
