//! `gpiotest` — checks, inside the driver VM's Linux, the GPIO pins Veda
//! gave it, as a test's ACPI table describes them: a simulated controller
//! of devmgr's (`VTST0002`, whose even pins drive the odd ones after them),
//! and a device wired to three of its pins — 0 an output, 1 an input that
//! interrupts on both edges, 3 an input. The guest has those pins, and no
//! other, on a GPIO controller of the platform's (`VEDA0001`): driving 0
//! reaches 1, whose edges come as events, through Linux's GPIO character
//! device. It says `gpiotest: PASS` when all is well.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use guest_sys::{POLLIN, ioc, ioctl, poll};

struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool) {
        println!("gpiotest: {} {}", if ok { "ok  " } else { "FAIL" }, what);
        if !ok {
            self.failed += 1;
        }
    }
}

// Linux's GPIO character device, version 2 (`include/uapi/linux/gpio.h`).

#[repr(C)]
struct ChipInfo {
    name: [u8; 32],
    label: [u8; 32],
    lines: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ConfigAttribute {
    id: u32,
    padding: u32,
    value: u64,
    mask: u64,
}

#[repr(C)]
struct LineConfig {
    flags: u64,
    num_attrs: u32,
    padding: [u32; 5],
    attrs: [ConfigAttribute; 10],
}

#[repr(C)]
struct LineRequest {
    offsets: [u32; 64],
    consumer: [u8; 32],
    config: LineConfig,
    num_lines: u32,
    event_buffer_size: u32,
    padding: [u32; 5],
    fd: i32,
}

#[repr(C)]
#[derive(Default)]
struct LineValues {
    bits: u64,
    mask: u64,
}

#[repr(C)]
#[derive(Default)]
struct LineEvent {
    timestamp_ns: u64,
    id: u32,
    offset: u32,
    seqno: u32,
    line_seqno: u32,
    padding: [u32; 6],
}

const GPIO: u8 = 0xB4;
const GET_CHIPINFO: u32 = ioc(2, GPIO, 0x01, size_of::<ChipInfo>());
const GET_LINE: u32 = ioc(3, GPIO, 0x07, size_of::<LineRequest>());
const GET_VALUES: u32 = ioc(3, GPIO, 0x0E, size_of::<LineValues>());
const SET_VALUES: u32 = ioc(3, GPIO, 0x0F, size_of::<LineValues>());
const FLAG_INPUT: u64 = 1 << 2;
const FLAG_OUTPUT: u64 = 1 << 3;
const FLAG_EDGES: u64 = 1 << 4 | 1 << 5;
const RISING: u32 = 1;
const FALLING: u32 = 2;

/// The chip of the platform's GPIO controller (its label is the ACPI
/// device's name).
fn platform_chip() -> Option<(File, ChipInfo)> {
    let mut chips: Vec<_> = fs::read_dir("/dev").ok()?.flatten().map(|e| e.path()).collect();
    chips.sort();
    for path in chips.iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("gpiochip"))) {
        let Ok(chip) = OpenOptions::new().read(true).write(true).open(path) else { continue };
        let mut info = ChipInfo { name: [0; 32], label: [0; 32], lines: 0 };
        // SAFETY: the kernel writes a gpiochip_info.
        if unsafe { ioctl(chip.as_raw_fd(), GET_CHIPINFO, &mut info as *mut _ as usize) }.is_ok()
            && info.label.starts_with(b"VEDA0001")
        {
            return Some((chip, info));
        }
    }
    None
}

/// Lines of `chip` requested with `flags`: a file of their own.
fn request(chip: RawFd, offsets: &[u32], flags: u64) -> std::io::Result<OwnedFd> {
    let mut r = LineRequest {
        offsets: [0; 64],
        consumer: [0; 32],
        config: LineConfig { flags, num_attrs: 0, padding: [0; 5], attrs: [ConfigAttribute::default(); 10] },
        num_lines: offsets.len() as u32,
        event_buffer_size: 0,
        padding: [0; 5],
        fd: -1,
    };
    r.offsets[..offsets.len()].copy_from_slice(offsets);
    r.consumer[..8].copy_from_slice(b"gpiotest");
    // SAFETY: the kernel reads the request and writes the lines' file.
    unsafe { ioctl(chip, GET_LINE, &mut r as *mut _ as usize) }?;
    // SAFETY: a file the kernel just made for us.
    Ok(unsafe { OwnedFd::from_raw_fd(r.fd) })
}

/// The level of a requested line (the first of its request).
fn level(line: &OwnedFd) -> Option<bool> {
    let mut v = LineValues { bits: 0, mask: 1 };
    // SAFETY: the kernel reads the mask and writes the bits.
    unsafe { ioctl(line.as_raw_fd(), GET_VALUES, &mut v as *mut _ as usize) }.ok()?;
    Some(v.bits & 1 != 0)
}

fn drive(line: &OwnedFd, high: bool) -> bool {
    let mut v = LineValues { bits: high as u64, mask: 1 };
    // SAFETY: the kernel reads the values.
    unsafe { ioctl(line.as_raw_fd(), SET_VALUES, &mut v as *mut _ as usize) }.is_ok()
}

/// The next edge of a line requested with edges, within two seconds.
fn edge(line: &OwnedFd) -> Option<LineEvent> {
    if poll(line.as_raw_fd(), POLLIN, 2000).ok()? & POLLIN == 0 {
        return None;
    }
    // SAFETY: the file is the line's; it outlives this read.
    let mut file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(line.as_raw_fd()) });
    let mut event = LineEvent::default();
    // SAFETY: a gpio_v2_line_event's bytes, which the kernel writes whole.
    let bytes = unsafe { std::slice::from_raw_parts_mut(&mut event as *mut _ as *mut u8, size_of::<LineEvent>()) };
    file.read_exact(bytes).ok()?;
    Some(event)
}

fn main() {
    let mut c = Checks { failed: 0 };
    let Some((chip, info)) = platform_chip() else {
        c.check("the platform's GPIO controller is a chip of Linux's", false);
        println!("gpiotest: FAIL");
        return;
    };
    let label = String::from_utf8_lossy(&info.label).trim_end_matches('\0').to_string();
    c.check(&format!("{label} has the lines up to its last pin (4)"), info.lines == 4);
    let chip = chip.as_raw_fd();
    let output = request(chip, &[0], FLAG_OUTPUT);
    let input = request(chip, &[1], FLAG_INPUT | FLAG_EDGES);
    c.check("pin 0 is an output, pin 1 an input with its edges", output.is_ok() && input.is_ok());
    c.check("pin 2, which no device is wired to, is not there", request(chip, &[2], FLAG_INPUT).is_err());
    let (Ok(output), Ok(input)) = (output, input) else {
        println!("gpiotest: FAIL");
        return;
    };
    c.check("pin 1 is low", level(&input) == Some(false));
    for (high, id) in [(true, RISING), (false, FALLING)] {
        let driven = drive(&output, high);
        let event = edge(&input);
        let what = if high { "rising" } else { "falling" };
        c.check(
            &format!("driving pin 0 {}, pin 1's {what} edge comes", if high { "high" } else { "low" }),
            driven && event.as_ref().is_some_and(|e| e.id == id && e.offset == 1),
        );
        c.check(&format!("pin 1 is {}", if high { "high" } else { "low" }), level(&input) == Some(high));
    }
    let three = request(chip, &[3], FLAG_INPUT);
    c.check("pin 3, nothing driving it, is low", three.as_ref().ok().and_then(level) == Some(false));
    println!("gpiotest: {}", if c.failed == 0 { "PASS" } else { "FAIL" });
}
