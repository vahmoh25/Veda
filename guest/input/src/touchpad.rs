//! Touchpads: where the fingers are on the pad, made a pointer's motion,
//! scrolling and clicks, as PCs' systems make them.
//!
//! * One finger moves the pointer by its own motion (the first report of a
//!   touch only places it), faster the faster it goes: millimetres on the
//!   pad, by its resolution, become pixels.
//! * Two fingers scroll; the content follows them.
//! * A short touch that hardly moves is a click: one finger's the left
//!   button, two fingers' the right one, three's the middle one.
//! * A physical click (a clickpad's) is the left button, or the right one
//!   with two fingers on the pad, the middle one with three.

use vproto::input::{InputEvent, keys};

const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOOL_QUINTTAP: u16 = 0x148;
const BTN_TOUCH: u16 = 0x14A;
const BTN_TOOL_DOUBLETAP: u16 = 0x14D;
const BTN_TOOL_TRIPLETAP: u16 = 0x14E;
const BTN_TOOL_QUADTAP: u16 = 0x14F;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// Pixels a millimetre of slow motion moves the pointer, and at most how
/// many times that a fast one does (at the speed, in millimetres a second,
/// where the gain grows by one).
const PX_PER_MM: f32 = 4.0;
const MAX_GAIN: f32 = 4.0;
const GAIN_SPEED: f32 = 60.0;
/// Millimetres of two fingers' motion a scroll step takes.
const MM_PER_STEP: f32 = 5.0;
/// A tap: a touch shorter than this (µs) that moves less (mm).
const TAP_US: u64 = 200_000;
const TAP_MM: f32 = 3.0;
/// A pad's size when it does not say its resolution (mm across).
const ASSUMED_WIDTH_MM: f32 = 100.0;

/// An axis of the pad: its range, and units a millimetre (0: unknown).
#[derive(Debug, Clone, Copy)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
    pub resolution: i32,
}

/// A touch from the first finger down to the last one up.
struct Touch {
    start_us: u64,
    /// The most fingers it had, how far it went (mm), and whether the pad
    /// was clicked during it.
    fingers: u8,
    moved_mm: f32,
    clicked: bool,
}

pub struct Touchpad {
    /// Units of the pad a millimetre, across and down.
    per_mm: (f32, f32),
    /// Fingers on the pad, as the tools say, and whether it is touched.
    fingers: u8,
    touching: bool,
    /// Where the (first) finger is, and was at the last report, with the
    /// fingers then and when.
    at: (i32, i32),
    last: Option<((i32, i32), u8, u64)>,
    /// What is not given out yet: fractions of a pixel, of a scroll step.
    rest: (f32, f32),
    scroll_rest: (f32, f32),
    touch: Option<Touch>,
    /// The button the physical click pressed, which its release releases.
    pressed: Option<u8>,
}

impl Touchpad {
    pub fn new(x: Axis, y: Axis) -> Touchpad {
        let per_mm = |a: Axis, fallback: f32| if a.resolution > 0 { a.resolution as f32 } else { fallback };
        let assumed = ((x.max - x.min).max(1) as f32 / ASSUMED_WIDTH_MM).max(1.0);
        Touchpad {
            per_mm: (per_mm(x, assumed), per_mm(y, assumed)),
            fingers: 0,
            touching: false,
            at: (0, 0),
            last: None,
            rest: (0.0, 0.0),
            scroll_rest: (0.0, 0.0),
            touch: None,
            pressed: None,
        }
    }

    /// Takes key or button `code`; false if it is none of the pad's.
    pub fn key(&mut self, code: u16, pressed: bool, time_us: u64, out: &mut Vec<InputEvent>) -> bool {
        let tool = match code {
            BTN_TOOL_FINGER => Some(1),
            BTN_TOOL_DOUBLETAP => Some(2),
            BTN_TOOL_TRIPLETAP => Some(3),
            BTN_TOOL_QUADTAP => Some(4),
            BTN_TOOL_QUINTTAP => Some(5),
            _ => None,
        };
        match code {
            _ if tool.is_some() => {
                let n = tool.unwrap_or(1);
                if pressed {
                    self.fingers = n;
                    if let Some(t) = &mut self.touch {
                        t.fingers = t.fingers.max(n);
                    }
                } else if self.fingers == n {
                    self.fingers = 0;
                }
            }
            BTN_TOUCH => {
                self.touching = pressed;
                if pressed {
                    let fingers = self.fingers.max(1);
                    self.touch = Some(Touch { start_us: time_us, fingers, moved_mm: 0.0, clicked: false });
                } else if let Some(t) = self.touch.take() {
                    self.last = None;
                    if !t.clicked && time_us.saturating_sub(t.start_us) < TAP_US && t.moved_mm < TAP_MM {
                        let button = match t.fingers {
                            2 => 1,
                            3 => 2,
                            _ => 0,
                        };
                        out.push(InputEvent::Button { button, pressed: true });
                        out.push(InputEvent::Button { button, pressed: false });
                    }
                }
            }
            keys::BTN_LEFT => {
                if pressed {
                    let button = match self.fingers {
                        2 => 1,
                        3 => 2,
                        _ => 0,
                    };
                    if let Some(t) = &mut self.touch {
                        t.clicked = true;
                    }
                    self.pressed = Some(button);
                    out.push(InputEvent::Button { button, pressed: true });
                } else if let Some(button) = self.pressed.take() {
                    out.push(InputEvent::Button { button, pressed: false });
                }
            }
            _ => return false,
        }
        true
    }

