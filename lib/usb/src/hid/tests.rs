use alloc::vec;
use alloc::vec::Vec;

use vproto::input::{InputEvent, keys};

use super::*;

/// The boot keyboard, which QEMU's usb-kbd also describes itself as.
const BOOT_KEYBOARD: &[u8] = boot::KEYBOARD;

/// QEMU's usb-tablet: 3 buttons, absolute X and Y from 0 to 32767, and a
/// wheel.
const TABLET: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25,
    0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x15, 0x00, 0x26, 0xFF, 0x7F, 0x35, 0x00, 0x46, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09,
    0x38, 0x15, 0x81, 0x25, 0x7F, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0,
];

/// A boot-compatible mouse: 5 buttons, relative X, Y and wheel in bytes.
const MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25,
    0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xC0, 0xC0,
];

/// A wireless receiver's mouse: report ID 2, 16 buttons, 12-bit X and Y,
/// a wheel and horizontal scrolling (AC Pan).
const NUMBERED_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15,
    0x00, 0x25, 0x01, 0x95, 0x10, 0x75, 0x01, 0x81, 0x02, 0x05, 0x01, 0x16, 0x01, 0xF8, 0x26, 0xFF, 0x07, 0x75, 0x0C,
    0x95, 0x02, 0x09, 0x30, 0x09, 0x31, 0x81, 0x06, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x09, 0x38, 0x81,
    0x06, 0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0,
];

/// An n-key-rollover keyboard: report ID 1 with modifiers and a bitmap of
/// keys 0x00-0x9F; report ID 3, consumer controls, which are not keys.
const NKRO_KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75,
    0x01, 0x95, 0x08, 0x81, 0x02, 0x19, 0x00, 0x29, 0x9F, 0x95, 0xA0, 0x81, 0x02, 0xC0, 0x05, 0x0C, 0x09, 0x01, 0xA1,
    0x01, 0x85, 0x03, 0x15, 0x00, 0x26, 0xFF, 0x03, 0x19, 0x00, 0x2A, 0xFF, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00,
    0xC0,
];

fn key(code: u16, pressed: bool) -> InputEvent {
    InputEvent::Key { code, pressed }
}

fn decode(d: &mut Decoder, report: &[u8]) -> Vec<InputEvent> {
    let mut out = Vec::new();
    d.decode(report, &mut out);
    out
}

#[test]
fn boot_keyboard_fields() {
    let r = ReportDescriptor::parse(BOOT_KEYBOARD).unwrap();
    assert!(!r.numbered);
    // Modifiers, lights, keys; the padding is left out.
    assert_eq!(r.fields.len(), 3);
    let mods = &r.fields[0];
    assert_eq!((mods.kind, mods.offset, mods.size, mods.count), (ReportKind::Input, 0, 1, 8));
    assert_eq!(mods.usages, vec![(usage(page::KEYBOARD, 0xE0), usage(page::KEYBOARD, 0xE7))]);
    assert_eq!(mods.application, usages::KEYBOARD);
    let lights = &r.fields[1];
    assert_eq!((lights.kind, lights.offset, lights.count), (ReportKind::Output, 0, 5));
    let array = &r.fields[2];
    assert_eq!((array.offset, array.size, array.count, array.is_variable()), (16, 8, 6, false));
    // A maximum of 0xFF in one byte means 255 when the minimum is 0.
    assert_eq!((array.logical_min, array.logical_max), (0, 255));
    assert_eq!(r.report_len(ReportKind::Input, 0), 8);
    assert_eq!(r.report_len(ReportKind::Output, 0), 1);
    assert_eq!(r.max_input_len(), 8);
}

