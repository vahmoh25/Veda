//! The mixer: client streams, the output and the mixing step.
//!
//! Every stream has its own resampler from the stream's rate to the
//! output rate. A mixing step produces up to `n` output frames: each
//! playing stream contributes what its ring holds (resampled, channel-
//! converted, scaled by a gain that ramps towards `volume x master`), and
//! the block written to the output is as long as the longest contribution.
//! Writing only what exists — instead of padding to a fixed size — means a
//! client that is momentarily late does not punch a hole into its own
//! audio; the driver bridges real starvation with silence.
//!
//! **Pacing.** With a sound device the mixer keeps about two periods
//! queued in the device ring ahead of the driver, which in turn keeps a few
//! periods queued in the hardware. Without a device ("null output"), the
//! mixer consumes streams in real time from a clock, so applications behave
//! the same — just silently.
//!
//! **Positions.** After each block, every stream records where its
//! consumed audio ends in output frames. As the driver reports output
//! frames played, these marks turn into a per-stream "played" position
//! that is published (with a timestamp) in the stream's ring.
//!
//! **Echo reference.** While an echo-cancelled capture stream is open, the
//! mixer also keeps the last two seconds of what it sent to the device,
//! as 16 kHz mono ([`Reference`]); [`Mixer::reference`] finds the part
//! that was playing at a given moment from the device's played position.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vaudio::mix;
use vaudio::resample::Resampler;
use vproto::audio::{
    AudioError, AudioStatus, DeviceFormat, DeviceLink, Ring, RingError, Role, StreamHandle, StreamSpec, StreamStatus,
    ring,
};
use vrt::object::{Event, Vmo};

/// Format of the null output (and the default for streams before a device
/// appears).
pub const NULL_RATE: u32 = 48_000;
pub const NULL_CHANNELS: usize = 2;
/// Largest block mixed in one step.
const MAX_BLOCK: usize = 4096;
/// Periods kept queued in the device ring ahead of the driver.
const TARGET_PERIODS: u32 = 2;
/// A stream counts as active for this long after it last produced audio.
const ACTIVE_HOLD_NS: u64 = 300_000_000;
/// Limits.
pub const MAX_STREAMS: usize = 64;
pub const MAX_STREAMS_PER_CLIENT: usize = 16;

/// One playback stream.
pub struct Stream {
    pub owner: u64,
    pub name: String,
    pub ring: Ring,
    pub event: Event,
    pub notify_frames: u32,
    pub paused: bool,
    /// A pause was requested: fade out during the next block first.
    pub fading_out: bool,
    pub volume: f32,
    /// Q15 gain reached at the end of the last block.
    gain: i32,
    resampler: Resampler,
    pub underruns: u32,
    starved: bool,
    produced_any: bool,
    /// (output frame at the end of a block, stream position then).
    marks: VecDeque<(u64, u64)>,
    /// The last mark that has been played completely.
    prev_mark: (u64, u64),
    pub played: u64,
    last_active_ns: u64,
    /// At the end of the stream the resampler's delay line was flushed
    /// with silence, so everything consumed has been mixed.
    tail_flushed: bool,
}

impl Stream {
    /// Stream position of the audio mixed so far (input consumed minus what
    /// the resampler still holds or delays).
    fn mixed_position(&self) -> u64 {
        if self.tail_flushed && self.resampler.buffered() == 0 {
            return self.ring.read_pos();
        }
        let lag = self.resampler.buffered() as u64 + self.resampler.delay_frames() as u64;
        self.ring.read_pos().saturating_sub(lag).max(self.prev_mark.1)
    }

    /// At the end of a stream (the producer set END and the ring is empty),
    /// pushes the resampler's delay worth of silence so that the last
    /// samples are played instead of staying in the filter.
    fn flush_tail(&mut self) {
        let ended = self.ring.producer_flags() & ring::flags::END != 0;
        if !ended {
            self.tail_flushed = false;
            return;
        }
        if !self.tail_flushed && self.ring.filled() == 0 {
            let silence = vec![0i16; self.resampler.delay_frames() * self.ring.channels() as usize];
            self.resampler.push(&silence);
            self.tail_flushed = true;
        }
    }

