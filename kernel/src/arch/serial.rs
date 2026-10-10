//! COM1, the serial port the kernel log goes to (QEMU's console, which
//! tests read; a cable's).
//!
//! It is given the log as fast as it takes it, never waited for: a PC's
//! UART takes tens of microseconds a character (115200 baud), and the
//! kernel writes its log holding its lock, which would stop every
//! processor's kernel meanwhile. When its transmitter is empty it takes
//! its FIFO's worth (16 bytes, a 16550A's, QEMU's too); `log` gives it the
//! rest later, from the timer.

use core::sync::atomic::{AtomicU8, Ordering};

use super::port::{inb, outb};

const COM1: u16 = 0x3F8;
const THR: u16 = COM1;
const IER: u16 = COM1 + 1;
/// The FIFO control register (written), the interrupt identification
/// register (read).
const FCR: u16 = COM1 + 2;
const IIR: u16 = COM1 + 2;
const LCR: u16 = COM1 + 3;
const MCR: u16 = COM1 + 4;
const LSR: u16 = COM1 + 5;
const SCR: u16 = COM1 + 7;
/// The transmitter is empty (its FIFO, with one).
const LSR_THRE: u8 = 0x20;
/// The FIFOs are on.
const IIR_FIFO: u8 = 0xC0;

/// Bytes the transmitter takes when it is empty: its FIFO's, one without
/// a FIFO, none without a UART.
static ROOM: AtomicU8 = AtomicU8::new(0);

/// Sets COM1 up: 115200 baud, 8 bits, no parity, one stop bit, its FIFOs
/// on. Nothing, without a UART there (its scratch register keeps nothing).
pub fn init() {
    // SAFETY: COM1's registers; without a UART, nothing answers.
    unsafe {
        outb(SCR, 0x5A);
        if inb(SCR) != 0x5A {
            return;
        }
        outb(IER, 0x00);
        outb(LCR, 0x80);
        outb(THR, 0x01);
        outb(IER, 0x00);
        outb(LCR, 0x03);
        outb(FCR, 0xC7);
        outb(MCR, 0x0B);
        let fifo = inb(IIR) & IIR_FIFO == IIR_FIFO;
        ROOM.store(if fifo { 16 } else { 1 }, Ordering::Relaxed);
    }
}

/// Whether there is a UART.
pub fn present() -> bool {
    ROOM.load(Ordering::Relaxed) != 0
}

/// How many bytes the transmitter takes now, without waiting.
pub fn room() -> usize {
    let room = ROOM.load(Ordering::Relaxed) as usize;
    // SAFETY: reading the UART's line status.
    if room == 0 || unsafe { inb(LSR) } & LSR_THRE == 0 { 0 } else { room }
}

/// Sends `b`, which [`room`] made room for.
pub fn put(b: u8) {
    // SAFETY: writing the UART's transmitter.
    unsafe { outb(THR, b) };
}

/// Sends `bytes` (a carriage return before each line feed), waiting for
/// the transmitter: when nothing else is to run again (a panic).
pub fn write_blocking(bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' {
            put_blocking(b'\r');
        }
        put_blocking(b);
    }
}

/// Sends `b`, waiting for the transmitter.
pub fn put_blocking(b: u8) {
    if !present() {
        return;
    }
    let mut spins = 0u32;
    // SAFETY: reading the UART's line status.
    while unsafe { inb(LSR) } & LSR_THRE == 0 && spins < 1_000_000 {
        spins += 1;
        core::hint::spin_loop();
    }
    put(b);
}
