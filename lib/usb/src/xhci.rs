//! The in-memory structures of an xHCI host controller (eXtensible Host
//! Controller Interface 1.2): transfer request blocks (section 6.4), slot
//! and endpoint contexts (section 6.2), and the values computed for them.
//!
//! Rings, contexts and doorbells live in the driver; this module only
//! encodes and decodes, so it can be tested on the host.

use crate::Speed;
use crate::descriptor::{Endpoint, TransferType};
use crate::request::Setup;

/// TRB types (section 6.4.6).
pub mod trb_type {
    pub const NORMAL: u8 = 1;
    pub const SETUP_STAGE: u8 = 2;
    pub const DATA_STAGE: u8 = 3;
    pub const STATUS_STAGE: u8 = 4;
    pub const LINK: u8 = 6;
    /// A TRB of a transfer ring that moves nothing (one cancelled).
    pub const NOOP: u8 = 8;
    pub const ENABLE_SLOT: u8 = 9;
    pub const DISABLE_SLOT: u8 = 10;
    pub const ADDRESS_DEVICE: u8 = 11;
    pub const CONFIGURE_ENDPOINT: u8 = 12;
    pub const EVALUATE_CONTEXT: u8 = 13;
    pub const RESET_ENDPOINT: u8 = 14;
    pub const STOP_ENDPOINT: u8 = 15;
    pub const SET_TR_DEQUEUE_POINTER: u8 = 16;
    pub const TRANSFER_EVENT: u8 = 32;
    pub const COMMAND_COMPLETION: u8 = 33;
    pub const PORT_STATUS_CHANGE: u8 = 34;
    pub const HOST_CONTROLLER: u8 = 37;
}

/// Completion codes (section 6.4.5).
pub mod completion {
    pub const SUCCESS: u8 = 1;
    pub const DATA_BUFFER_ERROR: u8 = 2;
    pub const BABBLE_DETECTED: u8 = 3;
    pub const USB_TRANSACTION_ERROR: u8 = 4;
    pub const TRB_ERROR: u8 = 5;
    pub const STALL_ERROR: u8 = 6;
    pub const RESOURCE_ERROR: u8 = 7;
    pub const BANDWIDTH_ERROR: u8 = 8;
    pub const NO_SLOTS_AVAILABLE: u8 = 9;
    pub const SHORT_PACKET: u8 = 13;
    pub const PARAMETER_ERROR: u8 = 17;
    pub const CONTEXT_STATE_ERROR: u8 = 19;
    pub const EVENT_RING_FULL: u8 = 21;
    pub const COMMAND_RING_STOPPED: u8 = 24;
    pub const COMMAND_ABORTED: u8 = 25;
    pub const STOPPED: u8 = 26;
    pub const STOPPED_LENGTH_INVALID: u8 = 27;

    /// A short description, for logs.
    pub fn name(code: u8) -> &'static str {
        match code {
            SUCCESS => "success",
            DATA_BUFFER_ERROR => "data buffer error",
            BABBLE_DETECTED => "babble",
            USB_TRANSACTION_ERROR => "transaction error",
            TRB_ERROR => "TRB error",
            STALL_ERROR => "stall",
            RESOURCE_ERROR => "out of resources",
            BANDWIDTH_ERROR => "not enough bandwidth",
            NO_SLOTS_AVAILABLE => "no device slots left",
            SHORT_PACKET => "short packet",
            PARAMETER_ERROR => "parameter error",
            CONTEXT_STATE_ERROR => "context state error",
            EVENT_RING_FULL => "event ring full",
            COMMAND_ABORTED => "command aborted",
            STOPPED | STOPPED_LENGTH_INVALID => "stopped",
            _ => "error",
        }
    }
}

/// Control field bits.
const CYCLE: u32 = 1 << 0;
/// Link TRB: the consumer toggles its cycle state when it follows the link.
const TOGGLE_CYCLE: u32 = 1 << 1;
/// Interrupt on short packet.
const ISP: u32 = 1 << 2;
const CHAIN: u32 = 1 << 4;
/// Interrupt on completion.
const IOC: u32 = 1 << 5;
/// Immediate data: the parameter holds the data (the setup packet).
const IDT: u32 = 1 << 6;
/// Address Device: block the SET_ADDRESS request.
const BSR: u32 = 1 << 9;
/// Data and status stages: the direction is IN.
const DIR_IN: u32 = 1 << 16;
/// Setup stage: transfer type (no data, OUT data, IN data).
const TRT_OUT: u32 = 2 << 16;
const TRT_IN: u32 = 3 << 16;

