//! The display driver's flip loop (`vigpu::scanout`) on the simulated
//! display engine, time passing as the tests say: requests and the
//! vertical blanks they are carried out at, the interrupts that wake the
//! driver (or do not), a screen left still, and faults.

use std::cell::Cell;

use vigpu::display::Readout;
use vigpu::flip::{Flipper, Lines, Vsync};
use vigpu::gtt::{Gtt, SEARCH_FROM};
use vigpu::irq::{self, Cause};
use vigpu::regs::{self, Pipe};
use vigpu::scanout::{self, Clock, Done, Flips, Note, Scanout};

use crate::igpu::{BAR_SIZE, DisplayEngine, GMCH_CTRL, Gop};

const A: Pipe = Pipe(0);
const PERIOD: u64 = 16_666_667;

/// Time that passes a microsecond each time it is read (so that the
/// driver's short waits end), and as much as a test says.
struct TestClock(Cell<u64>);

impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.set(self.0.get() + 1_000);
        self.0.get()
    }
}

impl TestClock {
    fn pass(&self, ns: u64) {
        self.0.set(self.0.get() + ns);
    }
}

/// The Zenbook's display engine with the driver started as `intel-gpu`
/// starts: the picture taken over, two pictures mapped, interrupts reset;
/// and a compositor newly attached.
struct Rig {
    engine: DisplayEngine,
    scanout: Scanout,
    flips: Flips,
    clock: TestClock,
}

impl Rig {
    /// With interrupts (MSI) or without.
    fn new(interrupts: bool) -> Rig {
        // The Zenbook as its firmware leaves it (see `display_tests`).
        let engine = DisplayEngine::new(4, &Gop { scratch: true, ..Gop::plain(1920, 1080, (3840, 2400)) });
        let readout = Readout::read(&engine, 4);
        let takeover = readout.takeover().unwrap();
        let gtt = Gtt::new(BAR_SIZE, GMCH_CTRL).unwrap();
        let bytes = takeover.picture_bytes();
        let mut busy = readout.scanned_out();
        let mut surfaces = Vec::new();
        for i in 0..2u64 {
            let (at, _) = gtt.find_room(&engine, SEARCH_FROM, bytes, regs::SURFACE_ALIGN, &busy).unwrap();
            gtt.map(&engine, at, 0x1_0000_0000 + i * 0x400_0000, bytes);
            busy.push((at, bytes));
            surfaces.push(at as u32);
        }
        let pipe = takeover.pipes[0];
        let lines = Lines::read(&engine, pipe);
        irq::reset(&engine, pipe);
        let scanout = Scanout {
            flipper: Flipper::new(takeover, surfaces),
            pipe,
            lines,
            vsync: Vsync::new(PERIOD),
            counting: true,
            polling: !interrupts,
        };
        let flips = Flips::new(&scanout, &engine);
        Rig { engine, scanout, flips, clock: TestClock(Cell::new(1_000_000_000)) }
    }

    /// The compositor asks for `picture` as request `seq`, a quarter of a
    /// frame after the last blank. What the driver reports right away.
    fn request(&mut self, picture: u32, seq: u32) -> Option<Done> {
        self.clock.pass(PERIOD / 4);
        self.flips.request(&mut self.scanout, &self.engine, picture, seq);
        self.flips.look(&mut self.scanout, &self.engine, &self.clock)
    }

    /// The rest of a frame passes, and the next vertical blank begins: its
    /// interrupt (if the GPU raises one) wakes the driver, which looks. What
    /// it reports.
    fn blank(&mut self) -> Option<Done> {
        self.clock.pass(PERIOD * 3 / 4);
        self.engine.vblank(A);
        if self.engine.take_interrupt() {
            let cause = irq::handle(&self.engine, A);
            self.flips.interrupted(&mut self.scanout, &self.engine, cause, &self.clock);
        }
        self.flips.look(&mut self.scanout, &self.engine, &self.clock)
    }

    /// As [`Rig::blank`], but the interrupt never reaches the driver: its
    /// deadline wakes it, a moment after the blank.
    fn blank_unheard(&mut self) -> Option<Done> {
        self.clock.pass(PERIOD * 3 / 4);
        self.engine.vblank(A);
        self.engine.take_interrupt();
        self.clock.pass(scanout::MISSED_IRQ_NS);
        self.flips.look(&mut self.scanout, &self.engine, &self.clock)
    }

    fn surface(&self, picture: usize) -> u32 {
        self.scanout.flipper.surfaces()[picture]
    }
}

#[test]
fn reports_each_flip_at_its_vertical_blank() {
    let mut rig = Rig::new(true);
    assert_eq!(rig.request(0, 1), None, "not before the blank");
    assert!(rig.flips.listening());
    let first = rig.blank().expect("carried out at the blank");
    assert_eq!(first.seq, 1);
    assert_eq!(rig.engine.live(A), rig.surface(0));
    assert_eq!(rig.request(1, 2), None);
    let second = rig.blank().expect("and the next at the next");
    assert_eq!((second.seq, second.frames), (2, first.frames + 1));
    assert_eq!(rig.engine.live(A), rig.surface(1));
    // A frame apart, give or take the microseconds the driver took.
    assert!((second.blank_ns - first.blank_ns).abs_diff(PERIOD) < 50_000);
    assert!(second.period_ns.abs_diff(PERIOD) < 50_000);
    assert_eq!(rig.flips.take_notes(), vec![]);
    assert_eq!(rig.engine.notes(), Vec::<String>::new());
}

