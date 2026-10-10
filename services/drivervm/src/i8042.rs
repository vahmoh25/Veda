//! The PC's keyboard controller (the i8042), when devmgr gives it the
//! guest: Linux's driver of it reaches its two ports through the monitor,
//! which carries the reads and writes out on the controller, but what would
//! reset the machine (the controller also drives the processor's reset
//! line); its interrupts, edges, go to the guest's processors as its
//! functions' MSIs do. The ports around them (0x61's NMI controls among
//! them) the guest does not reach.

use vrt::object::{Interrupt, IoPorts, Vcpu};
use vrt::sync::Mutex;

/// The controller's data port, and its status and command port.
pub const DATA: u16 = 0x60;
pub const COMMAND: u16 = 0x64;

/// The controller's commands that write its output port, whose lines
/// reset the processor (bit 0, low) and gate A20 (bit 1), and that turn
/// A20 off.
const WRITE_OUTPUT: u8 = 0xD1;
const A20_OFF: u8 = 0xDD;

pub struct I8042 {
    data: IoPorts,
    command: IoPorts,
    /// The keyboard's interrupt (ISA 1) and the mouse's (ISA 12).
    keyboard: Interrupt,
    mouse: Interrupt,
    /// The next data byte is the output port's (after `WRITE_OUTPUT`).
    output_next: Mutex<bool>,
}

impl I8042 {
    pub fn new(data: IoPorts, command: IoPorts, keyboard: Interrupt, mouse: Interrupt) -> I8042 {
        I8042 { data, command, keyboard, mouse, output_next: Mutex::new(false) }
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

    /// Routes ISA interrupt `irq` (1 or 12) to `vector` of `vcpu` (0: to
    /// nothing).
    pub fn route(&self, irq: u64, vcpu: &Vcpu, vector: u64) -> bool {
        let interrupt = match irq {
            1 => &self.keyboard,
            12 => &self.mouse,
            _ => return false,
        };
        u8::try_from(vector).is_ok_and(|v| vcpu.bind_interrupt(interrupt, v).is_ok())
    }
}
