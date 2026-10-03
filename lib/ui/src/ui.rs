//! The immediate-mode UI context.
//!
//! Each frame the application's `update` receives a [`Ui`]: it draws widgets
//! at explicit rectangles and learns about interactions in the same call
//! (`if ui.button(r, "Save") { ... }`). State that must survive between
//! frames (focus, scroll offsets, text cursors, open menus) lives in
//! [`UiState`], keyed by widget [`Id`]s.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Canvas, Color, Rect, Text};
use vproto::display::{Cursor, WindowEvent, modifiers};

use crate::theme::{Font, Theme};
use crate::window::Display;

/// Identifies a widget across frames.
pub type Id = u64;

/// FNV-1a hash of a string mixed with a salt.
pub fn hash_id(salt: u64, s: &str) -> Id {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ salt;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A key press delivered this frame.
#[derive(Debug, Clone)]
pub struct KeyPress {
    pub code: u16,
    pub modifiers: u32,
    pub repeat: bool,
}

/// Input accumulated since the previous frame.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// Pointer position (window coordinates), `None` when outside.
    pub pointer: Option<(i32, i32)>,
    pub down: [bool; 3],
    pub pressed: [bool; 3],
    pub released: [bool; 3],
    /// Click count of the most recent press (2 = double click).
    pub clicks: u8,
    pub scroll: (i32, i32),
    pub text: String,
    pub keys: Vec<KeyPress>,
    pub modifiers: u32,
    pub focused: bool,
    pub now: u64,
}

impl Input {
    /// Clears the per-frame fields.
    pub fn begin_frame(&mut self) {
        self.pressed = [false; 3];
        self.released = [false; 3];
        self.scroll = (0, 0);
        self.text.clear();
        self.keys.clear();
    }

    /// Returns `true` if the event must be applied in a fresh frame (so
    /// that a press and its release are seen in different frames).
    pub fn needs_flush_before(&self, ev: &WindowEvent) -> bool {
        match ev {
            WindowEvent::PointerButton { button, pressed, .. } => {
                let b = (*button as usize).min(2);
                if *pressed { self.released[b] || self.pressed[b] } else { self.pressed[b] }
            }
            _ => false,
        }
    }

    pub fn apply(&mut self, ev: &WindowEvent) {
        match ev {
            WindowEvent::PointerMove { x, y } => self.pointer = Some((*x, *y)),
            WindowEvent::PointerLeave {} => {
                if !self.down.iter().any(|d| *d) {
                    self.pointer = None;
                }
            }
            WindowEvent::PointerButton { x, y, button, pressed, clicks } => {
                let b = (*button as usize).min(2);
                self.pointer = Some((*x, *y));
                self.down[b] = *pressed;
                if *pressed {
                    self.pressed[b] = true;
                    self.clicks = *clicks;
                } else {
                    self.released[b] = true;
                }
            }
            WindowEvent::Scroll { dx, dy } => {
                self.scroll.0 += dx;
                self.scroll.1 += dy;
            }
            WindowEvent::Key { code, pressed, repeat, modifiers, text } => {
                self.modifiers = *modifiers;
                if *pressed {
                    self.keys.push(KeyPress { code: *code, modifiers: *modifiers, repeat: *repeat });
                    if modifiers & (modifiers::CTRL | modifiers::ALT | modifiers::SUPER) == 0 {
                        // Control characters are delivered as key codes only.
                        for c in text.chars().filter(|c| !c.is_control() || *c == '\t') {
                            self.text.push(c);
                        }
                    }
                }
            }
            WindowEvent::Focus { focused } => {
                self.focused = *focused;
                if !focused {
                    self.down = [false; 3];
                    self.modifiers = 0;
                }
            }
            _ => {}
        }
    }

    pub fn key(&self, code: u16) -> bool {
        self.keys.iter().any(|k| k.code == code)
    }

    /// A key pressed together with Ctrl.
    pub fn ctrl_key(&self, code: u16) -> bool {
        self.keys.iter().any(|k| k.code == code && k.modifiers & modifiers::CTRL != 0)
    }

    pub fn ctrl(&self) -> bool {
        self.modifiers & modifiers::CTRL != 0
    }

    pub fn shift(&self) -> bool {
        self.modifiers & modifiers::SHIFT != 0
    }

    pub fn alt(&self) -> bool {
        self.modifiers & modifiers::ALT != 0
    }
}

/// Persistent state of a single-line text editor.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextEditState {
    pub cursor: usize,
    pub anchor: usize,
    pub scroll: f32,
    pub blink_start: u64,
}

