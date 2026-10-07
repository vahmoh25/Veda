//! `intel-gpu` — the driver for Intel's integrated graphics of Tiger Lake
//! to Raptor Lake (Iris Xe and UHD Graphics: display versions 12 and 13):
//! the display, and the engines that run what the renderer draws with.
//!
//! The two halves share the device and little else. The display flips in
//! a thread of its own, above normal programs; the engines serve `gem` in
//! the driver's first thread ([`engines`]). Each goes on if the other
//! cannot start or stops.
//!
//! ## The display
//!
//! The firmware lights the screen and leaves a picture on it, which the
//! compositor draws into. Drawing into the picture the screen is showing
//! tears, and frames paced by a timer drift against the screen's own
//! rhythm: what looks smooth in a virtual machine's window judders on a
//! laptop's panel. This driver keeps the firmware's mode and takes its
//! picture over (`vigpu`): it gives the compositor pictures of its own and
//! shows each, when asked, from the start of a vertical blank
//! (`vproto::displaydev`). So frames reach the screen whole, and at the
//! screen's pace.
//!
//! * **Takeover**: the display engine's state is read and logged. A picture
//!   that is not plain (tiled, rotated, in another format) stays the
//!   firmware's, and the compositor goes on drawing into it.
//! * **Pictures**: write-combining memory the display engine reads
//!   directly, mapped into the GPU's address table where nothing is
//!   scanned out.
//! * **Flips**: the display engine interrupts at each vertical blank while
//!   a flip waits (MSI); without interrupts the driver watches the frame
//!   counter. A completion says when the vertical blank it happened at
//!   began, from the line being scanned out.
//! * **Faults.** Should plane 1 fail to read the driver's pictures (a
//!   fault), or the pipe keep running short of pixels, the screen shows
//!   garbage: the driver shows the firmware's picture again and leaves the
//!   screen to it, and the compositor draws in place.
//! * If the compositor goes away, the firmware's picture, which a restarted
//!   compositor draws into, is shown again until that one attaches.
//!
//! No mode is set: the firmware's stays.

#![no_std]
#![no_main]

extern crate alloc;

mod engines;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use vabi::{WaitItem, signals};
use vigpu::Mmio;
use vigpu::device::{self, Platform};
use vigpu::display::{Readout, Takeover};
use vigpu::flip::{Flipper, Lines, Vsync};
use vigpu::gtt::{Gtt, SEARCH_FROM};
use vigpu::irq;
use vigpu::regs::{self, Pipe};
use vigpu::scanout::{Clock, DEFAULT_PERIOD_NS, Done, Flips, Note, Scanout};
use vproto::displaydev::{DisplayDevError, FlipState, Link, Screen, Shown, displaydev};
use vproto::pci::{DeviceInfo, pcidev};
use vrt::object::{Channel, Event, Interrupt, Resource, Vmo};
use vrt::println;
use vrt::time::{Duration, now_ns};
use vrt::vm::Mapping;

vrt::entry!(main);

const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
/// The subsystem's vendor and device ids in the configuration space.
const PCI_SUBSYSTEM: u16 = 0x2C;
/// The pictures the compositor draws into in turn.
const PICTURES: usize = 2;
/// The screen's period is measured over this many frames, each of which
/// may take this long to come.
const MEASURED_FRAMES: u32 = 8;
const FRAME_TIMEOUT_NS: u64 = 100_000_000;
/// How long the firmware's picture may take to come back.
const GIVE_BACK_NS: u64 = 100_000_000;

/// The GPU's first BAR: its registers, and the address table above them.
struct Registers {
    _map: Mapping,
    base: *mut u8,
    len: usize,
}

impl Registers {
    fn fits(&self, offset: u32, width: usize) -> bool {
        (offset as usize).is_multiple_of(width) && offset as usize + width <= self.len
    }
}

// SAFETY: registers, reached by volatile accesses of whole words, which
// the hardware orders. The display's thread and the engines' use their own
// (in the global table too: each maps its own range), and the mapping
// lives as long as the driver.
unsafe impl Send for Registers {}
unsafe impl Sync for Registers {}

impl Mmio for Registers {
    fn read(&self, offset: u32) -> u32 {
        // SAFETY: an aligned register inside the mapped BAR.
        if self.fits(offset, 4) { unsafe { read_volatile(self.base.add(offset as usize) as *const u32) } } else { !0 }
    }

    fn write(&self, offset: u32, value: u32) {
        if self.fits(offset, 4) {
            // SAFETY: as above.
            unsafe { write_volatile(self.base.add(offset as usize) as *mut u32, value) }
        }
    }

