//! Recording: the input stream into the audio service's input ring.
//!
//! The stream runs only while the service wants audio (the ring's CAPTURE
//! flag), so the microphone is not recorded while nobody listens. The
//! controller fills a ring of 10 ms periods round and round; each finished
//! period goes to the service, stamped with when its last frame was
//! recorded, which the service needs to line the microphone up with what
//! the speakers played (echo cancellation). Where the controller is comes
//! from its position, checked against the link's clock as for playback
//! (see `player`): a position that runs far ahead of the clock is not
//! believed, and recording is paced by the clock instead.

use alloc::vec;
use alloc::vec::Vec;

use vproto::audio::{Ring, ring::flags};
use vrt::object::Event;
use vrt::println;
use vrt::time::now_ns;

use crate::controller::{Controller, SD_STS_DESE, SD_STS_FIFOE, Stream};
use crate::player::{JUMP_SLACK, LOGGED_ERRORS, LinkClock, RUNAWAY_SLACK, STALL_NS};
use crate::{CHANNELS, FRAME_BYTES, Timing};

/// Periods in the buffer (160 ms).
pub const PERIODS: usize = 16;
/// The controller counts recorded frames before they are all in memory
/// (AMD's up to 32 frames before, as Linux found): a period is taken once
/// the position is this far past its end.
const IN_FLIGHT_FRAMES: u64 = 32;
/// A period held back for that is looked at again this soon.
pub const HELD_BACK_NS: u64 = 1_000_000;

pub struct Recorder {
    pub stream: Stream,
    timing: Timing,
    /// The next period to take, counted since the stream started.
    next: u64,
    clock: LinkClock,
    /// Periods by the link's clock when the position was last read.
    looked_periods: u64,
    /// The position then, and when it last changed.
    last_position: usize,
    moved_ns: u64,
    /// The position is not believed: recording is paced by the link's
    /// clock.
    by_clock: bool,
    /// The stream stopped moving; the controller fell behind writing
    /// audio (FIFO overruns); the driver fell behind taking it; a
    /// position went back or jumped.
    stalls: u32,
    fifo_errors: u32,
    overruns: u32,
    glitches: u32,
    pub running: bool,
    /// A finished period was held back (see [`IN_FLIGHT_FRAMES`]).
    pub held_back: bool,
    scratch: Vec<i16>,
}

impl Recorder {
    pub fn new(stream: Stream, timing: Timing) -> Recorder {
        Recorder {
            stream,
            timing,
            next: 0,
            clock: LinkClock::new(),
            looked_periods: 0,
            last_position: 0,
            moved_ns: 0,
            by_clock: false,
            stalls: 0,
            fifo_errors: 0,
            overruns: 0,
            glitches: 0,
            running: false,
            held_back: false,
            scratch: vec![0; timing.period_frames * CHANNELS],
        }
    }

    /// Records while the audio service wants audio. Returns true if the
    /// state changed.
    pub fn follow(&mut self, hc: &Controller, ring: &Ring) -> bool {
        let wanted = ring.consumer_flags() & flags::CAPTURE != 0;
        if wanted == self.running {
            return false;
        }
        if wanted {
            self.start(hc);
            self.running = true;
            println!("recording started");
        } else {
            self.stop(hc);
            println!("recording stopped");
        }
        true
    }

    /// Starts the stream from the first period.
    fn start(&mut self, hc: &Controller) {
        self.stream.clear();
        self.next = 0;
        self.stream.start(hc, self.timing.format);
        self.clock.start(hc);
        self.looked_periods = 0;
        self.last_position = 0;
        self.moved_ns = now_ns();
    }

    pub fn stop(&mut self, hc: &Controller) {
        if self.running {
            self.stream.stop(hc);
            self.running = false;
        }
    }

    /// Reports the stream's errors: FIFO overruns (the controller could
    /// not write the audio in time, and lost some) and descriptor errors
    /// (it could not read its buffer list, and stopped).
    fn check_errors(&mut self, hc: &Controller) {
        let errors = self.stream.errors(hc);
        if errors & SD_STS_FIFOE != 0 {
            self.fifo_errors += 1;
            if self.fifo_errors <= LOGGED_ERRORS || self.fifo_errors.is_multiple_of(100) {
                println!("the controller stored the recording too late {} times (FIFO overrun)", self.fifo_errors);
            }
        }
        if errors & SD_STS_DESE != 0 && self.stalls < LOGGED_ERRORS {
            println!("the controller cannot read the input stream's buffer list");
        }
    }

