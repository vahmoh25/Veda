//! A guest's domain: the second-level page tables that translate its
//! devices' requests (the guest's memory, as its processors see it), and
//! the devices in it.

use alloc::vec::Vec;

use vabi::Error;
use viommu::vtd::{self, SL_ADDRESS, SL_READ, SL_WRITE, Translation, desc};

use super::{Iommu, iommu, publish};
use crate::mm::{phys, phys_to_virt};

pub struct Domain {
    id: u16,
    /// The top table.
    root: u64,
    /// The devices in it: (their unit, requester id).
    devices: Vec<(usize, u16)>,
}

fn table(frame: u64) -> &'static mut [u64; 512] {
    // SAFETY: the domain's tables are frames it owns, direct-mapped.
    unsafe { &mut *(phys_to_virt(frame) as *mut [u64; 512]) }
}

fn index(address: u64, level: u32) -> usize {
    ((address >> (12 + 9 * level)) & 0x1FF) as usize
}

fn present(e: u64) -> bool {
    e & (SL_READ | SL_WRITE) != 0
}

impl Domain {
    /// An empty domain (`NotSupported` without an IOMMU).
    pub fn new() -> Result<Domain, Error> {
        let iommu = iommu().ok_or(Error::NotSupported)?;
        let id = iommu.domain_ids.lock().take().ok_or(Error::LimitReached)?;
        let Some(root) = phys::alloc_zeroed() else {
            iommu.domain_ids.lock().free.push(id);
            return Err(Error::NoMemory);
        };
        publish(iommu, root, 4096);
        Ok(Domain { id, root, devices: Vec::new() })
    }

    /// Requests reach addresses below this.
    pub fn limit() -> u64 {
        iommu().map_or(0, |i| 1 << (12 + 9 * i.levels))
    }

