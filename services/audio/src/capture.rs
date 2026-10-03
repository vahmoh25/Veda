//! Capture: the input device and the capture streams.
//!
//! An input driver produces frames into its link ring and signals
//! `data_event`; [`Capture::run`] then takes every waiting frame, converts
//! it to each stream's channel count and rate, and writes it into the
//! stream's ring (the service is the producer there). A client that does
//! not keep up loses the newest frames, which are counted as overruns; it
//! can never block the device or the other streams.
//!
//! **Echo cancellation.** Streams opened with `echo_cancel` (16 kHz mono,
//! for voice assistants) share one path: the microphone is converted to
//! 16 kHz mono, and the echo canceller removes the system's own playback,
//! taken from the mixer's [`Reference`](crate::mixer::Reference) at the
//! moment the frames were recorded (the driver publishes a capture
//! clock). The canceller estimates the remaining delay itself.
//!
//! **Power.** The device records only while some stream is open: the
//! service sets the ring's CAPTURE flag and signals `wake_event` when the
//! first stream opens and clears it when the last one closes.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vaudio::mix;
use vaudio::resample::Resampler;
use vproto::audio::{AudioError, DeviceFormat, InputHandle, InputLink, InputSpec, Ring, RingError, Role, ring};
use vrt::object::Event;

use crate::echo::EchoCanceller;
use crate::mixer::Mixer;

/// Capture stream ids carry this bit, so `close` can tell them from
/// playback streams.
pub const ID_BIT: u32 = 0x8000_0000;
/// Rate and channel count of echo-cancelled streams.
pub const ECHO_RATE: u32 = 16_000;
/// Echo path length the canceller models.
const ECHO_TAIL_MS: u32 = 250;
/// Largest block taken from the device at once.
const MAX_BLOCK: usize = 4096;
pub const MAX_INPUT_STREAMS: usize = 16;
const MAX_INPUT_STREAMS_PER_CLIENT: usize = 4;

struct Device {
    ring: Ring,
    data_event: Event,
    wake_event: Event,
    name: String,
    rate: u32,
    channels: usize,
}

struct InStream {
    owner: u64,
    name: String,
    ring: Ring,
    event: Event,
    notify_frames: u32,
    echo_cancel: bool,
    /// Device rate to stream rate (plain streams only).
    resampler: Resampler,
    overruns: u32,
}

/// The shared echo-cancelled 16 kHz mono path.
struct EchoPath {
    to_16k: Resampler,
    canceller: EchoCanceller,
    /// 16 kHz microphone samples not yet processed (the canceller works on
    /// whole frames).
    pending: Vec<i16>,
    /// Time the newest pending sample was recorded.
    pending_end_ns: u64,
}

pub struct Capture {
    device: Option<Device>,
    streams: BTreeMap<u32, InStream>,
    next_id: u32,
    pub muted: bool,
    echo: Option<EchoPath>,
    raw: Vec<i16>,
    converted: Vec<i16>,
    resampled: Vec<i16>,
}

fn ring_err(e: RingError) -> AudioError {
    match e {
        RingError::NoMemory => AudioError::NoMemory,
        RingError::BadHeader => AudioError::BadFormat,
    }
}

impl Capture {
    pub fn new() -> Capture {
        Capture {
            device: None,
            streams: BTreeMap::new(),
            next_id: 1,
            muted: false,
            echo: None,
            raw: vec![0; MAX_BLOCK * 2],
            converted: vec![0; MAX_BLOCK * 2],
            resampled: vec![0; MAX_BLOCK * 8],
        }
    }

    /// Attaches a driver's input device.
    pub fn attach(&mut self, f: &DeviceFormat) -> Result<InputLink, AudioError> {
        if self.device.is_some() {
            return Err(AudioError::Busy);
        }
        if !(8_000..=192_000).contains(&f.rate) || !(1..=2).contains(&f.channels) {
            return Err(AudioError::BadFormat);
        }
        // About half a second of slack for a busy service.
        let capacity = (f.rate / 2).max(4096);
        let (ring, vmo) = Ring::create(capacity, f.channels, f.rate, Role::Consumer).map_err(ring_err)?;
        let data_event = Event::create().map_err(|_| AudioError::NoMemory)?;
        let wake_event = Event::create().map_err(|_| AudioError::NoMemory)?;
        let dup = |e: &Event| e.0.duplicate(None).map(Event::from_handle).map_err(|_| AudioError::NoMemory);
        let link = InputLink { ring: vmo, data_event: dup(&data_event)?, wake_event: dup(&wake_event)? };
        self.device = Some(Device {
            ring,
            data_event,
            wake_event,
            name: f.name.clone(),
            rate: f.rate,
            channels: f.channels as usize,
        });
        for s in self.streams.values_mut() {
            if !s.echo_cancel {
                s.resampler = Resampler::new(f.rate, s.ring.rate(), s.ring.channels() as usize);
            }
        }
        if let Some(e) = &mut self.echo {
            e.to_16k = Resampler::new(f.rate, ECHO_RATE, 1);
            e.pending.clear();
        }
        self.update_wanted();
        Ok(link)
    }

