//! The "Agent" page: the voice agent as an OS setting.
//!
//! * **Deepgram**: the API key (stored by the agent in its private
//!   directory; this page only ever sees its last characters) and a check
//!   that Deepgram accepts it.
//! * **Voice**: the agent's name, its voice (Deepgram's Aura-2 voices, with
//!   a spoken preview) and speaking rate.
//! * **Intelligence**: the language model it thinks with (Deepgram's
//!   catalogue, marked with Deepgram's price tier) and the speech
//!   recognition model.
//! * **Listening**: answering to its name, and how long a quiet
//!   conversation lasts.
//! * **Memory** and **Always allowed**: what the agent remembers about the
//!   user and the actions the user allowed for good — both can be removed.
//!
//! Requests go to the agent service from a background thread (checking the
//! key and fetching catalogues wait on the network), so the page never
//! freezes.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vmath::FloatExt;
use vproto::agent::{AgentConfig, AgentError, AgentStatus, MemoryItem, ModelCatalog, ModelChoice, Permission, agent};
use vrt::object::Event;
use vrt::sync::{Condvar, Mutex};
use vui::{Align, ButtonKind, Color, Font, Icon, Rect, Ui};

enum Request {
    Load,
    SetConfig(AgentConfig),
    SetKey(String),
    CheckKey,
    Catalog,
    Preview(String),
    ForgetOne(u64),
    ForgetAll,
    Revoke(String),
}

enum Reply {
    /// The agent service is not running.
    Unavailable,
    State {
        config: AgentConfig,
        status: AgentStatus,
        memories: Vec<MemoryItem>,
        permissions: Vec<Permission>,
    },
    ConfigSaved(Result<(), AgentError>),
    KeySaved(Result<(), AgentError>),
    KeyChecked(Result<String, AgentError>),
    Catalog(Result<ModelCatalog, AgentError>),
    Previewed(Result<(), AgentError>),
    Failed(AgentError),
}

struct Queue {
    requests: Mutex<VecDeque<Request>>,
    wake: Condvar,
    replies: Mutex<Vec<Reply>>,
}

/// Talks to the agent service from a background thread.
struct Link {
    q: Arc<Queue>,
    event: Event,
}

fn agent_running() -> bool {
    vproto::with_registry(|r| r.list().map(|l| l.iter().any(|n| n == agent::NAME)).unwrap_or(false)).unwrap_or(false)
}

impl Link {
    fn start() -> Option<Link> {
        let event = Event::create().ok()?;
        let signal = Event::from_handle(event.0.duplicate(None).ok()?);
        let q = Arc::new(Queue {
            requests: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            replies: Mutex::new(Vec::new()),
        });
        let wq = q.clone();
        vrt::thread::Builder::new().name("agent-link").spawn(move || worker(wq, signal)).ok()?;
        Some(Link { q, event })
    }

    fn send(&self, r: Request) {
        let mut q = self.q.requests.lock();
        // Refreshes pile up while a slow request (checking the key) runs.
        if matches!(r, Request::Load) && q.iter().any(|x| matches!(x, Request::Load)) {
            return;
        }
        q.push_back(r);
        drop(q);
        self.q.wake.notify_one();
    }

    fn take(&self) -> Vec<Reply> {
        let _ = self.event.clear();
        core::mem::take(&mut *self.q.replies.lock())
    }
}

fn worker(q: Arc<Queue>, event: Event) {
    let mut client: Option<agent::Client> = None;
    loop {
        let req = {
            let mut r = q.requests.lock();
            loop {
                if let Some(x) = r.pop_front() {
                    break x;
                }
                r = q.wake.wait(r);
            }
        };
        if client.is_none() && agent_running() {
            client = vproto::connect(agent::NAME).ok().map(agent::Client::new);
        }
        let answered = client.as_ref().is_some_and(|c| serve(c, req, &q));
        if !answered {
            client = None;
            q.replies.lock().push(Reply::Unavailable);
        }
        let _ = event.signal();
    }
}

