//! Devices: finding them on the root hub's ports, giving them an address,
//! reading their descriptors, configuring what drives them, and forgetting
//! them when they go away.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vrt::println;
use vusb::Speed;
use vusb::descriptor::{
    Configuration, DeviceDescriptor, Endpoint, class, class_name, control_packet_size, first_language, kind,
    parse_string,
};
use vusb::hub::HubDescriptor;
use vusb::request::Setup;
use vusb::xhci::{EndpointContext, SlotContext, Trb, completion, endpoint_index, speed_from_id, speed_id};
use vvirtio::DmaBuffer;

use crate::controller::{Controller, portsc};
use crate::hid::HidInterface;
use crate::hub::Hub;
use crate::ring::Ring;
use crate::{UsbError, Work, Xhci, queue};

/// A new connection must last this long before the port is reset (USB 2.0
/// section 7.1.7.3).
pub const DEBOUNCE_MS: u64 = 100;
/// Time for a device to recover from a port reset (section 7.1.7.5).
pub const RESET_RECOVERY_MS: u64 = 10;
/// Time for a device to take its new address (section 9.2.6.3).
const SET_ADDRESS_RECOVERY_MS: u64 = 2;
/// Times a device is reset and enumerated before giving up on it.
pub const ATTEMPTS: u32 = 3;
/// Errors in a row after which an interrupt endpoint is no longer polled.
const MAX_ERRORS: u32 = 8;

/// Where a device is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub root_port: u8,
    /// The hub ports from the root port down, 4 bits per hub.
    pub route: u32,
    /// The number of hubs between the root port and the device.
    pub depth: u8,
    /// The hub (its slot) and port the device is on; `None` on a root port.
    pub parent: Option<(u8, u8)>,
    /// For a low- or full-speed device behind a high-speed hub: that hub's
    /// slot and port, whose transaction translator serves the device.
    pub tt: Option<(u8, u8)>,
}

impl Location {
    pub fn root(port: u8) -> Location {
        Location { root_port: port, route: 0, depth: 0, parent: None, tt: None }
    }

    /// Where a device on `port` of the hub at this location is.
    pub fn child(&self, hub: u8, hub_speed: Speed, port: u8, speed: Speed) -> Location {
        Location {
            root_port: self.root_port,
            route: self.route | (port.min(15) as u32) << (4 * self.depth),
            depth: self.depth + 1,
            parent: Some((hub, port)),
            tt: if hub_speed == Speed::High && speed.is_usb1() { Some((hub, port)) } else { self.tt },
        }
    }

    /// The port path, as in "3" or "3.1.2".
    pub fn name(&self) -> String {
        let mut name = format!("{}", self.root_port);
        for tier in 0..self.depth {
            name += &format!(".{}", (self.route >> (4 * tier)) & 0xF);
        }
        name
    }
}

/// What drives a device.
pub enum Function {
    /// Nothing: the device is addressed but not configured.
    None,
    Hub(Hub),
    /// Its keyboard, mouse and tablet interfaces.
    Hid(Vec<HidInterface>),
    /// Lent to the driver VM.
    Lent(crate::lend::Lent),
}

/// A device with a slot.
pub struct Device {
    pub at: Location,
    pub speed: Speed,
    /// The device context, which the controller keeps up to date.
    pub context: DmaBuffer,
    pub ep0: Ring,
    pub ep0_packet_size: u16,
    pub function: Function,
}

impl Device {
    /// The interrupt endpoint with device context index `index`.
    pub fn pipe_mut(&mut self, index: u8) -> Option<&mut Pipe> {
        match &mut self.function {
            Function::Hub(hub) => (hub.pipe.index == index).then_some(&mut hub.pipe),
            Function::Hid(interfaces) => interfaces.iter_mut().map(|h| &mut h.pipe).find(|p| p.index == index),
            Function::None | Function::Lent(_) => None,
        }
    }
}

/// An interrupt IN endpoint, polled with one transfer at a time.
pub struct Pipe {
    pub slot: u8,
    /// The device context index (the doorbell target).
    pub index: u8,
    /// The endpoint's address on the device.
    pub address: u8,
    pub ring: Ring,
    buffer: DmaBuffer,
    /// Bytes per transfer.
    pub len: u32,
    /// Errors in a row.
    errors: u32,
}

impl Pipe {
    /// Starts the next transfer.
    pub fn arm(&mut self, hc: &Controller) {
        self.ring.push(Trb::normal(self.buffer.phys(), self.len, 0, false));
        hc.ring_doorbell(self.slot, self.index);
    }

