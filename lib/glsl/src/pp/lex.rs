//! The preprocessor's tokenizer: phases 1 to 8 of GLSL ES compilation.
//!
//! The shader's source strings are read as one stream of characters (a
//! token may even span two strings, as GL concatenates them), cut at their
//! first zero byte. Carriage returns and line feeds become newlines, a
//! backslash before a newline joins the lines (GLSL ES 3.00 only), and
//! comments count as white space, though the newlines inside `/* */` still
//! advance the line number. What remains is split into preprocessing
//! tokens: identifiers, preprocessing numbers (C's pp-number: a digit or a
//! period and digit, then any letters, digits, underscores, periods and
//! signed exponents), punctuators and newlines. Any other character
//! becomes an [`PpKind::Invalid`] token, which is an error only if it
//! survives preprocessing.

use alloc::string::String;
use alloc::vec::Vec;

use super::Punct;
use crate::diag::{Diagnostics, Loc, error};
use crate::intern::{Interner, Symbol};

/// What a preprocessing token is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PpKind {
    Ident(Symbol),
    /// A preprocessing number, spelled as the symbol.
    Number(Symbol),
    Punct(Punct),
    /// A character that is no part of the language (`@`, `$`, `"`, `\`,
    /// anything outside ASCII...).
    Invalid(char),
    Newline,
}

/// A preprocessing token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PpTok {
    pub kind: PpKind,
    /// Where the token's first character is, before any `#line`.
    pub loc: Loc,
    /// Whether white space (or a comment) comes just before the token.
    pub space: bool,
    /// Set on a macro name met while that macro was being expanded: it is
    /// never expanded, even later (C's "painted blue" tokens).
    pub noexpand: bool,
}

impl PpTok {
    pub fn new(kind: PpKind, loc: Loc, space: bool) -> PpTok {
        PpTok { kind, loc, space, noexpand: false }
    }

    pub fn is_punct(&self, p: Punct) -> bool {
        self.kind == PpKind::Punct(p)
    }

    pub fn ident(&self) -> Option<Symbol> {
        match self.kind {
            PpKind::Ident(s) => Some(s),
            _ => None,
        }
    }
}

/// Reads the source strings as one stream of bytes, tracking which string
/// and line each byte is on.
struct Reader<'a> {
    strings: &'a [&'a str],
    /// The current string (index) and byte within it.
    s: usize,
    pos: usize,
    line: u32,
    continuation: bool,
}

