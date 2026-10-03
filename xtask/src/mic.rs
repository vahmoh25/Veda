//! The test microphone: audio that automation scripts make Vindows hear.
//!
//! [`MicServer`] listens on a local TCP port; inside Vindows, `testmic`
//! (started with the boot argument from [`MicServer::boot_arg`]) connects
//! to it through QEMU's NAT (the host is 10.0.2.2) and attaches to the audio
//! service as the microphone. The server streams 16 kHz mono 16-bit PCM in
//! real time: queued audio when there is some, silence otherwise.
//!
//! Scripts queue speech with `say "..."`, synthesised by Deepgram's
//! text-to-speech REST API on the host ([`synthesize`], with the key from
//! `$DEEPGRAM_API_KEY`). Results are cached under
//! `target/vindows/tts-cache`, so a phrase is only paid for once.

use std::collections::VecDeque;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::util::{self, Result};

/// Sample rate of the test microphone.
pub const RATE: u32 = 16_000;
/// Samples sent per packet (20 ms).
const CHUNK: usize = (RATE / 50) as usize;
/// The voice that plays the user in tests (different from the agent's).
pub const USER_VOICE: &str = "aura-2-orion-en";

struct Shared {
    queue: Mutex<VecDeque<i16>>,
    connected: AtomicBool,
    stop: AtomicBool,
}

/// The host side of the test microphone.
pub struct MicServer {
    port: u16,
    shared: Arc<Shared>,
}

impl MicServer {
    /// Listens on a free local port and starts streaming.
    pub fn start() -> Result<MicServer> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("test microphone: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("testmic".into())
            .spawn(move || serve(listener, s))
            .map_err(|e| e.to_string())?;
        Ok(MicServer { port, shared })
    }

    /// The kernel command line argument that starts `testmic` in Vindows.
    pub fn boot_arg(&self) -> String {
        format!("run=testmic:10.0.2.2,{},{}", self.port, RATE)
    }

    /// Queues samples to be "heard".
    pub fn enqueue(&self, samples: &[i16]) {
        self.shared.queue.lock().unwrap().extend(samples.iter().copied());
    }