    /// The input driver went away.
    pub fn detach(&mut self) {
        self.device = None;
    }

    pub fn device_name(&self) -> &str {
        self.device.as_ref().map_or("none", |d| d.name.as_str())
    }

    pub fn device_rate(&self) -> u32 {
        self.device.as_ref().map_or(0, |d| d.rate)
    }

    pub fn device_channels(&self) -> u32 {
        self.device.as_ref().map_or(0, |d| d.channels as u32)
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// True if an echo-cancelled stream needs the playback reference.
    pub fn wants_reference(&self) -> bool {
        self.echo.is_some()
    }

    /// The device's data event (to wait on).
    pub fn data_event(&self) -> Option<&Event> {
        self.device.as_ref().map(|d| &d.data_event)
    }

    /// Tells the driver whether to record (some stream is open).
    fn update_wanted(&mut self) {
        let want = !self.streams.is_empty();
        if let Some(d) = &self.device {
            let flags = if want { ring::flags::CAPTURE } else { 0 };
            if d.ring.consumer_flags() != flags {
                d.ring.set_consumer_flags(flags);
                let _ = d.wake_event.signal();
            }
            if !want {
                d.ring.discard_all();
            }
        }
    }

    pub fn open(&mut self, owner: u64, spec: &InputSpec) -> Result<InputHandle, AudioError> {
        if !(8_000..=48_000).contains(&spec.rate) || !(1..=2).contains(&spec.channels) {
            return Err(AudioError::BadFormat);
        }
        if spec.echo_cancel && (spec.rate != ECHO_RATE || spec.channels != 1) {
            return Err(AudioError::BadFormat);
        }
        if self.streams.len() >= MAX_INPUT_STREAMS
            || self.streams.values().filter(|s| s.owner == owner).count() >= MAX_INPUT_STREAMS_PER_CLIENT
        {
            return Err(AudioError::TooManyStreams);
        }
        let capacity = spec.buffer_frames.clamp(1024, ring::MAX_CAPACITY).next_power_of_two();
        let (ring, vmo) = Ring::create(capacity, spec.channels, spec.rate, Role::Producer).map_err(ring_err)?;
        let event = Event::create().map_err(|_| AudioError::NoMemory)?;
        let client_event = Event::from_handle(event.0.duplicate(None).map_err(|_| AudioError::NoMemory)?);
        let notify = if spec.notify_frames == 0 { (spec.rate / 50).max(1) } else { spec.notify_frames.min(capacity) };
        let id = ID_BIT | self.next_id;
        self.next_id = (self.next_id + 1) & !ID_BIT;
        if self.next_id == 0 {
            self.next_id = 1;
        }
        let device_rate = self.device_rate();
        let in_rate = if device_rate == 0 { spec.rate } else { device_rate };
        if spec.echo_cancel && self.echo.is_none() {
            self.echo = Some(EchoPath {
                to_16k: Resampler::new(in_rate, ECHO_RATE, 1),
                canceller: EchoCanceller::new(ECHO_RATE, ECHO_TAIL_MS),
                pending: Vec::new(),
                pending_end_ns: 0,
            });
        }
        self.streams.insert(
            id,
            InStream {
                owner,
                name: spec.name.chars().take(32).collect(),
                ring,
                event,
                notify_frames: notify,
                echo_cancel: spec.echo_cancel,
                resampler: Resampler::new(in_rate, spec.rate, spec.channels as usize),
                overruns: 0,
            },
        );
        self.update_wanted();
        Ok(InputHandle { id, ring: vmo, event: client_event, device_rate })
    }

    pub fn close(&mut self, owner: u64, id: u32) -> Result<(), AudioError> {
        if !self.streams.get(&id).is_some_and(|s| s.owner == owner) {
            return Err(AudioError::NoSuchStream);
        }
        if let Some(s) = self.streams.remove(&id) {
            vrt::println!("capture stream {} \"{}\" closed ({} overruns)", id & !ID_BIT, s.name, s.overruns);
        }
        self.after_close();
        Ok(())
    }

    pub fn close_owner(&mut self, owner: u64) {
        let before = self.streams.len();
        self.streams.retain(|_, s| s.owner != owner);
        if self.streams.len() != before {
            self.after_close();
        }
    }

    fn after_close(&mut self) {
        if !self.streams.values().any(|s| s.echo_cancel) {
            self.echo = None;
        }
        self.update_wanted();
    }

    /// Delivers everything the device recorded to the streams.
    pub fn run(&mut self, mixer: &Mixer, now: u64) {
        let Some(dev) = &self.device else { return };
        let _ = dev.data_event.clear();
        let dch = dev.channels;
        let rate = dev.rate;
        loop {
            let Some(dev) = &self.device else { return };
            let waiting = dev.ring.filled() as usize;
            if waiting == 0 {
                break;
            }
            let n = waiting.min(MAX_BLOCK);
            // When the newest frame in the ring was recorded; the frames
            // taken now end `after` frames before it.
            let (clock_pos, clock_ns) = dev.ring.capture_clock();
            let got = dev.ring.read(&mut self.raw[..n * dch]);
            if got == 0 {
                break;
            }
            let end_pos = dev.ring.read_pos();
            let end_ns = if clock_ns == 0 {
                now
            } else {
                let after = clock_pos.saturating_sub(end_pos);
                clock_ns.saturating_sub(after * 1_000_000_000 / rate as u64)
            };
            if self.muted {
                self.raw[..got * dch].fill(0);
            }
            self.deliver(got, dch, end_ns, mixer);
        }
    }

    /// Converts `n` device frames in `self.raw` for every stream.
    fn deliver(&mut self, n: usize, dch: usize, end_ns: u64, mixer: &Mixer) {
        let raw = &self.raw[..n * dch];
        for s in self.streams.values_mut().filter(|s| !s.echo_cancel) {
            let sch = s.ring.channels() as usize;
            let src: &[i16] = if sch == dch {
                raw
            } else {
                mix::convert_channels(raw, dch, &mut self.converted[..n * sch], sch);
                &self.converted[..n * sch]
            };
            s.resampler.push(src);
            loop {
                let got = s.resampler.pull(&mut self.resampled[..MAX_BLOCK * sch]);
                if got == 0 {
                    break;
                }
                write_stream(s, &self.resampled[..got * sch], end_ns);
            }
        }
        if let Some(echo) = &mut self.echo {
            let mono: &[i16] = if dch == 1 {
                raw
            } else {
                mix::convert_channels(raw, dch, &mut self.converted[..n], 1);
                &self.converted[..n]
            };
            echo.to_16k.push(mono);
            loop {
                let got = echo.to_16k.pull(&mut self.resampled[..MAX_BLOCK]);
                if got == 0 {
                    break;
                }
                echo.pending.extend_from_slice(&self.resampled[..got]);
            }
            echo.pending_end_ns = end_ns;
            let frame = EchoCanceller::FRAME;
            let whole = echo.pending.len() / frame * frame;
            if whole == 0 {
                return;
            }
            // The reference that was playing while these samples were
            // recorded (the newest pending sample was recorded at
            // `pending_end_ns`).
            let mut reference = vec![0i16; whole];
            let backlog_ns = (echo.pending.len() - whole) as u64 * 1_000_000_000 / ECHO_RATE as u64;
            mixer.reference(echo.pending_end_ns.saturating_sub(backlog_ns), &mut reference);
            let mut cleaned = vec![0i16; whole];
            for (i, chunk) in echo.pending[..whole].chunks_exact(frame).enumerate() {
                let r = &reference[i * frame..(i + 1) * frame];
                echo.canceller.process(chunk, r, &mut cleaned[i * frame..(i + 1) * frame]);
            }
            echo.pending.drain(..whole);
            for s in self.streams.values_mut().filter(|s| s.echo_cancel) {
                write_stream(s, &cleaned, end_ns);
            }
        }
    }

    /// Diagnostics for the log: how well the canceller is doing.
    pub fn echo_report(&self) -> Option<(f32, Option<f32>)> {
        self.echo.as_ref().map(|e| (e.canceller.erle_db(), e.canceller.echo_delay_ms()))
    }
}

/// Appends frames to a stream (dropping what does not fit), publishes when
/// the newest was recorded and wakes the reader.
fn write_stream(s: &mut InStream, frames: &[i16], recorded_ns: u64) {
    let ch = s.ring.channels() as usize;
    let written = s.ring.write(frames);
    let lost = frames.len() / ch - written;
    if lost > 0 {
        s.overruns = s.overruns.saturating_add(lost as u32);
        s.ring.set_overruns(s.overruns);
    }
    s.ring.set_capture_clock(s.ring.write_pos(), recorded_ns);
    if s.ring.filled() >= s.notify_frames {
        let _ = s.event.signal();
    }
}
