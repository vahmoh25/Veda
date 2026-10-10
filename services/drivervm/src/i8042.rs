//! The PC's keyboard controller (the i8042), when devmgr gives it the
//! guest: Linux's driver of it reaches its two ports through the monitor,
//! which carries the reads and writes out on the controller, but what would
//! reset the machine (the controller also drives the processor's reset
//! line). Its interrupts are the guest's lines 1 and 12 (the keyboard's and
//! the mouse's, edges: see `lines`). The ports around them (0x61's NMI
//! controls among them) the guest does not reach.

use vrt::object::IoPorts;
use vrt::sync::Mutex;

/// The controller's data port, and its status and command port; the
/// keyboard's and the mouse's interrupt lines.
pub use vhv::acpi::{I8042_COMMAND as COMMAND, I8042_DATA as DATA, KEYBOARD_GSI, MOUSE_GSI};

/// The controller's commands that write its output port, whose lines
/// reset the processor (bit 0, low) and gate A20 (bit 1), and that turn
/// A20 off.
const WRITE_OUTPUT: u8 = 0xD1;
const A20_OFF: u8 = 0xDD;

pub struct I8042 {
    data: IoPorts,
    command: IoPorts,
    /// The next data byte is the output port's (after `WRITE_OUTPUT`).
    output_next: Mutex<bool>,
}

impl I8042 {
    pub fn new(data: IoPorts, command: IoPorts) -> I8042 {
        I8042 { data, command, output_next: Mutex::new(false) }
    }

    /// Reads a port of the controller's.
    pub fn read(&self, port: u16) -> u8 {
        if port == COMMAND { self.command.in8(port) } else { self.data.in8(port) }
    }

    /// Carries out the guest's write of a port of the controller's, but
    /// for pulses of the reset line (commands 0xF0 to 0xFF with bit 0
    /// clear) and A20 off; a value of the output port goes with the reset
    /// and A20 lines high.
    pub fn write(&self, port: u16, value: u8) {
        let mut output_next = self.output_next.lock();
        if port == COMMAND {
            *output_next = value == WRITE_OUTPUT;
            if value != A20_OFF && value & 0xF1 != 0xF0 {
                self.command.out8(port, value);
            }
        } else {
            let value = if core::mem::take(&mut *output_next) { value | 0b11 } else { value };
            self.data.out8(port, value);
        }
    }
}
