//! A conversation with the Deepgram Voice Agent.
//!
//! [`Connector`] opens the WebSocket on a helper thread (name lookup, TCP,
//! TLS and the upgrade take a while, and the agent must keep serving its
//! interface meanwhile). [`Session`] then carries the conversation: the
//! `Settings` message goes out first; microphone audio recorded before
//! Deepgram confirms the settings is held and sent right after, so the
//! first words are not lost; messages and voice come back through
//! [`Session::poll`], which never blocks.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vagent::deepgram::{self, ServerMessage};
use vjson::Value;
use vrt::object::Event;
use vrt::sync::Mutex;
use vrt::time::Duration;
use vweb::ws::{Message, WebSocket};
use vweb::{Conn, WebError};

/// Microphone audio kept while the conversation is being set up.
const MAX_PENDING_MIC: usize = 4 * crate::voice::MIC_RATE as usize;
/// Deepgram closes a conversation that hears nothing for about ten
/// seconds; a keep-alive goes out after this long without audio.
const KEEP_ALIVE_NS: u64 = 5_000_000_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Something Deepgram sent.
pub enum Incoming {
    Message(ServerMessage),
    /// Voice: 24 kHz mono samples.
    Audio(Vec<i16>),
    /// The server closed the conversation.
    Closed {
        code: u16,
        reason: String,
    },
}

/// Opens a conversation's WebSocket on a helper thread.
pub struct Connector {
    result: Arc<Mutex<Option<Result<WebSocket<Conn>, WebError>>>>,
    event: Event,
}

impl Connector {
    pub fn start(url: String, key: String) -> Option<Connector> {
        let event = Event::create().ok()?;
        let signal = Event::from_handle(event.0.duplicate(None).ok()?);
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        vrt::thread::Builder::new()
            .name("connect")
            .spawn(move || {
                let auth = alloc::format!("Token {key}");
                let r = WebSocket::connect(&url, &[("Authorization", &auth)], CONNECT_TIMEOUT);
                *slot.lock() = Some(r);
                let _ = signal.signal();
            })
            .ok()?;
        Some(Connector { result, event })
    }

    pub fn handle(&self) -> RawHandle {
        self.event.raw()
    }

    /// The connection, once the helper finished.
    pub fn take(&self) -> Option<Result<WebSocket<Conn>, WebError>> {
        let _ = self.event.clear();
        self.result.lock().take()
    }
}

/// An open conversation.
pub struct Session {
    ws: WebSocket<Conn>,
    /// Deepgram applied the settings: audio may flow.
    pub ready: bool,
    pub started_ns: u64,
    /// The last time someone spoke (for ending quiet conversations).
    pub last_activity_ns: u64,
    /// The agent is in its turn: from `AgentStartedSpeaking` until its
    /// audio is done and played.
    pub agent_turn: bool,
    /// `AgentAudioDone` arrived for the current turn.
    pub audio_done: bool,
    /// The agent asked to end the conversation after its reply.
    pub end_requested: bool,
    /// Function calls the user cancelled by speaking again.
    pub cancelled: BTreeSet<String>,
    pending_mic: VecDeque<i16>,
    pending_inject: Vec<String>,
    last_sent_ns: u64,
    /// A byte of voice left over from an odd-sized message.
    carry: Option<u8>,
}

impl Session {
    pub fn new(ws: WebSocket<Conn>, now: u64) -> Session {
        Session {
            ws,
            ready: false,
            started_ns: now,
            last_activity_ns: now,
            agent_turn: false,
            audio_done: false,
            end_requested: false,
            cancelled: BTreeSet::new(),
            pending_mic: VecDeque::new(),
            pending_inject: Vec::new(),
            last_sent_ns: now,
            carry: None,
        }
    }

    /// Readable when Deepgram sent something.
    pub fn handle(&self) -> RawHandle {
        self.ws.handle()
    }

    pub fn send(&mut self, v: &Value) -> Result<(), WebError> {
        self.last_sent_ns = vrt::time::now_ns();
        self.ws.send_text(&v.to_string())
    }

    /// Sends (or, before the settings apply, holds) microphone audio.
    pub fn send_mic(&mut self, pcm: &[i16]) -> Result<(), WebError> {
        if !self.ready {
            self.pending_mic.extend(pcm.iter().copied());
            let excess = self.pending_mic.len().saturating_sub(MAX_PENDING_MIC);
            self.pending_mic.drain(..excess);
            return Ok(());
        }
        if pcm.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.last_sent_ns = vrt::time::now_ns();
        self.ws.send_binary(&bytes)
    }

    /// Asks the agent to respond to a notice from Vindows (sent once the
    /// conversation is ready).
    pub fn inject(&mut self, text: &str) -> Result<(), WebError> {
        if self.ready {
            self.send(&deepgram::inject_user_message(text))
        } else {
            self.pending_inject.push(text.to_string());
            Ok(())
        }
    }

    /// The settings were applied: send what was held back.
    pub fn on_ready(&mut self) -> Result<(), WebError> {
        self.ready = true;
        let held: Vec<i16> = self.pending_mic.drain(..).collect();
        for chunk in held.chunks(crate::voice::MIC_PACKET * 5) {
            self.send_mic(chunk)?;
        }
        for text in core::mem::take(&mut self.pending_inject) {
            self.send(&deepgram::inject_user_message(&text))?;
        }
        Ok(())
    }

    /// Sends a keep-alive when no audio went out for a while (the
    /// microphone is muted).
    pub fn keep_alive(&mut self, now: u64) -> Result<(), WebError> {
        if self.ready && now.saturating_sub(self.last_sent_ns) > KEEP_ALIVE_NS {
            self.send(&deepgram::keep_alive())?;
        }
        Ok(())
    }

    /// The next thing Deepgram sent, if any (never blocks).
    pub fn poll(&mut self) -> Result<Option<Incoming>, WebError> {
        loop {
            match self.ws.poll()? {
                None => return Ok(None),
                Some(Message::Text(t)) => match deepgram::parse_server_message(&t) {
                    Ok(m) => return Ok(Some(Incoming::Message(m))),
                    Err(e) => vrt::println!(
                        "ignoring a malformed message ({}): {}",
                        e,
                        t.chars().take(120).collect::<String>()
                    ),
                },
                Some(Message::Binary(b)) => {
                    let mut pcm = Vec::with_capacity(b.len() / 2 + 1);
                    let mut bytes = &b[..];
                    if let Some(lo) = self.carry.take()
                        && let Some((&hi, rest)) = bytes.split_first()
                    {
                        pcm.push(i16::from_le_bytes([lo, hi]));
                        bytes = rest;
                    }
                    for p in bytes.chunks_exact(2) {
                        pcm.push(i16::from_le_bytes([p[0], p[1]]));
                    }
                    if bytes.len() % 2 == 1 {
                        self.carry = bytes.last().copied();
                    }
                    return Ok(Some(Incoming::Audio(pcm)));
                }
                Some(Message::Close { code, reason }) => return Ok(Some(Incoming::Closed { code, reason })),
            }
        }
    }

    /// Ends the conversation politely.
    pub fn close(&mut self) {
        let _ = self.ws.close(vweb::ws::close_code::NORMAL, "");
    }
}