    /// Queues `secs` of silence.
    /// Something like a voice (a buzzing tone that rises and falls four
    /// times a second, like syllables): enough for a voice detector, though
    /// no recogniser would make words of it.
    pub fn tone(&self, secs: f64) {
        let n = (secs * RATE as f64) as usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                let syllables = 0.55 + 0.45 * (t * 4.0 * std::f64::consts::TAU).sin();
                let buzz = (t * 150.0 * std::f64::consts::TAU).sin() + 0.5 * (t * 300.0 * std::f64::consts::TAU).sin();
                (buzz * syllables * 7000.0) as i16
            })
            .collect();
        self.enqueue(&samples);
    }

    pub fn silence(&self, secs: f64) {
        let n = (secs * RATE as f64) as usize;
        self.shared.queue.lock().unwrap().extend(std::iter::repeat_n(0i16, n));
    }

    /// Samples still to be sent.
    pub fn queued(&self) -> usize {
        self.shared.queue.lock().unwrap().len()
    }

    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst)
    }

    /// Waits until everything queued has been sent.
    pub fn wait_drained(&self, timeout: Duration) -> Result {
        let start = Instant::now();
        while self.queued() > 0 {
            if start.elapsed() > timeout {
                return Err(if self.connected() {
                    "the test microphone did not finish playing in time".into()
                } else {
                    "testmic never connected (is it in the image and on the command line?)".into()
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

impl Drop for MicServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Accepts the guest's connection (again after it drops) and streams.
fn serve(listener: TcpListener, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::SeqCst) {
        let Ok((stream, _)) = listener.accept() else { continue };
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        shared.connected.store(true, Ordering::SeqCst);
        stream_to(stream, &shared);
        shared.connected.store(false, Ordering::SeqCst);
    }
}

/// Sends a 20 ms packet every 20 ms until the connection fails.
fn stream_to(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_nodelay(true);
    let period = Duration::from_millis(20);
    let mut next = Instant::now();
    let mut packet = Vec::with_capacity(CHUNK * 2);
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        packet.clear();
        {
            let mut q = shared.queue.lock().unwrap();
            for _ in 0..CHUNK {
                let s = q.pop_front().unwrap_or(0);
                packet.extend_from_slice(&s.to_le_bytes());
            }
        }
        if stream.write_all(&packet).is_err() {
            return;
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(200) {
            // Fell far behind (the host was busy): do not burst.
            next = now;
        }
    }
}

/// Speech for `text` as 16 kHz mono samples, from the cache or from
/// Deepgram's text-to-speech API (needs `$DEEPGRAM_API_KEY`).
pub fn synthesize(text: &str, voice: &str) -> Result<Vec<i16>> {
    let dir = util::out_dir().join("tts-cache");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("{:016x}.pcm", fnv64(&format!("{voice}\0{RATE}\0{text}"))));
    if let Ok(bytes) = std::fs::read(&file)
        && !bytes.is_empty()
    {
        return Ok(pcm_from_bytes(&bytes));
    }
    let key = std::env::var("DEEPGRAM_API_KEY")
        .map_err(|_| "`say` needs a Deepgram API key in $DEEPGRAM_API_KEY (or a cached phrase)".to_string())?;
    let bytes = deepgram_speak(text, voice, &key, &dir)?;
    std::fs::write(&file, &bytes).map_err(|e| e.to_string())?;
    Ok(pcm_from_bytes(&bytes))
}

fn pcm_from_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// Calls `POST /v1/speak` with curl (the key goes in a header file, not on
/// the command line).
fn deepgram_speak(text: &str, voice: &str, key: &str, dir: &Path) -> Result<Vec<u8>> {
    let nonce = fnv64(&format!("{text}{:?}", Instant::now()));
    let header = dir.join(format!("auth-{nonce:016x}.txt"));
    let body = dir.join(format!("body-{nonce:016x}.json"));
    let out = dir.join(format!("out-{nonce:016x}.pcm"));
    let cleanup = |paths: &[&PathBuf]| {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    };
    std::fs::write(&header, format!("Authorization: Token {key}\n")).map_err(|e| e.to_string())?;
    std::fs::write(&body, format!("{{\"text\":{}}}", json_string(text))).map_err(|e| e.to_string())?;
    let url =
        format!("https://api.deepgram.com/v1/speak?model={voice}&encoding=linear16&sample_rate={RATE}&container=none");
    let result = Command::new("curl")
        .args(["-sS", "--fail-with-body", "-X", "POST", &url, "-H"])
        .arg(format!("@{}", header.display()))
        .args(["-H", "Content-Type: application/json", "--data-binary"])
        .arg(format!("@{}", body.display()))
        .arg("-o")
        .arg(&out)
        .output();
    let _ = std::fs::remove_file(&header);
    let output = match result {
        Ok(o) => o,
        Err(e) => {
            cleanup(&[&body, &out]);
            return Err(format!("running curl: {e}"));
        }
    };
    let bytes = std::fs::read(&out).unwrap_or_default();
    cleanup(&[&body, &out]);
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let detail = String::from_utf8_lossy(&bytes);
        return Err(format!(
            "Deepgram text-to-speech failed: {} {}",
            err.trim(),
            detail.chars().take(200).collect::<String>()
        ));
    }
    if bytes.len() < 2 {
        return Err("Deepgram text-to-speech returned no audio".into());
    }
    Ok(bytes)
}

/// Reads a 16-bit PCM WAV file as 16 kHz mono samples (other rates are
/// converted by linear interpolation, which is good enough for tests).
pub fn read_wav(path: &Path) -> Result<Vec<i16>> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bad = || format!("{}: not a 16-bit PCM WAV file", path.display());
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err(bad());
    }
    let (mut channels, mut rate, mut bits, mut samples) = (0u16, 0u32, 0u16, None);
    let mut i = 12;
    while i + 8 <= data.len() {
        let id = &data[i..i + 4];
        let len = u32::from_le_bytes(data[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &data[i + 8..(i + 8 + len).min(data.len())];
        if id == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            samples = Some(pcm_from_bytes(body));
        }
        i += 8 + len + (len & 1);
    }
    let samples = samples.ok_or_else(bad)?;
    if bits != 16 || channels == 0 || rate == 0 {
        return Err(bad());
    }
    let mono: Vec<i16> = samples
        .chunks_exact(channels as usize)
        .map(|f| (f.iter().map(|&s| s as i32).sum::<i32>() / channels as i32) as i16)
        .collect();
    if rate == RATE {
        return Ok(mono);
    }
    let n = (mono.len() as u64 * RATE as u64 / rate as u64) as usize;
    Ok((0..n)
        .map(|k| {
            let pos = k as f64 * rate as f64 / RATE as f64;
            let i = pos as usize;
            let frac = pos - i as f64;
            let a = mono.get(i).copied().unwrap_or(0) as f64;
            let b = mono.get(i + 1).copied().unwrap_or(0) as f64;
            (a + (b - a) * frac) as i16
        })
        .collect())
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn fnv64(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_queued_audio_in_real_time() {
        let mic = MicServer::start().unwrap();
        let port: u16 = mic.boot_arg().split(',').nth(1).unwrap().parse().unwrap();
        let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
        mic.enqueue(&[1000; 1600]);
        let start = Instant::now();
        let mut got = vec![0u8; 6400];
        std::io::Read::read_exact(&mut conn, &mut got).unwrap();
        // 200 ms of audio takes about 200 ms to arrive.
        assert!(start.elapsed() >= Duration::from_millis(150), "{:?}", start.elapsed());
        let samples = pcm_from_bytes(&got);
        assert!(samples.iter().filter(|&&s| s == 1000).count() >= 1500);
        mic.wait_drained(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("say \"hi\"\n\\"), "\"say \\\"hi\\\"\\n\\\\\"");
    }
}
