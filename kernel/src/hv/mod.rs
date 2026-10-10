//! The hypervisor: virtual machines for user-space monitors.
//!
//! The kernel gives a virtual machine monitor (a process holding a
//! hypervisor resource) two kinds of objects, and keeps to mechanism:
//!
//! * [`Guest`]: a guest-physical address space made of VMOs (EPT), whose
//!   pages stay committed while mapped;
//! * [`Vcpu`]: a virtual processor with a local APIC, which a thread of the
//!   monitor runs until the monitor is needed.
//!
//! What a guest's platform is (its memory map, how it boots, its devices
//! and hypercalls) is the monitor's. The kernel handles only what must be
//! fast or belongs to the processor (see [`vcpu`]). Guests run on Intel
//! VMX with EPT ([`vmx`]).

pub mod ept;
pub mod guest;
pub mod vcpu;
pub mod vmx;

pub use guest::Guest;
pub use vcpu::Vcpu;

/// Logs whether this machine can run guests, once at boot.
pub fn report() {
    match vmx::caps() {
        Ok(c) => crate::kinfo!(
            "hv: VMX with EPT{}; preemption timer at TSC/{}",
            if c.vpid { " and VPIDs" } else { "" },
            1u32 << c.preemption_rate
        ),
        Err(why) => crate::kinfo!("hv: no virtual machines ({})", why),
    }
}
