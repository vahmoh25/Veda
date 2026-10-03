//! A stand-in for the Deepgram Voice Agent API, for deterministic tests.
//!
//! [`AgentSim`] listens on a local port; the agent in Vindows connects to
//! it (QEMU's NAT makes the host 10.0.2.2) when booted with the argument
//! from [`AgentSim::boot_arg`]. It speaks the same WebSocket protocol as
//! Deepgram: it answers the `Settings` message with `Welcome` and
//! `SettingsApplied`, and then does what the script says — send a function
//! call and collect its result, play voice, report that the user started
//! speaking — while recording everything the agent sends.
//!
//! It also stands in for Deepgram's streaming speech recognition, which
//! the agent uses to hear its name while asleep: the script decides what
//! was "heard" (`agent-hear TEXT`) once the agent streams audio to it.
//!
//! Script commands (see `automate.rs`): `agent-connected`, `agent-call NAME
//! ARGS-JSON`, `agent-result TEXT`, `agent-expect TEXT`, `agent-speak
//! SECONDS`, `agent-interrupt`, `agent-send JSON`, `agent-audio BYTES`,
//! `agent-hear TEXT`, and `agent-asleep` (the agent is not woken at start).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

use crate::util::Result;

/// What the simulator has received.
#[derive(Default)]
struct Log {
    /// Text messages from the agent, in order.
    texts: Vec<String>,
    /// Bytes of microphone audio received.
    audio_bytes: usize,
    connections: u32,
    /// Speech recognition: connections, audio received, text messages.
    listen_connections: u32,
    listen_audio_bytes: usize,
    listen_texts: Vec<String>,
}

struct Shared {
    log: Mutex<Log>,
    writer: Mutex<Option<TcpStream>>,
    /// The open speech recognition connection.
    listen_writer: Mutex<Option<TcpStream>>,
}

/// The simulated Voice Agent service.
pub struct AgentSim {
    port: u16,
    shared: Arc<Shared>,
    next_call: u32,
}

