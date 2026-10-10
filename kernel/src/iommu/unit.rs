//! One remapping unit: its registers, its invalidation queue, and the root
//! and context tables that put each of its devices in a domain.
//!
//! Every bus starts with the same context table, which puts each device in
//! the host's domain. A bus gets a table of its own when one of its devices
//! goes elsewhere: a copy, with that device's entry changed.

use alloc::collections::BTreeMap;

use viommu::vtd::{self, Cap, Ecap, Fault, Translation, desc, fsts, gcmd, reg};

use crate::mm::paging::Cache;
use crate::mm::{kvirt, phys, phys_to_virt};
use crate::sync::SpinLock;

/// The host's domain: devices in it reach memory untranslated.
pub const HOST_DOMAIN: u16 = 1;

/// How long a unit may take to do a command, or the queue's descriptors.
const TIMEOUT_NS: u64 = 1_000_000_000;

/// Descriptors in the queue (one page of them).
const QUEUE_LEN: usize = 256;

pub struct Unit {
    /// Where its registers are (physical; for the log).
    pub base: u64,
    regs: u64,
    pub cap: Cap,
    pub ecap: Ecap,
    queue: SpinLock<Queue>,
    tables: SpinLock<Tables>,
}

struct Queue {
    /// The ring of descriptors (a frame).
    ring: u64,
    /// Where the next descriptor goes.
    tail: usize,
    /// The word the unit writes when it has done the descriptors before (a
    /// frame's first), and the value it writes next.
    status: u64,
    sequence: u32,
}

struct Tables {
    root: u64,
    /// The context table every bus starts with.
    host: u64,
    /// Buses with a context table of their own.
    own: BTreeMap<u8, u64>,
}

fn table(frame: u64) -> &'static mut [[u64; 2]; 256] {
    // SAFETY: root and context tables are frames the unit owns,
    // direct-mapped; entries are written with volatile stores below.
    unsafe { &mut *(phys_to_virt(frame) as *mut [[u64; 2]; 256]) }
}

