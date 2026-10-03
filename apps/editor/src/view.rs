//! The text view: draws a [`Document`] (line numbers, syntax colours,
//! search matches, selection, cursor) and turns keyboard and mouse input
//! into edits and cursor movement.

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::{Align, Color, Rect};
use vmath::FloatExt;
use vproto::display::modifiers;
use vproto::input::keys;
use vtext::highlight::{self, Language, Span, State, Token};
use vtext::{Document, Layout, Motion, Pos, Selection, char_width};
use vui::{Cursor, Font, MenuItem, Ui};

/// View settings shared by all tabs.
#[derive(Debug, Clone, Copy)]
pub struct ViewOptions {
    pub wrap: bool,
    pub line_numbers: bool,
    pub font_size: f32,
}

/// Editing commands also reachable from menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    ToggleComment,
    Indent,
    Outdent,
    MoveLineUp,
    MoveLineDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragUnit {
    Char,
    Word,
    Line,
}

const SCROLLBAR: i32 = 12;
const PAD: i32 = 6;
const BLINK_NS: u64 = 530_000_000;

/// Colour of a syntax token.
fn token_color(t: Token, text: Color) -> Color {
    match t {
        Token::Text => text,
        Token::Keyword => Color::hex(0xC678DD),
        Token::Type => Color::hex(0xE5C07B),
        Token::Function => Color::hex(0x61AFEF),
        Token::Macro | Token::Link => Color::hex(0x56B6C2),
        Token::String => Color::hex(0x98C379),
        Token::Number | Token::Constant => Color::hex(0xD19A66),
        Token::Comment => Color::hex(0x7F848E),
        Token::Attribute => Color::hex(0xE06C75),
        Token::Heading => Color::hex(0x6EA8FF),
    }
}

fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Pixel geometry of the view for one frame.
#[derive(Clone, Copy)]
struct Geometry {
    /// Gutter and text, without the scrollbar.
    area: Rect,
    /// x of display column 0 (after horizontal scrolling).
    origin_x: i32,
    lh: i32,
    cw: f32,
}

pub struct TextView {
    scroll_y: i32,
    scroll_x: i32,
    layout: Option<Layout>,
    layout_key: (u64, Option<usize>, usize),
    /// Highlighter state at the start of each line (a valid prefix).
    hl_states: Vec<State>,
    hl_lang: Language,
    drag: Option<(DragUnit, Selection)>,
    scrollbar_grab: Option<i32>,
    blink_start: u64,
    last_seen: (Selection, u64),
    /// Scroll the cursor into view on the next frame.
    reveal: bool,
    /// Line height of the last frame (for keyboard scrolling).
    lh: i32,
    page_rows: usize,
}

impl TextView {
    pub fn new() -> TextView {
        TextView {
            scroll_y: 0,
            scroll_x: 0,
            layout: None,
            layout_key: (u64::MAX, None, 0),
            hl_states: Vec::new(),
            hl_lang: Language::Plain,
            drag: None,
            scrollbar_grab: None,
            blink_start: 0,
            last_seen: (Selection::default(), u64::MAX),
            reveal: true,
            lh: 20,
            page_rows: 20,
        }
    }

    /// The widget id (for focus).
    fn id(ui: &Ui) -> vui::Id {
        ui.id("text-view")
    }

    /// Gives the view keyboard focus.
    pub fn focus(ui: &mut Ui) {
        let id = TextView::id(ui);
        ui.focus(id);
    }

    /// Scrolls the cursor into view on the next frame.
    pub fn reveal_cursor(&mut self) {
        self.reveal = true;
    }

    fn ensure_layout(&mut self, doc: &Document, wrap: Option<usize>) -> &Layout {
        let key = (doc.revision(), wrap, doc.tab_width);
        if self.layout.is_none() || self.layout_key != key {
            self.layout = Some(Layout::new(doc.buffer(), wrap, doc.tab_width));
            self.layout_key = key;
        }
        self.layout.as_ref().unwrap()
    }