impl AgentSim {
    pub fn start() -> Result<AgentSim> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("agent simulator: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let shared = Arc::new(Shared {
            log: Mutex::new(Log::default()),
            writer: Mutex::new(None),
            listen_writer: Mutex::new(None),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("agentsim".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let s2 = s.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = serve(stream, &s2) {
                            eprintln!("agent simulator: {e}");
                        }
                    });
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(AgentSim { port, shared, next_call: 1 })
    }

    /// The kernel command line arguments pointing the agent here (and,
    /// with `wake`, starting a conversation at once).
    pub fn boot_arg(&self, wake: bool) -> String {
        format!(
            "agent.endpoint=ws://10.0.2.2:{0}/v1/agent/converse agent.listen=ws://10.0.2.2:{0}/v1/listen{1}",
            self.port,
            if wake { " agent.wake" } else { "" }
        )
    }

    /// Once the agent streams audio for recognition, says that `text` was
    /// heard (a final transcript at the end of an utterance).
    pub fn hear(&self, text: &str, timeout: Duration) -> Result {
        let start = Instant::now();
        loop {
            let streaming = self.shared.listen_writer.lock().unwrap().is_some()
                && self.shared.log.lock().unwrap().listen_audio_bytes > 0;
            if streaming {
                break;
            }
            if start.elapsed() > timeout {
                return Err("the agent did not stream speech for recognition".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let msg = format!(
            "{{\"type\":\"Results\",\"is_final\":true,\"speech_final\":true,\"channel\":{{\"alternatives\":[{{\"transcript\":{},\"confidence\":0.98}}]}}}}",
            json_string(text)
        );
        let mut w = self.shared.listen_writer.lock().unwrap();
        let s = w.as_mut().ok_or("the recognition stream closed")?;
        s.write_all(&frame(1, msg.as_bytes())).map_err(|e| format!("agent simulator: {e}"))
    }

    /// How many recognition streams the agent opened.
    pub fn listen_connections(&self) -> u32 {
        self.shared.log.lock().unwrap().listen_connections
    }

    fn send(&self, frame: Vec<u8>) -> Result {
        let mut w = self.shared.writer.lock().unwrap();
        let s = w.as_mut().ok_or("the agent is not connected to the simulator")?;
        s.write_all(&frame).map_err(|e| format!("agent simulator: {e}"))
    }

    pub fn send_json(&self, json: &str) -> Result {
        self.send(frame(1, json.as_bytes()))
    }

    /// Waits until the agent is connected (and configured).
    pub fn wait_connected(&self, timeout: Duration) -> Result {
        let start = Instant::now();
        loop {
            if self.shared.writer.lock().unwrap().is_some() && self.texts().iter().any(|t| t.contains("\"Settings\"")) {
                return Ok(());
            }
            if start.elapsed() > timeout {
                return Err("the agent did not connect to the simulator".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn texts(&self) -> Vec<String> {
        self.shared.log.lock().unwrap().texts.clone()
    }

    pub fn audio_bytes(&self) -> usize {
        self.shared.log.lock().unwrap().audio_bytes
    }

    /// Waits until the agent sent a text message containing `needle`
    /// (after the first `skip` messages); returns it.
    pub fn wait_text(&self, needle: &str, skip: usize, timeout: Duration) -> Result<String> {
        let start = Instant::now();
        loop {
            if let Some(t) = self.texts().into_iter().skip(skip).find(|t| t.contains(needle)) {
                return Ok(t);
            }
            if start.elapsed() > timeout {
                return Err(format!("the agent did not send \"{needle}\" in time"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Asks the agent to run a function and returns its result.
    pub fn call(&mut self, name: &str, args_json: &str, timeout: Duration) -> Result<String> {
        let id = format!("call-{}", self.next_call);
        self.next_call += 1;
        let skip = self.texts().len();
        let msg = format!(
            "{{\"type\":\"FunctionCallRequest\",\"functions\":[{{\"id\":\"{id}\",\"name\":\"{name}\",\"arguments\":{},\"client_side\":true}}]}}",
            json_string(args_json)
        );
        self.send_json(&msg)?;
        let response = self.wait_text(&format!("\"id\":\"{id}\""), skip, timeout)?;
        // The result as the language model reads it (the response carries
        // it as a JSON string).
        Ok(string_field(&response, "content").unwrap_or(response))
    }

    /// The agent "speaks" for `secs` seconds (a tone at 24 kHz).
    pub fn speak(&self, secs: f64) -> Result {
        self.send_json(
            r#"{"type":"AgentStartedSpeaking","total_latency":"0.5","tts_latency":"0.1","ttt_latency":"0.4"}"#,
        )?;
        let n = (secs * 24_000.0) as usize;
        let mut pcm = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / 24_000.0;
            let s = ((t * 220.0 * std::f64::consts::TAU).sin() * 6000.0) as i16;
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        for chunk in pcm.chunks(4800) {
            self.send(frame(2, chunk))?;
        }
        self.send_json(r#"{"type":"AgentAudioDone"}"#)
    }
}

/// A server frame (not masked).
fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | opcode];
    if payload.len() < 126 {
        f.push(payload.len() as u8);
    } else if payload.len() <= 0xFFFF {
        f.push(126);
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

/// The value of the string field `name` in a flat JSON message, decoded.
fn string_field(json: &str, name: &str) -> Option<String> {
    let start = json.find(&format!("\"{name}\":\""))? + name.len() + 4;
    let mut out = String::new();
    let mut chars = json[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?).unwrap_or('\u{fffd}'));
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One connection from the agent: the upgrade, then frames.
fn serve(mut stream: TcpStream, shared: &Shared) -> Result {
    let _ = stream.set_nodelay(true);
    // The upgrade request.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).map_err(|e| e.to_string())? == 0 {
            return Err("closed during the handshake".into());
        }
        head.push(byte[0]);
        if head.len() > 16 * 1024 {
            return Err("handshake too long".into());
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let key = text
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ").or_else(|| l.strip_prefix("sec-websocket-key: ")))
        .ok_or("no WebSocket key")?
        .trim()
        .to_string();
    if !text.lines().any(|l| l.to_ascii_lowercase().starts_with("authorization: token ")) {
        let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        return Err("no Authorization header".into());
    }
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64(&h.finalize());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(resp.as_bytes()).map_err(|e| e.to_string())?;
    let path = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
    if path.starts_with("/v1/listen") {
        return serve_listen(stream, shared);
    }
    *shared.writer.lock().unwrap() = Some(stream.try_clone().map_err(|e| e.to_string())?);
    shared.log.lock().unwrap().connections += 1;
    stream.write_all(&frame(1, br#"{"type":"Welcome","request_id":"sim-1"}"#)).map_err(|e| e.to_string())?;
    loop {
        let mut h2 = [0u8; 2];
        if stream.read_exact(&mut h2).is_err() {
            break;
        }
        let opcode = h2[0] & 0x0F;
        let mut len = (h2[1] & 0x7F) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            stream.read_exact(&mut b).map_err(|e| e.to_string())?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            stream.read_exact(&mut b).map_err(|e| e.to_string())?;
            len = u64::from_be_bytes(b);
        }
        if h2[1] & 0x80 == 0 {
            return Err("unmasked frame from the client".into());
        }
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask).map_err(|e| e.to_string())?;
        let mut payload = vec![0u8; len as usize];
        stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
        match opcode {
            1 => {
                let t = String::from_utf8_lossy(&payload).to_string();
                let settings = t.contains("\"type\":\"Settings\"");
                shared.log.lock().unwrap().texts.push(t);
                if settings {
                    stream.write_all(&frame(1, br#"{"type":"SettingsApplied"}"#)).map_err(|e| e.to_string())?;
                }
            }
            2 => shared.log.lock().unwrap().audio_bytes += payload.len(),
            8 => {
                let _ = stream.write_all(&frame(8, &payload));
                break;
            }
            9 => {
                let _ = stream.write_all(&frame(10, &payload));
            }
            _ => {}
        }
    }
    *shared.writer.lock().unwrap() = None;
    Ok(())
}

/// Reads one client frame: (opcode, unmasked payload), or None at the end.
fn read_frame(stream: &mut TcpStream) -> Result<Option<(u8, Vec<u8>)>> {
    let mut h2 = [0u8; 2];
    if stream.read_exact(&mut h2).is_err() {
        return Ok(None);
    }
    let mut len = (h2[1] & 0x7F) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b).map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b).map_err(|e| e.to_string())?;
        len = u64::from_be_bytes(b);
    }
    if h2[1] & 0x80 == 0 {
        return Err("unmasked frame from the client".into());
    }
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask).map_err(|e| e.to_string())?;
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
    Ok(Some((h2[0] & 0x0F, payload)))
}

/// A speech recognition stream: audio in; transcripts come from the
/// script (`agent-hear`). `CloseStream` ends it like Deepgram does.
fn serve_listen(mut stream: TcpStream, shared: &Shared) -> Result {
    *shared.listen_writer.lock().unwrap() = Some(stream.try_clone().map_err(|e| e.to_string())?);
    shared.log.lock().unwrap().listen_connections += 1;
    while let Some((opcode, payload)) = read_frame(&mut stream)? {
        match opcode {
            1 => {
                let t = String::from_utf8_lossy(&payload).to_string();
                let close = t.contains("CloseStream");
                shared.log.lock().unwrap().listen_texts.push(t);
                if close {
                    let _ = stream.write_all(&frame(1, br#"{"type":"Metadata","duration":1.0}"#));
                    let _ = stream.write_all(&frame(8, &1000u16.to_be_bytes()));
                    break;
                }
            }
            2 => shared.log.lock().unwrap().listen_audio_bytes += payload.len(),
            8 => {
                let _ = stream.write_all(&frame(8, &payload));
                break;
            }
            9 => {
                let _ = stream.write_all(&frame(10, &payload));
            }
            _ => {}
        }
    }
    *shared.listen_writer.lock().unwrap() = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_and_base64() {
        let mut h = Sha1::new();
        h.update(b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        assert_eq!(base64(&h.finalize()), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(frame(1, b"hi"), vec![0x81, 2, b'h', b'i']);
    }

    #[test]
    fn decodes_function_results() {
        let msg = r#"{"type":"FunctionCallResponse","id":"call-1","content":"{\"ok\":true,\"text\":\"a\\nb é\"}"}"#;
        assert_eq!(string_field(msg, "content").as_deref(), Some(r#"{"ok":true,"text":"a\nb é"}"#));
        assert_eq!(string_field(msg, "id").as_deref(), Some("call-1"));
        assert_eq!(string_field(msg, "missing"), None);
        assert_eq!(string_field(r#"{"content":"cut"#, "content"), None);
    }
}
