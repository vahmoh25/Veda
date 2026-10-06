//! `lpss-spi` — the SPI controllers of Intel's chipsets, and what the
//! firmware places on them: today, a laptop's speaker amplifiers (Cirrus
//! Logic CS35L41, as in ASUS Zenbooks and many Lenovo and HP laptops),
//! without which the speakers play only what the codec drives itself.
//!
//! devmgr starts it for each controller; it asks devmgr which devices the
//! firmware describes on this one, and touches nothing unless they include
//! amplifiers it drives. Then it brings them up (`vcs35l41`, over the
//! controller, `vspi`, and the GPIO pins devmgr drives), loads and starts
//! their DSP firmware (Cirrus's speaker protection and the board's tuning,
//! from `/system/firmware`, as Linux does), and serves the `speakers`
//! protocol: the HD Audio driver says when the stream to the speakers
//! starts and stops, and when headphones should mute them.

#![no_std]
#![no_main]

extern crate alloc;

mod board;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vcs35l41::group::{Amps, SpiLink, SpiMode};
use vipc::WaitSet;
use vproto::pci::{AcpiResource, pcidev};
use vproto::speakers::{SpeakerError, speakers};
use vrt::object::Channel;
use vrt::println;
use vrt::time::now_ns;

use board::LpssBoard;

vrt::entry!(main);

/// Role of the `pcidev` channel (must match devmgr).
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;

/// Controllers whose clock input runs at 120 MHz (Cannon Lake to Tiger
/// Lake-LP); the later ones run at 100 MHz (Linux's `intel-lpss-pci`).
const CLOCK_120_MHZ: &[u16] = &[
    0x02AA, 0x02AB, 0x02FB, 0x06AA, 0x06AB, 0x06FB, 0x34AA, 0x34AB, 0x34FB, 0x4DAA, 0x4DAB, 0x4DFB, 0x9DAA, 0x9DAB,
    0x9DFB, 0xA0AA, 0xA0AB, 0xA0DE, 0xA0DF, 0xA0FB, 0xA0FD, 0xA0FE, 0xA32A, 0xA32B, 0xA37B,
];

/// How often latched amplifier errors are looked for while playing.
const ERROR_CHECK_NS: u64 = 1_000_000_000;
/// Power transitions logged (then only problems).
const LOGGED_TRANSITIONS: u32 = 3;

fn main() -> i32 {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        println!("no pcidev channel");
        return 1;
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let Ok(info) = pci.info() else { return 1 };
    let location = format!("{:02x}:{:02x}.{}", info.bus, info.slot, info.function);
    let devices = pci.acpi_devices().unwrap_or_default();
    // Nothing is touched unless the firmware describes amplifiers this
    // driver drives on the bus.
    let Some((index, d)) = devices.iter().enumerate().find(|(_, d)| d.hid == "CSC3551" && d.status & 1 != 0) else {
        let what: Vec<String> = devices.iter().map(|d| format!("{} ({})", d.path, d.hid)).collect();
        if what.is_empty() {
            println!("{}: the firmware describes nothing on this bus", location);
        } else {
            println!("{}: nothing this driver drives on the bus: {}", location, what.join(", "));
        }
        return 0;
    };
    if !d.resource_error.is_empty() {
        println!("{}: {}", d.path, d.resource_error);
        return 0;
    }
    let links: Vec<SpiLink> = d
        .resources
        .iter()
        .filter_map(|r| match r {
            AcpiResource::Spi { chip_select, speed_hz, cpol, cpha, cs_active_high: false, .. } => Some(SpiLink {
                chip_select: *chip_select,
                speed_hz: *speed_hz,
                mode: SpiMode { cpol: *cpol, cpha: *cpha },
            }),
            _ => None,
        })
        .collect();
    let mut amps = match Amps::find(&d.path, &d.sub, &links) {
        Ok(a) => a,
        Err(why) => {
            println!("{}", why);
            return 0;
        }
    };
    let input_hz = if CLOCK_120_MHZ.contains(&info.device) { 120_000_000 } else { 100_000_000 };
    let mut board = match LpssBoard::start(&pci, &info, input_hz, index as u32) {
        Ok(b) => b,
        Err(e) => {
            println!("controller at {}: {}", location, e);
            return 1;
        }
    };
    let speed = board.spi.set_speed(amps.speed_hz);
    println!(
        "controller at {} ({:04x}:{:04x}): {} MHz port clock, {} chip select(s); {} at {} kHz",
        location,
        info.vendor,
        info.device,
        board.spi.clock_hz / 1_000_000,
        board.spi.chip_selects,
        amps.path,
        speed / 1000
    );
    if !amps.bring_up(&mut board) {
        return 1;
    }
    // Their DSPs' firmware (from /system/firmware), as Linux loads it;
    // they play without it if it is missing or fails.
    let started = now_ns();
    if amps.start_firmware(&mut board) {
        println!("{}: firmware started in {} ms", amps.path, (now_ns() - started) / 1_000_000);
    }
    serve(&mut board, &mut amps);
    1
}

