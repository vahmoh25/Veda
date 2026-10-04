//! The voice agent's presence on the desktop.
//!
//! * [`AgentLink`]: the shell's connection to the agent service — the
//!   interface link (status and approval events, the shared page of live
//!   voice levels) and the requests only the shell may make (wake, sleep,
//!   decide on approvals, mute).
//! * [`AgentWindow`]: the agent's window, deliberately minimal: the
//!   taskbar's own background and a white ring, OS1's from "Her" (see
//!   [`crate::presence`]). The ring breathes while the agent listens,
//!   trembles and glows with its voice while it speaks, and a light runs
//!   around it while it thinks; waking up, it is the film's coil, which
//!   turns to face the user and becomes the ring when the agent is ready.
//!   A line of small text says what is going on only when that helps. When
//!   the agent needs consent for an action, the request appears at the
//!   bottom of the window (or, when the window is closed, as a
//!   notification).
//! * [`draw_orb`]: the agent's tray item, a small ring that does the same.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vmath::FloatExt;
use vproto::agent::{AGENT_EVENT, AgentEvent, AgentState, AgentStatus, ApprovalRequest, Live, agent};
use vrt::object::Channel;
use vrt::vm::Mapping;
use vui::{Font, Icon, Ui};

use crate::presence::{self, RING_WIDTH};
use crate::{Action, Model, chrome, connect_running, taskbar};

/// The window's size.
pub const WIDTH: i32 = 340;
pub const HEIGHT: i32 = 420;
/// The taskbar's tint (see `taskbar::Taskbar::update`).
const TINT: Color = Color::rgba(20, 20, 28, 200);
/// How long the coil takes to become the ring once the agent is ready,
/// and the ring the coil when it wakes.
const RESOLVE_NS: u64 = 1_200_000_000;
const DISSOLVE_NS: u64 = 700_000_000;
/// How fast the coil spins (radians a second), and how much more it turns
/// as it becomes the ring.
const SPIN: f32 = 2.1;
const SPIN_EXTRA: f32 = 9.0;

/// Where the window appears: above the tray, at the right.
pub fn placement(screen: Rect) -> Rect {
    Rect::new(screen.w - WIDTH - 12, screen.h - taskbar::HEIGHT - 12 - HEIGHT, WIDTH, HEIGHT)
}

/// The shell's link to the agent service.
pub struct AgentLink {
    client: agent::Client,
    events: Channel,
    live: Mapping,
}

impl AgentLink {
    /// Attaches the agent's interface, if the agent service runs.
    pub fn connect() -> Option<AgentLink> {
        let client = agent::Client::new(connect_running(agent::NAME)?);
        client.set_timeout(2_000_000_000);
        let link = client.attach_ui().ok()?.ok()?;
        let live = Mapping::new(link.live, 4096, vabi::map_flags::READ).ok()?;
        Some(AgentLink { client, events: link.events, live })
    }

    pub fn events_handle(&self) -> vabi::RawHandle {
        self.events.raw()
    }

    /// Events that arrived; `None` once the agent service has gone away.
    pub fn take_events(&self) -> Option<Vec<AgentEvent>> {
        let mut out = Vec::new();
        loop {
            match self.events.read() {
                Ok(msg) => {
                    if let Ok((AGENT_EVENT, ev)) = vipc::decode_event::<AgentEvent>(msg) {
                        out.push(ev);
                    }
                }
                Err(vabi::Error::ShouldWait) => return Some(out),
                Err(_) => return None,
            }
        }
    }

    /// The voice levels right now: (agent's voice, microphone).
    pub fn levels(&self) -> (f32, f32) {
        // SAFETY: a page shared with the agent service holding `Live`
        // (atomics, valid for any bit pattern), mapped read-only.
        let live = unsafe { &*(self.live.as_ptr() as *const Live) };
        live.levels()
    }

    pub fn wake(&self) {
        let _ = self.client.wake();
    }

    pub fn sleep(&self) {
        let _ = self.client.sleep();
    }

    pub fn decide(&self, id: u64, allow: bool, always: bool) {
        let _ = self.client.decide(id, allow, always);
    }