/// Performs one request; false if the agent did not answer.
fn serve(c: &agent::Client, req: Request, q: &Queue) -> bool {
    let push = |r: Reply| q.replies.lock().push(r);
    let reload = match req {
        Request::Load => true,
        Request::SetConfig(cfg) => match c.set_config(cfg) {
            Ok(r) => {
                push(Reply::ConfigSaved(r));
                true
            }
            Err(_) => return false,
        },
        Request::SetKey(k) => match c.set_api_key(k) {
            Ok(r) => {
                push(Reply::KeySaved(r));
                true
            }
            Err(_) => return false,
        },
        Request::CheckKey => match c.check_key() {
            Ok(r) => {
                push(Reply::KeyChecked(r));
                false
            }
            Err(_) => return false,
        },
        Request::Catalog => match c.catalog() {
            Ok(r) => {
                push(Reply::Catalog(r));
                false
            }
            Err(_) => return false,
        },
        Request::Preview(v) => match c.preview_voice(v) {
            Ok(r) => {
                push(Reply::Previewed(r));
                false
            }
            Err(_) => return false,
        },
        Request::ForgetOne(id) => {
            if c.forget(id).is_err() {
                return false;
            }
            true
        }
        Request::ForgetAll => {
            if c.forget_all().is_err() {
                return false;
            }
            true
        }
        Request::Revoke(k) => {
            if c.revoke(k).is_err() {
                return false;
            }
            true
        }
    };
    if reload {
        let config = match c.config() {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                push(Reply::Failed(e));
                return true;
            }
            Err(_) => return false,
        };
        let Ok(status) = c.status() else { return false };
        let Ok(memories) = c.memories() else { return false };
        let Ok(permissions) = c.permissions() else { return false };
        push(Reply::State {
            config,
            status,
            memories: memories.unwrap_or_default(),
            permissions: permissions.unwrap_or_default(),
        });
    }
    true
}

/// Which list is unfolded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Voices,
    Models,
}

pub struct AgentPage {
    link: Option<Link>,
    available: bool,
    config: Option<AgentConfig>,
    status: Option<AgentStatus>,
    memories: Vec<MemoryItem>,
    permissions: Vec<Permission>,
    catalog: Option<ModelCatalog>,
    catalog_asked: bool,
    catalog_error: Option<String>,
    key: String,
    reveal: bool,
    /// The key field is shown (no key yet, or replacing it).
    editing_key: bool,
    /// The outcome of saving or checking the key: (good, message).
    key_message: Option<(bool, String)>,
    checking: bool,
    name: String,
    open: Option<Open>,
    previewing: Option<String>,
    confirm_forget: bool,
    error: Option<String>,
    loaded: bool,
    refresh_at: u64,
    /// A change not yet sent (sliders send once they settle), and when.
    dirty: Option<u64>,
    /// Changes sent and not yet confirmed: the agent's copy is older.
    saving: u32,
    /// Show the agent's name again (a name it refused).
    reset_name: bool,
    now: u64,
}

/// How long a setting must stay unchanged before it is saved.
const SETTLE_NS: u64 = 350_000_000;

fn explain(e: AgentError) -> String {
    match e {
        AgentError::BadKey => "Deepgram did not accept this key.".into(),
        AgentError::Network => "Deepgram could not be reached. Check the Internet connection.".into(),
        AgentError::NoKey => "Add a Deepgram API key first.".into(),
        AgentError::BadConfig => "That setting is not valid.".into(),
        e => format!("The agent refused: {e}."),
    }
}

impl AgentPage {
    pub fn new() -> AgentPage {
        let link = Link::start();
        if let Some(l) = &link {
            l.send(Request::Load);
        }
        AgentPage {
            link,
            available: true,
            config: None,
            status: None,
            memories: Vec::new(),
            permissions: Vec::new(),
            catalog: None,
            catalog_asked: false,
            catalog_error: None,
            key: String::new(),
            reveal: false,
            editing_key: false,
            key_message: None,
            checking: false,
            name: String::new(),
            open: None,
            previewing: None,
            confirm_forget: false,
            error: None,
            loaded: false,
            refresh_at: 0,
            dirty: None,
            saving: 0,
            reset_name: false,
            now: 0,
        }
    }

    /// Shows the agent's settings again (they were changed elsewhere).
    pub fn reload(&mut self) {
        self.reset_name = true;
        self.send(Request::Load);
    }

    pub fn wait_handle(&self) -> Option<vabi::RawHandle> {
        self.link.as_ref().map(|l| l.event.raw())
    }

    fn send(&self, r: Request) {
        if let Some(l) = &self.link {
            l.send(r);
        }
    }

