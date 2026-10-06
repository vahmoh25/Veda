//! Playback: the audio service's mixed output through the output stream.
//!
//! The stream's buffer is a ring of 10 ms periods that the controller plays
//! round and round. The driver keeps a few periods ahead of it filled from
//! the service's ring (padding with silence so the controller never plays
//! stale audio when the service is late, which counts as an underrun while
//! streams are active and deepens the queue), and silences every period as
//! soon as it has been played. The played position counts the service's
//! frames in finished periods plus the part of the current one already
//! played. After a while without audio the stream stops; it starts again,
//! from the first period, when audio arrives. A stream that stops moving is
//! stopped and started again, rather than left to play what is queued over
//! and over.
//!
//! **Where the controller is.** Its position in the buffer says which
//! periods it has played, checked against the link's clock: the 24 MHz
//! clock the controller plays by. A position that claims more periods than
//! the clock allowed since the last reading went back or jumped, and is
//! not taken as progress (that would throw queued audio away). A position
//! that runs far ahead of the clock is not believed at all: the stream
//! starts again, paced by the clock from then on.
//!
//! Soon after playback starts, the driver checks the output once: how fast
//! the link's clock ran by the system's, how fast every position counter
//! moved by the link's clock, how many buffers the controller completed,
//! the stream's format and FIFO, and the glitches and underruns so far.

use alloc::vec;
use alloc::vec::Vec;

use vproto::audio::{Ring, ring::flags};
use vrt::println;
use vrt::time::now_ns;

use crate::controller::{Controller, SD_STS_DESE, SD_STS_FIFOE, Stream};
use crate::speakers::Speakers;
use crate::{CHANNELS, FRAME_BYTES, Timing};

/// Periods in the buffer (160 ms).
pub const PERIODS: usize = 16;
/// Periods kept queued normally (60 ms), and at most after underruns.
const DEPTH: usize = 6;
pub const MAX_DEPTH: usize = 12;
/// Below this many queued periods the driver pads with silence.
const MIN_QUEUED: usize = 2;
/// Periods of audio needed before (re)starting.
const PREFILL: u32 = 2;
/// Stop after this long without audio.
const IDLE_STOP_NS: u64 = 1_500_000_000;
/// A running stream that has not moved for this long has stopped.
pub const STALL_NS: u64 = 1_000_000_000;
/// Errors of each kind logged at most (then every hundredth).
pub const LOGGED_ERRORS: u32 = 3;
/// Periods a position reading may claim beyond the link's clock since the
/// last one (controllers and emulators move in bursts).
pub const JUMP_SLACK: u64 = 6;
/// A position more than this many periods beyond twice the link clock's
/// is not believed (an emulator's first fetches stay well below).
pub const RUNAWAY_SLACK: u64 = 8;
/// The link's clock.
pub const LINK_HZ: u64 = 24_000_000;
/// A link clock that has not moved after this long (by the system's) is
/// not used.
const CLOCK_TRUSTED_AFTER_NS: u64 = 50_000_000;
/// The output is checked once, over `CHECK_NS` after the stream has played
/// for `CHECK_SETTLE_NS` (past the controller's first fetches).
const CHECK_SETTLE_NS: u64 = 250_000_000;
const CHECK_NS: u64 = 1_000_000_000;

/// The link's clock as a stream sees it, from when the stream started.
pub struct LinkClock {
    last: u32,
    ticks: u64,
    started_ns: u64,
    /// The clock never moved: the system's is used instead.
    pub frozen: bool,
}

impl LinkClock {
    pub fn new() -> LinkClock {
        LinkClock { last: 0, ticks: 0, started_ns: 0, frozen: false }
    }

    pub fn start(&mut self, hc: &Controller) {
        self.last = hc.wall_clock();
        self.ticks = 0;
        self.started_ns = now_ns();
    }

    /// Reads the clock. Returns true if it just turned out not to move.
    pub fn update(&mut self, hc: &Controller, now: u64) -> bool {
        let clock = hc.wall_clock();
        self.ticks += clock.wrapping_sub(self.last) as u64;
        self.last = clock;
        if !self.frozen && self.ticks == 0 && now.saturating_sub(self.started_ns) > CLOCK_TRUSTED_AFTER_NS {
            self.frozen = true;
            return true;
        }
        false
    }

    /// Frames played at `rate` since the start, by this clock.
    pub fn frames(&self, rate: u32, now: u64) -> u64 {
        if self.frozen {
            now.saturating_sub(self.started_ns) * rate as u64 / 1_000_000_000
        } else {
            self.ticks * rate as u64 / LINK_HZ
        }
    }
}

/// The start of the output check's window.
#[derive(Clone, Copy)]
struct CheckFrom {
    ns: u64,
    clock: u32,
    completions: u32,
}