/// A transfer request block: commands, transfers and events all use it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(16))]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    /// The type, flags and cycle bit.
    pub control: u32,
}

impl Trb {
    fn new(kind: u8, parameter: u64, status: u32, flags: u32) -> Trb {
        Trb { parameter, status, control: (kind as u32) << 10 | flags }
    }

    /// The TRB type ([`trb_type`]).
    pub fn kind(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }

    /// The cycle bit: whose turn the TRB is.
    pub fn cycle(&self) -> bool {
        self.control & CYCLE != 0
    }

    /// The same TRB owned by the side whose cycle state is `cycle`.
    pub fn with_cycle(mut self, cycle: bool) -> Trb {
        self.control = (self.control & !CYCLE) | cycle as u32;
        self
    }

    /// Whether more TRBs of the same transfer descriptor follow.
    pub fn chained(&self) -> bool {
        self.control & CHAIN != 0
    }

    // Transfers (section 6.4.1).

    /// The setup stage of a control transfer.
    pub fn setup_stage(setup: &Setup) -> Trb {
        let trt = match (setup.length, setup.is_in()) {
            (0, _) => 0,
            (_, true) => TRT_IN,
            (_, false) => TRT_OUT,
        };
        Trb::new(trb_type::SETUP_STAGE, u64::from_le_bytes(setup.to_bytes()), 8, IDT | trt)
    }

    /// The first TRB of the data stage of a control transfer.
    pub fn data_stage(buffer: u64, len: u32, td_size: u32, device_to_host: bool, chain: bool) -> Trb {
        let mut flags = if chain { CHAIN } else { 0 };
        if device_to_host {
            flags |= DIR_IN | ISP;
        }
        Trb::new(trb_type::DATA_STAGE, buffer, transfer_status(len, td_size), flags)
    }

    /// The status stage of a control transfer; its direction is opposite to
    /// the data stage's (IN when there is none).
    pub fn status_stage(device_to_host_data: bool) -> Trb {
        Trb::new(trb_type::STATUS_STAGE, 0, 0, IOC | if device_to_host_data { 0 } else { DIR_IN })
    }

    /// A normal TRB: data for bulk and interrupt endpoints, and the
    /// continuation of a control transfer's data stage. The completion of
    /// the last TRB of a transfer and short packets interrupt.
    pub fn normal(buffer: u64, len: u32, td_size: u32, chain: bool) -> Trb {
        let flags = ISP | if chain { CHAIN } else { IOC };
        Trb::new(trb_type::NORMAL, buffer, transfer_status(len, td_size), flags)
    }

    /// A cancelled transfer's TRB: the same place on the ring, moving
    /// nothing, with no event (`chain` as the TRB it replaces had).
    pub fn noop(chain: bool) -> Trb {
        Trb::new(trb_type::NOOP, 0, 0, if chain { CHAIN } else { 0 })
    }

    /// The link at the end of a ring, back to its start; `chain` when a
    /// transfer descriptor continues after it.
    pub fn link(ring_start: u64, chain: bool) -> Trb {
        Trb::new(trb_type::LINK, ring_start, 0, TOGGLE_CYCLE | if chain { CHAIN } else { 0 })
    }

    // Commands (section 6.4.3).

    /// Enable Slot: a slot for a new device (its completion names it).
    pub fn enable_slot() -> Trb {
        Trb::new(trb_type::ENABLE_SLOT, 0, 0, 0)
    }

    /// Disable Slot: the controller forgets a device.
    pub fn disable_slot(slot: u8) -> Trb {
        Trb::new(trb_type::DISABLE_SLOT, 0, 0, (slot as u32) << 24)
    }

    /// Address Device with the input context at `input`; `block` leaves out
    /// the SET_ADDRESS request.
    pub fn address_device(input: u64, slot: u8, block: bool) -> Trb {
        Trb::new(trb_type::ADDRESS_DEVICE, input, 0, (slot as u32) << 24 | if block { BSR } else { 0 })
    }

