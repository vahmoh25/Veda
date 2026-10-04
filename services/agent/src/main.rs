//! `agent` — the voice agent that lives in Vindows.
//!
//! A system service, started at boot and restarted if it fails. It is the
//! presence the user talks to: it listens (or sleeps until called), holds
//! conversations through the Deepgram Voice Agent API, speaks, and acts on
//! the system and its applications on the user's behalf.
//!
//! * **Conversations** ([`session`]): waking (from the tray, a shortcut, a
//!   due reminder) opens a Voice Agent WebSocket with the agent's
//!   character, what it knows and the functions it may call; the
//!   microphone streams to Deepgram and the voice comes back. When the user
//!   starts talking, the voice stops at once. A conversation ends after a
//!   quiet spell (or when the agent says goodbye), to save cost; the next
//!   one starts from the recent history, so it feels continuous.
//! * **Actions** ([`worker`]): function calls run on the worker thread.
//!   Anything above routine risk waits for the user's approval, which the
//!   shell asks for — in the agent's window when it is open, otherwise in a
//!   notification — and the outcome is told to the agent.
//! * **Who may do what** is decided by the registry's identity of each
//!   connection: any program may register its abilities; only the shell
//!   attaches the interface and answers approvals; only Settings changes
//!   the configuration ([`admin`]).
//! * **State** ([`shared`]): settings, the key, memory, permissions and
//!   timers live in the agent's private directory ([`store`]).

#![no_std]
#![no_main]

extern crate alloc;

mod admin;
mod listen;
mod session;
mod shared;
mod store;
mod ui;
mod voice;
mod worker;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vagent::deepgram::{self, Role, ServerMessage};
use vagent::gate::{self, EchoGate, Verdict};
use vagent::prompt::{self, PromptContext};
use vagent::tools;
use vjson::object;
use vproto::agent::{
    AgentConfig, AgentError, AgentEvent, AgentState, AgentStatus, ApprovalRequest, MemoryItem, ModelCatalog,
    Permission, Risk, UiLink, agent,
};
use vproto::init::ClientIdentity;
use vrt::object::Channel;
use vrt::println;
use vrt::sync::Mutex;

use session::{Connector, Incoming, Session};
use shared::Shared;
use ui::Ui;
use voice::{MIC_PACKET, Voice};
use worker::{Done, Job, Outcome, Pending, Worker};

vrt::entry!(main);

/// An approval request expires after this long.
const APPROVAL_TTL_NS: u64 = 180_000_000_000;
/// How often the interface's live levels are refreshed.
const LIVE_PERIOD_NS: u64 = 33_000_000;
/// Context carried into a new conversation.
const HISTORY_CHARS: usize = 3000;
/// Only conversation from the last few hours is carried over.
const HISTORY_AGE_S: u64 = 6 * 3600;

/// A client connection.
struct Conn {
    channel: Channel,
    who: ClientIdentity,
}

/// An action waiting for the user.
struct Approval {
    id: u64,
    pending: Pending,
    created: u64,
}

struct Agent {
    shared: Arc<Mutex<Shared>>,
    /// The status as published (for Settings' thread).
    published: Arc<Mutex<AgentStatus>>,
    state: AgentState,
    detail: String,
    muted: bool,
    window_open: bool,
    conns: BTreeMap<u64, Conn>,
    next_key: u64,
    ui: Option<Ui>,
    voice: Option<Voice>,
    worker: Worker,
    connector: Option<Connector>,
    session: Option<Session>,
    approvals: Vec<Approval>,
    next_approval: u64,
    /// Results of approved actions go back to the conversation as a notice
    /// (the function call that asked has long been answered).
    approved: BTreeMap<String, String>,
    /// Calls the language model is waiting on.
    calls_in_flight: usize,
    mic_buf: Vec<i16>,
    /// What was playing while `mic_buf` was recorded, and `mic_buf` before
    /// echo cancellation (empty if the audio service does not say).
    playing_buf: Vec<i16>,
    recorded_buf: Vec<i16>,
    /// Microphone audio short of a whole packet (and what goes with it).
    mic_pending: Vec<i16>,
    playing_pending: Vec<i16>,
    recorded_pending: Vec<i16>,
    /// Keeps the agent's own voice from the recogniser.
    gate: EchoGate,
    /// Packets the gate held back, the newest last (sent if the user turns
    /// out to be talking).
    preroll: VecDeque<Vec<i16>>,
    /// Notices to give the agent when the next conversation starts.
    on_wake: Vec<String>,
    seen_config: u64,
    seen_apps: u64,
    last_live_ns: u64,
    /// The Voice Agent endpoint (a local simulator in tests).
    endpoint: String,
    /// Start a conversation as soon as the agent runs (tests).
    wake_at_start: bool,
    /// Listening for the agent's name while it is asleep.
    listener: Option<listen::Listener>,
    /// Where speech is recognised then (a local simulator in tests).
    listen_endpoint: Option<String>,
    listen_retry_at: u64,
}

impl Agent {
    fn config(&self) -> vagent::config::Config {
        self.shared.lock().config.clone()
    }

    fn status(&self) -> AgentStatus {
        let s = self.shared.lock();
        AgentStatus {
            state: self.state,
            name: s.config.name.clone(),
            muted: self.muted,
            configured: s.key.is_some(),
            detail: self.detail.clone(),
            pending: self.approvals.len() as u32,
        }
    }