    /// Takes where the (first) finger is.
    pub fn abs(&mut self, code: u16, value: i32) {
        match code {
            ABS_X => self.at.0 = value,
            ABS_Y => self.at.1 = value,
            _ => {}
        }
    }

    /// The end of a report: the motion, or the scrolling, since the last.
    pub fn report(&mut self, time_us: u64, out: &mut Vec<InputEvent>) {
        if !self.touching {
            self.last = None;
            return;
        }
        let now = (self.at, self.fingers, time_us);
        // A touch, or a change of fingers, only places the motion's start.
        let Some(((x, y), fingers, then_us)) = self.last.replace(now).filter(|l| l.1 == self.fingers) else {
            return;
        };
        let mm = ((self.at.0 - x) as f32 / self.per_mm.0, (self.at.1 - y) as f32 / self.per_mm.1);
        let distance = (mm.0 * mm.0 + mm.1 * mm.1).sqrt();
        if let Some(t) = &mut self.touch {
            t.moved_mm += distance;
        }
        match fingers {
            1 => {
                let seconds = (time_us.saturating_sub(then_us) as f32 / 1e6).max(0.001);
                let gain = PX_PER_MM * (1.0 + distance / seconds / GAIN_SPEED).min(MAX_GAIN);
                self.rest.0 += mm.0 * gain;
                self.rest.1 += mm.1 * gain;
                let (dx, dy) = (self.rest.0.trunc(), self.rest.1.trunc());
                self.rest = (self.rest.0 - dx, self.rest.1 - dy);
                if dx != 0.0 || dy != 0.0 {
                    out.push(InputEvent::Motion { dx: dx as i32, dy: dy as i32 });
                }
            }
            2 => {
                // The content follows the fingers: up, and the view goes
                // down the page (a wheel turned towards the user).
                self.scroll_rest.0 -= mm.0 / MM_PER_STEP;
                self.scroll_rest.1 += mm.1 / MM_PER_STEP;
                let (dx, dy) = (self.scroll_rest.0.trunc(), self.scroll_rest.1.trunc());
                self.scroll_rest = (self.scroll_rest.0 - dx, self.scroll_rest.1 - dy);
                if dx != 0.0 || dy != 0.0 {
                    out.push(InputEvent::Scroll { dx: dx as i32, dy: dy as i32 });
                }
            }
            _ => {}
        }
    }

    /// Linux dropped events: the next report only places the motion's
    /// start.
    pub fn dropped(&mut self) {
        self.last = None;
    }

