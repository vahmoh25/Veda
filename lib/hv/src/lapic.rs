//! The local APIC of a virtual processor, in x2APIC mode.
//!
//! A guest reaches it through the x2APIC MSRs (`0x800` to `0x8FF`) and
//! `IA32_TSC_DEADLINE`: there is no memory-mapped (xAPIC) interface, and
//! the APIC cannot leave x2APIC mode. Interrupts are delivered as fixed
//! interrupts (or NMIs, which the caller delivers): edges, or level-
//! triggered ones, whose end-of-interrupt the APIC reports, for the caller
//! to end the interrupt at its source (as an APIC's end-of-interrupt
//! message ends an I/O APIC's).
//!
//! Time is the TSC's, which the guest reads as it is: the timer counts TSC
//! ticks divided by its divider, and in TSC-deadline mode fires when the
//! TSC reaches the deadline. The caller tells every operation what the TSC
//! is (`now`), and asks [`Lapic::timer_deadline`] when to come back.
//!
//! A register a guest may not read or write (one that does not exist, or a
//! value with reserved bits set) is refused: the caller raises `#GP`.

/// `IA32_TSC_DEADLINE`.
pub const MSR_TSC_DEADLINE: u32 = 0x6E0;
/// The first and last x2APIC MSR.
pub const MSR_FIRST: u32 = 0x800;
pub const MSR_LAST: u32 = 0x8FF;

/// Whether `msr` belongs to the local APIC.
pub fn is_lapic_msr(msr: u32) -> bool {
    (MSR_FIRST..=MSR_LAST).contains(&msr) || msr == MSR_TSC_DEADLINE
}

const ID: u32 = 0x802;
const VERSION: u32 = 0x803;
const TPR: u32 = 0x808;
const PPR: u32 = 0x80A;
const EOI: u32 = 0x80B;
const LDR: u32 = 0x80D;
const SVR: u32 = 0x80F;
const ISR: u32 = 0x810;
const TMR: u32 = 0x818;
const IRR: u32 = 0x820;
const ESR: u32 = 0x828;
const ICR: u32 = 0x830;
const LVT_TIMER: u32 = 0x832;
const LVT_ERROR: u32 = 0x837;
const TIMER_INITIAL: u32 = 0x838;
const TIMER_CURRENT: u32 = 0x839;
const TIMER_DIVIDE: u32 = 0x83E;
const SELF_IPI: u32 = 0x83F;

/// Version 0x14, with six local vector table entries (timer, thermal,
/// performance counters, LINT0, LINT1, error).
const VERSION_VALUE: u32 = 0x0005_0014;
const LVT_COUNT: usize = 6;
const LVT_MASKED: u32 = 1 << 16;
/// The bits of each local vector table entry a guest may write, in the
/// order of the registers.
const LVT_WRITABLE: [u32; LVT_COUNT] = [0x7_00FF, 0x1_07FF, 0x1_07FF, 0x1_A7FF, 0x1_A7FF, 0x1_00FF];
const SVR_ENABLE: u32 = 1 << 8;

/// Error status bits.
const ESR_SEND_ILLEGAL: u32 = 1 << 5;
const ESR_RECEIVE_ILLEGAL: u32 = 1 << 6;

/// The timer's modes (LVT timer bits 17-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMode {
    OneShot,
    Periodic,
    Deadline,
}

/// How an inter-processor interrupt is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A fixed interrupt (lowest-priority delivery is taken for one too).
    Fixed(u8),
    Nmi,
    /// INIT, start-up, SMI and the reserved modes: the platform has no use
    /// for them (processors start through a hypercall).
    Unsupported(u8),
}

/// Whom an inter-processor interrupt goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// The local APIC whose id this is (all with `0xFFFF_FFFF`).
    Physical(u32),
    /// The local APICs a logical destination (cluster and member bits)
    /// names (all with `0xFFFF_FFFF`).
    Logical(u32),
    SelfOnly,
    All,
    AllButSelf,
}

/// An inter-processor interrupt a guest sent, for the caller to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipi {
    pub delivery: Delivery,
    pub destination: Destination,
}

/// The effect of a register write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    Done,
    /// The guest may not write that (`#GP`).
    Refused,
    /// The guest sent an inter-processor interrupt.
    Ipi(Ipi),
    /// The guest ended a level-triggered interrupt of this vector.
    EndOfLevel(u8),
}

