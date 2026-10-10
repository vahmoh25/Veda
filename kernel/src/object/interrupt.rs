//! Interrupt objects deliver hardware interrupts to user-space drivers.
//!
//! When the interrupt fires the kernel sets `SIGNALED` on the object (and,
//! for level-triggered lines, masks the line so it cannot storm). The driver
//! waits for `SIGNALED`, services its device and calls `irq_ack`, which
//! clears the signal and unmasks the line.
//!
//! An interrupt may instead be bound to a virtual processor (the device is
//! a guest's): then it raises a vector of the processor's local APIC, and
//! the guest's driver handles it as on a machine of its own. An edge (an
//! MSI, an edge-triggered line) needs nothing more; a level-triggered line
//! is masked when it fires, as above, and raised as a level-triggered
//! interrupt, whose end-of-interrupt at the virtual processor unmasks the
//! line again — as a PC's I/O APIC waits for the processor's
//! end-of-interrupt — unless it is no longer bound by then (the guest masks
//! a line by having it bound to nothing).
//!
//! Every device interrupt has a vector of its own, from one pool: an MSI's
//! when it is made, a line's when it is set up (any input of the machine's
//! I/O APICs).
//!
//! A program can make interrupts of its own and raise them (`irq_raise`):
//! the lines of an interrupt controller it drives itself, which the kernel
//! does not (a GPIO controller's pins, whose controller has one line for
//! them all). They reach their driver, or a guest, as a line's do. A
//! level-triggered one stays raised until it is ended — acknowledged, or
//! the guest's end-of-interrupt — and then signals the event its maker
//! gave, so the maker looks at its source again (KVM's irqfd and resample
//! eventfd have VFIO do the same).

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};

use vabi::Error;
use vabi::signals::SIGNALED;

use super::Signals;
use super::event::Event;
use crate::arch::{apic, idt};
use crate::hv::vcpu::Vcpu;
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// An I/O APIC input.
    Gsi { gsi: u32, level: bool },
    /// A message-signalled interrupt (of a PCI function).
    Msi,
    /// Raised by a program: no vector of the machine's.
    Software { level: bool },
}

pub struct Interrupt {
    pub koid: u64,
    pub vector: u8,
    pub source: Source,
    pub signals: Signals,
    /// The virtual processor and vector it is raised at instead, if bound.
    target: SpinLock<Option<(Weak<Vcpu>, u8)>>,
    /// A level-triggered software interrupt raised and not yet ended.
    raised: AtomicBool,
    /// What a software interrupt's ending signals.
    ended: Option<Arc<Event>>,
}

/// The interrupt each vector belongs to, and its source (seen without
/// taking a reference, which could be the last).
struct Slot {
    irq: Weak<Interrupt>,
    source: Source,
}

impl Slot {
    /// A live interrupt of I/O APIC input `gsi`.
    fn has_line(&self, gsi: u32) -> bool {
        self.irq.strong_count() > 0 && matches!(self.source, Source::Gsi { gsi: g, .. } if g == gsi)
    }
}

static VECTORS: SpinLock<[Option<Slot>; 256]> = SpinLock::new([const { None }; 256]);

impl Interrupt {
    fn new(vector: u8, source: Source) -> Arc<Interrupt> {
        Self::with_ended(vector, source, None)
    }

    fn with_ended(vector: u8, source: Source, ended: Option<Arc<Event>>) -> Arc<Interrupt> {
        Arc::new(Interrupt {
            koid: super::new_koid(),
            vector,
            source,
            signals: Signals::new(0),
            target: SpinLock::new(None),
            raised: AtomicBool::new(false),
            ended,
        })
    }

    /// An interrupt its maker raises: an edge, or a level-triggered one,
    /// whose ending signals `ended`.
    pub fn new_software(level: bool, ended: Option<Arc<Event>>) -> Arc<Interrupt> {
        Self::with_ended(0, Source::Software { level }, ended)
    }

    /// Raises a software interrupt: at the virtual processor it is bound
    /// to, or its signal. A level-triggered one raised already stays
    /// raised, once, until it is ended.
    pub fn raise(self: &Arc<Self>) -> Result<(), Error> {
        let Source::Software { level } = self.source else { return Err(Error::WrongType) };
        if level && self.raised.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let target = self.target.lock().clone();
        match target {
            Some((vcpu, vector)) => {
                // A processor that is gone takes no more interrupts.
                if let Some(vcpu) = vcpu.upgrade() {
                    if level {
                        vcpu.interrupt_level(vector, self);
                    } else {
                        vcpu.interrupt(vector);
                    }
                }
            }
            None => self.signals.update(0, SIGNALED),
        }
        Ok(())
    }