    fn read64(&self, offset: u32) -> u64 {
        // SAFETY: as above.
        if self.fits(offset, 8) { unsafe { read_volatile(self.base.add(offset as usize) as *const u64) } } else { !0 }
    }

    fn write64(&self, offset: u32, value: u64) {
        if self.fits(offset, 8) {
            // SAFETY: as above.
            unsafe { write_volatile(self.base.add(offset as usize) as *mut u64, value) }
        }
    }
}

/// The GPU as both halves of the driver reach it.
struct Device {
    regs: &'static Registers,
    /// "Intel Alder Lake-P".
    name: String,
    platform: Platform,
    info: DeviceInfo,
    /// The subsystem's vendor and device (the computer maker's).
    subsystem: (u16, u16),
    gtt: Gtt,
    /// For memory the GPU reaches.
    dma: Resource,
}

/// The GPU's display, its screen taken over.
struct Gpu {
    regs: &'static Registers,
    name: String,
    gtt: Gtt,
    /// The screen taken over, and the flips on it.
    scanout: Scanout,
    /// The pictures' memory, the driver's for as long as the display
    /// engine may show it.
    pictures: Vec<Vmo>,
    /// Where the firmware's picture is, as the processor reaches it.
    firmware: Vec<u64>,
    irq: Option<Interrupt>,
}

/// The system's monotonic clock.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        now_ns()
    }
}

fn map_registers(pci: &pcidev::Client) -> Result<Registers, String> {
    let Ok(Ok(vmo)) = pci.map_bar(0) else { return Err("cannot map the registers".into()) };
    let len = vmo.size().map_err(|_| "cannot map the registers")?;
    let map = Mapping::new(vmo, len, vabi::map_flags::READ | vabi::map_flags::WRITE)
        .map_err(|_| "cannot map the registers")?;
    Ok(Registers { base: map.as_ptr(), _map: map, len })
}

/// Frames a second, to two decimals, of a screen whose frames take
/// `period` ns.
fn rate(period: u64) -> String {
    let millihertz = 1_000_000_000_000 / period.max(1);
    format!("{}.{:02}", millihertz / 1000, millihertz % 1000 / 10)
}

/// Finds the GPU and maps its registers, its interrupts off: what both
/// halves start from. Also devmgr's channel for the device, open while the
/// driver runs, and the GPU's interrupt (MSI), the display's.
fn open() -> Result<(pcidev::Client, Device, Option<Interrupt>), String> {
    let h = vrt::env::take_handle(PCIDEV_ROLE).ok_or("no pcidev channel")?;
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let info = pci.info().map_err(|_| "devmgr went away")?;
    let location = format!("{:02x}:{:02x}.{}", info.bus, info.slot, info.function);
    let platform = device::platform(info.device).ok_or_else(|| {
        format!("{:04x}:{:04x} at {} is no GPU this driver knows", info.vendor, info.device, location)
    })?;
    let name = format!("Intel {}", platform.name);
    // The display engine reads the driver's pictures, and the engines
    // their memory, as bus masters.
    if !matches!(pci.enable(true), Ok(Ok(()))) {
        return Err("cannot enable the device".into());
    }
    let regs: &'static Registers = Box::leak(Box::new(map_registers(&pci)?));
    if regs.read(regs::transconf(Pipe(0))) == !0 {
        return Err(format!("{} at {} does not answer", name, location));
    }
    let gmch = match pci.config_read(regs::PCI_GMCH_CTRL, 2) {
        Ok(Ok(v)) => v as u16,
        _ => 0,
    };
    let gtt = Gtt::new(regs.len as u64, gmch)
        .ok_or_else(|| format!("no address table in a {} MiB BAR (GMCH control {:#06x})", regs.len >> 20, gmch))?;
    println!(
        "{} (display version {}) at {}, {:04x}:{:04x}: {} pipes, an address table for {} MiB",
        name,
        platform.display,
        location,
        info.vendor,
        info.device,
        platform.pipes,
        gtt.span() >> 20
    );
    let subsystem = match pci.config_read(PCI_SUBSYSTEM, 4) {
        Ok(Ok(v)) => (v as u16, (v >> 16) as u16),
        _ => (info.vendor, info.device),
    };
    let Ok(Ok(dma)) = pci.dma_resource() else { return Err("no DMA resource".into()) };
    // No interrupt from the GPU until a flip waits.
    regs.write(regs::MASTER_IRQ, 0);
    let irq = vproto::pci::enable_msi(&pci);
    Ok((pci, Device { regs, name, platform, info, subsystem, gtt, dma }, irq))
}

