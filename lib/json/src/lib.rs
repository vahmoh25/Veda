//! `vjson` — JSON for Veda.
//!
//! * [`Value`]: a JSON document in memory. Objects keep their members in
//!   insertion order ([`Map`]), so what a program builds is written back
//!   in the same order, which keeps protocol messages and saved files
//!   readable.
//! * [`parse`] reads a document (RFC 8259) with a nesting limit and precise
//!   error positions; it never panics on malformed input.
//! * [`Value::to_string`] / [`Value::pretty`] write compact or indented
//!   JSON ([`core::fmt::Display`] writes the compact form).
//! * [`object!`] and [`array!`] build values; [`From`] converts the usual
//!   Rust types.
//!
//! ```ignore
//! let msg = vjson::object! { "type" => "KeepAlive" };
//! let reply = vjson::parse(r#"{"type":"Welcome","request_id":"1"}"#)?;
//! assert_eq!(reply.get("type").and_then(Value::as_str), Some("Welcome"));
//! ```

#![no_std]

extern crate alloc;

mod parse;
mod write;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

pub use parse::{ParseError, ParseErrorKind, parse, parse_bytes};

/// Deepest nesting of arrays and objects [`parse`] accepts.
pub const MAX_DEPTH: usize = 128;

/// A JSON number. Integers keep their exact value; everything else is an
/// `f64`.
#[derive(Debug, Clone, Copy)]
pub enum Number {
    Int(i64),
    UInt(u64),
    Float(f64),
}

impl Number {
    pub fn as_f64(&self) -> f64 {
        match *self {
            Number::Int(i) => i as f64,
            Number::UInt(u) => u as f64,
            Number::Float(f) => f,
        }
    }

    /// The value as an `i64`, if it is a whole number in range.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Number::Int(i) => Some(i),
            Number::UInt(u) => i64::try_from(u).ok(),
            Number::Float(f) => float_to_int(f).filter(|v| (i64::MIN as f64..=i64::MAX as f64).contains(&(*v as f64))),
        }
    }

    /// The value as a `u64`, if it is a non-negative whole number in range.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Number::Int(i) => u64::try_from(i).ok(),
            Number::UInt(u) => Some(u),
            Number::Float(f) => float_to_int(f).and_then(|v| u64::try_from(v).ok()),
        }
    }
}

/// `f` as an integer if it has no fractional part and fits an `i64`.
fn float_to_int(f: f64) -> Option<i64> {
    if f.is_finite() && f >= i64::MIN as f64 && f < i64::MAX as f64 && (f as i64) as f64 == f {
        Some(f as i64)
    } else {
        None
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        match (self.as_i64(), other.as_i64()) {
            (Some(a), Some(b)) => a == b,
            _ => match (self.as_u64(), other.as_u64()) {
                (Some(a), Some(b)) => a == b,
                _ => self.as_f64() == other.as_f64(),
            },
        }
    }
}

/// The members of a JSON object, in insertion order. Setting an existing
/// key replaces its value in place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map {
    entries: Vec<(String, Value)>,
}

