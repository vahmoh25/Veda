//! The writer: compact or indented JSON.

use alloc::string::String;
use core::fmt::Write;

use crate::{Number, Value};

/// Appends `v` to `out`. `indent` is `None` for compact output, or the
/// current depth for indented output.
pub(crate) fn write_value(out: &mut String, v: &Value, indent: Option<usize>) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, *n),
        Value::String(s) => write_str(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent.map(|d| d + 1));
                write_value(out, item, indent.map(|d| d + 1));
            }
            newline(out, indent);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent.map(|d| d + 1));
                write_str(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent.map(|d| d + 1));
            }
            newline(out, indent);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>) {
    if let Some(depth) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    }
}

fn write_number(out: &mut String, n: Number) {
    match n {
        Number::Int(i) => {
            let _ = write!(out, "{}", i);
        }
        Number::UInt(u) => {
            let _ = write!(out, "{}", u);
        }
        Number::Float(f) => {
            if !f.is_finite() {
                out.push_str("null");
                return;
            }
            let a = if f < 0.0 { -f } else { f };
            if a != 0.0 && !(1e-5..1e16).contains(&a) {
                // Exponent form for very large and very small magnitudes
                // ("1e300", "2.5e-7"), which is valid JSON.
                let _ = write!(out, "{:e}", f);
            } else {
                let start = out.len();
                let _ = write!(out, "{}", f);
                // Keep floats recognisable as floats ("2.0", not "2").
                if !out[start..].contains('.') {
                    out.push_str(".0");
                }
            }
        }
    }
}

/// Appends `s` as a quoted JSON string.
pub(crate) fn write_str(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: Option<&str> = match b {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            0x08 => Some("\\b"),
            0x0c => Some("\\f"),
            0..=0x1f => None,
            _ => continue,
        };
        out.push_str(&s[start..i]);
        match esc {
            Some(e) => out.push_str(e),
            None => {
                let _ = write!(out, "\\u{:04x}", b);
            }
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use crate::{Value, object, parse};
    use alloc::string::ToString;

    #[test]
    fn writes_floats_readably() {
        let cases: [(f64, &str); 7] = [
            (0.0, "0.0"),
            (1.0, "1.0"),
            (-2.5, "-2.5"),
            (1e300, "1e300"),
            (2.5e-7, "2.5e-7"),
            (1234.5, "1234.5"),
            (0.1, "0.1"),
        ];
        for (f, text) in cases {
            assert_eq!(Value::from(f).to_string(), text);
            assert_eq!(parse(text).unwrap().as_f64(), Some(f));
        }
    }

    #[test]
    fn pretty_output_is_indented() {
        let v = object! { "a" => alloc::vec![1, 2], "b" => object! {}, "c" => Value::array() };
        assert_eq!(v.pretty(), "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": []\n}");
    }
}
