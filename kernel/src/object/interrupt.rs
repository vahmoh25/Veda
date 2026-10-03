//! Interrupt objects deliver hardware interrupts to user-space drivers.
//!
//! When the interrupt fires the kernel sets `SIGNALED` on the object (and,
//! for level-triggered lines, masks the line so it cannot storm). The driver
//! waits for `SIGNALED`, services its device and calls `irq_ack`, which
//! clears the signal and unmasks the line.

use alloc::sync::{Arc, Weak};

use vabi::signals::SIGNALED;

use super::Signals;
use crate::arch::{apic, idt};
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// An I/O APIC input.
    Gsi { gsi: u32, level: bool },
    /// A message-signalled interrupt vector.
    Msi,
}

pub struct Interrupt {
    pub koid: u64,
    pub vector: u8,
    pub source: Source,
    pub signals: Signals,
}

static VECTORS: SpinLock<[Option<Weak<Interrupt>>; 256]> = SpinLock::new([const { None }; 256]);

impl Interrupt {
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
        let irq = Arc::new(Interrupt {
            koid: super::new_koid(),
            vector,
            source: Source::Gsi { gsi, level },
            signals: Signals::new(0),
        });
        table[vector as usize] = Some(Arc::downgrade(&irq));
        drop(table);
        if !apic::route_gsi(gsi, level, active_low, false) {
            return None;
        }
        Some(irq)
    }

    /// Allocates a free MSI vector.
    pub fn new_msi() -> Option<Arc<Interrupt>> {
        let mut table = VECTORS.lock();
        let vector = (idt::MSI_VECTOR_FIRST..=idt::MSI_VECTOR_LAST)
            .find(|&v| table[v as usize].as_ref().is_none_or(|w| w.strong_count() == 0))?;
        let irq =
            Arc::new(Interrupt { koid: super::new_koid(), vector, source: Source::Msi, signals: Signals::new(0) });
        table[vector as usize] = Some(Arc::downgrade(&irq));
        Some(irq)
    }

    /// Re-arms the interrupt after the driver has serviced it.
    pub fn ack(&self) {
        self.signals.update(SIGNALED, 0);
        if let Source::Gsi { gsi, level: true } = self.source {
            apic::set_gsi_masked(gsi, false);
        }
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        if let Source::Gsi { gsi, .. } = self.source {
            apic::set_gsi_masked(gsi, true);
        }
        VECTORS.lock()[self.vector as usize] = None;
    }
}

/// Called from the trap dispatcher for device vectors (BKL held).
pub fn dispatch(vector: u8) {
    crate::random::add_interrupt_timing(vector);
    let irq = VECTORS.lock()[vector as usize].as_ref().and_then(|w| w.upgrade());
    match irq {
        Some(irq) => {
            if let Source::Gsi { gsi, level: true } = irq.source {
                apic::set_gsi_masked(gsi, true);
            }
            irq.signals.update(0, SIGNALED);
        }
        None => crate::kdebug!("spurious device interrupt on vector {:#x}", vector),
    }
}
