//! The voice agent's presence on the desktop.
//!
//! * [`AgentLink`]: the shell's connection to the agent service — the
//!   interface link (status and approval events, the shared page of live
//!   voice levels) and the requests only the shell may make (wake, sleep,
//!   decide on approvals, mute).
//! * [`AgentWindow`]: the agent's window, deliberately minimal: the
//!   taskbar's own background and a white circle. The circle breathes while
//!   the agent listens and trembles with its voice while it speaks; a line
//!   of small text says what is going on only when that helps. When the
//!   agent needs consent for an action, the request appears at the bottom
//!   of the window (or, when the window is closed, as a notification).
//! * [`draw_orb`]: the agent's tray item.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, FillRule, Path, Rect};
use vmath::FloatExt;
use vproto::agent::{AGENT_EVENT, AgentEvent, AgentState, AgentStatus, ApprovalRequest, Live, agent};
use vrt::object::Channel;
use vrt::vm::Mapping;
use vui::{Font, Icon, Ui};

use crate::{Action, Model, chrome, connect_running, taskbar};

/// The window's size.
pub const WIDTH: i32 = 340;
pub const HEIGHT: i32 = 420;
/// The taskbar's tint (see `taskbar::Taskbar::update`).
const TINT: Color = Color::rgba(20, 20, 28, 200);

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
}

impl AgentModel {
    pub fn state(&self) -> Option<AgentState> {
        self.status.as_ref().map(|s| s.state)
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
            AgentState::Asleep => format!("Say \u{201c}{}\u{201d} or tap the circle", s.name),
            AgentState::Waking => "One moment\u{2026}".into(),
            AgentState::Listening => "Listening".into(),
            AgentState::Thinking => "Thinking\u{2026}".into(),
            AgentState::Speaking => String::new(),
        }
    }
}

/// The closed outline of the circle at time `t` (seconds): radius `r`,
/// trembling by `amount` (0..=1).
fn blob(cx: f32, cy: f32, r: f32, amount: f32, t: f32) -> Path {
    let mut p = Path::new();
    const N: usize = 96;
    for i in 0..=N {
        let a = i as f32 / N as f32 * core::f32::consts::TAU;
        let wave = (5.0 * a + 3.1 * t).sin() + 0.6 * (8.0 * a - 4.3 * t).sin() + 0.4 * (3.0 * a + 2.2 * t).sin();
        let rr = r * (1.0 + amount * (0.06 + 0.035 * wave));
        let (x, y) = (cx + rr * a.cos(), cy + rr * a.sin());
        if i == 0 {
            p.move_to(x, y);
        } else {
            p.line_to(x, y);
        }
    }
    p.close();
    p
}

/// Draws the circle for the agent's state (`levels` smoothed).
pub fn draw_circle(ui: &mut Ui, cx: f32, cy: f32, base: f32, a: &AgentModel, t: f32) {
    let (voice, mic) = a.levels;
    let state = a.state().unwrap_or(AgentState::Off);
    let white = |alpha: u8| Color::rgba(255, 255, 255, alpha);
    match state {
        AgentState::Speaking => {
            // A soft halo that swells with the voice, and the trembling disc.
            let halo = base * (1.12 + 0.18 * voice);
            ui.canvas.fill_circle(cx, cy, halo, white((18.0 + 40.0 * voice) as u8));
            ui.canvas.fill_path(&blob(cx, cy, base, 0.25 + 1.4 * voice, t), white(255), FillRule::NonZero);
        }
        AgentState::Listening => {
            let breath = 0.5 + 0.5 * (t * 1.6).sin();
            let r = base * (0.97 + 0.03 * breath + 0.06 * mic);
            ui.canvas.fill_circle(cx, cy, r * 1.14, white((14.0 + 22.0 * breath + 40.0 * mic) as u8));
            ui.canvas.fill_path(&blob(cx, cy, r, 0.15 + 0.6 * mic, t * 0.6), white(250), FillRule::NonZero);
        }
        AgentState::Thinking | AgentState::Waking => {
            let pulse = 0.5 + 0.5 * (t * 3.4).sin();
            let r = base * (0.92 + 0.04 * pulse);
            ui.canvas.fill_circle(cx, cy, r * 1.12, white((10.0 + 20.0 * pulse) as u8));
            ui.canvas.fill_circle(cx, cy, r, white((200.0 + 55.0 * pulse) as u8));
        }
        AgentState::Asleep => {
            ui.canvas.fill_circle(cx, cy, base * 0.86, white(150));
        }
        AgentState::Off | AgentState::Error => {
            let mut ring = Path::new();
            ring.circle(cx, cy, base * 0.86);
            ui.canvas.stroke_path(&ring, &vgfx::StrokeStyle::new(2.0), white(110));
        }
    }
}