/// State that survives between frames.
#[derive(Default)]
pub struct UiState {
    pub hot: Option<Id>,
    pub active: Option<Id>,
    pub focus: Option<Id>,
    pub floats: BTreeMap<Id, f32>,
    pub edits: BTreeMap<Id, TextEditState>,
    /// Open menu: (menu bar id, menu index).
    pub open_menu: Option<(Id, usize)>,
    /// Open context menu: (id, position).
    pub context_menu: Option<(Id, i32, i32)>,
    /// Overlay rectangles drawn last frame (they capture the pointer).
    pub overlays: Vec<Rect>,
    /// A modal dialog was shown last frame.
    pub modal: bool,
    pub tooltip: Option<(Id, u64)>,
}

/// Fonts and other per-window resources shared by all frames.
pub struct Context {
    pub text: Text,
    pub fonts: [usize; 4],
    pub theme: Theme,
    pub state: UiState,
    pub display: Display,
    pub window_id: u32,
}

impl Context {
    pub fn font(&self, f: Font) -> usize {
        self.fonts[f as usize]
    }
}

/// Deferred drawing for overlays (menus, tooltips) after the main pass.
pub(crate) enum Overlay {
    Menu { rect: Rect, items: Vec<crate::menu::MenuItem>, hovered: Option<usize> },
    Tooltip { rect: Rect, text: String },
}

/// The per-frame UI context.
pub struct Ui<'a> {
    pub canvas: Canvas<'a>,
    pub ctx: &'a mut Context,
    pub input: &'a Input,
    pub width: i32,
    pub height: i32,
    pub(crate) cursor: Cursor,
    pub(crate) repaint_at: Option<u64>,
    pub(crate) overlays: Vec<Overlay>,
    pub(crate) new_overlay_rects: Vec<Rect>,
    /// Pointer input is hidden from widgets (covered by an overlay/modal).
    pub(crate) blocked: bool,
    pub(crate) in_modal: bool,
    pub(crate) modal_shown: bool,
    pub(crate) salt: u64,
    pub(crate) close_requested: bool,
}

/// Result of interacting with a widget area.
#[derive(Debug, Clone, Copy, Default)]
pub struct Response {
    pub rect: Rect,
    pub hovered: bool,
    /// Primary button went down on the widget this frame.
    pub pressed: bool,
    /// Primary button was released over the widget after pressing on it.
    pub clicked: bool,
    pub double_clicked: bool,
    /// Primary button is held after pressing on the widget.
    pub held: bool,
    pub right_clicked: bool,
}

