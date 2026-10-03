//! `audio` — the system audio service.
//!
//! * Registers `audio` (the public protocol, [`vproto::audio::audio`]):
//!   applications open playback streams backed by shared rings, pause,
//!   flush and adjust them, and set the master volume.
//! * Registers `audiodev` ([`vproto::audio::audiodev`]): a sound driver
//!   attaches its output device and receives the ring the mixer fills.
//! * Mixes every playing stream into the device format (see [`mixer`]).
//!   Without a device, audio is consumed silently in real time so that
//!   applications behave the same on machines without sound hardware.
//!
//! The service runs on one high-priority thread: IPC requests are short,
//! and mixing in the same loop avoids any locking.

#![no_std]
#![no_main]

extern crate alloc;

mod mixer;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use vabi::signals;
use vipc::WaitSet;
use vproto::audio::{
    AudioError, AudioStatus, DeviceFormat, DeviceLink, StreamHandle, StreamSpec, StreamStatus, audio, audiodev,
};
use vrt::object::Channel;
use vrt::println;

use mixer::Mixer;

vrt::entry!(main);

/// Wait-set keys.
const KEY_AUDIO_LISTENER: u64 = 1;
const KEY_DEV_LISTENER: u64 = 2;
const KEY_SPACE: u64 = 3;
const FIRST_CONN: u64 = 16;
/// Clients beyond this many are refused (their channel is closed).
const MAX_CLIENTS: usize = 48;

/// What a connection speaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Client,
    Driver,
}

struct Service {
    mixer: Mixer,
    conns: BTreeMap<u64, (Kind, Channel)>,
    /// The driver connection whose device is attached.
    attached: Option<u64>,
    next_key: u64,
}

struct ClientSession<'a> {
    mixer: &'a mut Mixer,
    owner: u64,
}

impl audio::Server for ClientSession<'_> {
    fn open_output(&mut self, spec: StreamSpec) -> Result<StreamHandle, AudioError> {
        let r = self.mixer.open(self.owner, &spec);
        if let Ok(h) = &r {
            println!("stream {} \"{}\" opened ({} Hz, {} ch)", h.id, spec.name, spec.rate, spec.channels);
        }
        r
    }

    fn close(&mut self, stream: u32) -> Result<(), AudioError> {
        self.mixer.close(self.owner, stream)
    }

    fn set_paused(&mut self, stream: u32, paused: bool) -> Result<(), AudioError> {
        self.mixer.set_paused(self.owner, stream, paused)
    }

    fn set_volume(&mut self, stream: u32, volume: f32) -> Result<(), AudioError> {
        self.mixer.set_volume(self.owner, stream, volume)
    }

    fn flush(&mut self, stream: u32) -> Result<u64, AudioError> {
        self.mixer.flush(self.owner, stream)
    }

    fn stream_status(&mut self, stream: u32) -> Result<StreamStatus, AudioError> {
        self.mixer.stream_status(self.owner, stream)
    }

    fn status(&mut self) -> AudioStatus {
        self.mixer.status()
    }

    fn set_master(&mut self, volume: f32, muted: bool) {
        self.mixer.master = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
        self.mixer.muted = muted;
    }
}

struct DriverSession<'a> {
    mixer: &'a mut Mixer,
    attached: bool,
}

impl audiodev::Server for DriverSession<'_> {
    fn attach(&mut self, format: DeviceFormat) -> Result<DeviceLink, AudioError> {
        let link = self.mixer.attach(&format)?;
        println!(
            "audio: output device \"{}\" attached ({} Hz, {} ch, {} frames per period)",
            format.name, format.rate, format.channels, format.period_frames
        );
        self.attached = true;
        Ok(link)
    }
}

impl Service {
    fn accept(&mut self, listener: &Channel, kind: Kind) {
        while let Some(ch) = vproto::accept(listener) {
            let clients = self.conns.values().filter(|(k, _)| *k == Kind::Client).count();
            if kind == Kind::Client && clients >= MAX_CLIENTS {
                println!("too many clients; refusing a connection");
                continue;
            }
            self.conns.insert(self.next_key, (kind, ch));
            self.next_key += 1;
        }
    }

