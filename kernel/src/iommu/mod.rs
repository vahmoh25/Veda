//! The IOMMU (Intel VT-d): what devices reach of memory, and which
//! interrupts they may raise.
//!
//! At boot, before any device has an interrupt, every remapping unit the
//! firmware's DMAR table describes is set up the same way:
//!
//! * Interrupt remapping. One table, which the units share, has an entry
//!   per interrupt vector of the boot processor: vector V is raised through
//!   entry V. I/O APIC inputs and MSIs are programmed in the remappable
//!   format, and the compatibility format is blocked. The entry of a
//!   device's MSI names the device, and the units refuse it to any other:
//!   a device raises only its own interrupts.
//! * DMA remapping. Every device starts in the host's domain, which passes
//!   its requests through untranslated, so Veda's own drivers reach memory
//!   as they would without an IOMMU. A device given to a guest
//!   ([`Domain::attach`]) is put in the guest's domain, whose page tables
//!   mirror the guest's memory; when the domain goes, the device reaches
//!   nothing at all.
//!
//! The kernel uses the IOMMU only if every unit can do all of that (queued
//! invalidation, interrupt remapping, pass-through) and the table names
//! every I/O APIC. Otherwise it leaves the units alone: devices work as
//! they would without one, and none can be given to a guest.

mod domain;
mod unit;

use alloc::vec::Vec;

use viommu::dmar::{self, Dmar};
use viommu::vtd::{self, Fault, Irte};

pub use domain::Domain;
use unit::Unit;

use crate::arch::{apic, idt, percpu};
use crate::mm::{phys, phys_to_virt};
use crate::sync::{Once, SpinLock};

/// The remapping table's size field: 2^(7+1) entries, one per vector.
const TABLE_SIZE: u32 = 7;

struct Iommu {
    units: Vec<Unit>,
    dmar: Dmar,
    /// The interrupt remapping table (a frame), entry V for vector V.
    table: u64,
    /// Its entries' destinations are x2APIC ids (extended interrupt mode).
    x2apic: bool,
    /// The I/O APICs' requester ids: (I/O APIC id, requester id).
    ioapics: Vec<(u8, u16)>,
    /// Whether every unit's walks see the processors' caches; whether one
    /// caches absent entries (then adding one needs invalidating too).
    coherent: bool,
    caching_mode: bool,
    /// The depth of guests' page tables, and the address width their
    /// context entries give for it.
    levels: u32,
    address_width: u64,
    /// The domain ids every unit has.
    domain_ids: SpinLock<DomainIds>,
    /// Faults reported so far (only the first ones are logged).
    faults: SpinLock<u32>,
}

/// Faults past this many go unlogged, but for every 1024th.
const FAULTS_LOGGED: u32 = 32;

/// Domain ids for guests' domains: from 2 up (0 is reserved in caching
/// mode, 1 is the host's), below what every unit has.
struct DomainIds {
    next: u32,
    limit: u32,
    free: Vec<u16>,
}

impl DomainIds {
    fn take(&mut self) -> Option<u16> {
        self.free.pop().or_else(|| {
            (self.next < self.limit).then(|| {
                self.next += 1;
                (self.next - 1) as u16
            })
        })
    }
}

static IOMMU: Once<Iommu> = Once::new();

fn iommu() -> Option<&'static Iommu> {
    IOMMU.get()
}

/// Writes back what the processor wrote to `[phys, phys + len)` if a unit
/// reads memory around the caches.
fn publish(iommu: &Iommu, phys: u64, len: u64) {
    if !iommu.coherent {
        crate::arch::cpu::flush_cache_range(phys_to_virt(phys), len);
    }
}

/// Sets up the IOMMU, if the firmware describes one (after the ACPI tables
/// and the I/O APICs, before any interrupt of a device is routed).
pub fn init() {
    let Some(acpi) = crate::acpi::ACPI.get() else { return };
    let Some(table) = acpi.dmar else { return };
    let dmar = match dmar::parse(crate::acpi::table_bytes(table)) {
        Ok(d) => d,
        Err(e) => {
            crate::kwarn!("iommu: the DMAR table: {}", e);
            return;
        }
    };
    match setup(dmar, acpi) {
        Ok(iommu) => {
            crate::kinfo!(
                "iommu: {} VT-d unit(s): interrupts remapped ({} destinations), devices passed through until given \
                 to a guest ({}-level tables{}{})",
                iommu.units.len(),
                if iommu.x2apic { "x2APIC" } else { "xAPIC" },
                iommu.levels,
                if iommu.coherent { "" } else { ", flushed for the units" },
                if iommu.caching_mode { ", caching mode" } else { "" }
            );
            IOMMU.set(iommu);
        }
        Err(why) => crate::kinfo!("iommu: not used: {}", why),
    }
}

