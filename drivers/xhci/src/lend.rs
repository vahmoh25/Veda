//! Devices lent to the driver VM (`vproto::usb`): the consumer's transfers,
//! carried out on the controller.
//!
//! A lent device has its address and nothing more until the consumer
//! chooses a configuration: then its bulk and interrupt endpoints get rings
//! (isochronous ones are not lent). Control transfers are done one at a
//! time, as the driver does its own, after what the controller must know
//! about them: a configuration or an alternate setting configures the
//! endpoints, clearing a halt resets one. Other transfers are queued on
//! their endpoints' rings, many at a time, and answered as their completion
//! events come. A transfer cancelled while queued becomes No-Op TRBs, which
//! the controller skips. When the consumer lets go, the device is reset (it
//! is found again on its port) and lent again.
//!
//! Nothing here waits for the consumer: a device is offered with an event,
//! the answer comes on the device's channel, which the driver's loop waits
//! on with the controller's interrupts, and an offer without an answer is
//! taken back. A driver VM that hangs freezes no keyboard.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vipc::{Bytes, Decode};
use vproto::usb::{
    LEND, Lend, MAX_TRANSFER, URB_ANSWER, URB_REQUEST, UrbAnswer, UrbRequest, UrbStatus, UsbDevice, UsbSpeed,
};
use vrt::object::Channel;
use vrt::println;
use vusb::Speed;
use vusb::descriptor::{Configuration, DeviceDescriptor, Endpoint, kind};
use vusb::request::Setup;
use vusb::xhci::{EndpointContext, Trb, completion, endpoint_index, td_size};
use vvirtio::DmaBuffer;

use crate::controller::endpoint_state;
use crate::device::{Function, Location};
use crate::ring::Ring;
use crate::{MS, UsbError, Work, Xhci, queue};

/// How long the consumer may take to answer an offer, and how long until a
/// device is offered again when there was no consumer, or it refused.
const ANSWER_MS: u64 = 2000;
const OFFER_AGAIN_MS: u64 = 1000;
const OFFER_AGAIN_REFUSED_MS: u64 = 30_000;
/// A TRB's buffer crosses no 64 KiB boundary (xHCI section 4.11.7.1).
const BOUNDARY: u64 = 64 * 1024;
/// Buffers of finished transfers kept for the next ones.
const BUFFERS_KEPT: usize = 64;

/// Standard requests the driver acts on (USB 2.0 section 9.4): their
/// `bmRequestType` (type standard, the recipient) and `bRequest`.
const TO_DEVICE: u8 = 0x00;
const TO_INTERFACE: u8 = 0x01;
const TO_ENDPOINT: u8 = 0x02;
const CLEAR_FEATURE: u8 = 1;
const SET_ADDRESS: u8 = 5;
const SET_CONFIGURATION: u8 = 9;
const SET_INTERFACE: u8 = 11;
const ENDPOINT_HALT: u16 = 0;

/// A transfer queued on a lent endpoint.
struct InFlight {
    id: u32,
    /// Its TRBs, in order: their addresses and lengths.
    trbs: Vec<(u64, u32)>,
    buffer: Option<DmaBuffer>,
    length: u32,
    is_in: bool,
}

/// A lent endpoint: its ring, and its transfers in the order they were
/// queued (they end in that order).
struct LentEndpoint {
    packet_size: u16,
    ring: Ring,
    queue: VecDeque<InFlight>,
}

/// A device lent to the driver VM.
pub struct Lent {
    /// The device's channel to the consumer, once offered (`None` until
    /// then), and whether the consumer took it: until then, when the offer
    /// is taken back.
    channel: Option<Channel>,
    taken: bool,
    answer_by: u64,
    info: UsbDevice,
    configs: Vec<Configuration>,
    /// The configuration chosen, and the interfaces' alternate settings.
    configuration: Option<u8>,
    alternates: BTreeMap<u8, u8>,
    /// Its endpoints but 0, by device context index.
    endpoints: BTreeMap<u8, LentEndpoint>,
}