    /// Serves every queued request on a connection.
    fn serve(&mut self, key: u64) {
        loop {
            let Some((kind, ch)) = self.conns.get(&key) else { return };
            let kind = *kind;
            let Ok(msg) = ch.read() else { return };
            let reply = match kind {
                Kind::Client => audio::dispatch(&mut ClientSession { mixer: &mut self.mixer, owner: key }, msg),
                Kind::Driver => {
                    let mut s = DriverSession { mixer: &mut self.mixer, attached: false };
                    let r = audiodev::dispatch(&mut s, msg);
                    if s.attached {
                        self.attached = Some(key);
                    }
                    r
                }
            };
            match reply {
                Ok(reply) => {
                    if let Some((_, ch)) = self.conns.get(&key) {
                        let _ = reply.send(ch);
                    }
                }
                Err(e) => println!("bad request: {}", e),
            }
        }
    }

    fn disconnect(&mut self, key: u64) {
        let Some((kind, _)) = self.conns.remove(&key) else { return };
        match kind {
            Kind::Client => self.mixer.close_owner(key),
            Kind::Driver => {
                if self.attached == Some(key) {
                    self.attached = None;
                    self.mixer.detach();
                    println!("output device detached; continuing without sound");
                }
            }
        }
    }

    fn run(&mut self, audio_listener: Channel, dev_listener: Channel) -> ! {
        loop {
            let now = vrt::time::now_ns();
            let mut ws = WaitSet::new();
            ws.add(audio_listener.raw(), signals::READABLE, KEY_AUDIO_LISTENER);
            ws.add(dev_listener.raw(), signals::READABLE, KEY_DEV_LISTENER);
            if let Some(ev) = self.mixer.space_event() {
                ws.add(ev.raw(), signals::SIGNALED, KEY_SPACE);
            }
            for (&k, (_, ch)) in &self.conns {
                ws.add(ch.raw(), signals::READABLE | signals::PEER_CLOSED, k);
            }
            let ready = ws.wait(self.mixer.deadline(now)).unwrap_or_default();
            let mut closed = Vec::new();
            for (key, observed) in ready {
                match key {
                    KEY_AUDIO_LISTENER => self.accept(&audio_listener, Kind::Client),
                    KEY_DEV_LISTENER => self.accept(&dev_listener, Kind::Driver),
                    KEY_SPACE => {
                        if let Some(ev) = self.mixer.space_event() {
                            let _ = ev.clear();
                        }
                    }
                    k => {
                        if observed & signals::READABLE != 0 {
                            self.serve(k);
                        }
                        if observed & signals::PEER_CLOSED != 0 && observed & signals::READABLE == 0 {
                            closed.push(k);
                        }
                    }
                }
            }
            for k in closed {
                self.disconnect(k);
            }
            self.mixer.run(vrt::time::now_ns());
        }
    }
}

fn main() -> i32 {
    let audio_listener = match vproto::register(audio::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register: {:?}", e);
            return 1;
        }
    };
    let dev_listener = match vproto::register(audiodev::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register the device endpoint: {:?}", e);
            return 1;
        }
    };
    println!("ready (mixing at {} Hz until a device attaches)", mixer::NULL_RATE);
    // Mixing is time-critical: run above normal applications.
    let worker = vrt::thread::Builder::new().name("mixer").priority(vabi::priority::HIGH).spawn(move || -> () {
        let mut svc = Service { mixer: Mixer::new(), conns: BTreeMap::new(), attached: None, next_key: FIRST_CONN };
        svc.run(audio_listener, dev_listener)
    });
    match worker {
        Ok(handle) => {
            let _ = handle.join();
            0
        }
        Err(e) => {
            println!("cannot start the mixer thread: {}", e);
            1
        }
    }
}