    fn reset_positions(&mut self, output_frame: u64) {
        self.marks.clear();
        let pos = self.ring.read_pos();
        self.prev_mark = (output_frame, pos);
        self.played = pos;
    }
}

/// Rate of the echo reference.
pub const REFERENCE_RATE: u32 = 16_000;
/// Reference kept, in 16 kHz samples (two seconds).
const REFERENCE_KEEP: usize = 32_000;

/// The recent output as 16 kHz mono, for echo cancellation.
pub struct Reference {
    resampler: Resampler,
    mono: Vec<i16>,
    pulled: Vec<i16>,
    history: VecDeque<i16>,
    /// Reference sample index of `history[0]`.
    start: u64,
    /// Output frame at which reference sample 0 was produced.
    origin: u64,
    /// The output rate.
    rate: u32,
}

impl Reference {
    fn new(rate: u32, origin: u64) -> Reference {
        Reference {
            resampler: Resampler::new(rate, REFERENCE_RATE, 1),
            mono: vec![0; MAX_BLOCK],
            pulled: vec![0; MAX_BLOCK],
            history: VecDeque::with_capacity(REFERENCE_KEEP + MAX_BLOCK),
            start: 0,
            origin,
            rate,
        }
    }

    /// Appends output frames (`channels` interleaved).
    fn push(&mut self, frames: &[i16], channels: usize) {
        let n = frames.len() / channels;
        for chunk in (0..n).step_by(MAX_BLOCK) {
            let m = (n - chunk).min(MAX_BLOCK);
            let src = &frames[chunk * channels..(chunk + m) * channels];
            if channels == 1 {
                self.mono[..m].copy_from_slice(src);
            } else {
                mix::convert_channels(src, channels, &mut self.mono[..m], 1);
            }
            self.resampler.push(&self.mono[..m]);
            loop {
                let got = self.resampler.pull(&mut self.pulled);
                if got == 0 {
                    break;
                }
                self.history.extend(&self.pulled[..got]);
            }
        }
        let excess = self.history.len().saturating_sub(REFERENCE_KEEP);
        if excess > 0 {
            self.history.drain(..excess);
            self.start += excess as u64;
        }
    }
}

/// Where mixed audio goes.
pub enum Output {
    /// A sound driver's ring.
    Device { ring: Ring, data_event: Event, space_event: Event, name: String, period: u32 },
    /// No sound hardware: consume in real time.
    Null { started_ns: u64, consumed: u64 },
}

pub struct Mixer {
    pub streams: BTreeMap<u32, Stream>,
    pub output: Output,
    pub rate: u32,
    pub channels: usize,
    /// Frames emitted to the output since it was set up.
    pub written: u64,
    pub master: f32,
    pub muted: bool,
    /// The echo reference, while someone needs it.
    reference: Option<Reference>,
    next_id: u32,
    acc: Vec<i32>,
    out: Vec<i16>,
    input: Vec<i16>,
    resampled: Vec<i16>,
    converted: Vec<i16>,
}

fn ring_err(e: RingError) -> AudioError {
    match e {
        RingError::NoMemory => AudioError::NoMemory,
        RingError::BadHeader => AudioError::BadFormat,
    }
}

impl Mixer {
    pub fn new() -> Mixer {
        let max = MAX_BLOCK * 2;
        Mixer {
            streams: BTreeMap::new(),
            output: Output::Null { started_ns: vrt::time::now_ns(), consumed: 0 },
            rate: NULL_RATE,
            channels: NULL_CHANNELS,
            written: 0,
            master: 1.0,
            muted: false,
            reference: None,
            next_id: 1,
            acc: vec![0; max],
            out: vec![0; max],
            // Enough input for MAX_BLOCK output frames at up to 4x the rate.
            input: vec![0; max * 4 + 512],
            resampled: vec![0; max],
            converted: vec![0; max],
        }
    }

    fn set_format(&mut self, rate: u32, channels: usize) {
        self.rate = rate;
        self.channels = channels;
        if self.reference.is_some() {
            self.reference = Some(Reference::new(rate, self.written));
        }
        for s in self.streams.values_mut() {
            s.resampler = Resampler::new(s.ring.rate(), rate, s.ring.channels() as usize);
            s.tail_flushed = false;
        }
    }