    /// Changes the state and tells the interface.
    fn set_state(&mut self, state: AgentState, detail: &str) {
        if self.state == state && self.detail == detail {
            return;
        }
        if self.state != state {
            println!("state: {:?}{}", state, if detail.is_empty() { String::new() } else { format!(" ({detail})") });
        }
        self.state = state;
        self.detail = detail.to_string();
        self.publish();
    }

    fn publish(&mut self) {
        let status = self.status();
        *self.published.lock() = status.clone();
        if let Some(ui) = &self.ui {
            ui.live().state.store(status.state as u32, core::sync::atomic::Ordering::Relaxed);
            if !ui.send(AgentEvent::Status { status }) {
                self.ui = None;
            }
        }
    }

    /// The resting state: asleep, or off without a key.
    fn rest(&mut self) {
        let (configured, enabled) = {
            let s = self.shared.lock();
            (s.key.is_some() || self.endpoint != deepgram::AGENT_URL, s.config.enabled)
        };
        if !enabled {
            self.set_state(AgentState::Off, "The agent is switched off in Settings.");
        } else if !configured {
            self.set_state(AgentState::Off, "Add your Deepgram API key in Settings to talk with me.");
        } else {
            self.set_state(AgentState::Asleep, "");
        }
        self.update_listening();
    }

    /// Listens for the agent's name while it is asleep (when the user
    /// wants that and the microphone is on), and stops otherwise.
    fn update_listening(&mut self) {
        let (key, name, wanted) = {
            let s = self.shared.lock();
            (s.key.clone(), s.config.name.clone(), s.config.enabled && s.config.listen_for_name)
        };
        let key = key.or_else(|| self.listen_endpoint.as_ref().map(|_| String::from("test")));
        let listen = wanted
            && key.is_some()
            && !self.muted
            && self.state == AgentState::Asleep
            && self.session.is_none()
            && self.connector.is_none();
        if !listen {
            if self.listener.take().is_some()
                && self.session.is_none()
                && let Some(v) = &mut self.voice
            {
                v.close_mic();
            }
            return;
        }
        if self.listener.is_some() {
            return;
        }
        let Some(v) = &mut self.voice else { return };
        if !v.open_mic() {
            return;
        }
        let url = match &self.listen_endpoint {
            Some(e) => e.clone(),
            None => deepgram::listen_url(&[name.as_str()]),
        };
        self.listener = Some(listen::Listener::new(&name, &key.unwrap_or_default(), url, vrt::time::now_ns()));
    }

    // ---- conversations -------------------------------------------------

    /// Starts a conversation (if none is running).
    fn wake(&mut self, reason: &str) {
        if self.session.is_some() || self.connector.is_some() {
            return;
        }
        let (key, enabled) = {
            let s = self.shared.lock();
            (s.key.clone(), s.config.enabled)
        };
        if !enabled {
            self.rest();
            return;
        }
        let key = match key {
            Some(k) => k,
            None if self.endpoint != deepgram::AGENT_URL => "test".into(),
            None => {
                self.rest();
                return;
            }
        };
        println!("waking ({reason})");
        self.listener = None;
        if let Some(v) = &mut self.voice
            && !self.muted
            && !v.open_mic()
        {
            println!("listening without a microphone");
        }
        match Connector::start(self.endpoint.clone(), key) {
            Some(c) => {
                self.connector = Some(c);
                self.set_state(AgentState::Waking, "");
            }
            None => self.set_state(AgentState::Error, "The agent could not start a conversation."),
        }
    }

    /// The prompt for a conversation.
    fn build_prompt(&self) -> String {
        let installed = worker::installed_apps();
        let (memory, infos, name, first) = {
            let s = self.shared.lock();
            (
                s.memory.prompt_section(),
                s.apps.clone(),
                s.config.name.clone(),
                s.memory.facts.is_empty() && s.memory.turns.is_empty(),
            )
        };
        let apps = worker::catalog(&installed, &infos);
        let screen = screen_summary();
        let now = worker::now_text();
        prompt::system_prompt(&PromptContext {
            agent_name: &name,
            now: &now,
            screen: &screen,
            memory: &memory,
            apps: &apps,
            first_meeting: first,
        })
    }

    /// The `Settings` message for a new conversation.
    fn settings(&self) -> vjson::Value {
        let c = self.config();
        let since = (vrt::time::unix_time_ns() / 1_000_000_000).saturating_sub(HISTORY_AGE_S);
        let history = self.shared.lock().memory.recent_turns(HISTORY_CHARS, since);
        deepgram::settings(&deepgram::SessionSettings {
            listen_model: c.listen_model.clone(),
            keyterms: alloc::vec![c.name.clone()],
            think_provider: c.think_provider.clone(),
            think_model: c.think_model.clone(),
            prompt: self.build_prompt(),
            functions: tools::definitions(),
            voice: c.voice.clone(),
            speed: c.speed,
            history,
            greeting: None,
        })
    }

    fn connected(&mut self, ws: vweb::ws::WebSocket<vweb::Conn>) {
        let now = vrt::time::now_ns();
        let mut s = Session::new(ws, now);
        let settings = self.settings();
        if let Err(e) = s.send(&settings) {
            println!("cannot configure the conversation: {}", e);
            self.end_conversation(Some("The conversation could not be set up."));
            return;
        }
        for notice in core::mem::take(&mut self.on_wake) {
            let _ = s.inject(&notice);
        }
        let (gc, ga) = {
            let sh = self.shared.lock();
            (sh.config_generation, sh.apps_generation)
        };
        self.seen_config = gc;
        self.seen_apps = ga;
        self.session = Some(s);
    }

