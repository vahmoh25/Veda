//! Text output to the system log (`print!`, `println!`, `log!`).

use alloc::string::String;
use core::fmt::{self, Write};

use crate::sys;

/// Writes raw text to the kernel log (tagged with the process name).
pub fn write_str(s: &str) {
    sys::debug_write(s.as_bytes());
}

struct LineBuffer(String);

impl Write for LineBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.push_str(s);
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut buf = LineBuffer(String::new());
    let _ = buf.write_fmt(args);
    write_str(&buf.0);
}

/// Prints to the system log.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::_print(format_args!($($arg)*)) };
}

/// Prints a line to the system log.
#[macro_export]
macro_rules! println {
    () => { $crate::io::_print(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::io::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

/// Alias of `println!` for diagnostics.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => { $crate::println!($($arg)*) };
}
