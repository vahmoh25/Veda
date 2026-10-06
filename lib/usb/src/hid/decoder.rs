//! Input reports of keyboards, mice and tablets turned into input events,
//! and the output report that sets a keyboard's lights.

use alloc::vec;
use alloc::vec::Vec;

use vproto::input::InputEvent;

use super::keymap::{is_modifier, key_code};
use super::{Field, ReportDescriptor, ReportKind, id_of, page, usages, write_bits};

/// How a device points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    /// By movements (a mouse, a trackball, a touchpad in mouse mode).
    Relative,
    /// By positions (a tablet, a touch screen, a virtual machine's pointer).
    Absolute,
}

/// The lights of a keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Leds {
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}

/// Keyboard usages that stand for errors rather than keys (ErrorRollOver,
/// POSTFail, ErrorUndefined): the report says nothing about the keys.
const ERROR_USAGES: core::ops::RangeInclusive<u16> = 0x01..=0x03;

/// Turns the input reports of one HID interface into input events.
#[derive(Debug, Clone)]
pub struct Decoder {
    numbered: bool,
    /// Input fields of keyboard and keypad collections with keyboard usages.
    keys: Vec<Field>,
    /// Input fields of mouse and pointer collections: buttons, X, Y and the
    /// wheels.
    pointer: Vec<Field>,
    /// Output fields with keyboard lights, all in one report.
    leds: Vec<Field>,
    led_report_len: usize,
    max_input_len: usize,
    /// The length of each input report that is decoded, by report ID.
    input_lens: Vec<(u8, usize)>,
    /// Keys held, per report ID, as sorted key codes.
    keys_down: Vec<(u8, Vec<u16>)>,
    /// Buttons held: bit 0 left, 1 right, 2 middle.
    buttons: u8,
    /// The last absolute position, and the range of each axis.
    position: [u32; 2],
    range: [u32; 2],
}

impl Decoder {
    /// A decoder for the reports `d` describes.
    pub fn new(d: &ReportDescriptor) -> Decoder {
        let inputs = || d.fields.iter().filter(|f| f.kind == ReportKind::Input);
        let keys: Vec<Field> = inputs()
            .filter(|f| matches!(f.application, usages::KEYBOARD | usages::KEYPAD) && f.has_page(page::KEYBOARD))
            .cloned()
            .collect();
        let pointer: Vec<Field> = inputs()
            .filter(|f| matches!(f.application, usages::MOUSE | usages::POINTER))
            .filter(|f| {
                f.has_page(page::BUTTON)
                    || [usages::X, usages::Y, usages::WHEEL, usages::AC_PAN].iter().any(|&u| f.has_usage(u))
            })
            .cloned()
            .collect();
        let led_field = |f: &&Field| f.kind == ReportKind::Output && f.is_variable() && f.has_page(page::LED);
        let led_id = d.fields.iter().find(led_field).map(|f| f.report_id);
        let leds: Vec<Field> =
            d.fields.iter().filter(led_field).filter(|f| Some(f.report_id) == led_id).cloned().collect();
        let mut range = [1, 1];
        for (axis, u) in [usages::X, usages::Y].into_iter().enumerate() {
            if let Some(f) = pointer.iter().find(|f| !f.is_relative() && f.has_usage(u)) {
                range[axis] = (f.logical_max - f.logical_min).clamp(1, u32::MAX as i64) as u32;
            }
        }
        let mut input_lens: Vec<(u8, usize)> = Vec::new();
        for f in keys.iter().chain(&pointer) {
            if !input_lens.iter().any(|&(id, _)| id == f.report_id) {
                input_lens.push((f.report_id, d.report_len(ReportKind::Input, f.report_id)));
            }
        }
        Decoder {
            numbered: d.numbered,
            led_report_len: led_id.map_or(0, |id| d.report_len(ReportKind::Output, id)),
            max_input_len: d.max_input_len(),
            input_lens,
            keys,
            pointer,
            leds,
            keys_down: Vec::new(),
            buttons: 0,
            position: [0, 0],
            range,
        }
    }