    /// The buffer position under window point (x, y).
    fn hit(&self, doc: &Document, g: &Geometry, x: i32, y: i32) -> Pos {
        let layout = self.layout.as_ref().unwrap();
        let row = ((y - g.area.y + self.scroll_y).max(0) / g.lh) as usize;
        if row >= layout.row_count() {
            return doc.buffer().end();
        }
        let cells = ((x - g.origin_x) as f32 / g.cw + 0.5).floor().max(0.0) as usize;
        layout.pos_at(doc.buffer(), row, cells)
    }

    /// Moves the cursor `delta` visual rows, keeping the goal column.
    fn vertical(&mut self, doc: &mut Document, wrap: Option<usize>, delta: isize, extend: bool) {
        let sel = doc.selection();
        let from = match (extend, sel.is_empty(), delta < 0) {
            (false, false, true) => sel.start(),
            (false, false, false) => sel.end(),
            _ => sel.cursor,
        };
        let layout = self.ensure_layout(doc, wrap);
        let goal = doc.goal_x.unwrap_or_else(|| layout.x_of(doc.buffer(), from));
        let p = layout.vertical(doc.buffer(), from, goal, delta);
        doc.set_cursor(p, extend);
        doc.goal_x = Some(goal);
    }

    /// Runs an editing command.
    pub fn command(&mut self, ui: &mut Ui, doc: &mut Document, lang: Language, read_only: bool, cmd: Command) {
        let edit = !read_only;
        match cmd {
            Command::Undo if edit => {
                doc.undo();
            }
            Command::Redo if edit => {
                doc.redo();
            }
            Command::Cut if edit => {
                let s = doc.cut();
                if !s.is_empty() {
                    ui.set_clipboard(&s);
                }
            }
            Command::Copy => {
                let s = doc.selected_text();
                if !s.is_empty() {
                    ui.set_clipboard(&s);
                }
            }
            Command::Paste if edit => {
                let s = ui.clipboard();
                if !s.is_empty() {
                    doc.paste(&s);
                }
            }
            Command::Delete if edit => doc.delete_selection(),
            Command::SelectAll => doc.select_all(),
            Command::ToggleComment if edit => {
                if let Some(prefix) = lang.line_comment() {
                    doc.toggle_comment(prefix);
                }
            }
            Command::Indent if edit => doc.indent(),
            Command::Outdent if edit => doc.outdent(),
            Command::MoveLineUp if edit => doc.move_lines(true),
            Command::MoveLineDown if edit => doc.move_lines(false),
            _ => return,
        }
        self.reveal = true;
        self.blink_start = ui.now();
    }

