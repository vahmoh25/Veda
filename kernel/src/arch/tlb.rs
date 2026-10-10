//! TLB shootdowns.
//!
//! When the BKL holder changes a user mapping that other CPUs may have
//! cached, it asks every other online CPU to flush its TLB and waits for the
//! acknowledgements. CPUs running user code receive an IPI; CPUs spinning on
//! the BKL poll their request flag (see `sync::bkl::acquire`); idle CPUs are
//! woken by the IPI. No CPU needs the BKL to acknowledge, so this cannot
//! deadlock.
//!
//! Guests' physical memory (EPT) is shot down the same way: a CPU that
//! runs a guest leaves it for the IPI, and forgets every guest's
//! translations (`invept`).

use core::sync::atomic::Ordering;

use super::{apic, cpu, idt::TLB_VECTOR, percpu};

/// Flushes the local TLB, or the guests' translations, if another CPU
/// requested it.
pub fn service_pending() {
    let me = percpu::current();
    let tlb = me.tlb_flush_pending.swap(false, Ordering::AcqRel);
    let ept = me.ept_flush_pending.swap(false, Ordering::AcqRel);
    if tlb {
        cpu::flush_tlb();
    }
    if ept && me.vmx_on.load(Ordering::Relaxed) {
        crate::hv::vmx::invept_all();
    }
    if tlb || ept {
        me.tlb_flush_done.fetch_add(1, Ordering::AcqRel);
    }
}

/// Handler for the shootdown IPI.
pub fn handle_ipi() {
    service_pending();
}

/// Flushes `addr..addr+len` locally and on every other online CPU.
pub fn shootdown(addr: u64, len: u64) {
    let pages = len.div_ceil(4096);
    if pages <= 32 {
        for i in 0..pages {
            cpu::invlpg(addr + i * 4096);
        }
    } else {
        cpu::flush_tlb();
    }
    request_all(|p| &p.tlb_flush_pending);
}

/// Makes every CPU forget the translations of guests' physical memory it
/// caches, before memory unmapped from a guest is reused.
pub fn shootdown_guest_memory() {
    if percpu::current().vmx_on.load(Ordering::Relaxed) {
        crate::hv::vmx::invept_all();
    }
    request_all(|p| &p.ept_flush_pending);
}

/// Sets the flag `flag` picks on every other online CPU and waits until
/// each has acknowledged it.
fn request_all(flag: impl Fn(&percpu::PerCpu) -> &core::sync::atomic::AtomicBool) {
    let me = percpu::cpu_id_or_boot();
    let mut targets: [(u32, u64); percpu::MAX_CPUS] = [(u32::MAX, 0); percpu::MAX_CPUS];
    let mut n = 0;
    for p in percpu::online() {
        if p.cpu_id == me {
            continue;
        }
        let before = p.tlb_flush_done.load(Ordering::Acquire);
        flag(p).store(true, Ordering::Release);
        apic::send_ipi(p.apic_id, TLB_VECTOR as u32);
        targets[n] = (p.cpu_id, before);
        n += 1;
    }
    for &(id, before) in &targets[..n] {
        let p = percpu::get(id as usize);
        let mut spins = 0u64;
        // The target clears `pending` before flushing, so only the
        // acknowledgement counter proves the flush has completed.
        while p.tlb_flush_done.load(Ordering::Acquire) == before {
            core::hint::spin_loop();
            spins += 1;
            if spins == 100_000_000 {
                crate::kwarn!("tlb: CPU {id} is slow to acknowledge a shootdown");
            }
        }
    }
}
