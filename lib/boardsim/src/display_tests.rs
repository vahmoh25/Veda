//! The display driver's logic (`vigpu`) on a simulated Intel display
//! engine, as UEFI leaves it: what the Zenbook's firmware most likely
//! does for the live system (1920x1080, scaled to the 3840x2400 panel),
//! and the variations a driver must take over or leave alone.

use vigpu::Mmio;
use vigpu::display::{Readout, Refusal};
use vigpu::flip::{Flipper, Lines, Vsync};
use vigpu::gtt::{Gtt, SEARCH_FROM};
use vigpu::irq;
use vigpu::regs::{self, Pipe};

use crate::igpu::{BAR_SIZE, DisplayEngine, GMCH_CTRL, Gop};

const A: Pipe = Pipe(0);
const B: Pipe = Pipe(1);

/// As the Zenbook's firmware leaves it (its log, 2026-10-07): pipe A only,
/// 3840x2400, showing a 1920x1080 picture at the table's bottom, and no
/// unused entry in the table above it.
fn zenbook() -> Gop {
    Gop { scratch: true, ..Gop::plain(1920, 1080, (3840, 2400)) }
}

/// The driver's start, as `intel-gpu` does it: the picture taken over, two
/// pictures of its own mapped (at made-up physical addresses) clear of
/// everything the engine scans out, ready to flip.
fn started(engine: &DisplayEngine) -> (Flipper, Vec<u32>) {
    let readout = Readout::read(engine, 4);
    let takeover = readout.takeover().expect("a plain picture");
    let gtt = Gtt::new(BAR_SIZE, GMCH_CTRL).unwrap();
    let bytes = takeover.picture_bytes();
    let mut busy = readout.scanned_out();
    let mut surfaces = Vec::new();
    for i in 0..2u64 {
        let (at, _) = gtt.find_room(engine, SEARCH_FROM, bytes, regs::SURFACE_ALIGN, &busy).expect("room in the table");
        gtt.map(engine, at, 0x1_0000_0000 + i * 0x400_0000, bytes);
        busy.push((at, bytes));
        surfaces.push(at as u32);
    }
    (Flipper::new(takeover, surfaces.clone()), surfaces)
}

/// Whether two ranges (start, bytes) overlap.
fn overlap((a, n): (u64, u64), (b, m): (u64, u64)) -> bool {
    a < b + m && b < a + n
}

#[test]
fn takes_the_firmwares_picture_over() {
    let engine = DisplayEngine::new(4, &zenbook());
    let readout = Readout::read(&engine, 4);
    assert!(readout.pipes[0].scalers > 0, "the 1080p picture is scaled to the panel");
    let t = readout.takeover().unwrap();
    assert_eq!(
        (t.pipes.clone(), t.width, t.height, t.stride, t.rgbx, t.surface),
        (vec![A], 1920, 1080, 7680, false, 0)
    );
    assert_eq!(t.picture_bytes(), 7680 * 1080);
    // Panel self refresh changes nothing.
    let engine = DisplayEngine::new(4, &Gop { psr: true, ..zenbook() });
    assert!(Readout::read(&engine, 4).takeover().is_ok());
}

#[test]
fn takes_a_scaled_plane_over() {
    // Firmware may scale plane 1 rather than the pipe: then the pipe's
    // picture is the panel's size, and the plane's is the framebuffer's.
    let readout = Readout::read(&DisplayEngine::new(4, &Gop { plane_scaling: true, ..zenbook() }), 4);
    assert_eq!(readout.pipes[0].source, (3840, 2400));
    let t = readout.takeover().unwrap();
    assert_eq!((t.width, t.height, t.stride), (1920, 1080, 7680));
}

#[test]
fn leaves_alone_what_it_cannot_flip() {
    let refusal = |gop: Gop| Readout::read(&DisplayEngine::new(4, &gop), 4).takeover().unwrap_err();
    assert_eq!(refusal(Gop { tiling: 4, ..zenbook() }), Refusal::Tiled(A));
    assert_eq!(refusal(Gop { format: 2, ..zenbook() }), Refusal::Format(A, 2));
    assert_eq!(refusal(Gop { pipes: Vec::new(), ..zenbook() }), Refusal::NoPipe);
    assert_eq!(refusal(Gop { offset: (0, 8), ..zenbook() }), Refusal::Offset(A));
    // Two pipes showing different pictures.
    let engine = DisplayEngine::new(4, &Gop { pipes: vec![A, B], ..zenbook() });
    engine.write(regs::plane_surf(B, 1), 0x0400_0000);
    engine.vblank(B);
    assert_eq!(Readout::read(&engine, 4).takeover().unwrap_err(), Refusal::SeveralPictures);
}