    /// Takes the agent's answers; returns when to look again.
    pub fn poll(&mut self, now: u64) -> u64 {
        if let Some(l) = &self.link {
            for r in l.take() {
                match r {
                    Reply::Unavailable => {
                        self.available = false;
                        self.checking = false;
                        self.previewing = None;
                    }
                    Reply::State { config, status, memories, permissions } => {
                        self.available = true;
                        if !self.loaded || self.reset_name {
                            self.name = config.name.clone();
                            self.reset_name = false;
                        }
                        self.editing_key |= !config.has_key;
                        match &mut self.config {
                            // Keep what the user is changing.
                            Some(c) if self.dirty.is_some() || self.saving > 0 => {
                                c.has_key = config.has_key;
                                c.key_hint = config.key_hint;
                            }
                            c => *c = Some(config),
                        }
                        self.status = Some(status);
                        self.memories = memories;
                        self.permissions = permissions;
                        self.loaded = true;
                    }
                    Reply::ConfigSaved(r) => {
                        self.saving = self.saving.saturating_sub(1);
                        if let Err(e) = r {
                            self.error = Some(explain(e));
                            self.reset_name = true;
                        }
                    }
                    Reply::KeySaved(r) => match r {
                        Ok(()) => {
                            self.key.clear();
                            self.editing_key = false;
                            self.key_message = Some((true, "Key saved. Checking it with Deepgram\u{2026}".into()));
                            self.checking = true;
                            self.send(Request::CheckKey);
                        }
                        Err(AgentError::BadConfig) => {
                            self.key_message = Some((false, "That does not look like a Deepgram API key.".into()))
                        }
                        Err(e) => self.key_message = Some((false, explain(e))),
                    },
                    Reply::KeyChecked(r) => {
                        self.checking = false;
                        self.key_message = Some(match r {
                            Ok(msg) => (true, msg),
                            Err(e) => (false, explain(e)),
                        });
                        if self.catalog.is_none() {
                            self.catalog_asked = false;
                        }
                    }
                    Reply::Catalog(r) => match r {
                        Ok(c) => {
                            self.catalog = Some(c);
                            self.catalog_error = None;
                        }
                        Err(e) => self.catalog_error = Some(explain(e)),
                    },
                    Reply::Previewed(r) => {
                        self.previewing = None;
                        if let Err(e) = r {
                            self.error = Some(explain(e));
                        }
                    }
                    Reply::Failed(e) => self.error = Some(explain(e)),
                }
            }
        }
        self.now = now;
        if let Some(at) = self.dirty
            && now >= at + SETTLE_NS
        {
            self.dirty = None;
            if let Some(c) = self.config.clone() {
                self.saving += 1;
                self.send(Request::SetConfig(c));
            }
        }
        // The agent's state changes while the page is open.
        if now >= self.refresh_at {
            self.refresh_at = now + 3_000_000_000;
            if self.loaded || !self.available {
                self.send(Request::Load);
            }
        }
        match self.dirty {
            Some(at) => self.refresh_at.min(at + SETTLE_NS),
            None => self.refresh_at,
        }
    }

    /// Changes a setting; it is saved once it has settled.
    fn change(&mut self, f: impl FnOnce(&mut AgentConfig)) {
        if let Some(c) = &mut self.config {
            f(c);
            self.dirty = Some(self.now);
        }
    }

