//! Listening for the agent's name while it is asleep.
//!
//! The microphone stays open and a voice detector runs here, on the
//! computer: while nobody speaks, nothing leaves it. When someone does, what
//! they say — with the moment before, so the first word is not cut — is
//! streamed to Deepgram's speech recognition (Nova-3, with the agent's name
//! as a key term), until they have been quiet for a few seconds, or nothing
//! was said in words for a while. Each piece of transcript, and what was
//! said since the last pause, is checked for the agent's name
//! ([`vagent::wake::call`]). A call wakes it once the sentence is over (the
//! recogniser may finish "Hey Veda," before the rest of it), and with the
//! rest of the sentence becomes the first thing the user said in the
//! conversation.
//!
//! The detector is [`vaudio::vad::Vad`]; music or a television can seem
//! like speech to it. When recognition heard no words for a while, the
//! sound it was given is taken for such a background: until it stops,
//! recognition starts again only for a sound that stands out from it
//! (louder than its usual peaks), so music playing does not keep the
//! recogniser busy, while someone calling over it still does. Recognition
//! costs a fraction of a conversation, and a budget caps how long it may
//! run each hour.

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
/// Recognition also stops after this long without a word (music).
const WORDLESS_NS: u64 = 8_000_000_000;
/// Over such sound, recognition starts again only for a sound this much
/// louder than its usual peaks ...
const STAND_OUT_DB: f32 = 10.0;
/// ... the loudest tenth of the last ten seconds (music builds up).
const BACKGROUND_FRAMES: usize = 500;
/// Such sound has stopped once the detector heard nothing like speech for
/// this long.
const BACKGROUND_GONE_NS: u64 = 5_000_000_000;
/// Speech starts an utterance after this long (ms): a name takes longer,
/// a knock or a cough does not.
const START_MS: u32 = 200;
/// The detector learns the room's background for this long before
/// anything starts recognition (until then, a fan sounds like speech).
const WARM_UP_NS: u64 = 1_600_000_000;
/// Near misses are logged with this many words at most.
const LOG_WORDS: usize = 8;
/// After a call, the rest of the sentence is awaited until the speaker
/// pauses, or no words came for this long ...
const CALL_REST_NS: u64 = 1_500_000_000;
/// ... and at most this long.
const CALL_MAX_NS: u64 = 6_000_000_000;

/// A call heard while the speaker may still be talking ("Hey Veda," ...
/// "what time is it?").
struct Call {
    /// What was said, from the call on.
    said: String,
    /// Something was asked, with the name or after it.
    asked: bool,
    /// The recogniser heard the speaker pause after the latest words.
    ended: bool,
    /// When the call was heard, and the latest words after it.
    at: u64,
    words_ns: u64,
}

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
    last_words_ns: u64,
    /// The level of each recent frame (dBFS), newest last.
    levels: VecDeque<f32>,
    /// The sound going on held no words (see the module documentation).
    wordless: bool,
    /// What was said in this stretch of speech (final transcripts).
    heard: String,
    /// A call waiting for the rest of its sentence.
    call: Option<Call>,
    retry_at: u64,
    /// When listening began.
    since_ns: u64,
    hour_start: u64,
    used_ns: u64,
    over_budget: bool,
}

