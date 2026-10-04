//! The agent's ears and voice.
//!
//! * The microphone: a 16 kHz mono capture stream with echo cancellation,
//!   so the agent can listen while it talks (and be interrupted) without
//!   hearing itself.
//! * The voice: a 24 kHz mono playback stream. Deepgram sends speech faster
//!   than real time; what does not fit in the stream's ring waits in a
//!   queue. When the user interrupts, [`Voice::stop_speaking`] drops the
//!   queue and flushes the stream at once.
//! * Levels for the interface's animation: how loud the voice that is
//!   audible *right now* is (from the stream's played position), and how
//!   loud the microphone is.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use vabi::{RawHandle, signals};
use vproto::audio::{InputSpec, InputStream, OutputStream, StreamSpec, audio};

/// Microphone rate (what Deepgram receives).
pub const MIC_RATE: u32 = 16_000;
/// Voice rate (what Deepgram sends).
pub const VOICE_RATE: u32 = 24_000;
/// Microphone audio goes out in 20 ms packets.
pub const MIC_PACKET: usize = (MIC_RATE / 50) as usize;
/// The voice stream's buffer (Deepgram's voice arrives faster than real
/// time and is written ahead).
const STREAM_FRAMES: u32 = VOICE_RATE * 4;
/// Voice samples kept to measure the audible level: everything the stream
/// may still be playing, and a little more.
const RECENT: usize = (STREAM_FRAMES + VOICE_RATE) as usize;

pub struct Voice {
    client: audio::Client,
    mic: Option<InputStream>,
    speaker: Option<OutputStream>,
    /// Voice that did not fit into the stream yet.
    queue: VecDeque<i16>,
    /// The most recent voice samples written; `recent_end` is the frame
    /// index just after the last one.
    recent: VecDeque<i16>,
    recent_end: u64,
    mic_level: f32,
    scratch: Vec<i16>,
}