    pub fn draw(&mut self, ui: &mut Ui, r: Rect) -> i32 {
        let t = ui.theme().clone();
        self.now = ui.now();
        ui.label(Rect::new(r.x, r.y, r.w, 36), "Agent", Font::Bold, t.title_size, t.text, Align::Left);
        let summary = if !self.available {
            String::from("The agent is not running.")
        } else {
            match (&self.status, &self.config) {
                (Some(_), Some(c)) if !c.enabled => format!("{} is switched off.", c.name),
                (Some(_), Some(c)) if !c.has_key => format!("{} needs a Deepgram API key to talk.", c.name),
                (Some(s), _) if !s.detail.is_empty() && matches!(s.state, vproto::agent::AgentState::Error) => {
                    s.detail.clone()
                }
                (Some(_), Some(c)) if c.listen_for_name => {
                    format!(
                        "Say \u{201c}{}\u{201d}, click the circle on the taskbar or press Win+Space to talk.",
                        c.name
                    )
                }
                (Some(_), _) => String::from("Click the circle on the taskbar or press Win+Space to talk."),
                _ => String::from("Loading\u{2026}"),
            }
        };
        ui.label(Rect::new(r.x, r.y + 38, r.w, 22), &summary, Font::Regular, t.font_size, t.text_dim, Align::Left);
        let mut y = r.y + 78;
        let Some(c) = self.config.clone() else {
            return y - r.y;
        };
        // The agent itself, on or off.
        let mut on = c.enabled;
        let sw = Rect::new(r.right() - 46, r.y + 8, 44, 22);
        ui.label(
            Rect::new(sw.x - 60, r.y + 2, 50, 34),
            if on { "On" } else { "Off" },
            Font::Regular,
            t.font_size,
            t.text_dim,
            Align::Right,
        );
        if ui.toggle(sw, "agent-enabled", &mut on) {
            self.change(|c| c.enabled = on);
        }
        y = self.deepgram_card(ui, r.x, y, r.w);
        y = self.voice_card(ui, r.x, y, r.w);
        y = self.intelligence_card(ui, r.x, y, r.w);
        y = self.listening_card(ui, r.x, y, r.w);
        y = self.memory_card(ui, r.x, y, r.w);
        if !self.permissions.is_empty() {
            y = self.permissions_card(ui, r.x, y, r.w);
        }
        y - r.y
    }

    /// The page's dialogs (drawn after the scrolled content).
    pub fn overlay(&mut self, ui: &mut Ui) {
        if let Some(msg) = self.error.clone() {
            if ui.message_box("Agent", &msg, &["OK"]).is_some() {
                self.error = None;
            }
            return;
        }
        if self.confirm_forget {
            match ui.message_box(
                "Forget everything?",
                "The agent will forget everything it has learned about you, its record of your habits and your recent conversations.",
                &["Forget everything", "Cancel"],
            ) {
                Some(0) => {
                    self.send(Request::ForgetAll);
                    self.confirm_forget = false;
                }
                Some(_) => self.confirm_forget = false,
                None => {}
            }
        }
    }

    fn card_title(ui: &mut Ui, card: Rect, icon: Icon, title: &str, subtitle: &str) {
        let t = ui.theme().clone();
        ui.icon(Rect::new(card.x + 22, card.y + 14, 28, 44), icon, 22.0, t.accent);
        ui.label(
            Rect::new(card.x + 62, card.y + 14, card.w - 84, 22),
            title,
            Font::Bold,
            t.font_size,
            t.text,
            Align::Left,
        );
        ui.label(
            Rect::new(card.x + 62, card.y + 36, card.w - 84, 20),
            subtitle,
            Font::Regular,
            t.small_size,
            t.text_dim,
            Align::Left,
        );
    }

