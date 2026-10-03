//! Syntax highlighting.
//!
//! A small hand-written lexer per [`Language`] splits each line into
//! [`Span`]s of [`Token`] kinds. Constructs that span lines (block comments,
//! strings, fenced code) are carried in a [`State`] from one line to the
//! next, so a view can highlight just the visible lines given the state at
//! the first one.

use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Plain,
    Rust,
    C,
    Toml,
    Markdown,
}

impl Language {
    /// Picks a language from a file name's extension.
    pub fn for_path(path: &str) -> Language {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let Some((_, ext)) = name.rsplit_once('.') else { return Language::Plain };
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Language::Rust,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => Language::C,
            "toml" | "ini" | "cfg" | "conf" | "app" => Language::Toml,
            "md" | "markdown" => Language::Markdown,
            _ => Language::Plain,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::Plain => "Plain Text",
            Language::Rust => "Rust",
            Language::C => "C/C++",
            Language::Toml => "TOML/INI",
            Language::Markdown => "Markdown",
        }
    }

    /// The line-comment prefix, if the language has one.
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::Rust | Language::C => Some("//"),
            Language::Toml => Some("#"),
            Language::Plain | Language::Markdown => None,
        }
    }
}

/// What a span of text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Text,
    Keyword,
    Type,
    Function,
    Macro,
    String,
    Number,
    Constant,
    Comment,
    Attribute,
    Heading,
    Link,
}

/// Highlighter state at the start of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    /// Nesting depth of block comments.
    comment: u8,
    /// 0: not in a string; 1: in a normal string; 2 + n: in a raw string
    /// closed by `"` and n `#`.
    string: u8,
    /// Inside a Markdown code fence.
    fence: bool,
}

/// Bytes `start..end` of a line are a `token`. Boundaries are always on
/// character boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub token: Token,
}

/// Collects spans, filling gaps with [`Token::Text`] and merging neighbours.
struct Spans<'a> {
    out: &'a mut Vec<Span>,
    pos: usize,
}

impl Spans<'_> {
    fn mark(&mut self, start: usize, end: usize, token: Token) {
        if start > self.pos {
            self.push(self.pos, start, Token::Text);
        }
        if end > start {
            self.push(start, end, token);
        }
        self.pos = self.pos.max(end);
    }

    fn push(&mut self, start: usize, end: usize, token: Token) {
        if let Some(last) = self.out.last_mut()
            && last.token == token
            && last.end == start
        {
            last.end = end;
            return;
        }
        self.out.push(Span { start, end, token });
    }

    fn finish(&mut self, len: usize) {
        if len > self.pos {
            self.push(self.pos, len, Token::Text);
        }
    }
}

/// Highlights one line, appending its spans to `out` (which together cover
/// the whole line). Returns the state for the next line.
pub fn highlight_line(lang: Language, line: &str, state: State, out: &mut Vec<Span>) -> State {
    let mut spans = Spans { out, pos: 0 };
    let next = match lang {
        Language::Plain => state,
        Language::Rust | Language::C => code(lang, line, state, &mut spans),
        Language::Toml => {
            toml(line, &mut spans);
            state
        }
        Language::Markdown => markdown(line, state, &mut spans),
    };
    spans.finish(line.len());
    next
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "union", "unsafe", "use", "where", "while",
];
const RUST_TYPES: &[&str] = &[
    "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
    "f32", "f64",
];
const C_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "constexpr",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "explicit",
    "extern",
    "false",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "private",
    "protected",
    "public",
    "return",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "using",
    "virtual",
    "volatile",
    "while",
];
const C_TYPES: &[&str] = &[
    "auto", "void", "bool", "char", "short", "int", "long", "float", "double", "signed", "unsigned", "size_t",
    "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t",
];

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Skips UTF-8 continuation bytes so `i` lands on a character boundary.
fn to_boundary(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] & 0xC0) == 0x80 {
        i += 1;
    }
    i.min(b.len())
}

/// Length of a character literal at the start of `s` (which starts with `'`).
fn char_literal_len(s: &str) -> Option<usize> {
    let mut it = s.char_indices().skip(1);
    let (_, c) = it.next()?;
    match c {
        '\\' => it.take(10).find(|&(_, c)| c == '\'').map(|(j, _)| j + 1),
        '\'' => None,
        _ => it.next().filter(|&(_, c)| c == '\'').map(|(j, _)| j + 1),
    }
}

