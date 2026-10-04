//! The agent's ears and voice.
//!
//! * The microphone: a 16 kHz capture stream with echo cancellation, so
//!   the agent can listen while it talks (and be interrupted) without
//!   hearing itself. Beside the cleaned microphone the audio service
//!   delivers what was playing when it was recorded and the microphone as
//!   recorded, for the echo gate (`vagent::gate`).
//! * The voice: a 24 kHz mono playback stream. Deepgram's speech arrives in
//!   bursts, at times ahead of what plays and at times behind; what does
//!   not fit in the stream's ring waits in a queue. An answer starts
//!   playing once some of it has arrived (or all of it), a quarter of a
//!   second at first: starting on the first packet would turn every late
//!   one into a gap. Each time the voice runs dry in the middle of an
//!   answer it gathers more before going on, and every answer after that
//!   (up to three quarters of a second); answers that play through bring
//!   it back down little by little. A jitter buffer, as in a phone call.
//!   When the user interrupts, [`Voice::stop_speaking`] drops the queue and
//!   flushes the stream at once. The stream stays open between answers;
//!   at the end of each it is marked as ended, so the audio service plays
//!   out its last samples and does not take the silence after it for the
//!   voice starving.
//! * Levels for the interface's animation: how loud the voice that is
//!   audible *right now* is (from the stream's played position), and how
//!   loud the microphone is.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use vabi::{RawHandle, signals};
use vproto::audio::{InputSpec, InputStream, OutputStream, StreamSpec, audio, ring};

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
/// An answer starts playing once this much of it is here (in ms), at
/// least and at most ...
const PREBUFFER_MIN_MS: u32 = 250;
const PREBUFFER_MAX_MS: u32 = 750;
/// ... more by this each time the voice runs dry mid-answer, less by this
/// after an answer that played through ...
const PREBUFFER_STEP_MS: u32 = 125;
const PREBUFFER_RELAX_MS: u32 = 25;
/// ... or once all of it is here, or this long after that much should have
/// arrived.
const PREBUFFER_GRACE_MS: u32 = 150;

