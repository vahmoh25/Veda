//! `alsa` — Veda's sound driver for Linux: the guest's sound card (Linux's
//! first playback device, through ALSA's kernel interface) as Veda's output
//! device.
//!
//! It attaches to Veda's audio service (`audiodev`) over the bridge, as
//! Veda's own sound drivers do, and plays the service's mixed output from
//! the shared ring: a period (10 ms) at a time, a few periods ahead of what
//! is heard. Running short while streams play, it pads with silence (an
//! underrun, which deepens the queue). How much has played comes from the
//! card's own count (ALSA's delay), stamped with Veda's clock, which the
//! guest reads itself. The card's period interrupts pace this program while
//! it plays; after a while without audio it stops, and waits for the
//! service to have some. The card's own mixer stays at unity gain (`ctl`):
//! the volume is Veda's.

mod ctl;
mod pcm;

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use vabi::{WaitItem, signals};
use vproto::audio::{DeviceFormat, Ring, Role, audiodev, ring::flags};
use vrt::object::{Channel, Event};
use vrt::time::now_ns;

use pcm::Pcm;

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
/// A period: 10 ms.
const PERIOD: u32 = RATE / 100;
/// The card's buffer, in periods.
const PERIODS: u32 = 16;
/// Periods kept queued normally (60 ms), and at most after underruns.
const DEPTH: u32 = 6;
const MAX_DEPTH: u32 = 12;
/// Below this many queued periods the driver pads with silence.
const MIN_QUEUED: u32 = 2;
/// Periods of audio needed before (re)starting playback.
const PREFILL: u32 = 2;
/// Stop the card after this long without audio.
const IDLE_STOP_NS: u64 = 1_500_000_000;