    /// Takes the finished periods into the ring. Returns true if frames
    /// were delivered.
    pub fn reap(&mut self, hc: &Controller, ring: &Ring, data_event: &Event) -> bool {
        self.held_back = false;
        if !self.running {
            return false;
        }
        let now = now_ns();
        if self.clock.update(hc, now) {
            println!("the link's clock does not move; timing recording by the system's");
        }
        let period = self.timing.period_frames;
        let period_bytes = self.stream.period_bytes;
        let total = PERIODS * period_bytes;
        let clock_frames = self.clock.frames(self.timing.rate, now);
        let clock_periods = clock_frames / period as u64;
        self.check_errors(hc);
        // Where the controller is, in bytes into the buffer.
        let position = if self.by_clock {
            (clock_frames % (PERIODS * period) as u64) as usize * FRAME_BYTES
        } else {
            self.stream.position(hc)
        };
        let mut full = if self.by_clock {
            // The period just finished by the clock may not all be in
            // memory yet.
            let complete = clock_frames.saturating_sub(IN_FLIGHT_FRAMES) / period as u64;
            if complete < clock_periods {
                self.held_back = true;
            }
            complete.saturating_sub(self.next)
        } else {
            if position != self.last_position {
                self.last_position = position;
                self.moved_ns = now;
            } else if now.saturating_sub(self.moved_ns) > STALL_NS {
                self.stalls += 1;
                if self.stalls <= LOGGED_ERRORS {
                    println!(
                        "the input stream stopped (at byte {} of its buffer, control {:#010x}); starting it again",
                        position,
                        self.stream.state(hc)
                    );
                }
                self.start(hc);
                return false;
            }
            let current = position / period_bytes;
            let mut full = ((current + PERIODS - (self.next % PERIODS as u64) as usize) % PERIODS) as u64;
            if full > 0 && position % period_bytes < IN_FLIGHT_FRAMES as usize * FRAME_BYTES {
                // The end of the period just finished may not be in memory
                // yet.
                full -= 1;
                self.held_back = true;
            }
            let elapsed = clock_periods.saturating_sub(self.looked_periods);
            if elapsed < PERIODS as u64 - 1 && full > elapsed + JUMP_SLACK {
                // More periods than the clock allows: the position went
                // back or jumped. Taking them would deliver old audio; look
                // again later.
                self.glitches += 1;
                if self.glitches <= LOGGED_ERRORS {
                    println!("the input position jumped to byte {}; ignored", position);
                }
                return false;
            }
            if self.next + full > 2 * clock_periods + RUNAWAY_SLACK {
                println!(
                    "the input position ran {} periods in {} by the link's clock; recording by the clock from now on",
                    self.next + full,
                    clock_periods
                );
                self.by_clock = true;
                self.start(hc);
                return false;
            }
            full
        };
        // Asleep for most of a lap: the oldest periods have been written
        // over. Skip to the newest complete one.
        let elapsed = clock_periods.saturating_sub(self.looked_periods);
        if elapsed >= PERIODS as u64 - 1 || full >= PERIODS as u64 {
            let keep = full.min(1);
            let lost = elapsed.max(full).saturating_sub(keep);
            self.next += full - keep;
            full = keep;
            ring.set_overruns(ring.overruns().saturating_add((lost * period as u64) as u32));
            self.overruns += 1;
            if self.overruns <= 5 {
                println!("recording fell behind; {} ms lost", lost * self.timing.period_ns / 1_000_000);
            }
        }
        for k in 0..full {
            self.stream.read_period(((self.next + k) % PERIODS as u64) as usize, &mut self.scratch);
            let written = ring.write(&self.scratch);
            if written < period {
                ring.set_overruns(ring.overruns().saturating_add((period - written) as u32));
            }
        }
        self.next += full;
        self.looked_periods = clock_periods;
        if full == 0 {
            return false;
        }
        // The newest frame taken ended where the next period to take
        // begins; the controller has recorded this much since.
        let end = (self.next % PERIODS as u64) as usize * period_bytes;
        let behind = ((position + total - end) % total / FRAME_BYTES) as u64;
        ring.set_capture_clock(ring.write_pos(), now.saturating_sub(behind * 1_000_000_000 / self.timing.rate as u64));
        let _ = data_event.signal();
        true
    }
}