    /// Attaches a driver's output device.
    pub fn attach(&mut self, f: &DeviceFormat) -> Result<DeviceLink, AudioError> {
        if matches!(self.output, Output::Device { .. }) {
            return Err(AudioError::Busy);
        }
        if !(8_000..=192_000).contains(&f.rate)
            || !(1..=2).contains(&f.channels)
            || !(32..=8192).contains(&f.period_frames)
        {
            return Err(AudioError::BadFormat);
        }
        let capacity = (f.period_frames * 8).max(4096);
        let (ring, vmo) = Ring::create(capacity, f.channels, f.rate, Role::Producer).map_err(ring_err)?;
        let data_event = Event::create().map_err(|_| AudioError::NoMemory)?;
        let space_event = Event::create().map_err(|_| AudioError::NoMemory)?;
        let dup = |e: &Event| e.0.duplicate(None).map(Event::from_handle).map_err(|_| AudioError::NoMemory);
        let link = DeviceLink { ring: vmo, data_event: dup(&data_event)?, space_event: dup(&space_event)? };
        if f.rate != self.rate || f.channels as usize != self.channels {
            self.set_format(f.rate, f.channels as usize);
        }
        self.written = 0;
        for s in self.streams.values_mut() {
            s.reset_positions(0);
        }
        if self.reference.is_some() {
            self.reference = Some(Reference::new(f.rate, 0));
        }
        self.output = Output::Device { ring, data_event, space_event, name: f.name.clone(), period: f.period_frames };
        Ok(link)
    }

    /// The driver went away: continue with the null output.
    pub fn detach(&mut self) {
        let now = vrt::time::now_ns();
        self.output = Output::Null { started_ns: now, consumed: 0 };
        if self.rate != NULL_RATE || self.channels != NULL_CHANNELS {
            self.set_format(NULL_RATE, NULL_CHANNELS);
        }
        let w = self.written;
        for s in self.streams.values_mut() {
            s.reset_positions(w);
        }
    }

    /// Keeps (or stops keeping) the echo reference.
    pub fn set_reference_wanted(&mut self, wanted: bool) {
        if wanted && self.reference.is_none() {
            self.reference = Some(Reference::new(self.rate, self.written));
        } else if !wanted {
            self.reference = None;
        }
    }

    /// Fills `out` with the 16 kHz mono reference that was playing up to
    /// monotonic time `end_ns` (silence where nothing was playing or the
    /// history does not reach).
    pub fn reference(&self, end_ns: u64, out: &mut [i16]) {
        out.fill(0);
        let (Some(r), Output::Device { ring, .. }) = (&self.reference, &self.output) else { return };
        let (played, played_ns) = ring.played();
        if played_ns == 0 {
            return;
        }
        // The output frame playing at `end_ns`, extrapolated from the
        // driver's last report.
        let dt = end_ns as i128 - played_ns as i128;
        let frame = played as i128 + dt * r.rate as i128 / 1_000_000_000;
        let end = (frame - r.origin as i128) * REFERENCE_RATE as i128 / r.rate as i128;
        let start = end - out.len() as i128;
        for (i, o) in out.iter_mut().enumerate() {
            let idx = start + i as i128 - r.start as i128;
            if idx >= 0 && (idx as usize) < r.history.len() {
                *o = r.history[idx as usize];
            }
        }
    }

    pub fn device_name(&self) -> &str {
        match &self.output {
            Output::Device { name, .. } => name,
            Output::Null { .. } => "none",
        }
    }

    /// The space event of the device link (to wait on).
    pub fn space_event(&self) -> Option<&Event> {
        match &self.output {
            Output::Device { space_event, .. } => Some(space_event),
            Output::Null { .. } => None,
        }
    }

    /// Output frames that have been played.
    fn output_played(&self) -> u64 {
        match &self.output {
            Output::Device { ring, .. } => ring.played().0.min(self.written),
            Output::Null { .. } => self.written,
        }
    }