    pub fn set_window_open(&self, open: bool) {
        let _ = self.client.set_window_open(open);
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.client.set_muted(muted);
    }
}

/// The agent's state as the shell knows it.
#[derive(Default)]
pub struct AgentModel {
    pub status: Option<AgentStatus>,
    pub approvals: Vec<ApprovalRequest>,
    /// Voice levels (agent, microphone), smoothed for drawing.
    pub levels: (f32, f32),
    pub window_open: bool,
    /// When the state last changed, and how much of a ring the figure was
    /// then (see [`AgentModel::figure`]).
    since_ns: u64,
    figure_from: f32,
}

/// Smooth from 0 to 1.
fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

impl AgentModel {
    pub fn state(&self) -> Option<AgentState> {
        self.status.as_ref().map(|s| s.state)
    }

    /// Takes a new status (noting when the state changed, for the figure).
    pub fn set_status(&mut self, status: AgentStatus, now: u64) {
        if self.state() != Some(status.state) {
            self.figure_from = self.figure(now);
            self.since_ns = now;
        }
        self.status = Some(status);
    }

    /// How much of a ring the agent's figure is at `now`: 0 is the coil
    /// (waking up), 1 the ring; in between it turns from one to the other.
    pub fn figure(&self, now: u64) -> f32 {
        let (target, duration) =
            if self.state() == Some(AgentState::Waking) { (0.0, DISSOLVE_NS) } else { (1.0, RESOLVE_NS) };
        if self.since_ns == 0 {
            return target;
        }
        let k = now.saturating_sub(self.since_ns) as f32 / duration as f32;
        self.figure_from + (target - self.figure_from) * ease(k)
    }

    /// In a conversation (or starting one).
    pub fn active(&self) -> bool {
        matches!(
            self.state(),
            Some(AgentState::Waking | AgentState::Listening | AgentState::Thinking | AgentState::Speaking)
        )
    }

    pub fn name(&self) -> &str {
        self.status.as_ref().map_or("Agent", |s| s.name.as_str())
    }

    /// One short line about the state, for the tray's tooltip and the
    /// window's caption.
    pub fn caption(&self) -> String {
        let Some(s) = &self.status else { return "The agent is not running".into() };
        if s.muted && !matches!(s.state, AgentState::Off | AgentState::Error) {
            return "Microphone off".into();
        }
        match s.state {
            AgentState::Off | AgentState::Error => s.detail.clone(),
            AgentState::Asleep => format!("Say \u{201c}{}\u{201d} or tap the ring", s.name),
            AgentState::Waking => "One moment\u{2026}".into(),
            AgentState::Listening => "Listening".into(),
            AgentState::Thinking => "Thinking\u{2026}".into(),
            AgentState::Speaking => String::new(),
        }
    }
}