    /// Configure Endpoint: adds the endpoints of the input context at
    /// `input`, and updates the slot context.
    pub fn configure_endpoint(input: u64, slot: u8) -> Trb {
        Trb::new(trb_type::CONFIGURE_ENDPOINT, input, 0, (slot as u32) << 24)
    }

    /// Evaluate Context: takes new values (endpoint 0's packet size) from
    /// the input context at `input`.
    pub fn evaluate_context(input: u64, slot: u8) -> Trb {
        Trb::new(trb_type::EVALUATE_CONTEXT, input, 0, (slot as u32) << 24)
    }

    /// Reset Endpoint: takes a halted endpoint (after a stall or an error)
    /// back to the stopped state.
    pub fn reset_endpoint(slot: u8, endpoint: u8) -> Trb {
        Trb::new(trb_type::RESET_ENDPOINT, 0, 0, (slot as u32) << 24 | (endpoint as u32) << 16)
    }

    /// Stop Endpoint: stops a running endpoint, abandoning its transfer.
    pub fn stop_endpoint(slot: u8, endpoint: u8) -> Trb {
        Trb::new(trb_type::STOP_ENDPOINT, 0, 0, (slot as u32) << 24 | (endpoint as u32) << 16)
    }

    /// Set TR Dequeue Pointer: where a stopped endpoint continues, and with
    /// which cycle state.
    pub fn set_dequeue(slot: u8, endpoint: u8, dequeue: u64, cycle: bool) -> Trb {
        Trb::new(
            trb_type::SET_TR_DEQUEUE_POINTER,
            dequeue | cycle as u64,
            0,
            (slot as u32) << 24 | (endpoint as u32) << 16,
        )
    }

    // Events (section 6.4.2).

    /// Transfer and command completion events: the [`completion`] code.
    pub fn completion_code(&self) -> u8 {
        (self.status >> 24) as u8
    }

    /// Transfer and command completion events: the slot.
    pub fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }

    /// Transfer events: the endpoint's device context index.
    pub fn endpoint(&self) -> u8 {
        ((self.control >> 16) & 0x1F) as u8
    }

    /// Transfer events: the bytes not transferred.
    pub fn residual(&self) -> u32 {
        self.status & 0xFF_FFFF
    }

    /// Transfer and command completion events: the address of the TRB the
    /// event is about.
    pub fn pointer(&self) -> u64 {
        self.parameter
    }

    /// Port status change events: the root hub port (from 1).
    pub fn port(&self) -> u8 {
        (self.parameter >> 24) as u8
    }
}

/// The status field of a transfer TRB: its length and the TD Size (the
/// packets still to come).
fn transfer_status(len: u32, td_size: u32) -> u32 {
    (len & 0x1_FFFF) | td_size.min(31) << 17
}

/// The TD Size of a TRB of a transfer descriptor (section 4.11.2.4): with
/// `done` bytes in the TRBs before it and `len` bytes in it, out of
/// `total`. Controllers before version 1.0 count the remaining bytes, this
/// TRB included, in KiB.
pub fn td_size(version: u16, done: u32, len: u32, total: u32, max_packet: u16) -> u32 {
    let after = done + len;
    if version < 0x100 {
        return ((total - done) >> 10).min(31);
    }
    if after >= total {
        return 0;
    }
    let packet = max_packet.max(1) as u32;
    (total.div_ceil(packet) - after / packet).min(31)
}

/// Endpoint types of the endpoint context.
pub mod endpoint_type {
    pub const ISOCH_OUT: u8 = 1;
    pub const BULK_OUT: u8 = 2;
    pub const INTERRUPT_OUT: u8 = 3;
    pub const CONTROL: u8 = 4;
    pub const ISOCH_IN: u8 = 5;
    pub const BULK_IN: u8 = 6;
    pub const INTERRUPT_IN: u8 = 7;
}

/// The device context index of an endpoint (1 for endpoint 0): the
/// doorbell target and the position of its context.
pub fn endpoint_index(address: u8) -> u8 {
    let number = address & 0x0F;
    if number == 0 { 1 } else { number * 2 + (address >> 7) }
}

/// The speed ID of a slot context (the default protocol speed IDs).
pub fn speed_id(speed: Speed) -> u8 {
    match speed {
        Speed::Full => 1,
        Speed::Low => 2,
        Speed::High => 3,
        Speed::Super => 4,
        Speed::SuperPlus => 5,
    }
}