    pub fn open(&mut self, owner: u64, spec: &StreamSpec) -> Result<StreamHandle, AudioError> {
        if !(8_000..=192_000).contains(&spec.rate) || !(1..=2).contains(&spec.channels) {
            return Err(AudioError::BadFormat);
        }
        if self.streams.len() >= MAX_STREAMS
            || self.streams.values().filter(|s| s.owner == owner).count() >= MAX_STREAMS_PER_CLIENT
        {
            return Err(AudioError::TooManyStreams);
        }
        let capacity = spec.buffer_frames.clamp(1024, ring::MAX_CAPACITY).next_power_of_two();
        let (ring, vmo): (Ring, Vmo) =
            Ring::create(capacity, spec.channels, spec.rate, Role::Consumer).map_err(ring_err)?;
        let event = Event::create().map_err(|_| AudioError::NoMemory)?;
        let client_event = Event::from_handle(event.0.duplicate(None).map_err(|_| AudioError::NoMemory)?);
        let notify = if spec.notify_frames == 0 { capacity / 4 } else { spec.notify_frames.min(capacity) };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let paused = spec.paused;
        if paused {
            ring.set_consumer_flags(ring::flags::PAUSED);
        }
        let _ = event.signal();
        let mut s = Stream {
            owner,
            name: spec.name.chars().take(32).collect(),
            ring,
            event,
            notify_frames: notify,
            paused,
            fading_out: false,
            volume: if spec.volume.is_finite() { spec.volume.clamp(0.0, 1.0) } else { 1.0 },
            gain: 0,
            resampler: Resampler::new(spec.rate, self.rate, spec.channels as usize),
            underruns: 0,
            starved: false,
            produced_any: false,
            marks: VecDeque::new(),
            prev_mark: (0, 0),
            played: 0,
            last_active_ns: vrt::time::now_ns(),
            tail_flushed: false,
        };
        s.reset_positions(self.written);
        self.streams.insert(id, s);
        Ok(StreamHandle { id, ring: vmo, event: client_event, device_rate: self.rate })
    }

    fn stream(&mut self, owner: u64, id: u32) -> Result<&mut Stream, AudioError> {
        self.streams.get_mut(&id).filter(|s| s.owner == owner).ok_or(AudioError::NoSuchStream)
    }

    pub fn close(&mut self, owner: u64, id: u32) -> Result<(), AudioError> {
        self.stream(owner, id)?;
        if let Some(s) = self.streams.remove(&id) {
            vrt::println!("stream {} \"{}\" closed ({} underruns)", id, s.name, s.underruns);
        }
        Ok(())
    }

    /// Removes every stream of a client that disconnected.
    pub fn close_owner(&mut self, owner: u64) {
        self.streams.retain(|_, s| s.owner != owner);
    }

    pub fn set_paused(&mut self, owner: u64, id: u32, paused: bool) -> Result<(), AudioError> {
        let s = self.stream(owner, id)?;
        if paused {
            if !s.paused {
                s.fading_out = true;
            }
        } else {
            s.paused = false;
            s.fading_out = false;
            s.ring.set_consumer_flags(0);
            s.last_active_ns = vrt::time::now_ns();
        }
        let _ = s.event.signal();
        Ok(())
    }

    pub fn set_volume(&mut self, owner: u64, id: u32, volume: f32) -> Result<(), AudioError> {
        let s = self.stream(owner, id)?;
        s.volume = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
        Ok(())
    }

    pub fn flush(&mut self, owner: u64, id: u32) -> Result<u64, AudioError> {
        let written = self.written;
        let rate = self.rate;
        let s = self.stream(owner, id)?;
        s.ring.discard_all();
        s.resampler = Resampler::new(s.ring.rate(), rate, s.ring.channels() as usize);
        s.tail_flushed = false;
        s.reset_positions(written);
        // Fade the new audio in so it does not click against the old.
        s.gain = 0;
        s.ring.set_played(s.played, vrt::time::now_ns());
        let _ = s.event.signal();
        Ok(s.played)
    }

    pub fn stream_status(&mut self, owner: u64, id: u32) -> Result<StreamStatus, AudioError> {
        let latency_ns = self.latency_ns();
        let s = self.stream(owner, id)?;
        Ok(StreamStatus {
            written: s.ring.write_pos(),
            consumed: s.ring.read_pos(),
            played: s.played,
            latency_ns,
            underruns: s.underruns,
            paused: s.paused,
        })
    }