    /// Handles one key press; returns `false` if the key is not for the view.
    fn key(
        &mut self,
        ui: &mut Ui,
        doc: &mut Document,
        lang: Language,
        read_only: bool,
        wrap: Option<usize>,
        k: &vui::ui::KeyPress,
    ) -> bool {
        let ctrl = k.modifiers & modifiers::CTRL != 0;
        let shift = k.modifiers & modifiers::SHIFT != 0;
        let alt = k.modifiers & modifiers::ALT != 0;
        let edit = !read_only;
        let page = self.page_rows.max(1) as isize;
        let command = |c| Some(c);
        let cmd = match k.code {
            keys::LEFT => {
                doc.motion(if ctrl { Motion::WordLeft } else { Motion::Left }, shift);
                None
            }
            keys::RIGHT => {
                doc.motion(if ctrl { Motion::WordRight } else { Motion::Right }, shift);
                None
            }
            keys::UP if alt => command(Command::MoveLineUp),
            keys::DOWN if alt => command(Command::MoveLineDown),
            keys::UP | keys::DOWN if ctrl => {
                self.scroll_y += if k.code == keys::UP { -self.lh } else { self.lh };
                return true;
            }
            keys::UP => {
                self.vertical(doc, wrap, -1, shift);
                None
            }
            keys::DOWN => {
                self.vertical(doc, wrap, 1, shift);
                None
            }
            keys::PAGEUP => {
                self.vertical(doc, wrap, -page, shift);
                self.scroll_y -= page as i32 * self.lh;
                None
            }
            keys::PAGEDOWN => {
                self.vertical(doc, wrap, page, shift);
                self.scroll_y += page as i32 * self.lh;
                None
            }
            keys::HOME if ctrl => {
                doc.motion(Motion::DocStart, shift);
                None
            }
            keys::END if ctrl => {
                doc.motion(Motion::DocEnd, shift);
                None
            }
            keys::HOME => {
                let layout = self.ensure_layout(doc, wrap);
                let cursor = doc.cursor();
                if layout.is_continuation(layout.row_of(cursor)) {
                    let p = layout.row_start(cursor);
                    doc.set_cursor(p, shift);
                    doc.goal_x = None;
                } else {
                    doc.motion(Motion::LineStart, shift);
                }
                None
            }
            keys::END => {
                let layout = self.ensure_layout(doc, wrap);
                let p = layout.row_end(doc.buffer(), doc.cursor());
                doc.set_cursor(p, shift);
                doc.goal_x = None;
                None
            }
            keys::BACKSPACE if edit => {
                doc.backspace(ctrl);
                None
            }
            keys::DELETE if edit => {
                doc.delete_forward(ctrl);
                None
            }
            keys::ENTER | keys::KPENTER if edit && !ctrl && !alt => {
                doc.newline();
                None
            }
            keys::TAB if !ctrl && !alt => command(if shift { Command::Outdent } else { Command::Indent }),
            keys::A if ctrl => command(Command::SelectAll),
            keys::C if ctrl => command(Command::Copy),
            keys::X if ctrl => command(Command::Cut),
            keys::V if ctrl => command(Command::Paste),
            keys::Z if ctrl && shift => command(Command::Redo),
            keys::Z if ctrl => command(Command::Undo),
            keys::Y if ctrl => command(Command::Redo),
            keys::SLASH if ctrl => command(Command::ToggleComment),
            keys::L if ctrl => {
                let sel = doc.selection();
                let (first, last) = sel.lines();
                let end = if last + 1 < doc.buffer().line_count() {
                    Pos::new(last + 1, 0)
                } else {
                    doc.buffer().line_end(last)
                };
                doc.set_selection(Selection::new(Pos::new(first, 0), end));
                None
            }
            _ => match k.ch {
                Some(ch) if ch != '\t' && edit && !ctrl && !alt => {
                    let mut buf = [0u8; 4];
                    doc.type_text(ch.encode_utf8(&mut buf));
                    None
                }
                _ => return false,
            },
        };
        if let Some(c) = cmd {
            self.command(ui, doc, lang, read_only, c);
        }
        self.reveal = true;
        self.blink_start = ui.now();
        true
    }

    /// Computes highlighter states up to the start of `line`.
    fn states_until(&mut self, lang: Language, doc: &Document, line: usize) {
        if self.hl_lang != lang {
            self.hl_states.clear();
            self.hl_lang = lang;
        }
        if self.hl_states.is_empty() {
            self.hl_states.push(State::default());
        }
        let buf = doc.buffer();
        let line = line.min(buf.line_count() - 1);
        let mut scratch = Vec::new();
        while self.hl_states.len() <= line {
            let i = self.hl_states.len() - 1;
            scratch.clear();
            let next = highlight::highlight_line(lang, buf.line(i), self.hl_states[i], &mut scratch);
            self.hl_states.push(next);
        }
    }