    /// Ends the conversation; `error` keeps the agent in the error state.
    fn end_conversation(&mut self, error: Option<&str>) {
        if let Some(mut s) = self.session.take() {
            s.close();
            println!("conversation ended after {} s", (vrt::time::now_ns() - s.started_ns) / 1_000_000_000);
            let g = &mut self.gate;
            if g.held > 0 {
                let (cancelled, recorded) = g.coupling_db();
                println!(
                    "kept {:.1} s of the agent's own voice from the recogniser (its echo up to {:.0} dB after cancellation, {:.0} dB before), the user talked over it {} time(s)",
                    g.held as f32 * 0.02,
                    cancelled,
                    recorded,
                    g.openings
                );
                g.held = 0;
                g.openings = 0;
            }
        }
        self.mic_pending.clear();
        self.playing_pending.clear();
        self.recorded_pending.clear();
        self.preroll.clear();
        self.connector = None;
        self.calls_in_flight = 0;
        if let Some(v) = &mut self.voice {
            v.close_mic();
            v.close_speaker();
        }
        self.shared.lock().save_memory();
        match error {
            Some(e) => self.set_state(AgentState::Error, e),
            None => self.rest(),
        }
    }

    fn send(&mut self, v: &vjson::Value) {
        let failed = match &mut self.session {
            Some(s) => s.send(v).is_err(),
            None => false,
        };
        if failed {
            self.end_conversation(Some("The connection to Deepgram was lost."));
        }
    }

    /// Tells the agent something that happened (as a turn of its own).
    fn notice(&mut self, text: &str) {
        match &mut self.session {
            Some(s) => {
                if s.inject(text).is_err() {
                    self.end_conversation(Some("The connection to Deepgram was lost."));
                }
            }
            None => {
                self.on_wake.push(text.to_string());
                self.wake("a notice");
            }
        }
    }

    fn handle_message(&mut self, m: ServerMessage) {
        let now = vrt::time::now_ns();
        let unix = vrt::time::unix_time_ns() / 1_000_000_000;
        match m {
            ServerMessage::Welcome { request_id } => println!("connected to Deepgram (request {request_id})"),
            ServerMessage::SettingsApplied => {
                if let Some(s) = &mut self.session
                    && s.on_ready().is_err()
                {
                    self.end_conversation(Some("The connection to Deepgram was lost."));
                    return;
                }
                self.set_state(AgentState::Listening, "");
            }
            ServerMessage::ConversationText { role, content } => {
                println!("{}: {}", if role == Role::User { "user" } else { "agent" }, content);
                if let Some(s) = &mut self.session {
                    s.last_activity_ns = now;
                }
                self.shared.lock().memory.add_turn(role, &content, unix);
            }
            ServerMessage::History { role, content } => {
                self.shared.lock().memory.add_turn(role, &content, unix);
            }
            ServerMessage::UserStartedSpeaking => {
                if let Some(v) = &mut self.voice
                    && v.speaking()
                {
                    let gate = if self.gate.is_open() {
                        "open"
                    } else if self.gate.echoing() {
                        "holding"
                    } else {
                        "passing"
                    };
                    println!("interrupted: voice stopped (echo gate {gate})");
                    v.stop_speaking();
                }
                if let Some(s) = &mut self.session {
                    s.last_activity_ns = now;
                    s.agent_turn = false;
                    // Talking over the goodbye: the conversation goes on.
                    // (Before it, the user may just be finishing the
                    // sentence the agent answered early.)
                    if s.goodbye_started {
                        s.cancel_end();
                    }
                }
                self.set_state(AgentState::Listening, "");
            }
            ServerMessage::AgentThinking { .. } => self.set_state(AgentState::Thinking, ""),
            ServerMessage::FunctionCallRequest { calls } => {
                // More work after asking to end: the conversation goes on.
                if calls.iter().any(|c| c.name != tools::names::END_CONVERSATION)
                    && let Some(s) = &mut self.session
                {
                    s.cancel_end();
                }
                for c in calls.into_iter().filter(|c| c.client_side) {
                    println!("call {} {}", c.name, c.arguments.chars().take(200).collect::<String>());
                    self.calls_in_flight += 1;
                    self.worker.submit(Job::Call { args: c.args(), id: c.id, name: c.name, approved: false });
                }
                self.set_state(AgentState::Thinking, "");
            }
            ServerMessage::FunctionCallCancelled { ids } => {
                if let Some(s) = &mut self.session {
                    s.cancelled.extend(ids);
                }
            }
            ServerMessage::AgentStartedSpeaking { latency } => {
                if let Some(l) = latency {
                    println!("speaking (latency {:.2} s)", l);
                }
                self.agent_speaks();
            }
            ServerMessage::AgentAudioDone => {
                if let Some(v) = &mut self.voice {
                    v.answer_complete();
                }
                if let Some(s) = &mut self.session {
                    s.audio_done = true;
                    s.last_activity_ns = now;
                    if s.goodbye_started {
                        s.goodbye_sent = true;
                    }
                }
            }
            ServerMessage::LatencyReport { total, think } => match (total, think) {
                (Some(t), Some(m)) if m > 0.0 => println!("answered in {:.2} s (language model {:.2} s)", t, m),
                (Some(t), _) => println!("answered in {:.2} s", t),
                _ => {}
            },
            ServerMessage::Updated { what } => println!("{what}"),
            ServerMessage::InjectionRefused { message } => println!("notice refused: {message}"),
            ServerMessage::Error { code, description } => {
                println!("Deepgram error {code}: {description}");
                let lower = description.to_lowercase();
                if code.contains("AUTH") || lower.contains("auth") || lower.contains("credential") {
                    self.end_conversation(Some("Deepgram did not accept the API key. Check it in Settings."));
                }
            }
            ServerMessage::Warning { code, description } => println!("Deepgram warning {code}: {description}"),
            // Echoes of what this side sent (kept by Deepgram's history),
            // and Flux's turn-taking events (its decisions are acted on by
            // Deepgram already).
            ServerMessage::Other { kind }
                if matches!(
                    kind.as_str(),
                    "FunctionCallResponse" | "History" | "StartOfTurn" | "EndOfTurn" | "EagerEndOfTurn" | "TurnResumed"
                ) => {}
            ServerMessage::Other { kind } => {
                if let Some(s) = &mut self.session
                    && s.unknown.insert(kind.clone())
                {
                    println!("Deepgram sent a message not handled here: {kind}");
                }
            }
        }
    }