fn usb_speed(speed: Speed) -> UsbSpeed {
    match speed {
        Speed::Low => UsbSpeed::Low,
        Speed::Full => UsbSpeed::Full,
        Speed::High => UsbSpeed::High,
        Speed::Super => UsbSpeed::Super,
        Speed::SuperPlus => UsbSpeed::SuperPlus,
    }
}

/// How a transfer that ended with completion `code` went.
fn status_of(code: u8) -> UrbStatus {
    match code {
        completion::SUCCESS | completion::SHORT_PACKET => UrbStatus::Ok,
        completion::STALL_ERROR => UrbStatus::Stall,
        completion::BABBLE_DETECTED => UrbStatus::Overflow,
        _ => UrbStatus::Error,
    }
}

fn status_of_error(e: UsbError) -> UrbStatus {
    match e {
        UsbError::Transfer(code) => status_of(code),
        UsbError::Gone => UrbStatus::Gone,
        UsbError::NoMemory | UsbError::BadDescriptor => UrbStatus::Unsupported,
        UsbError::Command(_) | UsbError::Timeout => UrbStatus::Error,
    }
}

impl Xhci {
    /// Whether the device of `d` is one to lend.
    pub(crate) fn to_lend(&self, d: &DeviceDescriptor) -> bool {
        self.lend.contains(&(d.vendor, d.product))
    }

    /// Lends the device in `slot` (`name`, as its product string says):
    /// reads its configurations, and offers it to the driver VM. Says what
    /// became of it, for the log.
    pub(crate) fn lend(&mut self, slot: u8, d: &DeviceDescriptor, name: Option<&str>) -> Result<String, UsbError> {
        let mut configs = Vec::new();
        for index in 0..d.configurations {
            let header = Setup::get_descriptor(kind::CONFIGURATION, index, Configuration::HEADER_LEN as u16);
            let total = Configuration::total_length(&self.control_in(slot, header)?).ok_or(UsbError::BadDescriptor)?;
            let bytes = self.control_in(slot, Setup::get_descriptor(kind::CONFIGURATION, index, total))?;
            configs.push(Configuration::parse(&bytes).ok_or(UsbError::BadDescriptor)?);
        }
        let location = self.location.clone();
        let dev = self.devices.get_mut(&slot).ok_or(UsbError::Gone)?;
        let class = match d.class {
            0 => configs.first().and_then(|c| c.interfaces.first()).map_or(0, |i| i.class),
            c => c,
        };
        let info = UsbDevice {
            vendor: d.vendor,
            product: d.product,
            class,
            speed: usb_speed(dev.speed),
            name: String::from(name.unwrap_or("")),
            location: format!("{} port {}", location, dev.at.name()),
        };
        dev.function = Function::Lent(Lent {
            channel: None,
            taken: false,
            answer_by: 0,
            info,
            configs,
            configuration: None,
            alternates: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        });
        queue(&mut self.work, Work::Offer(slot));
        Ok(String::from("lent to the driver VM"))
    }