impl Unit {
    /// Maps the unit's registers and checks it can do what Veda needs:
    /// queued invalidation, interrupt remapping, pass-through, three- or
    /// four-level page tables.
    pub fn new(base: u64) -> Result<Unit, &'static str> {
        let mut regs = kvirt::map_mmio(base, 4096, Cache::Uncached);
        // SAFETY: the registers were just mapped.
        let read = |regs: u64, r: usize| unsafe { core::ptr::read_volatile((regs + r as u64) as *const u64) };
        let (cap, ecap) = (Cap(read(regs, reg::CAP)), Ecap(read(regs, reg::ECAP)));
        let (records, count) = cap.fault_records();
        if records + count * 16 > 4096 {
            regs = kvirt::map_mmio(base, (records + count * 16) as u64, Cache::Uncached);
        }
        crate::kdebug!("iommu: unit at {:#x}: capabilities {:#x}, extended {:#x}", base, cap.0, ecap.0);
        if !ecap.queued_invalidation() || !ecap.interrupt_remapping() {
            return Err("a unit cannot remap interrupts");
        }
        if !ecap.pass_through() {
            return Err("a unit cannot pass devices' requests through");
        }
        if vtd::levels(cap.sagaw()).is_none() {
            return Err("a unit walks neither three- nor four-level page tables");
        }
        if cap.rwbf() {
            return Err("a unit needs its write buffer flushed (a kind Veda does not drive)");
        }
        let (_, address_width) = vtd::levels(cap.sagaw()).unwrap_or_default();
        let frames = [phys::alloc_zeroed(), phys::alloc_zeroed(), phys::alloc_zeroed(), phys::alloc_zeroed()];
        let [Some(ring), Some(status), Some(root), Some(host)] = frames else {
            frames.into_iter().flatten().for_each(phys::free);
            return Err("no memory for a unit's tables");
        };
        // Every device of every bus in the host's domain. For pass-through
        // the address width is the widest the unit walks.
        let pass = vtd::context_entry(Translation::PassThrough, HOST_DOMAIN, address_width);
        table(host).fill(pass);
        table(root).fill(vtd::root_entry(host));
        let unit = Unit {
            base,
            regs,
            cap,
            ecap,
            queue: SpinLock::new(Queue { ring, tail: 0, status, sequence: 0 }),
            tables: SpinLock::new(Tables { root, host, own: BTreeMap::new() }),
        };
        unit.publish(host, 4096);
        unit.publish(root, 4096);
        Ok(unit)
    }

    fn read32(&self, r: usize) -> u32 {
        // SAFETY: `r` is a register of the unit, mapped uncached.
        unsafe { core::ptr::read_volatile((self.regs + r as u64) as *const u32) }
    }

    fn read64(&self, r: usize) -> u64 {
        // SAFETY: as above.
        unsafe { core::ptr::read_volatile((self.regs + r as u64) as *const u64) }
    }

    fn write32(&self, r: usize, value: u32) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.regs + r as u64) as *mut u32, value) };
    }

    fn write64(&self, r: usize, value: u64) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.regs + r as u64) as *mut u64, value) };
    }

    /// Writes back what the processor wrote to the unit's tables, if its
    /// walks do not see the caches.
    fn publish(&self, phys: u64, len: u64) {
        if !self.ecap.coherent() {
            crate::arch::cpu::flush_cache_range(phys_to_virt(phys), len);
        }
    }

    /// Spins until `done`, for at most [`TIMEOUT_NS`].
    fn wait(&self, done: impl Fn() -> bool) -> bool {
        let start = crate::time::now_ns();
        while !done() {
            if crate::time::now_ns() - start > TIMEOUT_NS {
                return done();
            }
            core::hint::spin_loop();
        }
        true
    }

    /// Turns a state of the unit on or off (`TE`, `QIE`, `IRE`, `CFI`), or
    /// gives a one-shot command (`SRTP`, `SIRTP`: `on`), and waits until
    /// its status shows it done.
    fn command(&self, bit: u32, on: bool) -> Result<(), &'static str> {
        let states = self.read32(reg::GSTS) & gcmd::STATES;
        self.write32(reg::GCMD, if on { states | bit } else { states & !bit });
        if self.wait(|| (self.read32(reg::GSTS) & bit != 0) == on) {
            Ok(())
        } else {
            Err("a unit did not do a command")
        }
    }

    /// Turns the unit on: its queue, interrupt remapping through `table`
    /// (`2^(size+1)` entries) with the compatibility format blocked, every
    /// device in the host's domain, and its fault event interrupt (to
    /// `vector` of the processor with APIC id `destination`).
    pub fn enable(
        &self,
        table: u64,
        size: u32,
        x2apic: bool,
        vector: u8,
        destination: u32,
    ) -> Result<(), &'static str> {
        // What the firmware left on goes off first.
        let status = self.read32(reg::GSTS);
        if status & gcmd::TE != 0 {
            self.command(gcmd::TE, false)?;
        }
        if status & gcmd::IRE != 0 {
            self.command(gcmd::IRE, false)?;
        }
        if status & gcmd::QIE != 0 {
            self.wait(|| self.read64(reg::IQH) == self.read64(reg::IQT));
            self.command(gcmd::QIE, false)?;
        }
        // Faults from before go; new ones are reported to the kernel.
        self.take_faults(|_| {});
        self.write32(reg::FEDATA, vector as u32);
        self.write32(reg::FEADDR, 0xFEE0_0000 | ((destination & 0xFF) << 12));
        self.write32(reg::FEUADDR, destination & !0xFF);
        self.write32(reg::FECTL, 0);

        let (ring, root) = (self.queue.lock().ring, self.tables.lock().root);
        self.write64(reg::IQT, 0);
        self.write64(reg::IQA, ring);
        self.command(gcmd::QIE, true)?;

        self.write64(reg::IRTA, vtd::irta(table, size, x2apic));
        self.command(gcmd::SIRTP, true)?;
        self.invalidate(&[desc::interrupt_global()])?;
        self.command(gcmd::CFI, false)?;
        self.command(gcmd::IRE, true)?;

        self.write64(reg::RTADDR, root);
        self.command(gcmd::SRTP, true)?;
        self.invalidate(&[desc::context_global(), desc::iotlb_global()])?;
        self.command(gcmd::TE, true)?;

        // Protected memory (which some firmware leaves on) is not needed
        // once requests are translated.
        if self.read32(reg::PMEN) & vtd::PMEN_EPM != 0 {
            self.write32(reg::PMEN, 0);
            self.wait(|| self.read32(reg::PMEN) & vtd::PMEN_PRS == 0);
        }
        Ok(())
    }

    /// Turns translation, interrupt remapping and the queue off (when
    /// another unit could not be set up).
    pub fn disable(&self) {
        let _ = self.command(gcmd::TE, false);
        let _ = self.command(gcmd::IRE, false);
        let _ = self.command(gcmd::QIE, false);
    }

    /// Has the unit do `descs` and waits until it has.
    pub fn invalidate(&self, descs: &[[u64; 2]]) -> Result<(), &'static str> {
        let mut q = self.queue.lock();
        q.sequence = q.sequence.wrapping_add(1).max(1);
        let (status, sequence) = (phys_to_virt(q.status) as *mut u32, q.sequence);
        let ring = phys_to_virt(q.ring) as *mut [u64; 2];
        // SAFETY (all three): the status word and the ring are frames of
        // the queue, direct-mapped; `tail` is below QUEUE_LEN.
        unsafe { core::ptr::write_volatile(status, 0) };
        for d in descs.iter().copied().chain(core::iter::once(desc::wait(q.status, sequence))) {
            unsafe { core::ptr::write_volatile(ring.add(q.tail), d) };
            q.tail = (q.tail + 1) % QUEUE_LEN;
        }
        self.write64(reg::IQT, (q.tail as u64) << 4);
        let failed = || self.read32(reg::FSTS) & fsts::IQE != 0;
        let done = self.wait(|| unsafe { core::ptr::read_volatile(status) } == sequence || failed());
        if failed() {
            // The unit stopped at the descriptor: what is left is dropped.
            q.tail = (self.read64(reg::IQH) >> 4) as usize % QUEUE_LEN;
            self.write64(reg::IQT, (q.tail as u64) << 4);
            self.write32(reg::FSTS, fsts::IQE);
            return Err("a unit refused an invalidation");
        }
        if done { Ok(()) } else { Err("a unit did not finish an invalidation") }
    }

    /// The context entry of device `sid`.
    pub fn context(&self, sid: u16) -> [u64; 2] {
        let t = self.tables.lock();
        let frame = t.own.get(&((sid >> 8) as u8)).copied().unwrap_or(t.host);
        table(frame)[(sid & 0xFF) as usize]
    }

    /// Sets device `sid`'s context entry (`None`: not present, the device
    /// reaches nothing), and has the unit forget the old one and what it
    /// translated with it.
    pub fn set_context(&self, sid: u16, entry: Option<[u64; 2]>) -> Result<(), &'static str> {
        let mut t = self.tables.lock();
        let bus = (sid >> 8) as u8;
        let frame = match t.own.get(&bus) {
            Some(&f) => f,
            None => {
                let f = phys::alloc().ok_or("no memory for a context table")?;
                *table(f) = *table(t.host);
                self.publish(f, 4096);
                let root = t.root;
                // SAFETY: an entry of the root table, which the unit reads.
                unsafe { core::ptr::write_volatile(&mut table(root)[bus as usize], vtd::root_entry(f)) };
                self.publish(root + bus as u64 * 16, 16);
                t.own.insert(bus, f);
                f
            }
        };
        let at = frame + (sid & 0xFF) as u64 * 16;
        let e = &mut table(frame)[(sid & 0xFF) as usize];
        let old = *e;
        let domain = |e: [u64; 2]| (e[1] >> 8) as u16;
        // A present entry goes absent first, and is forgotten, so that the
        // unit never reads half of the new one.
        if vtd::context_state(old).is_some() {
            // SAFETY (both blocks): the entry is the unit's, read by it.
            unsafe { core::ptr::write_volatile(&mut e[0], 0) };
            self.publish(at, 16);
            self.invalidate(&[desc::context_device(domain(old), sid), desc::iotlb_domain(domain(old))])?;
        }
        if let Some([low, high]) = entry {
            unsafe {
                core::ptr::write_volatile(&mut e[1], high);
                core::ptr::write_volatile(&mut e[0], low);
            }
            self.publish(at, 16);
            // Units in caching mode may have cached the absent entry.
            self.invalidate(&[desc::context_device(domain([low, high]), sid)])?;
        }
        Ok(())
    }

    /// Hands each fault the unit recorded to `report`, and clears them and
    /// the errors it noted.
    pub fn take_faults(&self, mut report: impl FnMut(Fault)) {
        let status = self.read32(reg::FSTS);
        if status & fsts::PPF != 0 {
            let (records, count) = self.cap.fault_records();
            let mut i = fsts::first_record(status) % count;
            for _ in 0..count {
                let at = records + i * 16;
                let Some(fault) = Fault::from_record([self.read64(at), self.read64(at + 8)]) else { break };
                report(fault);
                // The record's F bit (its last) is cleared by writing it.
                self.write32(at + 12, 1 << 31);
                i = (i + 1) % count;
            }
        }
        if status & fsts::ERRORS != 0 {
            self.write32(reg::FSTS, status & fsts::ERRORS);
        }
    }
}
