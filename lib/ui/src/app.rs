//! Running UIs: [`Host`] drives one window (events, frame pacing, drawing);
//! [`run`] is the event loop for the common one-window [`App`], which also
//! serves the voice agent for apps that offer it something (see
//! [`crate::agent`]).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use vabi::{RawHandle, WaitItem, signals};
use vgfx::Text;
use vproto::display::{Cursor, WindowEvent, WindowSpec};

use crate::agent::{AgentLink, AgentServer, AppAgentInfo};
use crate::theme::Theme;
use crate::ui::{Context, Input, Ui, UiState};
use crate::window::{Display, Window, WindowError, connect};

/// An application with one main window.
pub trait App {
    /// Builds and draws one frame.
    fn update(&mut self, ui: &mut Ui);

    /// The user asked to close the window; return `true` to allow it.
    fn close_requested(&mut self) -> bool {
        true
    }

    /// Additional kernel objects to wait on (handle, signals).
    fn wait_handles(&self) -> Vec<(RawHandle, u32)> {
        Vec::new()
    }

    /// One of [`App::wait_handles`] was signalled; a frame follows.
    fn handle_signaled(&mut self, _index: usize, _observed: u32) {}

    /// Called once the window exists, before the first frame.
    fn init(&mut self, _window: &Window) {}

    /// What the app offers the voice agent: a summary and its actions.
    /// `None` (the default) keeps the app to itself.
    fn agent_info(&self) -> Option<AppAgentInfo> {
        None
    }

    /// What the app shows right now, for the agent (a JSON value).
    fn agent_state(&self) -> vjson::Value {
        vjson::Value::Null
    }

    /// Runs one of the actions from [`App::agent_info`] for the agent.
    /// `args` is a JSON object; the result is a JSON value for the agent
    /// (or a short explanation of what went wrong).
    fn agent_invoke(&mut self, _action: &str, _args: &vjson::Value) -> Result<vjson::Value, alloc::string::String> {
        Err("this app does not offer that".into())
    }
}

/// Serves the agent through an [`App`]'s methods.
struct AppAgent<'a, A: App>(&'a mut A);

impl<A: App> AgentServer for AppAgent<'_, A> {
    fn agent_info(&self) -> AppAgentInfo {
        self.0.agent_info().unwrap_or(AppAgentInfo {
            summary: alloc::string::String::new(),
            actions: Vec::new(),
            has_state: false,
        })
    }

    fn agent_state(&self) -> vjson::Value {
        self.0.agent_state()
    }

    fn agent_invoke(&mut self, action: &str, args: &vjson::Value) -> Result<vjson::Value, alloc::string::String> {
        self.0.agent_invoke(action, args)
    }
}

/// Font files, loaded once per process and shared by every window.
static FONT_DATA: vrt::sync::Mutex<Option<Vec<Option<&'static [u8]>>>> = vrt::sync::Mutex::new(None);

const FONT_FILES: [&str; 5] = [
    "/system/fonts/Inter-Regular.otf",
    "/system/fonts/Inter-SemiBold.otf",
    "/system/fonts/JetBrainsMono-Regular.ttf",
    "/system/fonts/JetBrainsMono-Bold.ttf",
    "/system/fonts/Lato-Regular.ttf",
];

/// Loads the standard fonts. Order defines the fallback chain: Inter,
/// Inter SemiBold, JetBrains Mono (+ Bold), Lato. Returns the text renderer
/// and the indices of the [`crate::Font`] roles.
pub fn load_fonts() -> (Text, [usize; 4]) {
    let mut data = FONT_DATA.lock();
    if data.is_none() {
        let mut files = Vec::new();
        let vfs = vproto::connect(vproto::vfs::NAME).ok().map(vproto::vfs::Client::new);
        for path in FONT_FILES {
            let bytes = vfs.as_ref().and_then(|vfs| {
                let (vmo, len) = vfs.read_file(path.into()).ok()?.ok()?;
                let mut buf = alloc::vec![0u8; len as usize];
                vmo.read(0, &mut buf).ok()?;
                Some(&*buf.leak())
            });
            files.push(bytes);
        }
        *data = Some(files);
    }
    let mut text = Text::new();
    let mut idx = [0usize; 4];
    for (i, bytes) in data.as_ref().unwrap().iter().enumerate() {
        if let Some(f) = bytes.and_then(|b| text.add_font(b))
            && i < 4
        {
            idx[i] = f;
        }
    }
    (text, idx)
}

/// Drives one window: turns window events into UI input, paces frames to the
/// compositor and calls a drawing function.
pub struct Host {
    pub window: Window,
    pub ctx: Context,
    pub state: UiState,
    input: Input,
    cursor: Cursor,
    dirty: bool,
    repaint_at: Option<u64>,
    pending: VecDeque<WindowEvent>,
    /// The UI asked to close the window (`Ui::close_window`).
    pub close_requested: bool,
}

impl Host {
    pub fn new(display: &Display, spec: WindowSpec) -> Result<Host, WindowError> {
        let window = Window::new(display, spec)?;
        let (text, fonts) = load_fonts();
        let ctx = Context { text, fonts, theme: Theme::dark(), display: display.clone(), window_id: window.id };
        Ok(Host {
            window,
            ctx,
            state: UiState::default(),
            input: Input { focused: true, ..Default::default() },
            cursor: Cursor::Arrow,
            dirty: true,
            repaint_at: None,
            pending: VecDeque::new(),
            close_requested: false,
        })
    }