    /// Offers a lent device to the driver VM: its answer comes on the
    /// device's channel. Without a consumer, it is offered again later.
    pub(crate) fn offer(&mut self, slot: u8) {
        let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) else { return };
        if lent.channel.is_some() {
            return;
        }
        let offered = (|| {
            let connection = vproto::connect(vproto::usb::NAME).ok()?;
            let (ours, theirs) = Channel::create().ok()?;
            vipc::send_event(&connection, LEND, Lend { device: lent.info.clone(), transfers: theirs }).ok()?;
            Some(ours)
        })();
        match offered {
            Some(channel) => {
                lent.channel = Some(channel);
                lent.taken = false;
                lent.answer_by = vrt::time::now_ns() + ANSWER_MS * MS;
            }
            None => self.offer_again.push((slot, vrt::time::now_ns() + OFFER_AGAIN_MS * MS)),
        }
    }

    /// Takes back the offer of the device in `slot` (no answer, or a
    /// refusal), to offer it again after `ms`.
    fn take_back(&mut self, slot: u8, ms: u64) {
        if let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) {
            lent.channel = None;
        }
        self.offer_again.push((slot, vrt::time::now_ns() + ms * MS));
    }

    /// Takes back the offers whose answers are late; says when the next
    /// one would be.
    pub(crate) fn late_offers(&mut self) -> u64 {
        let now = vrt::time::now_ns();
        let mut late = Vec::new();
        let mut next = vabi::DEADLINE_INFINITE;
        for (&slot, d) in &self.devices {
            if let Function::Lent(Lent { channel: Some(_), taken: false, answer_by, .. }) = &d.function {
                if *answer_by <= now {
                    late.push(slot);
                } else {
                    next = next.min(*answer_by);
                }
            }
        }
        for slot in late {
            println!("{}: the driver VM does not answer for it", self.lent_name(slot));
            self.take_back(slot, OFFER_AGAIN_MS);
        }
        next
    }

    fn lent_name(&self, slot: u8) -> String {
        match self.devices.get(&slot).map(|d| &d.function) {
            Some(Function::Lent(l)) => format!("{} ({:04x}:{:04x})", l.info.location, l.info.vendor, l.info.product),
            _ => format!("slot {slot}"),
        }
    }

    /// Carries out what the consumers of the lent devices ask.
    pub(crate) fn serve_lent(&mut self) {
        let slots: Vec<u8> = self
            .devices
            .iter()
            .filter(|(_, d)| matches!(&d.function, Function::Lent(l) if l.channel.is_some()))
            .map(|(&s, _)| s)
            .collect();
        for slot in slots {
            while let Some(Function::Lent(Lent { channel: Some(channel), .. })) =
                self.devices.get(&slot).map(|d| &d.function)
            {
                let request = match channel.read() {
                    Ok(mut msg) => match vipc::open(&mut msg) {
                        Ok((h, mut d)) if h.ordinal == URB_REQUEST => UrbRequest::decode(&mut d).ok(),
                        _ => None,
                    },
                    Err(vabi::Error::ShouldWait) => break,
                    Err(_) => {
                        let taken =
                            matches!(self.devices.get(&slot).map(|d| &d.function), Some(Function::Lent(l)) if l.taken);
                        if taken {
                            self.reclaim(slot);
                        } else {
                            // Gone before it answered (a driver VM that ended).
                            self.take_back(slot, OFFER_AGAIN_MS);
                        }
                        break;
                    }
                };
                if let Some(r) = request {
                    self.request(slot, r);
                }
            }
        }
    }

    fn answer(&self, slot: u8, answer: UrbAnswer) {
        if let Some(Function::Lent(Lent { channel: Some(channel), .. })) = self.devices.get(&slot).map(|d| &d.function)
        {
            let _ = vipc::send_event(channel, URB_ANSWER, answer);
        }
    }

    fn request(&mut self, slot: u8, r: UrbRequest) {
        match r {
            UrbRequest::Submit { id, endpoint, setup, length, data, zero_packet } => {
                if endpoint & 0x0F == 0 {
                    let (status, data) = self.lent_control(slot, Setup::from_bytes(setup), &data.0);
                    let actual = if setup[0] & 0x80 != 0 { data.len() as u32 } else { length };
                    self.answer(slot, UrbAnswer::Done { id, status, actual, data: Bytes(data) });
                } else if let Err(status) = self.queue_transfer(slot, id, endpoint, length, &data.0, zero_packet) {
                    self.answer(slot, UrbAnswer::Done { id, status, actual: 0, data: Bytes(Vec::new()) });
                }
            }
            UrbRequest::Unlink { id } => self.unlink(slot, id),
            UrbRequest::Taken {} => {
                println!("{} taken by the driver VM", self.lent_name(slot));
                if let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) {
                    lent.taken = true;
                }
            }
            UrbRequest::Refused {} => {
                println!("{}: the driver VM cannot take it", self.lent_name(slot));
                self.take_back(slot, OFFER_AGAIN_REFUSED_MS);
            }
        }
    }

    // ---- Control transfers ----------------------------------------------

    /// A control transfer of the consumer's, after what the controller
    /// must know of it.
    fn lent_control(&mut self, slot: u8, setup: Setup, out: &[u8]) -> (UrbStatus, Vec<u8>) {
        let prepared = match (setup.request_type, setup.request) {
            // The device's address is the controller's; the consumer's is
            // its own.
            (TO_DEVICE, SET_ADDRESS) => return (UrbStatus::Ok, Vec::new()),
            (TO_DEVICE, SET_CONFIGURATION) => self.lent_configure(slot, setup.value as u8),
            (TO_INTERFACE, SET_INTERFACE) => self.lent_set_interface(slot, setup.index as u8, setup.value as u8),
            (TO_ENDPOINT, CLEAR_FEATURE) if setup.value == ENDPOINT_HALT => {
                self.lent_reset_endpoint(slot, setup.index as u8);
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = prepared {
            return (status_of_error(e), Vec::new());
        }
        let result = self.control(slot, setup, out);
        // A halt cleared on the device: the endpoint's transfers go on.
        if (setup.request_type, setup.request) == (TO_ENDPOINT, CLEAR_FEATURE) {
            self.hc.ring_doorbell(slot, endpoint_index(setup.index as u8));
        }
        match result {
            Ok(data) => (UrbStatus::Ok, data),
            Err(e) => (status_of_error(e), Vec::new()),
        }
    }

    /// Gives the endpoints of configuration `value` (its interfaces' first
    /// alternate settings) rings; 0 takes them all away.
    fn lent_configure(&mut self, slot: u8, value: u8) -> Result<(), UsbError> {
        let Some(Function::Lent(lent)) = self.devices.get(&slot).map(|d| &d.function) else {
            return Err(UsbError::Gone);
        };
        let endpoints: Vec<Endpoint> = match lent.configs.iter().find(|c| c.value == value) {
            Some(c) => c.default_interfaces().flat_map(|i| i.endpoints.iter().copied()).collect(),
            None if value == 0 => Vec::new(),
            None => return Err(UsbError::Transfer(completion::STALL_ERROR)),
        };
        let all: Vec<u8> = lent.endpoints.keys().copied().collect();
        self.change_endpoints(slot, &all, &endpoints)?;
        if let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) {
            lent.configuration = (value != 0).then_some(value);
            lent.alternates.clear();
        }
        Ok(())
    }

    /// Changes interface `interface`'s endpoints to those of its alternate
    /// setting `alternate`.
    fn lent_set_interface(&mut self, slot: u8, interface: u8, alternate: u8) -> Result<(), UsbError> {
        let Some(Function::Lent(lent)) = self.devices.get(&slot).map(|d| &d.function) else {
            return Err(UsbError::Gone);
        };
        let Some(config) = lent.configs.iter().find(|c| Some(c.value) == lent.configuration) else {
            return Err(UsbError::Transfer(completion::STALL_ERROR));
        };
        let current = lent.alternates.get(&interface).copied().unwrap_or(0);
        let endpoints_of = |alt: u8| {
            config.interfaces.iter().find(|i| i.number == interface && i.alternate == alt).map(|i| i.endpoints.clone())
        };
        let (Some(old), Some(new)) = (endpoints_of(current), endpoints_of(alternate)) else {
            return Err(UsbError::Transfer(completion::STALL_ERROR));
        };
        let drop: Vec<u8> = old.iter().map(|e| endpoint_index(e.address)).collect();
        self.change_endpoints(slot, &drop, &new)?;
        if let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) {
            lent.alternates.insert(interface, alternate);
        }
        Ok(())
    }

    /// Configure Endpoint: the endpoints of device context indexes `drop`
    /// go, `add` come (bulk and interrupt ones), with new rings. Transfers
    /// still queued on those that go end, cancelled.
    fn change_endpoints(&mut self, slot: u8, drop: &[u8], add: &[Endpoint]) -> Result<(), UsbError> {
        // The new endpoints, each with its context and ring.
        let mut new = Vec::new();
        for e in add.iter().filter(|e| endpoint_index(e.address) > 1) {
            let ring = self.hc.ring().ok_or(UsbError::NoMemory)?;
            let dev = self.devices.get(&slot).ok_or(UsbError::Gone)?;
            match EndpointContext::for_endpoint(dev.speed, e, ring.base()) {
                Some(c) => new.push((endpoint_index(e.address), c, e.packet_size(), ring)),
                None => {
                    println!("port {}: endpoint {:#04x} is isochronous, which is not lent", dev.at.name(), e.address)
                }
            }
        }
        let dev = self.devices.get(&slot).ok_or(UsbError::Gone)?;
        let Some(Function::Lent(lent)) = Some(&dev.function) else { return Err(UsbError::Gone) };
        // Only contexts that are there go.
        let drop: Vec<u8> = drop.iter().copied().filter(|i| lent.endpoints.contains_key(i)).collect();
        let mut kept: Vec<u8> = lent.endpoints.keys().copied().filter(|i| !drop.contains(i)).collect();
        kept.extend(new.iter().map(|n| n.0));
        let drop_mask = drop.iter().fold(0u32, |m, &i| m | 1 << i);
        let add_mask = new.iter().fold(1u32, |m, n| m | 1 << n.0);
        let mut context = self.hc.device_slot(&dev.context);
        context.context_entries = kept.iter().copied().fold(1, u8::max);
        (context.address, context.state) = (0, 0);
        self.hc.input_change(drop_mask, add_mask);
        self.hc.input_slot(&context);
        for (index, c, ..) in &new {
            self.hc.input_endpoint(*index, c);
        }
        self.command(Trb::configure_endpoint(self.hc.input_context(), slot))?;
        let mut cancelled = Vec::new();
        if let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) {
            for index in &drop {
                if let Some(ep) = lent.endpoints.remove(index) {
                    cancelled.extend(ep.queue.into_iter().map(|t| t.id));
                }
            }
            for (index, _, packet_size, ring) in new {
                lent.endpoints.insert(index, LentEndpoint { packet_size, ring, queue: VecDeque::new() });
            }
        }
        for id in cancelled {
            self.answer(slot, UrbAnswer::Done { id, status: UrbStatus::Cancelled, actual: 0, data: Bytes(Vec::new()) });
        }
        Ok(())
    }

    /// Makes a halted endpoint (`address`) run again on the controller's
    /// side, past the transfer that stalled; its device's side is the
    /// consumer's request.
    fn lent_reset_endpoint(&mut self, slot: u8, address: u8) {
        let index = endpoint_index(address);
        let Some(dev) = self.devices.get(&slot) else { return };
        if index < 2 || self.hc.endpoint_state(&dev.context, index) != endpoint_state::HALTED {
            return;
        }
        if self.command(Trb::reset_endpoint(slot, index)).is_err() {
            return;
        }
        self.continue_after(slot, index);
    }

    /// Points a stopped endpoint at its first transfer still queued (or
    /// past everything on its ring).
    fn continue_after(&mut self, slot: u8, index: u8) {
        let Some(Function::Lent(lent)) = self.devices.get(&slot).map(|d| &d.function) else { return };
        let Some(ep) = lent.endpoints.get(&index) else { return };
        let (at, cycle) = match ep.queue.front() {
            Some(t) => (t.trbs[0].0, ep.ring.cycle_at(t.trbs[0].0)),
            None => ep.ring.next(),
        };
        let _ = self.command(Trb::set_dequeue(slot, index, at, cycle));
    }

    // ---- Bulk and interrupt transfers -----------------------------------

    /// Queues a transfer on its endpoint's ring.
    fn queue_transfer(
        &mut self,
        slot: u8,
        id: u32,
        endpoint: u8,
        length: u32,
        data: &[u8],
        zero_packet: bool,
    ) -> Result<(), UrbStatus> {
        let is_in = endpoint & 0x80 != 0;
        let len = if is_in { length } else { data.len() as u32 };
        if len > MAX_TRANSFER {
            return Err(UrbStatus::Unsupported);
        }
        let buffer = match len {
            0 => None,
            n => Some(self.buffer(n as usize).ok_or(UrbStatus::Error)?),
        };
        if let (Some(b), false) = (&buffer, is_in) {
            b.write(0, data);
        }
        let version = self.hc.version;
        let index = endpoint_index(endpoint);
        let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) else {
            return Err(UrbStatus::Gone);
        };
        let Some(ep) = lent.endpoints.get_mut(&index) else { return Err(UrbStatus::Unsupported) };
        let packet = ep.packet_size.max(1);
        // A zero-length packet ends an OUT transfer that fills its last
        // packet, if asked: a TD of its own.
        let zero = zero_packet && !is_in && len > 0 && len.is_multiple_of(packet as u32);
        let base = buffer.as_ref().map_or(0, |b| b.phys());
        let mut trbs = Vec::new();
        let mut done = 0u32;
        loop {
            let at = base + done as u64;
            let piece = (len - done).min((BOUNDARY - at % BOUNDARY) as u32);
            let last = done + piece >= len;
            let trb = Trb::normal(at, piece, td_size(version, done, piece, len, packet), !last);
            trbs.push((ep.ring.push(trb), piece));
            done += piece;
            if last {
                break;
            }
        }
        if zero {
            trbs.push((ep.ring.push(Trb::normal(0, 0, 0, false)), 0));
        }
        ep.queue.push_back(InFlight { id, trbs, buffer, length: len, is_in });
        self.hc.ring_doorbell(slot, index);
        Ok(())
    }

    /// A transfer event of a lent device's endpoint.
    pub(crate) fn lent_event(&mut self, slot: u8, ev: Trb) {
        let (index, code) = (ev.endpoint(), ev.completion_code());
        // Its stopping (to cancel a transfer) ends nothing.
        if matches!(code, completion::STOPPED | completion::STOPPED_LENGTH_INVALID) {
            return;
        }
        let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) else { return };
        let Some(ep) = lent.endpoints.get_mut(&index) else { return };
        let Some(pos) = ep.queue.iter().position(|t| t.trbs.iter().any(|&(a, _)| a == ev.pointer())) else { return };
        let trb = ep.queue[pos].trbs.iter().position(|&(a, _)| a == ev.pointer()).unwrap_or(0);
        let last = trb + 1 == ep.queue[pos].trbs.len();
        // The middle of a transfer interrupts only when it ends short (the
        // rest of the transfer is skipped) or fails.
        if code == completion::SUCCESS && !last {
            return;
        }
        let Some(t) = ep.queue.remove(pos) else { return };
        let before: u32 = t.trbs[..trb].iter().map(|&(_, l)| l).sum();
        let actual = (before + t.trbs[trb].1.saturating_sub(ev.residual())).min(t.length);
        let data = match (&t.buffer, t.is_in) {
            // SAFETY: the transfer has ended; the controller no longer
            // writes its buffer.
            (Some(b), true) => unsafe { b.bytes(0, actual as usize) }.to_vec(),
            _ => Vec::new(),
        };
        let status = status_of(code);
        self.answer(slot, UrbAnswer::Done { id: t.id, status, actual, data: Bytes(data) });
        if let Some(b) = t.buffer {
            self.give_back(b);
        }
        // A failure other than a stall (which the consumer clears) halted the
        // endpoint: it goes on with the next transfer.
        if !matches!(code, completion::SUCCESS | completion::SHORT_PACKET | completion::STALL_ERROR)
            && self.command(Trb::reset_endpoint(slot, index)).is_ok()
        {
            self.continue_after(slot, index);
            self.hc.ring_doorbell(slot, index);
        }
    }

    /// A buffer for a transfer of `len` bytes: one a transfer is done with,
    /// if there is one big enough.
    fn buffer(&mut self, len: usize) -> Option<DmaBuffer> {
        match self.buffers.iter().position(|b| b.len() >= len) {
            Some(i) => Some(self.buffers.swap_remove(i)),
            None => self.hc.alloc(len.next_multiple_of(4096)),
        }
    }

    /// Keeps a transfer's buffer for the next ones (a few).
    fn give_back(&mut self, buffer: DmaBuffer) {
        if self.buffers.len() < BUFFERS_KEPT {
            self.buffers.push(buffer);
        }
    }

    /// Cancels a transfer, unless it ended.
    fn unlink(&mut self, slot: u8, id: u32) {
        let find = |x: &Xhci| match x.devices.get(&slot).map(|d| &d.function) {
            Some(Function::Lent(lent)) => {
                lent.endpoints.iter().find(|(_, ep)| ep.queue.iter().any(|t| t.id == id)).map(|(&i, _)| i)
            }
            _ => None,
        };
        let Some(index) = find(self) else {
            self.answer(slot, UrbAnswer::Unlinked { id, cancelled: false });
            return;
        };
        // What the endpoint finishes until it stops comes as events first.
        let _ = self.command(Trb::stop_endpoint(slot, index));
        if find(self).is_none() {
            self.answer(slot, UrbAnswer::Unlinked { id, cancelled: false });
            self.hc.ring_doorbell(slot, index);
            return;
        }
        let Some(dev) = self.devices.get(&slot) else { return };
        let stopped_at = self.hc.endpoint_dequeue(&dev.context, index);
        let Some(Function::Lent(lent)) = self.devices.get_mut(&slot).map(|d| &mut d.function) else { return };
        let Some(ep) = lent.endpoints.get_mut(&index) else { return };
        let Some(pos) = ep.queue.iter().position(|t| t.id == id) else { return };
        let Some(t) = ep.queue.remove(pos) else { return };
        for &(at, _) in &t.trbs {
            ep.ring.cancel(at);
        }
        // Stopped in the middle of it: the controller continues past it.
        if t.trbs.iter().any(|&(at, _)| at == stopped_at) {
            self.continue_after(slot, index);
        }
        self.answer(slot, UrbAnswer::Unlinked { id, cancelled: true });
        self.hc.ring_doorbell(slot, index);
    }

    /// The consumer let go of the device: it is found again on its port
    /// (reset) and lent again.
    fn reclaim(&mut self, slot: u8) {
        let Some(at) = self.devices.get(&slot).map(|d| d.at.clone()) else { return };
        println!("port {}: the driver VM let go of it; it is reset and lent again", at.name());
        self.remove(slot);
        queue(&mut self.work, Work::Reattach(at));
    }

    /// Finds the device at `at` again (it is reset and enumerated).
    pub(crate) fn reattach(&mut self, at: Location) {
        match at.parent {
            None => self.root_port_changed(at.root_port),
            Some((hub, port)) => self.hub_port_changed(hub, port),
        }
    }

    /// The lent devices' channels, to wait on.
    pub(crate) fn lent_channels(&self) -> Vec<vabi::WaitItem> {
        self.devices
            .values()
            .filter_map(|d| match &d.function {
                Function::Lent(l) => l.channel.as_ref().map(|c| vabi::WaitItem {
                    handle: c.raw(),
                    signals: signals::READABLE | signals::PEER_CLOSED,
                    ..Default::default()
                }),
                _ => None,
            })
            .collect()
    }
}