    fn iommu(&self) -> &'static Iommu {
        // A domain exists only with the IOMMU.
        iommu().expect("a domain without an IOMMU")
    }

    /// Maps the page at `address` to `frame`. The devices see it after
    /// [`Domain::commit`].
    pub fn map(&mut self, address: u64, frame: u64, read: bool, write: bool) -> Result<(), Error> {
        let iommu = self.iommu();
        if address >= Self::limit() {
            return Err(Error::OutOfRange);
        }
        let mut t = self.root;
        for level in (1..iommu.levels).rev() {
            let at = t + index(address, level) as u64 * 8;
            let e = &mut table(t)[index(address, level)];
            if !present(*e) {
                let next = phys::alloc_zeroed().ok_or(Error::NoMemory)?;
                publish(iommu, next, 4096);
                *e = next | SL_READ | SL_WRITE;
                publish(iommu, at, 8);
            }
            t = *e & SL_ADDRESS;
        }
        let access = if read { SL_READ } else { 0 } | if write { SL_WRITE } else { 0 };
        table(t)[index(address, 0)] = (frame & SL_ADDRESS) | access;
        publish(iommu, t + index(address, 0) as u64 * 8, 8);
        Ok(())
    }

    /// The frame the page at `address` is mapped to, if it is.
    fn translate(&self, address: u64) -> Option<u64> {
        let iommu = self.iommu();
        let mut t = self.root;
        for level in (1..iommu.levels).rev() {
            let e = table(t)[index(address, level)];
            if !present(e) {
                return None;
            }
            t = e & SL_ADDRESS;
        }
        let e = table(t)[index(address, 0)];
        present(e).then_some(e & SL_ADDRESS)
    }

    /// Removes the mapping of the page at `address`, if any. The page may
    /// be reused only after [`Domain::commit`].
    pub fn unmap(&mut self, address: u64) {
        let iommu = self.iommu();
        let mut t = self.root;
        for level in (1..iommu.levels).rev() {
            let e = table(t)[index(address, level)];
            if !present(e) {
                return;
            }
            t = e & SL_ADDRESS;
        }
        table(t)[index(address, 0)] = 0;
        publish(iommu, t + index(address, 0) as u64 * 8, 8);
    }

    /// Makes the devices see the domain as it is now: the units forget
    /// what they cached of it. Needed after unmapping (before the pages are
    /// reused), and after mapping on units that cache absent entries.
    pub fn commit(&self, unmapped: bool) {
        let iommu = self.iommu();
        if !unmapped && !iommu.caching_mode {
            return;
        }
        let mut units: Vec<usize> = self.devices.iter().map(|&(u, _)| u).collect();
        units.dedup();
        for u in units {
            let unit = &iommu.units[u];
            if let Err(e) = unit.invalidate(&[desc::iotlb_domain(self.id)]) {
                crate::kwarn!("iommu: unit at {:#x}: {}", unit.base, e);
            }
        }
    }

    /// Puts device `sid` (a requester id on segment 0) in the domain. It
    /// must be in the host's domain, or in none (left by a guest that
    /// ended); not in another guest's. A device the firmware keeps memory
    /// for (an RMRR: the firmware may go on using it with the device, as a
    /// GPU its stolen memory and the firmware's framebuffer in it) comes
    /// only if the domain maps that memory where it is: else it stays the
    /// host's.
    pub fn attach(&mut self, sid: u16) -> Result<(), Error> {
        let iommu = self.iommu();
        let (bus, dev, func) = (sid >> 8, (sid >> 3) & 0x1F, sid & 7);
        let reserved = iommu.dmar.reserved.iter().filter(|r| r.segment == 0);
        for r in reserved.filter(|r| r.scopes.iter().any(|s| s.source_id() == Some(sid))) {
            if (r.base..=r.limit).step_by(4096).any(|page| self.translate(page) != Some(page)) {
                crate::kwarn!(
                    "iommu: {:02x}:{:02x}.{} has memory the firmware keeps for it ({:#x}-{:#x}), which the domain \
                     does not have where it is; it stays the host's",
                    bus,
                    dev,
                    func,
                    r.base,
                    r.limit
                );
                return Err(Error::NotSupported);
            }
        }
        let u = iommu.dmar.unit_for(0, sid).ok_or(Error::NotSupported)?;
        let unit = &iommu.units[u];
        if let Some(Translation::Tables(_)) = vtd::context_state(unit.context(sid)) {
            return Err(Error::Busy);
        }
        let entry = vtd::context_entry(Translation::Tables(self.root), self.id, iommu.address_width);
        unit.set_context(sid, Some(entry)).map_err(|e| {
            crate::kwarn!("iommu: unit at {:#x}: {}", unit.base, e);
            Error::Internal
        })?;
        self.devices.push((u, sid));
        self.devices.sort_unstable();
        super::joined_guest(iommu, sid);
        crate::kinfo!("iommu: {:02x}:{:02x}.{} is in domain {} now", bus, dev, func, self.id);
        Ok(())
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        let iommu = self.iommu();
        // The devices reach nothing from now on, and the units forget the
        // domain's translations: then its tables can go. (If a unit could
        // not be told, they stay, lest a device walk freed memory.)
        let mut forgotten = true;
        for &(u, sid) in &self.devices {
            let unit = &iommu.units[u];
            let (bus, dev, func) = (sid >> 8, (sid >> 3) & 0x1F, sid & 7);
            match unit.set_context(sid, None) {
                Ok(()) => {
                    super::left_guest(iommu, sid);
                    crate::kinfo!("iommu: {:02x}:{:02x}.{} left domain {}: it reaches nothing", bus, dev, func, self.id)
                }
                Err(e) => {
                    crate::kwarn!("iommu: unit at {:#x}: {}; domain {} is kept", unit.base, e, self.id);
                    forgotten = false;
                }
            }
        }
        if !forgotten {
            return;
        }
        let mut tables: Vec<(u64, u32)> = alloc::vec![(self.root, iommu.levels - 1)];
        while let Some((t, level)) = tables.pop() {
            if level > 0 {
                tables.extend(table(t).iter().filter(|&&e| present(e)).map(|&e| (e & SL_ADDRESS, level - 1)));
            }
            phys::free(t);
        }
        iommu.domain_ids.lock().free.push(self.id);
    }
}