    /// Forces a redraw at the next opportunity.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// What to wait on for this window.
    pub fn wait_item(&self) -> WaitItem {
        WaitItem {
            handle: self.window.event_channel().raw(),
            signals: signals::READABLE | signals::PEER_CLOSED,
            ..Default::default()
        }
    }

    /// When the next frame is due (absent new events).
    pub fn deadline(&self) -> u64 {
        if (self.dirty || !self.pending.is_empty()) && self.window.can_draw() {
            // A frame is wanted and may be drawn now (e.g. after
            // `invalidate`): do not wait for further events.
            0
        } else if self.dirty {
            vabi::DEADLINE_INFINITE // waiting for FrameDone
        } else {
            self.repaint_at.unwrap_or(vabi::DEADLINE_INFINITE)
        }
    }

    fn frame(&mut self, draw: &mut impl FnMut(&mut Ui)) {
        self.input.now = vrt::time::now_ns();
        let bg = self.ctx.theme.bg;
        let window_state = self.window.state;
        let Ok(mut canvas) = self.window.begin_frame() else { return };
        canvas.clear(bg);
        // While a menu is open it has the keyboard: widgets see no keys.
        let menu_open = self.state.open_menu.is_some() || self.state.context_menu.is_some();
        let quiet;
        let input = if menu_open {
            quiet = self.input.without_keys();
            &quiet
        } else {
            &self.input
        };
        let mut ui = Ui::new(canvas, &mut self.ctx, &mut self.state, input);
        ui.menu_input = &self.input;
        ui.window_state = window_state;
        draw(&mut ui);
        ui.finish();
        let (repaint, close, cursor, skip) = (ui.repaint_at, ui.close_requested, ui.cursor, ui.skip_present);
        drop(ui);
        if !skip {
            let _ = self.window.present(&[]);
        }
        if cursor != self.cursor {
            self.cursor = cursor;
            self.window.set_cursor(cursor);
        }
        self.input.begin_frame();
        self.repaint_at = repaint;
        self.close_requested |= close;
    }

    /// Processes pending window events and draws if needed. Events the UI
    /// does not consume (close requests, shell notifications) are returned.
    pub fn pump(&mut self, mut draw: impl FnMut(&mut Ui)) -> Vec<WindowEvent> {
        let mut unhandled = Vec::new();
        self.pending.extend(self.window.poll_events());
        while let Some(ev) = self.pending.front() {
            if self.input.needs_flush_before(ev) {
                if !self.window.can_draw() {
                    break;
                }
                self.frame(&mut draw);
                continue;
            }
            let ev = self.pending.pop_front().unwrap();
            match &ev {
                WindowEvent::CloseRequested {}
                | WindowEvent::WindowsChanged {}
                | WindowEvent::StartMenuKey {}
                | WindowEvent::AgentKey {} => unhandled.push(ev.clone()),
                WindowEvent::FrameDone { .. } => {}
                WindowEvent::Configure { .. } => {}
                _ => self.input.apply(&ev),
            }
            if !matches!(ev, WindowEvent::FrameDone { .. }) {
                self.dirty = true;
            }
        }
        if self.repaint_at.is_some_and(|t| vrt::time::now_ns() >= t) {
            self.repaint_at = None;
            self.dirty = true;
        }
        if self.dirty && self.window.can_draw() {
            self.dirty = false;
            self.frame(&mut draw);
        }
        unhandled
    }
}

/// Runs `app` in a window described by `spec` until it closes. Returns the
/// process exit code.
pub fn run<A: App>(spec: WindowSpec, mut app: A) -> i32 {
    let display = match connect() {
        Ok(d) => d,
        Err(e) => {
            vrt::println!("cannot connect to the display: {:?}", e);
            return 1;
        }
    };
    let mut host = match Host::new(&display, spec) {
        Ok(h) => h,
        Err(e) => {
            vrt::println!("cannot create a window: {:?}", e);
            return 1;
        }
    };
    app.init(&host.window);
    let mut agent = if app.agent_info().is_some() { AgentLink::connect() } else { None };
    loop {
        let events = host.pump(|ui| app.update(ui));
        if host.window.closed {
            return 0;
        }
        let wants_close = host.close_requested || events.iter().any(|e| matches!(e, WindowEvent::CloseRequested {}));
        if wants_close {
            host.close_requested = false;
            if app.close_requested() {
                return 0;
            }
            host.invalidate();
        }
        let extra = app.wait_handles();
        let mut items = Vec::with_capacity(2 + extra.len());
        items.push(host.wait_item());
        for (h, s) in &extra {
            items.push(WaitItem { handle: *h, signals: *s, ..Default::default() });
        }
        if let Some(link) = &agent {
            items.push(WaitItem {
                handle: link.handle(),
                signals: signals::READABLE | signals::PEER_CLOSED,
                ..Default::default()
            });
        }
        let _ = vrt::object::wait_many(&mut items, host.deadline());
        for (i, it) in items.iter().enumerate().skip(1).take(extra.len()) {
            if it.observed & it.signals != 0 {
                app.handle_signaled(i - 1, it.observed);
                host.invalidate();
            }
        }
        if let Some(link) = &agent
            && items.last().is_some_and(|it| it.observed != 0)
        {
            // The agent asked something (the answer may change what the
            // window shows), or the agent service went away.
            if !link.serve(&mut AppAgent(&mut app)) {
                agent = None;
            }
            host.invalidate();
        }
    }
}
