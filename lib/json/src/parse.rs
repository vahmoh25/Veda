//! The parser: RFC 8259 JSON, strict (no comments, no trailing commas),
//! with a nesting limit. Duplicate object keys keep the last value.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::{MAX_DEPTH, Map, Number, Value};

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The input ended inside a value.
    UnexpectedEnd,
    /// A character that cannot start or continue a value here.
    UnexpectedChar(char),
    /// Something other than whitespace after the document.
    TrailingCharacters,
    BadNumber,
    BadEscape,
    /// A `\u` escape that is a lone surrogate.
    BadUnicode,
    /// A raw control character inside a string.
    ControlCharacter,
    /// The input is not UTF-8.
    InvalidUtf8,
    /// Arrays and objects nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

/// A parse error and the byte offset where it was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ParseErrorKind::UnexpectedEnd => write!(f, "unexpected end of JSON"),
            ParseErrorKind::UnexpectedChar(c) => write!(f, "unexpected {:?} at byte {}", c, self.offset),
            ParseErrorKind::TrailingCharacters => {
                write!(f, "unexpected data after the JSON value at byte {}", self.offset)
            }
            ParseErrorKind::BadNumber => write!(f, "malformed number at byte {}", self.offset),
            ParseErrorKind::BadEscape => write!(f, "malformed escape at byte {}", self.offset),
            ParseErrorKind::BadUnicode => write!(f, "invalid unicode escape at byte {}", self.offset),
            ParseErrorKind::ControlCharacter => write!(f, "control character in a string at byte {}", self.offset),
            ParseErrorKind::InvalidUtf8 => write!(f, "the JSON is not valid UTF-8"),
            ParseErrorKind::TooDeep => write!(f, "JSON nested too deeply at byte {}", self.offset),
        }
    }
}

/// Parses a JSON document.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.i < p.s.len() {
        return Err(p.err(ParseErrorKind::TrailingCharacters));
    }
    Ok(v)
}