#[test]
fn maps_its_pictures_clear_of_the_firmwares() {
    // A firmware that leaves unused entries, but also uses room where the
    // driver looks first.
    let gop = Gop { also_mapped: vec![(SEARCH_FROM, 16)], scratch: false, ..zenbook() };
    let engine = DisplayEngine::new(4, &gop);
    let (_, surfaces) = started(&engine);
    for &s in &surfaces {
        assert!(s as u64 >= SEARCH_FROM + 16 * 4096 && (s as u64).is_multiple_of(regs::SURFACE_ALIGN), "{s:#x}");
    }
    assert!(surfaces[1] as u64 >= surfaces[0] as u64 + 7680 * 1080, "the pictures do not overlap");
    assert_eq!(engine.mapped(surfaces[0] as u64 / 4096), Some(0x1_0000_0000));
    assert!(engine.flushes() >= 2);
    assert_eq!(engine.notes(), Vec::<String>::new());
}

#[test]
fn fills_a_table_the_firmware_filled() {
    // Every entry maps a scratch page, so none is unused: the pictures go
    // where nothing is scanned out, and the firmware's entries stay.
    let engine = DisplayEngine::new(4, &Gop { scratch: true, ..zenbook() });
    let gtt = Gtt::new(BAR_SIZE, GMCH_CTRL).unwrap();
    let readout = Readout::read(&engine, 4);
    let bytes = readout.takeover().unwrap().picture_bytes();
    let busy = readout.scanned_out();
    assert_eq!(gtt.find_room(&engine, SEARCH_FROM, bytes, regs::SURFACE_ALIGN, &busy), Some((SEARCH_FROM, false)));
    let (mut flipper, surfaces) = started(&engine);
    flipper.queue(&engine, 1, 1);
    engine.vblank(A);
    assert_eq!(flipper.completed(&engine), Some((1, 1)));
    assert_eq!(engine.live(A), surfaces[1]);
    assert_eq!(engine.notes(), Vec::<String>::new());
}

#[test]
fn keeps_clear_of_what_is_set_aside() {
    // Room set aside (the engines' contexts') is not taken where its
    // entries are unused either: nothing is mapped there yet.
    let engine = DisplayEngine::new(4, &Gop::plain(1920, 1080, (3840, 2400)));
    let gtt = Gtt::new(BAR_SIZE, GMCH_CTRL).unwrap();
    let readout = Readout::read(&engine, 4);
    let bytes = readout.takeover().unwrap().picture_bytes();
    let aside = (SEARCH_FROM, 64 << 20);
    let mut busy = readout.scanned_out();
    busy.push(aside);
    let found = gtt.find_room(&engine, SEARCH_FROM, bytes, regs::SURFACE_ALIGN, &busy);
    assert_eq!(found, Some((SEARCH_FROM + (64 << 20), true)));
}

#[test]
fn keeps_clear_of_what_is_scanned_out() {
    // The firmware's picture and a cursor where the driver looks first, in
    // a table without an unused entry.
    let cursor = (SEARCH_FROM + (16 << 20)) as u32;
    let gop = Gop { scratch: true, surface: SEARCH_FROM, cursor: Some(cursor), ..zenbook() };
    let engine = DisplayEngine::new(4, &gop);
    let busy = Readout::read(&engine, 4).scanned_out();
    let bytes = 7680 * 1080;
    assert_eq!(busy, vec![(SEARCH_FROM, bytes), (cursor as u64, regs::CURSOR_BYTES)]);
    let (_, surfaces) = started(&engine);
    for &s in &surfaces {
        for &range in &busy {
            assert!(!overlap((s as u64, bytes), range), "{s:#x} overlaps {range:x?}");
        }
    }
    assert!(!overlap((surfaces[0] as u64, bytes), (surfaces[1] as u64, bytes)));
    assert_eq!(engine.notes(), Vec::<String>::new());
}

#[test]
fn gives_the_screen_back() {
    // When the compositor goes away, the firmware's picture (which the next
    // one draws into until it attaches) is shown again.
    let engine = DisplayEngine::new(4, &zenbook());
    let (mut flipper, surfaces) = started(&engine);
    flipper.queue(&engine, 0, 1);
    engine.vblank(A);
    assert_eq!(flipper.completed(&engine), Some((0, 1)));
    assert_eq!(engine.live(A), surfaces[0]);
    flipper.queue(&engine, 1, 2);
    flipper.restore(&engine);
    assert_eq!((flipper.shown(), flipper.pending()), (None, false));
    engine.vblank(A);
    assert_eq!(engine.live(A), 0);
    assert_eq!(engine.notes(), Vec::<String>::new());
}

#[test]
fn flips_at_the_vertical_blank() {
    let engine = DisplayEngine::new(4, &zenbook());
    let (mut flipper, surfaces) = started(&engine);
    assert!(flipper.queue(&engine, 0, 1));
    // Not before the blank: the firmware's picture is still scanned out.
    assert_eq!(flipper.completed(&engine), None);
    assert_eq!(engine.live(A), 0);
    engine.vblank(A);
    assert_eq!(flipper.completed(&engine), Some((0, 1)));
    assert_eq!((engine.live(A), flipper.shown()), (surfaces[0], Some(0)));
    // A later request replaces one not yet carried out.
    assert!(flipper.queue(&engine, 1, 2));
    assert!(flipper.queue(&engine, 0, 3));
    engine.vblank(A);
    assert_eq!(flipper.completed(&engine), Some((0, 3)));
    assert!(!flipper.queue(&engine, 2, 4), "no third picture");
    // Only the surface registers changed, and only mapped memory was shown.
    assert_eq!(engine.notes(), Vec::<String>::new());
}