#[test]
fn boot_keyboard_reports() {
    let mut d = Decoder::new(&ReportDescriptor::parse(BOOT_KEYBOARD).unwrap());
    assert!(d.is_keyboard() && d.pointer().is_none() && d.has_leds());
    assert_eq!(d.describe(), "keyboard");
    // Shift + A: the modifier goes first.
    assert_eq!(decode(&mut d, &[0x02, 0, 0x04, 0, 0, 0, 0, 0]), [key(keys::LEFTSHIFT, true), key(keys::A, true)]);
    // The same state again says nothing new.
    assert!(decode(&mut d, &[0x02, 0, 0x04, 0, 0, 0, 0, 0]).is_empty());
    // A replaced by B with Shift released: releases first.
    assert_eq!(
        decode(&mut d, &[0x00, 0, 0x05, 0, 0, 0, 0, 0]),
        [key(keys::A, false), key(keys::LEFTSHIFT, false), key(keys::B, true)]
    );
    // Too many keys: the report is ignored, B stays held.
    assert!(decode(&mut d, &[0x00, 0, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]).is_empty());
    // A report shorter than described reads as if padded with zeros: Shift
    // only. An empty report says nothing.
    assert_eq!(decode(&mut d, &[0x02]), [key(keys::B, false), key(keys::LEFTSHIFT, true)]);
    assert!(decode(&mut d, &[]).is_empty());
    assert_eq!(decode(&mut d, &[0; 8]), [key(keys::LEFTSHIFT, false)]);
    // Right Alt, Enter on the keypad, and an unknown key.
    assert_eq!(
        decode(&mut d, &[0x40, 0, 0x58, 0x87, 0, 0, 0, 0]),
        [key(keys::RIGHTALT, true), key(keys::KPENTER, true)]
    );
    // Unplugged with keys held: everything is released.
    let mut out = Vec::new();
    d.release_all(&mut out);
    assert_eq!(out, [key(keys::KPENTER, false), key(keys::RIGHTALT, false)]);
}

#[test]
fn keyboard_lights() {
    let d = Decoder::new(&ReportDescriptor::parse(BOOT_KEYBOARD).unwrap());
    let caps = Leds { caps_lock: true, ..Leds::default() };
    assert_eq!(d.led_report(caps), Some((0, vec![0x02])));
    let all = Leds { num_lock: true, caps_lock: true, scroll_lock: true };
    assert_eq!(d.led_report(all), Some((0, vec![0x07])));
    let mouse = Decoder::new(&ReportDescriptor::parse(MOUSE).unwrap());
    assert_eq!(mouse.led_report(all), None);
}

#[test]
fn tablet() {
    let mut d = Decoder::new(&ReportDescriptor::parse(TABLET).unwrap());
    assert_eq!((d.is_keyboard(), d.pointer()), (false, Some(Pointer::Absolute)));
    assert_eq!(d.describe(), "tablet");
    assert_eq!(d.max_report_len(), 6);
    assert_eq!(
        decode(&mut d, &[0x01, 0x00, 0x40, 0x00, 0x20, 0x00]),
        [
            InputEvent::Absolute { x: 0x4000, y: 0x2000, max_x: 0x7FFF, max_y: 0x7FFF },
            InputEvent::Button { button: 0, pressed: true },
        ]
    );
    assert_eq!(
        decode(&mut d, &[0x00, 0xFF, 0x7F, 0x00, 0x00, 0xFF]),
        [
            InputEvent::Absolute { x: 0x7FFF, y: 0, max_x: 0x7FFF, max_y: 0x7FFF },
            InputEvent::Button { button: 0, pressed: false },
            InputEvent::Scroll { dx: 0, dy: -1 },
        ]
    );
}