/// Loudness (0..=1) of samples: RMS mapped from -50..-6 dBFS.
fn loudness(samples: impl Iterator<Item = i16>) -> f32 {
    let mut sum = 0f64;
    let mut n = 0u32;
    for s in samples {
        sum += (s as f64) * (s as f64);
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let rms = vmath_sqrt((sum / n as f64) as f32) / 32768.0;
    if rms < 1e-5 {
        return 0.0;
    }
    let db = 20.0 * vmath::f32::log10(rms);
    ((db + 50.0) / 44.0).clamp(0.0, 1.0)
}

fn vmath_sqrt(x: f32) -> f32 {
    vmath::f32::sqrt(x)
}

impl Voice {
    /// Connects to the audio service (streams are opened on demand).
    pub fn new() -> Option<Voice> {
        let client = audio::Client::new(vproto::connect(audio::NAME).ok()?);
        Some(Voice {
            client,
            mic: None,
            speaker: None,
            queue: VecDeque::new(),
            recent: VecDeque::with_capacity(RECENT),
            recent_end: 0,
            mic_level: 0.0,
            scratch: vec![0; 4096],
        })
    }

    /// Starts listening. Echo cancellation is asked for; without it (an old
    /// audio service) the plain microphone is used.
    pub fn open_mic(&mut self) -> bool {
        if self.mic.is_some() {
            return true;
        }
        let mut spec = InputSpec::mono(MIC_RATE, 2000, "Agent");
        spec.echo_cancel = true;
        spec.notify_frames = MIC_PACKET as u32;
        let mic = InputStream::open(&self.client, spec.clone())
            .or_else(|_| InputStream::open(&self.client, InputSpec { echo_cancel: false, ..spec }));
        match mic {
            Ok(m) => {
                self.mic = Some(m);
                true
            }
            Err(e) => {
                vrt::println!("cannot open the microphone: {:?}", e);
                false
            }
        }
    }

    pub fn close_mic(&mut self) {
        if let Some(m) = self.mic.take() {
            let _ = self.client.close(m.id);
        }
        self.mic_level = 0.0;
    }

    /// Takes everything the microphone recorded (in whole packets) into
    /// `out`; returns how many samples.
    pub fn read_mic(&mut self, out: &mut Vec<i16>) -> usize {
        let Some(mic) = &self.mic else { return 0 };
        let mut total = 0;
        loop {
            let n = mic.read(&mut self.scratch);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&self.scratch[..n]);
            total += n;
        }
        if total > 0 {
            let fresh = loudness(out[out.len() - total..].iter().copied());
            // Rise fast, fall slowly.
            self.mic_level = if fresh > self.mic_level { fresh } else { self.mic_level * 0.7 + fresh * 0.3 };
        }
        total
    }

    pub fn mic_level(&self) -> f32 {
        self.mic_level
    }

    fn open_speaker(&mut self) -> bool {
        if self.speaker.is_some() {
            return true;
        }
        let spec = StreamSpec {
            rate: VOICE_RATE,
            channels: 1,
            buffer_frames: STREAM_FRAMES,
            notify_frames: VOICE_RATE / 10,
            name: "Agent".into(),
            paused: false,
            volume: 1.0,
        };
        match OutputStream::open(&self.client, spec) {
            Ok(s) => {
                self.recent_end = s.written();
                self.speaker = Some(s);
                true
            }
            Err(e) => {
                vrt::println!("cannot open the speaker: {:?}", e);
                false
            }
        }
    }

    /// Closes the voice stream (when the conversation ends).
    pub fn close_speaker(&mut self) {
        self.queue.clear();
        if let Some(s) = self.speaker.take() {
            let _ = self.client.close(s.id);
        }
        self.recent.clear();
    }

    /// Plays voice samples (queued behind what is already playing).
    pub fn speak(&mut self, pcm: &[i16]) {
        if !self.open_speaker() {
            return;
        }
        self.queue.extend(pcm.iter().copied());
        self.pump();
    }

    /// Moves queued voice into the stream as space allows.
    pub fn pump(&mut self) {
        let Some(s) = &self.speaker else { return };
        while !self.queue.is_empty() {
            let free = s.free() as usize;
            if free == 0 {
                break;
            }
            let n = free.min(self.queue.len()).min(self.scratch.len());
            for (dst, src) in self.scratch[..n].iter_mut().zip(self.queue.drain(..n)) {
                *dst = src;
            }
            let written = s.write(&self.scratch[..n]);
            for &x in &self.scratch[..written] {
                if self.recent.len() >= RECENT {
                    self.recent.pop_front();
                }
                self.recent.push_back(x);
            }
            self.recent_end += written as u64;
            if written < n {
                // Put back what did not fit (rare: the ring filled up).
                for &x in self.scratch[written..n].iter().rev() {
                    self.queue.push_front(x);
                }
                break;
            }
        }
    }

    /// Stops talking at once (the user interrupted).
    pub fn stop_speaking(&mut self) {
        self.queue.clear();
        if let Some(s) = &self.speaker {
            let _ = self.client.flush(s.id);
            self.recent_end = s.written();
        }
        self.recent.clear();
    }

    /// Voice is queued or still playing.
    pub fn speaking(&self) -> bool {
        match &self.speaker {
            Some(s) => !self.queue.is_empty() || s.played_now() + (VOICE_RATE as u64 / 50) < s.written(),
            None => false,
        }
    }

    /// Loudness of the voice audible right now (0..=1).
    pub fn output_level(&self) -> f32 {
        let Some(s) = &self.speaker else { return 0.0 };
        let played = s.played_now();
        let window = (VOICE_RATE / 40) as u64; // 25 ms
        let start = self.recent_end.saturating_sub(self.recent.len() as u64);
        if played < start + window || played > self.recent_end {
            return 0.0;
        }
        let from = (played - window - start) as usize;
        loudness(self.recent.range(from..from + window as usize).copied())
    }

    /// Level of the voice audible right now, over 20 ms (dBFS; -100 when
    /// nothing plays): what the echo gate compares the microphone with.
    pub fn output_db(&self) -> f32 {
        let Some(s) = &self.speaker else { return -100.0 };
        let played = s.played_now();
        let window = (VOICE_RATE / 50) as u64;
        let start = self.recent_end.saturating_sub(self.recent.len() as u64);
        if played < start + window || played > self.recent_end {
            return -100.0;
        }
        let from = (played - window - start) as usize;
        let mut buf = [0i16; (VOICE_RATE / 50) as usize];
        for (dst, &src) in buf.iter_mut().zip(self.recent.range(from..from + window as usize)) {
            *dst = src;
        }
        vaudio::level::rms_dbfs(&buf)
    }

    /// What to wait on: the microphone's event, and the speaker's while
    /// voice is queued.
    pub fn wait_handles(&self) -> Vec<(RawHandle, u32)> {
        let mut v = Vec::new();
        if let Some(m) = &self.mic {
            v.push((m.event().raw(), signals::SIGNALED));
        }
        if let (Some(s), false) = (&self.speaker, self.queue.is_empty()) {
            v.push((s.event().raw(), signals::SIGNALED));
        }
        v
    }
}
