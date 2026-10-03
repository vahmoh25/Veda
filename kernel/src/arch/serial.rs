//! Polled COM1 serial output for the kernel log.

use super::port::{inb, outb};

const COM1: u16 = 0x3F8;

pub fn init() {
    // SAFETY: programming the standard COM1 UART.
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

pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

fn write_byte(b: u8) {
    // SAFETY: polling and writing the initialised COM1 UART.
    unsafe {
        let mut spins = 0u32;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 1_000_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        outb(COM1, b);
    }
}