#[test]
fn relative_mouse() {
    let mut d = Decoder::new(&ReportDescriptor::parse(MOUSE).unwrap());
    assert_eq!((d.pointer(), d.describe()), (Some(Pointer::Relative), "mouse"));
    // Right and middle buttons, 5 left, 3 down, wheel 1 up; buttons 4 and
    // 5 (bits 3 and 4) are not passed on.
    assert_eq!(
        decode(&mut d, &[0x1E, 0xFB, 0x03, 0x01]),
        [
            InputEvent::Motion { dx: -5, dy: 3 },
            InputEvent::Button { button: 1, pressed: true },
            InputEvent::Button { button: 2, pressed: true },
            InputEvent::Scroll { dx: 0, dy: 1 },
        ]
    );
    assert_eq!(
        decode(&mut d, &[0x00, 0x00, 0x00, 0x00]),
        [InputEvent::Button { button: 1, pressed: false }, InputEvent::Button { button: 2, pressed: false }]
    );
}

#[test]
fn boot_mouse() {
    let r = ReportDescriptor::parse(boot::MOUSE).unwrap();
    assert_eq!(r.max_input_len(), 3);
    let mut d = Decoder::new(&r);
    assert_eq!(d.describe(), "mouse");
    // Extra bytes after the boot report are ignored.
    assert_eq!(
        decode(&mut d, &[0x01, 0x02, 0xFE, 0x01]),
        [InputEvent::Motion { dx: 2, dy: -2 }, InputEvent::Button { button: 0, pressed: true }]
    );
}

#[test]
fn numbered_mouse() {
    let r = ReportDescriptor::parse(NUMBERED_MOUSE).unwrap();
    assert!(r.numbered);
    assert_eq!(r.max_input_len(), 8);
    let mut d = Decoder::new(&r);
    assert_eq!(d.pointer(), Some(Pointer::Relative));
    // Left button; X -5 and Y +3 packed in 12 bits each; wheel 1 up;
    // scrolled 1 to the left.
    assert_eq!(
        decode(&mut d, &[0x02, 0x01, 0x00, 0xFB, 0x3F, 0x00, 0x01, 0xFF]),
        [
            InputEvent::Motion { dx: -5, dy: 3 },
            InputEvent::Button { button: 0, pressed: true },
            InputEvent::Scroll { dx: -1, dy: 1 },
        ]
    );
    // Other report IDs, and an empty report, are not for the mouse.
    assert!(decode(&mut d, &[0x05, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00]).is_empty());
    assert!(decode(&mut d, &[]).is_empty());
}

#[test]
fn nkro_keyboard() {
    let r = ReportDescriptor::parse(NKRO_KEYBOARD).unwrap();
    let mut d = Decoder::new(&r);
    assert!(d.is_keyboard());
    assert_eq!(d.pointer(), None);
    assert_eq!(r.report_len(ReportKind::Input, 1), 21);
    // Left Ctrl with A and B (bits 4 and 5 of the bitmap).
    let mut report = vec![0u8; 22];
    report[0] = 1;
    report[1] = 0x01;
    report[2] = 0x30;
    assert_eq!(decode(&mut d, &report), [key(keys::LEFTCTRL, true), key(keys::A, true), key(keys::B, true)]);
    // A consumer report (volume up) leaves the keys alone.
    assert!(decode(&mut d, &[0x03, 0xE9, 0x00]).is_empty());
    report[2] = 0x20;
    assert_eq!(decode(&mut d, &report), [key(keys::A, false)]);
}

#[test]
fn joysticks_do_not_point() {
    // A joystick with buttons and absolute X and Y.
    let joystick = [
        0x05, 0x01, 0x09, 0x04, 0xA1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
        0x95, 0x08, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95,
        0x02, 0x81, 0x02, 0xC0,
    ];
    let mut d = Decoder::new(&ReportDescriptor::parse(&joystick).unwrap());
    assert!(!d.is_useful());
    assert_eq!(d.describe(), "HID device");
    assert!(decode(&mut d, &[0x01, 0x80, 0x80]).is_empty());
}