fn setup(dmar: Dmar, acpi: &crate::acpi::AcpiInfo) -> Result<Iommu, &'static str> {
    if dmar.units.is_empty() {
        return Err("the DMAR table describes no unit");
    }
    if dmar.units.iter().any(|u| u.segment != 0) {
        return Err("a unit is on a PCI segment other than 0");
    }
    // An I/O APIC's interrupts are remapped as its, so it must be named.
    let mut ioapics = Vec::new();
    for io in &acpi.ioapics {
        let sid = dmar.ioapic_source(io.id).ok_or("the DMAR table does not name every I/O APIC")?;
        ioapics.push((io.id, sid));
    }
    let mut units = Vec::new();
    for u in &dmar.units {
        units.push(Unit::new(u.base)?);
    }
    let x2apic = apic::is_x2apic() && units.iter().all(|u| u.ecap.extended_interrupt_mode());
    if !x2apic && percpu::get(0).apic_id > 0xFF {
        return Err("the boot processor's APIC id needs extended interrupt mode, which a unit lacks");
    }
    let sagaw = units.iter().fold(0x1F, |common, u| common & u.cap.sagaw());
    let (levels, address_width) = vtd::levels(sagaw).ok_or("the units walk no page-table depth in common")?;
    let limit = units.iter().map(|u| u.cap.domains()).min().unwrap_or(0);
    let coherent = units.iter().all(|u| u.ecap.coherent());
    let caching_mode = units.iter().any(|u| u.cap.caching_mode());
    let table = phys::alloc_zeroed().ok_or("no memory for the interrupt remapping table")?;
    let iommu = Iommu {
        units,
        dmar,
        table,
        x2apic,
        ioapics,
        coherent,
        caching_mode,
        levels,
        address_width,
        domain_ids: SpinLock::new(DomainIds { next: 2, limit, free: Vec::new() }),
        faults: SpinLock::new(0),
    };
    publish(&iommu, table, 4096);
    let fault_destination = percpu::get(0).apic_id;
    for (i, unit) in iommu.units.iter().enumerate() {
        if let Err(e) = unit.enable(table, TABLE_SIZE, x2apic, idt::IOMMU_VECTOR, fault_destination) {
            for u in &iommu.units[..=i] {
                u.disable();
            }
            phys::free(table);
            return Err(e);
        }
    }
    Ok(iommu)
}

/// Points remapping entry `vector` at `vector` of the boot processor, for
/// requests of `source` only if it is given.
fn route(iommu: &Iommu, vector: u8, level: bool, source: Option<u16>) {
    let destination = percpu::get(0).apic_id;
    let entry = Irte { vector, destination, level, source }.encode(iommu.x2apic);
    write_entry(iommu, vector, Some(entry));
}

/// Writes remapping entry `index` (`None`: not present), so that no unit
/// ever reads half an entry.
fn write_entry(iommu: &Iommu, index: u8, entry: Option<[u64; 2]>) {
    let at = iommu.table + index as u64 * 16;
    // SAFETY: the table is ours, direct-mapped; entry `index` is in it.
    let e = unsafe { &mut *(phys_to_virt(at) as *mut [u64; 2]) };
    let present = |e: &[u64; 2]| e[0] & 1 != 0;
    // SAFETY (both): volatile, since the units read the entry.
    if present(e) {
        unsafe { core::ptr::write_volatile(&mut e[0], 0) };
        publish(iommu, at, 16);
        invalidate_entry(iommu, index);
    }
    if let Some([low, high]) = entry {
        unsafe {
            core::ptr::write_volatile(&mut e[1], high);
            core::ptr::write_volatile(&mut e[0], low);
        }
        publish(iommu, at, 16);
        invalidate_entry(iommu, index);
    }
}

fn invalidate_entry(iommu: &Iommu, index: u8) {
    for u in &iommu.units {
        if let Err(e) = u.invalidate(&[vtd::desc::interrupt_entry(index as u16)]) {
            crate::kwarn!("iommu: unit at {:#x}: {}", u.base, e);
        }
    }
}

/// The message an MSI of device `source` (its requester id) sends to raise
/// `vector` on the boot processor: through the vector's remapping entry,
/// which only that device may raise. `None` if interrupts are not
/// remapped.
pub fn msi_message(vector: u8, source: u16) -> Option<(u64, u32)> {
    let iommu = iommu()?;
    route(iommu, vector, false, Some(source));
    Some(vtd::msi_message(vector as u16))
}

/// The redirection entry by which I/O APIC `ioapic` raises `vector` on the
/// boot processor, through the vector's remapping entry. `None` if
/// interrupts are not remapped.
pub fn ioapic_entry(ioapic: u8, vector: u8, level: bool, active_low: bool, masked: bool) -> Option<u64> {
    let iommu = iommu()?;
    let source = iommu.ioapics.iter().find(|(id, _)| *id == ioapic).map(|&(_, sid)| sid);
    route(iommu, vector, level, source);
    Some(vtd::ioapic_entry(vector as u16, vector, level, active_low, masked))
}

/// No device raises `vector` any more (its remapping entry is cleared).
pub fn release(vector: u8) {
    if let Some(iommu) = iommu() {
        write_entry(iommu, vector, None);
    }
}

/// The fault event interrupt: logs what the units refused.
pub fn fault_interrupt() {
    let Some(iommu) = iommu() else { return };
    for u in &iommu.units {
        u.take_faults(|f| log_fault(iommu, u, f));
    }
}

fn log_fault(iommu: &Iommu, unit: &Unit, f: Fault) {
    let n = {
        let mut count = iommu.faults.lock();
        *count = count.wrapping_add(1);
        *count
    };
    if n > FAULTS_LOGGED && !n.is_multiple_of(1024) {
        return;
    }
    let (bus, dev, func) = (f.source >> 8, (f.source >> 3) & 0x1F, f.source & 7);
    let what = if f.is_interrupt() {
        alloc::format!("an interrupt through entry {:#x}", f.address)
    } else {
        alloc::format!("a {} at {:#x}", if f.read { "read" } else { "write" }, f.address)
    };
    crate::kwarn!(
        "iommu: unit at {:#x} refused {} from {:02x}:{:02x}.{}: {} (fault {:#x}){}",
        unit.base,
        what,
        bus,
        dev,
        func,
        vtd::fault_reason(f.reason),
        f.reason,
        if n == FAULTS_LOGGED { "; the next faults go unlogged" } else { "" }
    );
}
