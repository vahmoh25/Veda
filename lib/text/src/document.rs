//! [`Document`]: a buffer with a selection, editing commands and grouped
//! undo/redo.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::buffer::{Buffer, Pos, end_of, ordered};
use crate::layout::display_col;

/// A selection: `anchor` stays put while `cursor` moves. Empty selections
/// are plain carets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: Pos,
    pub cursor: Pos,
}

impl Selection {
    pub const fn new(anchor: Pos, cursor: Pos) -> Selection {
        Selection { anchor, cursor }
    }

    pub const fn caret(p: Pos) -> Selection {
        Selection { anchor: p, cursor: p }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    pub fn start(&self) -> Pos {
        ordered(self.anchor, self.cursor).0
    }

    pub fn end(&self) -> Pos {
        ordered(self.anchor, self.cursor).1
    }

    /// First and last line touched. A selection that ends at the very start
    /// of a line does not include that line (as when selecting whole lines).
    pub fn lines(&self) -> (usize, usize) {
        let (a, b) = (self.start(), self.end());
        let last = if b.line > a.line && b.col == 0 { b.line - 1 } else { b.line };
        (a.line, last)
    }
}

/// Cursor movements that do not depend on the visual layout (vertical
/// movement is in [`crate::Layout`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    /// First non-blank character, or column 0 if already there.
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

/// What kind of step an undo group is; consecutive typing (or deleting)
/// steps merge into one group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Typing,
    Deleting,
    Other,
}

/// One primitive change: `removed` at `at` was replaced by `inserted`.
#[derive(Debug, Clone)]
struct Change {
    at: Pos,
    removed: String,
    inserted: String,
}

/// Changes undone and redone together.
#[derive(Debug, Clone)]
struct Group {
    changes: Vec<Change>,
    kind: Kind,
    before: Selection,
    after: Selection,
    state_before: u64,
    state_after: u64,
}

/// Maximum number of undo steps kept.
const UNDO_LIMIT: usize = 1000;

/// Adjusts a position on `line` for the replacement of `a..b` by `len` bytes.
fn adjust(p: Pos, line: usize, a: usize, b: usize, len: usize) -> Pos {
    if p.line != line || p.col <= a {
        p
    } else if p.col >= b {
        Pos::new(line, p.col - (b - a) + len)
    } else {
        Pos::new(line, a + len.min(p.col - a))
    }
}

/// An editable text document.
#[derive(Debug, Clone)]
pub struct Document {
    buffer: Buffer,
    sel: Selection,
    /// Display column kept while moving vertically (managed by the view).
    pub goal_x: Option<usize>,
    /// Spaces per indentation level and tab stop.
    pub tab_width: usize,
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Identity of the current content: new for every edit, restored by
    /// undo/redo, so undoing back to the saved text clears "modified".
    state: u64,
    next_state: u64,
    saved_state: u64,
    revision: u64,
    /// First line changed since [`Document::take_changed_from`] (`usize::MAX`: none).
    changed_from: usize,
    /// The last undo group may absorb the next typing/deleting step.
    merge_open: bool,
}

impl Default for Document {
    fn default() -> Self {
        Document::new()
    }
}

impl Document {
    pub fn new() -> Document {
        Document::from_buffer(Buffer::new())
    }

    pub fn from_text(text: &str) -> Document {
        Document::from_buffer(Buffer::from_text(text))
    }

