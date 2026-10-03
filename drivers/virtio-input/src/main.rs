//! virtio-input driver: forwards tablet, mouse and keyboard events from a
//! virtio input device to the window system's `input` service.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use vproto::input::{self, InputEvent, keys};
use vproto::pci::pcidev;
use vrt::object::Channel;
use vrt::println;
use vvirtio::{DmaBuffer, Segment};

vrt::entry!(main);

const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
const QUEUE_SIZE: u16 = 64;
const EVENT_SIZE: usize = 8;

// Configuration selectors (virtio spec 5.8.2).
const CFG_ID_NAME: u8 = 0x01;
const CFG_ABS_INFO: u8 = 0x12;

// evdev event types.
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;

/// Reads a configuration union selected by (`select`, `subsel`).
fn query(dev: &vvirtio::Device, select: u8, subsel: u8) -> Vec<u8> {
    dev.cfg_write8(0, select);
    dev.cfg_write8(1, subsel);
    let size = dev.cfg_read8(2) as u32;
    (0..size.min(128)).map(|i| dev.cfg_read8(8 + i)).collect()
}

fn abs_max(dev: &vvirtio::Device, axis: u8) -> u32 {
    let info = query(dev, CFG_ABS_INFO, axis);
    if info.len() >= 8 { u32::from_le_bytes([info[4], info[5], info[6], info[7]]) } else { 32767 }
}

/// Accumulates evdev events until a SYN report, then emits a batch.
struct Translator {
    pending: Vec<InputEvent>,
    abs_x: u32,
    abs_y: u32,
    abs_dirty: bool,
    max_x: u32,
    max_y: u32,
    rel_x: i32,
    rel_y: i32,
}

impl Translator {
    fn feed(&mut self, ty: u16, code: u16, value: u32) -> Option<Vec<InputEvent>> {
        match ty {
            EV_KEY => {
                let pressed = value != 0;
                let ev = match code {
                    keys::BTN_LEFT => InputEvent::Button { button: 0, pressed },
                    keys::BTN_RIGHT => InputEvent::Button { button: 1, pressed },
                    keys::BTN_MIDDLE => InputEvent::Button { button: 2, pressed },
                    // Auto-repeat (value 2) is synthesised by the compositor.
                    _ if value == 2 => return None,
                    _ => InputEvent::Key { code, pressed },
                };
                self.pending.push(ev);
            }
            EV_REL => match code {
                REL_X => self.rel_x += value as i32,
                REL_Y => self.rel_y += value as i32,
                REL_WHEEL => self.pending.push(InputEvent::Scroll { dx: 0, dy: value as i32 }),
                REL_HWHEEL => self.pending.push(InputEvent::Scroll { dx: value as i32, dy: 0 }),
                _ => {}
            },
            EV_ABS => match code {
                ABS_X => {
                    self.abs_x = value;
                    self.abs_dirty = true;
                }
                ABS_Y => {
                    self.abs_y = value;
                    self.abs_dirty = true;
                }
                _ => {}
            },
            EV_SYN => {
                if self.abs_dirty {
                    self.pending.insert(
                        0,
                        InputEvent::Absolute { x: self.abs_x, y: self.abs_y, max_x: self.max_x, max_y: self.max_y },
                    );
                    self.abs_dirty = false;
                }
                if self.rel_x != 0 || self.rel_y != 0 {
                    self.pending.insert(0, InputEvent::Motion { dx: self.rel_x, dy: self.rel_y });
                    self.rel_x = 0;
                    self.rel_y = 0;
                }
                if !self.pending.is_empty() {
                    return Some(core::mem::take(&mut self.pending));
                }
            }
            _ => {}
        }
        None
    }
}

fn main() -> i32 {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        println!("no pcidev channel");
        return 1;
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let mut dev = match vvirtio::Device::new(pci) {
        Ok(d) => d,
        Err(e) => {
            println!("device setup failed: {:?}", e);
            return 1;
        }
    };
    if let Err(e) = dev.initialize(0) {
        println!("feature negotiation failed: {:?}", e);
        return 1;
    }
    let name = String::from_utf8_lossy(&query(&dev, CFG_ID_NAME, 0)).into_owned();
    let (max_x, max_y) = (abs_max(&dev, ABS_X as u8), abs_max(&dev, ABS_Y as u8));
    let Ok(mut queue) = dev.setup_queue(0, QUEUE_SIZE) else {
        println!("no event queue");
        return 1;
    };
    let Ok(buffers) = DmaBuffer::new(dev.dma(), QUEUE_SIZE as usize * EVENT_SIZE) else {
        println!("out of DMA memory");
        return 1;
    };
    let irq = dev.msix_vector(Some(0)).ok();
    // Each descriptor maps to one 8-byte event slot; remember which.
    let mut slot_of_head = alloc::vec![0usize; queue.size() as usize];
    for i in 0..queue.size() as usize {
        let seg = Segment { phys: buffers.phys() + (i * EVENT_SIZE) as u64, len: EVENT_SIZE as u32, device_writes: true };
        if let Some(head) = queue.push(&[seg]) {
            slot_of_head[head as usize] = i;
        }
    }
    dev.driver_ok();
    queue.notify();
    println!("{} ready ({}, abs range {}x{})", name, if irq.is_some() { "MSI-X" } else { "polled" }, max_x, max_y);

    let input = match vproto::connect(input::NAME) {
        Ok(ch) => ch,
        Err(e) => {
            println!("cannot reach the input service: {:?}", e);
            return 1;
        }
    };
    let mut t = Translator {
        pending: Vec::new(),
        abs_x: 0,
        abs_y: 0,
        abs_dirty: false,
        max_x,
        max_y,
        rel_x: 0,
        rel_y: 0,
    };
    loop {
        match &irq {
            Some(irq) => {
                let _ = irq.wait_irq(vabi::DEADLINE_INFINITE);
            }
            None => vrt::time::sleep(vrt::time::Duration::from_millis(5)),
        }
        // MSI-X is edge-like: re-arm before draining so no completion is lost.
        if let Some(irq) = &irq {
            let _ = irq.ack();
        }
        let mut reposted = false;
        while let Some((head, _len)) = queue.pop_used() {
            let slot = slot_of_head[head as usize];
            // SAFETY: the device has finished writing this slot.
            let e = unsafe { buffers.bytes(slot * EVENT_SIZE, EVENT_SIZE) };
            let (ty, code) = (u16::from_le_bytes([e[0], e[1]]), u16::from_le_bytes([e[2], e[3]]));
            let value = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
            if let Some(batch) = t.feed(ty, code, value) {
                if input::report(&input, batch).is_err() {
                    println!("input service went away");
                    return 1;
                }
            }
            let seg = Segment { phys: buffers.phys() + (slot * EVENT_SIZE) as u64, len: EVENT_SIZE as u32, device_writes: true };
            if let Some(h) = queue.push(&[seg]) {
                slot_of_head[h as usize] = slot;
            }
            reposted = true;
        }
        if reposted {
            queue.notify();
        }
    }
}