pub struct Player {
    pub stream: Stream,
    timing: Timing,
    /// Frames taken from the service's ring into each period (the rest is
    /// silence).
    taken: [u32; PERIODS],
    /// Periods filled, and periods finished, since the stream started.
    filled: u64,
    finished: u64,
    clock: LinkClock,
    /// Periods by the link's clock when the position was last read.
    looked_periods: u64,
    /// The position then, and when it last changed.
    last_position: usize,
    moved_ns: u64,
    /// The position is not believed: playback is paced by the link's clock.
    pub by_clock: bool,
    /// The stream stopped moving; the controller fell behind fetching
    /// audio (FIFO underruns); a position went back or jumped.
    stalls: u32,
    fifo_errors: u32,
    glitches: u32,
    check_from: Option<CheckFrom>,
    checked: bool,
    /// The check was just logged (see [`Player::take_checked`]).
    just_checked: bool,
    pub running: bool,
    /// When the stream last started.
    pub started_ns: u64,
    depth: usize,
    /// Service frames in finished periods, and in the current period so
    /// far.
    played: u64,
    played_partly: u64,
    played_ns: u64,
    underruns: u32,
    starving: bool,
    last_audio_ns: u64,
    scratch: Vec<i16>,
    /// Amplifiers beside the codec, if the machine has some.
    pub speakers: Speakers,
}

impl Player {
    pub fn new(stream: Stream, timing: Timing) -> Player {
        Player {
            stream,
            timing,
            taken: [0; PERIODS],
            filled: 0,
            finished: 0,
            clock: LinkClock::new(),
            looked_periods: 0,
            last_position: 0,
            moved_ns: 0,
            by_clock: false,
            stalls: 0,
            fifo_errors: 0,
            glitches: 0,
            check_from: None,
            checked: false,
            just_checked: false,
            running: false,
            started_ns: 0,
            depth: DEPTH,
            played: 0,
            played_partly: 0,
            played_ns: 0,
            underruns: 0,
            starving: false,
            last_audio_ns: 0,
            scratch: vec![0; timing.period_frames * CHANNELS],
            speakers: Speakers::start(),
        }
    }

    /// Continues counting from `position` (a new link to the service).
    pub fn resume_at(&mut self, position: u64) {
        self.played = position;
        self.played_partly = 0;
        self.played_ns = 0;
    }

    fn queued(&self) -> u64 {
        self.filled - self.finished
    }

    /// The service's frames taken and not played yet.
    fn pending(&self) -> u64 {
        (self.finished..self.filled).map(|a| self.taken[(a % PERIODS as u64) as usize] as u64).sum()
    }

    /// Reports the stream's errors: FIFO underruns (the controller could
    /// not fetch the audio in time) and descriptor errors (it could not
    /// read its buffer list, and stopped).
    fn check_errors(&mut self, hc: &Controller) {
        let errors = self.stream.errors(hc);
        if errors & SD_STS_FIFOE != 0 {
            self.fifo_errors += 1;
            if self.fifo_errors <= LOGGED_ERRORS || self.fifo_errors.is_multiple_of(100) {
                println!("the controller fetched the audio too late {} times (FIFO underrun)", self.fifo_errors);
            }
        }
        if errors & SD_STS_DESE != 0 && self.stalls < LOGGED_ERRORS {
            println!("the controller cannot read the output stream's buffer list");
        }
    }