    fn from_buffer(buffer: Buffer) -> Document {
        Document {
            buffer,
            sel: Selection::default(),
            goal_x: None,
            tab_width: 4,
            undo: Vec::new(),
            redo: Vec::new(),
            state: 0,
            next_state: 1,
            saved_state: 0,
            revision: 0,
            changed_from: usize::MAX,
            merge_open: false,
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn selection(&self) -> Selection {
        self.sel
    }

    pub fn cursor(&self) -> Pos {
        self.sel.cursor
    }

    /// Changes whenever the content changes (for caches such as layout).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The first line changed since the previous call, if any (for caches
    /// that are valid up to a line, such as highlighting state).
    pub fn take_changed_from(&mut self) -> Option<usize> {
        let line = core::mem::replace(&mut self.changed_from, usize::MAX);
        (line != usize::MAX).then_some(line)
    }

    /// The content differs from what was last saved.
    pub fn is_modified(&self) -> bool {
        self.state != self.saved_state
    }

    pub fn mark_saved(&mut self) {
        self.saved_state = self.state;
        self.merge_open = false;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn selected_text(&self) -> String {
        self.buffer.slice(self.sel.start(), self.sel.end())
    }

    // ---- selection ---------------------------------------------------------

    pub fn set_selection(&mut self, sel: Selection) {
        self.sel = Selection::new(self.buffer.clamp(sel.anchor), self.buffer.clamp(sel.cursor));
        self.merge_open = false;
    }

    /// Moves the cursor to `p`, extending the selection if `extend`.
    pub fn set_cursor(&mut self, p: Pos, extend: bool) {
        let p = self.buffer.clamp(p);
        self.sel = if extend { Selection::new(self.sel.anchor, p) } else { Selection::caret(p) };
        self.merge_open = false;
    }

    pub fn select_all(&mut self) {
        self.set_selection(Selection::new(Pos::new(0, 0), self.buffer.end()));
    }

    /// Selects the word around `p`.
    pub fn select_word(&mut self, p: Pos) {
        let (a, b) = self.buffer.word_at(p);
        self.set_selection(Selection::new(a, b));
    }

    /// Selects a whole line including its line break.
    pub fn select_line(&mut self, line: usize) {
        let line = line.min(self.buffer.line_count() - 1);
        let end = if line + 1 < self.buffer.line_count() { Pos::new(line + 1, 0) } else { self.buffer.line_end(line) };
        self.set_selection(Selection::new(Pos::new(line, 0), end));
    }

    pub fn motion(&mut self, m: Motion, extend: bool) {
        let c = self.sel.cursor;
        let b = &self.buffer;
        let target = match m {
            Motion::Left if !extend && !self.sel.is_empty() => self.sel.start(),
            Motion::Right if !extend && !self.sel.is_empty() => self.sel.end(),
            Motion::Left => b.prev(c),
            Motion::Right => b.next(c),
            Motion::WordLeft => b.word_left(c),
            Motion::WordRight => b.word_right(c),
            Motion::LineStart => {
                let line = b.line(c.line);
                let indent = line.len() - line.trim_start().len();
                Pos::new(c.line, if c.col == indent { 0 } else { indent })
            }
            Motion::LineEnd => b.line_end(c.line),
            Motion::DocStart => Pos::new(0, 0),
            Motion::DocEnd => b.end(),
        };
        self.goal_x = None;
        self.set_cursor(target, extend);
    }

    /// Moves the cursor to the start of 1-based line `n`.
    pub fn goto_line(&mut self, n: usize) {
        self.goal_x = None;
        self.set_cursor(Pos::new(n.saturating_sub(1).min(self.buffer.line_count() - 1), 0), false);
    }

    // ---- editing primitives -----------------------------------------------

    /// Replaces `a..b` with `text` and returns the change.
    fn apply(&mut self, a: Pos, b: Pos, text: &str) -> Change {
        let (a, b) = ordered(a, b);
        let removed = if a == b { String::new() } else { self.buffer.delete(a, b) };
        let inserted = if text.contains('\r') { text.replace("\r\n", "\n") } else { text.to_string() };
        if !inserted.is_empty() {
            self.buffer.insert(a, &inserted);
        }
        self.revision += 1;
        self.changed_from = self.changed_from.min(a.line);
        Change { at: a, removed, inserted }
    }

    /// Records changes as an undo step, merging consecutive typing or
    /// deleting steps.
    fn record(&mut self, changes: Vec<Change>, kind: Kind, before: Selection) {
        let state_before = self.state;
        self.state = self.next_state;
        self.next_state += 1;
        self.redo.clear();
        if self.merge_open
            && kind != Kind::Other
            && let Some(g) = self.undo.last_mut()
            && g.kind == kind
            && g.after == before
            && g.state_after == state_before
        {
            g.changes.extend(changes);
            g.after = self.sel;
            g.state_after = self.state;
            return;
        }
        self.undo.push(Group { changes, kind, before, after: self.sel, state_before, state_after: self.state });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.merge_open = kind != Kind::Other;
    }

    fn replace_selection(&mut self, text: &str, kind: Kind) {
        let before = self.sel;
        let a = before.start();
        let change = self.apply(a, before.end(), text);
        self.sel = Selection::caret(end_of(a, &change.inserted));
        self.goal_x = None;
        self.record(vec![change], kind, before);
    }

    fn delete_range(&mut self, a: Pos, b: Pos, kind: Kind) {
        let before = self.sel;
        let change = self.apply(a, b, "");
        self.sel = Selection::caret(ordered(a, b).0);
        self.goal_x = None;
        self.record(vec![change], kind, before);
    }

    /// Applies `f` to each line in `first..=last`; `f` returns the byte
    /// range to replace and its replacement. All changes form one undo step
    /// and the selection follows the text.
    fn edit_lines(&mut self, first: usize, last: usize, mut f: impl FnMut(&str) -> Option<(usize, usize, String)>) {
        let before = self.sel;
        let (mut anchor, mut cursor) = (self.sel.anchor, self.sel.cursor);
        let mut changes = Vec::new();
        for line in first..=last {
            let Some((a, b, rep)) = f(self.buffer.line(line)) else { continue };
            changes.push(self.apply(Pos::new(line, a), Pos::new(line, b), &rep));
            anchor = adjust(anchor, line, a, b, rep.len());
            cursor = adjust(cursor, line, a, b, rep.len());
        }
        if changes.is_empty() {
            return;
        }
        self.sel = Selection::new(self.buffer.clamp(anchor), self.buffer.clamp(cursor));
        self.goal_x = None;
        self.record(changes, Kind::Other, before);
    }

    // ---- editing commands --------------------------------------------------

    /// Typed text replaces the selection. Consecutive typing is undone as
    /// one step per word.
    pub fn type_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let starts_word_break = text.starts_with(char::is_whitespace);
        if starts_word_break && !self.buffer.char_before(self.sel.start()).is_some_and(char::is_whitespace) {
            self.merge_open = false;
        }
        let kind = if self.sel.is_empty() { Kind::Typing } else { Kind::Other };
        self.replace_selection(text, kind);
    }

    /// Inserts text as a separate undo step (paste, replace).
    pub fn paste(&mut self, text: &str) {
        self.replace_selection(text, Kind::Other);
        self.merge_open = false;
    }

    /// Enter: a line break that keeps the current indentation, one level
    /// deeper after an opening bracket.
    pub fn newline(&mut self) {
        let start = self.sel.start();
        let line = self.buffer.line(start.line);
        let mut text = String::from("\n");
        text.extend(line[..start.col].chars().take_while(|c| *c == ' ' || *c == '\t'));
        if line[..start.col].trim_end().ends_with(['{', '[', '(']) {
            text.extend(core::iter::repeat_n(' ', self.tab_width));
        }
        self.type_text(&text);
    }

    /// Backspace: deletes the selection, or the character (or word) before
    /// the cursor. Inside leading spaces it deletes back to the previous
    /// indentation stop.
    pub fn backspace(&mut self, word: bool) {
        if !self.sel.is_empty() {
            return self.delete_selection();
        }
        let c = self.sel.cursor;
        let target = if word {
            self.buffer.word_left(c)
        } else if c.col > 0 && self.buffer.line(c.line)[..c.col].bytes().all(|b| b == b' ') {
            Pos::new(c.line, (c.col - 1) / self.tab_width * self.tab_width)
        } else {
            self.buffer.prev(c)
        };
        if target != c {
            // Word deletions are separate undo steps; single characters merge.
            self.delete_range(target, c, if word { Kind::Other } else { Kind::Deleting });
        }
    }

    /// Delete: deletes the selection, or the character (or word) after the
    /// cursor.
    pub fn delete_forward(&mut self, word: bool) {
        if !self.sel.is_empty() {
            return self.delete_selection();
        }
        let c = self.sel.cursor;
        let target = if word { self.buffer.word_right(c) } else { self.buffer.next(c) };
        if target != c {
            self.delete_range(c, target, if word { Kind::Other } else { Kind::Deleting });
        }
    }

    pub fn delete_selection(&mut self) {
        if !self.sel.is_empty() {
            self.delete_range(self.sel.start(), self.sel.end(), Kind::Other);
        }
    }

    /// Removes the selection and returns it (empty if nothing is selected).
    pub fn cut(&mut self) -> String {
        let text = self.selected_text();
        self.delete_selection();
        text
    }

    /// Tab: indents the selected lines, or inserts spaces up to the next
    /// tab stop.
    pub fn indent(&mut self) {
        if self.sel.anchor.line == self.sel.cursor.line {
            let c = self.sel.start();
            let x = display_col(self.buffer.line(c.line), c.col, self.tab_width);
            let n = self.tab_width - x % self.tab_width;
            self.type_text(&" ".repeat(n));
            return;
        }
        let (first, last) = self.sel.lines();
        let pad = " ".repeat(self.tab_width);
        self.edit_lines(first, last, |l| (!l.is_empty()).then(|| (0, 0, pad.clone())));
    }

    /// Shift+Tab: removes one indentation level from the selected lines.
    pub fn outdent(&mut self) {
        let (first, last) = self.sel.lines();
        let tw = self.tab_width;
        self.edit_lines(first, last, |l| {
            let n = if l.starts_with('\t') { 1 } else { l.bytes().take(tw).take_while(|&b| b == b' ').count() };
            (n > 0).then(String::new).map(|s| (0, n, s))
        });
    }

    /// Comments or uncomments the selected lines with a line-comment
    /// `prefix` such as `//` or `#`.
    pub fn toggle_comment(&mut self, prefix: &str) {
        let (first, last) = self.sel.lines();
        let blank = |l: &str| l.trim().is_empty();
        let lines: Vec<&str> = (first..=last).map(|i| self.buffer.line(i)).collect();
        let uncomment = lines.iter().filter(|l| !blank(l)).all(|l| l.trim_start().starts_with(prefix))
            && lines.iter().any(|l| !blank(l));
        let indent = lines.iter().filter(|l| !blank(l)).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        let insert = alloc::format!("{prefix} ");
        self.edit_lines(first, last, |l| {
            if blank(l) {
                None
            } else if uncomment {
                let i = l.len() - l.trim_start().len();
                let rest = &l[i + prefix.len()..];
                let n = prefix.len() + usize::from(rest.starts_with(' '));
                Some((i, i + n, String::new()))
            } else {
                Some((indent, indent, insert.clone()))
            }
        });
    }

    /// Moves the selected lines one line up or down.
    pub fn move_lines(&mut self, up: bool) {
        let (first, last) = self.sel.lines();
        if (up && first == 0) || (!up && last + 1 >= self.buffer.line_count()) {
            return;
        }
        let before = self.sel;
        let block = self.buffer.slice(Pos::new(first, 0), self.buffer.line_end(last));
        let (lo, hi, text) = if up {
            let other = self.buffer.line(first - 1);
            (first - 1, last, alloc::format!("{block}\n{other}"))
        } else {
            let other = self.buffer.line(last + 1);
            (first, last + 1, alloc::format!("{other}\n{block}"))
        };
        let change = self.apply(Pos::new(lo, 0), self.buffer.line_end(hi), &text);
        let shift = |p: Pos| Pos::new(if up { p.line - 1 } else { p.line + 1 }, p.col);
        self.sel = Selection::new(shift(before.anchor), shift(before.cursor));
        self.goal_x = None;
        self.record(vec![change], Kind::Other, before);
    }

    // ---- undo ----------------------------------------------------------------

    pub fn undo(&mut self) -> bool {
        let Some(g) = self.undo.pop() else { return false };
        for c in g.changes.iter().rev() {
            self.buffer.delete(c.at, end_of(c.at, &c.inserted));
            self.buffer.insert(c.at, &c.removed);
            self.changed_from = self.changed_from.min(c.at.line);
        }
        self.revision += 1;
        self.sel = g.before;
        self.state = g.state_before;
        self.goal_x = None;
        self.merge_open = false;
        self.redo.push(g);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(g) = self.redo.pop() else { return false };
        for c in &g.changes {
            self.buffer.delete(c.at, end_of(c.at, &c.removed));
            self.buffer.insert(c.at, &c.inserted);
            self.changed_from = self.changed_from.min(c.at.line);
        }
        self.revision += 1;
        self.sel = g.after;
        self.state = g.state_after;
        self.goal_x = None;
        self.merge_open = false;
        self.undo.push(g);
        true
    }

    // ---- search ----------------------------------------------------------------

    /// Selects the next match after the selection (wrapping around).
    pub fn find_next(&mut self, needle: &str, case_sensitive: bool) -> bool {
        match self.buffer.find(needle, self.sel.end(), case_sensitive) {
            Some((a, b)) => {
                self.set_selection(Selection::new(a, b));
                true
            }
            None => false,
        }
    }

    /// Selects the previous match before the selection (wrapping around).
    pub fn find_prev(&mut self, needle: &str, case_sensitive: bool) -> bool {
        match self.buffer.find_back(needle, self.sel.start(), case_sensitive) {
            Some((a, b)) => {
                self.set_selection(Selection::new(a, b));
                true
            }
            None => false,
        }
    }

    fn selection_matches(&self, needle: &str, case_sensitive: bool) -> bool {
        let t = self.selected_text();
        if case_sensitive { t == needle } else { t.eq_ignore_ascii_case(needle) }
    }

    /// Replaces the selection if it is a match, then selects the next match.
    pub fn replace_next(&mut self, needle: &str, replacement: &str, case_sensitive: bool) -> bool {
        if !self.sel.is_empty() && self.selection_matches(needle, case_sensitive) {
            self.paste(replacement);
        }
        self.find_next(needle, case_sensitive)
    }

    /// Replaces every match as one undo step; returns how many.
    pub fn replace_all(&mut self, needle: &str, replacement: &str, case_sensitive: bool) -> usize {
        let matches = self.buffer.find_all(needle, case_sensitive, usize::MAX);
        if matches.is_empty() {
            return 0;
        }
        let before = self.sel;
        let changes: Vec<Change> = matches.iter().rev().map(|&(a, b)| self.apply(a, b, replacement)).collect();
        self.sel = Selection::caret(self.buffer.clamp(before.cursor));
        self.goal_x = None;
        self.record(changes, Kind::Other, before);
        matches.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str, cursor: Pos) -> Document {
        let mut d = Document::from_text(text);
        d.set_cursor(cursor, false);
        d
    }

    #[test]
    fn typing_groups_by_word_and_undoes() {
        let mut d = Document::new();
        for c in "hello world".chars() {
            let mut buf = [0u8; 4];
            d.type_text(c.encode_utf8(&mut buf));
        }
        assert_eq!(d.text(), "hello world");
        assert!(d.is_modified());
        assert!(d.undo());
        assert_eq!(d.text(), "hello");
        assert!(d.undo());
        assert_eq!(d.text(), "");
        assert!(!d.is_modified());
        assert!(!d.undo());
        assert!(d.redo());
        assert!(d.redo());
        assert_eq!(d.text(), "hello world");
        assert_eq!(d.cursor(), Pos::new(0, 11));
    }

    #[test]
    fn modified_tracks_saved_state() {
        let mut d = doc("abc", Pos::new(0, 3));
        d.type_text("d");
        d.mark_saved();
        assert!(!d.is_modified());
        d.type_text("e");
        assert!(d.is_modified());
        d.undo();
        assert!(!d.is_modified());
        assert_eq!(d.text(), "abcd");
    }

    #[test]
    fn moving_the_cursor_breaks_undo_groups() {
        let mut d = doc("", Pos::new(0, 0));
        d.type_text("a");
        d.type_text("b");
        d.motion(Motion::Left, false);
        d.type_text("c");
        assert_eq!(d.text(), "acb");
        d.undo();
        assert_eq!(d.text(), "ab");
    }

    #[test]
    fn backspace_and_delete() {
        let mut d = doc("one two", Pos::new(0, 7));
        d.backspace(true);
        assert_eq!(d.text(), "one ");
        d.backspace(false);
        d.backspace(false);
        assert_eq!(d.text(), "on");
        d.undo();
        assert_eq!(d.text(), "one ");
        let mut d = doc("        x", Pos::new(0, 8));
        d.backspace(false);
        assert_eq!(d.text(), "    x");
        let mut d = doc("a\nb", Pos::new(0, 1));
        d.delete_forward(false);
        assert_eq!(d.text(), "ab");
        d.set_selection(Selection::new(Pos::new(0, 0), Pos::new(0, 2)));
        assert_eq!(d.cut(), "ab");
        assert_eq!(d.text(), "");
    }

    #[test]
    fn newline_keeps_indent() {
        let mut d = doc("    if x {", Pos::new(0, 10));
        d.newline();
        assert_eq!(d.text(), "    if x {\n        ");
        assert_eq!(d.cursor(), Pos::new(1, 8));
        let mut d = doc("  a", Pos::new(0, 3));
        d.newline();
        d.type_text("b");
        assert_eq!(d.text(), "  a\n  b");
    }

    #[test]
    fn indent_outdent_and_comments() {
        let mut d = Document::from_text("a\n  b\n\nc");
        d.set_selection(Selection::new(Pos::new(0, 0), Pos::new(3, 1)));
        d.indent();
        assert_eq!(d.text(), "    a\n      b\n\n    c");
        assert_eq!(d.selection(), Selection::new(Pos::new(0, 0), Pos::new(3, 5)));
        d.outdent();
        d.outdent();
        assert_eq!(d.text(), "a\nb\n\nc");
        d.undo();
        assert_eq!(d.text(), "a\n  b\n\nc");
        d.select_all();
        d.toggle_comment("//");
        assert_eq!(d.text(), "// a\n//   b\n\n// c");
        d.toggle_comment("//");
        assert_eq!(d.text(), "a\n  b\n\nc");
        let mut d = doc("ab", Pos::new(0, 1));
        d.indent();
        assert_eq!(d.text(), "a   b");
    }

    #[test]
    fn move_lines() {
        let mut d = doc("1\n2\n3", Pos::new(1, 1));
        d.move_lines(true);
        assert_eq!(d.text(), "2\n1\n3");
        assert_eq!(d.cursor(), Pos::new(0, 1));
        d.move_lines(true);
        assert_eq!(d.text(), "2\n1\n3");
        d.move_lines(false);
        d.move_lines(false);
        assert_eq!(d.text(), "1\n3\n2");
        d.undo();
        d.undo();
        d.undo();
        assert_eq!(d.text(), "1\n2\n3");
    }

    #[test]
    fn find_and_replace() {
        let mut d = doc("cat dog cat", Pos::new(0, 0));
        assert!(d.find_next("cat", true));
        assert_eq!(d.selection(), Selection::new(Pos::new(0, 0), Pos::new(0, 3)));
        assert!(d.replace_next("cat", "bird", true));
        assert_eq!(d.text(), "bird dog cat");
        assert_eq!(d.selected_text(), "cat");
        assert_eq!(d.replace_all("o", "0", true), 1);
        assert_eq!(d.replace_all("CAT", "lion", false), 1);
        assert_eq!(d.text(), "bird d0g lion");
        d.undo();
        assert_eq!(d.text(), "bird d0g cat");
        assert!(d.find_prev("bird", true));
        assert_eq!(d.selection().start(), Pos::new(0, 0));
    }

    #[test]
    fn line_motions_and_selection() {
        let mut d = doc("    text", Pos::new(0, 6));
        d.motion(Motion::LineStart, false);
        assert_eq!(d.cursor(), Pos::new(0, 4));
        d.motion(Motion::LineStart, false);
        assert_eq!(d.cursor(), Pos::new(0, 0));
        d.motion(Motion::LineEnd, true);
        assert_eq!(d.selected_text(), "    text");
        d.motion(Motion::Left, false);
        assert_eq!(d.cursor(), Pos::new(0, 0));
        let mut d = Document::from_text("ab\ncd");
        d.select_line(0);
        assert_eq!(d.selected_text(), "ab\n");
        d.goto_line(2);
        assert_eq!(d.cursor(), Pos::new(1, 0));
    }
}