    fn latency_ns(&self) -> u64 {
        let pending = self.written.saturating_sub(self.output_played());
        pending * 1_000_000_000 / self.rate as u64
    }

    pub fn status(&self) -> AudioStatus {
        let (period, underruns) = match &self.output {
            Output::Device { ring, period, .. } => (*period, ring.underruns() as u64),
            Output::Null { .. } => (0, 0),
        };
        AudioStatus {
            device: self.device_name().into(),
            rate: self.rate,
            channels: self.channels as u32,
            period_frames: period,
            latency_ns: self.latency_ns(),
            master_volume: self.master,
            muted: self.muted,
            streams: self.streams.len() as u32,
            underruns,
            input_device: String::new(),
            input_rate: 0,
            input_channels: 0,
            input_muted: false,
            input_streams: 0,
        }
    }

    /// True if some stream is playing (unpaused and recently active or
    /// holding data).
    fn active(&self, now: u64) -> bool {
        self.streams.values().any(|s| {
            let filled = s.ring.filled() > 0;
            // A stream that ended and has been drained is finished, not
            // starving.
            let finished = !filled && s.ring.producer_flags() & ring::flags::END != 0;
            !s.paused && !finished && (filled || now.saturating_sub(s.last_active_ns) < ACTIVE_HOLD_NS)
        })
    }

    /// When the main loop should call [`Mixer::run`] again at the latest.
    pub fn deadline(&self, now: u64) -> u64 {
        let playing = self.streams.values().any(|s| !s.paused);
        match &self.output {
            Output::Null { .. } if playing => now + 10_000_000,
            Output::Device { ring, .. } if playing => {
                if ring.consumer_flags() & ring::flags::RUNNING != 0 {
                    // The driver wakes us; this is only a safety net.
                    now + 50_000_000
                } else {
                    now + 10_000_000
                }
            }
            _ => vabi::DEADLINE_INFINITE,
        }
    }

    /// Mixes one block of up to `n` frames into `self.out`; returns the
    /// number of frames produced.
    fn mix_block(&mut self, n: usize, now: u64) -> usize {
        let dch = self.channels;
        let n = n.min(MAX_BLOCK);
        self.acc[..n * dch].fill(0);
        let master = if self.muted { 0.0 } else { self.master };
        let mut produced_max = 0;
        let mut produced_by: Vec<(u32, usize)> = Vec::with_capacity(self.streams.len());
        for (&id, s) in self.streams.iter_mut() {
            if s.paused {
                continue;
            }
            s.flush_tail();
            let sch = s.ring.channels() as usize;
            let target = if s.fading_out { 0 } else { mix::gain_q15(s.volume * master) };
            let g_start = s.gain;
            let gain_at = |done: usize| g_start + ((target - g_start) as i64 * done as i64 / n as i64) as i32;
            let mut produced = 0;
            while produced < n {
                let want = n - produced;
                let need = s.resampler.input_needed(want);
                let take = need.min(s.ring.filled() as usize).min(self.input.len() / sch);
                if take > 0 {
                    let got = s.ring.read(&mut self.input[..take * sch]);
                    s.resampler.push(&self.input[..got * sch]);
                }
                let got = s.resampler.pull(&mut self.resampled[..want * sch]);
                if got == 0 {
                    break;
                }
                let src: &[i16] = if sch == dch {
                    &self.resampled[..got * sch]
                } else {
                    mix::convert_channels(&self.resampled[..got * sch], sch, &mut self.converted[..got * dch], dch);
                    &self.converted[..got * dch]
                };
                let range = produced * dch..(produced + got) * dch;
                mix::mix_into(&mut self.acc[range], src, dch, gain_at(produced), gain_at(produced + got));
                produced += got;
            }
            s.gain = gain_at(produced);
            if produced > 0 {
                s.last_active_ns = now;
                s.produced_any = true;
            }
            if s.fading_out && (produced == n || produced == 0 || s.gain == 0) {
                s.fading_out = false;
                s.paused = true;
                s.gain = 0;
                s.ring.set_consumer_flags(ring::flags::PAUSED);
                let _ = s.event.signal();
            }
            produced_max = produced_max.max(produced);
            produced_by.push((id, produced));
        }
        if produced_max == 0 {
            return 0;
        }
        // Streams that fell short of the block have a gap: an underrun.
        let end = self.written + produced_max as u64;
        for (id, produced) in produced_by {
            let Some(s) = self.streams.get_mut(&id) else { continue };
            let ended = s.ring.producer_flags() & ring::flags::END != 0;
            if produced < produced_max && s.produced_any && !ended {
                if !s.starved {
                    s.starved = true;
                    s.underruns += 1;
                }
            } else if produced > 0 {
                s.starved = false;
            }
            let pos = s.mixed_position();
            if s.marks.len() >= 256 {
                s.marks.pop_front();
            }
            s.marks.push_back((end, pos));
        }
        mix::finish_mix(&self.acc[..produced_max * dch], &mut self.out[..produced_max * dch], mix::UNITY_Q15);
        produced_max
    }