    /// Lets go of the button it holds (the pad went away).
    pub fn let_go(&mut self) -> Option<InputEvent> {
        self.pressed.take().map(|button| InputEvent::Button { button, pressed: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pad 100 mm by 60 mm, 10 units a millimetre.
    fn pad() -> Touchpad {
        Touchpad::new(Axis { min: 0, max: 1000, resolution: 10 }, Axis { min: 0, max: 600, resolution: 10 })
    }

    /// One report: the finger at (x, y), at `ms`.
    fn at(p: &mut Touchpad, x: i32, y: i32, ms: u64) -> Vec<InputEvent> {
        let mut out = Vec::new();
        p.abs(ABS_X, x);
        p.abs(ABS_Y, y);
        p.report(ms * 1000, &mut out);
        out
    }

    fn down(p: &mut Touchpad, tool: u16, ms: u64) -> Vec<InputEvent> {
        let mut out = Vec::new();
        p.key(tool, true, ms * 1000, &mut out);
        p.key(BTN_TOUCH, true, ms * 1000, &mut out);
        out
    }

    fn up(p: &mut Touchpad, tool: u16, ms: u64) -> Vec<InputEvent> {
        let mut out = Vec::new();
        p.key(tool, false, ms * 1000, &mut out);
        p.key(BTN_TOUCH, false, ms * 1000, &mut out);
        p.report(ms * 1000, &mut out);
        out
    }

    fn motion(events: &[InputEvent]) -> (i32, i32) {
        events.iter().fold((0, 0), |(x, y), e| match e {
            InputEvent::Motion { dx, dy } => (x + dx, y + dy),
            _ => (x, y),
        })
    }

    #[test]
    fn a_finger_moves_the_pointer_from_where_it_touched() {
        let mut p = pad();
        down(&mut p, BTN_TOOL_FINGER, 0);
        // The first report only places it.
        assert!(at(&mut p, 500, 300, 0).is_empty());
        // 1 mm right, slowly (100 mm/s gain would be more): 4 px or more.
        let slow = motion(&at(&mut p, 510, 300, 100));
        assert!(slow.0 >= 4 && slow.1 == 0, "{slow:?}");
        // Faster motion moves the pointer further a millimetre.
        let fast = motion(&at(&mut p, 610, 300, 110));
        assert!(fast.0 > 10 * slow.0, "{fast:?} against {slow:?}");
        // Up and left.
        let back = motion(&at(&mut p, 600, 290, 210));
        assert!(back.0 < 0 && back.1 < 0, "{back:?}");
        assert!(up(&mut p, BTN_TOOL_FINGER, 400).iter().all(|e| !matches!(e, InputEvent::Button { .. })));
    }

    #[test]
    fn slow_motion_is_not_lost() {
        let mut p = pad();
        down(&mut p, BTN_TOOL_FINGER, 0);
        at(&mut p, 500, 300, 0);
        let mut total = (0, 0);
        for i in 1..=40 {
            let m = motion(&at(&mut p, 500 + i, 300, i as u64 * 100));
            total = (total.0 + m.0, total.1 + m.1);
        }
        // 4 mm in tenths: at least 4 px a millimetre.
        assert!(total.0 >= 16, "{total:?}");
    }

    #[test]
    fn two_fingers_scroll_and_the_content_follows() {
        let mut p = pad();
        down(&mut p, BTN_TOOL_DOUBLETAP, 0);
        at(&mut p, 500, 300, 0);
        // 20 mm up: 4 steps, the view going down the page.
        let events = at(&mut p, 500, 100, 100);
        assert_eq!(events, vec![InputEvent::Scroll { dx: 0, dy: -4 }]);
        assert_eq!(motion(&events), (0, 0));
    }

    #[test]
    fn a_tap_clicks_and_two_fingers_tap_the_right_button() {
        let mut p = pad();
        down(&mut p, BTN_TOOL_FINGER, 0);
        at(&mut p, 500, 300, 0);
        at(&mut p, 505, 300, 50);
        let click = up(&mut p, BTN_TOOL_FINGER, 100);
        assert_eq!(
            click,
            vec![InputEvent::Button { button: 0, pressed: true }, InputEvent::Button { button: 0, pressed: false }]
        );
        down(&mut p, BTN_TOOL_FINGER, 1000);
        let mut out = Vec::new();
        p.key(BTN_TOOL_FINGER, false, 1_010_000, &mut out);
        p.key(BTN_TOOL_DOUBLETAP, true, 1_010_000, &mut out);
        at(&mut p, 500, 300, 1020);
        let right = up(&mut p, BTN_TOOL_DOUBLETAP, 1100);
        assert_eq!(
            right,
            vec![InputEvent::Button { button: 1, pressed: true }, InputEvent::Button { button: 1, pressed: false }]
        );
    }

    #[test]
    fn a_long_touch_or_one_that_moves_is_no_tap() {
        let mut p = pad();
        down(&mut p, BTN_TOOL_FINGER, 0);
        at(&mut p, 500, 300, 0);
        assert!(up(&mut p, BTN_TOOL_FINGER, 500).is_empty());
        down(&mut p, BTN_TOOL_FINGER, 1000);
        at(&mut p, 500, 300, 1000);
        at(&mut p, 600, 300, 1050);
        assert!(!up(&mut p, BTN_TOOL_FINGER, 1100).iter().any(|e| matches!(e, InputEvent::Button { .. })));
    }

    #[test]
    fn a_click_with_two_fingers_is_the_right_button() {
        let mut p = pad();
        down(&mut p, BTN_TOOL_DOUBLETAP, 0);
        let mut out = Vec::new();
        p.key(keys::BTN_LEFT, true, 10_000, &mut out);
        // The fingers change before the release; the release is still the
        // right button's, and the touch no tap.
        p.key(BTN_TOOL_DOUBLETAP, false, 20_000, &mut out);
        p.key(BTN_TOOL_FINGER, true, 20_000, &mut out);
        p.key(keys::BTN_LEFT, false, 30_000, &mut out);
        out.extend(up(&mut p, BTN_TOOL_FINGER, 40));
        assert_eq!(
            out,
            vec![InputEvent::Button { button: 1, pressed: true }, InputEvent::Button { button: 1, pressed: false }]
        );
    }
}