impl Map {
    pub fn new() -> Map {
        Map { entries: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Sets `key` (replacing an existing value, which is returned).
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => Some(core::mem::replace(v, value)),
            None => {
                self.entries.push((key, value));
                None
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(i).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// Adds a member without checking for an existing key (the parser
    /// keeps the last of duplicate keys through [`Map::insert`] instead).
    fn push(&mut self, key: String, value: Value) {
        self.entries.push((key, value));
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Map {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Map {
        let mut m = Map::new();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

impl IntoIterator for Map {
    type Item = (String, Value);
    type IntoIter = alloc::vec::IntoIter<(String, Value)>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

/// A JSON value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Map),
}

static NULL: Value = Value::Null;

impl Value {
    /// An empty object.
    pub fn object() -> Value {
        Value::Object(Map::new())
    }

    /// An empty array.
    pub fn array() -> Value {
        Value::Array(Vec::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// The member `key` of an object (`None` for other values).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }

    /// The string member `key` of an object.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// Element `i` of an array.
    pub fn at(&self, i: usize) -> Option<&Value> {
        self.as_array()?.get(i)
    }

    /// The value at a JSON pointer (RFC 6901), e.g. `"/agent/think/prompt"`.
    pub fn pointer(&self, pointer: &str) -> Option<&Value> {
        if pointer.is_empty() {
            return Some(self);
        }
        let mut v = self;
        for raw in pointer.strip_prefix('/')?.split('/') {
            let token = raw.replace("~1", "/").replace("~0", "~");
            v = match v {
                Value::Object(m) => m.get(&token)?,
                Value::Array(a) => a.get(token.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(v)
    }

    /// Sets member `key` of an object; a non-object becomes an object first.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Value {
        if !matches!(self, Value::Object(_)) {
            *self = Value::object();
        }
        if let Value::Object(m) = self {
            m.insert(key, value);
        }
        self
    }

    /// Builder form of [`Value::set`].
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Value {
        self.set(key, value);
        self
    }

    /// Appends to an array; a non-array becomes an array first.
    pub fn push(&mut self, value: impl Into<Value>) {
        if !matches!(self, Value::Array(_)) {
            *self = Value::array();
        }
        if let Value::Array(a) = self {
            a.push(value.into());
        }
    }

    /// Indented JSON (two spaces), for files people read.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        write::write_value(&mut out, self, Some(0));
        out
    }

    /// A short description of the kind of value, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

impl core::ops::Index<&str> for Value {
    type Output = Value;
    /// The member `key`, or `null` if it is missing (never panics).
    fn index(&self, key: &str) -> &Value {
        self.get(key).unwrap_or(&NULL)
    }
}

impl core::ops::Index<usize> for Value {
    type Output = Value;
    /// Element `i`, or `null` if it is missing (never panics).
    fn index(&self, i: usize) -> &Value {
        self.at(i).unwrap_or(&NULL)
    }
}

impl core::fmt::Display for Value {
    /// Compact JSON.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut out = String::new();
        write::write_value(&mut out, self, None);
        f.write_str(&out)
    }
}

/// Writes `s` as a JSON string literal (with quotes) to `out`.
pub fn write_string(out: &mut String, s: &str) {
    write::write_str(out, s);
}

/// `s` as a JSON string literal (with quotes).
pub fn quote(s: &str) -> String {
    let mut out = String::new();
    write::write_str(&mut out, s);
    out
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

macro_rules! from_int {
    ($variant:ident, $wide:ty, $($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(n: $t) -> Value {
                Value::Number(Number::$variant(n as $wide))
            }
        }
    )*};
}
from_int!(Int, i64, i8, i16, i32, i64, isize);
from_int!(UInt, u64, u8, u16, u32, u64, usize);

impl From<f64> for Value {
    /// Non-finite numbers have no JSON form and become `null`.
    fn from(f: f64) -> Value {
        if f.is_finite() { Value::Number(Number::Float(f)) } else { Value::Null }
    }
}

impl From<f32> for Value {
    /// The shortest decimal that reads back as the same `f32` (0.85, not
    /// 0.8500000238418579).
    fn from(f: f32) -> Value {
        alloc::format!("{f}").parse::<f64>().map_or(Value::Null, Value::from)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(s)
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Value {
        Value::String(s.clone())
    }
}

impl From<Map> for Value {
    fn from(m: Map) -> Value {
        Value::Object(m)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Value {
        Value::Array(v.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Null, Into::into)
    }
}

impl<T: Into<Value>> FromIterator<T> for Value {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Value {
        Value::Array(iter.into_iter().map(Into::into).collect())
    }
}

/// Builds an object: `object! { "type" => "Settings", "n" => 3 }`.
#[macro_export]
macro_rules! object {
    () => { $crate::Value::object() };
    ($($key:expr => $value:expr),+ $(,)?) => {{
        let mut m = $crate::Map::new();
        $( m.insert($key, $value); )+
        $crate::Value::Object(m)
    }};
}

/// Builds an array: `array!["a", 2, true]`.
#[macro_export]
macro_rules! array {
    () => { $crate::Value::array() };
    ($($value:expr),+ $(,)?) => {{
        let mut a = $crate::Value::array();
        $( a.push($value); )+
        a
    }};
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn builds_and_writes_in_insertion_order() {
        let v = object! {
            "type" => "Settings",
            "audio" => object! { "rate" => 16000, "gain" => 0.5 },
            "tags" => vec!["a", "b"],
            "none" => Value::Null,
        };
        assert_eq!(
            v.to_string(),
            r#"{"type":"Settings","audio":{"rate":16000,"gain":0.5},"tags":["a","b"],"none":null}"#
        );
        assert_eq!(v["audio"]["rate"].as_u64(), Some(16000));
        assert!(v["missing"]["deeper"].is_null());
        assert_eq!(v.pointer("/tags/1").and_then(Value::as_str), Some("b"));
        let mut w = v.clone();
        w.set("type", "Other");
        assert_eq!(w.as_object().unwrap().keys().next(), Some("type"));
        assert_eq!(w.str("type"), Some("Other"));
    }

    #[test]
    fn round_trips_through_the_parser() {
        let text = r#"{"a":[1,-2,3.5,1e3,true,false,null],"s":"x\"y\\z\né😀","o":{}}"#;
        let v = parse(text).unwrap();
        assert_eq!(v["a"][1].as_i64(), Some(-2));
        assert_eq!(v["a"][3].as_f64(), Some(1000.0));
        assert_eq!(v["s"].as_str(), Some("x\"y\\z\n\u{e9}\u{1f600}"));
        let again = parse(&v.to_string()).unwrap();
        assert_eq!(v, again);
        let pretty = v.pretty();
        assert!(pretty.contains("\n  \"a\": ["));
        assert_eq!(parse(&pretty).unwrap(), v);
    }

    #[test]
    fn numbers_keep_their_exact_values() {
        let v = parse("[9007199254740993, 18446744073709551615, -9223372036854775808, 0.1, 2.0]").unwrap();
        assert_eq!(v[0].as_u64(), Some(9_007_199_254_740_993));
        assert_eq!(v[1].as_u64(), Some(u64::MAX));
        assert_eq!(v[2].as_i64(), Some(i64::MIN));
        assert_eq!(v[3].as_f64(), Some(0.1));
        assert_eq!(v[4].as_i64(), Some(2));
        assert_eq!(v.to_string(), "[9007199254740993,18446744073709551615,-9223372036854775808,0.1,2.0]");
        assert_eq!(Value::from(f64::NAN), Value::Null);
        assert_eq!(Value::from(1.5f32).to_string(), "1.5");
        assert_eq!(Value::from(0.85f32).to_string(), "0.85");
        assert_eq!(Value::from(1.1f32).as_f64(), Some(1.1));
        assert_eq!(Value::from(f32::INFINITY), Value::Null);
        assert_eq!(Value::from(-0.0).to_string(), "-0.0");
    }

    #[test]
    fn escapes_control_characters() {
        let s = "tab\tnul\u{0}bell\u{7}del\u{7f}\u{2028}";
        let q = quote(s);
        assert_eq!(q, "\"tab\\tnul\\u0000bell\\u0007del\u{7f}\u{2028}\"");
        assert_eq!(parse(&q).unwrap().as_str(), Some(s));
    }
}