    /// Handles the end of the pipe's transfer: returns what was received
    /// and starts the next transfer, or, after an error, queues the
    /// endpoint's recovery.
    pub fn complete(&mut self, ev: &Trb, hc: &Controller, work: &mut VecDeque<Work>) -> Option<Vec<u8>> {
        if !self.ring.contains(ev.pointer()) {
            return None;
        }
        match ev.completion_code() {
            completion::SUCCESS | completion::SHORT_PACKET => {
                let len = self.len.saturating_sub(ev.residual()) as usize;
                // SAFETY: the transfer is complete; the next one starts below.
                let data = unsafe { self.buffer.bytes(0, len) }.to_vec();
                self.errors = 0;
                self.arm(hc);
                Some(data)
            }
            // Stopped by the driver (recovery).
            completion::STOPPED | completion::STOPPED_LENGTH_INVALID => None,
            code => {
                self.errors += 1;
                if self.errors < MAX_ERRORS {
                    queue(work, Work::Recover(self.slot, self.index));
                } else if self.errors == MAX_ERRORS {
                    println!(
                        "slot {} endpoint {}: {} too often; no longer polled",
                        self.slot,
                        self.index,
                        completion::name(code)
                    );
                }
                None
            }
        }
    }
}

impl Xhci {
    /// A root hub port changed: a device came or went.
    pub(crate) fn root_port_changed(&mut self, port: u8) {
        if port == 0 || port as usize > self.root.len() {
            return;
        }
        let sc = self.hc.portsc(port);
        self.hc.write_portsc(port, sc & portsc::CHANGES);
        let connected = sc & portsc::CCS != 0;
        if let Some(slot) = self.root[port as usize - 1] {
            if connected && sc & portsc::CSC == 0 {
                return;
            }
            self.detach(slot);
        }
        if connected {
            self.attach_root(port);
        }
    }

    fn attach_root(&mut self, port: u8) {
        let usb3 = self.hc.ports[port as usize - 1].usb3;
        for attempt in 1..=ATTEMPTS {
            self.sleep(DEBOUNCE_MS);
            if self.hc.portsc(port) & portsc::CCS == 0 {
                return;
            }
            let Some(speed) = self.reset_root_port(port, usb3) else {
                println!("port {}: no device after a reset (attempt {})", port, attempt);
                continue;
            };
            match self.enumerate(Location::root(port), speed) {
                Ok(slot) => {
                    self.root[port as usize - 1] = Some(slot);
                    return;
                }
                Err(e) => println!("port {}: {} (attempt {})", port, e, attempt),
            }
        }
        println!("port {}: the device does not respond; ignored", port);
    }

    /// Resets a root hub port (USB 3 links come up by themselves; one that
    /// did not gets a warm reset); returns the speed of its device.
    fn reset_root_port(&mut self, port: u8, usb3: bool) -> Option<Speed> {
        if usb3 {
            if !self.wait_port(port, 1000, |sc| sc & portsc::PED != 0) {
                self.hc.write_portsc(port, portsc::WPR);
                if !self.wait_port(port, 1000, |sc| sc & (portsc::WRC | portsc::PRC) != 0) {
                    return None;
                }
            }
        } else {
            self.hc.write_portsc(port, portsc::PR);
            if !self.wait_port(port, 1000, |sc| sc & portsc::PRC != 0 && sc & portsc::PR == 0) {
                return None;
            }
        }
        let sc = self.hc.portsc(port);
        self.hc.write_portsc(port, sc & (portsc::PRC | portsc::WRC));
        if sc & (portsc::CCS | portsc::PED) != portsc::CCS | portsc::PED {
            return None;
        }
        self.sleep(RESET_RECOVERY_MS);
        speed_from_id(portsc::speed(sc))
    }

    /// Waits up to `ms` milliseconds for a port's PORTSC to satisfy `done`.
    fn wait_port(&mut self, port: u8, ms: u64, done: impl Fn(u32) -> bool) -> bool {
        let end = vrt::time::now_ns() + ms * crate::MS;
        loop {
            if done(self.hc.portsc(port)) {
                return true;
            }
            let now = vrt::time::now_ns();
            if now >= end {
                return false;
            }
            self.pump(end.min(now + crate::MS));
        }
    }