/// One request from the sound driver.
struct Session<'a, 'b> {
    board: &'a mut LpssBoard<'b>,
    amps: &'a mut Amps,
    /// Power up once the reply has gone.
    power_up: bool,
    transitions: &'a mut u32,
}

fn report(amps: &Amps, what: &str, problems: &[String], transitions: &mut u32) {
    *transitions += 1;
    if !problems.is_empty() {
        println!("{}: {}: {}", amps.path, what, problems.join("; "));
    } else if *transitions <= LOGGED_TRANSITIONS {
        println!("{}: {}", amps.path, what);
    }
}

impl speakers::Server for Session<'_, '_> {
    fn playing(&mut self, on: bool) -> Result<(), SpeakerError> {
        if on {
            self.power_up = true;
            return Ok(());
        }
        if !self.amps.is_playing() {
            return Ok(());
        }
        let problems = self.amps.playing(self.board, false);
        report(self.amps, "speakers off", &problems, self.transitions);
        if problems.is_empty() { Ok(()) } else { Err(SpeakerError::Failed) }
    }

    fn mute(&mut self, muted: bool) -> Result<(), SpeakerError> {
        if self.amps.mute(self.board, muted) { Ok(()) } else { Err(SpeakerError::Failed) }
    }
}

fn serve(board: &mut LpssBoard, amps: &mut Amps) {
    let listener = match vproto::register(speakers::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register the speakers service: {:?}", e);
            return;
        }
    };
    let mut clients: BTreeMap<u64, Channel> = BTreeMap::new();
    let mut next = 1u64;
    let mut transitions = 0u32;
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE | signals::PEER_CLOSED, 0);
        for (&k, c) in &clients {
            ws.add(c.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        let deadline = if amps.is_playing() { now_ns() + ERROR_CHECK_NS } else { vabi::DEADLINE_INFINITE };
        let ready = ws.wait(deadline).unwrap_or_default();
        let mut closed = Vec::new();
        for (key, observed) in ready {
            if key == 0 {
                while let Some(ch) = vproto::accept(&listener) {
                    clients.insert(next, ch);
                    next += 1;
                }
                continue;
            }
            if observed & signals::READABLE != 0 {
                while let Some(Ok(msg)) = clients.get(&key).map(|c| c.read()) {
                    let power_up = {
                        let mut s =
                            Session { board: &mut *board, amps, power_up: false, transitions: &mut transitions };
                        if let Ok(reply) = speakers::dispatch(&mut s, msg)
                            && let Some(c) = clients.get(&key)
                        {
                            let _ = reply.send(c);
                        }
                        s.power_up
                    };
                    if power_up && !amps.is_playing() {
                        let problems = amps.playing(board, true);
                        report(amps, "speakers on", &problems, &mut transitions);
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                closed.push(key);
            }
        }
        for k in closed {
            clients.remove(&k);
        }
        // The sound driver gone while playing: its stream is gone too.
        if clients.is_empty() && amps.is_playing() {
            let problems = amps.playing(board, false);
            report(amps, "speakers off (the sound driver went away)", &problems, &mut transitions);
        }
        if amps.is_playing() {
            for e in amps.errors(board) {
                println!("{}: {} (it shut itself down; it is released when playback stops)", amps.path, e);
            }
        }
    }
}
