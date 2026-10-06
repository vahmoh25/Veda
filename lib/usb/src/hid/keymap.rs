//! Keyboard usages (HID Usage Tables, chapter 10) to Veda's key codes
//! (Linux evdev codes, as the PS/2 and virtio drivers report them).

use vproto::input::keys::*;

use super::{Usage, id_of, page, page_of};

const LETTERS: [u16; 26] = [A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z];
const DIGITS: [u16; 10] = [KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_0];
const FUNCTION_KEYS: [u16; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
const KEYPAD_DIGITS: [u16; 10] = [KP1, KP2, KP3, KP4, KP5, KP6, KP7, KP8, KP9, KP0];

/// The key code of a usage on the keyboard page (`None` for other pages
/// and for keys Veda has no code for).
pub fn key_code(u: Usage) -> Option<u16> {
    if page_of(u) != page::KEYBOARD {
        return None;
    }
    let id = id_of(u);
    Some(match id {
        0x04..=0x1D => LETTERS[(id - 0x04) as usize],
        0x1E..=0x27 => DIGITS[(id - 0x1E) as usize],
        0x28 => ENTER,
        0x29 => ESC,
        0x2A => BACKSPACE,
        0x2B => TAB,
        0x2C => SPACE,
        0x2D => MINUS,
        0x2E => EQUAL,
        0x2F => LEFTBRACE,
        0x30 => RIGHTBRACE,
        0x31 => BACKSLASH,
        // "Non-US # and ~", next to Enter on ISO keyboards: the same
        // position as the backslash key of US keyboards.
        0x32 => BACKSLASH,
        0x33 => SEMICOLON,
        0x34 => APOSTROPHE,
        0x35 => GRAVE,
        0x36 => COMMA,
        0x37 => DOT,
        0x38 => SLASH,
        0x39 => CAPSLOCK,
        0x3A..=0x45 => FUNCTION_KEYS[(id - 0x3A) as usize],
        0x46 => SYSRQ,
        0x47 => SCROLLLOCK,
        0x48 => PAUSE,
        0x49 => INSERT,
        0x4A => HOME,
        0x4B => PAGEUP,
        0x4C => DELETE,
        0x4D => END,
        0x4E => PAGEDOWN,
        0x4F => RIGHT,
        0x50 => LEFT,
        0x51 => DOWN,
        0x52 => UP,
        0x53 => NUMLOCK,
        0x54 => KPSLASH,
        0x55 => KPASTERISK,
        0x56 => KPMINUS,
        0x57 => KPPLUS,
        0x58 => KPENTER,
        0x59..=0x62 => KEYPAD_DIGITS[(id - 0x59) as usize],
        0x63 => KPDOT,
        // "Non-US \ and |", left of Z on ISO keyboards.
        0x64 => KEY_102ND,
        // The Application (menu) key.
        0x65 => COMPOSE,
        0xE0 => LEFTCTRL,
        0xE1 => LEFTSHIFT,
        0xE2 => LEFTALT,
        0xE3 => LEFTMETA,
        0xE4 => RIGHTCTRL,
        0xE5 => RIGHTSHIFT,
        0xE6 => RIGHTALT,
        0xE7 => RIGHTMETA,
        _ => return None,
    })
}

/// Whether a key code is a modifier (Ctrl, Shift, Alt, Super).
pub(super) fn is_modifier(code: u16) -> bool {
    matches!(code, LEFTCTRL | LEFTSHIFT | LEFTALT | LEFTMETA | RIGHTCTRL | RIGHTSHIFT | RIGHTALT | RIGHTMETA)
}