#[test]
fn usage_page_applies_at_the_main_item() {
    // Usage (X), Usage (Y) declared before Usage Page (Generic Desktop);
    // extended usages carry their own page.
    let desc = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x09, 0x09, 0x30, 0x09, 0x31, 0x05, 0x01, 0x15, 0x81, 0x25, 0x7F,
        0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0x0B, 0x38, 0x02, 0x0C, 0x00, 0x95, 0x01, 0x81, 0x06, 0xC0,
    ];
    let r = ReportDescriptor::parse(&desc).unwrap();
    assert_eq!(r.fields[0].usages, vec![(usages::X, usages::X), (usages::Y, usages::Y)]);
    assert_eq!(r.fields[1].usages, vec![(usages::AC_PAN, usages::AC_PAN)]);
}

#[test]
fn push_pop_and_errors() {
    // Push, change the report size, pop: the size is back to 8.
    let desc = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x75, 0x08, 0x95, 0x01, 0xA4, 0x75, 0x10, 0xB4, 0x09, 0x30, 0x81, 0x06,
        0xC0,
    ];
    let r = ReportDescriptor::parse(&desc).unwrap();
    assert_eq!(r.fields[0].size, 8);
    assert_eq!(ReportDescriptor::parse(&[0x05]), Err(ParseError::Truncated));
    assert_eq!(ReportDescriptor::parse(&[0x26, 0xFF]), Err(ParseError::Truncated));
    assert_eq!(ReportDescriptor::parse(&[0xC0]), Err(ParseError::Nesting));
    assert_eq!(ReportDescriptor::parse(&[0xB4]), Err(ParseError::Nesting));
    // 0xFFFF values of 0xFFFF bits.
    assert_eq!(
        ReportDescriptor::parse(&[0x77, 0xFF, 0xFF, 0, 0, 0x97, 0xFF, 0xFF, 0, 0, 0x81, 0x02]),
        Err(ParseError::TooLong)
    );
    // A long item is skipped.
    assert_eq!(ReportDescriptor::parse(&[0xFE, 0x02, 0x10, 0xAA, 0xBB]).unwrap().fields.len(), 0);
    assert_eq!(ReportDescriptor::parse(&[0xFE, 0x05, 0x10, 0xAA]), Err(ParseError::Truncated));
}

#[test]
fn values_and_usages() {
    let f = Field {
        kind: ReportKind::Input,
        report_id: 0,
        offset: 4,
        size: 12,
        count: 2,
        flags: flags::VARIABLE,
        logical_min: -2048,
        logical_max: 2047,
        usages: vec![(usages::X, usages::X)],
        application: usages::MOUSE,
    };
    // Bits 4-15: 0x800 (-2048); bits 16-27: 0x7FF.
    let data = [0x00, 0x80, 0xFF, 0x07];
    assert_eq!(f.value(&data, 0), Some(-2048));
    assert_eq!(f.value(&data, 1), Some(2047));
    assert_eq!(f.value(&data, 2), None);
    assert_eq!(f.value(&data[..3], 1), None);
    assert!(f.fits(&data) && !f.fits(&data[..3]));
    // The last usage repeats for the remaining values.
    assert_eq!(f.usage_at(1), Some(usages::X));
    let a = Field { flags: 0, logical_min: 1, logical_max: 3, usages: vec![(0x0009_0001, 0x0009_0003)], ..f };
    assert_eq!(a.array_usage(2), Some(0x0009_0002));
    assert_eq!((a.array_usage(0), a.array_usage(4)), (None, None));
}

#[test]
fn key_codes() {
    assert_eq!(key_code(usage(page::KEYBOARD, 0x04)), Some(keys::A));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x1D)), Some(keys::Z));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x1E)), Some(keys::KEY_1));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x27)), Some(keys::KEY_0));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x45)), Some(keys::F12));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x62)), Some(keys::KP0));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x52)), Some(keys::UP));
    assert_eq!(key_code(usage(page::KEYBOARD, 0xE3)), Some(keys::LEFTMETA));
    assert_eq!(key_code(usage(page::KEYBOARD, 0x00)), None);
    assert_eq!(key_code(usage(page::CONSUMER, 0x04)), None);
}
