//! `xhci` — the driver for xHCI USB host controllers: the USB 3
//! controllers of PCs since about 2012, QEMU's `qemu-xhci` and VirtualBox's
//! USB 3 controller.
//!
//! It takes the controller over from the firmware, resets it, and
//! enumerates the devices on the root hub's ports and behind USB 2.0 hubs.
//! Keyboards, mice and tablets (the HID class) are configured, and their
//! reports go to the window system's `input` service, like those of the
//! PS/2 and virtio drivers. Other devices get an address and their
//! descriptors are read, to log what they are, but nothing more: a USB
//! stick, for instance, is never configured, so its data is never touched.
//!
//! One thread does everything. Commands and control transfers are issued
//! one at a time and waited for; while waiting, the event ring is still
//! drained, so input keeps flowing. Port changes, and other work that has
//! to wait for a device, are queued and done one after the other.

#![no_std]
#![no_main]

extern crate alloc;

mod controller;
mod device;
mod hid;
mod hub;
mod ring;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::vec::Vec;

use vproto::input::{InputEvent, InputSink, keys};
use vproto::pci::pcidev;
use vrt::object::Channel;
use vrt::println;
use vrt::time::now_ns;
use vusb::hid::Leds;
use vusb::request::Setup;
use vusb::xhci::{Trb, completion, td_size, trb_type};

use controller::{CONTROL_BUFFER, Controller, endpoint_state};
use device::{Device, Function};

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
const MS: u64 = 1_000_000;
/// How long a command may take.
const COMMAND_TIMEOUT_MS: u64 = 5000;
/// How long a control transfer may take (USB 2.0 section 9.2.6.4).
const CONTROL_TIMEOUT_MS: u64 = 5000;

/// Why talking to a device failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbError {
    /// A command failed, with this completion code.
    Command(u8),
    /// A transfer failed, with this completion code.
    Transfer(u8),
    /// No answer in time.
    Timeout,
    /// Out of DMA memory.
    NoMemory,
    /// The device sent a descriptor that cannot be used.
    BadDescriptor,
    /// The device is gone.
    Gone,
}

impl core::fmt::Display for UsbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UsbError::Command(code) => write!(f, "command failed ({})", completion::name(*code)),
            UsbError::Transfer(code) => write!(f, "transfer failed ({})", completion::name(*code)),
            UsbError::Timeout => f.write_str("no answer"),
            UsbError::NoMemory => f.write_str("out of DMA memory"),
            UsbError::BadDescriptor => f.write_str("unusable descriptor"),
            UsbError::Gone => f.write_str("device gone"),
        }
    }
}

/// Work that waits for devices, done outside event handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// A root hub port changed.
    RootPort(u8),
    /// A hub (slot) reported changes: its status change bitmap.
    HubPorts(u8, Vec<u8>),
    /// An interrupt endpoint (slot, device context index) stopped after an
    /// error.
    Recover(u8, u8),
    /// The keyboard lights changed.
    Leds,
}

/// Queues work unless the same is already waiting.
fn queue(work: &mut VecDeque<Work>, w: Work) {
    if !work.contains(&w) {
        work.push_back(w);
    }
}

/// The TRBs of a control transfer on the ring, to match events with.
struct ControlTd {
    setup: u64,
    /// (address, offset in the data, length) of each data TRB.
    data: Vec<(u64, u32, u32)>,
    status: u64,
}

/// The driver: a controller and the devices on it.
pub struct Xhci {
    hc: Controller,
    _pci: pcidev::Client,
    /// Devices by slot.
    devices: BTreeMap<u8, Device>,
    /// The device on each root port.
    root: Vec<Option<u8>>,
    work: VecDeque<Work>,
    /// The completion event of the last command.
    command_done: Option<Trb>,
    /// Transfer events of endpoint 0 (control transfers).
    control_events: Vec<Trb>,
    input: InputSink,
    /// The keyboard lights: Num Lock is on, as the keypad types digits.
    leds: Leds,
}

impl Xhci {
    fn run(mut self) -> i32 {
        for port in 1..=self.root.len() as u8 {
            queue(&mut self.work, Work::RootPort(port));
        }
        loop {
            while let Some(w) = self.work.pop_front() {
                match w {
                    Work::RootPort(port) => self.root_port_changed(port),
                    Work::HubPorts(slot, report) => self.hub_changed(slot, &report),
                    Work::Recover(slot, index) => {
                        self.recover(slot, index);
                        if let Some(pipe) = self.devices.get_mut(&slot).and_then(|d| d.pipe_mut(index)) {
                            pipe.arm(&self.hc);
                        }
                    }
                    Work::Leds => {
                        let slots: Vec<u8> = self.devices.keys().copied().collect();
                        for slot in slots {
                            self.set_leds(slot);
                        }
                    }
                }
            }
            if !self.hc.healthy() {
                println!("the controller stopped with an error");
                return 1;
            }
            self.pump(vabi::DEADLINE_INFINITE);
        }
    }