    /// Whether the device reports keys.
    pub fn is_keyboard(&self) -> bool {
        !self.keys.is_empty()
    }

    /// How the device points, if it reports X and Y.
    pub fn pointer(&self) -> Option<Pointer> {
        let xy = self.pointer.iter().find(|f| f.has_usage(usages::X) || f.has_usage(usages::Y))?;
        Some(if xy.is_relative() { Pointer::Relative } else { Pointer::Absolute })
    }

    /// Whether the interface reports keys or pointer positions.
    pub fn is_useful(&self) -> bool {
        self.is_keyboard() || self.pointer().is_some()
    }

    /// What the device is, for logs.
    pub fn describe(&self) -> &'static str {
        match (self.is_keyboard(), self.pointer()) {
            (true, None) => "keyboard",
            (true, Some(Pointer::Relative)) => "keyboard and mouse",
            (true, Some(Pointer::Absolute)) => "keyboard and tablet",
            (false, Some(Pointer::Relative)) => "mouse",
            (false, Some(Pointer::Absolute)) => "tablet",
            (false, None) => "HID device",
        }
    }

    /// The length of the longest input report, with its ID byte.
    pub fn max_report_len(&self) -> usize {
        self.max_input_len
    }

    /// Whether the keyboard has lights to set.
    pub fn has_leds(&self) -> bool {
        !self.leds.is_empty()
    }

    /// Decodes one input report (with its ID byte, if the device numbers
    /// its reports) into `out`.
    pub fn decode(&mut self, report: &[u8], out: &mut Vec<InputEvent>) {
        let (id, data) = match (self.numbered, report.split_first()) {
            (false, _) => (0, report),
            (true, Some((&id, data))) => (id, data),
            (true, None) => return,
        };
        if data.is_empty() {
            return;
        }
        // Some devices send less than their descriptor says: the rest reads
        // as zeros, as other systems take it.
        let len = self.input_lens.iter().find(|&&(i, _)| i == id).map_or(0, |&(_, len)| len);
        let padded: Vec<u8>;
        let data = if data.len() < len {
            padded = data.iter().copied().chain(core::iter::repeat_n(0, len - data.len())).collect();
            &padded[..]
        } else {
            data
        };
        self.decode_pointer(id, data, out);
        self.decode_keys(id, data, out);
    }

    fn decode_keys(&mut self, id: u8, data: &[u8], out: &mut Vec<InputEvent>) {
        let mut fields = self.keys.iter().filter(|f| f.report_id == id).peekable();
        if fields.peek().is_none() {
            return;
        }
        let mut down: Vec<u16> = Vec::new();
        for f in fields {
            if !f.fits(data) {
                return;
            }
            for i in 0..f.count {
                let Some(v) = f.value(data, i) else { continue };
                let usage = if f.is_variable() {
                    if v == 0 {
                        continue;
                    }
                    f.usage_at(i)
                } else {
                    f.array_usage(v)
                };
                let Some(usage) = usage else { continue };
                if usage >> 16 == page::KEYBOARD as u32 && ERROR_USAGES.contains(&id_of(usage)) {
                    // Too many keys at once: keep the state as it was.
                    return;
                }
                if let Some(code) = key_code(usage) {
                    down.push(code);
                }
            }
        }
        down.sort_unstable();
        down.dedup();
        let held = match self.keys_down.iter().position(|(i, _)| *i == id) {
            Some(at) => &mut self.keys_down[at].1,
            None => {
                self.keys_down.push((id, Vec::new()));
                &mut self.keys_down.last_mut().expect("just pushed").1
            }
        };
        // Releases first; then presses, modifiers before the keys they
        // modify.
        for &code in held.iter().filter(|c| !down.contains(c)) {
            out.push(InputEvent::Key { code, pressed: false });
        }
        for modifiers in [true, false] {
            for &code in down.iter().filter(|&&c| is_modifier(c) == modifiers && !held.contains(&c)) {
                out.push(InputEvent::Key { code, pressed: true });
            }
        }
        *held = down;
    }

    fn decode_pointer(&mut self, id: u8, data: &[u8], out: &mut Vec<InputEvent>) {
        let mut buttons: Option<u8> = None;
        let (mut dx, mut dy, mut wheel, mut pan) = (0i64, 0i64, 0i64, 0i64);
        let mut moved = false;
        for f in self.pointer.iter().filter(|f| f.report_id == id && f.fits(data)) {
            for i in 0..f.count {
                let Some(v) = f.value(data, i) else { continue };
                let (usage, pressed) = if f.is_variable() { (f.usage_at(i), v != 0) } else { (f.array_usage(v), true) };
                let Some(usage) = usage else { continue };
                if usage >> 16 == page::BUTTON as u32 {
                    // Buttons 1 to 3: left, right, middle.
                    let held = buttons.get_or_insert(0);
                    if pressed && (1..=3).contains(&id_of(usage)) {
                        *held |= 1 << (id_of(usage) - 1);
                    }
                    continue;
                }
                match usage {
                    usages::X if f.is_relative() => dx += v,
                    usages::Y if f.is_relative() => dy += v,
                    usages::X | usages::Y => {
                        let axis = (usage == usages::Y) as usize;
                        self.position[axis] = (v.clamp(f.logical_min, f.logical_max) - f.logical_min) as u32;
                        moved = true;
                    }
                    usages::WHEEL => wheel += v,
                    usages::AC_PAN => pan += v,
                    _ => {}
                }
            }
        }
        let clamp = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        if moved {
            let ([x, y], [max_x, max_y]) = (self.position, self.range);
            out.push(InputEvent::Absolute { x, y, max_x, max_y });
        }
        if dx != 0 || dy != 0 {
            out.push(InputEvent::Motion { dx: clamp(dx), dy: clamp(dy) });
        }
        if let Some(held) = buttons {
            for button in 0..3 {
                if (held ^ self.buttons) & (1 << button) != 0 {
                    out.push(InputEvent::Button { button, pressed: held & (1 << button) != 0 });
                }
            }
            self.buttons = held;
        }
        if wheel != 0 || pan != 0 {
            out.push(InputEvent::Scroll { dx: clamp(pan), dy: clamp(wheel) });
        }
    }

    /// Releases every key and button still held (the device went away).
    pub fn release_all(&mut self, out: &mut Vec<InputEvent>) {
        for (_, held) in self.keys_down.drain(..) {
            out.extend(held.into_iter().map(|code| InputEvent::Key { code, pressed: false }));
        }
        for button in 0..3 {
            if self.buttons & (1 << button) != 0 {
                out.push(InputEvent::Button { button, pressed: false });
            }
        }
        self.buttons = 0;
    }

    /// The output report that sets the keyboard's lights, as (report ID,
    /// bytes to send with SET_REPORT); `None` without lights.
    pub fn led_report(&self, leds: Leds) -> Option<(u8, Vec<u8>)> {
        let id = self.leds.first()?.report_id;
        let mut data = vec![0u8; self.led_report_len];
        for f in &self.leds {
            for i in 0..f.count {
                let on = match f.usage_at(i) {
                    Some(usages::NUM_LOCK) => leds.num_lock,
                    Some(usages::CAPS_LOCK) => leds.caps_lock,
                    Some(usages::SCROLL_LOCK) => leds.scroll_lock,
                    _ => false,
                };
                write_bits(&mut data, f.offset as u64 + i as u64 * f.size as u64, f.size, on as u64);
            }
        }
        if self.numbered {
            data.insert(0, id);
        }
        Some((id, data))
    }
}