    fn deepgram_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let c = self.config.clone().unwrap_or_else(default_config);
        let h = 74 + 56 + if self.key_message.is_some() { 26 } else { 0 };
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        Self::card_title(
            ui,
            card,
            Icon::Lock,
            "Deepgram",
            "The agent listens, thinks and speaks through Deepgram, with your API key.",
        );
        let row = card.y + 74;
        let ix = card.x + 62;
        if self.editing_key {
            let field = Rect::new(ix, row, (w - 62 - 22 - 220).min(420), 34);
            let resp = ui.password_input(field, "agent-key", &mut self.key, "Deepgram API key", self.reveal);
            if ui.button(Rect::new(field.right() + 8, row, 60, 34), if self.reveal { "Hide" } else { "Show" }) {
                self.reveal = !self.reveal;
                ui.repaint();
            }
            let save = ui.button_full(Rect::new(field.right() + 76, row, 90, 34), None, "Save", ButtonKind::Primary);
            if (save || resp.submitted) && !self.key.trim().is_empty() {
                let k = self.key.trim().to_string();
                self.send(Request::SetKey(k));
                self.key_message = Some((true, "Saving\u{2026}".into()));
            }
            if c.has_key && ui.button(Rect::new(field.right() + 174, row, 80, 34), "Cancel") {
                self.editing_key = false;
                self.key.clear();
            }
        } else {
            let text = format!("Key saved ({})", c.key_hint);
            ui.label(Rect::new(ix, row, 260, 34), &text, Font::Regular, t.font_size, t.text, Align::Left);
            let bx = card.right() - 22 - 3 * 98;
            if ui.button(Rect::new(bx, row, 90, 34), if self.checking { "Checking\u{2026}" } else { "Check" })
                && !self.checking
            {
                self.checking = true;
                self.key_message = None;
                self.send(Request::CheckKey);
            }
            if ui.button(Rect::new(bx + 98, row, 90, 34), "Replace") {
                self.editing_key = true;
                self.key_message = None;
            }
            if ui.button_full(Rect::new(bx + 196, row, 90, 34), None, "Remove", ButtonKind::Danger) {
                self.send(Request::SetKey(String::new()));
                self.key_message = None;
            }
        }
        if let Some((good, msg)) = &self.key_message {
            let color = if *good { t.text_dim } else { Color::rgba(255, 140, 120, 255) };
            ui.label(Rect::new(ix, row + 40, w - 84, 22), msg, Font::Regular, t.small_size, color, Align::Left);
        }
        y + h + 16
    }

    fn voice_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let c = self.config.clone().unwrap_or_else(default_config);
        let voices: Vec<ModelChoice> = self.catalog.as_ref().map(|k| k.voices.clone()).unwrap_or_default();
        let list_h = if self.open == Some(Open::Voices) { (voices.len().max(1) as i32) * 40 + 8 } else { 0 };
        let h = 74 + 3 * 46 + list_h + 8;
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        Self::card_title(ui, card, Icon::Speaker, "Voice", "What the agent is called, and how it sounds.");
        let ix = card.x + 62;
        let iw = w - 62 - 22;
        let mut row = card.y + 74;
        ui.label(Rect::new(ix, row, 140, 34), "Name", Font::Regular, t.font_size, t.text, Align::Left);
        let field = Rect::new(ix + 150, row, 220, 34);
        let resp = ui.text_input(field, "agent-name", &mut self.name, "Vera");
        let trimmed = self.name.trim().to_string();
        if (resp.submitted || (!resp.focused && trimmed != c.name))
            && !trimmed.is_empty()
            && trimmed.chars().count() <= 24
            && trimmed != c.name
        {
            self.change(|c| c.name = trimmed.clone());
        }
        row += 46;
        ui.label(Rect::new(ix, row, 140, 34), "Voice", Font::Regular, t.font_size, t.text, Align::Left);
        let current = voices.iter().find(|v| v.id == c.voice);
        let label = match current {
            Some(v) => format!("{} \u{2014} {}", v.name, v.detail),
            None => voice_name(&c.voice),
        };
        let font = ui.ctx.font(Font::Regular);
        let label = ui.ctx.text.ellipsize(font, t.font_size, &label, (iw - 150 - 210) as f32);
        ui.label(
            Rect::new(ix + 150, row, iw - 150 - 210, 34),
            &label,
            Font::Regular,
            t.font_size,
            t.text_dim,
            Align::Left,
        );
        let busy = self.previewing.is_some();
        if ui.button(Rect::new(card.right() - 22 - 200, row, 96, 34), if busy { "Playing\u{2026}" } else { "Listen" })
            && !busy
        {
            self.previewing = Some(c.voice.clone());
            self.send(Request::Preview(c.voice.clone()));
        }
        if ui.button(
            Rect::new(card.right() - 22 - 96, row, 96, 34),
            if self.open == Some(Open::Voices) { "Done" } else { "Change" },
        ) {
            self.open = if self.open == Some(Open::Voices) { None } else { Some(Open::Voices) };
            self.want_catalog();
            ui.repaint();
        }
        row += 46;
        if self.open == Some(Open::Voices) {
            if voices.is_empty() {
                let msg = self.catalog_error.clone().unwrap_or_else(|| "Loading the voices\u{2026}".into());
                ui.label(Rect::new(ix, row, iw, 32), &msg, Font::Regular, t.small_size, t.text_dim, Align::Left);
            }
            for v in &voices {
                let rr = Rect::new(ix, row, iw, 38);
                let resp = ui.interact(ui.id(&format!("voice-{}", v.id)), rr);
                ui.row_background(
                    rr,
                    vui::RowState { selected: v.id == c.voice, hovered: resp.hovered, focused: false },
                );
                ui.label(Rect::new(rr.x + 12, rr.y, 130, 38), &v.name, Font::Bold, t.font_size, t.text, Align::Left);
                let detail = ui.ctx.text.ellipsize(font, t.small_size, &v.detail, (rr.w - 160 - 50) as f32);
                ui.label(
                    Rect::new(rr.x + 150, rr.y, rr.w - 200, 38),
                    &detail,
                    Font::Regular,
                    t.small_size,
                    t.text_dim,
                    Align::Left,
                );
                if ui.icon_button(Rect::new(rr.right() - 40, rr.y + 2, 34, 34), Icon::Play, "Listen")
                    && self.previewing.is_none()
                {
                    self.previewing = Some(v.id.clone());
                    self.send(Request::Preview(v.id.clone()));
                } else if resp.clicked && v.id != c.voice {
                    let id = v.id.clone();
                    self.change(|c| c.voice = id);
                }
                row += 40;
            }
            row += 8;
        }
        ui.label(Rect::new(ix, row, 140, 34), "Speaking rate", Font::Regular, t.font_size, t.text, Align::Left);
        let mut speed = c.speed;
        if ui.slider(Rect::new(ix + 150, row, (iw - 150 - 70).min(320), 34), "agent-speed", &mut speed, 0.7, 1.5) {
            let s = (speed * 20.0).round() / 20.0;
            if (s - c.speed).abs() > 0.001 {
                self.change(|c| c.speed = s);
            }
        }
        ui.label(
            Rect::new(ix + 150 + (iw - 150 - 70).min(320) + 10, row, 60, 34),
            &format!("{:.2}\u{00d7}", c.speed),
            Font::Regular,
            t.small_size,
            t.text_dim,
            Align::Left,
        );
        y + h + 16
    }

    fn intelligence_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let c = self.config.clone().unwrap_or_else(default_config);
        let models: Vec<ModelChoice> = self.catalog.as_ref().map(|k| k.think.clone()).unwrap_or_default();
        let listen: Vec<ModelChoice> = self.catalog.as_ref().map(|k| k.listen.clone()).unwrap_or_else(default_listen);
        let list_h = if self.open == Some(Open::Models) { (models.len().max(1) as i32) * 36 + 44 } else { 0 };
        let h = 74 + 46 + list_h + 30 + listen.len() as i32 * 38 + 14;
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        Self::card_title(
            ui,
            card,
            Icon::Cpu,
            "Intelligence",
            "The language model the agent thinks with, and how it understands speech.",
        );
        let ix = card.x + 62;
        let iw = w - 62 - 22;
        let mut row = card.y + 74;
        ui.label(Rect::new(ix, row, 140, 34), "Language model", Font::Regular, t.font_size, t.text, Align::Left);
        let current = models.iter().find(|m| m.id == c.think_model && m.provider == c.think_provider);
        let label = match current {
            Some(m) if !m.detail.is_empty() => format!("{} \u{00b7} {} tier", m.name, m.detail),
            Some(m) => m.name.clone(),
            None => c.think_model.clone(),
        };
        ui.label(
            Rect::new(ix + 150, row, iw - 150 - 110, 34),
            &label,
            Font::Regular,
            t.font_size,
            t.text_dim,
            Align::Left,
        );
        if ui.button(
            Rect::new(card.right() - 22 - 96, row, 96, 34),
            if self.open == Some(Open::Models) { "Done" } else { "Change" },
        ) {
            self.open = if self.open == Some(Open::Models) { None } else { Some(Open::Models) };
            self.want_catalog();
            ui.repaint();
        }
        row += 46;
        if self.open == Some(Open::Models) {
            ui.paragraph(
                Rect::new(ix, row, iw, 36),
                "Standard models cost less per minute of conversation; Advanced models are larger.",
                t.small_size,
                t.text_dim,
            );
            row += 36;
            if models.is_empty() {
                let msg = self.catalog_error.clone().unwrap_or_else(|| "Loading the models\u{2026}".into());
                ui.label(Rect::new(ix, row, iw, 32), &msg, Font::Regular, t.small_size, t.text_dim, Align::Left);
            }
            let mut sorted = models.clone();
            sorted.sort_by_key(|m| (m.detail != "Standard", m.provider.clone(), m.name.clone()));
            for m in &sorted {
                let rr = Rect::new(ix, row, iw, 34);
                let resp = ui.interact(ui.id(&format!("model-{}-{}", m.provider, m.id)), rr);
                let selected = m.id == c.think_model && m.provider == c.think_provider;
                ui.row_background(rr, vui::RowState { selected, hovered: resp.hovered, focused: false });
                ui.label(
                    Rect::new(rr.x + 12, rr.y, rr.w - 200, 34),
                    &m.name,
                    Font::Regular,
                    t.font_size,
                    t.text,
                    Align::Left,
                );
                let tier = if m.detail.is_empty() {
                    provider_name(&m.provider).to_string()
                } else {
                    format!("{} \u{00b7} {}", provider_name(&m.provider), m.detail)
                };
                ui.label(
                    Rect::new(rr.right() - 200, rr.y, 188, 34),
                    &tier,
                    Font::Regular,
                    t.small_size,
                    t.text_dim,
                    Align::Right,
                );
                if resp.clicked && !selected {
                    let (id, p) = (m.id.clone(), m.provider.clone());
                    self.change(|c| {
                        c.think_model = id;
                        c.think_provider = p;
                    });
                }
                row += 36;
            }
            row += 8;
        }
        ui.label(Rect::new(ix, row, iw, 26), "Speech recognition", Font::Regular, t.font_size, t.text, Align::Left);
        row += 30;
        for l in &listen {
            let rr = Rect::new(ix, row, iw, 34);
            let resp = ui.interact(ui.id(&format!("listen-{}", l.id)), rr);
            let selected = l.id == c.listen_model;
            ui.row_background(rr, vui::RowState { selected, hovered: resp.hovered, focused: false });
            ui.label(Rect::new(rr.x + 12, rr.y, 160, 34), &l.name, Font::Regular, t.font_size, t.text, Align::Left);
            ui.label(
                Rect::new(rr.x + 170, rr.y, rr.w - 180, 34),
                &l.detail,
                Font::Regular,
                t.small_size,
                t.text_dim,
                Align::Left,
            );
            if resp.clicked && !selected {
                let id = l.id.clone();
                self.change(|c| c.listen_model = id);
            }
            row += 38;
        }
        y + h + 16
    }

    fn listening_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let c = self.config.clone().unwrap_or_else(default_config);
        let h = 74 + 46 + 46 + 40;
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        Self::card_title(ui, card, Icon::Mic, "Listening", "When the agent listens, and for how long.");
        let ix = card.x + 62;
        let iw = w - 62 - 22;
        let mut row = card.y + 74;
        ui.label(
            Rect::new(ix, row, iw - 60, 34),
            &format!("Answer when someone says \u{201c}{}\u{201d}", c.name),
            Font::Regular,
            t.font_size,
            t.text,
            Align::Left,
        );
        let mut on = c.listen_for_name;
        if ui.toggle(Rect::new(card.right() - 66, row + 6, 44, 22), "agent-listen-name", &mut on) {
            self.change(|c| c.listen_for_name = on);
        }
        row += 46;
        ui.label(
            Rect::new(ix, row, 260, 34),
            "End a quiet conversation after",
            Font::Regular,
            t.font_size,
            t.text,
            Align::Left,
        );
        let mut secs = c.idle_timeout_s as f32;
        let sw = (iw - 270 - 70).clamp(120, 300);
        if ui.slider(Rect::new(ix + 270, row, sw, 34), "agent-idle", &mut secs, 10.0, 300.0) {
            let s = ((secs / 5.0).round() * 5.0) as u32;
            if s != c.idle_timeout_s {
                self.change(|c| c.idle_timeout_s = s);
            }
        }
        ui.label(
            Rect::new(ix + 270 + sw + 10, row, 70, 34),
            &format!("{} s", c.idle_timeout_s),
            Font::Regular,
            t.small_size,
            t.text_dim,
            Align::Left,
        );
        row += 46;
        ui.paragraph(
            Rect::new(ix, row, iw, 36),
            "While waiting for its name, speech near the computer is sent to Deepgram to be recognised. Conversations are billed by Deepgram per minute.",
            t.small_size,
            t.text_faint,
        );
        y + h + 16
    }

    fn memory_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let n = self.memories.len() as i32;
        let h = 74 + if n == 0 { 40 } else { n.min(200) * 40 + 54 };
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        let name = self.config.as_ref().map_or("The agent", |c| c.name.as_str());
        Self::card_title(
            ui,
            card,
            Icon::Heart,
            "Memory",
            &format!("What {name} remembers about you. Remove anything you like."),
        );
        let ix = card.x + 62;
        let iw = w - 62 - 22;
        let mut row = card.y + 74;
        if n == 0 {
            ui.label(Rect::new(ix, row, iw, 30), "Nothing yet.", Font::Regular, t.font_size, t.text_dim, Align::Left);
            return y + h + 16;
        }
        let mut forget = None;
        let font = ui.ctx.font(Font::Regular);
        for m in self.memories.iter().take(200) {
            let rr = Rect::new(ix, row, iw, 36);
            let text = ui.ctx.text.ellipsize(font, t.font_size, &m.text, (rr.w - 60) as f32);
            ui.label(Rect::new(rr.x + 4, rr.y, rr.w - 50, 36), &text, Font::Regular, t.font_size, t.text, Align::Left);
            if ui.icon_button(Rect::new(rr.right() - 38, rr.y + 1, 34, 34), Icon::Close, "Forget this") {
                forget = Some(m.id);
            }
            row += 40;
        }
        if let Some(id) = forget {
            self.send(Request::ForgetOne(id));
            self.memories.retain(|m| m.id != id);
            ui.repaint();
        }
        if ui.button_full(Rect::new(ix, row + 8, 180, 34), None, "Forget everything", ButtonKind::Danger) {
            self.confirm_forget = true;
            ui.repaint();
        }
        y + h + 16
    }

    fn permissions_card(&mut self, ui: &mut Ui, x: i32, y: i32, w: i32) -> i32 {
        let t = ui.theme().clone();
        let h = 74 + self.permissions.len() as i32 * 40 + 8;
        let card = Rect::new(x, y, w, h);
        ui.card(card);
        Self::card_title(ui, card, Icon::Check, "Always allowed", "Actions the agent may do without asking each time.");
        let ix = card.x + 62;
        let iw = w - 62 - 22;
        let mut row = card.y + 74;
        let mut revoke = None;
        for p in &self.permissions {
            ui.label(Rect::new(ix + 4, row, iw - 120, 36), &p.label, Font::Regular, t.font_size, t.text, Align::Left);
            if ui.button(Rect::new(card.right() - 22 - 100, row + 1, 100, 34), "Ask again") {
                revoke = Some(p.key.clone());
            }
            row += 40;
        }
        if let Some(k) = revoke {
            self.permissions.retain(|p| p.key != k);
            self.send(Request::Revoke(k));
            ui.repaint();
        }
        y + h + 16
    }

    fn want_catalog(&mut self) {
        if !self.catalog_asked && self.catalog.is_none() {
            self.catalog_asked = true;
            self.catalog_error = None;
            self.send(Request::Catalog);
        }
    }
}

