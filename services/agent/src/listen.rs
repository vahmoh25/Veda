//! Listening for the agent's name while it is asleep.
//!
//! The microphone stays open and a voice detector runs here, on the
//! computer: while nobody speaks, nothing leaves it. When someone does, what
//! they say — with the moment before, so the first word is not cut — is
//! streamed to Deepgram's speech recognition (Nova-3, with the agent's name
//! as a key term), until they have been quiet for a few seconds. A
//! transcript that addresses the agent by name ([`vagent::wake::addressed`])
//! wakes it, and becomes the first thing the user said in the conversation.
//!
//! The detector is [`vaudio::vad::Vad`]. Recognition costs a fraction
//! of a conversation, and a budget caps how
//! long it may run each hour, so a television or music playing all day
//! cannot run up the bill.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vabi::{RawHandle, signals};
use vagent::{deepgram, wake};
use vaudio::vad::Vad;
use vjson::object;
use vweb::ws::{Message, WebSocket};
use vweb::{Conn, WebError};

use crate::session::Connector;
use crate::voice::MIC_RATE;

/// A detector frame: 20 ms.
const FRAME: usize = (MIC_RATE / 50) as usize;
/// Audio kept from before speech starts.
const PREROLL: usize = (MIC_RATE as usize) * 6 / 10;
/// The most audio held while the connection opens.
const HOLD_MAX: usize = (MIC_RATE as usize) * 10;
/// Audio goes out in 100 ms messages.
const PACKET: usize = (MIC_RATE / 10) as usize;
/// Recognition stops after this much quiet...
const QUIET_NS: u64 = 3_000_000_000;
/// ... or this long (someone talking on and on).
const STREAM_MAX_NS: u64 = 60_000_000_000;
/// Recognition allowed per hour.
const BUDGET_NS: u64 = 10 * 60_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
/// After a failed connection, wait this long before trying again.
const RETRY_NS: u64 = 30_000_000_000;

pub struct Listener {
    name: String,
    key: String,
    url: String,
    vad: Vad,
    /// Audio not yet a whole frame.
    partial: Vec<i16>,
    preroll: VecDeque<i16>,
    /// Audio waiting for the connection.
    held: Vec<i16>,
    connector: Option<Connector>,
    stream: Option<WebSocket<Conn>>,
    started_ns: u64,
    last_speech_ns: u64,
    /// What was said in this stretch of speech (final transcripts).
    heard: String,
    retry_at: u64,
    hour_start: u64,
    used_ns: u64,
    over_budget: bool,
}

impl Listener {
    /// Listens for `name`; `url` is Deepgram's (or a simulator's).
    pub fn new(name: &str, key: &str, url: String, now: u64) -> Listener {
        vrt::println!("listening for its name");
        Listener {
            name: name.into(),
            key: key.into(),
            url,
            vad: Vad::new(MIC_RATE),
            partial: Vec::new(),
            preroll: VecDeque::with_capacity(PREROLL),
            held: Vec::new(),
            connector: None,
            stream: None,
            started_ns: 0,
            last_speech_ns: 0,
            heard: String::new(),
            retry_at: 0,
            hour_start: now,
            used_ns: 0,
            over_budget: false,
        }
    }

    /// What to wait on besides the microphone.
    pub fn wait_handles(&self) -> Vec<(RawHandle, u32)> {
        let mut v = Vec::new();
        if let Some(c) = &self.connector {
            v.push((c.handle(), signals::SIGNALED));
        }
        if let Some(s) = &self.stream {
            v.push((vweb::Connection::handle(s.connection()), signals::READABLE | signals::PEER_CLOSED));
        }
        v
    }

    fn recognising(&self) -> bool {
        self.connector.is_some() || self.stream.is_some()
    }

