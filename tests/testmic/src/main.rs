//! `testmic` — a microphone for automated tests.
//!
//! `testmic HOST,PORT,RATE` (started with `run=testmic:10.0.2.2,5555,16000`)
//! connects to a TCP server on the host and attaches to the audio service
//! as the input device. The server sends mono 16-bit little-endian PCM at
//! `RATE` in real time — speech that a test wants Vindows to hear, and
//! silence in between — and every frame goes into the device ring as if a
//! microphone had recorded it just now. If the connection drops, the
//! microphone records silence and the program reconnects.
//!
//! Test scripts drive it with `say`, `mic-wav` and `mic-wait` (see
//! `xtask/src/automate.rs`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;

use vnet::{IpAddr, SocketAddr, TcpStream};
use vproto::audio::{DeviceFormat, Ring, Role, audiodev, ring::flags};
use vrt::println;
use vrt::time::Duration;

vrt::entry!(main);

/// Frames per 20 ms period at 16 kHz (the period scales with the rate).
fn period(rate: u32) -> u32 {
    (rate / 50).max(32)
}

fn parse_args() -> Option<(IpAddr, u16, u32)> {
    let args = vrt::env::args();
    let spec: String = args.iter().skip(1).cloned().collect::<alloc::vec::Vec<_>>().join(",");
    let mut parts = spec.split(',').filter(|s| !s.is_empty());
    let host = parts.next()?.parse::<IpAddr>().ok()?;
    let port = parts.next()?.parse::<u16>().ok()?;
    let rate = parts.next().and_then(|r| r.parse::<u32>().ok()).unwrap_or(16_000);
    Some((host, port, rate))
}

fn main() -> i32 {
    let Some((host, port, rate)) = parse_args() else {
        println!("usage: testmic HOST,PORT[,RATE]");
        return 2;
    };
    if !(8_000..=48_000).contains(&rate) {
        println!("unsupported rate {}", rate);
        return 2;
    }
    let ch = match vproto::connect(audiodev::NAME) {
        Ok(ch) => ch,
        Err(e) => {
            println!("cannot reach the audio service: {:?}", e);
            return 1;
        }
    };
    let dev = audiodev::Client::new(ch);
    let format =
        DeviceFormat { name: "testmic".into(), rate, channels: 1, period_frames: period(rate), max_periods: 0 };
    let link = match dev.attach_input(format) {
        Ok(Ok(link)) => link,
        Ok(Err(e)) => {
            println!("the audio service refused the microphone: {}", e);
            return 1;
        }
        Err(e) => {
            println!("cannot attach: {:?}", e);
            return 1;
        }
    };
    let ring = match Ring::map(link.ring, Role::Producer) {
        Ok(r) => r,
        Err(e) => {
            println!("bad ring: {:?}", e);
            return 1;
        }
    };
    println!("attached ({} Hz mono); streaming from {}:{}", rate, host, port);
    let addr = SocketAddr::new(host, port);
    let mut stream: Option<TcpStream> = None;
    let mut buf = vec![0u8; period(rate) as usize * 2 * 4];
    // A byte left over from an odd-sized read.
    let mut carry: Option<u8> = None;
    let mut samples = vec![0i16; buf.len() / 2 + 1];
    let mut connected_once = false;
    loop {
        if stream.is_none() {
            match TcpStream::connect_timeout(addr, Duration::from_secs(3)) {
                Ok(mut s) => {
                    s.set_read_timeout(Some(Duration::from_millis(200)));
                    let _ = s.set_nodelay(true);
                    if !connected_once {
                        println!("connected to the host");
                    }
                    connected_once = true;
                    stream = Some(s);
                    carry = None;
                }
                Err(e) => {
                    if !connected_once {
                        println!("cannot connect to {}: {} (retrying)", addr, e);
                    }
                    vrt::time::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        let Some(s) = stream.as_mut() else { continue };
        let n = match s.read(&mut buf) {
            Ok(0) => {
                println!("the host closed the connection; reconnecting");
                stream = None;
                continue;
            }
            Ok(n) => n,
            Err(vnet::NetError::TimedOut) => continue,
            Err(e) => {
                println!("connection lost ({}); reconnecting", e);
                stream = None;
                continue;
            }
        };
        // Bytes to little-endian samples, keeping an odd byte for later.
        let mut count = 0;
        let mut bytes = &buf[..n];
        if let Some(lo) = carry.take()
            && let Some((&hi, rest)) = bytes.split_first()
        {
            samples[count] = i16::from_le_bytes([lo, hi]);
            count += 1;
            bytes = rest;
        }
        for pair in bytes.chunks_exact(2) {
            samples[count] = i16::from_le_bytes([pair[0], pair[1]]);
            count += 1;
        }
        if bytes.len() % 2 == 1 {
            carry = bytes.last().copied();
        }
        if ring.consumer_flags() & flags::CAPTURE == 0 {
            // Nobody is listening: the audio is dropped, like a microphone
            // whose recording has not been started.
            let _ = link.wake_event.clear();
            continue;
        }
        let written = ring.write(&samples[..count]);
        if written < count {
            ring.set_overruns(ring.overruns().saturating_add((count - written) as u32));
        }
        ring.set_capture_clock(ring.write_pos(), vrt::time::now_ns());
        let _ = link.data_event.signal();
    }
}