pub struct Voice {
    client: audio::Client,
    mic: Option<InputStream>,
    speaker: Option<OutputStream>,
    /// Voice that did not fit into the stream yet.
    queue: VecDeque<i16>,
    /// Since when the start of an answer is held back to gather some of
    /// it first.
    gathering_since: Option<u64>,
    /// All of the current answer has arrived (or none is under way).
    complete: bool,
    /// How much of an answer is gathered before it plays (ms), and whether
    /// the current one ran dry.
    prebuffer_ms: u32,
    ran_dry: bool,
    /// The stream is marked as ended (all of an answer is in it).
    ended: bool,
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
            gathering_since: None,
            complete: true,
            prebuffer_ms: PREBUFFER_MIN_MS,
            ran_dry: false,
            ended: false,
            recent: VecDeque::with_capacity(RECENT),
            recent_end: 0,
            mic_level: 0.0,
            scratch: vec![0; 4096],
        })
    }

    /// Starts listening. Echo cancellation is asked for, with what was
    /// playing and the recorded microphone beside it; an older audio
    /// service is asked for less (the plain microphone at the least).
    pub fn open_mic(&mut self) -> bool {
        if self.mic.is_some() {
            return true;
        }
        let mut spec = InputSpec::mono(MIC_RATE, 2000, "Agent");
        spec.echo_cancel = true;
        spec.notify_frames = MIC_PACKET as u32;
        let mic = InputStream::open(&self.client, InputSpec { channels: 3, ..spec.clone() })
            .or_else(|_| InputStream::open(&self.client, spec.clone()))
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

    /// Takes everything the microphone recorded into `out`, and what was
    /// playing at the time and the microphone as recorded (before echo
    /// cancellation) into `playing` and `recorded` when the audio service
    /// provides them; returns how many samples went into `out`.
    pub fn read_mic(&mut self, out: &mut Vec<i16>, playing: &mut Vec<i16>, recorded: &mut Vec<i16>) -> usize {
        let Some(mic) = &self.mic else { return 0 };
        let ch = mic.channels() as usize;
        let mut total = 0;
        loop {
            let n = mic.read(&mut self.scratch);
            if n == 0 {
                break;
            }
            for f in self.scratch[..n * ch].chunks_exact(ch) {
                out.push(f[0]);
                if ch >= 3 {
                    playing.push(f[1]);
                    recorded.push(f[2]);
                }
            }
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

    /// Makes other sound play quieter (music under a conversation), or not.
    pub fn duck_others(&self, on: bool) {
        let _ = self.client.set_ducking(on);
    }

    /// Closes the voice stream (when the conversation ends).
    pub fn close_speaker(&mut self) {
        self.queue.clear();
        self.gathering_since = None;
        self.complete = true;
        self.ended = false;
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
        if self.gathering_since.is_none() && !self.speaking() {
            // A new answer, or the voice ran dry: gather some first (more
            // from now on in the second case).
            if self.complete {
                self.ran_dry = false;
            } else {
                self.ran_dry = true;
                if self.prebuffer_ms < PREBUFFER_MAX_MS {
                    self.prebuffer_ms = (self.prebuffer_ms + PREBUFFER_STEP_MS).min(PREBUFFER_MAX_MS);
                    vrt::println!(
                        "the voice ran dry mid-answer; gathering {} ms before playing now",
                        self.prebuffer_ms
                    );
                }
            }
            self.gathering_since = Some(vrt::time::now_ns());
            self.complete = false;
        }
        self.queue.extend(pcm.iter().copied());
        self.pump();
    }

    /// All of the current answer has arrived: play what is gathered.
    pub fn answer_complete(&mut self) {
        if !self.complete && !self.ran_dry {
            self.prebuffer_ms = self.prebuffer_ms.saturating_sub(PREBUFFER_RELAX_MS).max(PREBUFFER_MIN_MS);
        }
        self.complete = true;
        self.pump();
    }

    /// Moves queued voice into the stream as space allows.
    pub fn pump(&mut self) {
        let Some(s) = &self.speaker else { return };
        if let Some(since) = self.gathering_since {
            let frames = (self.prebuffer_ms * (VOICE_RATE / 1000)) as usize;
            let wait_ns = (self.prebuffer_ms + PREBUFFER_GRACE_MS) as u64 * 1_000_000;
            let enough =
                self.queue.len() >= frames || self.complete || vrt::time::now_ns().saturating_sub(since) >= wait_ns;
            if !enough {
                return;
            }
            self.gathering_since = None;
        }
        // Signalled again when space frees up (while voice is queued).
        let _ = s.event().clear();
        if self.ended && !self.queue.is_empty() {
            s.ring().set_producer_flags(0);
            self.ended = false;
        }
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
        if self.complete && self.queue.is_empty() && !self.ended {
            s.ring().set_producer_flags(ring::flags::END);
            self.ended = true;
        }
    }

    /// Stops talking at once (the user interrupted).
    pub fn stop_speaking(&mut self) {
        self.queue.clear();
        self.gathering_since = None;
        self.complete = true;
        if let Some(s) = &self.speaker {
            let _ = self.client.flush(s.id);
            self.recent_end = s.written();
            s.ring().set_producer_flags(ring::flags::END);
            self.ended = true;
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
        // Once the voice is over, the last samples are not playing on.
        if !self.speaking() {
            return 0.0;
        }
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
    /// nothing plays): what the echo gate compares the microphone with when
    /// the audio service does not say what plays.
    pub fn output_db(&self) -> f32 {
        let Some(s) = &self.speaker else { return -100.0 };
        // Once the voice is over, silence (not the last 20 ms on and on).
        if !self.speaking() {
            return -100.0;
        }
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
        if let (Some(s), false) = (&self.speaker, self.queue.is_empty() || self.gathering_since.is_some()) {
            v.push((s.event().raw(), signals::SIGNALED));
        }
        v
    }
}
