//! `input` — Veda's input driver for Linux: the keyboards, mice, tablets
//! and touchscreens Linux drives (on USB, virtio…: its event devices,
//! evdev) as input devices of Veda's window system, whose `input` service
//! takes their events as it takes those of Veda's own drivers.
//!
//! Each event device is opened as it appears (the kernel's uevents say
//! when) and grabbed, so that nothing of Linux's acts on what is typed, and
//! one thread waits on them and the uevents. What a device reports at once
//! (up to its `SYN_REPORT`) goes to the service in one batch: keys and
//! buttons by evdev's codes, which are Veda's; relative motion and wheels;
//! where an absolute pointer is (a tablet's, a touchscreen's, whose touch
//! is the left button), as a fraction of its range; a touchpad's fingers as
//! a pointer's motion, scrolling and clicks (`touchpad`). Keys a device held
//! when it went away, or whose releases Linux dropped (its queue
//! overflowed), are let go. The keyboards' LEDs show what the window
//! system's keyboard does: Num Lock always (the keypad types digits), Caps
//! Lock while it is on (each press of it, on any keyboard here, turns it on
//! or off).

mod touchpad;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use guest_sys::{POLLIN, PollFd, ioc, ioctl, poll_many};
use vproto::input::{InputEvent, InputSink, keys};

use touchpad::{Axis, Touchpad};

// Linux's input events (linux/input-event-codes.h).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_LED: u16 = 0x11;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const LED_NUML: u16 = 0x00;
const LED_CAPSL: u16 = 0x01;
const KEY_A: u16 = 30;
/// Buttons are from here to `KEY_OK`, and from `BTN_TRIGGER_HAPPY` on.
const BTN_MISC: u16 = 0x100;
const BTN_JOYSTICK: u16 = 0x120;
const BTN_GAMEPAD: u16 = 0x130;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOUCH: u16 = 0x14A;
const KEY_OK: u16 = 0x160;
const BTN_TRIGGER_HAPPY: u16 = 0x2C0;
const INPUT_PROP_DIRECT: u16 = 0x01;
/// How many codes each kind has (`KEY_CNT`…), and properties.
const KEY_CNT: usize = 0x300;
const REL_CNT: usize = 0x10;
const ABS_CNT: usize = 0x40;
const LED_CNT: usize = 0x10;
const PROP_CNT: usize = 0x20;
/// Bytes of a `struct input_event` on x86-64: a `timeval`, the type, the
/// code and the value.
const EVENT_SIZE: usize = 24;

// evdev's requests (linux/input.h): their numbers, of type 'E'.
const EVIOCGID: u8 = 0x02;
const EVIOCGNAME: u8 = 0x06;
const EVIOCGPROP: u8 = 0x09;
const EVIOCGKEY: u8 = 0x18;
const EVIOCGBIT: u8 = 0x20;
const EVIOCGABS: u8 = 0x40;
const EVIOCGRAB: u8 = 0x90;

/// What evdev request `nr` reads into `len` bytes.
fn get(file: &File, nr: u8, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    // SAFETY: the request writes at most `len` bytes into `buf`.
    unsafe { ioctl(file.as_raw_fd(), ioc(2, b'E', nr, len), buf.as_mut_ptr() as usize)? };
    Ok(buf)
}

/// Whether bit `n` of a bitmap evdev gave is set.
fn has(bits: &[u8], n: u16) -> bool {
    bits.get(n as usize / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)
}

/// A key's code (not a button's).
fn is_key(code: u16) -> bool {
    code < BTN_MISC || (KEY_OK..BTN_TRIGGER_HAPPY).contains(&code)
}

/// How a device's absolute axes are taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pointer {
    /// It has none, or they are not a pointer's (a joystick's).
    None,
    /// Where on the screen (a tablet's, a pen's).
    Tablet,
    /// Where on the screen, touched: the touch is the left button.
    Touchscreen,
    /// Where on the pad: a touchpad's.
    Touchpad,
}

