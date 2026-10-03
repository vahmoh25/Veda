//! Hardware resources: capabilities that authorise access to I/O ports,
//! interrupt lines, physical memory, DMA and power control.
//!
//! The root resource (given only to `init`) authorises everything. Narrower
//! resources are derived from it with `resource_create` and handed to
//! drivers, so each driver can only touch its own device.

use alloc::sync::Arc;

use vabi::resource_kind;

pub struct Resource {
    pub koid: u64,
    pub kind: usize,
    pub base: u64,
    pub size: u64,
}

impl Resource {
    pub fn new(kind: usize, base: u64, size: u64) -> Arc<Resource> {
        Arc::new(Resource { koid: super::new_koid(), kind, base, size })
    }

    pub fn root() -> Arc<Resource> {
        Resource::new(resource_kind::ROOT, 0, u64::MAX)
    }

    /// Does this resource authorise `kind` access to `[base, base+size)`?
    pub fn permits(&self, kind: usize, base: u64, size: u64) -> bool {
        if self.kind == resource_kind::ROOT {
            return true;
        }
        let Some(end) = base.checked_add(size) else { return false };
        self.kind == kind && base >= self.base && end <= self.base.saturating_add(self.size)
    }
}