#[test]
fn clones_flip_together() {
    let engine = DisplayEngine::new(4, &Gop { pipes: vec![A, B], ..zenbook() });
    let (mut flipper, surfaces) = started(&engine);
    assert_eq!(flipper.takeover().pipes, vec![A, B]);
    flipper.queue(&engine, 1, 1);
    engine.vblank(A);
    engine.vblank(B);
    assert_eq!(flipper.completed(&engine), Some((1, 1)));
    assert_eq!((engine.live(A), engine.live(B)), (surfaces[1], surfaces[1]));
}

#[test]
fn the_simulation_notes_unmapped_pictures() {
    // The model's own check: a picture at an address nothing maps.
    let engine = DisplayEngine::new(4, &zenbook());
    let t = Readout::read(&engine, 4).takeover().unwrap();
    t.show(&engine, 0x2000_0000);
    engine.vblank(A);
    assert_eq!(engine.notes().len(), 1);
}

#[test]
fn interrupts_at_each_vertical_blank() {
    let engine = DisplayEngine::new(4, &zenbook());
    irq::reset(&engine, A);
    engine.vblank(A);
    assert!(!engine.take_interrupt(), "nothing until asked for");
    irq::enable_vblank(&engine, A);
    engine.vblank(A);
    assert!(engine.take_interrupt());
    let cause = irq::handle(&engine, A);
    assert_eq!((cause.vblank, cause.other), (true, false));
    assert_eq!(engine.read(regs::pipe_iir(A)), 0, "acknowledged");
    assert_eq!(engine.read(regs::MASTER_IRQ) & regs::MASTER_IRQ_ENABLE, regs::MASTER_IRQ_ENABLE);
    // Every blank, while enabled; none after.
    engine.vblank(A);
    assert!(engine.take_interrupt());
    irq::handle(&engine, A);
    irq::disable_vblank(&engine, A);
    engine.vblank(A);
    assert!(!engine.take_interrupt());
}

#[test]
fn notices_what_the_screen_could_not_show() {
    // Faults are latched, without interrupting, once the driver has reset
    // the interrupts: it finds them with the next vertical blank's
    // interrupt, or when it looks.
    let engine = DisplayEngine::new(4, &zenbook());
    irq::reset(&engine, A);
    let (mut flipper, _) = started(&engine);
    irq::enable_vblank(&engine, A);
    flipper.queue(&engine, 0, 1);
    engine.event(A, regs::PIPE_PLANE1_FAULT);
    assert!(!engine.take_interrupt(), "a fault alone does not interrupt");
    engine.vblank(A);
    assert!(engine.take_interrupt());
    let cause = irq::handle(&engine, A);
    assert_eq!((cause.vblank, cause.faults, cause.other), (true, regs::PIPE_PLANE1_FAULT, false));
    // Without vertical blank interrupts, latched all the same: found once.
    irq::disable_vblank(&engine, A);
    engine.event(A, regs::PIPE_FIFO_UNDERRUN);
    engine.vblank(A);
    assert!(!engine.take_interrupt());
    assert_eq!(irq::faults(&engine, A), regs::PIPE_FIFO_UNDERRUN);
    assert_eq!(irq::faults(&engine, A), 0);
}

#[test]
fn tells_when_the_blank_began() {
    let engine = DisplayEngine::new(4, &zenbook());
    let lines = Lines::read(&engine, A).unwrap();
    assert_eq!(lines, Lines { blank_start: 2400, total: 2440 });
    let (period, now) = (16_666_667, 1_000_000_000);
    // As the blank begins, the counter reads the line before it.
    assert_eq!(lines.blank_before(2399, now, period), now);
    assert_eq!(lines.blank_before(2409, now, period), now - 10 * period / 2440);
    // Halfway down the next frame: the blank's 40 lines, then 1200.
    assert_eq!(lines.blank_before(1199, now, period), now - 1240 * period / 2440);
    // A line beyond the frame tells nothing.
    assert_eq!(lines.blank_before(5000, now, period), now);
}

#[test]
fn learns_the_screens_rhythm() {
    let mut v = Vsync::default();
    let period = 16_666_667;
    for (i, frame) in (100u32..110).enumerate() {
        v.blank(frame, 1_000_000_000 + i as u64 * period);
    }
    assert!((v.period() as i64 - period as i64).abs() < 1000, "{}", v.period());
    // Blanks missed while nothing listened are counted out; a reading
    // after a long silence teaches nothing.
    v.blank(112, 1_000_000_000 + 12 * period);
    assert!((v.period() as i64 - period as i64).abs() < 1000);
    v.blank(500, 9_000_000_000);
    assert!((v.period() as i64 - period as i64).abs() < 1000);
    assert_eq!(v.frames(), Some(500));
}