/// Parses a JSON document from bytes (which must be UTF-8).
pub fn parse_bytes(bytes: &[u8]) -> Result<Value, ParseError> {
    match core::str::from_utf8(bytes) {
        Ok(text) => parse(text),
        Err(e) => Err(ParseError { kind: ParseErrorKind::InvalidUtf8, offset: e.valid_up_to() }),
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, kind: ParseErrorKind) -> ParseError {
        ParseError { kind, offset: self.i }
    }

    fn unexpected(&self) -> ParseError {
        match self.peek() {
            None => self.err(ParseErrorKind::UnexpectedEnd),
            Some(_) => {
                // Report the whole character, not a UTF-8 fragment.
                let rest = core::str::from_utf8(&self.s[self.i..]).ok();
                let c = rest.and_then(|r| r.chars().next()).unwrap_or('\u{fffd}');
                self.err(ParseErrorKind::UnexpectedChar(c))
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn expect_word(&mut self, word: &[u8], v: Value) -> Result<Value, ParseError> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else if self.s.len() - self.i < word.len() && word.starts_with(&self.s[self.i..]) {
            self.i = self.s.len();
            Err(self.err(ParseErrorKind::UnexpectedEnd))
        } else {
            Err(self.unexpected())
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        match self.peek() {
            None => Err(self.err(ParseErrorKind::UnexpectedEnd)),
            Some(b'n') => self.expect_word(b"null", Value::Null),
            Some(b't') => self.expect_word(b"true", Value::Bool(true)),
            Some(b'f') => self.expect_word(b"false", Value::Bool(false)),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b'[') => {
                if depth >= MAX_DEPTH {
                    return Err(self.err(ParseErrorKind::TooDeep));
                }
                self.array(depth)
            }
            Some(b'{') => {
                if depth >= MAX_DEPTH {
                    return Err(self.err(ParseErrorKind::TooDeep));
                }
                self.object(depth)
            }
            Some(_) => Err(self.unexpected()),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.unexpected()),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.i += 1; // '{'
        let mut map = Map::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.unexpected());
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.unexpected());
            }
            self.i += 1;
            self.skip_ws();
            let v = self.value(depth + 1)?;
            if map.contains_key(&key) {
                map.insert(key, v);
            } else {
                map.push(key, v);
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.unexpected()),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.i + 4 > self.s.len() {
            self.i = self.s.len();
            return Err(self.err(ParseErrorKind::UnexpectedEnd));
        }
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.s[self.i] {
                c @ b'0'..=b'9' => c - b'0',
                c @ b'a'..=b'f' => c - b'a' + 10,
                c @ b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(self.err(ParseErrorKind::BadEscape)),
            };
            v = v * 16 + d as u32;
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            // Copy the longest run of plain characters at once.
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > start {
                // The input is a &str and runs end at ASCII bytes, so the
                // slice is valid UTF-8.
                out.push_str(
                    core::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err(ParseErrorKind::InvalidUtf8))?,
                );
            }
            match self.peek() {
                None => return Err(self.err(ParseErrorKind::UnexpectedEnd)),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let Some(e) = self.peek() else { return Err(self.err(ParseErrorKind::UnexpectedEnd)) };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let at = self.i;
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                // A high surrogate must be followed by a low one.
                                if self.s.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return Err(ParseError { kind: ParseErrorKind::BadUnicode, offset: at });
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(ParseError { kind: ParseErrorKind::BadUnicode, offset: at });
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(ParseError { kind: ParseErrorKind::BadUnicode, offset: at });
                            } else {
                                hi
                            };
                            match char::from_u32(code) {
                                Some(c) => out.push(c),
                                None => return Err(ParseError { kind: ParseErrorKind::BadUnicode, offset: at }),
                            }
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.err(ParseErrorKind::BadEscape));
                        }
                    }
                }
                Some(_) => return Err(self.err(ParseErrorKind::ControlCharacter)),
            }
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        let bad = |p: &Parser, at: usize| ParseError { kind: ParseErrorKind::BadNumber, offset: at.min(p.s.len()) };
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while let Some(b'0'..=b'9') = self.peek() {
                    self.i += 1;
                }
            }
            _ => return Err(bad(self, start)),
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            float = true;
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(bad(self, start));
            }
            while let Some(b'0'..=b'9') = self.peek() {
                self.i += 1;
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            float = true;
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(bad(self, start));
            }
            while let Some(b'0'..=b'9') = self.peek() {
                self.i += 1;
            }
        }
        // ASCII digits and signs only, so this is valid UTF-8.
        let text = core::str::from_utf8(&self.s[start..self.i]).map_err(|_| bad(self, start))?;
        if !float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Number(Number::Int(i)));
            }
            if let Ok(u) = text.parse::<u64>() {
                return Ok(Value::Number(Number::UInt(u)));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Number(Number::Float(f))),
            _ => Err(bad(self, start)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;

    fn kind(text: &str) -> ParseErrorKind {
        parse(text).unwrap_err().kind
    }

    #[test]
    fn rejects_malformed_documents() {
        assert_eq!(kind(""), ParseErrorKind::UnexpectedEnd);
        assert_eq!(kind("[1,]"), ParseErrorKind::UnexpectedChar(']'));
        assert_eq!(kind("{\"a\" 1}"), ParseErrorKind::UnexpectedChar('1'));
        assert_eq!(kind("{\"a\":1,}"), ParseErrorKind::UnexpectedChar('}'));
        assert_eq!(kind("tru"), ParseErrorKind::UnexpectedEnd);
        assert_eq!(kind("trux"), ParseErrorKind::UnexpectedChar('t'));
        assert_eq!(kind("01"), ParseErrorKind::TrailingCharacters);
        assert_eq!(kind("1."), ParseErrorKind::BadNumber);
        assert_eq!(kind("-"), ParseErrorKind::BadNumber);
        assert_eq!(kind("1e"), ParseErrorKind::BadNumber);
        assert_eq!(kind("1e999"), ParseErrorKind::BadNumber);
        assert_eq!(kind("\"a\\x\""), ParseErrorKind::BadEscape);
        assert_eq!(kind("\"\\ud800\""), ParseErrorKind::BadUnicode);
        assert_eq!(kind("\"\\udc00\""), ParseErrorKind::BadUnicode);
        assert_eq!(kind("\"a\nb\""), ParseErrorKind::ControlCharacter);
        assert_eq!(kind("\"abc"), ParseErrorKind::UnexpectedEnd);
        assert_eq!(kind("[1] x"), ParseErrorKind::TrailingCharacters);
        assert_eq!(kind("é"), ParseErrorKind::UnexpectedChar('é'));
        assert_eq!(parse_bytes(b"\"\xff\"").unwrap_err().kind, ParseErrorKind::InvalidUtf8);
        // Errors render as text.
        assert!(parse("[1,]").unwrap_err().to_string().contains("byte 3"));
    }

    #[test]
    fn limits_nesting() {
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse(&deep).is_ok());
        let too_deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert_eq!(kind(&too_deep), ParseErrorKind::TooDeep);
        let objects = format!("{}1{}", "{\"a\":".repeat(MAX_DEPTH + 5), "}".repeat(MAX_DEPTH + 5));
        assert_eq!(kind(&objects), ParseErrorKind::TooDeep);
    }

    #[test]
    fn keeps_the_last_duplicate_key() {
        let v = parse(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        assert_eq!(v["a"].as_i64(), Some(3));
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn whitespace_everywhere() {
        let v = parse(" \t\r\n{ \"a\" : [ 1 , 2 ] , \"b\" : { } } \n").unwrap();
        assert_eq!(v["a"][1].as_i64(), Some(2));
    }

    #[test]
    fn never_panics_on_random_input() {
        // A tiny deterministic fuzz: random byte strings and mutations of a
        // valid document must parse or fail cleanly.
        let base = br#"{"a":[1,2.5e-3,"x\u00e9\n",true,null,{"b":-0}],"c":"\ud83d\ude00"}"#;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..20_000 {
            let mut doc = base.to_vec();
            let edits = 1 + (next() % 4) as usize;
            for _ in 0..edits {
                let pos = (next() as usize) % doc.len();
                match next() % 3 {
                    0 => doc[pos] = (next() & 0x7f) as u8,
                    1 => {
                        doc.remove(pos);
                    }
                    _ => {
                        let alphabet = b"{}[]\",:\\u0e-.";
                        doc.insert(pos, alphabet[(next() % alphabet.len() as u64) as usize])
                    }
                }
                if doc.is_empty() {
                    doc.push(b'[');
                }
            }
            let _ = parse_bytes(&doc);
            if round % 7 == 0 {
                let len = (next() % 40) as usize;
                let junk: alloc::vec::Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
                let _ = parse_bytes(&junk);
            }
        }
    }
}