impl Listener {
    /// Listens for `name`; `url` is Deepgram's (or a simulator's).
    pub fn new(name: &str, key: &str, url: String, now: u64) -> Listener {
        vrt::println!("listening for its name");
        let mut vad = Vad::new(MIC_RATE);
        vad.set_timing(START_MS, 600);
        Listener {
            name: name.into(),
            key: key.into(),
            url,
            vad,
            partial: Vec::new(),
            preroll: VecDeque::with_capacity(PREROLL),
            held: Vec::new(),
            connector: None,
            stream: None,
            started_ns: 0,
            last_speech_ns: 0,
            last_words_ns: 0,
            levels: VecDeque::with_capacity(BACKGROUND_FRAMES),
            wordless: false,
            heard: String::new(),
            call: None,
            retry_at: 0,
            since_ns: now,
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
            if self.levels.len() == BACKGROUND_FRAMES {
                self.levels.pop_front();
            }
            self.levels.push_back(vaudio::level::rms_dbfs(f));
            if self.wordless && now.saturating_sub(self.last_speech_ns) > BACKGROUND_GONE_NS {
                // The wordless sound stopped.
                self.wordless = false;
            }
            if self.recognising() {
                if self.held.len() + f.len() <= HOLD_MAX {
                    self.held.extend_from_slice(f);
                }
            } else if speaking && (!self.wordless || self.stands_out()) && self.may_recognise(now) {
                vrt::println!("someone is speaking");
                self.held.clear();
                self.held.extend(self.preroll.drain(..));
                self.held.extend_from_slice(f);
                self.heard.clear();
                self.started_ns = now;
                self.last_words_ns = now;
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

    /// The last tenth of a second is clearly louder than the background's
    /// usual peaks (the loudest tenth of the last seconds).
    fn stands_out(&self) -> bool {
        let mut sorted: Vec<f32> = self.levels.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        let peaks = sorted.get(sorted.len() * 9 / 10).copied().unwrap_or(-100.0);
        let now = self.levels.iter().rev().take(5).copied().fold(-100.0, f32::max);
        now > peaks + STAND_OUT_DB
    }

    fn may_recognise(&mut self, now: u64) -> bool {
        if now < self.since_ns + WARM_UP_NS {
            return false;
        }
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
        self.call = None;
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
        let mut failed = false;
        let mut near_misses = Vec::new();
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
                        let piece = h.text.trim();
                        if let Some(c) = &mut self.call {
                            // The rest of the sentence (interim words say
                            // that the speaker goes on).
                            if !piece.is_empty() {
                                c.asked = true;
                                c.ended = false;
                                c.words_ns = now;
                                if h.is_final {
                                    c.said.push(' ');
                                    c.said.push_str(piece);
                                }
                            }
                            c.ended |= h.speech_final;
                            continue;
                        }
                        if h.is_final && !piece.is_empty() {
                            self.last_words_ns = now;
                            if !self.heard.is_empty() {
                                self.heard.push(' ');
                            }
                            self.heard.push_str(piece);
                            // This piece on its own (other words may come
                            // before it: a song, someone else), or with what
                            // came before it since the last pause ("Hey" ...
                            // "Veda").
                            let call = wake::call(piece, &self.name).or_else(|| wake::call(&self.heard, &self.name));
                            if let Some(said) = call {
                                let asked = wake::addressed(&said, &self.name).is_some_and(|r| !r.is_empty());
                                self.call = Some(Call { said, asked, ended: h.speech_final, at: now, words_ns: now });
                                continue;
                            }
                            if wake::resembles(piece, &self.name) {
                                near_misses
                                    .push(piece.split_whitespace().take(LOG_WORDS).collect::<Vec<_>>().join(" "));
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
        for m in near_misses {
            vrt::println!("heard something like its name, not a call: \"{}\"", m);
        }
        // A call wakes the agent once the speaker has finished the sentence:
        // the recogniser heard them pause (after just the name, the detector
        // must hear them stop too), or no words came for a moment.
        let finished = self.call.as_ref().is_some_and(|c| {
            failed
                || (c.ended && (c.asked || !self.vad.speaking()))
                || now.saturating_sub(c.words_ns) > CALL_REST_NS
                || now.saturating_sub(c.at) > CALL_MAX_NS
        });
        if finished {
            let said = self.call.take().map(|c| c.said);
            self.stop(now);
            vrt::println!("woken by its name");
            return said;
        }
        if failed {
            vrt::println!("listening for its name: the connection ended");
            self.stop(now);
            self.retry_at = now + RETRY_NS / 6;
        } else if self.recognising() && self.call.is_none() {
            let wordless = self.stream.is_some() && now.saturating_sub(self.last_words_ns) > WORDLESS_NS;
            if wordless && !self.wordless {
                vrt::println!("sound without words; listening for its name over it");
            }
            self.wordless |= wordless;
            if wordless
                || now.saturating_sub(self.last_speech_ns) > QUIET_NS
                || now.saturating_sub(self.started_ns) > STREAM_MAX_NS
            {
                self.stop(now);
            }
        }
        None
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop(vrt::time::now_ns());
    }
}
