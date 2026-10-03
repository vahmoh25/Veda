//! Keyboard state: modifiers, the US keyboard layout, and auto-repeat.

use alloc::string::String;

use vproto::display::modifiers;
use vproto::input::keys;

/// Delay before a held key starts repeating, and the repeat interval.
pub const REPEAT_DELAY_NS: u64 = 450_000_000;
pub const REPEAT_INTERVAL_NS: u64 = 33_000_000;

#[derive(Default)]
pub struct Keyboard {
    shift: u8,
    ctrl: u8,
    alt: u8,
    meta: u8,
    caps: bool,
    /// Key being auto-repeated and when it next fires.
    repeat: Option<(u16, u64)>,
    /// Super pressed with no other key in between (opens the start menu).
    meta_alone: bool,
}

/// Maps an evdev code to the (unshifted, shifted) characters of a US layout.
fn us_layout(code: u16) -> Option<(char, char)> {
    const ROW1: &[(char, char)] = &[
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
        ('0', ')'),
        ('-', '_'),
        ('=', '+'),
    ];
    Some(match code {
        2..=13 => ROW1[(code - 2) as usize],
        16..=25 => {
            let c = b"qwertyuiop"[(code - 16) as usize] as char;
            (c, c.to_ascii_uppercase())
        }
        26 => ('[', '{'),
        27 => (']', '}'),
        30..=38 => {
            let c = b"asdfghjkl"[(code - 30) as usize] as char;
            (c, c.to_ascii_uppercase())
        }
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        44..=50 => {
            let c = b"zxcvbnm"[(code - 44) as usize] as char;
            (c, c.to_ascii_uppercase())
        }
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        57 => (' ', ' '),
        55 => ('*', '*'),
        74 => ('-', '-'),
        78 => ('+', '+'),
        98 => ('/', '/'),
        71 => ('7', '7'),
        72 => ('8', '8'),
        73 => ('9', '9'),
        75 => ('4', '4'),
        76 => ('5', '5'),
        77 => ('6', '6'),
        79 => ('1', '1'),
        80 => ('2', '2'),
        81 => ('3', '3'),
        82 => ('0', '0'),
        83 => ('.', '.'),
        _ => return None,
    })
}

/// Keys that auto-repeat while held.
fn repeats(code: u16) -> bool {
    us_layout(code).is_some()
        || matches!(
            code,
            keys::BACKSPACE
                | keys::DELETE
                | keys::ENTER
                | keys::KPENTER
                | keys::TAB
                | keys::LEFT
                | keys::RIGHT
                | keys::UP
                | keys::DOWN
                | keys::PAGEUP
                | keys::PAGEDOWN
        )
}

/// What a key event means after keyboard processing.
pub struct KeyOutput {
    pub code: u16,
    pub pressed: bool,
    pub repeat: bool,
    pub modifiers: u32,
    pub text: String,
}

impl Keyboard {
    pub fn modifiers(&self) -> u32 {
        let mut m = 0;
        if self.shift > 0 {
            m |= modifiers::SHIFT;
        }
        if self.ctrl > 0 {
            m |= modifiers::CTRL;
        }
        if self.alt > 0 {
            m |= modifiers::ALT;
        }
        if self.meta > 0 {
            m |= modifiers::SUPER;
        }
        if self.caps {
            m |= modifiers::CAPS_LOCK;
        }
        m
    }

    fn text_for(&self, code: u16) -> String {
        let mut s = String::new();
        if self.ctrl > 0 || self.alt > 0 || self.meta > 0 {
            return s;
        }
        if let Some((lower, upper)) = us_layout(code) {
            let letter = lower.is_ascii_alphabetic();
            let shifted = (self.shift > 0) ^ (letter && self.caps);
            s.push(if shifted { upper } else { lower });
        } else if code == keys::ENTER || code == keys::KPENTER {
            s.push('\n');
        } else if code == keys::TAB {
            s.push('\t');
        }
        s
    }

    /// Updates state for a key event. Returns `Some(true)` from
    /// `meta_tapped` semantics via [`Keyboard::take_meta_tap`].
    pub fn process(&mut self, code: u16, pressed: bool, now: u64) -> KeyOutput {
        let delta = |v: &mut u8| {
            *v = if pressed { v.saturating_add(1).min(2) } else { v.saturating_sub(1) };
        };
        match code {
            keys::LEFTSHIFT | keys::RIGHTSHIFT => delta(&mut self.shift),
            keys::LEFTCTRL | keys::RIGHTCTRL => delta(&mut self.ctrl),
            keys::LEFTALT | keys::RIGHTALT => delta(&mut self.alt),
            keys::LEFTMETA | keys::RIGHTMETA => {
                delta(&mut self.meta);
                if pressed {
                    self.meta_alone = true;
                }
            }
            keys::CAPSLOCK if pressed => self.caps = !self.caps,
            _ => {
                if pressed {
                    self.meta_alone = false;
                }
            }
        }
        if pressed && repeats(code) {
            self.repeat = Some((code, now + REPEAT_DELAY_NS));
        } else if !pressed && self.repeat.is_some_and(|(c, _)| c == code) {
            self.repeat = None;
        }
        KeyOutput { code, pressed, repeat: false, modifiers: self.modifiers(), text: if pressed { self.text_for(code) } else { String::new() } }
    }

    /// True once if Super was pressed and released on its own.
    pub fn take_meta_tap(&mut self, code: u16, pressed: bool) -> bool {
        if !pressed && matches!(code, keys::LEFTMETA | keys::RIGHTMETA) && self.meta_alone {
            self.meta_alone = false;
            return true;
        }
        false
    }

    /// The deadline of the next auto-repeat, if a key is held.
    pub fn repeat_deadline(&self) -> Option<u64> {
        self.repeat.map(|(_, t)| t)
    }

    /// Produces a repeat event if it is due.
    pub fn poll_repeat(&mut self, now: u64) -> Option<KeyOutput> {
        let (code, due) = self.repeat?;
        if now < due {
            return None;
        }
        self.repeat = Some((code, now + REPEAT_INTERVAL_NS));
        Some(KeyOutput { code, pressed: true, repeat: true, modifiers: self.modifiers(), text: self.text_for(code) })
    }

    /// Forgets held keys (e.g. when focus changes).
    pub fn reset_repeat(&mut self) {
        self.repeat = None;
    }
}
