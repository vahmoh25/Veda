//! The driver's flip loop, without the waiting: what it does when the
//! compositor asks for a picture, when the GPU interrupts, and whenever it
//! looks at the screen. The driver waits for those (a request, the GPU's
//! interrupt, a deadline) and calls in; what is to be reported and logged
//! comes back. Time comes from a [`Clock`], so that host tests run the loop
//! against a simulated display engine.
//!
//! The display engine interrupts at each vertical blank while flips come
//! (and a little longer, so that a steady stream of frames does not turn
//! interrupts off and on), and not while the screen is still. A flip is
//! carried out at the first vertical blank after its request; that is when
//! the driver looks for it, and it reports it with the time the blank
//! began, from the line being scanned out. Without interrupts (none come,
//! or the GPU keeps raising others) the driver watches the frame counter.
//! Faults that show the screen cannot read the driver's pictures end it.

use alloc::vec::Vec;

use crate::Mmio;
use crate::flip::{Flipper, Lines, Vsync};
use crate::irq::{self, Cause};
use crate::regs::{self, Pipe};

/// After a vertical blank began, how long the surface registers may take
/// to show the new picture (and how long the driver waits for the line
/// counter to get past the line before a blank).
pub const SETTLE_NS: u64 = 200_000;
/// Vertical blank interrupts stay on for this many blanks after the last
/// flip.
pub const LINGER_BLANKS: u32 = 2;
/// With interrupts on, how long after a vertical blank was due the driver
/// looks anyway (in case its interrupt went missing).
pub const MISSED_IRQ_NS: u64 = 2_000_000;
/// Blanks that pass without an interrupt, before any came, until the
/// driver stops waiting for interrupts and watches the frame counter.
pub const SILENT_BLANKS: u32 = 8;
/// Interrupts in a second for something other than a vertical blank: more,
/// and the driver turns the GPU's interrupts off and watches the frame
/// counter.
pub const STRAYS_PER_SECOND: u32 = 200;
/// FIFO underruns in a second that make the driver give the screen back.
pub const UNDERRUNS_PER_SECOND: u32 = 3;
/// Without interrupts, how often the frame counter is looked at while a
/// flip waits.
pub const POLL_NS: u64 = 1_000_000;
/// The period assumed while none is known: 60 frames a second.
pub const DEFAULT_PERIOD_NS: u64 = 16_666_667;
const SECOND: u64 = 1_000_000_000;

/// Monotonic time, in nanoseconds.
pub trait Clock {
    fn now(&self) -> u64;
}

/// A flip carried out, to report to the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done {
    /// The request's number.
    pub seq: u32,
    /// The frame counter at the vertical blank it happened at, and when
    /// that blank began.
    pub frames: u32,
    pub blank_ns: u64,
    /// The time between two blanks (ns).
    pub period_ns: u64,
}

/// What the driver logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// Interrupts are off for good; the frame counter is watched (why).
    Polling(&'static str),
    /// The pipe ran short of pixels.
    Underrun,
    /// The compositor asked for a picture there is not.
    NoSuchPicture(u32),
}

/// The screen the driver took over: its pictures, its rhythm, and how the
/// driver learns of its vertical blanks.
pub struct Scanout {
    pub flipper: Flipper,
    /// The pipe that paces the flips, and its lines.
    pub pipe: Pipe,
    pub lines: Option<Lines>,
    pub vsync: Vsync,
    /// The frame counter counts (it does not on a panel that refreshes
    /// itself, say).
    pub counting: bool,
    /// No interrupts (none to be had, or the driver stopped listening for
    /// them): the frame counter is watched.
    pub polling: bool,
}

impl Scanout {
    /// The frame counter, and when that frame's vertical blank began: from
    /// the line being scanned out, so that it does not matter how late the
    /// driver looks.
    pub fn sample(&self, mmio: &impl Mmio, clock: &impl Clock) -> (u32, u64) {
        let takeover = self.flipper.takeover();
        let give_up = clock.now() + SETTLE_NS;
        loop {
            let frames = takeover.frames(mmio);
            let line = mmio.read(regs::scanline(self.pipe)) & regs::SCANLINE_MASK;
            let now = clock.now();
            // A blank began between the readings, or is about to (on the
            // last line before it the two counters might not agree on the
            // frame): again, a moment later.
            let edge = self.lines.is_some_and(|l| line + 2 == l.blank_start);
            if takeover.frames(mmio) == frames && !edge {
                let blank = self.lines.map_or(now, |l| l.blank_before(line, now, self.vsync.period()));
                return (frames, blank);
            }
            if now >= give_up {
                return (frames, now);
            }
            core::hint::spin_loop();
        }
    }