/// The speed of a port speed ID from PORTSC (the default IDs).
pub fn speed_from_id(id: u8) -> Option<Speed> {
    Some(match id {
        1 => Speed::Full,
        2 => Speed::Low,
        3 => Speed::High,
        4 => Speed::Super,
        5..=15 => Speed::SuperPlus,
        _ => return None,
    })
}

/// The Interval of a periodic endpoint context: the service period is
/// 2^Interval × 125 µs (section 6.2.3.6).
pub fn interval(speed: Speed, endpoint: &Endpoint) -> u8 {
    let b = endpoint.interval;
    match (speed, endpoint.transfer_type()) {
        // In frames (1 ms): the largest power of two of microframes that
        // is not longer.
        (Speed::Low | Speed::Full, TransferType::Interrupt) => {
            let microframes = b.max(1) as u32 * 8;
            (31 - microframes.leading_zeros()).clamp(3, 10) as u8
        }
        // Full-speed isochronous: 2^(bInterval-1) frames.
        (Speed::Low | Speed::Full, _) => (b.clamp(1, 16) + 2).min(15),
        // 2^(bInterval-1) microframes.
        _ => b.clamp(1, 16) - 1,
    }
}

/// A slot context (section 6.2.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotContext {
    /// The hub ports from the root port to the device, 4 bits per tier.
    pub route: u32,
    pub speed: u8,
    /// A hub whose transaction translators are per port.
    pub multi_tt: bool,
    pub hub: bool,
    /// The last valid endpoint context index.
    pub context_entries: u8,
    pub root_port: u8,
    /// Hubs: number of downstream ports.
    pub ports: u8,
    /// Low- and full-speed devices behind a high-speed hub: its slot and
    /// port.
    pub tt_hub_slot: u8,
    pub tt_port: u8,
    /// Hubs: the transaction translator think time.
    pub tt_think_time: u8,
    /// Written by the controller: the device address and slot state.
    pub address: u8,
    pub state: u8,
}

impl SlotContext {
    /// The context as the controller reads it.
    pub fn to_dwords(&self) -> [u32; 4] {
        [
            (self.route & 0xF_FFFF)
                | (self.speed as u32 & 0xF) << 20
                | (self.multi_tt as u32) << 25
                | (self.hub as u32) << 26
                | (self.context_entries as u32 & 0x1F) << 27,
            (self.root_port as u32) << 16 | (self.ports as u32) << 24,
            self.tt_hub_slot as u32 | (self.tt_port as u32) << 8 | (self.tt_think_time as u32 & 3) << 16,
            self.address as u32 | (self.state as u32) << 27,
        ]
    }

    /// A context as the controller wrote it (in a device context).
    pub fn from_dwords(d: [u32; 4]) -> SlotContext {
        SlotContext {
            route: d[0] & 0xF_FFFF,
            speed: ((d[0] >> 20) & 0xF) as u8,
            multi_tt: d[0] & 1 << 25 != 0,
            hub: d[0] & 1 << 26 != 0,
            context_entries: (d[0] >> 27) as u8,
            root_port: (d[1] >> 16) as u8,
            ports: (d[1] >> 24) as u8,
            tt_hub_slot: d[2] as u8,
            tt_port: (d[2] >> 8) as u8,
            tt_think_time: ((d[2] >> 16) & 3) as u8,
            address: d[3] as u8,
            state: (d[3] >> 27) as u8,
        }
    }
}

/// An endpoint context (section 6.2.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointContext {
    pub endpoint_type: u8,
    pub max_packet_size: u16,
    /// Packets per burst minus one.
    pub max_burst: u8,
    /// Periodic endpoints: see [`interval`].
    pub interval: u8,
    /// Retries after transaction errors (3 is the usual value).
    pub error_count: u8,
    /// The transfer ring, with the consumer cycle state in bit 0.
    pub dequeue: u64,
    pub average_trb_length: u16,
    /// Periodic endpoints: bytes per service interval.
    pub max_esit_payload: u32,
}

impl EndpointContext {
    /// The context of endpoint 0, before the device descriptor gives its
    /// packet size.
    pub fn control(max_packet_size: u16, ring: u64) -> EndpointContext {
        EndpointContext {
            endpoint_type: endpoint_type::CONTROL,
            max_packet_size,
            error_count: 3,
            dequeue: ring | 1,
            average_trb_length: 8,
            ..EndpointContext::default()
        }
    }