    /// The agent's voice is coming: its turn starts (once per turn).
    fn agent_speaks(&mut self) {
        let Some(s) = &mut self.session else { return };
        s.last_activity_ns = vrt::time::now_ns();
        if s.end_requested_at.is_some() {
            s.goodbye_started = true;
        }
        if s.agent_turn && !s.audio_done {
            return;
        }
        s.agent_turn = true;
        s.audio_done = false;
        self.set_state(AgentState::Speaking, "");
    }

    /// Handles what Deepgram sent. Returns false if more may be waiting
    /// (it stops after a while, so the rest of the loop keeps running).
    fn poll_session(&mut self) -> bool {
        for _ in 0..200 {
            let Some(s) = &mut self.session else { return true };
            match s.poll() {
                Ok(None) => return true,
                Ok(Some(Incoming::Message(m))) => self.handle_message(m),
                Ok(Some(Incoming::Audio(pcm))) => {
                    self.agent_speaks();
                    if let Some(v) = &mut self.voice {
                        v.speak(&pcm);
                    }
                }
                Ok(Some(Incoming::Closed { code, reason })) => {
                    println!("Deepgram closed the conversation ({code} {reason})");
                    let error =
                        if code == 1000 || code == 1005 { None } else { Some("Deepgram ended the conversation.") };
                    self.end_conversation(error);
                    return true;
                }
                Err(e) => {
                    println!("connection to Deepgram lost: {}", e);
                    self.end_conversation(Some("The connection to Deepgram was lost."));
                    return true;
                }
            }
        }
        false
    }

    // ---- actions and approvals -------------------------------------------

    fn finished(&mut self, done: Done) {
        let Done::Call { id, name, outcome } = done;
        if let Some(label) = self.approved.remove(&id) {
            // An approved action: tell the agent how it went (a result can
            // be a failure: the file was gone by then, say).
            let text = match outcome {
                Outcome::Result(r) => match vjson::parse(&r).ok().filter(|v| v["ok"].as_bool() == Some(false)) {
                    Some(v) => {
                        let why = v.str("error").unwrap_or("it failed").to_string();
                        println!("approved action failed: {why}");
                        format!("[Vindows] The user allowed \"{label}\", but it failed: {why}")
                    }
                    None => format!("[Vindows] The user allowed \"{label}\" and it was done. Result: {r}"),
                },
                _ => format!("[Vindows] The user allowed \"{label}\", but it could not be done."),
            };
            self.notice(&text);
            return;
        }
        self.calls_in_flight = self.calls_in_flight.saturating_sub(1);
        if self.session.as_ref().is_some_and(|s| s.cancelled.contains(&id)) {
            return;
        }
        let content = match outcome {
            Outcome::Result(r) => r,
            Outcome::End => {
                if let Some(s) = &mut self.session {
                    s.request_end(vrt::time::now_ns());
                }
                tools::ok(vjson::Value::Null)
            }
            Outcome::Approval(p) => self.ask(p),
        };
        println!("result {} {}", name, content.chars().take(200).collect::<String>());
        self.send(&deepgram::function_response(&id, &name, &content));
        if self.calls_in_flight == 0 && self.state == AgentState::Thinking {
            // What the agent said before the call may still be playing.
            let speaking = self.voice.as_ref().is_some_and(Voice::speaking);
            self.set_state(if speaking { AgentState::Speaking } else { AgentState::Listening }, "");
        }
    }

    /// Asks the user (through the shell) to approve an action; returns the
    /// function result that tells the agent to wait.
    fn ask(&mut self, p: Pending) -> String {
        if self.ui.is_none() {
            return tools::error("this needs the user's approval on screen, but the screen is not available");
        }
        let id = self.next_approval;
        self.next_approval += 1;
        let request = ApprovalRequest {
            id,
            app: p.app.clone(),
            action: p.action.clone(),
            detail: p.detail.clone(),
            risk: p.risk,
            allow_always: p.risk == Risk::Sensitive,
        };
        let label = p.action.clone();
        println!("approval {} requested: {} ({})", id, label, p.detail);
        self.approvals.push(Approval { id, pending: p, created: vrt::time::now_ns() });
        if let Some(ui) = &self.ui
            && !ui.send(AgentEvent::Approval { request })
        {
            self.ui = None;
        }
        self.publish();
        object! {
            "ok" => false,
            "waiting_for_approval" => true,
            "message" => format!("Vindows is asking the user to allow \"{label}\" on screen. Tell them briefly; the outcome will follow."),
        }
        .to_string()
    }

