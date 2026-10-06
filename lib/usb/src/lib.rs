//! `vusb` — USB for Veda's host controller drivers.
//!
//! The parts of USB that do not touch hardware, so that they run inside
//! Veda and in host tests alike:
//!
//! * [`descriptor`]: device, configuration, interface, endpoint, string
//!   and HID descriptors (USB 2.0 and 3.2 chapter 9, HID 1.11);
//! * [`request`]: control requests — standard, HID class and hub class;
//! * [`hub`]: the USB 2.0 hub class (hub descriptor, port status);
//! * [`hid`]: HID report descriptors, and the reports of keyboards, mice
//!   and tablets turned into Veda input events;
//! * [`xhci`]: the in-memory structures of an xHCI host controller (TRBs,
//!   slot and endpoint contexts) and the values computed for them.
//!
//! Everything a device sends is untrusted: parsers return `None` or an
//! error, or skip what they cannot read, instead of panicking.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod descriptor;
pub mod hid;
pub mod hub;
pub mod request;
pub mod xhci;

/// The speed a device runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Speed {
    /// 1.5 Mb/s (USB 1.x).
    Low,
    /// 12 Mb/s (USB 1.x).
    Full,
    /// 480 Mb/s (USB 2.0).
    High,
    /// 5 Gb/s (USB 3.x).
    Super,
    /// 10 Gb/s and more (USB 3.1 and later).
    SuperPlus,
}

impl Speed {
    /// The speed's name, for logs.
    pub fn name(self) -> &'static str {
        match self {
            Speed::Low => "low speed",
            Speed::Full => "full speed",
            Speed::High => "high speed",
            Speed::Super => "SuperSpeed",
            Speed::SuperPlus => "SuperSpeedPlus",
        }
    }

    /// The maximum packet size of endpoint 0 to use until the device
    /// descriptor gives the real one. A full-speed device may use 8 to 64
    /// bytes, but its first 8 bytes always fit into one packet.
    pub fn initial_control_packet_size(self) -> u16 {
        match self {
            Speed::Low | Speed::Full => 8,
            Speed::High => 64,
            Speed::Super | Speed::SuperPlus => 512,
        }
    }

    /// A USB 1.x speed: behind a high-speed hub, such a device's traffic
    /// goes through the hub's transaction translator.
    pub fn is_usb1(self) -> bool {
        matches!(self, Speed::Low | Speed::Full)
    }
}

impl core::fmt::Display for Speed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
