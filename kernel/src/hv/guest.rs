//! Guests: virtual machines, each a guest-physical address space and its
//! virtual processors, and the devices given to it.
//!
//! A guest's memory is made of VMOs mapped at guest-physical addresses
//! (`guest_map`). Every page of a mapping is committed when it is mapped
//! and stays the VMO's, which the mapping holds: devices given to the
//! guest may reach any of them at any time, so none can come and go
//! behind the guest's back. Unmapping makes every processor and device
//! forget the pages before the mapping (and so perhaps the VMO) is dropped.
//!
//! Devices given to the guest (`guest_attach_device`) are in an IOMMU
//! domain of the guest's: its page tables mirror the guest's memory, so a
//! device reaches exactly what the guest's processors reach, at the same
//! addresses. The domain is made when the first device comes.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use vabi::Error;

use super::ept::{self, Access, Ept};
use super::vcpu::Vcpu;
use crate::iommu::Domain;
use crate::mm::PAGE_SIZE;
use crate::mm::vmo::Vmo;
use crate::sync::SpinLock;

/// A guest may have at most this many processors.
pub const MAX_VCPUS: u32 = 64;

struct Mapping {
    len: u64,
    offset: u64,
    access: Access,
    vmo: Arc<Vmo>,
}

impl Mapping {
    /// Mirrors the mapping, at `gpa`, into the devices' domain.
    fn mirror(&self, domain: &mut Domain, gpa: u64) -> Result<(), Error> {
        for off in (0..self.len).step_by(PAGE_SIZE as usize) {
            let frame = self.vmo.page(self.offset + off, false).ok_or(Error::Internal)?;
            domain.map(gpa + off, frame, self.access.read, self.access.write)?;
        }
        Ok(())
    }
}

pub struct Guest {
    pub koid: u64,
    /// How many processors the guest has (what `cpuid` tells it).
    pub cpus: u32,
    ept: SpinLock<Ept>,
    /// Mappings by guest-physical address.
    mappings: SpinLock<BTreeMap<u64, Mapping>>,
    /// The domain of the devices given to the guest, once there is one.
    domain: SpinLock<Option<Domain>>,
    vcpus: SpinLock<Vec<Weak<Vcpu>>>,
}

impl Guest {
    pub fn new(cpus: u32) -> Result<Arc<Guest>, Error> {
        if cpus == 0 || cpus > MAX_VCPUS {
            return Err(Error::InvalidArgs);
        }
        Ok(Arc::new(Guest {
            koid: crate::object::new_koid(),
            cpus,
            ept: SpinLock::new(Ept::new().ok_or(Error::NoMemory)?),
            mappings: SpinLock::new(BTreeMap::new()),
            domain: SpinLock::new(None),
            vcpus: SpinLock::new(Vec::new()),
        }))
    }

    /// The EPT pointer of the guest's address space.
    pub fn ept_pointer(&self) -> u64 {
        self.ept.lock().pointer()
    }

    /// Maps `len` bytes of `vmo` from `offset` at `gpa`.
    pub fn map(&self, vmo: Arc<Vmo>, offset: u64, len: u64, gpa: u64, access: Access) -> Result<(), Error> {
        if len == 0 || !offset.is_multiple_of(PAGE_SIZE) || !gpa.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        let len = crate::mm::checked_page_align_up(len).ok_or(Error::OutOfRange)?;
        if offset.checked_add(len).is_none_or(|e| e > vmo.size()) || gpa.checked_add(len).is_none_or(|e| e > ept::LIMIT)
        {
            return Err(Error::OutOfRange);
        }
        let mut mappings = self.mappings.lock();
        let overlaps = mappings.range(..gpa + len).next_back().is_some_and(|(&start, m)| start + m.len > gpa);
        if overlaps {
            return Err(Error::AlreadyExists);
        }
        let mut ept = self.ept.lock();
        let mut domain = self.domain.lock();
        let mut done = 0;
        let mut mapped = Ok(());
        while done < len && mapped.is_ok() {
            mapped = vmo
                .page(offset + done, true)
                .ok_or(Error::NoMemory)
                .and_then(|frame| ept.map(gpa + done, frame, access, vmo.cache()).map_err(|_| Error::NoMemory));
            if mapped.is_ok() {
                done += PAGE_SIZE;
            }
        }
        let mapping = Mapping { len, offset, access, vmo };
        if mapped.is_ok()
            && let Some(d) = domain.as_mut()
        {
            mapped = mapping.mirror(d, gpa);
        }
        if let Err(e) = mapped {
            // A processor (or device) may have reached the pages already:
            // they are forgotten everywhere before the VMO can go.
            for off in (0..done).step_by(PAGE_SIZE as usize) {
                ept.unmap(gpa + off);
            }
            if let Some(d) = domain.as_mut() {
                for off in (0..len).step_by(PAGE_SIZE as usize) {
                    d.unmap(gpa + off);
                }
                d.commit(true);
            }
            drop((ept, domain, mappings));
            crate::arch::tlb::shootdown_guest_memory();
            return Err(e);
        }
        if let Some(d) = domain.as_ref() {
            d.commit(false);
        }
        mappings.insert(gpa, mapping);
        Ok(())
    }