    /// Counts the periods the controller finished and silences them.
    /// Returns true if any did, or the stream stopped.
    pub fn reap(&mut self, hc: &Controller) -> bool {
        if !self.running {
            return false;
        }
        let now = now_ns();
        if self.clock.update(hc, now) {
            println!("the link's clock does not move; timing playback by the system's");
        }
        let (period_frames, period_bytes) = (self.timing.period_frames as u64, self.stream.period_bytes);
        let clock_frames = self.clock.frames(self.timing.rate, now);
        // Periods the controller has played by the clock.
        let clock_periods = clock_frames / period_frames;
        self.check_errors(hc);
        let (done, current, into) = if self.by_clock {
            let done = clock_periods.saturating_sub(self.finished);
            (done, (clock_periods % PERIODS as u64) as usize, clock_frames % period_frames)
        } else {
            let position = self.stream.position(hc);
            if position != self.last_position {
                self.last_position = position;
                self.moved_ns = now;
            } else if now.saturating_sub(self.moved_ns) > STALL_NS {
                self.stalls += 1;
                if self.stalls <= LOGGED_ERRORS {
                    println!(
                        "the output stream stopped (at byte {} of its buffer, control {:#010x}); starting it again",
                        position,
                        self.stream.state(hc)
                    );
                }
                self.stop(hc);
                return true;
            }
            let current = position / period_bytes;
            let mut done = ((current + PERIODS - (self.finished % PERIODS as u64) as usize) % PERIODS) as u64;
            let elapsed = clock_periods.saturating_sub(self.looked_periods);
            let laps = PERIODS as u64;
            if elapsed >= laps {
                // Asleep for a whole lap or more: the position alone cannot
                // tell how many laps; the clock can.
                done += (elapsed - done + laps / 2) / laps * laps;
            } else if done > elapsed + JUMP_SLACK {
                // More periods than the clock allows: the position went
                // back (across a period boundary that reads as a whole
                // lap) or jumped. Not progress; look again later.
                self.glitches += 1;
                if self.glitches <= LOGGED_ERRORS {
                    println!(
                        "the output position jumped from period {} to byte {} after {} periods; ignored",
                        self.finished % PERIODS as u64,
                        position,
                        elapsed
                    );
                }
                return false;
            }
            if self.finished + done > 2 * clock_periods + RUNAWAY_SLACK {
                // Far ahead of the clock, the position does not show where
                // the controller is: start again, paced by the clock.
                println!(
                    "the output position ran {} periods in {} by the link's clock; playing by the clock from now on",
                    self.finished + done,
                    clock_periods
                );
                self.by_clock = true;
                self.stop(hc);
                return true;
            }
            (done, current, (position % period_bytes / FRAME_BYTES) as u64)
        };
        for k in 0..done {
            let p = ((self.finished + k) % PERIODS as u64) as usize;
            self.played += self.taken[p] as u64;
            self.taken[p] = 0;
            self.stream.clear_period(p);
        }
        self.finished += done;
        if self.filled <= self.finished {
            // The controller caught up with the data: it plays silence.
            self.filled = self.finished + 1;
        }
        self.played_partly = into.min(self.taken[current] as u64);
        self.played_ns = now;
        self.looked_periods = clock_periods;
        if !self.checked {
            let completions = hc.completions[self.stream.index()];
            match self.check_from {
                None if now.saturating_sub(self.started_ns) >= CHECK_SETTLE_NS => {
                    self.check_from = Some(CheckFrom { ns: now, clock: hc.wall_clock(), completions });
                }
                Some(from) if now.saturating_sub(from.ns) >= CHECK_NS => {
                    self.checked = true;
                    self.just_checked = true;
                    self.check(hc, from, now);
                }
                _ => {}
            }
        }
        done > 0
    }

    /// Whether the output check was logged since the last call: once.
    pub fn take_checked(&mut self) -> bool {
        core::mem::take(&mut self.just_checked)
    }

    /// Logs the output check (see the module documentation): how fast the
    /// link's clock ran by the system's, and how many buffers the
    /// controller completed per second.
    fn check(&self, hc: &Controller, from: CheckFrom, now: u64) {
        let ns = now.saturating_sub(from.ns).max(1);
        let link_khz = hc.wall_clock().wrapping_sub(from.clock) as u64 * 1_000_000 / ns;
        let completions = hc.completions[self.stream.index()].wrapping_sub(from.completions) as u64;
        let (format, fifo) = self.stream.format_and_fifo(hc);
        println!(
            "output check: link clock {}.{:02} MHz by the system's (24 expected), {} buffers completed per second ({} expected), format {:#06x}, FIFO {} bytes, {}, {} position glitches, {} underruns",
            link_khz / 1000,
            link_khz % 1000 / 10,
            completions * 1_000_000_000 / ns,
            self.timing.rate as u64 / self.timing.period_frames as u64,
            format,
            fifo,
            if self.by_clock { "paced by the link's clock" } else { "paced by the position" },
            self.glitches,
            self.underruns
        );
    }

    /// Logs how fast each position counter moves by the link's clock,
    /// sampled closely so that none can lap unseen. Takes 20 ms: called
    /// right after a refill.
    pub fn log_positions(&self, hc: &Controller) {
        let expected = self.timing.rate as u64 * FRAME_BYTES as u64;
        let total = self.stream.periods * self.stream.period_bytes;
        let rates = sample_rates(hc, total, |hc| {
            [Some(self.stream.link_position(hc)), Some(self.stream.buffer_position(hc)), self.stream.dma_position(hc)]
        });
        let rate = |r: Option<u64>| {
            r.map_or(alloc::string::String::from("-"), |r| alloc::format!("{}.{}", r / 1000, r % 1000 / 100))
        };
        println!(
            "output positions by the link's clock: link {} kB/s, position buffer {} kB/s, DMA {} kB/s ({}.{} expected)",
            rate(rates[0]),
            rate(rates[1]),
            rate(rates[2]),
            expected / 1000,
            expected % 1000 / 100
        );
    }