/// Draws the agent's figure for its state around (`cx`, `cy`): a ring of
/// radius `base` (the coil while waking up). `now` is the time in
/// nanoseconds; small figures (the tray) are simpler.
pub fn draw_presence(ui: &mut Ui, cx: f32, cy: f32, base: f32, a: &AgentModel, now: u64) {
    let (voice, mic) = a.levels;
    let state = a.state().unwrap_or(AgentState::Off);
    // Seconds, wrapped every hour (an f32 keeps them precise).
    let t = (now % 3_600_000_000_000) as f32 / 1e9;
    let small = base < 16.0;
    let width = (RING_WIDTH * base).max(2.2);
    let white = |alpha: f32| Color::rgba(255, 255, 255, (alpha.clamp(0.0, 1.0) * 255.0) as u8);
    let c = &mut ui.canvas;
    let v = a.figure(now);
    if v < 1.0 {
        // Turning between the coil and the ring.
        let ring_alpha = ease((v - 0.85) / 0.15);
        let coil_alpha = 1.0 - ease((v - 0.8) / 0.12);
        if coil_alpha > 0.0 {
            let spin = SPIN * t + SPIN_EXTRA * v * v * v;
            let turn = ease((v - 0.25) / 0.75);
            presence::coil(c, cx, cy, presence::coil_scale(base), spin, turn, coil_alpha, small);
        }
        if ring_alpha > 0.0 {
            let r = base * (0.9 + 0.1 * ring_alpha);
            presence::ring(c, cx, cy, r, width, 0.0, t, white(ring_alpha));
        }
        return;
    }
    match state {
        AgentState::Speaking => {
            // A soft glow that swells with the voice; the ring itself grows
            // bolder with it and trembles a little.
            let w = width * (1.0 + 0.35 * voice);
            presence::glow(c, cx, cy, base, w, base * (0.18 + 0.12 * voice), 0.16 + 0.3 * voice);
            presence::ring(c, cx, cy, base, w, 0.1 + 0.5 * voice, t, white(1.0));
        }
        AgentState::Listening => {
            let breath = 0.5 + 0.5 * (t * 1.6).sin();
            let r = base * (0.98 + 0.02 * breath + 0.04 * mic);
            presence::glow(c, cx, cy, r, width, base * 0.16, 0.06 + 0.08 * breath + 0.25 * mic);
            presence::ring(c, cx, cy, r, width, 0.3 * mic, t * 0.6, white(0.98));
        }
        AgentState::Thinking => presence::ring_with_light(c, cx, cy, base, width, t * 3.0, 150, 255),
        AgentState::Waking => presence::ring(c, cx, cy, base, width, 0.0, t, white(0.9)),
        AgentState::Asleep => presence::ring(c, cx, cy, base * 0.94, width, 0.0, t, white(0.6)),
        AgentState::Off | AgentState::Error => {
            presence::ring(c, cx, cy, base * 0.9, (0.06 * base).max(1.5), 0.0, t, white(0.43));
        }
    }
}

/// The agent's tray item: a small ring that echoes the big one.
pub fn draw_orb(ui: &mut Ui, r: Rect, a: &AgentModel, now: u64) {
    let (cx, cy) = (r.center().0 as f32, r.center().1 as f32);
    draw_presence(ui, cx, cy, 7.0, a, now);
    if a.status.as_ref().is_some_and(|s| s.muted) {
        ui.canvas.fill_rect(Rect::new(r.center().0 - 9, r.center().1, 18, 2), Color::rgba(255, 120, 110, 230));
    }
    if !a.approvals.is_empty() {
        let t = ui.theme().clone();
        ui.canvas.fill_circle(cx + 8.0, cy - 8.0, 4.0, t.accent);
    }
}

/// The agent's window.
pub struct AgentWindow {
    /// "Always allow" ticked on the approval being shown.
    always: bool,
}

/// What an approval asks for, in bold over up to two lines (the end of a
/// longer one is cut); returns the height used. `r` is the first line.
pub fn approval_title(ui: &mut Ui, r: Rect, text: &str) -> i32 {
    let th = ui.theme().clone();
    let font = ui.ctx.font(Font::Bold);
    let lines = ui.ctx.text.wrap(font, 14.0, text, r.w as f32);
    if lines.len() <= 1 {
        ui.label(r, text, Font::Bold, 14.0, th.text, Align::Left);
        return r.h;
    }
    let first = text[lines[0].clone()].trim_end();
    let rest = text[lines[1].start..].trim();
    let second = ui.ctx.text.ellipsize(font, 14.0, rest, r.w as f32);
    ui.label(r, first, Font::Bold, 14.0, th.text, Align::Left);
    ui.label(r.translate(0, 18), &second, Font::Bold, 14.0, th.text, Align::Left);
    r.h + 18
}