#[test]
fn waits_for_registers_that_take_the_picture_late() {
    // The surface registers show the new picture a moment after the
    // blank's interrupt: the flip is still reported at that blank, not a
    // frame later.
    let mut rig = Rig::new(true);
    rig.engine.delay_latch(20);
    rig.request(0, 1);
    assert_eq!(rig.blank().map(|d| d.seq), Some(1));
}

#[test]
fn tells_when_the_blank_began_however_late_it_looks() {
    let mut rig = Rig::new(false);
    rig.request(0, 1);
    rig.clock.pass(PERIOD * 3 / 4);
    let began = rig.clock.0.get();
    rig.engine.vblank(A);
    // The driver looks 541 lines (the blank's 40, and 501 of the next
    // frame) later.
    let late = 541 * PERIOD / 2440;
    rig.clock.pass(late);
    rig.engine.set_line(A, 500);
    let done = rig.flips.look(&mut rig.scanout, &rig.engine, &rig.clock).expect("carried out");
    assert!(done.blank_ns.abs_diff(began) < 50_000, "{} ns off", done.blank_ns.abs_diff(began));
}

#[test]
fn stops_listening_while_the_screen_is_still() {
    let mut rig = Rig::new(true);
    rig.request(0, 1);
    rig.blank().unwrap();
    // A blank or two more, in case the next frame comes; then quiet.
    assert_eq!(rig.blank(), None);
    assert!(rig.flips.listening());
    assert_eq!(rig.blank(), None);
    assert!(!rig.flips.listening());
    rig.engine.vblank(A);
    assert!(!rig.engine.take_interrupt(), "no interrupt from a still screen");
    assert_eq!(rig.flips.deadline(&rig.scanout, rig.clock.now()), None);
    // The next frame: listening again, flipped at the next blank.
    assert_eq!(rig.request(1, 2), None);
    assert!(rig.flips.listening());
    assert_eq!(rig.blank().map(|d| d.seq), Some(2));
}

#[test]
fn watches_the_frame_counter_when_no_interrupt_comes() {
    // The interrupts are asked for but never come: each flip is still
    // reported (the driver's deadline wakes it after each blank), and
    // after a few blanks it stops waiting for them.
    let mut rig = Rig::new(true);
    for seq in 1..=scanout::SILENT_BLANKS {
        rig.request(seq % 2, seq);
        assert_eq!(rig.blank_unheard().map(|d| d.seq), Some(seq));
    }
    assert_eq!(rig.flips.take_notes(), vec![Note::Polling("no vertical blank interrupt comes")]);
    assert!(rig.scanout.polling && !rig.flips.listening());
    // From then on it looks every millisecond while a flip waits.
    rig.request(1, 100);
    let now = rig.clock.now();
    assert_eq!(rig.flips.deadline(&rig.scanout, now), Some(now + scanout::POLL_NS));
    assert_eq!(rig.blank_unheard().map(|d| d.seq), Some(100));
}

#[test]
fn stops_listening_to_a_gpu_that_keeps_interrupting() {
    let mut rig = Rig::new(true);
    rig.request(0, 1);
    let other = Cause { other: true, ..Cause::default() };
    for _ in 0..=scanout::STRAYS_PER_SECOND {
        rig.flips.interrupted(&mut rig.scanout, &rig.engine, other, &rig.clock);
    }
    assert_eq!(rig.flips.take_notes(), vec![Note::Polling("the GPU keeps interrupting for something else")]);
    assert!(rig.scanout.polling);
    // The flip still comes.
    assert_eq!(rig.blank().map(|d| d.seq), Some(1));
}

#[test]
fn gives_up_when_plane_1_cannot_read_its_picture() {
    let mut rig = Rig::new(true);
    rig.request(0, 1);
    rig.engine.event(A, regs::PIPE_PLANE1_FAULT);
    rig.blank();
    assert_eq!(rig.flips.fatal(), Some("plane 1 read memory it could not reach"));
    // The screen goes back to the firmware's picture at the next blank.
    rig.scanout.give_back(&rig.engine);
    rig.engine.vblank(A);
    assert!(rig.scanout.given_back(&rig.engine));
    assert!(!rig.engine.take_interrupt());
}

#[test]
fn survives_an_underrun_now_and_then() {
    let mut rig = Rig::new(true);
    rig.request(0, 1);
    rig.engine.event(A, regs::PIPE_FIFO_UNDERRUN);
    rig.blank();
    assert_eq!((rig.flips.take_notes(), rig.flips.fatal()), (vec![Note::Underrun], None));
    rig.clock.pass(2_000_000_000);
    rig.engine.event(A, regs::PIPE_FIFO_UNDERRUN);
    rig.request(1, 2);
    assert_eq!(rig.flips.fatal(), None, "a second apart");
    // Three within a second: the screen is not to be trusted.
    for seq in 3..5 {
        rig.engine.event(A, regs::PIPE_FIFO_UNDERRUN);
        rig.blank();
        rig.request(seq % 2, seq);
    }
    assert_eq!(rig.flips.fatal(), Some("the pipe keeps running short of pixels"));
}
