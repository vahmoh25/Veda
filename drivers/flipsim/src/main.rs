//! `flipsim` — a test stand-in: QEMU's standard VGA played as a display
//! that flips, so that the compositor's flips are tested without the
//! hardware that has them.
//!
//! Display engines that flip (Intel's, which `intel-gpu` drives) show one
//! of several pictures from the start of a vertical blank on. QEMU's VGA
//! cannot: it shows its video memory, where the firmware's framebuffer is.
//! This driver attaches to the compositor as a display that flips would
//! (`vproto::displaydev`), with pictures in ordinary memory, and plays the
//! display engine: at its vertical blanks (60 a second, by the clock) it
//! copies the picture asked for into the video memory, which QEMU then
//! shows. So QEMU shows what such a display would, and test scripts see it
//! (`tests/ui/flips.vts`).
//!
//! devmgr starts it only when the kernel command line asks for it
//! (`flipsim`); with `flipsim=N` it goes away after N flips, leaving the
//! screen as it is, as a driver that crashed would.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::read_volatile;

use vabi::{WaitItem, signals};
use vproto::displaydev::{FlipState, Link, Screen, Shown, displaydev};
use vproto::pci::pcidev;
use vrt::object::{Channel, Event, Vmo};
use vrt::println;
use vrt::time::{Duration, now_ns};
use vrt::vm::Mapping;

vrt::entry!(main);

const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
/// The pretended screen's period: 60 frames a second.
const PERIOD_NS: u64 = 16_666_667;
/// The Bochs display interface's registers (16 bits each), in the second
/// BAR at 0x500 + index * 2 (QEMU's `docs/specs/standard-vga.rst`).
const DISPI: usize = 0x500;
const DISPI_XRES: usize = 1;
const DISPI_YRES: usize = 2;
const DISPI_BPP: usize = 3;
const DISPI_VIRT_WIDTH: usize = 6;

/// The video memory, and the picture in it the firmware set up.
struct Vga {
    vram: Mapping,
    width: u32,
    height: u32,
    stride: usize,
    /// Where the video memory is (the firmware's framebuffer's address).
    base: u64,
    _pci: pcidev::Client,
}

fn map_bar(pci: &pcidev::Client, index: u8) -> Result<Mapping, String> {
    let Ok(Ok(vmo)) = pci.map_bar(index) else { return Err(format!("cannot map BAR {}", index)) };
    let len = vmo.size().map_err(|e| format!("BAR {}: {}", index, e))?;
    Mapping::new(vmo, len, vabi::map_flags::READ | vabi::map_flags::WRITE).map_err(|e| format!("BAR {}: {}", index, e))
}

fn setup() -> Result<Vga, String> {
    let h = vrt::env::take_handle(PCIDEV_ROLE).ok_or("no pcidev channel")?;
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let info = pci.info().map_err(|_| "devmgr went away")?;
    let base = info.bars.iter().find(|b| b.index == 0 && !b.io).ok_or("no video memory")?.address;
    let regs = map_bar(&pci, 2)?;
    let reg = |index: usize| {
        // SAFETY: a 16-bit register inside the mapped BAR (4 KiB).
        unsafe { read_volatile(regs.as_ptr().add(DISPI + index * 2) as *const u16) }
    };
    let (width, height, bpp) = (reg(DISPI_XRES) as u32, reg(DISPI_YRES) as u32, reg(DISPI_BPP));
    let stride = reg(DISPI_VIRT_WIDTH) as usize * 4;
    let vram = map_bar(&pci, 0)?;
    if bpp != 32 || width == 0 || stride < width as usize * 4 || stride * height as usize > vram.len() {
        return Err(format!("the firmware's mode is no picture to flip: {}x{} at {} bits", width, height, bpp));
    }
    Ok(Vga { vram, width, height, stride, base, _pci: pci })
}

/// An attachment to the compositor, and the driver's view of the pictures.
struct Session {
    link: Channel,
    state: FlipState,
    request: Event,
    done: Event,
    pictures: Vec<Mapping>,
}