    // Events.

    /// Waits for events until `deadline` (or the next interrupt) and
    /// handles them.
    fn pump(&mut self, deadline: u64) {
        self.hc.wait(deadline);
        self.drain();
    }

    /// Handles the events the controller has written.
    fn drain(&mut self) {
        while let Some(ev) = self.hc.next_event() {
            match ev.kind() {
                trb_type::COMMAND_COMPLETION => self.command_done = Some(ev),
                trb_type::TRANSFER_EVENT => self.on_transfer(ev),
                trb_type::PORT_STATUS_CHANGE => queue(&mut self.work, Work::RootPort(ev.port())),
                trb_type::HOST_CONTROLLER => {
                    println!("controller event: {}", completion::name(ev.completion_code()))
                }
                _ => {}
            }
        }
        self.hc.events_done();
    }

    /// A transfer finished: control transfers are waited for elsewhere;
    /// reports of keyboards, mice and hubs are handled here.
    fn on_transfer(&mut self, ev: Trb) {
        let (slot, index) = (ev.slot(), ev.endpoint());
        if index == 1 {
            self.control_events.push(ev);
            return;
        }
        let Some(dev) = self.devices.get_mut(&slot) else { return };
        match &mut dev.function {
            Function::Hub(hub) if hub.pipe.index == index => {
                if let Some(report) = hub.pipe.complete(&ev, &self.hc, &mut self.work) {
                    queue(&mut self.work, Work::HubPorts(slot, report));
                }
            }
            Function::Hid(interfaces) => {
                let Some(h) = interfaces.iter_mut().find(|h| h.pipe.index == index) else { return };
                let Some(report) = h.pipe.complete(&ev, &self.hc, &mut self.work) else { return };
                let mut events = Vec::new();
                h.decoder.decode(&report, &mut events);
                if events.contains(&InputEvent::Key { code: keys::CAPSLOCK, pressed: true }) {
                    self.leds.caps_lock = !self.leds.caps_lock;
                    queue(&mut self.work, Work::Leds);
                }
                if !events.is_empty() {
                    self.input.report(events);
                }
            }
            _ => {}
        }
    }

    /// Waits `ms` milliseconds, handling events meanwhile.
    fn sleep(&mut self, ms: u64) {
        let end = now_ns() + ms * MS;
        while now_ns() < end {
            self.pump(end);
        }
    }

    // Commands and control transfers.

    /// Runs a command and waits for its completion.
    fn command(&mut self, trb: Trb) -> Result<Trb, UsbError> {
        self.command_done = None;
        let at = self.hc.submit_command(trb);
        let end = now_ns() + COMMAND_TIMEOUT_MS * MS;
        loop {
            if let Some(done) = self.command_done.take_if(|e| e.pointer() == at) {
                return match done.completion_code() {
                    completion::SUCCESS => Ok(done),
                    code => Err(UsbError::Command(code)),
                };
            }
            if now_ns() >= end {
                self.hc.abort_command();
                self.drain();
                return Err(UsbError::Timeout);
            }
            self.pump(end);
        }
    }

    /// A control transfer on endpoint 0 of `slot`: sends `out`, or returns
    /// what the device sent (up to `setup.length` bytes).
    fn control(&mut self, slot: u8, setup: Setup, out: &[u8]) -> Result<Vec<u8>, UsbError> {
        let len = (setup.length as usize).min(CONTROL_BUFFER);
        if !setup.is_in() {
            self.hc.control_buffer.write(0, &out[..len.min(out.len())]);
        }
        self.control_events.clear();
        let td = self.queue_control(slot, &setup)?;
        let end = now_ns() + CONTROL_TIMEOUT_MS * MS;
        let mut received = len as u32;
        loop {
            for ev in core::mem::take(&mut self.control_events) {
                if ev.slot() != slot {
                    continue;
                }
                let code = ev.completion_code();
                if let Some(&(_, offset, chunk)) = td.data.iter().find(|d| d.0 == ev.pointer()) {
                    match code {
                        completion::SUCCESS => {}
                        completion::SHORT_PACKET => received = offset + chunk.saturating_sub(ev.residual()),
                        _ => {
                            self.recover(slot, 1);
                            return Err(UsbError::Transfer(code));
                        }
                    }
                } else if ev.pointer() == td.setup || ev.pointer() == td.status {
                    if code != completion::SUCCESS {
                        self.recover(slot, 1);
                        return Err(UsbError::Transfer(code));
                    }
                    if ev.pointer() == td.status {
                        if !setup.is_in() {
                            return Ok(Vec::new());
                        }
                        // SAFETY: the transfer is complete; the controller
                        // no longer writes the buffer.
                        return Ok(unsafe { self.hc.control_buffer.bytes(0, received as usize) }.to_vec());
                    }
                }
            }
            if now_ns() >= end {
                self.recover(slot, 1);
                return Err(UsbError::Timeout);
            }
            self.pump(end);
        }
    }