    fn decide(&mut self, id: u64, allow: bool, always: bool) -> Result<(), AgentError> {
        let i = self.approvals.iter().position(|a| a.id == id).ok_or(AgentError::NotFound)?;
        let a = self.approvals.remove(i);
        if let Some(ui) = &self.ui {
            let _ = ui.send(AgentEvent::ApprovalDone { id });
        }
        self.publish();
        let label = a.pending.action.clone();
        println!("approval {} {}: {}", id, if allow { "allowed" } else { "declined" }, label);
        if !allow {
            self.notice(&format!("[Vindows] The user declined \"{label}\". It was not done."));
            return Ok(());
        }
        if always && a.pending.risk == Risk::Sensitive {
            let mut s = self.shared.lock();
            if s.permissions.allow_always(
                &a.pending.key,
                &format!("{}: {}", a.pending.app, label),
                tools::Risk::Sensitive,
            ) {
                s.save_permissions();
            }
        }
        let call_id = format!("approved-{id}");
        self.approved.insert(call_id.clone(), label);
        self.worker.submit(Job::Call { id: call_id, name: a.pending.name, args: a.pending.args, approved: true });
        Ok(())
    }

    fn expire_approvals(&mut self, now: u64) {
        let expired: Vec<u64> =
            self.approvals.iter().filter(|a| now - a.created > APPROVAL_TTL_NS).map(|a| a.id).collect();
        for id in expired {
            if let Some(i) = self.approvals.iter().position(|a| a.id == id) {
                let a = self.approvals.remove(i);
                if let Some(ui) = &self.ui {
                    let _ = ui.send(AgentEvent::ApprovalDone { id });
                }
                if self.session.is_some() {
                    self.notice(&format!(
                        "[Vindows] The request to \"{}\" expired without an answer.",
                        a.pending.action
                    ));
                }
            }
        }
    }

    // ---- the loop ----------------------------------------------------------

    fn accept(&mut self, listener: &Channel) {
        while let Some((channel, who)) = vproto::accept_with_identity(listener) {
            if who.app && who.name == "settings" {
                admin::spawn(channel, self.shared.clone(), self.published.clone(), self.worker.registrar());
                continue;
            }
            self.next_key += 1;
            self.conns.insert(self.next_key, Conn { channel, who });
        }
    }

    fn serve(&mut self, key: u64) -> bool {
        loop {
            let Some(c) = self.conns.get(&key) else { return false };
            match c.channel.read() {
                Ok(msg) => {
                    let who = c.who.clone();
                    let reply = agent::dispatch(&mut Client { agent: self, who }, msg);
                    if let (Ok(reply), Some(c)) = (reply, self.conns.get(&key)) {
                        let _ = reply.send(&c.channel);
                    }
                }
                Err(vabi::Error::ShouldWait) => return true,
                Err(_) => return false,
            }
        }
    }

    fn mic(&mut self) {
        let Some(v) = &mut self.voice else { return };
        self.mic_buf.clear();
        self.playing_buf.clear();
        self.recorded_buf.clear();
        if v.read_mic(&mut self.mic_buf, &mut self.playing_buf, &mut self.recorded_buf) == 0 {
            return;
        }
        if self.muted {
            return;
        }
        if self.session.is_none()
            && let Some(l) = &mut self.listener
        {
            l.hear(&self.mic_buf, vrt::time::now_ns());
            return;
        }
        if self.session.is_none() {
            return;
        }
        let out = self.gate_mic();
        let failed = match &mut self.session {
            Some(s) => s.send_mic(&out).is_err(),
            None => false,
        };
        if failed {
            self.end_conversation(Some("The connection to Deepgram was lost."));
        }
    }

    /// The microphone as Deepgram should hear it: 20 ms at a time, through
    /// the echo gate, so that what is left of the agent's own voice after
    /// echo cancellation is silence to the recogniser (see `vagent::gate`).
    fn gate_mic(&mut self) -> Vec<i16> {
        use vaudio::level::rms_dbfs;
        self.mic_pending.extend_from_slice(&self.mic_buf);
        self.playing_pending.extend_from_slice(&self.playing_buf);
        self.recorded_pending.extend_from_slice(&self.recorded_buf);
        let whole = self.mic_pending.len() / MIC_PACKET * MIC_PACKET;
        // The gate works while the agent's voice is audible (and against the
        // echo of everything playing; of the voice alone if the audio
        // service does not say what plays).
        let voice_db = self.voice.as_ref().map_or(-100.0, Voice::output_db);
        let mut out = Vec::with_capacity(whole + gate::PREROLL_PACKETS * MIC_PACKET);
        for (k, p) in self.mic_pending[..whole].chunks(MIC_PACKET).enumerate() {
            let at = k * MIC_PACKET..(k + 1) * MIC_PACKET;
            let mic_db = rms_dbfs(p);
            let playing_db = self.playing_pending.get(at.clone()).map_or(voice_db, rms_dbfs);
            let recorded_db = self.recorded_pending.get(at).map_or(mic_db, rms_dbfs);
            match self.gate.packet(mic_db, recorded_db, playing_db, voice_db) {
                Verdict::Pass => {
                    self.preroll.clear();
                    out.extend_from_slice(p);
                }
                Verdict::Through => out.extend_from_slice(p),
                Verdict::Hold => {
                    if self.preroll.len() == gate::PREROLL_PACKETS {
                        self.preroll.pop_front();
                    }
                    self.preroll.push_back(p.to_vec());
                    out.extend(core::iter::repeat_n(0i16, p.len()));
                }
                Verdict::Open => {
                    println!(
                        "the user talks over the agent (microphone at {:.0} dBFS, its echo at most {:.0})",
                        mic_db,
                        self.gate.expected_db()
                    );
                    for q in self.preroll.drain(..) {
                        out.extend_from_slice(&q);
                    }
                    out.extend_from_slice(p);
                }
            }
        }
        self.mic_pending.drain(..whole);
        let side = whole.min(self.playing_pending.len());
        self.playing_pending.drain(..side);
        let side = whole.min(self.recorded_pending.len());
        self.recorded_pending.drain(..side);
        out
    }

