//! Interrupt objects deliver hardware interrupts to user-space drivers.
//!
//! When the interrupt fires the kernel sets `SIGNALED` on the object (and,
//! for level-triggered lines, masks the line so it cannot storm). The driver
//! waits for `SIGNALED`, services its device and calls `irq_ack`, which
//! clears the signal and unmasks the line.
//!
//! An MSI may instead be bound to a virtual processor (the device is a
//! guest's): then it raises a vector of the processor's local APIC, and
//! the guest's driver handles it as on a machine of its own.

use alloc::sync::{Arc, Weak};

use vabi::signals::SIGNALED;

use super::Signals;
use crate::arch::{apic, idt};
use crate::hv::vcpu::Vcpu;
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// An I/O APIC input.
    Gsi { gsi: u32, level: bool },
    /// A message-signalled interrupt (of a PCI function).
    Msi,
}

pub struct Interrupt {
    pub koid: u64,
    pub vector: u8,
    pub source: Source,
    pub signals: Signals,
    /// The virtual processor and vector it is raised at instead, if bound.
    target: SpinLock<Option<(Weak<Vcpu>, u8)>>,
}

static VECTORS: SpinLock<[Option<Weak<Interrupt>>; 256]> = SpinLock::new([const { None }; 256]);

impl Interrupt {
    fn new(vector: u8, source: Source) -> Arc<Interrupt> {
        Arc::new(Interrupt {
            koid: super::new_koid(),
            vector,
            source,
            signals: Signals::new(0),
            target: SpinLock::new(None),
        })
    }

    /// Binds an I/O APIC input. Fails if the line is already bound.
    pub fn new_gsi(gsi: u32, level: bool, active_low: bool) -> Option<Arc<Interrupt>> {
        if gsi >= idt::IOAPIC_VECTORS as u32 {
            return None;
        }
        let vector = idt::IOAPIC_VECTOR_BASE + gsi as u8;
        let mut table = VECTORS.lock();
        if table[vector as usize].as_ref().is_some_and(|w| w.strong_count() > 0) {
            return None;
        }
        let irq = Interrupt::new(vector, Source::Gsi { gsi, level });
        table[vector as usize] = Some(Arc::downgrade(&irq));
        drop(table);
        if !apic::route_gsi(gsi, level, active_low, false) {
            return None;
        }
        Some(irq)
    }

    /// Allocates a free MSI vector for PCI function `device`, and the
    /// address and data that raise it (which only that function may use,
    /// when the IOMMU remaps interrupts).
    pub fn new_msi(device: u16) -> Option<(Arc<Interrupt>, (u64, u32))> {
        let mut table = VECTORS.lock();
        let vector = (idt::MSI_VECTOR_FIRST..=idt::MSI_VECTOR_LAST)
            .find(|&v| table[v as usize].as_ref().is_none_or(|w| w.strong_count() == 0))?;
        let irq = Interrupt::new(vector, Source::Msi);
        table[vector as usize] = Some(Arc::downgrade(&irq));
        drop(table);
        Some((irq, apic::msi_message(vector, device)))
    }

    /// Re-arms the interrupt after the driver has serviced it.
    pub fn ack(&self) {
        self.signals.update(SIGNALED, 0);
        if let Source::Gsi { gsi, level: true } = self.source {
            apic::set_gsi_masked(gsi, false);
        }
    }

    /// Makes the interrupt raise `vector` at `vcpu`'s local APIC from now
    /// on, instead of its signal. Only MSIs can be bound (an edge needs no
    /// end-of-interrupt from the guest).
    pub fn bind(&self, vcpu: &Arc<Vcpu>, vector: u8) -> bool {
        if !matches!(self.source, Source::Msi) {
            return false;
        }
        *self.target.lock() = Some((Arc::downgrade(vcpu), vector));
        true
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        let mut table = VECTORS.lock();
        // The vector may be another interrupt's already, taken while this
        // one was going: then the line and the vector are that one's.
        let slot = &mut table[self.vector as usize];
        if !slot.as_ref().is_some_and(|w| core::ptr::eq(w.as_ptr(), self)) {
            return;
        }
        *slot = None;
        if let Source::Gsi { gsi, .. } = self.source {
            apic::set_gsi_masked(gsi, true);
        }
        apic::release_vector(self.vector);
    }
}

/// Called from the trap dispatcher for device vectors (BKL held).
pub fn dispatch(vector: u8) {
    crate::random::add_interrupt_timing(vector);
    let irq = VECTORS.lock()[vector as usize].as_ref().and_then(|w| w.upgrade());
    let Some(irq) = irq else {
        crate::kdebug!("spurious device interrupt on vector {:#x}", vector);
        return;
    };
    let target = irq.target.lock().clone();
    if let Some((vcpu, guest_vector)) = target {
        // A processor that is gone takes no more interrupts.
        if let Some(vcpu) = vcpu.upgrade() {
            vcpu.interrupt(guest_vector);
        }
        return;
    }
    if let Source::Gsi { gsi, level: true } = irq.source {
        apic::set_gsi_masked(gsi, true);
    }
    irq.signals.update(0, SIGNALED);
}
