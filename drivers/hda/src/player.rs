//! Playback: the audio service's mixed output through the output stream.
//!
//! The stream's buffer is a ring of 10 ms periods that the controller plays
//! round and round. The driver keeps a few periods ahead of it filled from
//! the service's ring (padding with silence so the controller never plays
//! stale audio when the service is late, which counts as an underrun while
//! streams are active and deepens the queue), and silences every period as
//! soon as it has been played. The played position counts the service's
//! frames in finished periods plus the part of the current one already
//! played, read from the controller's position in the buffer. After a
//! while without audio the stream stops; it starts again, from the first
//! period, when audio arrives. A stream that stops moving is stopped and
//! started again, rather than left to play what is queued over and over.

use alloc::vec;
use alloc::vec::Vec;

use vproto::audio::{Ring, ring::flags};
use vrt::println;
use vrt::time::now_ns;

use crate::controller::{Controller, SD_STS_DESE, SD_STS_FIFOE, Stream};
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

pub struct Player {
    pub stream: Stream,
    timing: Timing,
    /// Frames taken from the service's ring into each period (the rest is
    /// silence).
    taken: [u32; PERIODS],
    /// Periods filled, and periods finished, since the stream started.
    filled: u64,
    finished: u64,
    /// When the position was last read; the position then, and when it
    /// last changed.
    looked_ns: u64,
    last_position: usize,
    moved_ns: u64,
    /// The stream stopped moving; the controller fell behind fetching
    /// audio (FIFO underruns).
    stalls: u32,
    fifo_errors: u32,
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
}

impl Player {
    pub fn new(stream: Stream, timing: Timing) -> Player {
        Player {
            stream,
            timing,
            taken: [0; PERIODS],
            filled: 0,
            finished: 0,
            looked_ns: 0,
            last_position: 0,
            moved_ns: 0,
            stalls: 0,
            fifo_errors: 0,
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
        let position = self.stream.position(hc);
        self.check_errors(hc);
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
        let current = position / self.stream.period_bytes;
        let mut done = ((current + PERIODS - (self.finished % PERIODS as u64) as usize) % PERIODS) as u64;
        // Asleep for a whole lap or more: the position alone cannot tell
        // how many laps; the time that passed can.
        let elapsed = now.saturating_sub(self.looked_ns) / self.timing.period_ns;
        let laps = PERIODS as u64;
        if elapsed >= laps {
            done += (elapsed - done + laps / 2) / laps * laps;
        }
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
        let into = (position % self.stream.period_bytes / FRAME_BYTES) as u64;
        self.played_partly = into.min(self.taken[current] as u64);
        self.played_ns = now;
        self.looked_ns = now;
        done > 0
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
        self.running = true;
        self.starving = false;
        self.looked_ns = now_ns();
        self.started_ns = self.looked_ns;
        self.last_position = 0;
        self.moved_ns = self.looked_ns;
        self.last_audio_ns = self.looked_ns;
    }

    pub fn stop(&mut self, hc: &Controller) {
        if self.running {
            self.stream.stop(hc);
            self.running = false;
        }
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