    /// Ends quiet conversations; finishes the agent's turns.
    fn housekeeping(&mut self, now: u64) {
        /// How long to wait for a goodbye after the agent asked to end.
        const END_GRACE_NS: u64 = 5_000_000_000;
        /// The longest goodbye.
        const END_LIMIT_NS: u64 = 30_000_000_000;
        let idle_ns = self.config().idle_timeout_s as u64 * 1_000_000_000;
        let speaking = self.voice.as_ref().is_some_and(Voice::speaking);
        let mut end = false;
        if let Some(s) = &mut self.session {
            if s.agent_turn && s.audio_done && !speaking {
                s.agent_turn = false;
                s.last_activity_ns = now;
            }
            // Asked to end: once the goodbye has been said, or if none
            // comes (it was said before the call) while all is quiet.
            if let Some(at) = s.end_requested_at {
                let quiet = !s.agent_turn && !speaking;
                let none_coming = !s.goodbye_started && now - at.max(s.last_activity_ns) > END_GRACE_NS;
                if (quiet && (s.goodbye_sent || none_coming)) || now - at > END_LIMIT_NS {
                    end = true;
                }
            }
            if speaking {
                s.last_activity_ns = now;
            }
            if s.ready
                && !s.agent_turn
                && self.calls_in_flight == 0
                && self.approvals.is_empty()
                && now - s.last_activity_ns > idle_ns
            {
                println!("quiet for a while; going to sleep");
                end = true;
            }
            if s.keep_alive(now).is_err() {
                end = true;
            }
        }
        if end {
            self.end_conversation(None);
        } else if self.session.as_ref().is_some_and(|s| s.ready && !s.agent_turn) && self.state == AgentState::Speaking
        {
            self.set_state(AgentState::Listening, "");
        }
        // Settings or applications changed during a conversation.
        let (gc, ga) = {
            let s = self.shared.lock();
            (s.config_generation, s.apps_generation)
        };
        if self.session.as_ref().is_some_and(|s| s.ready) {
            if gc != self.seen_config {
                let c = self.config();
                self.send(&deepgram::update_speak(&c.voice, c.speed));
                self.send(&deepgram::update_think(&c.think_provider, &c.think_model));
            }
            if ga != self.seen_apps || gc != self.seen_config {
                let p = self.build_prompt();
                self.send(&deepgram::update_prompt(&p));
            }
        }
        if gc != self.seen_config {
            if self.session.is_none() {
                // A new name is listened for from now on.
                self.listener = None;
                self.rest();
            }
            // The interface shows the agent's name.
            self.publish();
        }
        self.seen_config = gc;
        self.seen_apps = ga;
        // The microphone may not have been there when the agent fell asleep.
        if self.listener.is_none() && self.state == AgentState::Asleep && now >= self.listen_retry_at {
            self.listen_retry_at = now + 5_000_000_000;
            self.update_listening();
        }
        self.check_timers();
        self.expire_approvals(now);
    }

    fn check_timers(&mut self) {
        let unix = vrt::time::unix_time_ns() / 1_000_000_000;
        let due: Vec<shared::Timer> = {
            let mut s = self.shared.lock();
            let due: Vec<shared::Timer> = s.timers.iter().filter(|t| t.due <= unix).cloned().collect();
            if !due.is_empty() {
                s.timers.retain(|t| t.due > unix);
                s.save_timers();
            }
            due
        };
        for t in due {
            println!("timer due: {}", t.label);
            if let Some(shell) = vproto::connect(vproto::shell::shell::NAME).ok().map(vproto::shell::shell::Client::new)
            {
                let _ = shell.notify(t.label.clone(), "Your reminder is due.".into(), "agent".into());
            }
            self.notice(&format!("[Vindows] The timer \"{}\" is due now. Tell the user.", t.label));
        }
    }

    fn next_timer_ns(&self, now: u64) -> u64 {
        let unix = vrt::time::unix_time_ns() / 1_000_000_000;
        match self.shared.lock().timers.first() {
            Some(t) => now + t.due.saturating_sub(unix).min(3600) * 1_000_000_000 + 50_000_000,
            None => vabi::DEADLINE_INFINITE,
        }
    }

    fn update_live(&mut self, now: u64) {
        if now.saturating_sub(self.last_live_ns) < LIVE_PERIOD_NS {
            return;
        }
        self.last_live_ns = now;
        if let (Some(ui), Some(v)) = (&self.ui, &mut self.voice) {
            v.pump();
            ui.live().set_levels(v.output_level(), v.mic_level());
        }
    }

