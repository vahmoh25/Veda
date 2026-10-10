//! airsim — the simulated Wi-Fi environment for Veda under QEMU.
//!
//! QEMU cannot emulate a Wi-Fi adapter, so Veda's virtual radio (a
//! virtio-serial port, which `airlink` in the driver VM makes a radio of
//! Linux's) exchanges raw 802.11 frames with the host over that port.
//! `cargo xtask run --net wifi` starts QEMU with that port on a Unix socket
//! in the run's directory and starts airsim, which
//!
//! * connects to the port and speaks the radio link protocol
//!   ([`vradiolink`]) with the guest's `airlink`;
//! * runs the simulated access points (see [`world`]);
//! * bridges the access points to a QEMU user-mode network (NAT) through a
//!   pair of Unix datagram sockets (`-netdev dgram` on a hub with `-netdev
//!   user`), so the guest reaches the Internet over Wi-Fi;
//! * accepts control commands (see [`control`]) on a Unix socket, which
//!   tests use to change signal levels, switch access points off and so
//!   on.
//!
//! Its sockets are files of the run's (none is a port another run on the
//! machine could take); it has nothing to do with the host's own Wi-Fi.
//!
//! ```text
//! airsim --radio <socket> --wired-local <socket> --wired-remote <socket>
//!        [--control <socket>] [--password <password>] [--seed <n>] [--exit-with-stdin]
//! ```

mod control;
mod packet;
#[cfg(test)]
mod tests;
mod world;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vradiolink::{LinkError, Reader, msg};
use world::{Output, SimRandom, World, default_networks};

/// The password of the default secured networks.
const DEFAULT_PASSWORD: &str = "veda-wifi";
/// Messages waiting for the guest before newer ones are dropped (the guest
/// may not be reading, for example while it boots).
const GUEST_QUEUE: usize = 1024;

struct Options {
    radio: PathBuf,
    wired_local: PathBuf,
    wired_remote: PathBuf,
    control: Option<PathBuf>,
    password: String,
    seed: Option<u64>,
    exit_with_stdin: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: airsim --radio <socket> --wired-local <socket> --wired-remote <socket> [--control <socket>] \
         [--password <password>] [--seed <n>] [--exit-with-stdin]"
    );
    std::process::exit(2)
}

fn parse_options() -> Options {
    let mut args = std::env::args().skip(1);
    let (mut radio, mut wired_local, mut wired_remote, mut control) = (None, None, None, None);
    let (mut password, mut seed, mut exit_with_stdin) = (DEFAULT_PASSWORD.to_string(), None, false);
    let addr = |v: Option<String>| -> PathBuf { v.map(PathBuf::from).unwrap_or_else(|| usage()) };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--radio" => radio = Some(addr(args.next())),
            "--wired-local" => wired_local = Some(addr(args.next())),
            "--wired-remote" => wired_remote = Some(addr(args.next())),
            "--control" => control = Some(addr(args.next())),
            "--password" => password = args.next().unwrap_or_else(|| usage()),
            "--seed" => seed = Some(args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage())),
            "--exit-with-stdin" => exit_with_stdin = true,
            _ => usage(),
        }
    }
    if !(8..=63).contains(&password.len()) {
        eprintln!("airsim: the password must be 8 to 63 characters");
        std::process::exit(2);
    }
    match (radio, wired_local, wired_remote) {
        (Some(radio), Some(wired_local), Some(wired_remote)) => {
            Options { radio, wired_local, wired_remote, control, password, seed, exit_with_stdin }
        }
        _ => usage(),
    }
}

enum Event {
    /// Connected to the guest's port; the stream is for writing.
    RadioUp(UnixStream),
    RadioData(Vec<u8>),
    RadioDown,
    Wired(Vec<u8>),
    Control(String, Sender<String>),
    Quit,
}

fn log(start: Instant, line: &str) {
    let t = start.elapsed();
    println!("[{:>5}.{:03}] {line}", t.as_secs(), t.subsec_millis());
}

/// Until when (if at all) the radio link must stay down (`radio drop`).
type Hold = Arc<Mutex<Option<Instant>>>;