    /// The context of an interrupt IN endpoint.
    pub fn interrupt_in(speed: Speed, e: &Endpoint, ring: u64) -> EndpointContext {
        let (max_packet_size, max_burst) = match speed {
            Speed::Super | Speed::SuperPlus => (e.packet_size(), e.max_burst.min(15)),
            Speed::High => (e.packet_size(), e.extra_transactions()),
            _ => (e.packet_size(), 0),
        };
        let max_esit_payload = match speed {
            Speed::Super | Speed::SuperPlus if e.bytes_per_interval > 0 => e.bytes_per_interval as u32,
            _ => max_packet_size as u32 * (max_burst as u32 + 1),
        };
        EndpointContext {
            endpoint_type: endpoint_type::INTERRUPT_IN,
            max_packet_size,
            max_burst,
            interval: interval(speed, e),
            error_count: 3,
            dequeue: ring | 1,
            average_trb_length: max_packet_size.max(1),
            max_esit_payload,
        }
    }

    /// The context of a bulk or interrupt endpoint, of either direction
    /// (`None` for an isochronous one).
    pub fn for_endpoint(speed: Speed, e: &Endpoint, ring: u64) -> Option<EndpointContext> {
        match e.transfer_type() {
            TransferType::Interrupt => {
                let mut c = EndpointContext::interrupt_in(speed, e, ring);
                if !e.is_in() {
                    c.endpoint_type = endpoint_type::INTERRUPT_OUT;
                }
                Some(c)
            }
            TransferType::Bulk => Some(EndpointContext {
                endpoint_type: if e.is_in() { endpoint_type::BULK_IN } else { endpoint_type::BULK_OUT },
                max_packet_size: e.packet_size(),
                max_burst: match speed {
                    Speed::Super | Speed::SuperPlus => e.max_burst.min(15),
                    _ => 0,
                },
                error_count: 3,
                dequeue: ring | 1,
                // What section 4.14.1.1 suggests for bulk endpoints.
                average_trb_length: 3072,
                ..EndpointContext::default()
            }),
            _ => None,
        }
    }

    /// The context as the controller reads it.
    pub fn to_dwords(&self) -> [u32; 5] {
        [
            (self.interval as u32) << 16 | (self.max_esit_payload >> 16) << 24,
            (self.error_count as u32 & 3) << 1
                | (self.endpoint_type as u32 & 7) << 3
                | (self.max_burst as u32) << 8
                | (self.max_packet_size as u32) << 16,
            self.dequeue as u32,
            (self.dequeue >> 32) as u32,
            self.average_trb_length as u32 | (self.max_esit_payload & 0xFFFF) << 16,
        ]
    }
}

/// The input control context (section 6.2.5.1): which contexts a command
/// adds (bit 0 the slot, bit `i` endpoint context `i`).
pub fn input_control(add: u32) -> [u32; 2] {
    [0, add]
}