    fn run(&mut self, listener: Channel) -> ! {
        const LISTENER: u64 = 1;
        const WORKER: u64 = 2;
        const CONNECTOR: u64 = 3;
        const SESSION: u64 = 4;
        const VOICE: u64 = 5;
        // Settings or applications changed (housekeeping looks every pass).
        const CHANGED: u64 = 6;
        const CONN: u64 = 1 << 32;
        self.rest();
        if self.wake_at_start {
            self.wake("start-up option");
        }
        // Deepgram's messages are waiting beyond what one pass handled.
        let mut backlog = false;
        loop {
            let now = vrt::time::now_ns();
            let mut items: Vec<WaitItem> = Vec::new();
            let mut keys: Vec<u64> = Vec::new();
            let mut add = |h, s, k| {
                items.push(WaitItem { handle: h, signals: s, ..Default::default() });
                keys.push(k);
            };
            add(listener.raw(), signals::READABLE, LISTENER);
            if let Some(e) = &self.shared.lock().changed {
                add(e.raw(), signals::SIGNALED, CHANGED);
            }
            add(self.worker.handle(), signals::SIGNALED, WORKER);
            if let Some(c) = &self.connector {
                add(c.handle(), signals::SIGNALED, CONNECTOR);
            }
            if let Some(s) = &self.session {
                add(s.handle(), signals::READABLE | signals::PEER_CLOSED, SESSION);
            }
            if let Some(v) = &self.voice {
                for (h, s) in v.wait_handles() {
                    add(h, s, VOICE);
                }
            }
            if let Some(l) = &self.listener {
                for (h, s) in l.wait_handles() {
                    add(h, s, VOICE);
                }
            }
            for (&k, c) in &self.conns {
                add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, CONN + k);
            }
            let active = self.session.is_some() || self.connector.is_some();
            let mut deadline = self.next_timer_ns(now);
            if active {
                deadline = deadline.min(now + if self.ui.is_some() { LIVE_PERIOD_NS } else { 250_000_000 });
            }
            if !self.approvals.is_empty() {
                deadline = deadline.min(now + 1_000_000_000);
            }
            if backlog {
                deadline = now;
            }
            let _ = vrt::object::wait_many(&mut items, deadline);
            let mut closed = Vec::new();
            for (it, &k) in items.iter().zip(&keys) {
                if it.observed == 0 {
                    continue;
                }
                match k {
                    LISTENER => self.accept(&listener),
                    WORKER => {
                        for d in self.worker.take() {
                            self.finished(d);
                        }
                    }
                    CONNECTOR => {
                        if let Some(r) = self.connector.as_ref().and_then(Connector::take) {
                            self.connector = None;
                            match r {
                                Ok(ws) => self.connected(ws),
                                Err(e) => {
                                    println!("cannot reach Deepgram: {}", e);
                                    let msg = match e {
                                        vweb::WebError::Status { code: 401 | 403, .. } => {
                                            "Deepgram did not accept the API key. Check it in Settings."
                                        }
                                        _ => "Deepgram cannot be reached. Check the network connection.",
                                    };
                                    self.end_conversation(Some(msg));
                                }
                            }
                        }
                    }
                    // Polled below on every pass: TLS may hold decrypted
                    // data the socket no longer signals.
                    SESSION => {}
                    // Cleared before housekeeping looks at what changed.
                    CHANGED => {
                        if let Some(e) = &self.shared.lock().changed {
                            let _ = e.clear();
                        }
                    }
                    VOICE => {}
                    k => {
                        let key = k - CONN;
                        if !self.serve(key)
                            || (it.observed & signals::PEER_CLOSED != 0 && it.observed & signals::READABLE == 0)
                        {
                            closed.push(key);
                        }
                    }
                }
            }
            backlog = self.session.is_some() && !self.poll_session();
            if let Some(l) = &mut self.listener
                && let Some(said) = l.poll(vrt::time::now_ns())
            {
                // What woke it is the first thing the user said.
                self.on_wake.push(said);
                self.wake("its name");
            }
            for k in closed {
                if let Some(c) = self.conns.remove(&k)
                    && c.who.name == "shell"
                    && c.who.service
                {
                    self.ui = None;
                }
            }
            // The microphone and the voice are serviced every round.
            self.mic();
            if let Some(v) = &mut self.voice {
                v.pump();
            }
            let now = vrt::time::now_ns();
            self.housekeeping(now);
            self.update_live(now);
        }
    }
}

/// What is on the screen, for the prompt.
fn screen_summary() -> String {
    use vproto::display::{WindowKind, display};
    let Some(d) = vproto::connect(display::NAME).ok().map(display::Client::new) else { return String::new() };
    let Ok(list) = d.list_windows() else { return String::new() };
    let wins: Vec<String> = list
        .iter()
        .filter(|w| matches!(w.kind, WindowKind::Normal | WindowKind::Borderless))
        .map(|w| if w.focused { format!("{} (in front)", w.title) } else { w.title.clone() })
        .collect();
    if wins.is_empty() { "No windows are open.".into() } else { format!("Open windows: {}.", wins.join(", ")) }
}

/// Requests from programs other than Settings.
struct Client<'a> {
    agent: &'a mut Agent,
    who: ClientIdentity,
}

impl Client<'_> {
    fn shell(&self) -> Result<(), AgentError> {
        if self.who.service && self.who.name == "shell" { Ok(()) } else { Err(AgentError::Denied) }
    }
}