    /// Sets up an I/O APIC input. Fails if the line is already set up, if
    /// no I/O APIC has it or if no vector is free.
    pub fn new_gsi(gsi: u32, level: bool, active_low: bool) -> Result<Arc<Interrupt>, Error> {
        let mut table = VECTORS.lock();
        if table.iter().flatten().any(|s| s.has_line(gsi)) {
            return Err(Error::AlreadyExists);
        }
        let vector = free_vector(&table).ok_or(Error::LimitReached)?;
        let source = Source::Gsi { gsi, level };
        let irq = Interrupt::new(vector, source);
        table[vector as usize] = Some(Slot { irq: Arc::downgrade(&irq), source });
        drop(table);
        if !apic::route_gsi(gsi, vector, level, active_low, false) {
            return Err(Error::NotFound);
        }
        Ok(irq)
    }

    /// Allocates a vector for an MSI of PCI function `device`, and the
    /// address and data that raise it (which only that function may use,
    /// when the IOMMU remaps interrupts).
    pub fn new_msi(device: u16) -> Option<(Arc<Interrupt>, (u64, u32))> {
        let mut table = VECTORS.lock();
        let vector = free_vector(&table)?;
        let irq = Interrupt::new(vector, Source::Msi);
        table[vector as usize] = Some(Slot { irq: Arc::downgrade(&irq), source: Source::Msi });
        drop(table);
        Some((irq, apic::msi_message(vector, device)))
    }

    /// Re-arms the interrupt after the driver has serviced it: a line is
    /// unmasked, a level-triggered software interrupt ends.
    pub fn ack(&self) {
        self.signals.update(SIGNALED, 0);
        match self.source {
            Source::Gsi { gsi, level: true } => apic::set_gsi_masked(gsi, false),
            Source::Software { level: true } if self.raised.swap(false, Ordering::AcqRel) => {
                if let Some(e) = &self.ended {
                    e.signals.update(0, SIGNALED);
                }
            }
            _ => {}
        }
    }

    /// Makes the interrupt raise `vector` at `vcpu`'s local APIC from now
    /// on, instead of its signal.
    pub fn bind(&self, vcpu: &Arc<Vcpu>, vector: u8) {
        *self.target.lock() = Some((Arc::downgrade(vcpu), vector));
    }

    /// Makes the interrupt signal again, raising nothing at a processor (a
    /// level-triggered line that fires stays masked until it is acked).
    pub fn unbind(&self) {
        *self.target.lock() = None;
    }

    /// The guest ended the level-triggered interrupt this line raised:
    /// the line is unmasked, if it is still bound.
    pub fn end_of_level(&self) {
        if self.target.lock().is_some() {
            self.ack();
        }
    }
}

/// A vector no device interrupt has.
fn free_vector(table: &[Option<Slot>; 256]) -> Option<u8> {
    (idt::DEVICE_VECTOR_FIRST..=idt::DEVICE_VECTOR_LAST)
        .find(|&v| table[v as usize].as_ref().is_none_or(|s| s.irq.strong_count() == 0))
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        if let Source::Software { .. } = self.source {
            return;
        }
        let mut table = VECTORS.lock();
        // The vector may be another interrupt's already, taken while this
        // one was going: then the line and the vector are that one's.
        let slot = &mut table[self.vector as usize];
        if !slot.as_ref().is_some_and(|s| core::ptr::eq(s.irq.as_ptr(), self)) {
            return;
        }
        *slot = None;
        // The line, unless another interrupt has it already.
        if let Source::Gsi { gsi, .. } = self.source
            && !table.iter().flatten().any(|s| s.has_line(gsi))
        {
            apic::set_gsi_masked(gsi, true);
        }
        apic::release_vector(self.vector);
    }
}

/// Called from the trap dispatcher for device vectors (BKL held).
pub fn dispatch(vector: u8) {
    crate::random::add_interrupt_timing(vector);
    let irq = VECTORS.lock()[vector as usize].as_ref().and_then(|s| s.irq.upgrade());
    let Some(irq) = irq else {
        crate::kdebug!("spurious device interrupt on vector {:#x}", vector);
        return;
    };
    let level = match irq.source {
        Source::Gsi { gsi, level: true } => {
            apic::set_gsi_masked(gsi, true);
            true
        }
        _ => false,
    };
    let target = irq.target.lock().clone();
    if let Some((vcpu, guest_vector)) = target {
        // A processor that is gone takes no more interrupts.
        if let Some(vcpu) = vcpu.upgrade() {
            if level {
                vcpu.interrupt_level(guest_vector, &irq);
            } else {
                vcpu.interrupt(guest_vector);
            }
        }
        return;
    }
    irq.signals.update(0, SIGNALED);
}
