//! `viommu` — what Veda's kernel knows of Intel VT-d that touches no
//! hardware.
//!
//! * [`dmar`]: the firmware's DMAR table: the remapping units, the devices
//!   each translates, the memory the firmware's devices use.
//! * [`vtd`]: a unit's registers and capabilities, and the structures it
//!   reads from memory: context entries, second-level page tables,
//!   interrupt remapping entries and the invalidation queue's descriptors.
//!
//! The kernel drives the units with them (`kernel/src/iommu.rs`); the
//! host's unit tests check the encodings against the specification.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod dmar;
pub mod vtd;
