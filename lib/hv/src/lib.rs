//! `vhv` — what Veda's hypervisor knows that touches no hardware.
//!
//! * [`lapic`]: the local APIC of a guest's virtual processor, in x2APIC
//!   mode: registers, interrupt priorities, the timer, inter-processor
//!   interrupts.
//! * [`cpuid`]: what `cpuid` tells a guest: the processor's own features,
//!   less those a guest cannot use, and the platform's leaves.
//! * [`platform`]: the paravirtual platform guests run on: how they find
//!   it, and its hypercalls.
//! * [`linux`]: booting a Linux kernel on it (the x86 boot protocol).
//! * [`bridge`]: Veda's IPC for the guest's programs.
//! * [`pci`]: the configuration space of a PCI function given to a guest,
//!   and the stand-in for the PC's host bridge.
//! * [`igd`]: what an Intel integrated GPU's driver reads of the PC's
//!   firmware: its OpRegion, as the guest has it.
//! * [`acpi`]: the ACPI tables that describe the guest's machine.
//!
//! The kernel drives them under its virtual processors (`kernel/src/hv`),
//! and the virtual machine monitor and the Linux guest's side use the
//! platform's definitions; the host's unit tests run them all.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod acpi;
pub mod bridge;
pub mod cpuid;
pub mod igd;
pub mod lapic;
pub mod linux;
pub mod pci;
pub mod platform;