/// Rust and C/C++.
fn code(lang: Language, line: &str, mut st: State, s: &mut Spans) -> State {
    let b = line.as_bytes();
    let n = b.len();
    let rust = lang == Language::Rust;
    let (keywords, types) = if rust { (RUST_KEYWORDS, RUST_TYPES) } else { (C_KEYWORDS, C_TYPES) };
    let mut i = 0;
    while i < n {
        if st.comment > 0 {
            let start = i;
            while i < n && st.comment > 0 {
                if b[i..].starts_with(b"*/") {
                    st.comment -= 1;
                    i += 2;
                } else if rust && b[i..].starts_with(b"/*") {
                    st.comment = st.comment.saturating_add(1);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i = to_boundary(b, i);
            s.mark(start, i, Token::Comment);
            continue;
        }
        if st.string > 0 {
            let start = i;
            if st.string == 1 {
                while i < n {
                    match b[i] {
                        b'\\' => i = to_boundary(b, i + 2),
                        b'"' => {
                            i += 1;
                            st.string = 0;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            } else {
                let hashes = (st.string - 2) as usize;
                while i < n {
                    if b[i] == b'"' && b.len() >= i + 1 + hashes && b[i + 1..i + 1 + hashes].iter().all(|&c| c == b'#')
                    {
                        i += 1 + hashes;
                        st.string = 0;
                        break;
                    }
                    i += 1;
                }
            }
            i = to_boundary(b, i);
            s.mark(start, i, Token::String);
            continue;
        }
        let c = b[i];
        let prev_ident = i > 0 && is_ident(b[i - 1]);
        if b[i..].starts_with(b"//") {
            s.mark(i, n, Token::Comment);
            return st;
        }
        if b[i..].starts_with(b"/*") {
            st.comment = 1;
            s.mark(i, i + 2, Token::Comment);
            i += 2;
            continue;
        }
        if c == b'"' {
            st.string = 1;
            s.mark(i, i + 1, Token::String);
            i += 1;
            continue;
        }
        if rust && !prev_ident && (c == b'r' || (c == b'b' && b.get(i + 1) == Some(&b'r'))) {
            // Raw strings: r"..", r#".."#, br"..".
            let mut j = i + if c == b'b' { 2 } else { 1 };
            let hashes_start = j;
            while j < n && b[j] == b'#' {
                j += 1;
            }
            if j < n && b[j] == b'"' && j - hashes_start < 250 {
                st.string = 2 + (j - hashes_start) as u8;
                s.mark(i, j + 1, Token::String);
                i = j + 1;
                continue;
            }
        }
        if rust && !prev_ident && c == b'b' && matches!(b.get(i + 1), Some(b'"' | b'\'')) {
            // Byte string or byte literal prefix.
            s.mark(i, i + 1, Token::String);
            i += 1;
            continue;
        }
        if c == b'\'' {
            if let Some(len) = char_literal_len(&line[i..]) {
                s.mark(i, i + len, Token::String);
                i += len;
                continue;
            }
            if rust {
                let mut j = i + 1;
                while j < n && is_ident(b[j]) {
                    j += 1;
                }
                if j > i + 1 {
                    s.mark(i, j, Token::Type);
                    i = j;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        if c.is_ascii_digit() && !prev_ident {
            let start = i;
            while i < n
                && (b[i].is_ascii_alphanumeric()
                    || b[i] == b'_'
                    || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)))
            {
                i += 1;
            }
            s.mark(start, i, Token::Number);
            continue;
        }
        if is_ident(c) {
            let start = i;
            while i < n && is_ident(b[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let next = b.get(i).copied();
            let token = if keywords.contains(&word) {
                Token::Keyword
            } else if rust && next == Some(b'!') && b.get(i + 1) != Some(&b'=') {
                i += 1;
                Token::Macro
            } else if types.contains(&word) {
                Token::Type
            } else if word.len() > 1 && word.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            {
                Token::Constant
            } else if word.as_bytes()[0].is_ascii_uppercase() {
                Token::Type
            } else if next == Some(b'(') {
                Token::Function
            } else {
                Token::Text
            };
            s.mark(start, i, token);
            continue;
        }
        if c == b'#' {
            if rust && (b.get(i + 1) == Some(&b'[') || b[i + 1..].starts_with(b"![")) {
                let start = i;
                let mut depth = 0;
                while i < n {
                    match b[i] {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                i = to_boundary(b, i);
                s.mark(start, i, Token::Attribute);
                continue;
            }
            if !rust && line[..i].trim().is_empty() {
                // Preprocessor directive, with `<header>` as a string.
                let start = i;
                i += 1;
                while i < n && b[i] == b' ' {
                    i += 1;
                }
                while i < n && is_ident(b[i]) {
                    i += 1;
                }
                s.mark(start, i, Token::Macro);
                let rest = &line[i..];
                if let Some(open) = rest.find('<')
                    && rest[..open].trim().is_empty()
                    && let Some(close) = rest[open..].find('>')
                {
                    s.mark(i + open, i + open + close + 1, Token::String);
                    i += open + close + 1;
                }
                continue;
            }
        }
        i += 1;
    }
    st
}

/// TOML and INI: sections, keys, values and comments.
fn toml(line: &str, s: &mut Spans) {
    let t = line.trim_start();
    let lead = line.len() - t.len();
    if t.starts_with('#') || t.starts_with(';') {
        s.mark(lead, line.len(), Token::Comment);
        return;
    }
    if t.starts_with('[') {
        let end = t.find(']').map_or(line.len(), |e| lead + e + 1);
        s.mark(lead, end, Token::Heading);
        value(line, end, s);
        return;
    }
    match line.find('=') {
        Some(eq) if !line[..eq].contains(['"', '\'']) => {
            s.mark(lead, line[..eq].trim_end().len(), Token::Type);
            value(line, eq + 1, s);
        }
        _ => value(line, lead, s),
    }
}

/// A TOML value (or the rest of a line): strings, numbers, booleans and
/// trailing comments.
fn value(line: &str, from: usize, s: &mut Spans) {
    let b = line.as_bytes();
    let mut i = from;
    while i < b.len() {
        let c = b[i];
        if c == b'#' {
            s.mark(i, b.len(), Token::Comment);
            return;
        }
        if c == b'"' || c == b'\'' {
            let close = line[i + 1..].find(c as char).map_or(b.len(), |e| i + 1 + e + 1);
            s.mark(i, close, Token::String);
            i = close;
            continue;
        }
        let word_start = i == 0 || !is_ident(b[i - 1]);
        if word_start
            && (c.is_ascii_digit() || ((c == b'-' || c == b'+') && b.get(i + 1).is_some_and(u8::is_ascii_digit)))
        {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'.' | b'_' | b':' | b'-')) {
                i += 1;
            }
            s.mark(start, i, Token::Number);
            continue;
        }
        if word_start && is_ident(c) {
            let start = i;
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            if matches!(&line[start..i], "true" | "false") {
                s.mark(start, i, Token::Keyword);
            }
            continue;
        }
        i += 1;
    }
}

/// Markdown: headings, quotes, lists, code and links.
fn markdown(line: &str, mut st: State, s: &mut Spans) -> State {
    let t = line.trim_start();
    let lead = line.len() - t.len();
    if t.starts_with("```") || t.starts_with("~~~") {
        st.fence = !st.fence;
        s.mark(0, line.len(), Token::Comment);
        return st;
    }
    if st.fence {
        s.mark(0, line.len(), Token::String);
        return st;
    }
    if t.starts_with('#') {
        s.mark(lead, line.len(), Token::Heading);
        return st;
    }
    if t.starts_with('>') {
        s.mark(lead, line.len(), Token::Comment);
        return st;
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let mut i = if ["- ", "* ", "+ "].iter().any(|m| t.starts_with(m)) {
        s.mark(lead, lead + 1, Token::Keyword);
        lead + 1
    } else if digits > 0 && t[digits..].starts_with(". ") {
        s.mark(lead, lead + digits + 1, Token::Keyword);
        lead + digits + 1
    } else {
        lead
    };
    let b = line.as_bytes();
    while i < b.len() {
        match b[i] {
            b'`' => {
                let close = line[i + 1..].find('`').map_or(b.len(), |e| i + 1 + e + 1);
                s.mark(i, close, Token::String);
                i = close;
            }
            b'*' | b'_' if b.get(i + 1) == Some(&b[i]) => {
                let marker = &line[i..i + 2];
                let close = line[i + 2..].find(marker).map_or(b.len(), |e| i + 2 + e + 2);
                s.mark(i, close, Token::Keyword);
                i = close;
            }
            b'[' => {
                // [text](url)
                if let Some(mid) = line[i..].find("](")
                    && let Some(end) = line[i + mid..].find(')')
                {
                    s.mark(i, i + mid + 1, Token::Function);
                    s.mark(i + mid + 1, i + mid + end + 1, Token::Link);
                    i += mid + end + 1;
                    continue;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn spans(lang: Language, lines: &[&str]) -> Vec<Vec<(String, Token)>> {
        let mut st = State::default();
        let mut all = Vec::new();
        for line in lines {
            let mut out = Vec::new();
            st = highlight_line(lang, line, st, &mut out);
            // Spans cover the line exactly, in order.
            let mut pos = 0;
            for s in &out {
                assert_eq!(s.start, pos, "{line:?}: {out:?}");
                assert!(s.end > s.start);
                pos = s.end;
            }
            assert_eq!(pos, line.len(), "{line:?}");
            all.push(
                out.iter()
                    .filter(|s| s.token != Token::Text)
                    .map(|s| (String::from(&line[s.start..s.end]), s.token))
                    .collect(),
            );
        }
        all
    }

    fn tok(s: &str, t: Token) -> (String, Token) {
        (String::from(s), t)
    }

    #[test]
    fn rust_tokens() {
        let r = spans(
            Language::Rust,
            &["#[derive(Debug)]", "pub fn main() -> u32 { let s = \"a\\\"b\"; println!(\"{}\", 'x', 0x1F); } // done"],
        );
        assert_eq!(r[0], [tok("#[derive(Debug)]", Token::Attribute)]);
        assert_eq!(
            r[1],
            [
                tok("pub", Token::Keyword),
                tok("fn", Token::Keyword),
                tok("main", Token::Function),
                tok("u32", Token::Type),
                tok("let", Token::Keyword),
                tok("\"a\\\"b\"", Token::String),
                tok("println!", Token::Macro),
                tok("\"{}\"", Token::String),
                tok("'x'", Token::String),
                tok("0x1F", Token::Number),
                tok("// done", Token::Comment),
            ]
        );
    }

    #[test]
    fn multi_line_state() {
        let r =
            spans(Language::Rust, &["a /* one", "two /* nested */ still", "end */ fn", "let s = r#\"raw", "\"# + 1;"]);
        assert_eq!(r[0], [tok("/* one", Token::Comment)]);
        assert_eq!(r[1], [tok("two /* nested */ still", Token::Comment)]);
        assert_eq!(r[2], [tok("end */", Token::Comment), tok("fn", Token::Keyword)]);
        assert_eq!(r[3], [tok("let", Token::Keyword), tok("r#\"raw", Token::String)]);
        assert_eq!(r[4], [tok("\"#", Token::String), tok("1", Token::Number)]);
        let lifetimes = spans(Language::Rust, &["fn f<'a>(x: &'a str) {}"]);
        assert!(lifetimes[0].contains(&tok("'a", Token::Type)));
    }

    #[test]
    fn c_tokens() {
        let r = spans(Language::C, &["#include <stdio.h>", "int main(void) { return MAX_LEN; } /* x"]);
        assert_eq!(r[0], [tok("#include", Token::Macro), tok("<stdio.h>", Token::String)]);
        assert_eq!(
            r[1],
            [
                tok("int", Token::Type),
                tok("main", Token::Function),
                tok("void", Token::Type),
                tok("return", Token::Keyword),
                tok("MAX_LEN", Token::Constant),
                tok("/* x", Token::Comment),
            ]
        );
    }

    #[test]
    fn toml_and_markdown() {
        let t = spans(Language::Toml, &["[package]", "name = \"vindows\" # comment", "debug = true", "level = 3"]);
        assert_eq!(t[0], [tok("[package]", Token::Heading)]);
        assert_eq!(
            t[1],
            [tok("name", Token::Type), tok("\"vindows\"", Token::String), tok("# comment", Token::Comment)]
        );
        assert_eq!(t[2], [tok("debug", Token::Type), tok("true", Token::Keyword)]);
        assert_eq!(t[3], [tok("level", Token::Type), tok("3", Token::Number)]);
        let m = spans(
            Language::Markdown,
            &["# Title", "- item with `code` and **bold**", "```", "let x;", "```", "see [docs](http://x)"],
        );
        assert_eq!(m[0], [tok("# Title", Token::Heading)]);
        assert_eq!(m[1], [tok("-", Token::Keyword), tok("`code`", Token::String), tok("**bold**", Token::Keyword)]);
        assert_eq!(m[3], [tok("let x;", Token::String)]);
        assert_eq!(m[5], [tok("[docs]", Token::Function), tok("(http://x)", Token::Link)]);
    }

    #[test]
    fn languages_and_utf8() {
        assert_eq!(Language::for_path("/home/user/main.RS"), Language::Rust);
        assert_eq!(Language::for_path("notes"), Language::Plain);
        assert_eq!(Language::for_path("a.toml"), Language::Toml);
        // Non-ASCII text inside strings and identifiers keeps spans on boundaries.
        spans(Language::Rust, &["let é = \"\\é ü\"; // ö", "'é' 'ab"]);
        spans(Language::Markdown, &["**é", "`ü"]);
        spans(Language::Toml, &["k = 'é", "ü = 1"]);
    }
}
