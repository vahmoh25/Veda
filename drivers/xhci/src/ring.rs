//! The rings (xHCI section 4.9): the command ring and the transfer rings,
//! which the driver fills, and the event ring, which the controller fills.
//! Each is one page of TRBs; whose turn a TRB is tells its cycle bit.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use vusb::xhci::Trb;
use vvirtio::DmaBuffer;

/// TRBs in a ring (one page).
const TRBS: usize = 4096 / size_of::<Trb>();
/// The last TRB of a command or transfer ring links back to the first.
const LINK: usize = TRBS - 1;

fn write_trb(mem: &DmaBuffer, index: usize, trb: Trb) {
    // SAFETY: index < TRBS, inside the page-sized, 16-byte aligned buffer.
    unsafe {
        let p = mem.ptr().add(index * size_of::<Trb>()) as *mut u32;
        write_volatile(p as *mut u64, trb.parameter);
        write_volatile(p.add(2), trb.status);
        // The controller may take the TRB as soon as the cycle bit in the
        // control word is its own: write that last.
        fence(Ordering::Release);
        write_volatile(p.add(3), trb.control);
    }
}

/// A ring the driver produces TRBs on: the command ring or the transfer
/// ring of an endpoint.
pub struct Ring {
    mem: DmaBuffer,
    enqueue: usize,
    cycle: bool,
}

impl Ring {
    /// A ring in `mem` (a zeroed page: no TRB belongs to the controller).
    pub fn new(mem: DmaBuffer) -> Ring {
        Ring { mem, enqueue: 0, cycle: true }
    }

    /// Where the ring starts (for the context or CRCR).
    pub fn base(&self) -> u64 {
        self.mem.phys()
    }

    /// Hands `trb` to the controller; returns its address (which events
    /// about it carry). The doorbell is the caller's to ring.
    pub fn push(&mut self, trb: Trb) -> u64 {
        let at = self.enqueue;
        write_trb(&self.mem, at, trb.with_cycle(self.cycle));
        self.enqueue += 1;
        if self.enqueue == LINK {
            write_trb(&self.mem, LINK, Trb::link(self.base(), trb.chained()).with_cycle(self.cycle));
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        self.base() + (at * size_of::<Trb>()) as u64
    }

    /// Where the controller continues after everything pushed so far, and
    /// with which cycle state (for Set TR Dequeue Pointer, which skips
    /// whatever a failed transfer left on the ring).
    pub fn next(&self) -> (u64, bool) {
        (self.base() + (self.enqueue * size_of::<Trb>()) as u64, self.cycle)
    }

    /// Whether a TRB address lies on this ring.
    pub fn contains(&self, address: u64) -> bool {
        (self.base()..self.base() + 4096).contains(&address)
    }

    /// The control word of the TRB at `address` on this ring.
    fn control_at(&self, address: u64) -> u32 {
        let index = ((address - self.base()) as usize / size_of::<Trb>()).min(LINK);
        // SAFETY: index < TRBS, inside the page.
        unsafe { read_volatile((self.mem.ptr().add(index * size_of::<Trb>()) as *const u32).add(3)) }
    }

    /// The cycle state the TRB at `address` was handed over with.
    pub fn cycle_at(&self, address: u64) -> bool {
        self.control_at(address) & 1 != 0
    }

    /// Turns the TRB at `address` (of a cancelled transfer, on a stopped
    /// endpoint) into one that moves nothing, which the controller skips.
    pub fn cancel(&mut self, address: u64) {
        if !self.contains(address) {
            return;
        }
        let control = self.control_at(address);
        let index = (address - self.base()) as usize / size_of::<Trb>();
        let chain = Trb { control, ..Trb::default() }.chained();
        write_trb(&self.mem, index, Trb::noop(chain).with_cycle(control & 1 != 0));
    }
}

/// The event ring of interrupter 0, a single segment.
pub struct EventRing {
    mem: DmaBuffer,
    /// The event ring segment table, with one entry.
    table: DmaBuffer,
    dequeue: usize,
    cycle: bool,
}

impl EventRing {
    pub fn new(mem: DmaBuffer, table: DmaBuffer) -> EventRing {
        let mut entry = [0u8; 16];
        entry[..8].copy_from_slice(&mem.phys().to_le_bytes());
        entry[8..12].copy_from_slice(&(TRBS as u32).to_le_bytes());
        table.write(0, &entry);
        EventRing { mem, table, dequeue: 0, cycle: true }
    }

    pub fn table(&self) -> u64 {
        self.table.phys()
    }

    /// The address of the next event to read, for ERDP.
    pub fn dequeue_pointer(&self) -> u64 {
        self.mem.phys() + (self.dequeue * size_of::<Trb>()) as u64
    }

    /// The next event, if the controller has written one.
    pub fn pop(&mut self) -> Option<Trb> {
        // SAFETY: dequeue < TRBS, inside the page-sized buffer.
        let trb = unsafe {
            let p = self.mem.ptr().add(self.dequeue * size_of::<Trb>()) as *const u32;
            let control = read_volatile(p.add(3));
            if (control & 1 != 0) != self.cycle {
                return None;
            }
            // The rest of the event is valid once the cycle bit is.
            fence(Ordering::Acquire);
            Trb { parameter: read_volatile(p as *const u64), status: read_volatile(p.add(2)), control }
        };
        self.dequeue += 1;
        if self.dequeue == TRBS {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(trb)
    }
}
