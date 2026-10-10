//! The startup sound (`/system/sounds/startup.wav`), played once as the
//! desktop first appears, while the window system's startup splash dissolves
//! into it. Only when the shell starts with the system (init passes
//! `startup`): a shell restarted after a crash stays quiet.
//!
//! The window system waits for it: the splash dissolves into the desktop
//! once the shell says that what comes with it is ready
//! (`display::desktop_ready`), and tells the shell the moment it does
//! (`WindowEvent::Appearing`). So the shell readies the sound first. As
//! soon as a sound card is attached to the audio service it opens a stream
//! that holds the whole sound, paused, and says it is ready; as the
//! desktop appears it starts the stream, which plays at once. If no sound
//! card is coming (the audio service says whether devmgr gave one to its
//! driver), there is nothing to wait for. Should the desktop appear before
//! the card did (the window system waits only a few seconds for it), the
//! sound is not played: a sound well after the desktop appeared would come
//! out of nowhere. The stream is closed once the sound has played.

use alloc::vec;

use vaudio::wav::{self, WavInfo};
use vproto::audio::{OutputStream, StreamSpec, audio};
use vproto::vfs;
use vrt::println;

const PATH: &str = "/system/sounds/startup.wav";
/// How often to look for a sound card.
const LOOK_NS: u64 = 100_000_000;
/// How long a sound that is ready waits for the desktop to appear (if the
/// window system never says it does, it was restarted meanwhile).
const READY_NS: u64 = 60_000_000_000;
/// The longest sound a ring holds (frames).
const MAX_FRAMES: usize = 262_144;

enum State {
    /// Waiting for a sound card; the next look at `next`.
    Waiting {
        next: u64,
    },
    /// Ready: the whole sound in its stream, paused; `ms` long. Given up
    /// at `until`.
    Ready {
        stream: OutputStream,
        ms: u64,
        until: u64,
    },
    /// Playing until `until` (the stream closes then).
    Playing {
        stream: OutputStream,
        until: u64,
    },
    Done,
}

pub struct StartupSound {
    state: State,
}

impl StartupSound {
    pub fn new(now: u64) -> StartupSound {
        StartupSound { state: State::Waiting { next: now } }
    }

    /// When it next wants a look (never, once it has played).
    pub fn deadline(&self) -> u64 {
        match &self.state {
            State::Waiting { next } => *next,
            State::Ready { until, .. } | State::Playing { until, .. } => *until,
            State::Done => vabi::DEADLINE_INFINITE,
        }
    }

    pub fn done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// Looks for a sound card while it waits for one, readying the sound
    /// once there is one, and closes its stream once it has played. True
    /// the moment the sound is ready, or turns out to have nothing to wait
    /// for: the desktop may appear.
    pub fn poll(&mut self, audio: Option<&audio::Client>, vfs: Option<&vfs::Client>, now: u64) -> bool {
        match &self.state {
            State::Waiting { next } if now >= *next => {
                let status = audio.and_then(|a| a.status().ok());
                match (audio, status) {
                    (Some(a), Some(s)) if s.device != "none" => {
                        self.state = match ready(a, vfs) {
                            Ok((stream, ms)) => State::Ready { stream, ms, until: now + READY_NS },
                            Err(why) => {
                                println!("startup sound: {}", why);
                                State::Done
                            }
                        };
                        true
                    }
                    (Some(_), Some(s)) if !s.coming => {
                        println!("startup sound: no sound card; not played");
                        self.state = State::Done;
                        true
                    }
                    _ => {
                        self.state = State::Waiting { next: now + LOOK_NS };
                        false
                    }
                }
            }
            State::Ready { until, .. } | State::Playing { until, .. } if now >= *until => {
                self.close(audio);
                false
            }
            _ => false,
        }
    }

    /// The desktop appears: the sound plays, if it is ready.
    pub fn appearing(&mut self, audio: Option<&audio::Client>, now: u64) {
        match core::mem::replace(&mut self.state, State::Done) {
            State::Ready { stream, ms, .. } => {
                if let Some(a) = audio
                    && let Ok(Ok(())) = a.set_paused(stream.id, false)
                {
                    println!("startup sound: playing ({} ms)", ms);
                    // Its length, the device's latency, and a margin.
                    self.state = State::Playing { stream, until: now + (ms + 500) * 1_000_000 };
                } else {
                    println!("startup sound: the audio service went away");
                }
            }
            State::Waiting { .. } => println!("startup sound: no sound card when the desktop appeared; not played"),
            state => self.state = state,
        }
    }

    /// Closes the stream (if there is one): done.
    fn close(&mut self, audio: Option<&audio::Client>) {
        if let State::Ready { stream, .. } | State::Playing { stream, .. } = &self.state
            && let Some(a) = audio
        {
            let _ = a.close(stream.id);
        }
        self.state = State::Done;
    }
}

/// Opens a stream that holds the whole sound, paused, and writes it: the
/// stream, and how long the sound is (ms).
fn ready(audio: &audio::Client, vfs: Option<&vfs::Client>) -> Result<(OutputStream, u64), &'static str> {
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
        paused: true,
        volume: 1.0,
    };
    let stream = OutputStream::open(audio, spec).map_err(|_| "the audio service refused its stream")?;
    let written = stream.write(&pcm[..frames * 2]);
    Ok((stream, written as u64 * 1000 / info.rate.max(1) as u64))
}
