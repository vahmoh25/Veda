//! `speakertest` — speaker amplifiers for automated tests.
//!
//! `speakertest DELAY` (started with `run=speakertest:300`) registers the
//! `speakers` service, as a laptop's amplifier driver does, and logs what
//! the sound driver tells it (`speakertest: playing`, `speakertest:
//! stopped`, `speakertest: muted`, `speakertest: unmuted`). It answers
//! `DELAY` milliseconds late, as slow hardware would: the sound driver's
//! playback must not wait on it (see `tests/ui/hda-speakers.vts`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;

use vabi::signals;
use vipc::WaitSet;
use vproto::speakers::{SpeakerError, speakers};
use vrt::object::Channel;
use vrt::println;
use vrt::time::Duration;

vrt::entry!(main);

struct Amplifiers {
    delay: Duration,
}

impl speakers::Server for Amplifiers {
    fn playing(&mut self, on: bool) -> Result<(), SpeakerError> {
        vrt::time::sleep(self.delay);
        println!("{}", if on { "playing" } else { "stopped" });
        Ok(())
    }

    fn mute(&mut self, muted: bool) -> Result<(), SpeakerError> {
        vrt::time::sleep(self.delay);
        println!("{}", if muted { "muted" } else { "unmuted" });
        Ok(())
    }
}

fn main() -> i32 {
    let args = vrt::env::args();
    let ms = args.get(1).and_then(|a| a.parse::<u64>().ok()).unwrap_or(0);
    let mut amps = Amplifiers { delay: Duration::from_millis(ms) };
    let listener = match vproto::register(speakers::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register the speakers service: {:?}", e);
            return 1;
        }
    };
    println!("answering {} ms late", ms);
    let mut clients: BTreeMap<u64, Channel> = BTreeMap::new();
    let mut next = 1u64;
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE, 0);
        for (&k, c) in &clients {
            ws.add(c.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { continue };
        for (key, observed) in ready {
            if key == 0 {
                while let Some(ch) = vproto::accept(&listener) {
                    clients.insert(next, ch);
                    next += 1;
                }
            } else if observed & signals::READABLE != 0 {
                while let Some(Ok(msg)) = clients.get(&key).map(|c| c.read()) {
                    if let Ok(reply) = speakers::dispatch(&mut amps, msg)
                        && let Some(c) = clients.get(&key)
                    {
                        let _ = reply.send(c);
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                clients.remove(&key);
            }
        }
    }
}
