//! [`Buffer`]: text stored as a vector of lines.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// A position in a buffer: a line index and a byte offset into that line.
/// Valid positions are always on a character boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Pos {
        Pos { line, col }
    }
}

/// The line terminator a buffer is saved with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// Character classes for word-wise movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Returns the two positions in ascending order.
pub(crate) fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The position after `text` if it were inserted at `at`.
pub(crate) fn end_of(at: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        None => Pos::new(at.line, at.col + text.len()),
        Some(i) => Pos::new(at.line + text.matches('\n').count(), text.len() - i - 1),
    }
}

/// Byte offset of the first match of `needle` in `hay` at or after `start`.
/// With `case_sensitive == false`, ASCII letters match regardless of case.
fn find_in(hay: &str, needle: &str, start: usize, case_sensitive: bool) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    // Needles are valid UTF-8, so they cannot match starting at a
    // continuation byte: every match starts on a character boundary.
    (start..=h.len() - n.len()).find(|&i| {
        let w = &h[i..i + n.len()];
        if case_sensitive { w == n } else { w.eq_ignore_ascii_case(n) }
    })
}

/// Text as a list of lines (without terminators). Never empty: an empty
/// buffer has one empty line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    lines: Vec<String>,
    pub line_ending: LineEnding,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::new()
    }
}

impl Buffer {
    pub fn new() -> Buffer {
        Buffer { lines: vec![String::new()], line_ending: LineEnding::Lf }
    }

    /// Splits text into lines. `\r\n` and `\n` both end a line; a buffer
    /// read from a `\r\n` file is saved with `\r\n` again.
    pub fn from_text(text: &str) -> Buffer {
        let line_ending = if text.contains("\r\n") { LineEnding::CrLf } else { LineEnding::Lf };
        let lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
        Buffer { lines, line_ending }
    }

    /// The whole text, joined with the buffer's line ending.
    pub fn text(&self) -> String {
        self.lines.join(self.line_ending.as_str())
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Line `i` (panics if out of range).
    pub fn line(&self, i: usize) -> &str {
        &self.lines[i]
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|l| l.as_str())
    }