/// An event device of Linux's.
struct Device {
    file: File,
    /// Its name and what it is on, for the log.
    name: String,
    pointer: Pointer,
    /// The ranges of its X and Y axes, and where they are.
    x: (i32, i32),
    y: (i32, i32),
    at: (i32, i32),
    /// Whether it has a left button (or a touch is one).
    left: bool,
    /// Whether it has Num Lock's or Caps Lock's LED.
    leds: bool,
    /// A touchpad's fingers, made the pointer's motion.
    touchpad: Option<Touchpad>,
    /// What it reported since its last report.
    pending: Vec<InputEvent>,
    moved: bool,
    motion: (i32, i32),
    /// The keys and buttons it holds down.
    held: BTreeSet<u16>,
    /// Linux dropped some of its events: what comes until its next report
    /// goes too, then what it holds is asked for.
    dropped: bool,
}

/// The name of an input bus (`BUS_*` of linux/input.h).
fn bus(bus: u16) -> String {
    match bus {
        0x01 => "pci".into(),
        0x03 => "usb".into(),
        0x05 => "bluetooth".into(),
        0x06 => "virtual".into(),
        0x11 => "i8042".into(),
        0x18 => "i2c".into(),
        0x19 => "host".into(),
        0x1C => "spi".into(),
        b => format!("bus {b:#x}"),
    }
}