/// Takes the screen over: the firmware's picture, as `readout` found it,
/// and pictures of the driver's placed clear of `busy` (what is scanned
/// out, and the engines' room).
fn take_over(dev: &Device, readout: Readout, busy: Vec<(u64, u64)>, irq: Option<Interrupt>) -> Result<Gpu, String> {
    let regs = dev.regs;
    let takeover =
        readout.takeover().map_err(|why| format!("leaving the screen to the firmware's picture: {}", why))?;

    // The firmware's picture as the processor reaches it: through the
    // graphics aperture (the second BAR), or the memory itself.
    let aperture = dev.info.bars.iter().find(|b| b.index == 2 && !b.io).map(|b| b.address + takeover.surface as u64);
    let firmware: Vec<u64> = aperture.into_iter().chain(dev.gtt.translate(regs, takeover.surface as u64)).collect();
    let pipe = takeover.pipes[0];
    let lines = Lines::read(regs, pipe);
    let (vsync, counting) = match measure(regs, &takeover) {
        Some(vsync) => (vsync, true),
        None => (Vsync::new(DEFAULT_PERIOD_NS), false),
    };
    let bytes = takeover.picture_bytes();
    let (pictures, surfaces, unused) = place_pictures(regs, &dev.gtt, &dev.dma, bytes, busy)?;
    // No interrupt from the GPU until a flip waits.
    irq::reset(regs, pipe);

    let pipes: Vec<String> = takeover.pipes.iter().map(|p| format!("{}", p)).collect();
    println!(
        "taking the picture on {} over: {}x{}, {} bytes a row, {}, {} frames a second{}",
        pipes.join(" and "),
        takeover.width,
        takeover.height,
        takeover.stride,
        if takeover.rgbx { "RGBX" } else { "BGRX" },
        rate(vsync.period()),
        if counting { "" } else { " (assumed: the frame counter does not count)" }
    );
    let at: Vec<String> = firmware.iter().map(|a| format!("{:#x}", a)).collect();
    let surfaces_at: Vec<String> = surfaces.iter().map(|s| format!("{:#x}", s)).collect();
    println!(
        "the firmware's picture: {:#x} of the GPU's addresses, {} for the processor; the driver's: {} ({} KiB each{}); {}",
        takeover.surface,
        if at.is_empty() { String::from("unknown") } else { at.join(" or ") },
        surfaces_at.join(" and "),
        bytes >> 10,
        if unused { "" } else { ", over entries the firmware had filled" },
        if irq.is_some() { "interrupts by MSI" } else { "no MSI: watching the frame counter" }
    );
    let scanout =
        Scanout { flipper: Flipper::new(takeover, surfaces), pipe, lines, vsync, counting, polling: irq.is_none() };
    Ok(Gpu { regs, name: dev.name.clone(), gtt: dev.gtt, scanout, pictures, firmware, irq })
}

/// The screen's rhythm, from its frame counter over a few frames; `None`
/// if the counter does not count.
fn measure(regs: &Registers, takeover: &Takeover) -> Option<Vsync> {
    // The counter's next value after `from`, and when it was seen.
    let next = |from: u32| {
        let until = now_ns() + FRAME_TIMEOUT_NS;
        loop {
            let (frames, now) = (takeover.frames(regs), now_ns());
            if frames != from {
                return Some((frames, now));
            }
            if now >= until {
                return None;
            }
            vrt::time::sleep(Duration::from_micros(100));
        }
    };
    let (first, start) = next(takeover.frames(regs))?;
    let (mut frames, mut at) = (first, start);
    while frames.wrapping_sub(first) < MEASURED_FRAMES {
        (frames, at) = next(frames)?;
    }
    let period = (at - start) / frames.wrapping_sub(first) as u64;
    if !(4_000_000..=50_000_000).contains(&period) {
        return None;
    }
    let mut vsync = Vsync::new(period);
    vsync.blank(frames, at);
    Some(vsync)
}

/// Allocates one picture and finds room for it in the GPU's address space
/// clear of `busy`: its memory, physical address and place, and whether
/// the entries there were unused.
fn new_picture(
    regs: &Registers,
    gtt: &Gtt,
    dma: &Resource,
    bytes: u64,
    busy: &[(u64, u64)],
) -> Result<(Vmo, u64, u64, bool), String> {
    let vmo = Vmo::create_dma(dma, bytes as usize, vabi::dma_flags::WRITE_COMBINING)
        .map_err(|e| format!("no memory for a picture of {} KiB: {}", bytes >> 10, e))?;
    let phys = vmo.phys_addr(0).map_err(|e| format!("a picture's memory cannot be found: {}", e))?;
    let (at, unused) = gtt
        .find_room(regs, SEARCH_FROM, bytes, regs::SURFACE_ALIGN, busy)
        .ok_or("no room for a picture in the GPU's address space")?;
    Ok((vmo, phys, at, unused))
}

