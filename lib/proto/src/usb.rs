//! USB devices lent to the driver VM (service `"usbip"`, which the driver
//! VM's Linux provides): devices Veda has no driver for — Bluetooth
//! adapters, network adapters — go to Linux's drivers whole, while the USB
//! controller (and the keyboard on it) stays Veda's.
//!
//! The controller's driver lends each device on a connection of its own
//! to the service ([`NAME`]), which carries one event, a [`Lend`]: the
//! device, and a channel on which its transfers go — the consumer's
//! [`UrbRequest`]s, the driver's [`UrbAnswer`]s, as events. The consumer
//! says first whether it takes the device ([`UrbRequest::Taken`]), so that
//! the driver never waits for it: a consumer that hangs holds up nothing
//! (the controller's driver also serves the keyboard).
//! They are URBs as Linux's USB/IP moves them: control transfers on
//! endpoint 0, bulk and interrupt transfers on the others, each answered
//! when it ends, many at a time, and cancelled on request. The driver does
//! for the consumer what the controller must know: choosing a
//! configuration or an interface's alternate setting configures the
//! endpoints, clearing an endpoint's halt resets it. Isochronous endpoints
//! are not lent yet. The channel closes when the device goes away; when the
//! consumer closes it, the driver resets the device and lends it again.

use alloc::string::String;

use vipc::{Bytes, enumeration, message, union};
use vrt::object::Channel;

enumeration! {
    /// A device's speed (Linux's numbers, as USB/IP has them).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UsbSpeed {
        Low = 1,
        Full = 2,
        High = 3,
        Super = 5,
        SuperPlus = 6,
    }
}

message! {
    /// A device lent.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct UsbDevice {
        pub vendor: u16,
        pub product: u16,
        /// Its class (of its first interface, if the device has none).
        pub class: u8,
        pub speed: UsbSpeed,
        /// Its name, for the log ("Intel Bluetooth").
        pub name: String,
        /// Where it is ("xhci 00:14.0 port 3").
        pub location: String,
    }
}

/// The service USB devices are lent to.
pub const NAME: &str = "usbip";
/// Event ordinal of a [`Lend`] on a connection to the service.
pub const LEND: u32 = 1;

message! {
    /// A device lent, with the channel of its transfers.
    #[derive(Debug)]
    pub struct Lend {
        pub device: UsbDevice,
        pub transfers: Channel,
    }
}

/// Event ordinals on a device's channel: the consumer's requests, the
/// driver's answers.
pub const URB_REQUEST: u32 = 1;
pub const URB_ANSWER: u32 = 2;

/// The most bytes a transfer moves: its data travels in a message.
pub const MAX_TRANSFER: u32 = 60 * 1024;

union! {
    /// The consumer's requests.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum UrbRequest {
        /// A transfer on `endpoint` (its address: number, with 0x80 for IN):
        /// on endpoint 0 a control transfer of `setup`; `length` bytes to
        /// receive (IN), or `data` to send (OUT), followed by a zero-length
        /// packet if `zero_packet` and it fills its last packet.
        1 => Submit { id: u32, endpoint: u8, setup: [u8; 8], length: u32, data: Bytes, zero_packet: bool },
        /// Cancels transfer `id`, unless it has ended.
        2 => Unlink { id: u32 },
        /// The consumer has the device: its transfers come now (the first
        /// request).
        3 => Taken {},
        /// The consumer cannot take the device (it has no room for it):
        /// the first request, and the last.
        4 => Refused {},
    }
}

enumeration! {
    /// How a transfer ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UrbStatus {
        /// Done (an IN transfer may have received less than it asked).
        Ok = 1,
        /// The endpoint stalled: halted, or a control request refused.
        Stall = 2,
        /// The bus failed (no answer, a damaged packet).
        Error = 3,
        /// The device sent more than asked.
        Overflow = 4,
        /// Cancelled.
        Cancelled = 5,
        /// The device is gone.
        Gone = 6,
        /// Not done: an endpoint not configured, or a transfer too long.
        Unsupported = 7,
    }
}

union! {
    /// The driver's answers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum UrbAnswer {
        /// Transfer `id` ended, having moved `actual` bytes: those received
        /// (IN) are `data`.
        1 => Done { id: u32, status: UrbStatus, actual: u32, data: Bytes },
        /// Transfer `id` was cancelled (no `Done` comes for it), or had
        /// ended already (its `Done` came first).
        2 => Unlinked { id: u32, cancelled: bool },
    }
}