fn default_config() -> AgentConfig {
    AgentConfig {
        enabled: true,
        name: "Vera".into(),
        listen_model: "flux-general-en".into(),
        think_provider: "open_ai".into(),
        think_model: "gpt-4.1-mini".into(),
        voice: "aura-2-helena-en".into(),
        speed: 1.0,
        listen_for_name: true,
        idle_timeout_s: 40,
        has_key: false,
        key_hint: String::new(),
    }
}

fn default_listen() -> Vec<ModelChoice> {
    [
        ("flux-general-en", "Flux", "English, knows when you have finished speaking"),
        ("nova-3", "Nova-3", "English, general recognition"),
    ]
    .iter()
    .map(|(id, name, d)| ModelChoice {
        id: (*id).into(),
        name: (*name).into(),
        provider: String::new(),
        detail: (*d).into(),
    })
    .collect()
}

fn provider_name(p: &str) -> &str {
    match p {
        "open_ai" => "OpenAI",
        "anthropic" => "Anthropic",
        "google" => "Google",
        "nvidia" => "NVIDIA",
        other => other,
    }
}

/// "aura-2-helena-en" → "Helena".
fn voice_name(id: &str) -> String {
    let core = id.trim_start_matches("aura-2-").trim_end_matches("-en");
    let mut out = String::new();
    let mut chars = core.chars();
    if let Some(c) = chars.next() {
        out.extend(c.to_uppercase());
        out.push_str(chars.as_str());
    }
    out
}