/// The first playback device, once there is one: its path, its card's
/// number and the card's name.
fn find_device() -> (String, String, String) {
    let mut said = false;
    loop {
        let mut pcms: Vec<String> = std::fs::read_dir("/dev/snd")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        pcms.retain(|n| n.starts_with("pcmC") && n.ends_with('p'));
        pcms.sort();
        if let Some(pcm) = pcms.first() {
            let card = pcm[4..].split('D').next().unwrap_or("0").to_string();
            let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
            let name = cards
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("{card} [")))
                .and_then(|l| l.split(" - ").nth(1))
                .map_or_else(|| format!("card {card}"), |n| n.trim().to_string());
            return (format!("/dev/snd/{pcm}"), card, name);
        }
        if !said {
            println!("alsa: waiting for a sound card");
            said = true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The card, playing the ring.
struct Player {
    pcm: Pcm,
    running: bool,
    /// Periods kept queued.
    depth: u32,
    /// Frames written to the card since it started, and the chunks written
    /// (the card's position at a chunk's end, and how many of its frames
    /// came from the ring: the rest is silence).
    written: u64,
    chunks: VecDeque<(u64, u32)>,
    /// Frames written that are not heard yet.
    queued: u64,
    /// Ring frames heard, and when that count last grew.
    played: u64,
    played_ns: u64,
    underruns: u32,
    starving: bool,
    last_audio_ns: u64,
    scratch: Vec<i16>,
}

impl Player {
    fn new(pcm: Pcm) -> Player {
        let samples = (pcm.period * CHANNELS) as usize;
        Player {
            pcm,
            running: false,
            depth: DEPTH,
            written: 0,
            chunks: VecDeque::new(),
            queued: 0,
            played: 0,
            played_ns: 0,
            underruns: 0,
            starving: false,
            last_audio_ns: 0,
            scratch: vec![0; samples],
        }
    }

    fn period(&self) -> u32 {
        self.pcm.period
    }

    /// Periods queued in the card.
    fn queued_periods(&self) -> u32 {
        self.queued.div_ceil(self.period() as u64) as u32
    }

    /// Counts the chunks the card has played into the ring's played
    /// position. Returns true if any did.
    fn reap(&mut self) -> bool {
        if !self.running {
            return false;
        }
        // Ran dry: all of it was heard.
        let delay = self.pcm.delay().unwrap_or(0).min(self.written);
        let heard = self.written - delay;
        self.queued = delay;
        let now = now_ns();
        let mut any = false;
        while let Some(&(end, frames)) = self.chunks.front() {
            if end > heard {
                break;
            }
            self.chunks.pop_front();
            if frames > 0 {
                self.played += frames as u64;
                // That many frames ago, at the card's rate.
                self.played_ns = now.saturating_sub((heard - end) * 1_000_000_000 / RATE as u64);
            }
            any = true;
        }
        any
    }

    /// Writes a period: up to `frames` frames of the ring, then silence.
    /// Returns the ring frames the card took (`None`: it took nothing, it
    /// is full).
    fn queue(&mut self, ring: &Ring, frames: u32) -> io::Result<Option<u32>> {
        let (period, ch) = (self.period() as usize, CHANNELS as usize);
        let n = ring.peek(&mut self.scratch[..(frames as usize).min(period) * ch]);
        self.scratch[n * ch..].fill(0);
        let taken = self.pcm.write(&self.scratch)?;
        let from_ring = n.min(taken);
        ring.consume(from_ring);
        if taken > 0 {
            self.written += taken as u64;
            self.queued += taken as u64;
            self.chunks.push_back((self.written, from_ring as u32));
        }
        if from_ring > 0 {
            self.last_audio_ns = now_ns();
        }
        Ok((taken > 0).then_some(from_ring as u32))
    }

    /// Starts the card on what the ring holds.
    fn start(&mut self, ring: &Ring) -> io::Result<()> {
        self.pcm.prepare()?;
        (self.written, self.queued) = (0, 0);
        self.chunks.clear();
        // The card starts with the first period written.
        while self.queued_periods() < self.depth && ring.filled() > 0 {
            let n = ring.filled().min(self.period());
            if self.queue(ring, n)?.is_none() {
                break;
            }
        }
        self.running = true;
        self.starving = false;
        self.last_audio_ns = now_ns();
        Ok(())
    }

    /// The card ran dry and stopped (it was not refilled in time).
    fn ran_dry(&mut self) {
        self.underruns += 1;
        self.depth = (self.depth + 1).min(MAX_DEPTH);
        if self.underruns <= 10 || self.underruns.is_multiple_of(100) {
            println!("alsa: the card ran dry: underrun {} (queue depth now {})", self.underruns, self.depth);
        }
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.pcm.stop();
        self.running = false;
        self.queued = 0;
        self.chunks.clear();
    }

    /// Starts, refills or stops the card. Returns true if ring data was
    /// consumed or the state changed.
    fn pump(&mut self, ring: &Ring) -> bool {
        let active = ring.producer_flags() & flags::ACTIVE != 0;
        if !self.running {
            let filled = ring.filled();
            if filled == 0 || (active && filled < self.period() * PREFILL) {
                return false;
            }
            if let Err(e) = self.start(ring) {
                println!("alsa: the card does not start: {e}");
                self.stop();
            }
            return true;
        }
        if !active && ring.filled() == 0 && now_ns().saturating_sub(self.last_audio_ns) > IDLE_STOP_NS {
            // Nothing more: what is queued plays, then the card stops.
            if self.queued == 0 {
                self.stop();
                return true;
            }
            return false;
        }
        let mut changed = false;
        while self.queued_periods() < self.depth {
            let filled = ring.filled();
            let result = if filled >= self.period() {
                self.starving = false;
                self.queue(ring, self.period())
            } else if self.queued_periods() >= MIN_QUEUED {
                break;
            } else {
                // About to run dry: queue what there is, and silence.
                if active && !self.starving {
                    self.starving = true;
                    self.underruns += 1;
                    self.depth = (self.depth + 1).min(MAX_DEPTH);
                    if self.underruns <= 10 || self.underruns.is_multiple_of(100) {
                        println!("alsa: underrun {} (queue depth now {})", self.underruns, self.depth);
                    }
                }
                self.queue(ring, filled)
            };
            match result {
                Ok(Some(n)) => changed |= n > 0,
                Ok(None) => break,
                // EPIPE: it ran dry before this was written.
                Err(e) if e.raw_os_error() == Some(32) => {
                    self.ran_dry();
                    return true;
                }
                Err(e) => {
                    println!("alsa: the card refused audio: {e}");
                    self.stop();
                    return true;
                }
            }
        }
        changed
    }

    fn publish(&self, ring: &Ring) {
        ring.set_played(self.played, self.played_ns);
        ring.set_latency(self.queued.min(u32::MAX as u64) as u32);
        ring.set_underruns(self.underruns);
        ring.set_consumer_flags(if self.running { flags::RUNNING } else { 0 });
    }
}

/// Connects to the audio service and attaches the card.
fn attach(name: &str, player: &Player) -> Result<(Channel, Ring, Event, Event), String> {
    let ch = vproto::connect(audiodev::NAME).map_err(|e| format!("no audio service: {e:?}"))?;
    let client = audiodev::Client::new(ch);
    let format = DeviceFormat {
        name: format!("{name} (Linux)"),
        rate: RATE,
        channels: CHANNELS,
        period_frames: player.period(),
        max_periods: MAX_DEPTH,
    };
    let link = client
        .attach(format)
        .map_err(|e| format!("the audio service went away: {e}"))?
        .map_err(|e| format!("the audio service refused the card: {e}"))?;
    let ring = Ring::map(link.ring, Role::Consumer).map_err(|e| format!("bad ring: {e:?}"))?;
    if ring.rate() != RATE || ring.channels() != CHANNELS {
        return Err("the ring's format is not the card's".into());
    }
    Ok((client.into_channel(), ring, link.data_event, link.space_event))
}

/// Plays until the audio service goes away.
fn serve(player: &mut Player, link: &Channel, ring: &Ring, data_event: &Event, space_event: &Event) {
    let period_ms = (player.period() * 1000 / RATE) as i32;
    loop {
        if player.running {
            // The card's period interrupt (or two periods' time).
            let _ = player.pcm.wait(2 * period_ms + 5);
            let mut items = [WaitItem { handle: link.raw(), signals: signals::PEER_CLOSED, ..Default::default() }];
            let _ = vrt::object::wait_many(&mut items, 0);
            if items[0].observed & signals::PEER_CLOSED != 0 {
                return;
            }
        } else {
            let mut items = [
                WaitItem { handle: link.raw(), signals: signals::PEER_CLOSED, ..Default::default() },
                WaitItem { handle: data_event.raw(), signals: signals::SIGNALED, ..Default::default() },
            ];
            let _ = vrt::object::wait_many(&mut items, now_ns() + 500_000_000);
            if items[0].observed & signals::PEER_CLOSED != 0 {
                return;
            }
            let _ = data_event.clear();
        }
        let mut changed = player.reap();
        changed |= player.pump(ring);
        player.publish(ring);
        if changed {
            let _ = space_event.signal();
        }
    }
}

fn main() {
    let (path, card, name) = find_device();
    match ctl::open_outputs(&card) {
        Ok(set) => println!("alsa: outputs on, at unity gain: {}", set.join(", ")),
        Err(e) => println!("alsa: the mixer of card {card}: {e}"),
    }
    let pcm = match Pcm::open(&path, RATE, CHANNELS, PERIOD, PERIODS) {
        Ok(p) => p,
        Err(e) => {
            println!("alsa: {path} cannot play {RATE} Hz, 16-bit stereo: {e}");
            std::process::exit(1);
        }
    };
    println!("alsa: {name} ({path}): {} Hz, periods of {} frames, a buffer of {}", pcm.rate, pcm.period, pcm.buffer);
    let mut player = Player::new(pcm);
    loop {
        match attach(&name, &player) {
            Ok((link, ring, data_event, space_event)) => {
                println!("alsa: attached to the audio service");
                player.played = ring.read_pos();
                player.played_ns = 0;
                serve(&mut player, &link, &ring, &data_event, &space_event);
                println!("alsa: the audio service went away");
                player.stop();
            }
            Err(e) => {
                println!("alsa: cannot attach: {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}