impl<'a> Ui<'a> {
    pub(crate) fn new(canvas: Canvas<'a>, ctx: &'a mut Context, input: &'a Input) -> Ui<'a> {
        let (width, height) = (canvas.width(), canvas.height());
        let blocked_by_overlay = input.pointer.is_some_and(|(x, y)| ctx.state.overlays.iter().any(|r| r.contains(x, y)));
        let blocked = blocked_by_overlay || ctx.state.modal;
        ctx.state.hot = None;
        Ui {
            canvas,
            ctx,
            input,
            width,
            height,
            cursor: Cursor::Arrow,
            repaint_at: None,
            overlays: Vec::new(),
            new_overlay_rects: Vec::new(),
            blocked,
            in_modal: false,
            modal_shown: false,
            salt: 0,
            close_requested: false,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.ctx.theme
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn now(&self) -> u64 {
        self.input.now
    }

    /// An id for `label`, unique within the current salt scope.
    pub fn id(&self, label: &str) -> Id {
        hash_id(self.salt, label)
    }

    /// Runs `f` with ids salted by `scope` (for repeated widget groups).
    pub fn scope<R>(&mut self, scope: u64, f: impl FnOnce(&mut Ui<'a>) -> R) -> R {
        let old = self.salt;
        self.salt = self.salt.wrapping_mul(31).wrapping_add(scope.wrapping_add(1));
        let r = f(self);
        self.salt = old;
        r
    }

    /// Pointer position if it is not hidden by an overlay.
    pub fn pointer(&self) -> Option<(i32, i32)> {
        if self.blocked { None } else { self.input.pointer }
    }

    pub fn hovered(&self, r: Rect) -> bool {
        self.pointer().is_some_and(|(x, y)| r.contains(x, y))
    }

    /// Requests another frame at `deadline` (monotonic ns) for animation.
    pub fn repaint_at(&mut self, deadline: u64) {
        self.repaint_at = Some(self.repaint_at.map_or(deadline, |d| d.min(deadline)));
    }

    /// Requests another frame as soon as possible.
    pub fn repaint(&mut self) {
        self.repaint_at(self.input.now);
    }

    pub fn set_cursor(&mut self, c: Cursor) {
        self.cursor = c;
    }

    /// Asks the application loop to close the window after this frame.
    pub fn close_window(&mut self) {
        self.close_requested = true;
    }

    /// Basic button-like interaction for an area.
    pub fn interact(&mut self, id: Id, r: Rect) -> Response {
        let hovered = self.hovered(r);
        let st = &mut self.ctx.state;
        if hovered {
            st.hot = Some(id);
        }
        let pressed = hovered && self.input.pressed[0];
        if pressed {
            st.active = Some(id);
        }
        let is_active = st.active == Some(id);
        let clicked = is_active && self.input.released[0] && hovered;
        if is_active && self.input.released[0] {
            st.active = None;
        }
        Response {
            rect: r,
            hovered,
            pressed,
            clicked,
            double_clicked: pressed && self.input.clicks >= 2,
            held: is_active && self.input.down[0],
            right_clicked: hovered && self.input.pressed[1],
        }
    }

    pub fn focused(&self, id: Id) -> bool {
        self.ctx.state.focus == Some(id)
    }

    pub fn focus(&mut self, id: Id) {
        self.ctx.state.focus = Some(id);
    }

    /// Smoothly animates a per-widget value towards `target` (0..=1).
    pub fn animate(&mut self, id: Id, target: f32, speed_per_sec: f32) -> f32 {
        let key = id ^ 0x5bd1_e995;
        let v = self.ctx.state.floats.get(&key).copied().unwrap_or(target);
        if (v - target).abs() < 0.001 {
            self.ctx.state.floats.insert(key, target);
            return target;
        }
        // Frames arrive irregularly; assume ~30 fps steps.
        let step = speed_per_sec / 30.0;
        let nv = if v < target { (v + step).min(target) } else { (v - step).max(target) };
        self.ctx.state.floats.insert(key, nv);
        let now = self.now();
        self.repaint_at(now + 30_000_000);
        nv
    }

    // ---- text --------------------------------------------------------------

    /// Draws one line of text centred vertically in `r`.
    pub fn label(&mut self, r: Rect, text: &str, font: Font, size: f32, color: Color, align: Align) {
        let f = self.ctx.font(font);
        self.ctx.text.draw_in(&mut self.canvas, f, size, r, text, color, align);
    }

    /// Body text in the default style.
    pub fn text(&mut self, r: Rect, text: &str) {
        let (size, color) = (self.ctx.theme.font_size, self.ctx.theme.text);
        self.label(r, text, Font::Regular, size, color, Align::Left);
    }

    pub fn dim_text(&mut self, r: Rect, text: &str) {
        let (size, color) = (self.ctx.theme.font_size, self.ctx.theme.text_dim);
        self.label(r, text, Font::Regular, size, color, Align::Left);
    }

    pub fn heading(&mut self, r: Rect, text: &str) {
        let (size, color) = (self.ctx.theme.heading_size, self.ctx.theme.text);
        self.label(r, text, Font::Bold, size, color, Align::Left);
    }

    /// Word-wrapped text from the top of `r`; returns the height used.
    pub fn paragraph(&mut self, r: Rect, text: &str, size: f32, color: Color) -> i32 {
        let f = self.ctx.font(Font::Regular);
        self.ctx.text.draw_wrapped(&mut self.canvas, f, size, r, text, color, Align::Left)
    }

    pub fn measure(&self, text: &str, font: Font, size: f32) -> f32 {
        self.ctx.text.measure(self.ctx.font(font), size, text)
    }

    // ---- clipboard ---------------------------------------------------------

    pub fn set_clipboard(&mut self, text: &str) {
        let _ = self.ctx.display.set_clipboard(String::from(text));
    }

    pub fn clipboard(&mut self) -> String {
        self.ctx.display.get_clipboard().unwrap_or_default()
    }

    /// Finishes the frame: draws overlays and records what they cover.
    pub(crate) fn finish(&mut self) {
        let overlays = core::mem::take(&mut self.overlays);
        for o in overlays {
            match o {
                Overlay::Menu { rect, items, hovered } => crate::menu::draw_dropdown(self, rect, &items, hovered),
                Overlay::Tooltip { rect, text } => {
                    let th = self.ctx.theme.clone();
                    self.canvas.draw_shadow(rect, 6, 8, Color::rgba(0, 0, 0, 90));
                    self.canvas.fill_rounded_rect(rect, 6.0, Color::hex(0x2E2E36));
                    self.canvas.stroke_rounded_rect(rect, 6.0, 1.0, th.border_strong);
                    self.label(rect.inset(8, 0, 8, 0), &text, Font::Regular, th.small_size, th.text, Align::Center);
                }
            }
        }
        self.ctx.state.overlays = core::mem::take(&mut self.new_overlay_rects);
        self.ctx.state.modal = self.modal_shown;
        // Releasing the button anywhere ends any press interaction.
        if self.input.released[0] && !self.input.down[0] {
            self.ctx.state.active = None;
        }
    }
}
