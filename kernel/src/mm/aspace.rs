//! User address spaces: a page-table hierarchy plus the list of VMO mappings
//! that it is lazily populated from on page faults.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::Error;

use super::paging::{self, Flags};
use super::vmo::{Frames, Vmo, VmoKind};
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

    /// Every permission.
    pub const ALL: Perms = Perms { read: true, write: true, exec: true };

    /// Whether `self` asks for nothing beyond `max`.
    pub fn within(self, max: Perms) -> bool {
        (!self.read || max.read) && (!self.write || max.write) && (!self.exec || max.exec)
    }

    /// No access at all (`PROT_NONE`): the pages must not be mapped.
    pub fn none(self) -> bool {
        !self.read && !self.write && !self.exec
    }
}

/// The end of `[addr, addr+len)` with `len` rounded up to whole pages (both
/// come from user space: any values).
fn range_end(addr: u64, len: u64) -> Result<u64, Error> {
    super::checked_page_align_up(len).and_then(|len| addr.checked_add(len)).ok_or(Error::InvalidArgs)
}

#[derive(Clone)]
pub struct Mapping {
    pub start: u64,
    pub len: u64,
    pub vmo: Arc<Vmo>,
    pub vmo_offset: u64,
    pub perms: Perms,
    /// The most `perms` may become: what the handle the VMO was mapped
    /// with allowed. `vm_protect` cannot go beyond it.
    pub max: Perms,
    /// The VMO was made for this mapping ([`AddressSpace::allocate`]) and
    /// no handle reaches it: each of its pages is mapped here and nowhere
    /// else, so a page is freed as soon as it is unmapped or decommitted.
    pub private: bool,
}

impl Mapping {
    fn end(&self) -> u64 {
        self.start + self.len
    }