    /// Gives the device at `at` a slot and an address, reads its
    /// descriptors and starts what drives it. Returns its slot.
    pub(crate) fn enumerate(&mut self, at: Location, speed: Speed) -> Result<u8, UsbError> {
        let slot = self.command(Trb::enable_slot())?.slot();
        if slot == 0 || slot > self.hc.max_slots {
            return Err(UsbError::Command(completion::NO_SLOTS_AVAILABLE));
        }
        let (Some(context), Some(ep0)) = (self.hc.alloc_device_context(), self.hc.ring()) else {
            let _ = self.command(Trb::disable_slot(slot));
            return Err(UsbError::NoMemory);
        };
        self.hc.set_device_context(slot, context.phys());
        let ep0_packet_size = speed.initial_control_packet_size();
        self.devices.insert(slot, Device { at, speed, context, ep0, ep0_packet_size, function: Function::None });
        match self.set_up(slot) {
            Ok(()) => Ok(slot),
            Err(e) => {
                self.remove(slot);
                Err(e)
            }
        }
    }

    fn set_up(&mut self, slot: u8) -> Result<(), UsbError> {
        self.address(slot)?;
        let (descriptor, config) = self.identify(slot)?;
        let product = self.product_name(slot, &descriptor);
        let what = if self.to_lend(&descriptor) {
            Some(self.lend(slot, &descriptor, product.as_deref())?)
        } else if descriptor.class == class::HUB {
            self.start_hub(slot, &config)?
        } else if config.default_interfaces().any(|i| i.class == class::HID) {
            self.start_hid(slot, &config)?
        } else {
            None
        };
        let dev = self.devices.get(&slot).ok_or(UsbError::Gone)?;
        let device_class = match descriptor.class {
            class::PER_INTERFACE => config.interfaces.first().map_or(0, |i| i.class),
            c => c,
        };
        println!(
            "port {}: {}({:04x}:{:04x}, {}): {}",
            dev.at.name(),
            product.map(|p| format!("{p} ")).unwrap_or_default(),
            descriptor.vendor,
            descriptor.product,
            dev.speed,
            what.unwrap_or_else(|| format!("{}, not used", class_name(device_class)))
        );
        Ok(())
    }

    /// Address Device: the controller assigns an address and sets up
    /// endpoint 0.
    fn address(&mut self, slot: u8) -> Result<(), UsbError> {
        let dev = self.devices.get(&slot).ok_or(UsbError::Gone)?;
        let (tt_hub_slot, tt_port) = dev.at.tt.unwrap_or((0, 0));
        let context = SlotContext {
            route: dev.at.route,
            speed: speed_id(dev.speed),
            context_entries: 1,
            root_port: dev.at.root_port,
            tt_hub_slot,
            tt_port,
            ..SlotContext::default()
        };
        self.hc.input_reset(0b11);
        self.hc.input_slot(&context);
        self.hc.input_endpoint(1, &EndpointContext::control(dev.ep0_packet_size, dev.ep0.base()));
        self.command(Trb::address_device(self.hc.input_context(), slot, false))?;
        self.sleep(SET_ADDRESS_RECOVERY_MS);
        Ok(())
    }

    /// Reads the device and configuration descriptors.
    fn identify(&mut self, slot: u8) -> Result<(DeviceDescriptor, Configuration), UsbError> {
        let speed = self.devices.get(&slot).ok_or(UsbError::Gone)?.speed;
        if speed == Speed::Full {
            // Endpoint 0's packet size is in the first 8 bytes.
            let head = self.control_in(slot, Setup::get_descriptor(kind::DEVICE, 0, 8))?;
            let size = head.get(7).and_then(|&b| control_packet_size(speed, b)).ok_or(UsbError::BadDescriptor)?;
            if size != speed.initial_control_packet_size() {
                self.set_control_packet_size(slot, size)?;
            }
        }
        let bytes = self.control_in(slot, Setup::get_descriptor(kind::DEVICE, 0, DeviceDescriptor::LEN as u16))?;
        let descriptor = DeviceDescriptor::parse(&bytes).ok_or(UsbError::BadDescriptor)?;
        let header =
            self.control_in(slot, Setup::get_descriptor(kind::CONFIGURATION, 0, Configuration::HEADER_LEN as u16))?;
        let total = Configuration::total_length(&header).ok_or(UsbError::BadDescriptor)?;
        let bytes = self.control_in(slot, Setup::get_descriptor(kind::CONFIGURATION, 0, total))?;
        let config = Configuration::parse(&bytes).ok_or(UsbError::BadDescriptor)?;
        Ok((descriptor, config))
    }