fn attach(vga: &Vga) -> Result<Session, String> {
    let bytes = (vga.stride * vga.height as usize).next_multiple_of(4096);
    let client = displaydev::Client::new(vproto::connect(displaydev::NAME).map_err(|_| "no registry")?);
    let (state, state_vmo) = FlipState::create().map_err(|e| format!("{}", e))?;
    let (request, done) =
        (Event::create().map_err(|e| format!("{}", e))?, Event::create().map_err(|e| format!("{}", e))?);
    let dup = |e: &Event| e.0.duplicate(None).map(Event::from_handle).map_err(|e| format!("{}", e));
    let (mut ours, mut theirs) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let vmo = Vmo::create(bytes).map_err(|e| format!("{}", e))?;
        theirs.push(Vmo::from_handle(vmo.0.duplicate(None).map_err(|e| format!("{}", e))?));
        ours.push(Mapping::new(vmo, bytes, vabi::map_flags::READ).map_err(|e| format!("{}", e))?);
    }
    let screen = Screen {
        name: String::from("flipsim"),
        width: vga.width,
        height: vga.height,
        stride: vga.stride as u32,
        rgbx: false,
        firmware: alloc::vec![vga.base],
        period_ns: PERIOD_NS,
    };
    let link = Link { pictures: theirs, state: state_vmo, request: dup(&request)?, done: dup(&done)? };
    match client.attach(screen, link) {
        Ok(Ok(())) => Ok(Session { link: client.into_channel(), state, request, done, pictures: ours }),
        Ok(Err(e)) => Err(format!("refused: {}", e)),
        Err(_) => Err("the compositor went away".into()),
    }
}

/// Flips for one compositor until it goes away (true) or `limit` flips are
/// done (false).
fn serve(vga: &Vga, s: &Session, flips: &mut u64, limit: Option<u64>) -> bool {
    let start = now_ns();
    let mut last_seq = 0;
    // The flip asked for: the picture, the request's number, and the
    // vertical blank it is due at.
    let mut pending: Option<(usize, u32, u64)> = None;
    loop {
        let deadline = pending.map_or(vabi::DEADLINE_INFINITE, |(_, _, due)| due);
        let mut items = [
            WaitItem { handle: s.link.raw(), signals: signals::PEER_CLOSED, ..Default::default() },
            WaitItem { handle: s.request.raw(), signals: signals::SIGNALED, ..Default::default() },
        ];
        let _ = vrt::object::wait_many(&mut items, deadline);
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return true;
        }
        let now = now_ns();
        if items[1].observed & signals::SIGNALED != 0 {
            let _ = s.request.clear();
            let (picture, seq) = s.state.queued();
            if seq != last_seq && (picture as usize) < s.pictures.len() {
                last_seq = seq;
                // A later request replaces one not yet carried out.
                let due = start + ((now - start) / PERIOD_NS + 1) * PERIOD_NS;
                pending = Some((picture as usize, seq, due));
            }
        }
        if let Some((picture, seq, due)) = pending
            && now >= due
        {
            // The display engine's part: from this blank on, the picture is
            // what the screen shows.
            let bytes = vga.stride * vga.height as usize;
            // SAFETY: both mappings hold at least `bytes`.
            unsafe { core::ptr::copy_nonoverlapping(s.pictures[picture].as_ptr(), vga.vram.as_ptr(), bytes) };
            pending = None;
            let frames = ((due - start) / PERIOD_NS) as u32;
            s.state.set_shown(Shown { seq, frames, blank_ns: due, period_ns: PERIOD_NS });
            let _ = s.done.signal();
            *flips += 1;
            if limit.is_some_and(|n| *flips >= n) {
                return false;
            }
        }
    }
}

fn main() -> i32 {
    let limit = vrt::env::args().iter().find_map(|a| a.strip_prefix("flipsim=").and_then(|n| n.parse::<u64>().ok()));
    let vga = match setup() {
        Ok(vga) => vga,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    println!("QEMU's VGA as a display that flips: {}x{}, {} bytes a row", vga.width, vga.height, vga.stride);
    let mut flips = 0;
    loop {
        match attach(&vga) {
            Ok(session) => {
                println!("attached to the compositor");
                if !serve(&vga, &session, &mut flips, limit) {
                    println!("going away after {} flips, as a driver that crashed would", flips);
                    return 0;
                }
                println!("the compositor went away");
            }
            Err(e) => {
                println!("cannot attach: {}", e);
                vrt::time::sleep(Duration::from_secs(1));
            }
        }
    }
}
