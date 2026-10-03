//! The application runner: one main window, an event loop, frame pacing.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use vabi::{RawHandle, WaitItem, signals};
use vgfx::Text;
use vproto::display::{Cursor, WindowEvent, WindowSpec};

use crate::theme::Theme;
use crate::ui::{Context, Input, Ui, UiState};
use crate::window::{Window, connect};

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
}

/// Loads the standard fonts from the system image. Order defines the
/// fallback chain: Inter, Inter SemiBold, JetBrains Mono (+ Bold), Lato.
pub fn load_fonts() -> (Text, [usize; 4]) {
    let mut text = Text::new();
    let mut idx = [0usize; 4];
    let Ok(ch) = vproto::connect(vproto::vfs::NAME) else { return (text, idx) };
    let vfs = vproto::vfs::Client::new(ch);
    let files = [
        "/system/fonts/Inter-Regular.otf",
        "/system/fonts/Inter-SemiBold.otf",
        "/system/fonts/JetBrainsMono-Regular.ttf",
        "/system/fonts/JetBrainsMono-Bold.ttf",
        "/system/fonts/Lato-Regular.ttf",
    ];
    for (i, path) in files.iter().enumerate() {
        let Ok(Ok((vmo, len))) = vfs.read_file((*path).into()) else { continue };
        let mut data = alloc::vec![0u8; len as usize];
        if vmo.read(0, &mut data).is_err() {
            continue;
        }
        if let Some(f) = text.add_font(data.leak()) {
            if i < 4 {
                idx[i] = f;
            }
        }
    }
    (text, idx)
}

/// Draws one frame of `app` into `window`.
fn draw<A: App>(app: &mut A, window: &mut Window, ctx: &mut Context, input: &mut Input, cursor: &mut Cursor) -> (Option<u64>, bool) {
    input.now = vrt::time::now_ns();
    let bg = ctx.theme.bg;
    let Ok(mut canvas) = window.begin_frame() else { return (None, false) };
    canvas.clear(bg);
    let mut ui = Ui::new(canvas, ctx, input);
    app.update(&mut ui);
    ui.finish();
    let (repaint, close, new_cursor) = (ui.repaint_at, ui.close_requested, ui.cursor);
    drop(ui);
    let _ = window.present(&[]);
    if new_cursor != *cursor {
        *cursor = new_cursor;
        window.set_cursor(new_cursor);
    }
    input.begin_frame();
    (repaint, close)
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
    let mut window = match Window::new(&display, spec) {
        Ok(w) => w,
        Err(e) => {
            vrt::println!("cannot create a window: {:?}", e);
            return 1;
        }
    };
    let (text, fonts) = load_fonts();
    let mut ctx = Context { text, fonts, theme: Theme::dark(), state: UiState::default(), display: display.clone(), window_id: window.id };
    app.init(&window);
    let mut input = Input { focused: true, ..Default::default() };
    let mut cursor = Cursor::Arrow;
    let mut dirty = true;
    let mut repaint_at: Option<u64> = None;
    let mut pending: VecDeque<WindowEvent> = VecDeque::new();

    loop {
        pending.extend(window.poll_events());
        if window.closed {
            return 0;
        }
        while let Some(ev) = pending.front() {
            if input.needs_flush_before(ev) {
                if !window.can_draw() {
                    break; // wait for FrameDone, then continue
                }
                let (r, close) = draw(&mut app, &mut window, &mut ctx, &mut input, &mut cursor);
                if close && app.close_requested() {
                    return 0;
                }
                repaint_at = r;
                break;
            }
            let ev = pending.pop_front().unwrap();
            match &ev {
                WindowEvent::CloseRequested {} => {
                    if app.close_requested() {
                        return 0;
                    }
                }
                WindowEvent::FrameDone { .. } => {}
                _ => input.apply(&ev),
            }
            dirty = true;
        }
        let now = vrt::time::now_ns();
        if repaint_at.is_some_and(|t| now >= t) {
            repaint_at = None;
            dirty = true;
        }
        if dirty && window.can_draw() {
            dirty = false;
            let (r, close) = draw(&mut app, &mut window, &mut ctx, &mut input, &mut cursor);
            repaint_at = r;
            if close && app.close_requested() {
                return 0;
            }
        }
        // Wait for window events, the repaint deadline or app handles.
        let extra = app.wait_handles();
        let mut items = Vec::with_capacity(1 + extra.len());
        items.push(WaitItem { handle: window.event_channel().raw(), signals: signals::READABLE | signals::PEER_CLOSED, ..Default::default() });
        for (h, s) in &extra {
            items.push(WaitItem { handle: *h, signals: *s, ..Default::default() });
        }
        let deadline = if !pending.is_empty() && window.can_draw() {
            0
        } else if dirty {
            vabi::DEADLINE_INFINITE // waiting for FrameDone
        } else {
            repaint_at.unwrap_or(vabi::DEADLINE_INFINITE)
        };
        let _ = vrt::object::wait_many(&mut items, deadline);
        for (i, it) in items.iter().enumerate().skip(1) {
            if it.observed & it.signals != 0 {
                app.handle_signaled(i - 1, it.observed);
                dirty = true;
            }
        }
    }
}