    /// Draws the view in `r` and handles its input. `matches` are search
    /// results to highlight (sorted).
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        ui: &mut Ui,
        r: Rect,
        doc: &mut Document,
        lang: Language,
        opts: ViewOptions,
        read_only: bool,
        matches: &[(Pos, Pos)],
    ) {
        let t = ui.theme().clone();
        let id = TextView::id(ui);
        let mono = ui.ctx.font(Font::Mono);
        let size = opts.font_size;
        let cw = ui.ctx.text.measure(mono, size, "0").max(1.0);
        let lh = (size * 1.6).round() as i32;
        let metrics = ui.ctx.text.metrics(mono, size);
        let baseline = ((lh as f32 - (metrics.ascent + metrics.descent)) / 2.0 + metrics.ascent).round() as i32;
        let gutter = if opts.line_numbers {
            (digits(doc.buffer().line_count()).max(3) as f32 * cw).ceil() as i32 + 34
        } else {
            14
        };
        let area = Rect::new(r.x, r.y, r.w - SCROLLBAR, r.h);
        let text = Rect::new(r.x + gutter, r.y, area.w - gutter, r.h);
        let wrap = opts.wrap.then(|| (((text.w - PAD - 12) as f32 / cw) as usize).max(16));
        self.lh = lh;
        self.page_rows = (area.h / lh - 1).max(1) as usize;
        if let Some(line) = doc.take_changed_from() {
            self.hl_states.truncate(line + 1);
        }
        self.ensure_layout(doc, wrap);
        let g = Geometry { area, origin_x: text.x + PAD - self.scroll_x, lh, cw };

        // ---- pointer ----
        let resp = ui.interact(id, area);
        if resp.pressed || resp.right_clicked {
            ui.focus(id);
        }
        if resp.hovered {
            ui.set_cursor(if ui.input.pointer.is_some_and(|(x, _)| x >= text.x) {
                Cursor::Text
            } else {
                Cursor::Arrow
            });
        }
        let shift = ui.input.shift();
        if resp.pressed {
            if let Some((x, y)) = ui.input.pointer {
                let p = self.hit(doc, &g, x, y);
                let unit = if x < text.x || ui.input.clicks >= 3 {
                    DragUnit::Line
                } else if ui.input.clicks == 2 {
                    DragUnit::Word
                } else {
                    DragUnit::Char
                };
                match unit {
                    DragUnit::Char => doc.set_cursor(p, shift),
                    DragUnit::Word => doc.select_word(p),
                    DragUnit::Line => doc.select_line(p.line),
                }
                doc.goal_x = None;
                self.drag = Some((unit, doc.selection()));
                self.blink_start = ui.now();
            }
        } else if resp.held
            && let (Some((unit, origin)), Some((x, y))) = (self.drag, ui.input.pointer)
        {
            // Keep scrolling while the pointer is above or below the view.
            if y < area.y || y >= area.bottom() {
                self.scroll_y += if y < area.y { -lh } else { lh };
                let now = ui.now();
                ui.repaint_at(now + 40_000_000);
            }
            let p = self.hit(doc, &g, x, y.clamp(area.y, area.bottom() - 1));
            let buf = doc.buffer();
            let sel = match unit {
                DragUnit::Char => Selection::new(origin.anchor, p),
                DragUnit::Word => {
                    let (a, b) = buf.word_at(p);
                    if p < origin.start() {
                        Selection::new(origin.end(), a)
                    } else {
                        Selection::new(origin.start(), b.max(origin.end()))
                    }
                }
                DragUnit::Line => {
                    let next =
                        if p.line + 1 < buf.line_count() { Pos::new(p.line + 1, 0) } else { buf.line_end(p.line) };
                    if p < origin.start() {
                        Selection::new(origin.end(), Pos::new(p.line, 0))
                    } else {
                        Selection::new(origin.start(), next.max(origin.end()))
                    }
                }
            };
            doc.set_selection(sel);
        }
        if !ui.input.down[0] {
            self.drag = None;
        }

        // Context menu.
        if resp.right_clicked
            && let Some((x, y)) = ui.input.pointer
        {
            let p = self.hit(doc, &g, x, y);
            let sel = doc.selection();
            if sel.is_empty() || p < sel.start() || p > sel.end() {
                doc.set_cursor(p, false);
            }
            ui.open_context_menu("text-menu", x, y);
        }
        let has_sel = !doc.selection().is_empty();
        let edit = !read_only;
        let menu = [
            MenuItem::new("Undo").shortcut("Ctrl+Z").enabled(edit && doc.can_undo()),
            MenuItem::new("Redo").shortcut("Ctrl+Y").enabled(edit && doc.can_redo()),
            MenuItem::separator(),
            MenuItem::new("Cut").shortcut("Ctrl+X").enabled(edit && has_sel),
            MenuItem::new("Copy").shortcut("Ctrl+C").enabled(has_sel),
            MenuItem::new("Paste").shortcut("Ctrl+V").enabled(edit),
            MenuItem::separator(),
            MenuItem::new("Select All").shortcut("Ctrl+A"),
        ];
        let chosen = ui.context_menu("text-menu", &menu).and_then(|i| match i {
            0 => Some(Command::Undo),
            1 => Some(Command::Redo),
            3 => Some(Command::Cut),
            4 => Some(Command::Copy),
            5 => Some(Command::Paste),
            7 => Some(Command::SelectAll),
            _ => None,
        });
        if let Some(c) = chosen {
            self.command(ui, doc, lang, read_only, c);
        }

        // Wheel.
        if ui.hovered(r) {
            let (dx, dy) = ui.input.scroll;
            if dy != 0 {
                if shift && !opts.wrap {
                    self.scroll_x -= dy * 3 * cw as i32;
                } else {
                    self.scroll_y -= dy * 3 * lh;
                }
            }
            if dx != 0 && !opts.wrap {
                self.scroll_x -= dx * 3 * cw as i32;
            }
        }

        // ---- keyboard ----
        if ui.focused(id) {
            for k in ui.input.keys.clone() {
                self.key(ui, doc, lang, read_only, wrap, &k);
            }
        }

        // ---- scrolling ----
        let layout = self.ensure_layout(doc, wrap);
        let rows = layout.row_count() as i32;
        let cursor = doc.cursor();
        let cursor_row = layout.row_of(cursor) as i32;
        let cursor_x = layout.x_of(doc.buffer(), cursor);
        let max_width = layout.max_width();
        let visible_w = text.w - PAD - 12;
        if self.reveal {
            let margin = (lh * 2).min(area.h / 4);
            let y = cursor_row * lh;
            if y < self.scroll_y + margin {
                self.scroll_y = y - margin;
            } else if y + lh > self.scroll_y + area.h - margin {
                self.scroll_y = y + lh - area.h + margin;
            }
            if !opts.wrap {
                let x = (cursor_x as f32 * cw) as i32;
                let m = (cw * 4.0) as i32;
                if x < self.scroll_x + m {
                    self.scroll_x = x - m;
                } else if x > self.scroll_x + visible_w - m {
                    self.scroll_x = x - visible_w + m;
                }
            }
            self.reveal = false;
        }
        let content_h = rows * lh;
        let max_scroll = (content_h - area.h / 2).max(0);
        self.scroll_y = self.scroll_y.clamp(0, max_scroll);
        let max_x = if opts.wrap { 0 } else { ((max_width as f32 * cw) as i32 - visible_w + (cw * 4.0) as i32).max(0) };
        self.scroll_x = self.scroll_x.clamp(0, max_x);

        // Scrollbar.
        let track = Rect::new(r.right() - SCROLLBAR, r.y, SCROLLBAR, r.h);
        let total = (max_scroll + area.h).max(1);
        if max_scroll > 0 {
            let thumb_h = (area.h * area.h / total).max(32).min(track.h);
            let thumb_y = track.y + ((self.scroll_y as i64 * (track.h - thumb_h) as i64) / max_scroll as i64) as i32;
            let thumb = Rect::new(track.x + 2, thumb_y, SCROLLBAR - 4, thumb_h);
            let bar_id = id ^ 0x5c2;
            let bar = ui.interact(bar_id, track);
            if bar.pressed
                && let Some((_, py)) = ui.input.pointer
            {
                self.scrollbar_grab = Some(if thumb.contains(thumb.x, py) { py - thumb_y } else { thumb_h / 2 });
            }
            if bar.held {
                if let (Some(grab), Some((_, py))) = (self.scrollbar_grab, ui.input.pointer) {
                    let f = (py - grab - track.y) as f32 / (track.h - thumb_h).max(1) as f32;
                    self.scroll_y = (f.clamp(0.0, 1.0) * max_scroll as f32) as i32;
                }
            } else {
                self.scrollbar_grab = None;
            }
            let a = if bar.held {
                110
            } else if bar.hovered {
                80
            } else {
                48
            };
            ui.canvas.fill_rounded_rect(thumb, 4.0, Color::rgba(255, 255, 255, a));
            // Cursor and match markers on the track.
            let mark_y = |row: i32| track.y + ((row * lh) as i64 * (track.h - 3) as i64 / total as i64) as i32;
            let layout = self.layout.as_ref().unwrap();
            for &(a, _) in matches.iter().take(2000) {
                ui.canvas.fill_rect(
                    Rect::new(track.x + 2, mark_y(layout.row_of(a) as i32), SCROLLBAR - 4, 2),
                    Color::rgba(245, 180, 60, 200),
                );
            }
            ui.canvas.fill_rect(Rect::new(track.x, mark_y(cursor_row), SCROLLBAR, 2), t.accent);
        }

        // ---- drawing ----
        let sel = doc.selection();
        if (sel, doc.revision()) != self.last_seen {
            self.last_seen = (sel, doc.revision());
            self.blink_start = ui.now();
        }
        let origin_x = text.x + PAD - self.scroll_x;
        let first_row = (self.scroll_y / lh).max(0) as usize;
        let last_row = (((self.scroll_y + area.h) / lh + 1) as usize).min(rows as usize);
        let last_line = self.layout.as_ref().unwrap().row(last_row.saturating_sub(1)).line;
        self.states_until(lang, doc, last_line);
        let layout = self.layout.as_ref().unwrap();
        let buf = doc.buffer();
        let (s0, s1) = (sel.start(), sel.end());
        let focused = ui.focused(id);
        let text_color = Color::hex(0xD7DAE0);
        ui.canvas.save();
        ui.canvas.clip_to(area);
        let mut spans: Vec<Span> = Vec::new();
        let mut spans_line = usize::MAX;
        let mut expanded = String::new();
        let first_match = matches.partition_point(|&(_, b)| b.line < layout.row(first_row).line);
        for ri in first_row..last_row {
            let row = layout.row(ri);
            let y = r.y + ri as i32 * lh - self.scroll_y;
            let line = buf.line(row.line);
            // Cells from the row start to byte `col` of the line.
            let cells = |col: usize| {
                let mut x = 0;
                for c in line[row.start..col.clamp(row.start, row.end)].chars() {
                    x += char_width(c, x, doc.tab_width);
                }
                x
            };
            if row.line == sel.cursor.line && sel.is_empty() {
                ui.canvas.fill_rect(Rect::new(text.x, y, text.w, lh), Color::rgba(255, 255, 255, 8));
            }
            if opts.line_numbers && row.start == 0 {
                let n = alloc::format!("{}", row.line + 1);
                let c = if row.line == sel.cursor.line { t.text_dim } else { t.text_faint.with_alpha(170) };
                let nr = Rect::new(r.x, y, gutter - 18, lh);
                ui.label(nr, &n, Font::Mono, size - 1.0, c, Align::Right);
            }
            // Search matches.
            for &(a, b) in &matches[first_match.min(matches.len())..] {
                if a.line > row.line {
                    break;
                }
                if a.line == row.line && b.col > row.start && a.col < row.end.max(row.start + 1) {
                    let x0 = origin_x + (cells(a.col) as f32 * cw) as i32;
                    let x1 = origin_x + (cells(b.col) as f32 * cw) as i32;
                    let mr = Rect::new(x0, y + 2, (x1 - x0).max(2), lh - 4);
                    ui.canvas.fill_rounded_rect(mr, 3.0, Color::rgba(245, 180, 60, 60));
                    ui.canvas.stroke_rounded_rect(mr, 3.0, 1.0, Color::rgba(245, 180, 60, 150));
                }
            }
            // Selection.
            if !sel.is_empty() && (s0.line..=s1.line).contains(&row.line) {
                // Selected bytes of this line; `usize::MAX` = through the line break.
                let a = if row.line == s0.line { s0.col } else { 0 };
                let b = if row.line == s1.line { s1.col } else { usize::MAX };
                let (seg_a, seg_b) = (a.max(row.start), b.min(row.end));
                if seg_a <= seg_b {
                    let last_of_line = ri + 1 >= layout.row_count() || layout.row(ri + 1).line != row.line;
                    // A selected line break shows as one extra cell.
                    let nl = usize::from(b == usize::MAX && last_of_line);
                    let (x0, x1) = (cells(seg_a), cells(seg_b) + nl);
                    if x1 > x0 {
                        let color = if focused { t.selection } else { Color::rgba(255, 255, 255, 26) };
                        let x = origin_x + (x0 as f32 * cw) as i32;
                        ui.canvas.fill_rect(Rect::new(x, y, ((x1 - x0) as f32 * cw) as i32, lh), color);
                    }
                }
            }
            // Text.
            if spans_line != row.line {
                spans.clear();
                let st = self.hl_states.get(row.line).copied().unwrap_or_default();
                highlight::highlight_line(lang, line, st, &mut spans);
                spans_line = row.line;
            }
            for s in &spans {
                let (a, b) = (s.start.max(row.start), s.end.min(row.end));
                if a >= b {
                    continue;
                }
                let x0 = cells(a);
                let seg = &line[a..b];
                let seg = if seg.contains('\t') {
                    expanded.clear();
                    let mut x = x0;
                    for c in seg.chars() {
                        let w = char_width(c, x, doc.tab_width);
                        if c == '\t' {
                            expanded.extend(core::iter::repeat_n(' ', w));
                        } else {
                            expanded.push(c);
                        }
                        x += w;
                    }
                    expanded.as_str()
                } else {
                    seg
                };
                let font = if s.token == Token::Heading { ui.ctx.font(Font::MonoBold) } else { mono };
                let color = token_color(s.token, text_color);
                ui.ctx.text.draw(
                    &mut ui.canvas,
                    font,
                    size,
                    (origin_x as f32 + x0 as f32 * cw).round(),
                    (y + baseline) as f32,
                    seg,
                    color,
                );
            }
        }
        // Cursor.
        if focused {
            let elapsed = ui.now().saturating_sub(self.blink_start);
            if (elapsed / BLINK_NS).is_multiple_of(2) && (first_row as i32..last_row as i32).contains(&cursor_row) {
                let x = origin_x + (cursor_x as f32 * cw) as i32;
                let y = r.y + cursor_row * lh - self.scroll_y;
                ui.canvas.fill_rect(Rect::new(x - 1, y + 2, 2, lh - 4), t.accent_hover);
            }
            let next = self.blink_start + (elapsed / BLINK_NS + 1) * BLINK_NS;
            ui.repaint_at(next);
        }
        ui.canvas.restore();
        // A hairline between the gutter and the text when scrolled sideways.
        if self.scroll_x > 0 {
            ui.canvas.fill_rect(Rect::new(text.x, r.y, 1, r.h), Color::rgba(0, 0, 0, 120));
        }
    }
}