    /// Mixes as much as the output wants, publishes positions and signals
    /// streams with free space.
    pub fn run(&mut self, now: u64) {
        let active = self.active(now);
        match &self.output {
            Output::Device { ring, .. } => {
                ring.set_producer_flags(if active { ring::flags::ACTIVE } else { 0 });
            }
            Output::Null { .. } => {}
        }
        let mut emitted = false;
        for _ in 0..8 {
            let room = match &self.output {
                Output::Device { ring, period, .. } => {
                    let target = (period * TARGET_PERIODS).min(ring.capacity());
                    let fill = ring.filled();
                    if fill >= target { 0 } else { (target - fill).min(ring.free()) as usize }
                }
                Output::Null { started_ns, consumed } => {
                    let due = (now.saturating_sub(*started_ns) as u128 * self.rate as u128 / 1_000_000_000) as u64;
                    due.saturating_sub(*consumed).min(MAX_BLOCK as u64) as usize
                }
            };
            if room == 0 {
                break;
            }
            let n = self.mix_block(room, now);
            match &mut self.output {
                Output::Device { ring, .. } => {
                    if n == 0 {
                        break;
                    }
                    ring.write(&self.out[..n * self.channels]);
                    if let Some(r) = &mut self.reference {
                        r.push(&self.out[..n * self.channels], self.channels);
                    }
                    self.written += n as u64;
                    emitted = true;
                }
                Output::Null { consumed, started_ns } => {
                    // Time passes whether or not anything played.
                    *consumed += room as u64;
                    self.written += room as u64;
                    // Do not accumulate a backlog after long idle periods.
                    let due = (now.saturating_sub(*started_ns) as u128 * self.rate as u128 / 1_000_000_000) as u64;
                    if due > *consumed + self.rate as u64 {
                        *consumed = due;
                    }
                }
            }
        }
        if emitted && let Output::Device { data_event, .. } = &self.output {
            let _ = data_event.signal();
        }
        self.publish(now);
    }

    /// Publishes per-stream played positions and wakes writers.
    fn publish(&mut self, now: u64) {
        let played_out = self.output_played();
        let latency = self.written.saturating_sub(played_out);
        for s in self.streams.values_mut() {
            while let Some(&(d, p)) = s.marks.front() {
                if d <= played_out {
                    s.prev_mark = (d, p);
                    s.marks.pop_front();
                } else {
                    break;
                }
            }
            let pos = match s.marks.front() {
                Some(&(d, p)) => {
                    let (d0, p0) = s.prev_mark;
                    if d > d0 && played_out > d0 && p > p0 {
                        p0 + ((p - p0) as u128 * (played_out - d0) as u128 / (d - d0) as u128) as u64
                    } else {
                        p0
                    }
                }
                None => s.prev_mark.1,
            };
            s.played = s.played.max(pos).min(s.ring.read_pos());
            s.ring.set_played(s.played, now);
            s.ring.set_latency((latency * s.ring.rate() as u64 / self.rate as u64) as u32);
            s.ring.set_underruns(s.underruns);
            if !s.paused && s.ring.free() >= s.notify_frames {
                let _ = s.event.signal();
            }
        }
    }
}