impl Device {
    /// Opens the event device at `path`, grabbed, and says what it is.
    fn open(path: &Path) -> io::Result<Device> {
        const O_NONBLOCK: i32 = 0o4000;
        // (Written to: its LEDs.)
        let file = std::fs::OpenOptions::new().read(true).write(true).custom_flags(O_NONBLOCK).open(path)?;
        // (A device may have no name.)
        let name = get(&file, EVIOCGNAME, 256).unwrap_or_default();
        let name = String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or_default()).trim().to_string();
        let id = get(&file, EVIOCGID, 8)?;
        let word = |i: usize| u16::from_ne_bytes([id[i], id[i + 1]]);
        let name = format!("{name} ({} {:04x}:{:04x})", bus(word(0)), word(2), word(4));
        let keys = get(&file, EVIOCGBIT + EV_KEY as u8, KEY_CNT / 8)?;
        let rel = get(&file, EVIOCGBIT + EV_REL as u8, REL_CNT / 8)?;
        let abs = get(&file, EVIOCGBIT + EV_ABS as u8, ABS_CNT / 8)?;
        let props = get(&file, EVIOCGPROP, PROP_CNT / 8)?;
        let leds = get(&file, EVIOCGBIT + EV_LED as u8, LED_CNT / 8)?;
        let axes = has(&abs, ABS_X) && has(&abs, ABS_Y) && !has(&keys, BTN_JOYSTICK) && !has(&keys, BTN_GAMEPAD);
        let direct = has(&props, INPUT_PROP_DIRECT);
        let touch = has(&keys, BTN_TOUCH);
        let pointer = if !axes {
            Pointer::None
        } else if has(&keys, BTN_TOOL_FINGER) && !direct {
            Pointer::Touchpad
        } else if direct && touch {
            Pointer::Touchscreen
        } else if has(&keys, keys::BTN_LEFT) || touch || has(&keys, BTN_TOOL_PEN) {
            Pointer::Tablet
        } else {
            Pointer::None
        };
        // An axis: where it is, its minimum and maximum, its units a
        // millimetre.
        let axis = |a: u16| -> (i32, Axis) {
            let info = get(&file, EVIOCGABS + a as u8, 24).unwrap_or_else(|_| vec![0; 24]);
            let field = |i: usize| i32::from_ne_bytes([info[i * 4], info[i * 4 + 1], info[i * 4 + 2], info[i * 4 + 3]]);
            (field(0), Axis { min: field(1), max: field(2), resolution: field(5) })
        };
        let ((x, x_axis), (y, y_axis)) = (axis(ABS_X), axis(ABS_Y));
        let mut what = Vec::new();
        if has(&keys, KEY_A) {
            what.push("keyboard");
        }
        if has(&rel, REL_X) && has(&rel, REL_Y) {
            what.push("mouse");
        }
        match pointer {
            Pointer::Tablet => what.push("tablet"),
            Pointer::Touchscreen => what.push("touchscreen"),
            Pointer::Touchpad => what.push("touchpad"),
            Pointer::None => {}
        }
        if what.is_empty() && (0..KEY_CNT as u16).any(|k| is_key(k) && has(&keys, k)) {
            what.push("keys");
        }
        if what.is_empty() {
            what.push("nothing Veda takes");
        }
        // Linux's own handlers of keys (SysRq's) see nothing of it now.
        // SAFETY: the request takes a value.
        if let Err(e) = unsafe { ioctl(file.as_raw_fd(), ioc(1, b'E', EVIOCGRAB, 4), 1) } {
            println!("input: {name}: cannot have it alone: {e}");
        }
        println!("input: {name}: {}", what.join(" and "));
        Ok(Device {
            file,
            name,
            pointer,
            x: (x_axis.min, x_axis.max),
            y: (y_axis.min, y_axis.max),
            at: (x, y),
            left: has(&keys, keys::BTN_LEFT),
            leds: has(&leds, LED_NUML) || has(&leds, LED_CAPSL),
            touchpad: (pointer == Pointer::Touchpad).then(|| Touchpad::new(x_axis, y_axis)),
            pending: Vec::new(),
            moved: false,
            motion: (0, 0),
            held: BTreeSet::new(),
            dropped: false,
        })
    }

    /// Reads what the device has: the batches it reported. An error is
    /// that it went away.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<Vec<Vec<InputEvent>>> {
        let mut batches = Vec::new();
        loop {
            let n = match self.file.read(buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(batches),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            for e in buf[..n].as_chunks::<EVENT_SIZE>().0 {
                let field = |at: usize| i64::from_ne_bytes(e[at..at + 8].try_into().unwrap_or_default());
                let time_us = (field(0) * 1_000_000 + field(8)) as u64;
                let kind = u16::from_ne_bytes([e[16], e[17]]);
                let code = u16::from_ne_bytes([e[18], e[19]]);
                let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
                batches.extend(self.event(kind, code, value, time_us));
            }
        }
    }

    /// Takes an event (at `time_us`, Linux's): a batch at the end of a
    /// report.
    fn event(&mut self, kind: u16, code: u16, value: i32, time_us: u64) -> Option<Vec<InputEvent>> {
        if self.dropped {
            if (kind, code) == (EV_SYN, SYN_REPORT) {
                self.dropped = false;
                return self.resync(time_us);
            }
            return None;
        }
        match (kind, code) {
            (EV_SYN, SYN_REPORT) => {
                if let Some(t) = &mut self.touchpad {
                    t.report(time_us, &mut self.pending);
                }
                return self.report();
            }
            (EV_SYN, SYN_DROPPED) => {
                self.dropped = true;
                self.pending.clear();
                self.moved = false;
                self.motion = (0, 0);
                if let Some(t) = &mut self.touchpad {
                    t.dropped();
                }
            }
            // Auto-repeat (2) is the window system's.
            (EV_KEY, _) if value != 2 => {
                let pressed = value != 0;
                let changed = if pressed { self.held.insert(code) } else { self.held.remove(&code) };
                if changed {
                    self.key_changed(code, pressed, time_us);
                }
            }
            (EV_REL, REL_X) => self.motion.0 += value,
            (EV_REL, REL_Y) => self.motion.1 += value,
            (EV_REL, REL_WHEEL) => self.pending.push(InputEvent::Scroll { dx: 0, dy: value }),
            (EV_REL, REL_HWHEEL) => self.pending.push(InputEvent::Scroll { dx: value, dy: 0 }),
            (EV_ABS, ABS_X | ABS_Y) => {
                if let Some(t) = &mut self.touchpad {
                    t.abs(code, value);
                } else if matches!(self.pointer, Pointer::Tablet | Pointer::Touchscreen) {
                    if code == ABS_X {
                        self.at.0 = value;
                    } else {
                        self.at.1 = value;
                    }
                    self.moved = true;
                }
            }
            _ => {}
        }
        None
    }

    /// Key or button `code` went down or up: the touchpad's, or Veda's
    /// event for it.
    fn key_changed(&mut self, code: u16, pressed: bool, time_us: u64) {
        if let Some(t) = &mut self.touchpad
            && t.key(code, pressed, time_us, &mut self.pending)
        {
            return;
        }
        self.pending.extend(self.key(code, pressed));
    }

    /// Veda's event for key or button `code` going down or up, if it takes
    /// it.
    fn key(&self, code: u16, pressed: bool) -> Option<InputEvent> {
        let button = |button| Some(InputEvent::Button { button, pressed });
        match code {
            keys::BTN_LEFT => button(0),
            keys::BTN_RIGHT => button(1),
            keys::BTN_MIDDLE => button(2),
            BTN_TOUCH if !self.left && matches!(self.pointer, Pointer::Tablet | Pointer::Touchscreen) => button(0),
            _ if is_key(code) => Some(InputEvent::Key { code, pressed }),
            _ => None,
        }
    }

    /// The batch of a report, if it has anything: where the pointer went
    /// first, so that buttons pressed in the same report act there.
    fn report(&mut self) -> Option<Vec<InputEvent>> {
        if std::mem::take(&mut self.moved) {
            let span = |(min, max): (i32, i32)| (max - min).max(1);
            let (sx, sy) = (span(self.x), span(self.y));
            let x = (self.at.0 - self.x.0).clamp(0, sx) as u32;
            let y = (self.at.1 - self.y.0).clamp(0, sy) as u32;
            self.pending.insert(0, InputEvent::Absolute { x, y, max_x: sx as u32, max_y: sy as u32 });
        }
        if self.motion != (0, 0) {
            let (dx, dy) = std::mem::take(&mut self.motion);
            self.pending.insert(0, InputEvent::Motion { dx, dy });
        }
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    /// After Linux dropped events: the presses and releases that make what
    /// Veda was told the device holds what it does hold.
    fn resync(&mut self, time_us: u64) -> Option<Vec<InputEvent>> {
        let now = get(&self.file, EVIOCGKEY, KEY_CNT / 8).ok()?;
        for code in 0..KEY_CNT as u16 {
            let down = has(&now, code);
            let changed = if down { self.held.insert(code) } else { self.held.remove(&code) };
            if changed {
                self.key_changed(code, down, time_us);
            }
        }
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    /// Lights its LEDs as the window system's keyboard is: Num Lock, and
    /// Caps Lock if `caps`.
    fn light(&mut self, caps: bool) {
        if !self.leds {
            return;
        }
        let mut events = Vec::with_capacity(3 * EVENT_SIZE);
        for (kind, code, value) in
            [(EV_LED, LED_NUML, 1), (EV_LED, LED_CAPSL, i32::from(caps)), (EV_SYN, SYN_REPORT, 0)]
        {
            events.extend_from_slice(&[0; 16]);
            events.extend_from_slice(&kind.to_ne_bytes());
            events.extend_from_slice(&code.to_ne_bytes());
            events.extend_from_slice(&value.to_ne_bytes());
        }
        if let Err(e) = self.file.write_all(&events) {
            println!("input: {}: cannot light its LEDs: {e}", self.name);
        }
    }

    /// Lets go of what it holds (it went away).
    fn let_go(&mut self) -> Vec<InputEvent> {
        let held = std::mem::take(&mut self.held);
        if let Some(t) = &mut self.touchpad {
            return t.let_go().into_iter().collect();
        }
        held.into_iter().filter_map(|code| self.key(code, false)).collect()
    }
}

/// A socket the kernel's uevents come on: devices that come and go.
fn uevents() -> io::Result<OwnedFd> {
    const AF_NETLINK: i32 = 16;
    const SOCK_DGRAM: i32 = 2;
    const SOCK_NONBLOCK: i32 = 0o4000;
    const SOCK_CLOEXEC: i32 = 0o2000000;
    const NETLINK_KOBJECT_UEVENT: i32 = 15;
    let socket = guest_sys::socket(AF_NETLINK, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, NETLINK_KOBJECT_UEVENT)?;
    // A `sockaddr_nl`: the family, any port, the kernel's group (1).
    let mut address = [0u8; 12];
    address[0..2].copy_from_slice(&(AF_NETLINK as u16).to_ne_bytes());
    address[8..12].copy_from_slice(&1u32.to_ne_bytes());
    guest_sys::bind(socket.as_raw_fd(), &address)?;
    Ok(socket)
}

/// Reads the uevents that came: whether one was an input device's.
fn input_uevent(socket: &OwnedFd) -> bool {
    let mut buf = [0u8; 4096];
    let mut input = false;
    while let Ok(n) = guest_sys::read(socket.as_raw_fd(), &mut buf) {
        input |= buf[..n].split(|&b| b == 0).any(|field| field == b"SUBSYSTEM=input");
    }
    input
}

/// Opens the event devices that are not open yet (known by their device
/// numbers), their LEDs lit as `caps` says.
fn scan(devices: &mut BTreeMap<u64, Device>, caps: bool) {
    let Ok(dir) = std::fs::read_dir("/dev/input") else { return };
    for entry in dir.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let path = entry.path();
        let Ok(number) = std::fs::metadata(&path).map(|m| m.rdev()) else { continue };
        if devices.contains_key(&number) {
            continue;
        }
        match Device::open(&path) {
            Ok(mut d) => {
                d.light(caps);
                devices.insert(number, d);
            }
            Err(e) => println!("input: cannot use {}: {e}", path.display()),
        }
    }
}

