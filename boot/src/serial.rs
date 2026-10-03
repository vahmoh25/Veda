//! Polled 16550 UART output on COM1, usable both before and after
//! `ExitBootServices`.

use core::arch::asm;
use core::fmt;

const COM1: u16 = 0x3F8;

unsafe fn outb(port: u16, value: u8) {
    // SAFETY: the caller guarantees `port` belongs to the UART.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) }
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: as for `outb`.
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) }
    value
}

/// Programs COM1 for 115200 baud, 8N1, FIFOs enabled.
pub fn init() {
    // SAFETY: COM1 is the standard PC serial port; reprogramming it is
    // harmless even if the firmware also uses it.
    unsafe {
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x80);
        outb(COM1, 0x01);
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03);
        outb(COM1 + 2, 0xC7);
        outb(COM1 + 4, 0x0B);
    }
}

fn write_byte(b: u8) {
    // SAFETY: polling the line status register and writing the data register
    // of an initialised UART.
    unsafe {
        let mut spins = 0u32;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        outb(COM1, b);
    }
}

pub struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                write_byte(b'\r');
            }
            write_byte(b);
        }
        Ok(())
    }
}

/// Logs a formatted line to the serial console.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::serial::Serial, "[vboot] {}", format_args!($($arg)*));
    }};
}
