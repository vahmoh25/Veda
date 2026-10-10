//! `alsa` — Veda's sound driver for Linux: the guest's sound card (Linux's
//! first playback device and its card's capture device, through ALSA's
//! kernel interface) as Veda's output and input devices.
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
//!
//! The card records, on a thread of its own, while the audio service wants
//! audio (the input ring's `CAPTURE` flag): what it read goes into the
//! ring, with the moment it was recorded, and the service hears of it.

mod ctl;
mod pcm;

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use vabi::{WaitItem, signals};
use vproto::audio::{DeviceFormat, Ring, Role, audiodev, ring::flags};
use vrt::object::{Channel, Event};
use vrt::time::now_ns;

use pcm::{Pcm, Stream};

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

/// A sound card: its number and name, and its devices.
struct Card {
    number: String,
    name: String,
    playback: String,
    capture: Option<String>,
}

/// The card of the first playback device, once there is one, and that
/// card's first capture device. Linux lists a card (`/proc/asound/cards`)
/// once it has made all of its devices, the mixer last (devtmpfs makes
/// their nodes as it does): a card is taken once it is listed, and the list
/// is read first.
fn find_card() -> Card {
    let mut said = false;
    loop {
        let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        let pcms = std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
        // `CC-DD: id : name : playback 1 : capture 1`
        let devices: Vec<(u32, u32, bool, bool)> = pcms
            .lines()
            .filter_map(|l| {
                let (at, rest) = l.split_once(':')?;
                let (card, device) = at.split_once('-')?;
                Some((
                    card.trim().parse().ok()?,
                    device.trim().parse().ok()?,
                    rest.contains("playback"),
                    rest.contains("capture"),
                ))
            })
            .collect();
        for &(number, device, ..) in devices.iter().filter(|d| d.2) {
            // ` 0 [Intel          ]: HDA-Intel - HDA Intel`
            let listed = cards.lines().find(|l| l.trim_start().starts_with(&format!("{number} [")));
            let Some((_, name)) = listed.and_then(|l| l.split_once(" - ")) else {
                continue;
            };
            let capture =
                devices.iter().find(|d| d.0 == number && d.3).map(|d| format!("/dev/snd/pcmC{number}D{}c", d.1));
            return Card {
                number: number.to_string(),
                name: name.trim().to_string(),
                playback: format!("/dev/snd/pcmC{number}D{device}p"),
                capture,
            };
        }
        if !said {
            println!("alsa: waiting for a sound card");
            said = true;
        }
        std::thread::sleep(Duration::from_millis(100));
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

/// The card's capture device, recording into the input ring.
struct Recorder {
    pcm: Pcm,
    running: bool,
    overruns: u32,
    scratch: Vec<i16>,
}

/// The input link: the ring the recorder fills, the event that tells the
/// service, and the one by which it says it wants audio.
struct Input {
    ring: Ring,
    data_event: Event,
    wake_event: Event,
}

impl Recorder {
    fn new(pcm: Pcm) -> Recorder {
        let samples = (pcm.period * CHANNELS) as usize;
        Recorder { pcm, running: false, overruns: 0, scratch: vec![0; samples] }
    }

    /// Records while the service wants audio.
    fn follow(&mut self, ring: &Ring) {
        let wanted = ring.consumer_flags() & flags::CAPTURE != 0;
        if wanted == self.running {
            return;
        }
        if wanted {
            match self.pcm.prepare().and_then(|_| self.pcm.start()) {
                Ok(()) => {
                    self.running = true;
                    println!("alsa: recording started");
                }
                Err(e) => println!("alsa: the card does not record: {e}"),
            }
        } else {
            self.stop();
            println!("alsa: recording stopped");
        }
    }

    fn stop(&mut self) {
        let _ = self.pcm.stop();
        self.running = false;
    }

    /// Takes what the card recorded into the ring. Returns true if frames
    /// were delivered.
    fn reap(&mut self, ring: &Ring) -> bool {
        let mut delivered = false;
        while self.running {
            let n = match self.pcm.read(&mut self.scratch) {
                Ok(0) => break,
                Ok(n) => n,
                // EPIPE: it ran over (this program was too slow), and
                // stopped: start again.
                Err(e) if e.raw_os_error() == Some(32) => {
                    self.overruns += 1;
                    if self.overruns <= 5 {
                        println!("alsa: recording overrun; restarting the input");
                    }
                    if let Err(e) = self.pcm.prepare().and_then(|_| self.pcm.start()) {
                        println!("alsa: the card does not record: {e}");
                        self.running = false;
                    }
                    break;
                }
                Err(e) => {
                    println!("alsa: the card's recording failed: {e}");
                    self.stop();
                    break;
                }
            };
            let written = ring.write(&self.scratch[..n * CHANNELS as usize]);
            if written < n {
                ring.set_overruns(ring.overruns().saturating_add((n - written) as u32));
            }
            // What was read is all the card had: its last frame is now's.
            ring.set_capture_clock(ring.write_pos(), now_ns());
            delivered = true;
        }
        delivered
    }

    /// Records until `stop` is set (and the wake event signaled).
    fn serve(&mut self, input: &Input, stop: &AtomicBool) {
        let period_ms = (self.pcm.period * 1000 / RATE) as i32;
        while !stop.load(Ordering::Acquire) {
            if self.running {
                // The card's period interrupt (or two periods' time).
                let _ = self.pcm.wait(2 * period_ms + 5);
            } else {
                let mut items =
                    [WaitItem { handle: input.wake_event.raw(), signals: signals::SIGNALED, ..Default::default() }];
                let _ = vrt::object::wait_many(&mut items, now_ns() + 500_000_000);
                let _ = input.wake_event.clear();
            }
            self.follow(&input.ring);
            if self.reap(&input.ring) {
                let _ = input.data_event.signal();
            }
        }
        self.stop();
    }
}

/// The output link: the ring the player plays, and its events.
struct Output {
    ring: Ring,
    data_event: Event,
    space_event: Event,
}

/// Connects to the audio service and attaches the card (and its input,
/// with `recorder`).
fn attach(
    name: &str,
    player: &Player,
    recorder: Option<&Recorder>,
) -> Result<(Channel, Output, Option<Input>), String> {
    let ch = vproto::connect(audiodev::NAME).map_err(|e| format!("no audio service: {e:?}"))?;
    let client = audiodev::Client::new(ch);
    let format = |period_frames| DeviceFormat {
        name: format!("{name} (Linux)"),
        rate: RATE,
        channels: CHANNELS,
        period_frames,
        max_periods: MAX_DEPTH,
    };
    let link = client
        .attach(format(player.period()))
        .map_err(|e| format!("the audio service went away: {e}"))?
        .map_err(|e| format!("the audio service refused the card: {e}"))?;
    let ring = Ring::map(link.ring, Role::Consumer).map_err(|e| format!("bad ring: {e:?}"))?;
    if ring.rate() != RATE || ring.channels() != CHANNELS {
        return Err("the ring's format is not the card's".into());
    }
    let output = Output { ring, data_event: link.data_event, space_event: link.space_event };
    let input = recorder.and_then(|r| match client.attach_input(format(r.pcm.period)) {
        Ok(Ok(l)) => match Ring::map(l.ring, Role::Producer) {
            Ok(ring) => Some(Input { ring, data_event: l.data_event, wake_event: l.wake_event }),
            Err(e) => {
                println!("alsa: bad input ring: {e:?}");
                None
            }
        },
        Ok(Err(e)) => {
            println!("alsa: the audio service refused the input: {e}");
            None
        }
        Err(_) => None,
    });
    Ok((client.into_channel(), output, input))
}

/// Plays until the audio service goes away.
fn serve(player: &mut Player, link: &Channel, out: &Output) {
    let (ring, data_event, space_event) = (&out.ring, &out.data_event, &out.space_event);
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
    let card = find_card();
    let name = &card.name;
    match ctl::open_outputs(&card.number) {
        Ok(set) => println!("alsa: outputs on, at unity gain: {}", set.join(", ")),
        Err(e) => println!("alsa: the mixer of card {}: {e}", card.number),
    }
    let pcm = match Pcm::open(&card.playback, Stream::Playback, RATE, CHANNELS, PERIOD, PERIODS) {
        Ok(p) => p,
        Err(e) => {
            println!("alsa: {} cannot play {RATE} Hz, 16-bit stereo: {e}", card.playback);
            std::process::exit(1);
        }
    };
    println!(
        "alsa: {name} ({}): {} Hz, periods of {} frames, a buffer of {}",
        card.playback, pcm.rate, pcm.period, pcm.buffer
    );
    let mut recorder = card.capture.as_ref().and_then(|path| {
        match Pcm::open(path, Stream::Capture, RATE, CHANNELS, PERIOD, PERIODS) {
            Ok(p) => {
                println!("alsa: {name} records ({path}): periods of {} frames, a buffer of {}", p.period, p.buffer);
                Some(Recorder::new(p))
            }
            Err(e) => {
                println!("alsa: {path} cannot record {RATE} Hz, 16-bit stereo: {e}");
                None
            }
        }
    });
    let mut player = Player::new(pcm);
    loop {
        match attach(name, &player, recorder.as_ref()) {
            Ok((link, out, input)) => {
                println!(
                    "alsa: attached to the audio service{}",
                    if input.is_some() { " (with recording)" } else { "" }
                );
                player.played = out.ring.read_pos();
                player.played_ns = 0;
                // The recorder records on a thread of its own while the
                // link lasts.
                let stop = Arc::new(AtomicBool::new(false));
                let recording = match (input, recorder.take()) {
                    (Some(input), Some(mut rec)) => {
                        let stop = stop.clone();
                        let wake = input.wake_event.0.duplicate(None).ok().map(Event::from_handle);
                        let thread = std::thread::spawn(move || {
                            rec.serve(&input, &stop);
                            rec
                        });
                        Some((thread, wake))
                    }
                    (_, rec) => {
                        recorder = rec;
                        None
                    }
                };
                serve(&mut player, &link, &out);
                println!("alsa: the audio service went away");
                player.stop();
                if let Some((thread, wake)) = recording {
                    stop.store(true, Ordering::Release);
                    if let Some(w) = wake {
                        let _ = w.signal();
                    }
                    recorder = thread.join().ok();
                }
            }
            Err(e) => {
                println!("alsa: cannot attach: {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}