impl<'a> Reader<'a> {
    fn new(strings: &'a [&'a str], continuation: bool) -> Reader<'a> {
        let mut r = Reader { strings, s: 0, pos: 0, line: 1, continuation };
        r.skip_finished_strings();
        r
    }

    /// The current string's text, up to its first zero byte.
    fn text(&self, s: usize) -> &'a [u8] {
        let bytes = self.strings[s].as_bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        &bytes[..end]
    }

    fn skip_finished_strings(&mut self) {
        while self.s < self.strings.len() && self.pos >= self.text(self.s).len() {
            self.s += 1;
            self.pos = 0;
            self.line = 1;
        }
    }

    fn loc(&self) -> Loc {
        Loc::new(self.s as u32, self.line)
    }

    /// The raw byte `ahead` bytes on, across string boundaries, before
    /// newline handling.
    fn raw(&self, ahead: usize) -> Option<u8> {
        let (mut s, mut pos) = (self.s, self.pos + ahead);
        while s < self.strings.len() {
            let t = self.text(s);
            if pos < t.len() {
                return Some(t[pos]);
            }
            pos -= t.len();
            s += 1;
        }
        None
    }

    /// Advances one raw byte.
    fn bump_raw(&mut self) {
        self.pos += 1;
        self.skip_finished_strings();
    }

    /// Length of the newline sequence at `ahead` (CR LF, LF CR, CR or LF),
    /// or 0.
    fn newline_len(&self, ahead: usize) -> usize {
        match (self.raw(ahead), self.raw(ahead + 1)) {
            (Some(b'\r'), Some(b'\n')) | (Some(b'\n'), Some(b'\r')) => 2,
            (Some(b'\r'), _) | (Some(b'\n'), _) => 1,
            _ => 0,
        }
    }

    /// Skips line continuations at the current position.
    fn skip_continuations(&mut self) {
        while self.continuation && self.raw(0) == Some(b'\\') {
            let n = self.newline_len(1);
            if n == 0 {
                break;
            }
            self.line += 1;
            for _ in 0..1 + n {
                self.bump_raw();
            }
        }
    }

    /// The next character after line continuations, with any newline
    /// sequence reported as `\n`.
    fn peek(&mut self) -> Option<u8> {
        self.skip_continuations();
        match self.raw(0)? {
            b'\r' => Some(b'\n'),
            b => Some(b),
        }
    }

    /// The character after the next one (continuations skipped, best effort).
    fn peek2(&mut self) -> Option<u8> {
        self.skip_continuations();
        let first = self.raw(0)?;
        let mut ahead = if first == b'\r' || first == b'\n' { self.newline_len(0) } else { 1 };
        while self.continuation && self.raw(ahead) == Some(b'\\') {
            let n = self.newline_len(ahead + 1);
            if n == 0 {
                break;
            }
            ahead += 1 + n;
        }
        match self.raw(ahead)? {
            b'\r' => Some(b'\n'),
            b => Some(b),
        }
    }

    /// Consumes the next character (a whole newline sequence for `\n`).
    fn bump(&mut self) {
        self.skip_continuations();
        match self.raw(0) {
            Some(b'\r') | Some(b'\n') => {
                let n = self.newline_len(0);
                // Count the newline before stepping over it: if it ends its
                // string, the next string starts again at line 1.
                self.line += 1;
                for _ in 0..n {
                    self.bump_raw();
                }
            }
            Some(_) => self.bump_raw(),
            None => {}
        }
    }

    /// Consumes one UTF-8 character starting at the current byte and
    /// returns it (U+FFFD for an invalid sequence).
    fn bump_utf8(&mut self) -> char {
        let first = self.raw(0).unwrap_or(0);
        let len = match first {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 1,
        };
        let mut bytes = [0u8; 4];
        let mut got = 0;
        for (i, b) in bytes.iter_mut().enumerate().take(len) {
            match self.raw(i) {
                Some(x) if i == 0 || (0x80..0xC0).contains(&x) => {
                    *b = x;
                    got += 1;
                }
                _ => break,
            }
        }
        for _ in 0..got.max(1) {
            self.bump_raw();
        }
        core::str::from_utf8(&bytes[..got]).ok().and_then(|s| s.chars().next()).unwrap_or('\u{FFFD}')
    }
}

/// Splits the shader's source strings into preprocessing tokens, ending
/// every line (the last one too) with a newline token. `continuation`
/// enables GLSL ES 3.00's line continuation.
pub fn tokenize(strings: &[&str], continuation: bool, interner: &mut Interner, diags: &mut Diagnostics) -> Vec<PpTok> {
    let mut r = Reader::new(strings, continuation);
    let mut out = Vec::new();
    let mut space = true;
    let mut text = String::new();
    loop {
        let loc = r.loc();
        let Some(c) = r.peek() else { break };
        match c {
            b'\n' => {
                r.bump();
                out.push(PpTok::new(PpKind::Newline, loc, space));
                space = true;
            }
            b' ' | b'\t' | 0x0B | 0x0C => {
                r.bump();
                space = true;
            }
            b'/' if r.peek2() == Some(b'/') => {
                // A line comment runs up to, not including, the newline.
                while let Some(c) = r.peek() {
                    if c == b'\n' {
                        break;
                    }
                    if c >= 0x80 {
                        r.bump_utf8();
                    } else {
                        r.bump();
                    }
                }
                space = true;
            }
            b'/' if r.peek2() == Some(b'*') => {
                r.bump();
                r.bump();
                let mut closed = false;
                while let Some(c) = r.peek() {
                    if c == b'*' && r.peek2() == Some(b'/') {
                        r.bump();
                        r.bump();
                        closed = true;
                        break;
                    }
                    if c >= 0x80 {
                        r.bump_utf8();
                    } else {
                        r.bump();
                    }
                }
                if !closed {
                    error!(diags, loc, "unterminated comment");
                }
                space = true;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                text.clear();
                while let Some(c) = r.peek() {
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        text.push(c as char);
                        r.bump();
                    } else {
                        break;
                    }
                }
                out.push(PpTok::new(PpKind::Ident(interner.intern(&text)), loc, space));
                space = false;
            }
            b'0'..=b'9' => {
                lex_number(&mut r, &mut text);
                out.push(PpTok::new(PpKind::Number(interner.intern(&text)), loc, space));
                space = false;
            }
            b'.' if r.peek2().is_some_and(|d| d.is_ascii_digit()) => {
                lex_number(&mut r, &mut text);
                out.push(PpTok::new(PpKind::Number(interner.intern(&text)), loc, space));
                space = false;
            }
            c if c >= 0x80 => {
                let ch = r.bump_utf8();
                out.push(PpTok::new(PpKind::Invalid(ch), loc, space));
                space = false;
            }
            _ => {
                if let Some(p) = lex_punct(&mut r) {
                    out.push(PpTok::new(PpKind::Punct(p), loc, space));
                } else {
                    r.bump();
                    out.push(PpTok::new(PpKind::Invalid(c as char), loc, space));
                }
                space = false;
            }
        }
    }
    if out.last().is_none_or(|t| t.kind != PpKind::Newline) {
        out.push(PpTok::new(PpKind::Newline, r.loc(), space));
    }
    out
}