    /// A control transfer that reads.
    fn control_in(&mut self, slot: u8, setup: Setup) -> Result<Vec<u8>, UsbError> {
        self.control(slot, setup, &[])
    }

    /// Puts a control transfer on endpoint 0's ring: the setup stage, the
    /// data stage in page-sized TRBs, and the status stage.
    fn queue_control(&mut self, slot: u8, setup: &Setup) -> Result<ControlTd, UsbError> {
        let (version, buffer) = (self.hc.version, self.hc.control_buffer.phys());
        let dev = self.devices.get_mut(&slot).ok_or(UsbError::Gone)?;
        let packet = dev.ep0_packet_size;
        let ring = &mut dev.ep0;
        let setup_at = ring.push(Trb::setup_stage(setup));
        let total = (setup.length as usize).min(CONTROL_BUFFER) as u32;
        let mut data = Vec::new();
        let mut done = 0;
        while done < total {
            let at = buffer + done as u64;
            let len = (total - done).min(4096 - (at % 4096) as u32);
            let more = done + len < total;
            let size = td_size(version, done, len, total, packet);
            let trb = if done == 0 {
                Trb::data_stage(at, len, size, setup.is_in(), more)
            } else {
                Trb::normal(at, len, size, more)
            };
            data.push((ring.push(trb), done, len));
            done += len;
        }
        let status = ring.push(Trb::status_stage(total > 0 && setup.is_in()));
        self.hc.ring_doorbell(slot, 1);
        Ok(ControlTd { setup: setup_at, data, status })
    }

    /// Makes an endpoint usable after a failed or abandoned transfer: a
    /// halted endpoint is reset, a running one stopped, and the controller
    /// continues after the TRBs given so far. A halted interrupt endpoint
    /// is restarted on the device too.
    fn recover(&mut self, slot: u8, index: u8) {
        let Some(dev) = self.devices.get(&slot) else { return };
        let state = self.hc.endpoint_state(&dev.context, index);
        let stop = match state {
            endpoint_state::HALTED => Some(Trb::reset_endpoint(slot, index)),
            endpoint_state::RUNNING => Some(Trb::stop_endpoint(slot, index)),
            _ => None,
        };
        if let Some(trb) = stop
            && self.command(trb).is_err()
        {
            return;
        }
        let Some(dev) = self.devices.get_mut(&slot) else { return };
        let (next, address) = match (index, dev.pipe_mut(index)) {
            (1, _) => (Some(dev.ep0.next()), None),
            (_, Some(pipe)) => (Some(pipe.ring.next()), Some(pipe.address)),
            _ => (None, None),
        };
        if let Some((at, cycle)) = next {
            let _ = self.command(Trb::set_dequeue(slot, index, at, cycle));
        }
        // After a stall the device's end is halted as well; after an error
        // its data toggle no longer matches the controller's, which the
        // reset set back to DATA0. Clearing the halt fixes both.
        if state == endpoint_state::HALTED
            && let Some(address) = address
        {
            let _ = self.control(slot, Setup::clear_halt(address), &[]);
        }
    }
}

fn main() -> i32 {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        println!("no pcidev channel");
        return 1;
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let Ok(info) = pci.info() else {
        println!("cannot read the device's information");
        return 1;
    };
    let location = format!("{:02x}:{:02x}.{}", info.bus, info.slot, info.function);
    let input = match InputSink::connect() {
        Ok(sink) => sink,
        Err(e) => {
            println!("cannot reach the input service: {:?}", e);
            return 1;
        }
    };
    let hc = match Controller::start(&pci, &info) {
        Ok(hc) => hc,
        Err(e) => {
            println!("controller at {}: {}", location, e);
            return 1;
        }
    };
    let usb3 = hc.ports.iter().filter(|p| p.usb3).count();
    println!(
        "controller at {}: xHCI {:x}.{:x}, {} USB 2 and {} USB 3 ports, {}",
        location,
        hc.version >> 8,
        (hc.version >> 4) & 0xF,
        hc.ports.len() - usb3,
        usb3,
        hc.interrupts
    );
    let ports = hc.ports.len();
    Xhci {
        hc,
        _pci: pci,
        devices: BTreeMap::new(),
        root: alloc::vec![None; ports],
        work: VecDeque::new(),
        command_done: None,
        control_events: Vec::new(),
        input,
        leds: Leds { num_lock: true, ..Leds::default() },
    }
    .run()
}