/// A virtual local APIC.
#[derive(Debug, Clone)]
pub struct Lapic {
    id: u32,
    tpr: u32,
    svr: u32,
    /// Errors since the last write of the error status register, and what
    /// that write latched for reading.
    errors: u32,
    esr: u32,
    irr: [u32; 8],
    isr: [u32; 8],
    /// The trigger mode of the interrupts requested and in service: set
    /// for level-triggered ones.
    tmr: [u32; 8],
    lvt: [u32; LVT_COUNT],
    icr: u64,
    timer_initial: u32,
    timer_divide: u32,
    /// The TSC at which the timer fires next (0: not armed).
    timer_at: u64,
    /// A periodic timer's period, in TSC ticks.
    timer_period: u64,
}

impl Lapic {
    /// A local APIC as a processor has it at reset: software-disabled,
    /// every local interrupt masked.
    pub fn new(id: u32) -> Lapic {
        Lapic {
            id,
            tpr: 0,
            svr: 0xFF,
            errors: 0,
            esr: 0,
            irr: [0; 8],
            isr: [0; 8],
            tmr: [0; 8],
            lvt: [LVT_MASKED; LVT_COUNT],
            icr: 0,
            timer_initial: 0,
            timer_divide: 0,
            timer_at: 0,
            timer_period: 0,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// The logical id x2APIC mode gives this APIC: its cluster (the id's
    /// upper bits) and one bit of sixteen.
    pub fn logical_id(&self) -> u32 {
        ((self.id >> 4) << 16) | (1 << (self.id & 0xF))
    }

    fn enabled(&self) -> bool {
        self.svr & SVR_ENABLE != 0
    }

    fn timer_mode(&self) -> TimerMode {
        match (self.lvt[0] >> 17) & 3 {
            1 => TimerMode::Periodic,
            2 => TimerMode::Deadline,
            _ => TimerMode::OneShot,
        }
    }

    /// The timer's divider (1 to 128).
    fn divider(&self) -> u64 {
        let v = ((self.timer_divide & 8) >> 1) | (self.timer_divide & 3);
        if v == 7 { 1 } else { 2 << v }
    }

    /// Whether this APIC is one of `destination`'s (`from_self`: the
    /// sender is this APIC).
    pub fn is_destination(&self, destination: Destination, from_self: bool) -> bool {
        match destination {
            Destination::Physical(d) => d == u32::MAX || d == self.id,
            Destination::Logical(d) => {
                let ldr = self.logical_id();
                d == u32::MAX || (d >> 16 == ldr >> 16 && d & ldr & 0xFFFF != 0)
            }
            Destination::SelfOnly => from_self,
            Destination::All => true,
            Destination::AllButSelf => !from_self,
        }
    }

    /// Reads a register (`None`: the guest may not, `#GP`).
    pub fn read(&self, msr: u32, now: u64) -> Option<u64> {
        let v = match msr {
            ID => self.id,
            VERSION => VERSION_VALUE,
            TPR => self.tpr,
            PPR => self.ppr(),
            LDR => self.logical_id(),
            SVR => self.svr,
            ISR..=0x817 => self.isr[(msr - ISR) as usize],
            TMR..=0x81F => self.tmr[(msr - TMR) as usize],
            IRR..=0x827 => self.irr[(msr - IRR) as usize],
            ESR => self.esr,
            ICR => return Some(self.icr),
            LVT_TIMER..=LVT_ERROR => self.lvt[(msr - LVT_TIMER) as usize],
            TIMER_INITIAL => self.timer_initial,
            TIMER_CURRENT => self.timer_current(now),
            TIMER_DIVIDE => self.timer_divide,
            MSR_TSC_DEADLINE => {
                return Some(if self.timer_mode() == TimerMode::Deadline { self.timer_at } else { 0 });
            }
            _ => return None,
        };
        Some(v as u64)
    }

    /// Writes a register.
    pub fn write(&mut self, msr: u32, value: u64, now: u64) -> Write {
        if msr == MSR_TSC_DEADLINE {
            // Ignored in the other modes.
            if self.timer_mode() == TimerMode::Deadline {
                self.timer_at = value;
            }
            return Write::Done;
        }
        if msr == ICR {
            return self.write_icr(value);
        }
        // The other registers are 32 bits wide.
        let Ok(v) = u32::try_from(value) else { return Write::Refused };
        match msr {
            TPR if v <= 0xFF => self.tpr = v,
            EOI if v == 0 => {
                if let Some(vector) = self.eoi() {
                    return Write::EndOfLevel(vector);
                }
            }
            SVR if v & !0x1FF == 0 => {
                self.svr = v;
                if !self.enabled() {
                    for entry in &mut self.lvt {
                        *entry |= LVT_MASKED;
                    }
                }
            }
            ESR if v == 0 => {
                self.esr = self.errors;
                self.errors = 0;
            }
            LVT_TIMER..=LVT_ERROR => {
                let i = (msr - LVT_TIMER) as usize;
                let mode = self.timer_mode();
                let mut entry = v & LVT_WRITABLE[i];
                if !self.enabled() {
                    entry |= LVT_MASKED;
                }
                self.lvt[i] = entry;
                if i == 0 && self.timer_mode() != mode {
                    self.stop_timer();
                }
            }
            TIMER_INITIAL => {
                if self.timer_mode() != TimerMode::Deadline {
                    self.timer_initial = v;
                    let ticks = v as u64 * self.divider();
                    self.timer_period = ticks;
                    self.timer_at = if v == 0 { 0 } else { now.wrapping_add(ticks).max(1) };
                }
            }
            TIMER_DIVIDE if v & !0xB == 0 => self.timer_divide = v,
            SELF_IPI if v <= 0xFF => self.request(v as u8),
            _ => return Write::Refused,
        }
        Write::Done
    }

    fn write_icr(&mut self, value: u64) -> Write {
        let vector = value as u8;
        let mode = ((value >> 8) & 7) as u8;
        let logical = value & (1 << 11) != 0;
        let target = (value >> 32) as u32;
        let destination = match (value >> 18) & 3 {
            0 if logical => Destination::Logical(target),
            0 => Destination::Physical(target),
            1 => Destination::SelfOnly,
            2 => Destination::All,
            _ => Destination::AllButSelf,
        };
        self.icr = value;
        let delivery = match mode {
            0 | 1 if vector < 16 => {
                // An illegal vector is not sent.
                self.errors |= ESR_SEND_ILLEGAL;
                return Write::Done;
            }
            0 | 1 => Delivery::Fixed(vector),
            4 => Delivery::Nmi,
            m => Delivery::Unsupported(m),
        };
        Write::Ipi(Ipi { delivery, destination })
    }

    fn stop_timer(&mut self) {
        self.timer_at = 0;
        self.timer_initial = 0;
        self.timer_period = 0;
    }

    fn timer_current(&self, now: u64) -> u32 {
        if self.timer_mode() == TimerMode::Deadline || self.timer_at == 0 {
            return 0;
        }
        (self.timer_at.saturating_sub(now) / self.divider()).min(u32::MAX as u64) as u32
    }

    /// When the timer fires next (a TSC value), if it is armed.
    pub fn timer_deadline(&self) -> Option<u64> {
        (self.timer_at != 0).then_some(self.timer_at)
    }

    /// Fires the timer if its time has come: raises its interrupt (unless
    /// masked) and arms it again if it is periodic.
    pub fn expire_timer(&mut self, now: u64) {
        if self.timer_at == 0 || now < self.timer_at {
            return;
        }
        let entry = self.lvt[0];
        if entry & LVT_MASKED == 0 {
            self.request(entry as u8);
        }
        match self.timer_mode() {
            TimerMode::Periodic if self.timer_period != 0 => {
                // The next period after now (periods that passed unseen
                // are not made up).
                let late = (now - self.timer_at) / self.timer_period + 1;
                self.timer_at = self.timer_at.wrapping_add(late * self.timer_period);
            }
            _ => self.timer_at = 0,
        }
    }

    /// Accepts an edge-triggered interrupt (from a device, another
    /// processor or the APIC itself).
    pub fn request(&mut self, vector: u8) {
        if self.accept(vector) {
            clear(&mut self.tmr, vector);
        }
    }

    /// Accepts a level-triggered interrupt (a device's line): its
    /// end-of-interrupt is reported ([`Write::EndOfLevel`]).
    pub fn request_level(&mut self, vector: u8) {
        if self.accept(vector) {
            set(&mut self.tmr, vector);
        }
    }

    fn accept(&mut self, vector: u8) -> bool {
        if vector < 16 {
            self.errors |= ESR_RECEIVE_ILLEGAL;
            return false;
        }
        set(&mut self.irr, vector);
        true
    }

    /// The processor priority: the task priority, or the class of the
    /// highest interrupt in service if that is higher.
    fn ppr(&self) -> u32 {
        let isrv = highest(&self.isr).unwrap_or(0) as u32;
        if self.tpr & 0xF0 >= isrv & 0xF0 { self.tpr } else { isrv & 0xF0 }
    }

    /// The interrupt the processor would take now, if any: the highest
    /// requested one whose class is above the processor priority.
    pub fn pending(&self) -> Option<u8> {
        if !self.enabled() {
            return None;
        }
        let v = highest(&self.irr)?;
        (v as u32 & 0xF0 > self.ppr() & 0xF0).then_some(v)
    }

    /// The processor takes the pending interrupt: it moves from requested
    /// to in service.
    pub fn acknowledge(&mut self) -> Option<u8> {
        let v = self.pending()?;
        clear(&mut self.irr, v);
        set(&mut self.isr, v);
        Some(v)
    }

    /// End of interrupt: the highest interrupt in service is done. Its
    /// vector if it was level-triggered.
    fn eoi(&mut self) -> Option<u8> {
        let v = highest(&self.isr)?;
        clear(&mut self.isr, v);
        if !is_set(&self.tmr, v) {
            return None;
        }
        // The trigger mode is a new request's, if one came meanwhile.
        if !is_set(&self.irr, v) {
            clear(&mut self.tmr, v);
        }
        Some(v)
    }
}

fn set(bits: &mut [u32; 8], v: u8) {
    bits[v as usize / 32] |= 1 << (v % 32);
}

fn clear(bits: &mut [u32; 8], v: u8) {
    bits[v as usize / 32] &= !(1 << (v % 32));
}

fn is_set(bits: &[u32; 8], v: u8) -> bool {
    bits[v as usize / 32] & (1 << (v % 32)) != 0
}

fn highest(bits: &[u32; 8]) -> Option<u8> {
    bits.iter().enumerate().rev().find(|(_, w)| **w != 0).map(|(i, w)| (i * 32 + 31 - w.leading_zeros() as usize) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(id: u32) -> Lapic {
        let mut a = Lapic::new(id);
        assert_eq!(a.write(SVR, 0x1FF, 0), Write::Done);
        a
    }

    #[test]
    fn identity() {
        let a = Lapic::new(0x23);
        assert_eq!(a.read(ID, 0), Some(0x23));
        // Cluster 2, member 3.
        assert_eq!(a.read(LDR, 0), Some(0x2_0008));
        assert_eq!(a.read(VERSION, 0), Some(0x5_0014));
        // xAPIC-only registers and write-only ones cannot be read.
        assert_eq!(a.read(0x809, 0), None);
        assert_eq!(a.read(EOI, 0), None);
        assert_eq!(a.read(SELF_IPI, 0), None);
    }

    #[test]
    fn disabled_apic_takes_no_interrupts() {
        let mut a = Lapic::new(0);
        a.request(0x40);
        assert_eq!(a.pending(), None);
        assert_eq!(a.write(SVR, 0x1FF, 0), Write::Done);
        assert_eq!(a.pending(), Some(0x40));
    }

    #[test]
    fn priorities() {
        let mut a = enabled(0);
        a.request(0x41);
        a.request(0x62);
        a.request(0x30);
        assert_eq!(a.acknowledge(), Some(0x62));
        // 0x41 is of a lower class than 0x62, in service.
        assert_eq!(a.pending(), None);
        assert_eq!(a.read(PPR, 0), Some(0x60));
        assert_eq!(a.write(EOI, 0, 0), Write::Done);
        assert_eq!(a.acknowledge(), Some(0x41));
        // A task priority holds back interrupts of its class and below.
        assert_eq!(a.write(EOI, 0, 0), Write::Done);
        assert_eq!(a.write(TPR, 0x30, 0), Write::Done);
        assert_eq!(a.pending(), None);
        assert_eq!(a.write(TPR, 0x20, 0), Write::Done);
        assert_eq!(a.acknowledge(), Some(0x30));
        assert_eq!(a.read(ISR + 1, 0), Some(1 << 16));
        assert_eq!(a.write(EOI, 0, 0), Write::Done);
        assert_eq!(a.read(ISR + 1, 0), Some(0));
        // EOI takes no value but 0.
        assert_eq!(a.write(EOI, 1, 0), Write::Refused);
    }

    #[test]
    fn level_triggered_interrupts_report_their_end() {
        let mut a = enabled(0);
        a.request_level(0x51);
        a.request(0x40);
        assert_eq!(a.read(TMR + 2, 0), Some(1 << 17));
        assert_eq!(a.acknowledge(), Some(0x51));
        // Ending it says so, once; an edge's end says nothing.
        assert_eq!(a.write(EOI, 0, 0), Write::EndOfLevel(0x51));
        assert_eq!(a.read(TMR + 2, 0), Some(0));
        assert_eq!(a.acknowledge(), Some(0x40));
        assert_eq!(a.write(EOI, 0, 0), Write::Done);
        assert_eq!(a.write(EOI, 0, 0), Write::Done);
        // An edge on a vector that was level makes it an edge again.
        a.request_level(0x60);
        a.request(0x60);
        assert_eq!(a.acknowledge(), Some(0x60));
        assert_eq!(a.write(EOI, 0, 0), Write::Done);
    }

    #[test]
    fn illegal_vectors_are_reported() {
        let mut a = enabled(0);
        a.request(5);
        assert_eq!(a.pending(), None);
        assert_eq!(a.write(ICR, 0x0000_0001_0000_000A, 0), Write::Done);
        // The error register shows what happened once written.
        assert_eq!(a.read(ESR, 0), Some(0));
        assert_eq!(a.write(ESR, 0, 0), Write::Done);
        assert_eq!(a.read(ESR, 0), Some((ESR_SEND_ILLEGAL | ESR_RECEIVE_ILLEGAL) as u64));
        assert_eq!(a.write(ESR, 0, 0), Write::Done);
        assert_eq!(a.read(ESR, 0), Some(0));
    }

    #[test]
    fn inter_processor_interrupts() {
        let mut a = enabled(1);
        let ipi = |a: &mut Lapic, v: u64| match a.write(ICR, v, 0) {
            Write::Ipi(i) => i,
            w => panic!("{w:?}"),
        };
        assert_eq!(
            ipi(&mut a, 0x0000_0003_0000_00F0),
            Ipi { delivery: Delivery::Fixed(0xF0), destination: Destination::Physical(3) }
        );
        assert_eq!(
            ipi(&mut a, 0x000C_00FD),
            Ipi { delivery: Delivery::Fixed(0xFD), destination: Destination::AllButSelf }
        );
        assert_eq!(ipi(&mut a, 0x0008_0400), Ipi { delivery: Delivery::Nmi, destination: Destination::All });
        assert_eq!(
            ipi(&mut a, 0x0001_0003_0000_0800 | 0x31),
            Ipi { delivery: Delivery::Fixed(0x31), destination: Destination::Logical(0x1_0003) }
        );
        assert_eq!(ipi(&mut a, 0x0000_0002_0000_0500).delivery, Delivery::Unsupported(5));
        assert_eq!(a.read(ICR, 0), Some(0x0000_0002_0000_0500));
        // Self IPIs land at once.
        assert_eq!(a.write(SELF_IPI, 0x55, 0), Write::Done);
        assert_eq!(a.pending(), Some(0x55));
    }

    #[test]
    fn destinations() {
        let a = Lapic::new(0x13);
        assert!(a.is_destination(Destination::Physical(0x13), false));
        assert!(a.is_destination(Destination::Physical(u32::MAX), false));
        assert!(!a.is_destination(Destination::Physical(0x12), false));
        // Cluster 1, member 3.
        assert!(a.is_destination(Destination::Logical(0x1_0008), false));
        assert!(!a.is_destination(Destination::Logical(0x2_0008), false));
        assert!(!a.is_destination(Destination::Logical(0x1_0004), false));
        assert!(a.is_destination(Destination::AllButSelf, false));
        assert!(!a.is_destination(Destination::AllButSelf, true));
        assert!(a.is_destination(Destination::SelfOnly, true));
    }

    #[test]
    fn one_shot_timer() {
        let mut a = enabled(0);
        // Vector 0xEC, one-shot, divide by 16.
        assert_eq!(a.write(LVT_TIMER, 0xEC, 0), Write::Done);
        assert_eq!(a.write(TIMER_DIVIDE, 3, 0), Write::Done);
        assert_eq!(a.write(TIMER_INITIAL, 100, 1000), Write::Done);
        assert_eq!(a.timer_deadline(), Some(1000 + 1600));
        assert_eq!(a.read(TIMER_CURRENT, 1800), Some(50));
        a.expire_timer(2599);
        assert_eq!(a.pending(), None);
        a.expire_timer(2600);
        assert_eq!(a.pending(), Some(0xEC));
        assert_eq!(a.timer_deadline(), None);
        assert_eq!(a.read(TIMER_CURRENT, 3000), Some(0));
    }

    #[test]
    fn periodic_timer_keeps_its_rhythm() {
        let mut a = enabled(0);
        assert_eq!(a.write(TIMER_DIVIDE, 0xB, 0), Write::Done);
        assert_eq!(a.write(LVT_TIMER, (1 << 17) | 0xEC, 0), Write::Done);
        assert_eq!(a.write(TIMER_INITIAL, 1000, 0), Write::Done);
        a.expire_timer(1003);
        assert_eq!(a.timer_deadline(), Some(2000));
        // Late by more than a period: the next one after now.
        a.expire_timer(4500);
        assert_eq!(a.timer_deadline(), Some(5000));
    }

    #[test]
    fn deadline_timer() {
        let mut a = enabled(0);
        // Not in deadline mode: the deadline is ignored.
        assert_eq!(a.write(MSR_TSC_DEADLINE, 500, 0), Write::Done);
        assert_eq!(a.timer_deadline(), None);
        assert_eq!(a.write(LVT_TIMER, (2 << 17) | 0xEC, 0), Write::Done);
        assert_eq!(a.write(MSR_TSC_DEADLINE, 500, 0), Write::Done);
        assert_eq!(a.read(MSR_TSC_DEADLINE, 0), Some(500));
        // The initial count does nothing in this mode.
        assert_eq!(a.write(TIMER_INITIAL, 7, 0), Write::Done);
        assert_eq!(a.timer_deadline(), Some(500));
        a.expire_timer(600);
        assert_eq!(a.pending(), Some(0xEC));
        assert_eq!(a.read(MSR_TSC_DEADLINE, 0), Some(0));
        // Writing 0 disarms; changing mode too.
        assert_eq!(a.write(MSR_TSC_DEADLINE, 900, 0), Write::Done);
        assert_eq!(a.write(MSR_TSC_DEADLINE, 0, 0), Write::Done);
        assert_eq!(a.timer_deadline(), None);
        assert_eq!(a.write(MSR_TSC_DEADLINE, 900, 0), Write::Done);
        assert_eq!(a.write(LVT_TIMER, 0xEC, 0), Write::Done);
        assert_eq!(a.timer_deadline(), None);
    }

    #[test]
    fn masked_timer_fires_quietly() {
        let mut a = enabled(0);
        assert_eq!(a.write(LVT_TIMER, (LVT_MASKED | 0xEC) as u64, 0), Write::Done);
        assert_eq!(a.write(TIMER_INITIAL, 10, 0), Write::Done);
        a.expire_timer(100);
        assert_eq!(a.pending(), None);
        assert_eq!(a.timer_deadline(), None);
    }

    #[test]
    fn software_disable_masks_local_interrupts() {
        let mut a = enabled(0);
        assert_eq!(a.write(LVT_TIMER, 0xEC, 0), Write::Done);
        assert_eq!(a.write(SVR, 0xFF, 0), Write::Done);
        assert_eq!(a.read(LVT_TIMER, 0), Some((LVT_MASKED | 0xEC) as u64));
        assert_eq!(a.write(LVT_TIMER, 0xEC, 0), Write::Done);
        assert_eq!(a.read(LVT_TIMER, 0), Some((LVT_MASKED | 0xEC) as u64));
    }

    #[test]
    fn reserved_bits_are_refused() {
        let mut a = enabled(0);
        assert_eq!(a.write(TPR, 0x100, 0), Write::Refused);
        assert_eq!(a.write(SVR, 1 << 12, 0), Write::Refused);
        assert_eq!(a.write(TIMER_DIVIDE, 4, 0), Write::Refused);
        assert_eq!(a.write(LVT_TIMER, 1 << 32, 0), Write::Refused);
        assert_eq!(a.write(SELF_IPI, 0x100, 0), Write::Refused);
        assert_eq!(a.write(ID, 0, 0), Write::Refused);
        assert_eq!(a.write(0x831, 0, 0), Write::Refused);
    }
}