    /// Queues the next period: up to `frames` frames from the ring, padded
    /// with silence. Returns the frames taken.
    fn queue(&mut self, ring: &Ring, frames: u32) -> u32 {
        let p = (self.filled % PERIODS as u64) as usize;
        let want = (frames as usize).min(self.timing.period_frames);
        let n = ring.read(&mut self.scratch[..want * CHANNELS]);
        self.stream.write_period(p, &self.scratch[..n * CHANNELS]);
        self.taken[p] = n as u32;
        self.filled += 1;
        if n > 0 {
            self.last_audio_ns = now_ns();
        }
        n as u32
    }

    fn start(&mut self, hc: &Controller, ring: &Ring) {
        self.stream.clear();
        self.taken = [0; PERIODS];
        self.filled = 0;
        self.finished = 0;
        while (self.filled as usize) < self.depth && ring.filled() > 0 {
            let n = ring.filled().min(self.timing.period_frames as u32);
            self.queue(ring, n);
        }
        if self.filled == 0 {
            return;
        }
        self.stream.start(hc, self.timing.format);
        self.clock.start(hc);
        self.running = true;
        // Amplifiers power up once the codec clocks them.
        self.speakers.playing(true);
        self.starving = false;
        self.started_ns = now_ns();
        self.looked_periods = 0;
        self.last_position = 0;
        self.moved_ns = self.started_ns;
        self.last_audio_ns = self.started_ns;
        self.check_from = None;
    }

    pub fn stop(&mut self, hc: &Controller) {
        if self.running {
            // Amplifiers power down while the codec still clocks them.
            self.speakers.playing(false);
            self.stream.stop(hc);
            self.running = false;
        }
        self.check_from = None;
        // What is queued and not played yet is dropped: it counts as
        // played, so that the position keeps in step with the ring.
        self.played += self.pending();
        self.played_partly = 0;
        self.taken = [0; PERIODS];
    }

    /// Starts, refills or stops playback. Returns true if ring data was
    /// consumed or the state changed.
    pub fn pump(&mut self, hc: &Controller, ring: &Ring) -> bool {
        let active = ring.producer_flags() & flags::ACTIVE != 0;
        let period = self.timing.period_frames as u32;
        if !self.running {
            let filled = ring.filled();
            if filled == 0 || (active && filled < period * PREFILL) {
                return false;
            }
            self.start(hc, ring);
            return true;
        }
        if !active && ring.filled() == 0 && now_ns().saturating_sub(self.last_audio_ns) > IDLE_STOP_NS {
            // Everything queued has long been played.
            self.stop(hc);
            return true;
        }
        let mut changed = false;
        while self.queued() < self.depth as u64 {
            let filled = ring.filled();
            if filled >= period {
                self.queue(ring, period);
                self.starving = false;
                changed = true;
                continue;
            }
            if self.queued() >= MIN_QUEUED as u64 {
                break;
            }
            // About to run dry: queue what there is, and silence.
            if active && !self.starving {
                self.starving = true;
                self.underruns += 1;
                if self.depth < MAX_DEPTH {
                    self.depth += 1;
                }
                if self.underruns <= 10 || self.underruns.is_multiple_of(100) {
                    println!("underrun {} (queue depth now {})", self.underruns, self.depth);
                }
            }
            changed |= self.queue(ring, filled) > 0;
        }
        changed
    }

    /// Publishes the played position, the latency and the state.
    pub fn publish(&self, ring: &Ring) {
        ring.set_played(self.played + self.played_partly, self.played_ns);
        ring.set_latency(self.pending().saturating_sub(self.played_partly) as u32);
        ring.set_underruns(self.underruns);
        ring.set_consumer_flags(if self.running { flags::RUNNING } else { 0 });
    }
}

/// How fast up to three position counters move, in bytes per second by the
/// link's clock: each is read every 20 µs for 20 ms, so that a counter
/// running even a hundred times too fast is still followed round its
/// buffer of `total` bytes.
pub fn sample_rates(
    hc: &Controller,
    total: usize,
    mut read: impl FnMut(&Controller) -> [Option<usize>; 3],
) -> [Option<u64>; 3] {
    let mut moved = [0u64; 3];
    let mut last = read(hc);
    let first_clock = hc.wall_clock();
    let end = now_ns() + 20_000_000;
    while now_ns() < end {
        let wait = now_ns() + 20_000;
        while now_ns() < wait {
            core::hint::spin_loop();
        }
        let now = read(hc);
        for k in 0..3 {
            if let (Some(a), Some(b)) = (last[k], now[k]) {
                moved[k] += ((b + total - a) % total) as u64;
            }
        }
        last = now;
    }
    let ticks = (hc.wall_clock().wrapping_sub(first_clock) as u64).max(1);
    let mut rates = [None; 3];
    for k in 0..3 {
        if last[k].is_some() {
            rates[k] = Some(moved[k] * LINK_HZ / ticks);
        }
    }
    rates
}