/// The driver's pictures, mapped where nothing is scanned out (`busy`):
/// their memory, their places, and whether all those entries were unused.
fn place_pictures(
    regs: &Registers,
    gtt: &Gtt,
    dma: &Resource,
    bytes: u64,
    mut busy: Vec<(u64, u64)>,
) -> Result<(Vec<Vmo>, Vec<u32>, bool), String> {
    let (mut pictures, mut surfaces, mut all_unused) = (Vec::new(), Vec::new(), true);
    for _ in 0..PICTURES {
        match new_picture(regs, gtt, dma, bytes, &busy) {
            Ok((vmo, phys, at, unused)) => {
                gtt.map(regs, at, phys, bytes);
                busy.push((at, bytes));
                pictures.push(vmo);
                surfaces.push(at as u32);
                all_unused &= unused;
            }
            Err(e) => {
                for &s in &surfaces {
                    gtt.unmap(regs, s as u64, bytes);
                }
                return Err(e);
            }
        }
    }
    Ok((pictures, surfaces, all_unused))
}

impl Gpu {
    /// Shows the firmware's picture again, and waits until the screen does.
    fn give_back(&mut self) {
        self.scanout.give_back(&self.regs);
        let until = now_ns() + GIVE_BACK_NS;
        while !self.scanout.given_back(&self.regs) && now_ns() < until {
            vrt::time::sleep(Duration::from_micros(500));
        }
    }

    /// Takes the pictures out of the GPU's address space (with the
    /// firmware's picture on the screen).
    fn unmap(&self) {
        let bytes = self.scanout.flipper.takeover().picture_bytes();
        for &s in self.scanout.flipper.surfaces() {
            self.gtt.unmap(&self.regs, s as u64, bytes);
        }
    }
}

/// An attachment to the compositor.
struct Session {
    link: Channel,
    state: FlipState,
    request: Event,
    done: Event,
}