    /// Takes microphone audio.
    pub fn hear(&mut self, pcm: &[i16], now: u64) {
        self.partial.extend_from_slice(pcm);
        let whole = self.partial.len() / FRAME * FRAME;
        let frames: Vec<i16> = self.partial.drain(..whole).collect();
        for f in frames.chunks(FRAME) {
            self.vad.process(f);
            let speaking = self.vad.speaking();
            if speaking {
                self.last_speech_ns = now;
            }
            if self.recognising() {
                if self.held.len() + f.len() <= HOLD_MAX {
                    self.held.extend_from_slice(f);
                }
            } else if speaking && self.may_recognise(now) {
                vrt::println!("someone is speaking");
                self.held.clear();
                self.held.extend(self.preroll.drain(..));
                self.held.extend_from_slice(f);
                self.heard.clear();
                self.started_ns = now;
                self.connector = Connector::start(self.url.clone(), self.key.clone());
                if self.connector.is_none() {
                    self.retry_at = now + RETRY_NS;
                }
            } else {
                for &s in f {
                    if self.preroll.len() == PREROLL {
                        self.preroll.pop_front();
                    }
                    self.preroll.push_back(s);
                }
            }
        }
    }

    fn may_recognise(&mut self, now: u64) -> bool {
        if now >= self.hour_start + HOUR_NS {
            self.hour_start = now;
            self.used_ns = 0;
            self.over_budget = false;
        }
        if self.used_ns >= BUDGET_NS {
            if !self.over_budget {
                self.over_budget = true;
                vrt::println!("listening for its name paused: much speech this hour");
            }
            return false;
        }
        now >= self.retry_at
    }

    /// Ends recognition (quietly or after an error).
    fn stop(&mut self, now: u64) {
        if let Some(mut s) = self.stream.take() {
            let _ = s.send_text(&object! { "type" => "CloseStream" }.to_string());
            self.used_ns += now.saturating_sub(self.started_ns);
        }
        self.connector = None;
        self.held.clear();
        self.heard.clear();
    }

    /// Moves recognition along; returns what was said if it addressed the
    /// agent by name.
    pub fn poll(&mut self, now: u64) -> Option<String> {
        if let Some(c) = &self.connector
            && let Some(r) = c.take()
        {
            self.connector = None;
            match r {
                Ok(ws) => self.stream = Some(ws),
                Err(e) => {
                    vrt::println!("cannot listen for its name: {}", e);
                    let auth = matches!(e, WebError::Status { code: 401 | 403, .. });
                    self.retry_at = now + if auth { 10 * RETRY_NS } else { RETRY_NS };
                    self.held.clear();
                }
            }
        }
        let mut addressed = None;
        let mut failed = false;
        if let Some(ws) = &mut self.stream {
            // What was held while connecting, then the live audio.
            for chunk in self.held.chunks(PACKET) {
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                if ws.send_binary(&bytes).is_err() {
                    failed = true;
                    break;
                }
            }
            self.held.clear();
            loop {
                match ws.poll() {
                    Ok(Some(Message::Text(t))) => {
                        let Some(h) = deepgram::parse_listen_result(&t) else { continue };
                        if h.is_final && !h.text.trim().is_empty() {
                            if !self.heard.is_empty() {
                                self.heard.push(' ');
                            }
                            self.heard.push_str(h.text.trim());
                            if wake::addressed(&self.heard, &self.name).is_some() {
                                addressed = Some(core::mem::take(&mut self.heard));
                                break;
                            }
                        }
                        if h.speech_final {
                            self.heard.clear();
                        }
                    }
                    Ok(Some(Message::Close { .. })) | Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                }
            }
        }
        if addressed.is_some() {
            self.stop(now);
            vrt::println!("woken by its name");
            return addressed;
        }
        if failed {
            vrt::println!("listening for its name: the connection ended");
            self.stop(now);
            self.retry_at = now + RETRY_NS / 6;
        } else if self.recognising()
            && (now.saturating_sub(self.last_speech_ns) > QUIET_NS
                || now.saturating_sub(self.started_ns) > STREAM_MAX_NS)
        {
            self.stop(now);
        }
        None
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop(vrt::time::now_ns());
    }
}
