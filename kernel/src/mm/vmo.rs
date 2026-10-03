//! Virtual memory objects (VMOs): the unit of memory that can be mapped into
//! address spaces and shared between processes through handles.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::paging::Cache;
use super::{PAGE_SIZE, phys, phys_to_virt};
use crate::sync::SpinLock;

/// Upper bound on a single VMO (keeps page indices and arithmetic sane).
pub const MAX_VMO_SIZE: u64 = 64 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoKind {
    /// Zero-filled memory whose pages are allocated on first touch.
    Anonymous,
    /// A fixed physical range that the VMO does not own (MMIO, framebuffer,
    /// boot modules).
    Physical { base: u64 },
    /// Physically contiguous memory owned by the VMO (DMA buffers).
    Contiguous { base: u64 },
}

pub struct Vmo {
    pub koid: u64,
    size: u64,
    kind: VmoKind,
    cache: Cache,
    /// Committed pages of an anonymous VMO: page index -> frame.
    pages: SpinLock<BTreeMap<u64, u64>>,
    committed: AtomicU64,
}

impl Vmo {
    fn new(size: u64, kind: VmoKind, cache: Cache) -> Arc<Vmo> {
        Arc::new(Vmo {
            koid: crate::object::new_koid(),
            size,
            kind,
            cache,
            pages: SpinLock::new(BTreeMap::new()),
            committed: AtomicU64::new(0),
        })
    }

    pub fn new_anonymous(size: u64) -> Option<Arc<Vmo>> {
        let size = super::page_align_up(size);
        if size == 0 || size > MAX_VMO_SIZE {
            return None;
        }
        Some(Vmo::new(size, VmoKind::Anonymous, Cache::WriteBack))
    }

    pub fn new_physical(base: u64, size: u64, cache: Cache) -> Arc<Vmo> {
        Vmo::new(super::page_align_up(size), VmoKind::Physical { base: super::page_align_down(base) }, cache)
    }

    pub fn new_contiguous(size: u64) -> Option<Arc<Vmo>> {
        let size = super::page_align_up(size);
        if size == 0 || size > (256 << 20) {
            return None;
        }
        let base = phys::alloc_contiguous(size / PAGE_SIZE, PAGE_SIZE, u64::MAX)?;
        let vmo = Vmo::new(size, VmoKind::Contiguous { base }, Cache::WriteBack);
        vmo.committed.store(size, Ordering::Relaxed);
        Some(vmo)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn kind(&self) -> VmoKind {
        self.kind
    }

    pub fn cache(&self) -> Cache {
        self.cache
    }

    pub fn committed_bytes(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    /// Physical address of the page containing `offset`, committing a zeroed
    /// page for anonymous VMOs when `commit` is set. Returns `None` past the
    /// end, or for an uncommitted page when `commit` is false.
    pub fn page(&self, offset: u64, commit: bool) -> Option<u64> {
        if offset >= self.size {
            return None;
        }
        let page = offset / PAGE_SIZE;
        match self.kind {
            VmoKind::Physical { base } | VmoKind::Contiguous { base } => Some(base + page * PAGE_SIZE),
            VmoKind::Anonymous => {
                let mut pages = self.pages.lock();
                if let Some(&f) = pages.get(&page) {
                    return Some(f);
                }
                if !commit {
                    return None;
                }
                let f = phys::alloc_zeroed()?;
                pages.insert(page, f);
                self.committed.fetch_add(PAGE_SIZE, Ordering::Relaxed);
                Some(f)
            }
        }
    }

    /// Physical address for DMA (contiguous and physical VMOs only).
    pub fn phys_addr(&self, offset: u64) -> Option<u64> {
        match self.kind {
            VmoKind::Physical { base } | VmoKind::Contiguous { base } if offset < self.size => Some(base + offset),
            _ => None,
        }
    }

    /// Copies VMO contents into `out`; uncommitted pages read as zero.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> bool {
        if offset.checked_add(out.len() as u64).is_none_or(|end| end > self.size) {
            return false;
        }
        let mut done = 0usize;
        while done < out.len() {
            let pos = offset + done as u64;
            let in_page = (pos % PAGE_SIZE) as usize;
            let n = (PAGE_SIZE as usize - in_page).min(out.len() - done);
            match self.page(pos, false) {
                // SAFETY: the frame is direct-mapped and `n` stays in the page.
                Some(f) => unsafe {
                    core::ptr::copy_nonoverlapping(
                        (phys_to_virt(f) + in_page as u64) as *const u8,
                        out.as_mut_ptr().add(done),
                        n,
                    )
                },
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
        true
    }

    /// Copies `data` into the VMO, committing pages as needed.
    pub fn write(&self, offset: u64, data: &[u8]) -> bool {
        if offset.checked_add(data.len() as u64).is_none_or(|end| end > self.size) {
            return false;
        }
        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u64;
            let in_page = (pos % PAGE_SIZE) as usize;
            let n = (PAGE_SIZE as usize - in_page).min(data.len() - done);
            let Some(f) = self.page(pos, true) else { return false };
            // SAFETY: as in `read`.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr().add(done), (phys_to_virt(f) + in_page as u64) as *mut u8, n)
            };
            done += n;
        }
        true
    }
}

impl Drop for Vmo {
    fn drop(&mut self) {
        match self.kind {
            VmoKind::Anonymous => {
                for (_, f) in core::mem::take(&mut *self.pages.lock()) {
                    phys::free(f);
                }
            }
            VmoKind::Contiguous { base } => phys::free_contiguous(base, self.size / PAGE_SIZE),
            VmoKind::Physical { .. } => {}
        }
    }
}