    /// Evaluate Context with endpoint 0's real packet size.
    fn set_control_packet_size(&mut self, slot: u8, size: u16) -> Result<(), UsbError> {
        let dev = self.devices.get_mut(&slot).ok_or(UsbError::Gone)?;
        dev.ep0_packet_size = size;
        self.hc.input_reset(1 << 1);
        self.hc.input_endpoint_with_packet_size(&dev.context, 1, size);
        self.command(Trb::evaluate_context(self.hc.input_context(), slot)).map(|_| ())
    }

    /// The product name, in the device's first language.
    fn product_name(&mut self, slot: u8, d: &DeviceDescriptor) -> Option<String> {
        if d.product_string == 0 {
            return None;
        }
        let languages = self.control_in(slot, Setup::get_string(0, 0, 255)).ok()?;
        let language = first_language(&languages)?;
        let name = self.control_in(slot, Setup::get_string(d.product_string, language, 255)).ok()?;
        parse_string(&name).filter(|n| !n.is_empty())
    }

    /// Configures the device's interrupt IN `endpoints` (Configure
    /// Endpoint, which for a hub also tells the controller about its
    /// ports) and selects configuration `value`. Returns their pipes, not
    /// yet started.
    pub(crate) fn configure(
        &mut self,
        slot: u8,
        value: u8,
        endpoints: &[Endpoint],
        hub: Option<&HubDescriptor>,
    ) -> Result<Vec<Pipe>, UsbError> {
        let mut pipes = Vec::new();
        for e in endpoints {
            let (Some(ring), Some(buffer)) = (self.hc.ring(), self.hc.alloc(4096)) else {
                return Err(UsbError::NoMemory);
            };
            let len = e.packet_size().max(1) as u32;
            pipes.push(Pipe {
                slot,
                index: endpoint_index(e.address),
                address: e.address,
                ring,
                buffer,
                len,
                errors: 0,
            });
        }
        let dev = self.devices.get(&slot).ok_or(UsbError::Gone)?;
        let mut context = self.hc.device_slot(&dev.context);
        context.context_entries = pipes.iter().map(|p| p.index).fold(context.context_entries, u8::max);
        (context.address, context.state) = (0, 0);
        if let Some(h) = hub {
            context.hub = true;
            context.ports = h.ports;
            context.tt_think_time = if dev.speed == Speed::High { h.tt_think_time() } else { 0 };
        }
        self.hc.input_reset(pipes.iter().fold(1, |add, p| add | 1 << p.index));
        self.hc.input_slot(&context);
        for (e, p) in endpoints.iter().zip(&pipes) {
            self.hc.input_endpoint(p.index, &EndpointContext::interrupt_in(dev.speed, e, p.ring.base()));
        }
        self.command(Trb::configure_endpoint(self.hc.input_context(), slot))?;
        self.control(slot, Setup::set_configuration(value), &[])?;
        Ok(pipes)
    }

    /// A device went away: forgets it and the devices behind it.
    pub(crate) fn detach(&mut self, slot: u8) {
        if let Some(at) = self.remove(slot) {
            println!("port {}: removed", at.name());
        }
    }

    /// Releases a device's slot and memory; returns where it was.
    pub(crate) fn remove(&mut self, slot: u8) -> Option<Location> {
        let mut dev = self.devices.remove(&slot)?;
        match &mut dev.function {
            Function::Hub(hub) => {
                for child in hub.children.iter().flatten() {
                    self.detach(*child);
                }
            }
            Function::Hid(interfaces) => {
                // Keys and buttons held now would stay down.
                let mut events = Vec::new();
                for h in interfaces.iter_mut() {
                    h.decoder.release_all(&mut events);
                }
                if !events.is_empty() {
                    self.input.report(events);
                }
            }
            // Its consumer sees its channel close.
            Function::None | Function::Lent(_) => {}
        }
        // The controller stops using the device's memory once the slot is
        // disabled; it is freed after that, with `dev`.
        let _ = self.command(Trb::disable_slot(slot));
        self.hc.set_device_context(slot, 0);
        match dev.at.parent {
            None => {
                if let Some(entry) = self.root.get_mut(dev.at.root_port as usize - 1)
                    && *entry == Some(slot)
                {
                    *entry = None;
                }
            }
            Some((hub, port)) => {
                if let Some(Function::Hub(h)) = self.devices.get_mut(&hub).map(|d| &mut d.function)
                    && let Some(entry) = h.children.get_mut(port as usize - 1)
                    && *entry == Some(slot)
                {
                    *entry = None;
                }
            }
        }
        Some(dev.at)
    }
}
