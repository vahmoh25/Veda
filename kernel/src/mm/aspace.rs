//! User address spaces: a page-table hierarchy plus the list of VMO mappings
//! that it is lazily populated from on page faults.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::Error;

use super::paging::{self, Flags};
use super::vmo::{Vmo, VmoKind};
use super::{PAGE_SIZE, page_align_down};
use crate::sync::SpinLock;

const USER_START: u64 = vabi::USER_SPACE_START as u64;
const USER_END: u64 = vabi::USER_SPACE_END as u64;
/// Where non-fixed mappings are placed by default.
const DYNAMIC_BASE: u64 = 0x0000_1000_0000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perms {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perms {
    pub fn from_map_flags(flags: usize) -> Perms {
        use vabi::map_flags::*;
        Perms { read: flags & READ != 0, write: flags & WRITE != 0, exec: flags & EXECUTE != 0 }
    }
}

#[derive(Clone)]
pub struct Mapping {
    pub start: u64,
    pub len: u64,
    pub vmo: Arc<Vmo>,
    pub vmo_offset: u64,
    pub perms: Perms,
}

impl Mapping {
    fn end(&self) -> u64 {
        self.start + self.len
    }

    fn pte_flags(&self) -> Flags {
        Flags { writable: self.perms.write, executable: self.perms.exec, user: true, global: false, cache: self.vmo.cache() }
    }
}

pub struct AddressSpace {
    pml4: u64,
    mappings: SpinLock<BTreeMap<u64, Mapping>>,
}

impl AddressSpace {
    pub fn new() -> Option<Arc<AddressSpace>> {
        Some(Arc::new(AddressSpace { pml4: paging::new_user_pml4()?, mappings: SpinLock::new(BTreeMap::new()) }))
    }

    pub fn pml4(&self) -> u64 {
        self.pml4
    }

    /// Finds a free, page-aligned range of `len` bytes at or above `hint`.
    fn find_free(map: &BTreeMap<u64, Mapping>, len: u64, hint: u64) -> Option<u64> {
        let mut candidate = hint.max(USER_START);
        // A mapping that starts below `candidate` may still overlap it.
        if let Some((_, m)) = map.range(..candidate).next_back() {
            candidate = candidate.max(m.end());
        }
        for (_, m) in map.range(candidate..) {
            if m.start >= candidate + len {
                break;
            }
            // One unmapped guard page between dynamic mappings.
            candidate = m.end() + PAGE_SIZE;
        }
        (candidate + len <= USER_END).then_some(candidate)
    }

    fn overlaps(map: &BTreeMap<u64, Mapping>, start: u64, end: u64) -> bool {
        map.range(..end).next_back().is_some_and(|(_, m)| m.end() > start)
    }

    /// Maps `len` bytes of `vmo` starting at `vmo_offset`. Returns the
    /// address. `addr` is a hint unless `fixed` is set.
    pub fn map(
        &self,
        vmo: Arc<Vmo>,
        vmo_offset: u64,
        len: u64,
        addr: u64,
        fixed: bool,
        perms: Perms,
        commit: bool,
    ) -> Result<u64, Error> {
        if len == 0 || vmo_offset % PAGE_SIZE != 0 || addr % PAGE_SIZE != 0 {
            return Err(Error::InvalidArgs);
        }
        let len = super::page_align_up(len);
        if vmo_offset.checked_add(len).is_none_or(|e| e > vmo.size()) {
            return Err(Error::OutOfRange);
        }
        let mut map = self.mappings.lock();
        let start = if fixed {
            if addr < USER_START || addr.checked_add(len).is_none_or(|e| e > USER_END) {
                return Err(Error::OutOfRange);
            }
            if Self::overlaps(&map, addr, addr + len) {
                return Err(Error::AlreadyExists);
            }
            addr
        } else {
            let hint = if addr != 0 { addr } else { DYNAMIC_BASE };
            Self::find_free(&map, len, hint)
                .or_else(|| Self::find_free(&map, len, USER_START))
                .ok_or(Error::NoMemory)?
        };
        let m = Mapping { start, len, vmo, vmo_offset, perms };
        // Physical memory is always mapped eagerly; anonymous memory on
        // request (otherwise on first touch).
        let eager = commit || !matches!(m.vmo.kind(), VmoKind::Anonymous);
        if eager {
            for off in (0..len).step_by(PAGE_SIZE as usize) {
                let frame = m.vmo.page(vmo_offset + off, true).ok_or(Error::NoMemory)?;
                paging::map(self.pml4, start + off, frame, m.pte_flags(), true).map_err(|_| Error::NoMemory)?;
            }
        }
        map.insert(start, m);
        Ok(start)
    }

    /// Removes all mappings (or parts of mappings) inside `[addr, addr+len)`.
    pub fn unmap(&self, addr: u64, len: u64) -> Result<(), Error> {
        if addr % PAGE_SIZE != 0 || len == 0 {
            return Err(Error::InvalidArgs);
        }
        let end = addr.checked_add(super::page_align_up(len)).ok_or(Error::InvalidArgs)?;
        let mut map = self.mappings.lock();
        let affected: Vec<u64> = map.range(..end).filter(|(_, m)| m.end() > addr).map(|(&s, _)| s).collect();
        if affected.is_empty() {
            return Err(Error::NotFound);
        }
        for s in affected {
            let m = map.remove(&s).unwrap();
            let cut_start = m.start.max(addr);
            let cut_end = m.end().min(end);
            for va in (cut_start..cut_end).step_by(PAGE_SIZE as usize) {
                paging::unmap(self.pml4, va);
            }
            if m.start < cut_start {
                let mut left = m.clone();
                left.len = cut_start - m.start;
                map.insert(left.start, left);
            }
            if m.end() > cut_end {
                let mut right = m.clone();
                right.vmo_offset += cut_end - m.start;
                right.start = cut_end;
                right.len = m.end() - cut_end;
                map.insert(right.start, right);
            }
        }
        drop(map);
        crate::arch::tlb::shootdown(addr, end - addr);
        Ok(())
    }

    /// Changes the permissions of whole mappings inside `[addr, addr+len)`.
    pub fn protect(&self, addr: u64, len: u64, perms: Perms) -> Result<(), Error> {
        let end = addr.checked_add(super::page_align_up(len)).ok_or(Error::InvalidArgs)?;
        let mut map = self.mappings.lock();
        let starts: Vec<u64> = map.range(addr..end).map(|(&s, _)| s).collect();
        if starts.is_empty() {
            return Err(Error::NotFound);
        }
        for s in starts {
            let m = map.get_mut(&s).unwrap();
            if m.end() > end {
                return Err(Error::InvalidArgs);
            }
            m.perms = perms;
            let flags = m.pte_flags();
            for va in (m.start..m.end()).step_by(PAGE_SIZE as usize) {
                paging::protect(self.pml4, va, flags);
            }
        }
        drop(map);
        crate::arch::tlb::shootdown(addr, end - addr);
        Ok(())
    }

    /// Resolves a user page fault. Returns `false` if the access is invalid.
    pub fn handle_fault(&self, addr: u64, write: bool, exec: bool) -> bool {
        let map = self.mappings.lock();
        let Some((_, m)) = map.range(..=addr).next_back() else { return false };
        if addr >= m.end() || (write && !m.perms.write) || (exec && !m.perms.exec) || (!m.perms.read && !exec) {
            return false;
        }
        let va = page_align_down(addr);
        let Some(frame) = m.vmo.page(m.vmo_offset + (va - m.start), true) else { return false };
        paging::map(self.pml4, va, frame, m.pte_flags(), true).is_ok()
    }

    /// Ensures every page of `[addr, addr+len)` is mapped with the required
    /// access, faulting pages in as needed.
    pub fn ensure_range(&self, addr: u64, len: u64, write: bool) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len) else { return false };
        if addr < USER_START || end > USER_END {
            return false;
        }
        let mut va = page_align_down(addr);
        while va < end {
            let ok = match paging::translate(self.pml4, va) {
                Some((_, pte)) => pte & paging::USER != 0 && (!write || pte & paging::WRITABLE != 0),
                None => false,
            };
            if !ok && !self.handle_fault(va, write, false) {
                return false;
            }
            va += PAGE_SIZE;
        }
        true
    }

    /// Bytes of memory committed by the VMOs mapped here (shared VMOs are
    /// counted in every address space that maps them).
    pub fn committed_bytes(&self) -> u64 {
        let map = self.mappings.lock();
        let mut seen: Vec<u64> = Vec::new();
        let mut total = 0;
        for m in map.values() {
            if matches!(m.vmo.kind(), VmoKind::Physical { .. }) || seen.contains(&m.vmo.koid) {
                continue;
            }
            seen.push(m.vmo.koid);
            total += m.vmo.committed_bytes();
        }
        total
    }

    /// Loads this address space on the current CPU.
    pub fn activate(&self) {
        if crate::arch::cpu::read_cr3() != self.pml4 {
            // SAFETY: the PML4 shares the kernel half, so the kernel keeps
            // running after the switch.
            unsafe { crate::arch::cpu::write_cr3(self.pml4) };
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Mappings (and their VMO references) are dropped with the map; the
        // frames belong to the VMOs. Only the page tables are ours.
        self.mappings.lock().clear();
        paging::free_user_pml4(self.pml4);
    }
}