    /// Removes the mappings inside `[gpa, gpa+len)`, which must be whole
    /// mappings.
    pub fn unmap(&self, gpa: u64, len: u64) -> Result<(), Error> {
        let end = gpa.checked_add(len).ok_or(Error::InvalidArgs)?;
        let mut removed = Vec::new();
        {
            let mut mappings = self.mappings.lock();
            let starts: Vec<u64> = mappings.range(gpa..end).map(|(&s, _)| s).collect();
            if starts.is_empty() {
                return Err(Error::NotFound);
            }
            if starts.iter().any(|s| s + mappings[s].len > end) {
                return Err(Error::InvalidArgs);
            }
            let mut ept = self.ept.lock();
            let mut domain = self.domain.lock();
            for s in starts {
                let m = mappings.remove(&s).unwrap();
                for off in (0..m.len).step_by(PAGE_SIZE as usize) {
                    ept.unmap(s + off);
                    if let Some(d) = domain.as_mut() {
                        d.unmap(s + off);
                    }
                }
                removed.push(m);
            }
            // No device may still reach the pages when they are released.
            if let Some(d) = domain.as_ref() {
                d.commit(true);
            }
        }
        // Nor any processor.
        crate::arch::tlb::shootdown_guest_memory();
        drop(removed);
        Ok(())
    }

    /// Gives the guest PCI function `device` (a requester id): its DMA
    /// reaches the guest's memory from now on, and only that.
    pub fn attach_device(&self, device: u16) -> Result<(), Error> {
        let mappings = self.mappings.lock();
        let mut domain = self.domain.lock();
        if domain.is_none() {
            let mut d = Domain::new()?;
            for (&gpa, m) in mappings.iter() {
                m.mirror(&mut d, gpa)?;
            }
            *domain = Some(d);
        }
        let d = domain.as_mut().ok_or(Error::Internal)?;
        d.attach(device)?;
        d.commit(false);
        Ok(())
    }

    /// Registers a new virtual processor (`None`: the id is taken, or the
    /// guest has all its processors).
    pub fn add_vcpu(&self, vcpu: &Arc<Vcpu>) -> bool {
        let mut vcpus = self.vcpus.lock();
        vcpus.retain(|w| w.strong_count() > 0);
        if vcpus.len() as u32 >= self.cpus || vcpus.iter().filter_map(|w| w.upgrade()).any(|v| v.id == vcpu.id) {
            return false;
        }
        vcpus.push(Arc::downgrade(vcpu));
        true
    }

    /// The guest's virtual processors.
    pub fn vcpus(&self) -> Vec<Arc<Vcpu>> {
        self.vcpus.lock().iter().filter_map(|w| w.upgrade()).collect()
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        // The devices go first: they reach nothing from now on. Then the
        // tables go with the guest: no processor may keep translations of
        // them (or of the pages, which the mappings release now).
        drop(self.domain.lock().take());
        crate::arch::tlb::shootdown_guest_memory();
    }
}
