//! Single-line text input.

use alloc::string::String;

use vgfx::{Color, Rect};
use vproto::display::Cursor;
use vproto::input::keys;

use crate::theme::Font;
use crate::ui::{TextEditState, Ui};

/// What happened to a text input this frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextInputResponse {
    pub changed: bool,
    /// Enter was pressed.
    pub submitted: bool,
    pub focused: bool,
}

fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(p, _)| p).unwrap_or(0)
}

fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
}

fn word_left(s: &str, mut i: usize) -> usize {
    while i > 0 && s[..i].ends_with(' ') {
        i = prev_boundary(s, i);
    }
    while i > 0 && !s[..i].ends_with(' ') {
        i = prev_boundary(s, i);
    }
    i
}

fn word_right(s: &str, mut i: usize) -> usize {
    while i < s.len() && s[i..].starts_with(' ') {
        i = next_boundary(s, i);
    }
    while i < s.len() && !s[i..].starts_with(' ') {
        i = next_boundary(s, i);
    }
    i
}

impl<'a> Ui<'a> {
    /// A text box editing `value`. Clicking focuses it; typing, arrows,
    /// Home/End, Backspace/Delete, Shift-selection, Ctrl+A/C/X/V work.
    pub fn text_input(&mut self, r: Rect, id_label: &str, value: &mut String, placeholder: &str) -> TextInputResponse {
        let id = self.id(id_label) ^ 0x7e47;
        let t = self.ctx.theme.clone();
        let size = t.font_size;
        let font = self.ctx.font(Font::Regular);
        let resp = self.interact(id, r);
        let mut st = self.ctx.state.edits.get(&id).copied().unwrap_or_default();
        st.cursor = st.cursor.min(value.len());
        st.anchor = st.anchor.min(value.len());
        let pad = 10;
        let text_x = r.x + pad;
        if resp.pressed {
            self.focus(id);
            if let Some((px, _)) = self.input.pointer {
                let idx = self.ctx.text.index_for_x(font, size, value, (px - text_x) as f32 + st.scroll);
                st.cursor = idx;
                if !self.input.shift() {
                    st.anchor = idx;
                }
                if resp.double_clicked {
                    st.anchor = word_left(value, idx);
                    st.cursor = word_right(value, idx);
                }
            }
            st.blink_start = self.now();
        } else if resp.held {
            if let Some((px, _)) = self.input.pointer {
                st.cursor = self.ctx.text.index_for_x(font, size, value, (px - text_x) as f32 + st.scroll);
            }
        }
        let focused = self.focused(id);
        let mut out = TextInputResponse { focused, ..Default::default() };
        if focused {
            let shift = self.input.shift();
            let ctrl = self.input.ctrl();
            let sel = |st: &TextEditState| (st.cursor.min(st.anchor), st.cursor.max(st.anchor));
            for k in self.input.keys.clone() {
                let (s0, s1) = sel(&st);
                let mut moved = None;
                match k.code {
                    keys::LEFT => moved = Some(if ctrl { word_left(value, st.cursor) } else if s0 != s1 && !shift { s0 } else { prev_boundary(value, st.cursor) }),
                    keys::RIGHT => moved = Some(if ctrl { word_right(value, st.cursor) } else if s0 != s1 && !shift { s1 } else { next_boundary(value, st.cursor) }),
                    keys::HOME => moved = Some(0),
                    keys::END => moved = Some(value.len()),
                    keys::BACKSPACE => {
                        if s0 != s1 {
                            value.replace_range(s0..s1, "");
                            st.cursor = s0;
                        } else if st.cursor > 0 {
                            let p = if ctrl { word_left(value, st.cursor) } else { prev_boundary(value, st.cursor) };
                            value.replace_range(p..st.cursor, "");
                            st.cursor = p;
                        }
                        st.anchor = st.cursor;
                        out.changed = true;
                    }
                    keys::DELETE => {
                        if s0 != s1 {
                            value.replace_range(s0..s1, "");
                            st.cursor = s0;
                        } else if st.cursor < value.len() {
                            let n = next_boundary(value, st.cursor);
                            value.replace_range(st.cursor..n, "");
                        }
                        st.anchor = st.cursor;
                        out.changed = true;
                    }
                    keys::ENTER | keys::KPENTER => out.submitted = true,
                    keys::A if ctrl => {
                        st.anchor = 0;
                        st.cursor = value.len();
                    }
                    keys::C | keys::X if ctrl => {
                        if s0 != s1 {
                            let text = String::from(&value[s0..s1]);
                            self.set_clipboard(&text);
                            if k.code == keys::X {
                                value.replace_range(s0..s1, "");
                                st.cursor = s0;
                                st.anchor = s0;
                                out.changed = true;
                            }
                        }
                    }
                    keys::V if ctrl => {
                        let clip = self.clipboard();
                        let clip: String = clip.chars().filter(|c| !c.is_control()).collect();
                        value.replace_range(s0..s1, &clip);
                        st.cursor = s0 + clip.len();
                        st.anchor = st.cursor;
                        out.changed = true;
                    }
                    _ => {}
                }
                if let Some(m) = moved {
                    st.cursor = m;
                    if !shift {
                        st.anchor = m;
                    }
                }
                st.blink_start = self.now();
            }
            if !self.input.text.is_empty() {
                let (s0, s1) = sel(&st);
                let typed: String = self.input.text.chars().filter(|c| *c != '\n' && *c != '\t').collect();
                if !typed.is_empty() {
                    value.replace_range(s0..s1, &typed);
                    st.cursor = s0 + typed.len();
                    st.anchor = st.cursor;
                    out.changed = true;
                    st.blink_start = self.now();
                }
            }
            if self.input.key(keys::ESC) {
                self.ctx.state.focus = None;
            }
        }

        // Keep the cursor visible.
        let inner_w = (r.w - 2 * pad) as f32;
        let cx = self.ctx.text.x_for_index(font, size, value, st.cursor);
        if cx - st.scroll > inner_w {
            st.scroll = cx - inner_w;
        } else if cx < st.scroll {
            st.scroll = cx;
        }
        st.scroll = st.scroll.max(0.0);

        // Draw.
        let bg = if focused { t.bg } else if resp.hovered { t.control_hover } else { t.control };
        self.canvas.fill_rounded_rect(r, t.radius, bg);
        let border = if focused { t.accent } else { t.border };
        self.canvas.stroke_rounded_rect(r, t.radius, if focused { 1.5 } else { 1.0 }, border);
        let m = self.ctx.text.metrics(font, size);
        let baseline = r.y as f32 + (r.h as f32 - (m.ascent + m.descent)) / 2.0 + m.ascent;
        self.canvas.save();
        self.canvas.clip_to(r.inset(pad - 2, 2, pad - 2, 2));
        let (s0, s1) = (st.cursor.min(st.anchor), st.cursor.max(st.anchor));
        if focused && s0 != s1 {
            let x0 = self.ctx.text.x_for_index(font, size, value, s0) - st.scroll;
            let x1 = self.ctx.text.x_for_index(font, size, value, s1) - st.scroll;
            let sel_rect = Rect::new(text_x + x0 as i32, r.y + 5, (x1 - x0) as i32, r.h - 10);
            self.canvas.fill_rect(sel_rect, t.selection);
        }
        if value.is_empty() && !placeholder.is_empty() {
            self.ctx.text.draw(&mut self.canvas, font, size, text_x as f32, baseline.floor_px(), placeholder, t.text_faint);
        } else {
            self.ctx.text.draw(&mut self.canvas, font, size, text_x as f32 - st.scroll, baseline.floor_px(), value, t.text);
        }
        if focused {
            let phase = ((self.now() - st.blink_start) / 530_000_000) % 2;
            if phase == 0 {
                let x = text_x + (cx - st.scroll) as i32;
                self.canvas.fill_rect(Rect::new(x, r.y + 7, 2, r.h - 14), Color::hex(0xFFFFFF));
            }
            let next = st.blink_start + (((self.now() - st.blink_start) / 530_000_000) + 1) * 530_000_000;
            self.repaint_at(next);
        }
        self.canvas.restore();
        if resp.hovered {
            self.set_cursor(Cursor::Text);
        }
        self.ctx.state.edits.insert(id, st);
        out
    }
}

trait FloorPx {
    fn floor_px(self) -> f32;
}

impl FloorPx for f32 {
    fn floor_px(self) -> f32 {
        vmath::FloatExt::floor(self + 0.5)
    }
}