    /// Size in bytes, counting each line break as one byte.
    pub fn len(&self) -> usize {
        self.lines.iter().map(|l| l.len()).sum::<usize>() + self.lines.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn end(&self) -> Pos {
        self.line_end(self.lines.len() - 1)
    }

    pub fn line_end(&self, line: usize) -> Pos {
        Pos::new(line, self.lines[line].len())
    }

    /// The nearest valid position at or before `p`.
    pub fn clamp(&self, p: Pos) -> Pos {
        if p.line >= self.lines.len() {
            return self.end();
        }
        let s = &self.lines[p.line];
        let mut col = p.col.min(s.len());
        while !s.is_char_boundary(col) {
            col -= 1;
        }
        Pos::new(p.line, col)
    }

    /// The character after `p`: `'\n'` at the end of a line, `None` at the
    /// end of the buffer.
    pub fn char_after(&self, p: Pos) -> Option<char> {
        let s = &self.lines[p.line];
        if p.col < s.len() {
            s[p.col..].chars().next()
        } else if p.line + 1 < self.lines.len() {
            Some('\n')
        } else {
            None
        }
    }

    /// The character before `p`: `'\n'` at the start of a line, `None` at
    /// the start of the buffer.
    pub fn char_before(&self, p: Pos) -> Option<char> {
        if p.col > 0 {
            self.lines[p.line][..p.col].chars().next_back()
        } else if p.line > 0 {
            Some('\n')
        } else {
            None
        }
    }

    /// One character back (to the end of the previous line at a line start).
    pub fn prev(&self, p: Pos) -> Pos {
        if p.col > 0 {
            let n = self.lines[p.line][..p.col].chars().next_back().map_or(1, char::len_utf8);
            Pos::new(p.line, p.col - n)
        } else if p.line > 0 {
            self.line_end(p.line - 1)
        } else {
            p
        }
    }

    /// One character forward (to the start of the next line at a line end).
    pub fn next(&self, p: Pos) -> Pos {
        let s = &self.lines[p.line];
        if p.col < s.len() {
            let n = s[p.col..].chars().next().map_or(1, char::len_utf8);
            Pos::new(p.line, p.col + n)
        } else if p.line + 1 < self.lines.len() {
            Pos::new(p.line + 1, 0)
        } else {
            p
        }
    }

    /// The start of the word before `p`, skipping whitespace (Ctrl+Left).
    pub fn word_left(&self, mut p: Pos) -> Pos {
        while self.char_before(p).is_some_and(|c| class(c) == Class::Space) {
            p = self.prev(p);
        }
        if let Some(k) = self.char_before(p).map(class) {
            while self.char_before(p).is_some_and(|c| class(c) == k) {
                p = self.prev(p);
            }
        }
        p
    }

    /// The end of the word after `p`, skipping whitespace (Ctrl+Right).
    pub fn word_right(&self, mut p: Pos) -> Pos {
        while self.char_after(p).is_some_and(|c| class(c) == Class::Space) {
            p = self.next(p);
        }
        if let Some(k) = self.char_after(p).map(class) {
            while self.char_after(p).is_some_and(|c| class(c) == k) {
                p = self.next(p);
            }
        }
        p
    }

    /// The run of same-class characters around `p` within its line (what a
    /// double click selects). Prefers the character after `p`.
    pub fn word_at(&self, p: Pos) -> (Pos, Pos) {
        let p = self.clamp(p);
        let s = &self.lines[p.line];
        let after = s[p.col..].chars().next().map(class);
        let before = s[..p.col].chars().next_back().map(class);
        // Clicking just after a word selects that word rather than the space.
        let k = match (before, after) {
            (Some(Class::Word), Some(Class::Space) | None) => Class::Word,
            (_, Some(k)) => k,
            (Some(k), None) => k,
            (None, None) => return (p, p),
        };
        let mut a = p.col;
        while let Some(c) = s[..a].chars().next_back().filter(|&c| class(c) == k) {
            a -= c.len_utf8();
        }
        let mut b = p.col;
        while let Some(c) = s[b..].chars().next().filter(|&c| class(c) == k) {
            b += c.len_utf8();
        }
        (Pos::new(p.line, a), Pos::new(p.line, b))
    }

    /// Inserts `text` (with `\n` or `\r\n` line breaks) at `at`; returns the
    /// position after it.
    pub fn insert(&mut self, at: Pos, text: &str) -> Pos {
        let at = self.clamp(at);
        let mut parts = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
        let first = parts.next().unwrap_or("");
        let mut rest: Vec<String> = parts.map(|s| s.to_string()).collect();
        let line = &mut self.lines[at.line];
        if rest.is_empty() {
            line.insert_str(at.col, first);
            return Pos::new(at.line, at.col + first.len());
        }
        let tail = line.split_off(at.col);
        line.push_str(first);
        let n = rest.len();
        let end_col = rest[n - 1].len();
        rest[n - 1].push_str(&tail);
        self.lines.splice(at.line + 1..at.line + 1, rest);
        Pos::new(at.line + n, end_col)
    }

    /// The text between two positions (in either order), with `\n` breaks.
    pub fn slice(&self, a: Pos, b: Pos) -> String {
        let (a, b) = ordered(self.clamp(a), self.clamp(b));
        if a.line == b.line {
            return self.lines[a.line][a.col..b.col].to_string();
        }
        let mut s = String::from(&self.lines[a.line][a.col..]);
        for l in &self.lines[a.line + 1..b.line] {
            s.push('\n');
            s.push_str(l);
        }
        s.push('\n');
        s.push_str(&self.lines[b.line][..b.col]);
        s
    }

    /// Removes the text between two positions (in either order); returns it.
    pub fn delete(&mut self, a: Pos, b: Pos) -> String {
        let (a, b) = ordered(self.clamp(a), self.clamp(b));
        let removed = self.slice(a, b);
        if a.line == b.line {
            self.lines[a.line].replace_range(a.col..b.col, "");
        } else {
            let tail = self.lines[b.line][b.col..].to_string();
            self.lines[a.line].truncate(a.col);
            self.lines[a.line].push_str(&tail);
            self.lines.drain(a.line + 1..=b.line);
        }
        removed
    }

    /// The next match of `needle` (which must not contain a line break) at
    /// or after `from`, wrapping around the end of the buffer.
    pub fn find(&self, needle: &str, from: Pos, case_sensitive: bool) -> Option<(Pos, Pos)> {
        if needle.is_empty() || needle.contains('\n') {
            return None;
        }
        let from = self.clamp(from);
        let n = self.lines.len();
        for i in 0..=n {
            let line = (from.line + i) % n;
            let start = if i == 0 { from.col } else { 0 };
            if let Some(c) = find_in(&self.lines[line], needle, start, case_sensitive) {
                // On the wrapped-around pass, only matches before `from` are new.
                if i == n && c >= from.col {
                    return None;
                }
                return Some((Pos::new(line, c), Pos::new(line, c + needle.len())));
            }
        }
        None
    }

    /// The last match of `needle` starting before `from`, wrapping around the
    /// start of the buffer.
    pub fn find_back(&self, needle: &str, from: Pos, case_sensitive: bool) -> Option<(Pos, Pos)> {
        if needle.is_empty() || needle.contains('\n') {
            return None;
        }
        let from = self.clamp(from);
        let n = self.lines.len();
        let last_in = |line: usize, limit: usize| {
            let mut found = None;
            let mut start = 0;
            while let Some(c) = find_in(&self.lines[line], needle, start, case_sensitive) {
                if c >= limit {
                    break;
                }
                found = Some(c);
                start = c + 1;
            }
            found
        };
        for i in 0..=n {
            let line = (from.line + n - i % n) % n;
            let limit = if i == 0 { from.col } else { usize::MAX };
            let limit = if i == n { usize::MAX } else { limit };
            if let Some(c) = last_in(line, limit) {
                if i == n && c < from.col {
                    return None;
                }
                return Some((Pos::new(line, c), Pos::new(line, c + needle.len())));
            }
        }
        None
    }

    /// All non-overlapping matches, up to `limit`.
    pub fn find_all(&self, needle: &str, case_sensitive: bool, limit: usize) -> Vec<(Pos, Pos)> {
        let mut out = Vec::new();
        if needle.is_empty() || needle.contains('\n') {
            return out;
        }
        for (i, line) in self.lines.iter().enumerate() {
            let mut start = 0;
            while let Some(c) = find_in(line, needle, start, case_sensitive) {
                if out.len() >= limit {
                    return out;
                }
                out.push((Pos::new(i, c), Pos::new(i, c + needle.len())));
                start = c + needle.len();
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Buffer {
        Buffer::from_text(s)
    }

    #[test]
    fn round_trip_and_line_endings() {
        for text in ["", "a", "a\n", "one\ntwo\nthree", "\n\n", "x\r\ny\r\n"] {
            assert_eq!(buf(text).text(), text, "{text:?}");
        }
        assert_eq!(buf("a\r\nb").line_ending, LineEnding::CrLf);
        assert_eq!(buf("a\nb").line(1), "b");
        assert_eq!(buf("ab\ncd").len(), 5);
    }

    #[test]
    fn insert_and_delete() {
        let mut b = buf("hello world");
        let end = b.insert(Pos::new(0, 5), ",\nbig");
        assert_eq!(end, Pos::new(1, 3));
        assert_eq!(b.text(), "hello,\nbig world");
        assert_eq!(b.slice(Pos::new(0, 3), Pos::new(1, 2)), "lo,\nbi");
        assert_eq!(b.delete(Pos::new(1, 2), Pos::new(0, 3)), "lo,\nbi");
        assert_eq!(b.text(), "helg world");
        let end = b.insert(Pos::new(0, 0), "a\r\nb\r\n");
        assert_eq!(end, Pos::new(2, 0));
        assert_eq!(b.line_count(), 3);
        assert_eq!(end_of(Pos::new(4, 2), "xy\nabc"), Pos::new(5, 3));
        assert_eq!(end_of(Pos::new(4, 2), "xy"), Pos::new(4, 4));
    }

    #[test]
    fn utf8_movement() {
        let b = buf("aé€\nb");
        let mut p = Pos::new(0, 0);
        let mut stops = Vec::new();
        for _ in 0..6 {
            stops.push(p);
            p = b.next(p);
        }
        assert_eq!(
            stops,
            [Pos::new(0, 0), Pos::new(0, 1), Pos::new(0, 3), Pos::new(0, 6), Pos::new(1, 0), Pos::new(1, 1)]
        );
        assert_eq!(b.prev(Pos::new(0, 6)), Pos::new(0, 3));
        assert_eq!(b.clamp(Pos::new(0, 5)), Pos::new(0, 3));
        assert_eq!(b.clamp(Pos::new(9, 9)), Pos::new(1, 1));
    }

    #[test]
    fn words() {
        let b = buf("let x_1 = foo(bar);\n  next");
        assert_eq!(b.word_right(Pos::new(0, 0)), Pos::new(0, 3));
        assert_eq!(b.word_right(Pos::new(0, 3)), Pos::new(0, 7));
        assert_eq!(b.word_right(Pos::new(0, 13)), Pos::new(0, 14));
        assert_eq!(b.word_right(Pos::new(0, 19)), Pos::new(1, 6));
        assert_eq!(b.word_left(Pos::new(1, 2)), Pos::new(0, 17));
        assert_eq!(b.word_left(Pos::new(0, 7)), Pos::new(0, 4));
        assert_eq!(b.word_at(Pos::new(0, 5)), (Pos::new(0, 4), Pos::new(0, 7)));
        assert_eq!(b.word_at(Pos::new(0, 7)), (Pos::new(0, 4), Pos::new(0, 7)));
        assert_eq!(b.word_at(Pos::new(0, 13)), (Pos::new(0, 13), Pos::new(0, 14)));
    }

    #[test]
    fn search() {
        let b = buf("abc ABC abc\nxabc");
        assert_eq!(b.find("abc", Pos::new(0, 1), true), Some((Pos::new(0, 8), Pos::new(0, 11))));
        assert_eq!(b.find("abc", Pos::new(0, 1), false), Some((Pos::new(0, 4), Pos::new(0, 7))));
        assert_eq!(b.find("abc", Pos::new(1, 2), true), Some((Pos::new(0, 0), Pos::new(0, 3))));
        assert_eq!(b.find_back("abc", Pos::new(0, 8), true), Some((Pos::new(0, 0), Pos::new(0, 3))));
        assert_eq!(b.find_back("abc", Pos::new(0, 0), true), Some((Pos::new(1, 1), Pos::new(1, 4))));
        assert_eq!(b.find_all("abc", false, 100).len(), 4);
        assert_eq!(b.find("zzz", Pos::new(0, 0), true), None);
        assert_eq!(b.find_back("zzz", Pos::new(0, 0), true), None);
        let one = buf("only");
        assert_eq!(one.find("only", Pos::new(0, 2), true), Some((Pos::new(0, 0), Pos::new(0, 4))));
        assert_eq!(one.find_back("only", Pos::new(0, 0), true), Some((Pos::new(0, 0), Pos::new(0, 4))));
        assert_eq!(buf("é ÉE").find("ée", Pos::new(0, 0), false), None);
    }
}
