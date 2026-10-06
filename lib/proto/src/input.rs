//! Input events from device drivers to the window system.
//!
//! Drivers connect to the `input` service (provided by the compositor) and
//! send batches of [`InputEvent`]s as one-way messages with ordinal
//! [`REPORT`]. Key codes are Linux evdev codes (see [`keys`]), which is what
//! virtio-input reports natively and what PS/2 set-1 scan codes map to.

use alloc::vec::Vec;
use vipc::union;

pub const NAME: &str = "input";
/// Ordinal of a batch of events.
pub const REPORT: u32 = 1;

union! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InputEvent {
        /// A key went down or up (evdev key code).
        1 => Key { code: u16, pressed: bool },
        /// Relative pointer motion.
        2 => Motion { dx: i32, dy: i32 },
        /// Absolute pointer position, as a fraction `x / max` of the screen.
        3 => Absolute { x: u32, y: u32, max_x: u32, max_y: u32 },
        /// Mouse button (0 = left, 1 = right, 2 = middle).
        4 => Button { button: u8, pressed: bool },
        /// Scroll wheel (positive dy = away from the user / up).
        5 => Scroll { dx: i32, dy: i32 },
    }
}

/// Sends a batch of events.
pub fn report(ch: &vrt::object::Channel, events: Vec<InputEvent>) -> Result<(), vipc::IpcError> {
    vipc::send_event(ch, REPORT, events)
}

/// A driver's connection to the input service that survives restarts of
/// the window system: when the service goes away, the sink connects again
/// (the registry holds the new connection until a successor registers).
pub struct InputSink {
    channel: vrt::object::Channel,
}

impl InputSink {
    pub fn connect() -> Result<InputSink, crate::ServiceError> {
        Ok(InputSink { channel: crate::connect(NAME)? })
    }

    /// Sends a batch of events. Events that cannot be delivered (the service
    /// is restarting, or is too far behind to queue more) are dropped.
    pub fn report(&mut self, events: Vec<InputEvent>) {
        if let Err(vipc::IpcError::PeerClosed) = report(&self.channel, events)
            && let Ok(ch) = crate::connect(NAME)
        {
            self.channel = ch;
        }
    }
}

/// Linux evdev key codes used throughout Veda.
pub mod keys {
    pub const ESC: u16 = 1;
    pub const KEY_1: u16 = 2;
    pub const KEY_2: u16 = 3;
    pub const KEY_3: u16 = 4;
    pub const KEY_4: u16 = 5;
    pub const KEY_5: u16 = 6;
    pub const KEY_6: u16 = 7;
    pub const KEY_7: u16 = 8;
    pub const KEY_8: u16 = 9;
    pub const KEY_9: u16 = 10;
    pub const KEY_0: u16 = 11;
    pub const MINUS: u16 = 12;
    pub const EQUAL: u16 = 13;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const Q: u16 = 16;
    pub const W: u16 = 17;
    pub const E: u16 = 18;
    pub const R: u16 = 19;
    pub const T: u16 = 20;
    pub const Y: u16 = 21;
    pub const U: u16 = 22;
    pub const I: u16 = 23;
    pub const O: u16 = 24;
    pub const P: u16 = 25;
    pub const LEFTBRACE: u16 = 26;
    pub const RIGHTBRACE: u16 = 27;
    pub const ENTER: u16 = 28;
    pub const LEFTCTRL: u16 = 29;
    pub const A: u16 = 30;
    pub const S: u16 = 31;
    pub const D: u16 = 32;
    pub const F: u16 = 33;
    pub const G: u16 = 34;
    pub const H: u16 = 35;
    pub const J: u16 = 36;
    pub const K: u16 = 37;
    pub const L: u16 = 38;
    pub const SEMICOLON: u16 = 39;
    pub const APOSTROPHE: u16 = 40;
    pub const GRAVE: u16 = 41;
    pub const LEFTSHIFT: u16 = 42;
    pub const BACKSLASH: u16 = 43;
    pub const Z: u16 = 44;
    pub const X: u16 = 45;
    pub const C: u16 = 46;
    pub const V: u16 = 47;
    pub const B: u16 = 48;
    pub const N: u16 = 49;
    pub const M: u16 = 50;
    pub const COMMA: u16 = 51;
    pub const DOT: u16 = 52;
    pub const SLASH: u16 = 53;
    pub const RIGHTSHIFT: u16 = 54;
    pub const KPASTERISK: u16 = 55;
    pub const LEFTALT: u16 = 56;
    pub const SPACE: u16 = 57;
    pub const CAPSLOCK: u16 = 58;
    pub const F1: u16 = 59;
    pub const F2: u16 = 60;
    pub const F3: u16 = 61;
    pub const F4: u16 = 62;
    pub const F5: u16 = 63;
    pub const F6: u16 = 64;
    pub const F7: u16 = 65;
    pub const F8: u16 = 66;
    pub const F9: u16 = 67;
    pub const F10: u16 = 68;
    pub const NUMLOCK: u16 = 69;
    pub const SCROLLLOCK: u16 = 70;
    pub const KP7: u16 = 71;
    pub const KP8: u16 = 72;
    pub const KP9: u16 = 73;
    pub const KPMINUS: u16 = 74;
    pub const KP4: u16 = 75;
    pub const KP5: u16 = 76;
    pub const KP6: u16 = 77;
    pub const KPPLUS: u16 = 78;
    pub const KP1: u16 = 79;
    pub const KP2: u16 = 80;
    pub const KP3: u16 = 81;
    pub const KP0: u16 = 82;
    pub const KPDOT: u16 = 83;
    /// The extra key of ISO keyboards, left of Z.
    pub const KEY_102ND: u16 = 86;
    pub const F11: u16 = 87;
    pub const F12: u16 = 88;
    pub const KPENTER: u16 = 96;
    pub const RIGHTCTRL: u16 = 97;
    pub const KPSLASH: u16 = 98;
    /// Print Screen.
    pub const SYSRQ: u16 = 99;
    pub const RIGHTALT: u16 = 100;
    pub const HOME: u16 = 102;
    pub const UP: u16 = 103;
    pub const PAGEUP: u16 = 104;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const END: u16 = 107;
    pub const DOWN: u16 = 108;
    pub const PAGEDOWN: u16 = 109;
    pub const INSERT: u16 = 110;
    pub const DELETE: u16 = 111;
    pub const PAUSE: u16 = 119;
    pub const LEFTMETA: u16 = 125;
    pub const RIGHTMETA: u16 = 126;
    pub const COMPOSE: u16 = 127;
    /// Mouse buttons in evdev numbering.
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
}