/// The window system's input service, once it is there.
fn connect() -> InputSink {
    let mut told = false;
    loop {
        match InputSink::connect() {
            Ok(sink) => return sink,
            Err(e) => {
                if !told {
                    println!("input: waiting for Veda's input service ({e:?})");
                    told = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn main() {
    let mut sink = connect();
    // Uevents before the first look, lest a device come in between.
    let uevents = match uevents() {
        Ok(s) => s,
        Err(e) => {
            println!("input: no uevents: {e}");
            std::process::exit(1);
        }
    };
    let mut devices: BTreeMap<u64, Device> = BTreeMap::new();
    let mut caps = false;
    scan(&mut devices, caps);
    let mut buf = vec![0u8; EVENT_SIZE * 64];
    loop {
        let mut fds = vec![PollFd::new(uevents.as_raw_fd(), POLLIN)];
        let numbers: Vec<u64> = devices.keys().copied().collect();
        fds.extend(devices.values().map(|d| PollFd::new(d.file.as_raw_fd(), POLLIN)));
        if let Err(e) = poll_many(&mut fds, -1) {
            if e.kind() != io::ErrorKind::Interrupted {
                println!("input: cannot wait: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
            continue;
        }
        let mut gone = Vec::new();
        let was = caps;
        for (number, fd) in numbers.iter().zip(&fds[1..]) {
            let Some(d) = devices.get_mut(number).filter(|_| fd.revents != 0) else { continue };
            match d.read(&mut buf) {
                Ok(batches) => {
                    for b in batches {
                        if b.contains(&InputEvent::Key { code: keys::CAPSLOCK, pressed: true }) {
                            caps = !caps;
                        }
                        sink.report(b);
                    }
                }
                Err(_) => gone.push(*number),
            }
        }
        if caps != was {
            devices.values_mut().for_each(|d| d.light(caps));
        }
        for number in &gone {
            if let Some(mut d) = devices.remove(number) {
                let released = d.let_go();
                if !released.is_empty() {
                    sink.report(released);
                }
                println!("input: {}: gone", d.name);
            }
        }
        // A device came, or went (and its number may be another's now).
        if (fds[0].revents != 0 && input_uevent(&uevents)) || !gone.is_empty() {
            scan(&mut devices, caps);
        }
    }
}
