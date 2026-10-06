//! The startup sound (`/system/sounds/startup.wav`), played once as the
//! desktop first appears, while the window system's boot splash dissolves
//! into it. Only when the shell starts with the system (init passes
//! `startup`): a shell restarted after a crash stays quiet.
//!
//! It plays as soon as a sound device is attached to the audio service.
//! Drivers can come up after the desktop, so it waits a few seconds for
//! one, then gives up: a sound well after the desktop appeared would come
//! out of nowhere. The whole sound fits in the stream's ring, written at
//! once; the stream is closed once it has played.

use alloc::vec;

use vaudio::wav::{self, WavInfo};
use vproto::audio::{OutputStream, StreamSpec, audio};
use vproto::vfs;
use vrt::println;

const PATH: &str = "/system/sounds/startup.wav";
/// How often to look for a sound device, and for how long.
const LOOK_NS: u64 = 100_000_000;
const GIVE_UP_NS: u64 = 4_000_000_000;
/// The longest sound a ring holds (frames).
const MAX_FRAMES: usize = 262_144;

enum State {
    /// Waiting for a sound device since `since`; the next look at `next`.
    Waiting {
        since: u64,
        next: u64,
    },
    /// Playing until `until` (the stream closes then).
    Playing {
        _stream: OutputStream,
        until: u64,
    },
    Done,
}

pub struct StartupSound {
    state: State,
}

impl StartupSound {
    pub fn new(now: u64) -> StartupSound {
        StartupSound { state: State::Waiting { since: now, next: now } }
    }

    /// When it next wants a look (never, once it has played).
    pub fn deadline(&self) -> u64 {
        match &self.state {
            State::Waiting { next, .. } => *next,
            State::Playing { until, .. } => *until,
            State::Done => vabi::DEADLINE_INFINITE,
        }
    }

    pub fn done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// Plays the sound once there is a sound device, and closes its stream
    /// once it has played.
    pub fn poll(&mut self, audio: Option<&audio::Client>, vfs: Option<&vfs::Client>, now: u64) {
        match self.state {
            State::Waiting { since, next } if now >= next => {
                let status = audio.and_then(|a| a.status().ok());
                self.state = match (audio, status) {
                    (Some(a), Some(s)) if s.device != "none" => match play(a, vfs, now) {
                        Ok((stream, until)) => State::Playing { _stream: stream, until },
                        Err(why) => {
                            println!("startup sound: {}", why);
                            State::Done
                        }
                    },
                    _ if now - since >= GIVE_UP_NS => {
                        println!("startup sound: no sound device; not played");
                        State::Done
                    }
                    _ => State::Waiting { since, next: now + LOOK_NS },
                };
            }
            State::Playing { until, .. } if now >= until => self.state = State::Done,
            _ => {}
        }
    }
}

/// Opens a stream that holds the whole sound and writes it: the stream,
/// and when it will have played.
fn play(audio: &audio::Client, vfs: Option<&vfs::Client>, now: u64) -> Result<(OutputStream, u64), &'static str> {
    let bytes = vfs.and_then(|v| crate::read_file(v, PATH)).ok_or("cannot read /system/sounds/startup.wav")?;
    let info = WavInfo::parse(&bytes).map_err(|_| "/system/sounds/startup.wav is not a WAV file")?;
    let frames = (info.frames as usize).min(MAX_FRAMES);
    let mut pcm = vec![0i16; frames * 2];
    let frames = wav::read_stereo(&info, &bytes, 0, &mut pcm);
    let spec = StreamSpec {
        rate: info.rate,
        channels: 2,
        buffer_frames: frames as u32,
        notify_frames: 0,
        name: "Startup sound".into(),
        paused: false,
        volume: 1.0,
    };
    let stream = OutputStream::open(audio, spec).map_err(|_| "the audio service refused its stream")?;
    let written = stream.write(&pcm[..frames * 2]);
    let ms = written as u64 * 1000 / info.rate.max(1) as u64;
    println!("startup sound: playing ({} ms)", ms);
    // Its length, the device's latency, and a margin.
    Ok((stream, now + (ms + 500) * 1_000_000))
}