impl agent::Server for Client<'_> {
    fn status(&mut self) -> AgentStatus {
        self.agent.status()
    }

    fn register_app(&mut self, app: Channel) -> Result<(), AgentError> {
        if self.who.name.is_empty() {
            return Err(AgentError::Denied);
        }
        self.agent.worker.submit(Job::App { name: self.who.name.clone(), channel: app });
        Ok(())
    }

    fn attach_ui(&mut self) -> Result<UiLink, AgentError> {
        self.shell()?;
        let (ui, link) = Ui::attach()?;
        self.agent.ui = Some(ui);
        println!("interface attached");
        self.agent.publish();
        // Approvals that were waiting when the shell (re)started.
        for a in &self.agent.approvals {
            let request = ApprovalRequest {
                id: a.id,
                app: a.pending.app.clone(),
                action: a.pending.action.clone(),
                detail: a.pending.detail.clone(),
                risk: a.pending.risk,
                allow_always: a.pending.risk == Risk::Sensitive,
            };
            if let Some(ui) = &self.agent.ui {
                ui.send(AgentEvent::Approval { request });
            }
        }
        Ok(link)
    }

    fn decide(&mut self, id: u64, allow: bool, always: bool) -> Result<(), AgentError> {
        self.shell()?;
        self.agent.decide(id, allow, always)
    }

    fn wake(&mut self) -> Result<(), AgentError> {
        self.shell()?;
        if self.agent.shared.lock().key.is_none() && self.agent.endpoint == deepgram::AGENT_URL {
            self.agent.rest();
            return Err(AgentError::NoKey);
        }
        self.agent.wake("asked");
        Ok(())
    }

    fn sleep(&mut self) -> Result<(), AgentError> {
        self.shell()?;
        self.agent.end_conversation(None);
        Ok(())
    }

    fn set_window_open(&mut self, open: bool) -> Result<(), AgentError> {
        self.shell()?;
        self.agent.window_open = open;
        Ok(())
    }

    fn set_muted(&mut self, muted: bool) -> Result<(), AgentError> {
        self.shell()?;
        self.agent.muted = muted;
        println!("microphone {}", if muted { "muted" } else { "on" });
        if let Some(v) = &mut self.agent.voice {
            if muted {
                v.close_mic();
            } else if self.agent.session.is_some() {
                v.open_mic();
            }
        }
        self.agent.update_listening();
        self.agent.publish();
        Ok(())
    }

    fn config(&mut self) -> Result<AgentConfig, AgentError> {
        Err(AgentError::Denied)
    }

    fn set_config(&mut self, _config: AgentConfig) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn set_api_key(&mut self, _key: String) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn check_key(&mut self) -> Result<String, AgentError> {
        Err(AgentError::Denied)
    }

    fn catalog(&mut self) -> Result<ModelCatalog, AgentError> {
        Err(AgentError::Denied)
    }

    fn preview_voice(&mut self, _voice: String) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn memories(&mut self) -> Result<Vec<MemoryItem>, AgentError> {
        Err(AgentError::Denied)
    }

    fn forget(&mut self, _id: u64) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn forget_all(&mut self) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }

    fn permissions(&mut self) -> Result<Vec<Permission>, AgentError> {
        Err(AgentError::Denied)
    }

    fn revoke(&mut self, _key: String) -> Result<(), AgentError> {
        Err(AgentError::Denied)
    }
}

fn main() -> i32 {
    // Options from the kernel command line (handed down by init):
    // `agent.endpoint=ws://10.0.2.2:5000/` talks to a local simulator.
    let mut endpoint = String::from(deepgram::AGENT_URL);
    let mut wake_at_start = false;
    let mut listen_endpoint = None;
    for a in vrt::env::args().iter().skip(1) {
        wake_at_start |= a == "agent.wake";
        if let Some(e) = a.strip_prefix("agent.listen=")
            && (e.starts_with("ws://") || e.starts_with("wss://"))
        {
            println!("using the speech recognition endpoint {}", e);
            listen_endpoint = Some(String::from(e));
        }
        if let Some(e) = a.strip_prefix("agent.endpoint=")
            && (e.starts_with("ws://") || e.starts_with("wss://"))
        {
            println!("using the Voice Agent endpoint {}", e);
            endpoint = e.into();
        }
    }
    let shared = Arc::new(Mutex::new(Shared::load()));
    {
        let s = shared.lock();
        println!(
            "{} is here ({}; thinks with {}, speaks as {})",
            s.config.name,
            if s.key.is_some() { "Deepgram key set" } else { "no Deepgram key yet" },
            s.config.think_model,
            s.config.voice
        );
    }
    let listener = match vproto::register(agent::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register: {:?}", e);
            return 1;
        }
    };
    let Some(worker) = Worker::start(shared.clone()) else {
        println!("cannot start the worker");
        return 1;
    };
    let voice = Voice::new();
    if voice.is_none() {
        println!("no audio service; the agent cannot speak or listen");
    }
    let published = Arc::new(Mutex::new(AgentStatus {
        state: AgentState::Off,
        name: String::new(),
        muted: false,
        configured: false,
        detail: String::new(),
        pending: 0,
    }));
    let mut agent = Agent {
        shared,
        published,
        state: AgentState::Off,
        detail: String::from("starting"),
        muted: false,
        window_open: false,
        conns: BTreeMap::new(),
        next_key: 0,
        ui: None,
        voice,
        worker,
        connector: None,
        session: None,
        approvals: Vec::new(),
        next_approval: 1,
        approved: BTreeMap::new(),
        calls_in_flight: 0,
        mic_buf: Vec::new(),
        playing_buf: Vec::new(),
        recorded_buf: Vec::new(),
        mic_pending: Vec::new(),
        playing_pending: Vec::new(),
        recorded_pending: Vec::new(),
        gate: EchoGate::new(),
        preroll: VecDeque::new(),
        on_wake: Vec::new(),
        seen_config: 0,
        seen_apps: 0,
        last_live_ns: 0,
        endpoint,
        wake_at_start,
        listener: None,
        listen_endpoint,
        listen_retry_at: 0,
    };
    agent.run(listener)
}