/// The agent's tray item: a small circle that echoes the big one.
pub fn draw_orb(ui: &mut Ui, r: Rect, a: &AgentModel, t: f32) {
    let (cx, cy) = (r.center().0 as f32, r.center().1 as f32);
    let state = a.state();
    let (voice, mic) = a.levels;
    let white = |alpha: u8| Color::rgba(255, 255, 255, alpha);
    match state {
        Some(AgentState::Speaking) => {
            ui.canvas.fill_circle(cx, cy, 9.0 + 3.0 * voice, white(40));
            ui.canvas.fill_path(&blob(cx, cy, 7.0, 0.4 + 1.5 * voice, t), white(255), FillRule::NonZero);
        }
        Some(AgentState::Listening) => {
            let breath = 0.5 + 0.5 * (t * 1.6).sin();
            ui.canvas.fill_circle(cx, cy, 9.5, white((30.0 + 30.0 * breath + 50.0 * mic) as u8));
            ui.canvas.fill_circle(cx, cy, 7.0, white(255));
        }
        Some(AgentState::Thinking | AgentState::Waking) => {
            let pulse = 0.5 + 0.5 * (t * 3.4).sin();
            ui.canvas.fill_circle(cx, cy, 7.0, white((170.0 + 85.0 * pulse) as u8));
        }
        Some(AgentState::Asleep) => ui.canvas.fill_circle(cx, cy, 6.5, white(200)),
        _ => {
            let mut ring = Path::new();
            ring.circle(cx, cy, 6.5);
            ui.canvas.stroke_path(&ring, &vgfx::StrokeStyle::new(1.5), white(150));
        }
    }
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
    origin: (i32, i32),
    /// "Always allow" ticked on the approval being shown.
    always: bool,
    started: u64,
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
    pub fn new(origin: (i32, i32)) -> AgentWindow {
        AgentWindow { origin, always: false, started: vrt::time::now_ns() }
    }

    pub fn update(&mut self, ui: &mut Ui, m: &mut Model) {
        let th = ui.theme().clone();
        let (w, h) = (ui.width, ui.height);
        chrome::mica(&mut ui.canvas, &m.wallpaper.blurred, self.origin, TINT);
        if ui.input.key(vproto::input::keys::ESC) {
            ui.close_window();
        }
        let now = ui.now();
        let t = (now - self.started) as f32 / 1e9;
        let approval = m.agent.approvals.first().cloned();
        let cy = if approval.is_some() { 118 } else { 168 };
        let base = if approval.is_some() { 50.0 } else { 64.0 };

        // The circle: tap to talk, tap again to stop.
        let circle =
            Rect::new(w / 2 - base as i32 - 10, cy - base as i32 - 10, 2 * base as i32 + 20, 2 * base as i32 + 20);
        let resp = ui.interact(ui.id("circle"), circle);
        if resp.hovered {
            ui.set_cursor(vui::Cursor::Hand);
        }
        draw_circle(ui, w as f32 / 2.0, cy as f32, base, &m.agent, t);
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