/// Why the driver is not attached.
enum Attach {
    /// For good: the compositor will not take the screen.
    Refused(DisplayDevError),
    /// For now: no compositor yet, or it went away.
    Failed(&'static str),
}

fn attach(gpu: &Gpu) -> Result<Session, Attach> {
    let ch = vproto::connect(displaydev::NAME).map_err(|_| Attach::Failed("no registry"))?;
    let client = displaydev::Client::new(ch);
    let no_handles = |_| Attach::Failed("out of handles");
    let (state, state_vmo) = FlipState::create().map_err(no_handles)?;
    let (request, done) = (Event::create().map_err(no_handles)?, Event::create().map_err(no_handles)?);
    let dup = |e: &Event| e.0.duplicate(None).map(Event::from_handle).map_err(no_handles);
    let pictures = gpu
        .pictures
        .iter()
        .map(|p| p.0.duplicate(None).map(Vmo::from_handle))
        .collect::<Result<Vec<_>, _>>()
        .map_err(no_handles)?;
    let t = gpu.scanout.flipper.takeover();
    let screen = Screen {
        name: gpu.name.clone(),
        width: t.width,
        height: t.height,
        stride: t.stride,
        rgbx: t.rgbx,
        firmware: gpu.firmware.clone(),
        period_ns: gpu.scanout.vsync.period(),
    };
    let link = Link { pictures, state: state_vmo, request: dup(&request)?, done: dup(&done)? };
    match client.attach(screen, link) {
        Ok(Ok(())) => Ok(Session { link: client.into_channel(), state, request, done }),
        Ok(Err(e)) => Err(Attach::Refused(e)),
        Err(_) => Err(Attach::Failed("the compositor went away")),
    }
}

/// Why the driver stopped flipping for a compositor.
enum Ended {
    /// The compositor went away.
    Gone,
    /// The display engine cannot show the driver's pictures.
    Faulted(&'static str),
}

fn log(note: Note) {
    match note {
        Note::Polling(why) => println!("{}; watching the frame counter instead", why),
        Note::Underrun => println!("the pipe ran short of pixels (a FIFO underrun)"),
        Note::NoSuchPicture(p) => println!("the compositor asked for picture {}, which there is not", p),
    }
}

/// Flips for one compositor until it goes away, or the display engine
/// shows that it cannot show the driver's pictures.
fn serve(gpu: &mut Gpu, s: &Session) -> Ended {
    let clock = SystemClock;
    let mut flips = Flips::new(&gpu.scanout, &gpu.regs);
    loop {
        let irq = gpu.irq.as_ref().filter(|_| !gpu.scanout.polling).map(|i| i.raw());
        // An unused slot waits on the request event again.
        let mut items = [
            WaitItem { handle: s.link.raw(), signals: signals::PEER_CLOSED, ..Default::default() },
            WaitItem { handle: s.request.raw(), signals: signals::SIGNALED, ..Default::default() },
            WaitItem { handle: irq.unwrap_or(s.request.raw()), signals: signals::SIGNALED, ..Default::default() },
        ];
        let deadline = flips.deadline(&gpu.scanout, now_ns()).unwrap_or(vabi::DEADLINE_INFINITE);
        let _ = vrt::object::wait_many(&mut items, deadline);
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return Ended::Gone;
        }
        if irq.is_some() && items[2].observed & signals::SIGNALED != 0 {
            if let Some(irq) = &gpu.irq {
                let _ = irq.ack();
            }
            let cause = irq::handle(&gpu.regs, gpu.scanout.pipe);
            flips.interrupted(&mut gpu.scanout, &gpu.regs, cause, &clock);
        }
        if items[1].observed & signals::SIGNALED != 0 {
            let _ = s.request.clear();
            let (picture, seq) = s.state.queued();
            flips.request(&mut gpu.scanout, &gpu.regs, picture, seq);
        }
        if let Some(done) = flips.look(&mut gpu.scanout, &gpu.regs, &clock) {
            let Done { seq, frames, blank_ns, period_ns } = done;
            s.state.set_shown(Shown { seq, frames, blank_ns, period_ns });
            let _ = s.done.signal();
        }
        flips.take_notes().into_iter().for_each(log);
        if let Some(why) = flips.fatal() {
            return Ended::Faulted(why);
        }
    }
}

/// Attaches to the compositor and flips for it, and for the next one when
/// it goes away, until one refuses the screen or the display engine cannot
/// show the driver's pictures. Then the firmware's picture stays.
fn run(mut gpu: Gpu) {
    loop {
        match attach(&gpu) {
            Ok(session) => {
                println!("attached to the compositor");
                match serve(&mut gpu, &session) {
                    Ended::Gone => {
                        println!(
                            "the compositor went away: the firmware's picture is shown until the next one attaches"
                        );
                        gpu.give_back();
                    }
                    Ended::Faulted(why) => {
                        // The compositor draws in place once the connection
                        // closes (when the driver ends).
                        println!("{}: the firmware's picture is shown again, and stays", why);
                        gpu.give_back();
                        gpu.unmap();
                        return;
                    }
                }
            }
            Err(Attach::Refused(e)) => {
                println!("the compositor refused the screen: {}", e);
                gpu.unmap();
                return;
            }
            Err(Attach::Failed(why)) => {
                println!("cannot attach: {}", why);
                vrt::time::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn main() -> i32 {
    let (pci, dev, irq) = match open() {
        Ok(opened) => opened,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    let dev: &'static Device = Box::leak(Box::new(dev));
    let readout = Readout::read(dev.regs, dev.platform.pipes);
    for pipe in &readout.pipes {
        println!("  {}", pipe);
    }
    // The engines' room in the global table is set aside first, clear of
    // what is scanned out; the display's pictures go elsewhere.
    let mut busy = readout.scanned_out();
    let room = dev
        .gtt
        .find_clear(engines::ROOM_FROM, engines::ROOM_BYTES, engines::ROOM_ALIGN, &busy)
        .map(|at| (at, engines::ROOM_BYTES));
    busy.extend(room);
    // Flips are time-critical: they run above normal programs.
    let display =
        vrt::thread::Builder::new().name("flips").priority(vabi::priority::HIGH).spawn(move || {
            match take_over(dev, readout, busy, irq) {
                Ok(gpu) => run(gpu),
                Err(e) => println!("{}", e),
            }
        });
    if let Err(e) = &display {
        println!("cannot start the flip thread: {}", e);
    }
    match room {
        Some(room) => engines::run(dev, room),
        None => println!("GT: no room for the engines' contexts in the global table: no 3D"),
    }
    // The engines did not start: the display goes on alone.
    if let Ok(handle) = display {
        let _ = handle.join();
    }
    drop(pci);
    0
}
