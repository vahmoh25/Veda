//! The speaker amplifiers some laptops have beside the codec (CS35L41s on
//! an SPI bus, served by their own driver): they play only once told the
//! stream to the speakers runs, should power down before it stops, and are
//! muted while headphones play. Without them (most machines) nothing
//! happens here.
//!
//! The playback thread never waits on their driver, or on the registry: a
//! thread of their own talks to it, and is only told what is wanted. (A
//! connection to a service that is not registered waits in the registry
//! until one is, so the driver is looked for in the registry's list
//! first.) Before the stream stops, the playback thread gives that thread a
//! moment to power the amplifiers down while the codec still clocks them.

use alloc::sync::Arc;

use vproto::speakers::speakers;
use vrt::println;
use vrt::sync::{Condvar, Mutex};
use vrt::time::now_ns;

/// How often the amplifiers' driver is looked for while the stream runs
/// and it is not there.
const LOOK_NS: u64 = 5_000_000_000;
/// How long a reply may take (powering down waits for the amplifiers).
const TIMEOUT_NS: u64 = 1_000_000_000;
/// How long the playback thread waits for the amplifiers to power down
/// before it stops the stream.
const STOP_WAIT_NS: u64 = 250_000_000;

#[derive(Default)]
struct State {
    /// What the playback thread wants.
    on: bool,
    muted: bool,
    /// What the amplifiers were last told about the stream.
    told_on: bool,
    /// Their driver is connected.
    connected: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

pub struct Speakers {
    shared: Arc<Shared>,
}

impl Speakers {
    /// Starts the thread that talks to the amplifiers' driver.
    pub fn start() -> Speakers {
        let shared = Arc::new(Shared::default());
        let theirs = shared.clone();
        if let Err(e) = vrt::thread::Builder::new().name("speakers").spawn(move || run(&theirs)) {
            println!("cannot start the speaker amplifiers' thread: {}", e);
        }
        Speakers { shared }
    }

    /// The stream to the speakers has just started (`true`), or is about to
    /// stop: then this waits a moment for the amplifiers to power down.
    pub fn playing(&self, on: bool) {
        let mut s = self.shared.state.lock();
        if s.on == on {
            return;
        }
        s.on = on;
        self.shared.changed.notify_all();
        if on || !s.connected {
            return;
        }
        let deadline = now_ns() + STOP_WAIT_NS;
        while s.told_on && s.connected {
            let (guard, notified) = self.shared.changed.wait_until(s, deadline);
            s = guard;
            if !notified {
                break;
            }
        }
    }

    /// Mutes the amplifiers (headphones in) or unmutes them.
    pub fn mute(&self, muted: bool) {
        let mut s = self.shared.state.lock();
        if s.muted != muted {
            s.muted = muted;
            self.shared.changed.notify_all();
        }
    }
}

/// Connects to the amplifiers' driver if it is registered.
fn connect() -> Option<speakers::Client> {
    let registered = vproto::with_registry(|r| r.list()).ok()?.ok()?;
    if !registered.iter().any(|n| n == speakers::NAME) {
        return None;
    }
    let c = speakers::Client::new(vproto::connect(speakers::NAME).ok()?);
    c.set_timeout(TIMEOUT_NS);
    Some(c)
}

fn run(shared: &Shared) {
    let mut client: Option<speakers::Client> = None;
    // What the amplifiers were told: whether the stream runs, and muted.
    let (mut told_on, mut told_muted) = (false, false);
    let mut next_look = 0u64;
    let mut looked = false;
    loop {
        let (on, muted) = {
            let mut s = shared.state.lock();
            loop {
                let look = client.is_none() && s.on && now_ns() >= next_look;
                if look || (client.is_some() && (s.on, s.muted) != (told_on, told_muted)) {
                    break;
                }
                // Nothing to tell, and nobody to tell it to: the playback
                // thread need not wait.
                if client.is_none() && s.told_on != s.on {
                    s.told_on = s.on;
                    shared.changed.notify_all();
                }
                let deadline = if client.is_none() && s.on { next_look } else { vabi::DEADLINE_INFINITE };
                s = shared.changed.wait_until(s, deadline).0;
            }
            (s.on, s.muted)
        };
        if client.is_none() {
            next_look = now_ns() + LOOK_NS;
            client = connect();
            match (&client, looked) {
                (Some(_), _) => {
                    println!("speaker amplifiers found");
                    (told_on, told_muted) = (false, false);
                }
                (None, false) => println!("no speaker amplifiers' driver is running"),
                (None, true) => {}
            }
            looked = true;
        }
        if let Some(c) = &client {
            let mut answered = true;
            if muted != told_muted {
                answered &= c.mute(muted).is_ok();
            }
            if answered && on != told_on {
                // A refusal is the amplifiers' driver's to log.
                answered &= c.playing(on).is_ok();
            }
            if answered {
                (told_on, told_muted) = (on, muted);
            } else {
                println!("the speaker amplifiers' driver does not answer");
                client = None;
                told_on = false;
            }
        }
        let mut s = shared.state.lock();
        s.told_on = if client.is_some() { told_on } else { s.on };
        s.connected = client.is_some();
        shared.changed.notify_all();
    }
}
