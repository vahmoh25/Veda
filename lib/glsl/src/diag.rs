//! Diagnostics: errors and warnings with their source locations, and the
//! information log that `glGetShaderInfoLog` / `glGetProgramInfoLog` return.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

/// A position in the shader's source: the source string's number (`__FILE__`,
/// counting from 0) and the line within it (`__LINE__`, counting from 1), as
/// adjusted by `#line`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Loc {
    pub string: u32,
    pub line: u32,
}

impl Loc {
    /// A location for diagnostics that belong to no source line (linking).
    pub const NONE: Loc = Loc { string: 0, line: 0 };

    pub fn new(string: u32, line: u32) -> Loc {
        Loc { string, line }
    }
}

/// How serious a diagnostic is. Only errors make compilation fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

/// One message for the information log.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub loc: Loc,
    pub message: String,
}

/// The most messages kept. A shader with more errors is not worth reading
/// further, and a hostile one cannot make the log grow without bound.
const MAX_MESSAGES: usize = 100;

/// The diagnostics of one compilation or link, in the order they were found.
#[derive(Default, Debug)]
pub struct Diagnostics {
    list: Vec<Diagnostic>,
    errors: usize,
    dropped: usize,
}

impl Diagnostics {
    pub fn new() -> Diagnostics {
        Diagnostics::default()
    }

    /// Records an error.
    pub fn error(&mut self, loc: Loc, message: fmt::Arguments<'_>) {
        self.errors += 1;
        self.push(Severity::Error, loc, message);
    }

    /// Records a warning.
    pub fn warn(&mut self, loc: Loc, message: fmt::Arguments<'_>) {
        self.push(Severity::Warning, loc, message);
    }

    fn push(&mut self, severity: Severity, loc: Loc, message: fmt::Arguments<'_>) {
        if self.list.len() >= MAX_MESSAGES {
            self.dropped += 1;
            return;
        }
        let mut text = String::new();
        let _ = text.write_fmt(message);
        self.list.push(Diagnostic { severity, loc, message: text });
    }

    /// Whether any error was recorded.
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// The number of errors recorded (including dropped ones).
    pub fn error_count(&self) -> usize {
        self.errors
    }

    /// Whether the log is full: callers may stop looking for more errors.
    pub fn saturated(&self) -> bool {
        self.list.len() >= MAX_MESSAGES
    }

    /// The recorded messages.
    pub fn messages(&self) -> &[Diagnostic] {
        &self.list
    }

    /// The information log: one line per message, `ERROR: 0:12: ...` as most
    /// OpenGL implementations write them (string number, then line).
    pub fn log(&self) -> String {
        let mut out = String::new();
        for d in &self.list {
            let kind = match d.severity {
                Severity::Error => "ERROR",
                Severity::Warning => "WARNING",
            };
            let _ = if d.loc == Loc::NONE {
                writeln!(out, "{kind}: {}", d.message)
            } else {
                writeln!(out, "{kind}: {}:{}: {}", d.loc.string, d.loc.line, d.message)
            };
        }
        if self.dropped > 0 {
            let _ = writeln!(out, "ERROR: {} more messages not shown", self.dropped);
        }
        out
    }

    /// Moves `other`'s messages after this one's.
    pub fn append(&mut self, other: Diagnostics) {
        self.errors += other.errors;
        self.dropped += other.dropped;
        for d in other.list {
            if self.list.len() >= MAX_MESSAGES {
                self.dropped += 1;
            } else {
                self.list.push(d);
            }
        }
    }
}

/// Records an error: `error!(diags, loc, "format", args...)`.
macro_rules! error {
    ($diags:expr, $loc:expr, $($arg:tt)*) => {
        $diags.error($loc, format_args!($($arg)*))
    };
}

/// Records a warning: `warning!(diags, loc, "format", args...)`.
macro_rules! warning {
    ($diags:expr, $loc:expr, $($arg:tt)*) => {
        $diags.warn($loc, format_args!($($arg)*))
    };
}

pub(crate) use {error, warning};