/// The input control context of a Configure Endpoint that also drops the
/// endpoint contexts of `drop` (bit `i` for context `i`).
pub fn input_control_change(drop: u32, add: u32) -> [u32; 2] {
    [drop, add]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::kind;

    fn endpoint(address: u8, attributes: u8, max_packet_size: u16, interval: u8) -> Endpoint {
        Endpoint { address, attributes, max_packet_size, interval, max_burst: 0, bytes_per_interval: 0 }
    }

    #[test]
    fn bulk_and_interrupt_endpoints_of_both_directions() {
        // Bulk IN at high speed: 512-byte packets, no bursts.
        let c = EndpointContext::for_endpoint(Speed::High, &endpoint(0x81, 2, 512, 0), 0x1000).unwrap();
        assert_eq!((c.endpoint_type, c.max_packet_size, c.max_burst), (endpoint_type::BULK_IN, 512, 0));
        assert_eq!((c.dequeue, c.interval, c.error_count), (0x1001, 0, 3));
        // Bulk OUT at SuperSpeed bursts as its companion says.
        let mut e = endpoint(0x02, 2, 1024, 0);
        e.max_burst = 3;
        let c = EndpointContext::for_endpoint(Speed::Super, &e, 0x2000).unwrap();
        assert_eq!((c.endpoint_type, c.max_burst), (endpoint_type::BULK_OUT, 3));
        // Interrupt OUT: as an IN one, the other way.
        let c = EndpointContext::for_endpoint(Speed::Full, &endpoint(0x03, 3, 8, 10), 0x3000).unwrap();
        assert_eq!((c.endpoint_type, c.interval), (endpoint_type::INTERRUPT_OUT, 6));
        // Isochronous endpoints are not had.
        assert!(EndpointContext::for_endpoint(Speed::High, &endpoint(0x84, 1, 192, 1), 0x4000).is_none());
        // No-Op TRBs keep the chain.
        assert!(Trb::noop(true).chained() && Trb::noop(true).kind() == trb_type::NOOP);
    }

    #[test]
    fn control_transfer_trbs() {
        let setup = Setup::get_descriptor(kind::DEVICE, 0, 18);
        let s = Trb::setup_stage(&setup);
        assert_eq!(s.kind(), trb_type::SETUP_STAGE);
        assert_eq!(s.parameter, 0x0012_0000_0100_0680);
        assert_eq!(s.status, 8);
        assert_eq!(s.control, 2 << 10 | IDT | TRT_IN);
        assert_eq!(Trb::setup_stage(&Setup::set_configuration(1)).control, 2 << 10 | IDT);
        let d = Trb::data_stage(0x1000, 18, 0, true, false);
        assert_eq!((d.kind(), d.status, d.control & (DIR_IN | ISP | CHAIN)), (3, 18, DIR_IN | ISP));
        // The status stage goes the other way, and interrupts.
        assert_eq!(Trb::status_stage(true).control, 4 << 10 | IOC);
        assert_eq!(Trb::status_stage(false).control, 4 << 10 | IOC | DIR_IN);
    }

    #[test]
    fn transfer_and_link_trbs() {
        let n = Trb::normal(0x2000, 8, 0, false);
        assert_eq!(n.control, 1 << 10 | ISP | IOC);
        let c = Trb::normal(0x2000, 4096, 3, true);
        assert_eq!((c.status >> 17, c.chained(), c.control & IOC), (3, true, 0));
        let l = Trb::link(0x5000, false).with_cycle(true);
        assert_eq!((l.kind(), l.parameter, l.control), (trb_type::LINK, 0x5000, 6 << 10 | TOGGLE_CYCLE | CYCLE));
        assert!(!l.with_cycle(false).cycle());
        assert!(Trb::link(0x5000, true).chained());
    }

    #[test]
    fn commands_and_events() {
        assert_eq!(Trb::address_device(0x3000, 5, false).control, 11 << 10 | 5 << 24);
        assert_eq!(Trb::address_device(0x3000, 5, true).control & BSR, BSR);
        assert_eq!(Trb::reset_endpoint(2, 3).control, 14 << 10 | 2 << 24 | 3 << 16);
        assert_eq!(Trb::set_dequeue(2, 1, 0x4040, true).parameter, 0x4041);
        // A transfer event: slot 3, endpoint 3, short by 2 bytes.
        let ev = Trb { parameter: 0x6010, status: 13 << 24 | 2, control: 3 << 24 | 3 << 16 | 32 << 10 | 1 };
        assert_eq!((ev.kind(), ev.slot(), ev.endpoint()), (trb_type::TRANSFER_EVENT, 3, 3));
        assert_eq!((ev.completion_code(), ev.residual(), ev.pointer()), (completion::SHORT_PACKET, 2, 0x6010));
        assert!(ev.cycle());
        let port = Trb { parameter: 7 << 24, status: 1 << 24, control: 34 << 10 };
        assert_eq!((port.kind(), port.port()), (trb_type::PORT_STATUS_CHANGE, 7));
    }

    #[test]
    fn td_sizes() {
        // 1.0 and later: packets after this TRB.
        assert_eq!(td_size(0x100, 0, 4096, 4096, 64), 0);
        assert_eq!(td_size(0x100, 0, 4096, 5000, 512), 10 - 8);
        assert_eq!(td_size(0x110, 0, 512, 100_000, 512), 31);
        assert_eq!(td_size(0x100, 8192, 1808, 10000, 64), 0);
        // 0.96: remaining bytes, this TRB included, in KiB.
        assert_eq!(td_size(0x96, 0, 4096, 10000, 64), 9);
        assert_eq!(td_size(0x96, 8192, 1808, 10000, 64), 1);
    }

    #[test]
    fn endpoint_indexes_and_speeds() {
        assert_eq!(endpoint_index(0x00), 1);
        assert_eq!(endpoint_index(0x80), 1);
        assert_eq!(endpoint_index(0x81), 3);
        assert_eq!(endpoint_index(0x02), 4);
        assert_eq!(endpoint_index(0x8F), 31);
        for speed in [Speed::Low, Speed::Full, Speed::High, Speed::Super, Speed::SuperPlus] {
            assert_eq!(speed_from_id(speed_id(speed)), Some(speed));
        }
        assert_eq!(speed_from_id(0), None);
    }

    #[test]
    fn intervals() {
        // Full and low speed, in ms: 10 ms -> 8 ms (2^6 microframes).
        assert_eq!(interval(Speed::Full, &endpoint(0x81, 3, 8, 10)), 6);
        assert_eq!(interval(Speed::Low, &endpoint(0x81, 3, 8, 1)), 3);
        assert_eq!(interval(Speed::Full, &endpoint(0x81, 3, 8, 255)), 10);
        assert_eq!(interval(Speed::Full, &endpoint(0x81, 3, 8, 0)), 3);
        // High speed and SuperSpeed: 2^(bInterval-1) microframes.
        assert_eq!(interval(Speed::High, &endpoint(0x81, 3, 64, 4)), 3);
        assert_eq!(interval(Speed::Super, &endpoint(0x81, 3, 64, 1)), 0);
        assert_eq!(interval(Speed::High, &endpoint(0x81, 3, 64, 20)), 15);
    }

    #[test]
    fn slot_context() {
        let s = SlotContext {
            route: 0x21,
            speed: speed_id(Speed::Low),
            context_entries: 3,
            root_port: 4,
            tt_hub_slot: 7,
            tt_port: 2,
            ..SlotContext::default()
        };
        let d = s.to_dwords();
        assert_eq!(d, [0x21 | 2 << 20 | 3 << 27, 4 << 16, 7 | 2 << 8, 0]);
        assert_eq!(SlotContext::from_dwords(d), s);
        let hub = SlotContext { hub: true, multi_tt: true, ports: 4, tt_think_time: 1, ..SlotContext::default() };
        let d = hub.to_dwords();
        assert_eq!((d[0] >> 25 & 3, d[1] >> 24, d[2] >> 16), (3, 4, 1));
        // Address and state, written by the controller.
        let out = SlotContext::from_dwords([0, 0, 0, 5 | 2 << 27]);
        assert_eq!((out.address, out.state), (5, 2));
    }

    #[test]
    fn endpoint_contexts() {
        let c = EndpointContext::control(8, 0x7000).to_dwords();
        assert_eq!(c, [0, 3 << 1 | 4 << 3 | 8 << 16, 0x7001, 0, 8]);
        // A full-speed keyboard: 8 bytes every 10 ms.
        let kb = EndpointContext::interrupt_in(Speed::Full, &endpoint(0x81, 3, 8, 10), 0x1_0000_8000).to_dwords();
        assert_eq!(kb, [6 << 16, 3 << 1 | 7 << 3 | 8 << 16, 0x8001, 1, 8 | 8 << 16]);
        // A high-speed endpoint with 2 extra transactions per microframe.
        let hb = EndpointContext::interrupt_in(Speed::High, &endpoint(0x81, 3, 0x1400, 1), 0);
        assert_eq!((hb.max_packet_size, hb.max_burst, hb.max_esit_payload), (1024, 2, 3072));
        let d = hb.to_dwords();
        assert_eq!((d[0] >> 24, d[4] >> 16), (0, 3072));
        // SuperSpeed: the companion gives the burst and the payload.
        let mut ss = endpoint(0x82, 3, 1024, 1);
        ss.max_burst = 1;
        ss.bytes_per_interval = 0x1_000;
        let e = EndpointContext::interrupt_in(Speed::Super, &ss, 0);
        assert_eq!((e.max_burst, e.max_esit_payload, e.interval), (1, 4096, 0));
        assert_eq!(input_control(0b1011), [0, 0b1011]);
    }
}
