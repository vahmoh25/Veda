//! `bridgetest` — checks the driver VM's bridge from inside the Linux
//! guest: Veda's objects, used by a Linux program through `vrt` exactly as
//! a Veda program uses them.
//!
//! It reads the registry the guest sees, sends a message that carries a
//! VMO and an event over a channel of its own, maps the VMO and writes into
//! it, waits on the event with and without a deadline, has another thread
//! wake it through a channel, and measures how long a signal takes to
//! travel between two threads. It says `bridgetest: PASS` when all went
//! well.

use std::time::Instant;

use vabi::signals;
use vrt::object::{Channel, Event, Vmo};
use vrt::vm::Mapping;

struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool) {
        println!("bridgetest: {} {}", if ok { "ok  " } else { "FAIL" }, what);
        if !ok {
            self.failed += 1;
        }
    }
}

fn main() {
    let mut c = Checks { failed: 0 };

    // The registry: Veda's, narrowed to what drivers attach to. (Linux
    // may be up before Veda's audio service.)
    let list = || vproto::with_registry(|r| r.list()).ok().and_then(|r| r.ok()).unwrap_or_default();
    let mut names = list();
    for _ in 0..100 {
        if names.iter().any(|n| n == "audiodev") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        names = list();
    }
    println!("bridgetest: the registry lists {}", names.join(", "));
    c.check("the registry lists the audio devices' service", names.iter().any(|n| n == "audiodev"));
    c.check("the registry hides the file system", !names.iter().any(|n| n == "vfs"));
    let denied = Channel::create()
        .ok()
        .and_then(|(_, server)| vproto::with_registry(|r| r.connect("vfs".into(), server)).ok())
        .and_then(|r| r.ok());
    c.check("the file system cannot be reached", matches!(denied, Some(Err(_))));

    // A message that carries a VMO and an event.
    let (a, b) = Channel::create().expect("channel");
    let vmo = Vmo::create(64 * 1024).expect("vmo");
    let event = Event::create().expect("event");
    let peer = Event::from_handle(event.0.duplicate(None).expect("duplicate"));
    let sent = a.write(b"hello, Veda", vec![vmo.into_handle(), peer.into_handle()]);
    c.check("a message carries a VMO and an event", sent.is_ok());
    let msg = b.read();
    let ok = matches!(&msg, Ok(m) if m.bytes == b"hello, Veda" && m.handles.len() == 2);
    c.check("the message arrives with both handles", ok);
    let Ok(mut msg) = msg else { return finish(c) };
    let (Some(peer), Some(vmo)) = (msg.handles.pop(), msg.handles.pop()) else { return finish(c) };
    let (vmo, peer) = (Vmo::from_handle(vmo), Event::from_handle(peer));

    // The VMO, mapped: what is written there is Veda's memory.
    match Mapping::new(Vmo::from_handle(vmo.0.duplicate(None).expect("duplicate")), 64 * 1024, 3) {
        Ok(map) => {
            // SAFETY: the mapping is ours, 64 KiB long.
            let mem = unsafe { core::slice::from_raw_parts_mut(map.as_ptr(), 64 * 1024) };
            for (i, b) in mem.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            let mut back = vec![0u8; 64 * 1024];
            let read = vmo.read(0, &mut back).is_ok();
            c.check("a mapped VMO holds what was written into it", read && back[..] == mem[..]);
            let _ = vmo.write(1000, b"Veda");
            c.check("and shows what was written through Veda", &mem[1000..1004] == b"Veda");
        }
        Err(e) => c.check(&format!("a VMO maps ({e})"), false),
    }

    // Events: signaled, seen; not signaled, the deadline passes.
    let _ = peer.signal();
    c.check("a signaled event is seen at once", event.wait(signals::SIGNALED, 0).is_ok());
    let _ = event.clear();
    let started = vrt::time::now_ns();
    let timed_out = event.wait(signals::SIGNALED, started + 50_000_000);
    let waited = (vrt::time::now_ns() - started) / 1_000_000;
    c.check(
        &format!("a wait ends at its deadline ({waited} ms of 50)"),
        timed_out == Err(vabi::Error::TimedOut) && (45..250).contains(&waited),
    );

    // Another thread wakes a wait through a channel.
    let (x, y) = Channel::create().expect("channel");
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = x.write(b"wake", Vec::new());
        x
    });
    let woke = y.read_blocking(vabi::DEADLINE_INFINITE);
    if let Err(e) = &woke {
        println!("bridgetest: reading the message: {e}");
    }
    c.check("a waiting thread wakes when a message comes", matches!(woke, Ok(m) if m.bytes == b"wake"));
    let _ = writer.join();

    // Signals between two threads, there and back.
    let (ping, pong) = (Event::create().expect("event"), Event::create().expect("event"));
    let (ping2, pong2) = (
        Event::from_handle(ping.0.duplicate(None).expect("duplicate")),
        Event::from_handle(pong.0.duplicate(None).expect("duplicate")),
    );
    const ROUNDS: u32 = 200;
    let echo = std::thread::spawn(move || {
        for _ in 0..ROUNDS {
            if ping2.wait(signals::SIGNALED, vabi::DEADLINE_INFINITE).is_err() {
                return;
            }
            let _ = ping2.clear();
            let _ = pong2.signal();
        }
    });
    let started = Instant::now();
    let mut round_trips = 0;
    for _ in 0..ROUNDS {
        let _ = ping.signal();
        if pong.wait(signals::SIGNALED, vabi::DEADLINE_INFINITE).is_err() {
            break;
        }
        let _ = pong.clear();
        round_trips += 1;
    }
    let micros = started.elapsed().as_micros() / ROUNDS.max(1) as u128;
    let _ = echo.join();
    println!("bridgetest: a signal goes there and back in {micros} us");
    c.check("signals go back and forth between two threads", round_trips == ROUNDS);

    // Veda's clock, which the guest reads itself.
    let (t0, t1) = (vrt::time::now_ns(), vrt::time::now_ns());
    c.check("Veda's clock runs", t0 > 0 && t1 >= t0);
    finish(c);
}

fn finish(c: Checks) {
    if c.failed == 0 {
        println!("bridgetest: PASS");
    } else {
        println!("bridgetest: FAIL ({} checks)", c.failed);
    }
}
