//! PS/2 driver for the i8042 controller: keyboard (scan code set 1 via the
//! controller's translation) and mouse (with IntelliMouse wheel when
//! available). Events are forwarded to the `input` service.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use vabi::irq_flags;
use vproto::input::{self, InputEvent, keys};
use vrt::object::{Interrupt, IoPorts, Resource};
use vrt::println;

vrt::entry!(main);

const IOPORT_RESOURCE: u32 = vabi::startup::role::USER + 1;
const IRQ_RESOURCE: u32 = vabi::startup::role::USER + 2;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

struct Controller {
    io: IoPorts,
}

impl Controller {
    fn wait_write(&self) {
        for _ in 0..100_000 {
            if self.io.in8(STATUS) & 2 == 0 {
                return;
            }
        }
    }

    fn wait_read(&self) -> bool {
        for _ in 0..100_000 {
            if self.io.in8(STATUS) & 1 != 0 {
                return true;
            }
        }
        false
    }

    fn command(&self, c: u8) {
        self.wait_write();
        self.io.out8(COMMAND, c);
    }

    fn write_data(&self, d: u8) {
        self.wait_write();
        self.io.out8(DATA, d);
    }

    fn read_data(&self) -> Option<u8> {
        if self.wait_read() { Some(self.io.in8(DATA)) } else { None }
    }

    fn flush(&self) {
        for _ in 0..32 {
            if self.io.in8(STATUS) & 1 == 0 {
                break;
            }
            let _ = self.io.in8(DATA);
        }
    }

    /// Sends a byte to the mouse and waits for its acknowledgement.
    fn mouse_write(&self, b: u8) -> bool {
        self.command(0xD4);
        self.write_data(b);
        self.read_data() == Some(0xFA)
    }

    fn init(&self) -> bool {
        self.command(0xAD); // disable keyboard port
        self.command(0xA7); // disable mouse port
        self.flush();
        self.command(0x20);
        let mut config = self.read_data().unwrap_or(0);
        // Enable both IRQs and scan-code translation; enable both clocks.
        config |= 0b0100_0011;
        config &= !0b0011_0000;
        self.command(0x60);
        self.write_data(config);
        self.command(0xAE);
        self.command(0xA8);
        // Mouse: defaults, try to enable the scroll wheel, enable reporting.
        let wheel = self.mouse_write(0xF6)
            && self.mouse_write(0xF3)
            && self.mouse_write(200)
            && self.mouse_write(0xF3)
            && self.mouse_write(100)
            && self.mouse_write(0xF3)
            && self.mouse_write(80)
            && self.mouse_write(0xF2)
            && self.read_data() == Some(3);
        self.mouse_write(0xF4);
        self.flush();
        wheel
    }
}

/// Maps a set-1 scan code (with E0 prefix flag) to an evdev key code.
fn translate(code: u8, extended: bool) -> Option<u16> {
    if !extended {
        return (code < 0x59).then_some(code as u16);
    }
    Some(match code {
        0x1C => keys::KPENTER,
        0x1D => keys::RIGHTCTRL,
        0x35 => keys::KPSLASH,
        0x38 => keys::RIGHTALT,
        0x47 => keys::HOME,
        0x48 => keys::UP,
        0x49 => keys::PAGEUP,
        0x4B => keys::LEFT,
        0x4D => keys::RIGHT,
        0x4F => keys::END,
        0x50 => keys::DOWN,
        0x51 => keys::PAGEDOWN,
        0x52 => keys::INSERT,
        0x53 => keys::DELETE,
        0x5B => keys::LEFTMETA,
        0x5C => keys::RIGHTMETA,
        0x5D => keys::COMPOSE,
        _ => return None,
    })
}

fn main() -> i32 {
    let (Some(io_res), Some(irq_res)) = (
        vrt::env::take_handle(IOPORT_RESOURCE).map(Resource::from_handle),
        vrt::env::take_handle(IRQ_RESOURCE).map(Resource::from_handle),
    ) else {
        println!("missing resources");
        return 1;
    };
    let Ok(io) = IoPorts::create(&io_res, 0x60, 5) else {
        println!("cannot access the i8042 ports");
        return 1;
    };
    let ctl = Controller { io };
    let wheel = ctl.init();
    let (Ok(kbd_irq), Ok(mouse_irq)) =
        (Interrupt::create(&irq_res, 1, irq_flags::ISA), Interrupt::create(&irq_res, 12, irq_flags::ISA))
    else {
        println!("cannot bind IRQ 1/12");
        return 1;
    };
    println!("keyboard and mouse ready{}", if wheel { " (wheel)" } else { "" });
    let mut input = match input::InputSink::connect() {
        Ok(sink) => sink,
        Err(e) => {
            println!("cannot reach the input service: {:?}", e);
            return 1;
        }
    };

    let mut extended = false;
    let mut packet = [0u8; 4];
    let mut packet_len = 0usize;
    let packet_size = if wheel { 4 } else { 3 };
    let mut buttons = 0u8;
    loop {
        let mut items = [
            vabi::WaitItem { handle: kbd_irq.raw(), signals: vabi::signals::SIGNALED, ..Default::default() },
            vabi::WaitItem { handle: mouse_irq.raw(), signals: vabi::signals::SIGNALED, ..Default::default() },
        ];
        let _ = vrt::object::wait_many(&mut items, vabi::DEADLINE_INFINITE);
        // Edge-triggered: re-arm before draining so no interrupt is lost.
        let _ = kbd_irq.ack();
        let _ = mouse_irq.ack();
        let mut events: Vec<InputEvent> = Vec::new();
        // Drain the controller's output buffer.
        loop {
            let status = ctl.io.in8(STATUS);
            if status & 1 == 0 {
                break;
            }
            let byte = ctl.io.in8(DATA);
            if status & 0x20 != 0 {
                // Mouse byte. Resynchronise on the "always one" bit.
                if packet_len == 0 && byte & 0x08 == 0 {
                    continue;
                }
                packet[packet_len] = byte;
                packet_len += 1;
                if packet_len == packet_size {
                    packet_len = 0;
                    let flags = packet[0];
                    let dx = packet[1] as i32 - if flags & 0x10 != 0 { 256 } else { 0 };
                    let dy = packet[2] as i32 - if flags & 0x20 != 0 { 256 } else { 0 };
                    if dx != 0 || dy != 0 {
                        events.push(InputEvent::Motion { dx, dy: -dy });
                    }
                    for b in 0..3u8 {
                        let now = flags & (1 << b) != 0;
                        if now != (buttons & (1 << b) != 0) {
                            let button = match b {
                                0 => 0,
                                1 => 1,
                                _ => 2,
                            };
                            events.push(InputEvent::Button { button, pressed: now });
                        }
                    }
                    buttons = flags & 7;
                    if wheel {
                        let z = (packet[3] & 0x0F) as i8;
                        let z = if z & 0x08 != 0 { z | !0x0F } else { z };
                        if z != 0 {
                            events.push(InputEvent::Scroll { dx: 0, dy: -(z as i32) });
                        }
                    }
                }
            } else if byte == 0xE0 {
                extended = true;
            } else {
                let pressed = byte & 0x80 == 0;
                if let Some(code) = translate(byte & 0x7F, extended) {
                    events.push(InputEvent::Key { code, pressed });
                }
                extended = false;
            }
        }
        if !events.is_empty() {
            input.report(events);
        }
    }
}