/// Connects to the guest's port (retrying while QEMU starts, and again
/// after the connection drops) and forwards what arrives.
fn radio_thread(addr: PathBuf, events: Sender<Event>, hold: Hold) {
    loop {
        let until = *hold.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = until
            && Instant::now() < t
        {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match UnixStream::connect(&addr) {
            Ok(stream) => {
                let Ok(writer) = stream.try_clone() else { continue };
                if events.send(Event::RadioUp(writer)).is_err() {
                    return;
                }
                // `radio drop` shuts the socket down, which ends the read.
                let mut stream = stream;
                let mut buf = vec![0u8; 16384];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if events.send(Event::RadioData(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
                drop(stream);
                if events.send(Event::RadioDown).is_err() {
                    return;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Writes queued messages to the guest's port until the connection closes.
fn writer_thread(mut stream: UnixStream, queue: Receiver<Vec<u8>>) {
    while let Ok(bytes) = queue.recv() {
        if stream.write_all(&bytes).is_err() {
            return;
        }
    }
}

/// Receives frames from QEMU's network.
fn wired_thread(socket: UnixDatagram, remote: PathBuf, events: Sender<Event>) {
    let mut buf = vec![0u8; 65536];
    loop {
        match socket.recv_from(&mut buf) {
            // Only QEMU's socket may inject frames.
            Ok((n, from)) if from.as_pathname() == Some(remote.as_path()) => {
                if events.send(Event::Wired(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Serves control connections.
fn control_thread(listener: UnixListener, events: Sender<Event>) {
    for conn in listener.incoming().flatten() {
        let events = events.clone();
        std::thread::spawn(move || {
            let Ok(mut out) = conn.try_clone() else { return };
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { return };
                let (tx, rx) = mpsc::channel();
                if events.send(Event::Control(line, tx)).is_err() {
                    return;
                }
                let Ok(answer) = rx.recv() else { return };
                if out.write_all(answer.as_bytes()).is_err() {
                    return;
                }
            }
        });
    }
}

struct Guest {
    queue: SyncSender<Vec<u8>>,
    stream: UnixStream,
    reader: Reader,
}

/// A socket of ours at `path`, an old one of a run before removed first.
fn bind<T>(path: &Path, bind: impl Fn(&Path) -> std::io::Result<T>) -> T {
    let _ = std::fs::remove_file(path);
    bind(path).unwrap_or_else(|e| {
        eprintln!("airsim: cannot bind {}: {e}", path.display());
        std::process::exit(1)
    })
}

fn main() {
    let opts = parse_options();
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let mut rng = match opts.seed {
        Some(s) => SimRandom::from_seed(s),
        None => SimRandom::from_os(),
    };
    let networks = default_networks(now(), &mut rng, &opts.password);
    let mut world = World::new(networks, rng);

    let wired = bind(&opts.wired_local, |p| UnixDatagram::bind(p));
    let (events, inbox) = mpsc::channel::<Event>();
    {
        let (socket, events) = (wired.try_clone().expect("datagram socket clone"), events.clone());
        let remote = opts.wired_remote.clone();
        std::thread::spawn(move || wired_thread(socket, remote, events));
    }
    let hold: Hold = Arc::new(Mutex::new(None));
    {
        let (events, hold) = (events.clone(), hold.clone());
        let radio = opts.radio.clone();
        std::thread::spawn(move || radio_thread(radio, events, hold));
    }
    if let Some(path) = &opts.control {
        let listener = bind(path, |p| UnixListener::bind(p));
        let events = events.clone();
        std::thread::spawn(move || control_thread(listener, events));
    }
    if opts.exit_with_stdin {
        // The parent (xtask) holds our stdin open; when it exits, so do we.
        let events = events.clone();
        std::thread::spawn(move || {
            let mut sink = [0u8; 64];
            while matches!(std::io::stdin().read(&mut sink), Ok(n) if n > 0) {}
            let _ = events.send(Event::Quit);
        });
    }
    drop(events);

    log(
        start,
        &format!(
            "airsim: radio {}, wired {} <-> {}",
            opts.radio.display(),
            opts.wired_local.display(),
            opts.wired_remote.display()
        ),
    );
    for line in world.describe_networks().lines() {
        log(start, line);
    }

    let mut guest: Option<Guest> = None;
    loop {
        let wait = world.next_deadline().saturating_sub(now()).clamp(1, 1000);
        match inbox.recv_timeout(Duration::from_millis(wait)) {
            Ok(Event::RadioUp(stream)) => {
                let (queue, rx) = mpsc::sync_channel(GUEST_QUEUE);
                let writer = stream.try_clone().expect("TCP stream clone");
                std::thread::spawn(move || writer_thread(writer, rx));
                guest = Some(Guest { queue, stream, reader: Reader::new() });
                world.guest_connected();
            }
            Ok(Event::RadioData(data)) => {
                if let Some(g) = guest.as_mut() {
                    g.reader.push(&data);
                    loop {
                        match g.reader.next_message() {
                            Ok(Some(m)) => world.guest_message(m, now()),
                            Ok(None) => break,
                            Err(LinkError::BadLength | LinkError::BadMessage) => {
                                log(start, "radio link out of step; waiting for the guest's next hello");
                                g.reader.resync(msg::HELLO);
                            }
                        }
                    }
                }
            }
            Ok(Event::RadioDown) => {
                if let Some(g) = guest.take() {
                    let _ = g.stream.shutdown(Shutdown::Both);
                }
                world.guest_disconnected(now());
            }
            Ok(Event::Wired(frame)) => world.wired_frame(frame, now()),
            Ok(Event::Control(line, answer)) => {
                let text = match control::run(&mut world, &line, now()) {
                    Ok(text) => format!("{text}ok\n"),
                    Err(e) => format!("error: {e}\n"),
                };
                if !line.trim().is_empty() && !matches!(line.trim(), "list" | "status" | "help") {
                    log(start, &format!("control: {} -> {}", line.trim(), text.lines().last().unwrap_or("")));
                }
                let _ = answer.send(text);
                if let Some(secs) = world.radio_drop.take() {
                    log(start, &format!("cutting the radio link for {secs} s"));
                    *hold.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + Duration::from_secs(secs));
                    if let Some(g) = guest.take() {
                        let _ = g.stream.shutdown(Shutdown::Both);
                        world.guest_disconnected(now());
                    }
                }
            }
            Ok(Event::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        world.tick(now());
        for out in world.take_output() {
            match out {
                Output::Guest(m) => {
                    if let Some(g) = guest.as_ref() {
                        match g.queue.try_send(m.encode()) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => {}
                        }
                    }
                }
                Output::Wired(frame) => {
                    let _ = wired.send_to(&frame, &opts.wired_remote);
                }
                Output::Log(line) => log(start, &line),
            }
        }
    }
    log(start, "airsim: exiting");
}