/// Reads a preprocessing number into `text`.
fn lex_number(r: &mut Reader<'_>, text: &mut String) {
    text.clear();
    while let Some(c) = r.peek() {
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
            let exponent = (c == b'e' || c == b'E') && !text.starts_with("0x") && !text.starts_with("0X");
            text.push(c as char);
            r.bump();
            if exponent && let Some(s @ (b'+' | b'-')) = r.peek() {
                text.push(s as char);
                r.bump();
            }
        } else {
            break;
        }
    }
}

/// Reads the longest punctuator at the current position.
fn lex_punct(r: &mut Reader<'_>) -> Option<Punct> {
    use Punct::*;
    let c = r.peek()?;
    let n = r.peek2();
    // Three-character operators: <<= >>=.
    let two = |r: &mut Reader<'_>, p: Punct| {
        r.bump();
        r.bump();
        Some(p)
    };
    let one = |r: &mut Reader<'_>, p: Punct| {
        r.bump();
        Some(p)
    };
    match (c, n) {
        (b'<', Some(b'<')) => {
            r.bump();
            r.bump();
            if r.peek() == Some(b'=') {
                r.bump();
                return Some(ShlAssign);
            }
            Some(Shl)
        }
        (b'>', Some(b'>')) => {
            r.bump();
            r.bump();
            if r.peek() == Some(b'=') {
                r.bump();
                return Some(ShrAssign);
            }
            Some(Shr)
        }
        (b'+', Some(b'+')) => two(r, Inc),
        (b'-', Some(b'-')) => two(r, Dec),
        (b'<', Some(b'=')) => two(r, Le),
        (b'>', Some(b'=')) => two(r, Ge),
        (b'=', Some(b'=')) => two(r, EqEq),
        (b'!', Some(b'=')) => two(r, Ne),
        (b'&', Some(b'&')) => two(r, AndAnd),
        (b'|', Some(b'|')) => two(r, OrOr),
        (b'^', Some(b'^')) => two(r, XorXor),
        (b'+', Some(b'=')) => two(r, AddAssign),
        (b'-', Some(b'=')) => two(r, SubAssign),
        (b'*', Some(b'=')) => two(r, MulAssign),
        (b'/', Some(b'=')) => two(r, DivAssign),
        (b'%', Some(b'=')) => two(r, ModAssign),
        (b'&', Some(b'=')) => two(r, AndAssign),
        (b'^', Some(b'=')) => two(r, XorAssign),
        (b'|', Some(b'=')) => two(r, OrAssign),
        (b'(', _) => one(r, LParen),
        (b')', _) => one(r, RParen),
        (b'[', _) => one(r, LBracket),
        (b']', _) => one(r, RBracket),
        (b'{', _) => one(r, LBrace),
        (b'}', _) => one(r, RBrace),
        (b'.', _) => one(r, Dot),
        (b',', _) => one(r, Comma),
        (b';', _) => one(r, Semicolon),
        (b':', _) => one(r, Colon),
        (b'?', _) => one(r, Question),
        (b'+', _) => one(r, Plus),
        (b'-', _) => one(r, Minus),
        (b'*', _) => one(r, Star),
        (b'/', _) => one(r, Slash),
        (b'%', _) => one(r, Percent),
        (b'<', _) => one(r, Lt),
        (b'>', _) => one(r, Gt),
        (b'&', _) => one(r, Amp),
        (b'^', _) => one(r, Caret),
        (b'|', _) => one(r, Pipe),
        (b'~', _) => one(r, Tilde),
        (b'!', _) => one(r, Bang),
        (b'=', _) => one(r, Assign),
        (b'#', _) => one(r, Hash),
        _ => None,
    }
}