    /// Right after a vertical blank began: whether the waiting flip
    /// happened at it. The surface registers may take the new picture a
    /// moment after the blank's interrupt, so they are looked at for a
    /// little while.
    pub fn settle(&mut self, mmio: &impl Mmio, clock: &impl Clock) -> Option<(usize, u32)> {
        let until = clock.now() + SETTLE_NS;
        loop {
            if let Some(done) = self.flipper.completed(mmio) {
                return Some(done);
            }
            if clock.now() >= until {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// Stops vertical blank interrupts and shows the firmware's picture
    /// again, from the next vertical blank ([`Scanout::given_back`] says
    /// when it is).
    pub fn give_back(&mut self, mmio: &impl Mmio) {
        irq::disable_vblank(mmio, self.pipe);
        self.flipper.restore(mmio);
    }

    /// Whether the screen shows the firmware's picture.
    pub fn given_back(&self, mmio: &impl Mmio) -> bool {
        let takeover = self.flipper.takeover();
        takeover.shows(mmio, takeover.surface)
    }
}

/// One compositor's flips: what the loop knows between wake-ups.
pub struct Flips {
    /// The last request taken.
    seq: u32,
    /// The frame counter just before the waiting flip was asked for, and
    /// when last looked at.
    asked_at: u32,
    frames: u32,
    /// Vertical blank interrupts are on; blanks since a flip last waited.
    listening: bool,
    idle: u32,
    /// A vertical blank's interrupt came since the last look; how many
    /// came in all; blanks that passed without one.
    blank_irq: bool,
    vblanks: u64,
    silent: u32,
    /// Interrupts for something else, and FIFO underruns, in the second
    /// since each began to be counted.
    strays: (u32, u64),
    underruns: (u32, u64),
    /// Why the display engine cannot show the driver's pictures, once it is
    /// clear that it cannot.
    fatal: Option<&'static str>,
    notes: Vec<Note>,
}

/// Counts an event in `counter` (count, since when), a new count each
/// second; the count.
fn count(counter: &mut (u32, u64), now: u64) -> u32 {
    if now.saturating_sub(counter.1) > SECOND {
        *counter = (0, now);
    }
    counter.0 += 1;
    counter.0
}

impl Flips {
    /// Flips for a newly attached compositor.
    pub fn new(scanout: &Scanout, mmio: &impl Mmio) -> Flips {
        let frames = scanout.flipper.takeover().frames(mmio);
        Flips {
            seq: 0,
            asked_at: frames,
            frames,
            listening: false,
            idle: 0,
            blank_irq: false,
            vblanks: 0,
            silent: 0,
            strays: (0, 0),
            underruns: (0, 0),
            fatal: None,
            notes: Vec::new(),
        }
    }

    /// Vertical blank interrupts are on.
    pub fn listening(&self) -> bool {
        self.listening
    }

    /// Why the display engine cannot show the driver's pictures, if it is
    /// clear that it cannot: the driver is to give the screen back.
    pub fn fatal(&self) -> Option<&'static str> {
        self.fatal
    }

    /// What happened that the driver logs, since it last asked.
    pub fn take_notes(&mut self) -> Vec<Note> {
        core::mem::take(&mut self.notes)
    }

    /// When to look without being woken (`None`: not until woken).
    pub fn deadline(&self, scanout: &Scanout, now: u64) -> Option<u64> {
        if scanout.polling {
            return scanout.flipper.pending().then_some(now + POLL_NS);
        }
        if !self.listening {
            return None;
        }
        Some(scanout.vsync.next_after(now).unwrap_or(now + DEFAULT_PERIOD_NS) + MISSED_IRQ_NS)
    }

    /// Stops listening for interrupts, for good: the frame counter is
    /// watched instead.
    fn poll_instead(&mut self, scanout: &mut Scanout, mmio: &impl Mmio, why: &'static str) {
        irq::shut(mmio, scanout.pipe);
        scanout.polling = true;
        self.listening = false;
        self.notes.push(Note::Polling(why));
    }

    /// The pipe faulted ([`irq::FAULTS`] bits): plane 1 read memory it
    /// could not reach, which the screen showed as garbage; or the pipe ran
    /// short of pixels, which it survives once in a while but not again and
    /// again.
    fn fault(&mut self, bits: u32, now: u64) {
        if bits & regs::PIPE_PLANE1_FAULT != 0 {
            self.fatal = Some("plane 1 read memory it could not reach");
        }
        if bits & regs::PIPE_FIFO_UNDERRUN != 0 {
            self.notes.push(Note::Underrun);
            if count(&mut self.underruns, now) >= UNDERRUNS_PER_SECOND {
                self.fatal = Some("the pipe keeps running short of pixels");
            }
        }
    }

    /// The GPU interrupted (`cause`, from [`irq::handle`]).
    pub fn interrupted(&mut self, scanout: &mut Scanout, mmio: &impl Mmio, cause: Cause, clock: &impl Clock) {
        if cause.vblank {
            self.blank_irq = true;
            self.vblanks += 1;
            self.silent = 0;
        }
        if cause.faults != 0 {
            self.fault(cause.faults, clock.now());
        }
        if cause.other && count(&mut self.strays, clock.now()) > STRAYS_PER_SECOND && !scanout.polling {
            self.poll_instead(scanout, mmio, "the GPU keeps interrupting for something else");
        }
    }

    /// The compositor asked for `picture`, as request `seq`.
    pub fn request(&mut self, scanout: &mut Scanout, mmio: &impl Mmio, picture: u32, seq: u32) {
        if seq == self.seq {
            return;
        }
        self.seq = seq;
        let takeover_frames = |s: &Scanout| s.flipper.takeover().frames(mmio);
        // Interrupts first, so that no vertical blank after the flip goes
        // unnoticed; blanks they ought to have come for count from now.
        if !self.listening && !scanout.polling {
            irq::enable_vblank(mmio, scanout.pipe);
            self.listening = true;
            self.silent = 0;
            self.frames = takeover_frames(scanout);
        }
        self.idle = 0;
        self.asked_at = takeover_frames(scanout);
        if !scanout.flipper.queue(mmio, picture as usize, seq) {
            self.notes.push(Note::NoSuchPicture(picture));
        }
    }

    /// Looks at the screen: faults, vertical blanks that began, a flip
    /// carried out (returned, to report), interrupts that do not come or
    /// are no longer needed.
    pub fn look(&mut self, scanout: &mut Scanout, mmio: &impl Mmio, clock: &impl Clock) -> Option<Done> {
        let faults = irq::faults(mmio, scanout.pipe);
        if faults != 0 {
            self.fault(faults, clock.now());
        }
        let (frames, blank_ns) = scanout.sample(mmio, clock);
        let passed = frames.wrapping_sub(self.frames);
        let irq = core::mem::take(&mut self.blank_irq);
        if passed > 0 {
            self.frames = frames;
            scanout.vsync.blank(frames, blank_ns);
            if self.listening {
                if !irq {
                    self.silent = self.silent.saturating_add(passed);
                }
                if !scanout.flipper.pending() {
                    self.idle = self.idle.saturating_add(passed);
                }
            }
        }
        let mut done = None;
        // Without a frame counter, whenever the driver looks.
        if scanout.flipper.pending() && (frames != self.asked_at || !scanout.counting) {
            // The first blank since the request: the flip may be settling.
            let first = passed > 0 && frames == self.asked_at.wrapping_add(1);
            let completed = if first { scanout.settle(mmio, clock) } else { scanout.flipper.completed(mmio) };
            if let Some((_, seq)) = completed {
                done = Some(Done { seq, frames, blank_ns, period_ns: scanout.vsync.period() });
            }
        }
        if self.listening && self.vblanks == 0 && self.silent >= SILENT_BLANKS {
            self.poll_instead(scanout, mmio, "no vertical blank interrupt comes");
        }
        if self.listening && !scanout.flipper.pending() && self.idle >= LINGER_BLANKS {
            irq::disable_vblank(mmio, scanout.pipe);
            self.listening = false;
        }
        done
    }
}