impl AgentWindow {
    pub fn new() -> AgentWindow {
        AgentWindow { always: false }
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let th = ui.theme().clone();
        let (w, h) = (ui.width, ui.height);
        // The taskbar's own colour (what the bar looks like where it is),
        // not the wallpaper behind the window.
        let bar = Rect::new(0, m.screen.h - taskbar::HEIGHT, m.screen.w, taskbar::HEIGHT);
        let bg = chrome::mica_color(&m.wallpaper.blurred, bar, TINT);
        ui.canvas.fill_rect(ui.rect(), bg);
        if ui.input.key(vproto::input::keys::ESC) {
            ui.close_window();
        }
        let now = ui.now();
        let approval = m.agent.approvals.first().cloned();
        let cy = if approval.is_some() { 118 } else { 168 };
        let base = if approval.is_some() { 50.0 } else { 64.0 };

        // The ring: tap to talk, tap again to stop.
        let ring =
            Rect::new(w / 2 - base as i32 - 10, cy - base as i32 - 10, 2 * base as i32 + 20, 2 * base as i32 + 20);
        let resp = ui.interact(ui.id("ring"), ring);
        if resp.hovered {
            ui.set_cursor(vui::Cursor::Hand);
        }
        draw_presence(ui, w as f32 / 2.0, cy as f32, base, &m.agent, now);
        if resp.clicked {
            m.push(if m.agent.active() { Action::AgentSleep } else { Action::AgentWake });
        }

        // One quiet line under it.
        let caption = m.agent.caption();
        if !caption.is_empty() {
            let y = cy + base as i32 + 26;
            if ui.measure(&caption, Font::Regular, 13.0) <= (w - 56) as f32 {
                ui.label(Rect::new(28, y, w - 56, 20), &caption, Font::Regular, 13.0, th.text_dim, Align::Center);
            } else {
                ui.paragraph(Rect::new(28, y, w - 56, 60), &caption, 13.0, th.text_dim);
            }
            if matches!(m.agent.state(), Some(AgentState::Off | AgentState::Error)) {
                let link = Rect::new(w / 2 - 70, y + 44, 140, 30);
                if ui.button(link, "Open Settings") {
                    m.push(Action::Launch("settings".into(), alloc::vec!["agent".into()]));
                }
            }
        }

        // The microphone switch, small, in a corner.
        let muted = m.agent.status.as_ref().is_some_and(|s| s.muted);
        let mic = Rect::new(10, h - 44, 34, 34);
        let (icon, tip) =
            if muted { (Icon::MicOff, "Turn the microphone on") } else { (Icon::Mic, "Turn the microphone off") };
        if ui.icon_button(mic, icon, tip) {
            m.push(Action::AgentMute(!muted));
        }

        if let Some(req) = approval {
            self.approval_card(ui, m, &req, Rect::new(14, h - 196, w - 28, 146));
        }
        chrome::popup_frame(&mut ui.canvas);
        // Animate while there is something to animate.
        let rate = if m.agent.active() { 16_000_000 } else { 250_000_000 };
        ui.repaint_at(now + rate);
    }

    fn approval_card(&mut self, ui: &mut Ui, m: &mut Model, req: &ApprovalRequest, r: Rect) {
        let th = ui.theme().clone();
        ui.canvas.fill_rounded_rect(r, 10.0, Color::rgba(255, 255, 255, 18));
        ui.canvas.stroke_rounded_rect(r, 10.0, 1.0, Color::rgba(255, 255, 255, 30));
        let ask = format!("{} needs your OK", m.agent.name());
        ui.label(Rect::new(r.x + 14, r.y + 10, r.w - 28, 16), &ask, Font::Regular, 12.0, th.text_faint, Align::Left);
        let used = approval_title(ui, Rect::new(r.x + 14, r.y + 28, r.w - 28, 20), &req.action);
        let dy = used - 20;
        ui.paragraph(Rect::new(r.x + 14, r.y + 50 + dy, r.w - 28, 40 - dy), &req.detail, 12.5, th.text_dim);
        let by = r.bottom() - 44;
        if req.allow_always {
            ui.checkbox(Rect::new(r.x + 12, by + 4, 130, 26), "Always allow", &mut self.always);
        }
        let more = m.agent.approvals.len().saturating_sub(1);
        if more > 0 && !req.allow_always {
            ui.label(
                Rect::new(r.x + 14, by + 4, 120, 26),
                &format!("+{more} more"),
                Font::Regular,
                12.0,
                th.text_faint,
                Align::Left,
            );
        }
        if ui.button(Rect::new(r.right() - 186, by, 84, 32), "Deny") {
            m.push(Action::AgentDecide { id: req.id, allow: false, always: false });
            self.always = false;
        }
        if ui.primary_button(Rect::new(r.right() - 96, by, 84, 32), "Allow") {
            m.push(Action::AgentDecide { id: req.id, allow: true, always: self.always });
            self.always = false;
        }
    }
}
