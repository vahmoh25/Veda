//! The values AML computes with.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::name::Path;

/// An AML data object, or a reference to one.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// Nothing yet (a local before its first store, a method that returned
    /// nothing).
    #[default]
    Uninitialized,
    Integer(u64),
    String(String),
    Buffer(Vec<u8>),
    Package(Vec<Value>),
    /// A named object: a name inside a package, or what `RefOf` returns.
    Reference(Path),
    /// What `Index` returns: element `.1` of the buffer, string or package
    /// held by the place.
    Element(Box<Place>, usize),
}

/// Where a value lives, for the operators that change part of it
/// (`Index`, `CreateDWordField`).
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    Local(u8),
    Arg(u8),
    Named(Path),
    /// A value computed on the spot: changes to it go nowhere.
    Temporary(Value),
}

impl Value {
    /// The value as an integer, converting as AML does: a buffer's first
    /// eight bytes (little endian), a string's hexadecimal digits.
    pub fn as_integer(&self) -> Option<u64> {
        match self {
            Value::Integer(v) => Some(*v),
            Value::Uninitialized => Some(0),
            Value::Buffer(b) => Some(b.iter().take(8).rev().fold(0u64, |v, &x| v << 8 | x as u64)),
            Value::String(s) => {
                let s = s.trim();
                let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                let digits = &digits[..digits.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(digits.len())];
                if digits.is_empty() {
                    Some(0)
                } else {
                    u64::from_str_radix(&digits[digits.len().saturating_sub(16)..], 16).ok()
                }
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_buffer(&self) -> Option<&[u8]> {
        match self {
            Value::Buffer(b) => Some(b),
            _ => None,
        }
    }

    /// The bytes of a buffer, string or integer (for `ToBuffer` and the
    /// comparisons and concatenations that work on bytes).
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Value::Buffer(b) => Some(b.clone()),
            Value::String(s) => Some(s.as_bytes().to_vec()),
            Value::Integer(v) => Some(v.to_le_bytes().to_vec()),
            Value::Uninitialized => Some(Vec::new()),
            _ => None,
        }
    }

    /// The type number `ObjectType` reports.
    pub fn type_code(&self) -> u64 {
        match self {
            Value::Uninitialized => 0,
            Value::Integer(_) => 1,
            Value::String(_) => 2,
            Value::Buffer(_) => 3,
            Value::Package(_) => 4,
            Value::Reference(_) | Value::Element(..) => 0,
        }
    }
}
