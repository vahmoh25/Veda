//! Kernel log: formatted messages go to an in-memory ring buffer that user
//! space can read with the `log_read` system call, and from it to the
//! serial port, as fast as the port takes them (`arch::serial`): never
//! waited for, the rest given to it from the timer ([`pump_serial`]).

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};

use crate::arch::serial;
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

static MIN_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_level(level: Level) {
    MIN_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 >= MIN_LEVEL.load(Ordering::Relaxed)
}

const RING_SIZE: usize = 256 * 1024;

/// What a panic gives the serial port of what it did not have yet: the
/// last of it (a third of a second at 115200 baud).
const PANIC_BACKLOG: u64 = 4096;

struct Ring {
    buf: [u8; RING_SIZE],
    /// Total bytes ever written; the buffer holds the last `RING_SIZE`.
    head: u64,
    /// How far the serial port has it, and whether it has the carriage
    /// return before the line feed there.
    sent: u64,
    cr_sent: bool,
}

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.buf[(self.head % RING_SIZE as u64) as usize] = b;
            self.head += 1;
        }
    }

    /// The next byte for the serial port, a carriage return before a line
    /// feed (`None`: it has everything).
    fn next_for_serial(&mut self) -> Option<u8> {
        // What the ring no longer holds is lost to it.
        let oldest = self.head.saturating_sub(RING_SIZE as u64);
        if self.sent < oldest {
            (self.sent, self.cr_sent) = (oldest, false);
        }
        if self.sent == self.head {
            return None;
        }
        let b = self.buf[(self.sent % RING_SIZE as u64) as usize];
        if b == b'\n' && !self.cr_sent {
            self.cr_sent = true;
            return Some(b'\r');
        }
        self.cr_sent = false;
        self.sent += 1;
        Some(b)
    }

    /// Gives the serial port what it takes now of what it does not have.
    fn pump(&mut self) {
        if !serial::present() {
            (self.sent, self.cr_sent) = (self.head, false);
            return;
        }
        while self.sent < self.head {
            let room = serial::room();
            if room == 0 {
                return;
            }
            for _ in 0..room {
                match self.next_for_serial() {
                    Some(b) => serial::put(b),
                    None => return,
                }
            }
        }
    }
}

static RING: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; RING_SIZE], head: 0, sent: 0, cr_sent: false });

struct Sink<'a>(&'a mut Ring);

impl Write for Sink<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.push(s.as_bytes());
        Ok(())
    }
}

/// Gives the serial port more of the log (the timer's).
pub fn pump_serial() {
    RING.lock().pump();
}

/// Whether the serial port has not all of the log yet.
pub fn serial_pending() -> bool {
    let ring = RING.lock();
    ring.sent < ring.head
}

/// Writes one log record. Use the `kinfo!`-style macros instead.
pub fn write(level: Level, args: fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    let ns = crate::time::now_ns();
    let cpu = crate::arch::percpu::cpu_id_or_boot();
    let tag = match level {
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    };
    let mut ring = RING.lock();
    let _ = writeln!(
        Sink(&mut ring),
        "[{:5}.{:06}] cpu{} {:5} {}",
        ns / 1_000_000_000,
        (ns / 1000) % 1_000_000,
        cpu,
        tag,
        args
    );
    ring.pump();
}

/// Raw text from user space (`debug_write`), prefixed with the process name.
pub fn write_user(process: &str, text: &[u8]) {
    let ns = crate::time::now_ns();
    let mut ring = RING.lock();
    let mut sink = Sink(&mut ring);
    for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let line = core::str::from_utf8(line).unwrap_or("<invalid utf-8>");
        let _ = writeln!(sink, "[{:5}.{:06}] {:>10}: {}", ns / 1_000_000_000, (ns / 1000) % 1_000_000, process, line);
    }
    ring.pump();
}

/// Copies log bytes starting at absolute `offset` into `out`. Returns the
/// number of bytes copied and the offset to continue from. Offsets older than
/// the ring's capacity are advanced to the oldest retained byte.
pub fn read(offset: u64, out: &mut [u8]) -> (usize, u64) {
    let ring = RING.lock();
    let oldest = ring.head.saturating_sub(RING_SIZE as u64);
    let mut pos = offset.clamp(oldest, ring.head);
    let mut n = 0;
    while pos < ring.head && n < out.len() {
        out[n] = ring.buf[(pos % RING_SIZE as u64) as usize];
        n += 1;
        pos += 1;
    }
    (n, pos)
}

/// Writes directly to the serial port, waiting for it, after the last of
/// what it did not have of the log ([`PANIC_BACKLOG`]): the panic's path,
/// past every lock (the log's too, if a processor that stopped holds it).
pub fn emergency(args: fmt::Arguments) {
    struct Raw;
    impl Write for Raw {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            serial::write_blocking(s.as_bytes());
            Ok(())
        }
    }
    if let Some(mut ring) = RING.try_lock() {
        let last = ring.head.saturating_sub(PANIC_BACKLOG);
        if ring.sent < last {
            (ring.sent, ring.cr_sent) = (last, false);
        }
        while let Some(b) = ring.next_for_serial() {
            serial::put_blocking(b);
        }
    }
    let _ = Raw.write_fmt(args);
}

#[macro_export]
macro_rules! kdebug {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Debug, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! kinfo {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! kwarn {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Warn, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! kerror {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($arg)*)) };
}