    fn pte_flags(&self) -> Flags {
        Flags {
            writable: self.perms.write,
            executable: self.perms.exec,
            user: true,
            global: false,
            cache: self.vmo.cache(),
        }
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

    /// Finds a free, page-aligned range of `len` bytes at or above `hint`
    /// (which comes from user space: any value), inside user space.
    fn find_free(map: &BTreeMap<u64, Mapping>, len: u64, hint: u64) -> Option<u64> {
        let mut candidate = hint.clamp(USER_START, USER_END);
        // A mapping that starts below `candidate` may still overlap it.
        if let Some((_, m)) = map.range(..candidate).next_back() {
            candidate = candidate.max(m.end());
        }
        for (_, m) in map.range(candidate..) {
            if m.start >= candidate.checked_add(len)? {
                break;
            }
            // One unmapped guard page between dynamic mappings.
            candidate = m.end() + PAGE_SIZE;
        }
        candidate.checked_add(len).is_some_and(|end| end <= USER_END).then_some(candidate)
    }

    fn overlaps(map: &BTreeMap<u64, Mapping>, start: u64, end: u64) -> bool {
        map.range(..end).next_back().is_some_and(|(_, m)| m.end() > start)
    }

    /// Maps `len` bytes of `vmo` starting at `vmo_offset`. Returns the
    /// address. `addr` is a hint unless `fixed` is set. The mapping may
    /// later be given at most the permissions `max`.
    #[allow(clippy::too_many_arguments)] // mirrors the VM_MAP system call
    pub fn map(
        &self,
        vmo: Arc<Vmo>,
        vmo_offset: u64,
        len: u64,
        addr: u64,
        fixed: bool,
        perms: Perms,
        max: Perms,
        commit: bool,
    ) -> Result<u64, Error> {
        if len == 0 || !vmo_offset.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        let len = super::checked_page_align_up(len).ok_or(Error::OutOfRange)?;
        if vmo_offset.checked_add(len).is_none_or(|e| e > vmo.size()) {
            return Err(Error::OutOfRange);
        }
        self.insert(Mapping { start: 0, len, vmo, vmo_offset, perms, max, private: false }, addr, fixed, commit)
    }

    /// Maps `len` bytes of new zero-filled memory that only this mapping
    /// reaches (see [`Mapping::private`]). Returns the address; `addr` is
    /// a hint unless `fixed` is set.
    pub fn allocate(&self, len: u64, addr: u64, fixed: bool, perms: Perms, commit: bool) -> Result<u64, Error> {
        if len == 0 {
            return Err(Error::InvalidArgs);
        }
        let vmo = Vmo::new_anonymous(len).ok_or(Error::NoMemory)?;
        let len = vmo.size();
        self.insert(
            Mapping { start: 0, len, vmo, vmo_offset: 0, perms, max: Perms::ALL, private: true },
            addr,
            fixed,
            commit,
        )
    }

    /// Places `m` (whose `start` is chosen here) at `addr`, or near it
    /// unless `fixed`, and maps the pages that are mapped eagerly.
    fn insert(&self, mut m: Mapping, addr: u64, fixed: bool, commit: bool) -> Result<u64, Error> {
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Error::InvalidArgs);
        }
        if !m.perms.within(m.max) {
            return Err(Error::AccessDenied);
        }
        let mut map = self.mappings.lock();
        m.start = if fixed {
            if addr < USER_START || addr.checked_add(m.len).is_none_or(|e| e > USER_END) {
                return Err(Error::OutOfRange);
            }
            if Self::overlaps(&map, addr, addr + m.len) {
                return Err(Error::AlreadyExists);
            }
            addr
        } else {
            let hint = if addr != 0 { addr } else { DYNAMIC_BASE };
            Self::find_free(&map, m.len, hint)
                .or_else(|| Self::find_free(&map, m.len, USER_START))
                .ok_or(Error::NoMemory)?
        };
        // Physical memory is always mapped eagerly; anonymous memory on
        // request (otherwise on first touch). Inaccessible pages never are.
        let eager = (commit || !matches!(m.vmo.kind(), VmoKind::Anonymous)) && !m.perms.none();
        if eager {
            for off in (0..m.len).step_by(PAGE_SIZE as usize) {
                let mapped = m
                    .vmo
                    .page(m.vmo_offset + off, true)
                    .is_some_and(|frame| paging::map(self.pml4, m.start + off, frame, m.pte_flags(), true).is_ok());
                if !mapped {
                    // No page table entry may outlive the failed mapping.
                    for va in (m.start..m.start + off).step_by(PAGE_SIZE as usize) {
                        paging::unmap(self.pml4, va);
                    }
                    drop(map);
                    crate::arch::tlb::shootdown(m.start, off);
                    return Err(Error::NoMemory);
                }
            }
        }
        let start = m.start;
        map.insert(start, m);
        Ok(start)
    }

    /// Removes all mappings (or parts of mappings) inside `[addr, addr+len)`.
    pub fn unmap(&self, addr: u64, len: u64) -> Result<(), Error> {
        if !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
            return Err(Error::InvalidArgs);
        }
        let end = range_end(addr, len)?;
        let mut map = self.mappings.lock();
        let affected: Vec<u64> = map.range(..end).filter(|(_, m)| m.end() > addr).map(|(&s, _)| s).collect();
        if affected.is_empty() {
            return Err(Error::NotFound);
        }
        let mut removed = Vec::with_capacity(affected.len());
        let mut freed = Frames::default();
        for s in affected {
            let m = map.remove(&s).unwrap();
            let cut_start = m.start.max(addr);
            let cut_end = m.end().min(end);
            for va in (cut_start..cut_end).step_by(PAGE_SIZE as usize) {
                paging::unmap(self.pml4, va);
            }
            if m.private {
                freed.append(m.vmo.decommit(m.vmo_offset + (cut_start - m.start), cut_end - cut_start));
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
            removed.push(m);
        }
        drop(map);
        crate::arch::tlb::shootdown(addr, end - addr);
        // Memory is freed only now that no TLB can reach it: the pages cut
        // from private memory, and the VMOs whose last mapping went.
        drop(freed);
        drop(removed);
        Ok(())
    }

    /// Frees the pages of `[addr, addr+len)`, which private memory
    /// ([`allocate`](Self::allocate)) must cover completely: they read as
    /// zeros from then on. The mappings and their permissions stay.
    pub fn decommit(&self, addr: u64, len: u64) -> Result<(), Error> {
        if !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
            return Err(Error::InvalidArgs);
        }
        let end = range_end(addr, len)?;
        let map = self.mappings.lock();
        let first = Self::covering(&map, addr, end, |m| if m.private { Ok(()) } else { Err(Error::NotSupported) })?;
        let mut freed = Frames::default();
        for (_, m) in map.range(first..end) {
            let from = m.start.max(addr);
            let to = m.end().min(end);
            for va in (from..to).step_by(PAGE_SIZE as usize) {
                paging::unmap(self.pml4, va);
            }
            freed.append(m.vmo.decommit(m.vmo_offset + (from - m.start), to - from));
        }
        drop(map);
        crate::arch::tlb::shootdown(addr, end - addr);
        drop(freed);
        Ok(())
    }

    /// Checks that mappings cover `[addr, end)` without a gap (`NotFound`
    /// otherwise) and that `check` accepts each of them. Returns the start
    /// of the first.
    fn covering(
        map: &BTreeMap<u64, Mapping>,
        addr: u64,
        end: u64,
        check: impl Fn(&Mapping) -> Result<(), Error>,
    ) -> Result<u64, Error> {
        let first = map.range(..=addr).next_back().filter(|(_, m)| m.end() > addr).map_or(addr, |(&s, _)| s);
        let mut covered = addr;
        for (_, m) in map.range(first..end) {
            if m.start > covered {
                return Err(Error::NotFound);
            }
            check(m)?;
            covered = m.end();
        }
        if covered < end {
            return Err(Error::NotFound);
        }
        Ok(first)
    }

    /// Splits the mapping that straddles `at`, if any, so that a mapping
    /// boundary falls there.
    fn split_at(map: &mut BTreeMap<u64, Mapping>, at: u64) {
        let Some((&start, m)) = map.range(..at).next_back() else { return };
        if m.end() <= at {
            return;
        }
        let mut right = m.clone();
        right.vmo_offset += at - start;
        right.start = at;
        right.len = m.end() - at;
        map.get_mut(&start).unwrap().len = at - start;
        map.insert(at, right);
    }

    /// Changes the permissions of `[addr, addr+len)`, which must be mapped
    /// completely, splitting mappings at its ends as needed. No mapping may
    /// get permissions beyond those it was mapped with.
    pub fn protect(&self, addr: u64, len: u64, perms: Perms) -> Result<(), Error> {
        if !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
            return Err(Error::InvalidArgs);
        }
        let end = range_end(addr, len)?;
        let mut map = self.mappings.lock();
        // Check everything before changing anything.
        Self::covering(&map, addr, end, |m| if perms.within(m.max) { Ok(()) } else { Err(Error::AccessDenied) })?;
        Self::split_at(&mut map, addr);
        Self::split_at(&mut map, end);
        for (_, m) in map.range_mut(addr..end) {
            m.perms = perms;
            let flags = m.pte_flags();
            for va in (m.start..m.end()).step_by(PAGE_SIZE as usize) {
                if perms.none() {
                    // Faults on the page are refused from now on.
                    paging::unmap(self.pml4, va);
                } else {
                    paging::protect(self.pml4, va, flags);
                }
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
        // The hardware cannot make pages writable or executable but not
        // readable, so any access at all lets them be read.
        if addr >= m.end() || m.perms.none() || (write && !m.perms.write) || (exec && !m.perms.exec) {
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
