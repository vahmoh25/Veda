//! Standard descriptors (USB 2.0 and USB 3.2 section 9.6) and the HID class
//! descriptor (HID 1.11 section 6.2.1).

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Speed, le16};

/// Descriptor types.
pub mod kind {
    pub const DEVICE: u8 = 1;
    pub const CONFIGURATION: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTERFACE: u8 = 4;
    pub const ENDPOINT: u8 = 5;
    pub const HID: u8 = 0x21;
    pub const HID_REPORT: u8 = 0x22;
    pub const HUB: u8 = 0x29;
    pub const SUPERSPEED_ENDPOINT_COMPANION: u8 = 0x30;
}

/// Device and interface class codes.
pub mod class {
    /// In a device descriptor: each interface names its own class.
    pub const PER_INTERFACE: u8 = 0x00;
    pub const AUDIO: u8 = 0x01;
    pub const COMMUNICATIONS: u8 = 0x02;
    pub const HID: u8 = 0x03;
    pub const IMAGE: u8 = 0x06;
    pub const PRINTER: u8 = 0x07;
    pub const MASS_STORAGE: u8 = 0x08;
    pub const HUB: u8 = 0x09;
    pub const CDC_DATA: u8 = 0x0A;
    pub const SMART_CARD: u8 = 0x0B;
    pub const VIDEO: u8 = 0x0E;
    pub const AUDIO_VIDEO: u8 = 0x10;
    pub const WIRELESS: u8 = 0xE0;
    pub const MISCELLANEOUS: u8 = 0xEF;
    pub const VENDOR: u8 = 0xFF;
}

/// A short description of a class, for logs.
pub fn class_name(class: u8) -> &'static str {
    match class {
        class::AUDIO => "audio device",
        class::COMMUNICATIONS | class::CDC_DATA => "communications device",
        class::HID => "input device",
        class::IMAGE => "camera",
        class::PRINTER => "printer",
        class::MASS_STORAGE => "storage device",
        class::HUB => "hub",
        class::SMART_CARD => "smart card reader",
        class::VIDEO | class::AUDIO_VIDEO => "video device",
        class::WIRELESS => "wireless adapter",
        class::VENDOR => "vendor-specific device",
        _ => "device",
    }
}

/// The device descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// USB release (`bcdUSB`, 0x0200 = 2.0).
    pub usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// `bMaxPacketSize0`: bytes up to USB 2, a power of two from USB 3.
    pub max_packet_size0: u8,
    pub vendor: u16,
    pub product: u16,
    /// `bcdDevice`.
    pub release: u16,
    /// String indexes (0 = none).
    pub manufacturer_string: u8,
    pub product_string: u8,
    pub serial_string: u8,
    pub configurations: u8,
}

impl DeviceDescriptor {
    /// The descriptor's length in bytes.
    pub const LEN: usize = 18;

    /// Parses a device descriptor (`None`: too short or not one).
    pub fn parse(b: &[u8]) -> Option<DeviceDescriptor> {
        if b.len() < Self::LEN || (b[0] as usize) < Self::LEN || b[1] != kind::DEVICE {
            return None;
        }
        Some(DeviceDescriptor {
            usb: le16(b, 2),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet_size0: b[7],
            vendor: le16(b, 8),
            product: le16(b, 10),
            release: le16(b, 12),
            manufacturer_string: b[14],
            product_string: b[15],
            serial_string: b[16],
            configurations: b[17],
        })
    }
}

/// The maximum packet size of endpoint 0 in bytes, from `bMaxPacketSize0`
/// (the eighth byte of the device descriptor); `None` when the value is not
/// valid at that speed.
pub fn control_packet_size(speed: Speed, max_packet_size0: u8) -> Option<u16> {
    match speed {
        Speed::Low => (max_packet_size0 == 8).then_some(8),
        Speed::Full => matches!(max_packet_size0, 8 | 16 | 32 | 64).then_some(max_packet_size0 as u16),
        Speed::High => (max_packet_size0 == 64).then_some(64),
        Speed::Super | Speed::SuperPlus => (max_packet_size0 == 9).then_some(512),
    }
}

/// How an endpoint transfers data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

/// An endpoint descriptor, with its SuperSpeed companion if there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// `bEndpointAddress`: the number in bits 0-3, bit 7 set for IN.
    pub address: u8,
    /// `bmAttributes`: the transfer type in bits 0-1.
    pub attributes: u8,
    /// `wMaxPacketSize`: the packet size in bits 0-10 and, for high-speed
    /// periodic endpoints, extra transactions per microframe in bits 11-12.
    pub max_packet_size: u16,
    /// `bInterval`, whose unit depends on the speed and transfer type.
    pub interval: u8,
    /// From the SuperSpeed endpoint companion: packets per burst minus one.
    pub max_burst: u8,
    /// From the SuperSpeed endpoint companion: bytes per service interval
    /// of a periodic endpoint.
    pub bytes_per_interval: u16,
}

impl Endpoint {
    /// The endpoint number (1 to 15).
    pub fn number(&self) -> u8 {
        self.address & 0x0F
    }

    /// Whether data goes from the device to the host.
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    /// How the endpoint transfers data.
    pub fn transfer_type(&self) -> TransferType {
        match self.attributes & 3 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }

    /// Bytes per packet.
    pub fn packet_size(&self) -> u16 {
        self.max_packet_size & 0x7FF
    }

    /// Additional transactions per microframe of a high-speed periodic
    /// endpoint (0 to 2).
    pub fn extra_transactions(&self) -> u8 {
        (((self.max_packet_size >> 11) & 3) as u8).min(2)
    }
}

/// An interface (one alternate setting of it) with its endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub alternate: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
    /// The length of the report descriptor, from the HID descriptor of a
    /// HID interface.
    pub hid_report_length: Option<u16>,
}

/// A configuration descriptor and everything that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    /// `bConfigurationValue`, for SET_CONFIGURATION.
    pub value: u8,
    pub attributes: u8,
    /// `bMaxPower`, in units of 2 mA (USB 2) or 8 mA (USB 3).
    pub max_power: u8,
    pub interfaces: Vec<Interface>,
}

impl Configuration {
    /// The length of the first part of a configuration descriptor, which
    /// holds the total length.
    pub const HEADER_LEN: usize = 9;

    /// The total length of the configuration from its first 9 bytes.
    pub fn total_length(header: &[u8]) -> Option<u16> {
        (header.len() >= Self::HEADER_LEN && header[1] == kind::CONFIGURATION).then(|| le16(header, 2))
    }

    /// Parses a configuration with its interface, endpoint and class
    /// descriptors. A configuration cut short (the device sent less than it
    /// announced) yields what is complete.
    pub fn parse(b: &[u8]) -> Option<Configuration> {
        if b.len() < Self::HEADER_LEN || (b[0] as usize) < Self::HEADER_LEN || b[1] != kind::CONFIGURATION {
            return None;
        }
        let total = (le16(b, 2) as usize).clamp(Self::HEADER_LEN, b.len());
        let b = &b[..total];
        let mut c = Configuration { value: b[5], attributes: b[7], max_power: b[8], interfaces: Vec::new() };
        let mut at = b[0] as usize;
        while at + 2 <= b.len() {
            let len = b[at] as usize;
            if len < 2 || at + len > b.len() {
                break;
            }
            let d = &b[at..at + len];
            match d[1] {
                kind::INTERFACE if len >= 9 => c.interfaces.push(Interface {
                    number: d[2],
                    alternate: d[3],
                    class: d[5],
                    subclass: d[6],
                    protocol: d[7],
                    endpoints: Vec::new(),
                    hid_report_length: None,
                }),
                kind::ENDPOINT if len >= 7 => {
                    if let Some(i) = c.interfaces.last_mut() {
                        i.endpoints.push(Endpoint {
                            address: d[2],
                            attributes: d[3],
                            max_packet_size: le16(d, 4),
                            interval: d[6],
                            max_burst: 0,
                            bytes_per_interval: 0,
                        });
                    }
                }
                kind::SUPERSPEED_ENDPOINT_COMPANION if len >= 6 => {
                    if let Some(e) = c.interfaces.last_mut().and_then(|i| i.endpoints.last_mut()) {
                        e.max_burst = d[2];
                        e.bytes_per_interval = le16(d, 4);
                    }
                }
                // bNumDescriptors class descriptors follow as (type, length).
                kind::HID if len >= 9 => {
                    if let Some(i) = c.interfaces.last_mut() {
                        i.hid_report_length = d[6..]
                            .as_chunks::<3>()
                            .0
                            .iter()
                            .take(d[5] as usize)
                            .find(|e| e[0] == kind::HID_REPORT)
                            .map(|e| le16(e, 1));
                    }
                }
                _ => {}
            }
            at += len;
        }
        Some(c)
    }

    /// The interfaces in their default alternate setting (the one a
    /// configured device starts with).
    pub fn default_interfaces(&self) -> impl Iterator<Item = &Interface> {
        self.interfaces.iter().filter(|i| i.alternate == 0)
    }
}

/// The first language of string descriptor 0 (the list of languages).
pub fn first_language(b: &[u8]) -> Option<u16> {
    (b.len() >= 4 && b[0] >= 4 && b[1] == kind::STRING).then(|| le16(b, 2))
}

/// The text of a string descriptor (UTF-16), without control characters
/// and surrounding spaces.
pub fn parse_string(b: &[u8]) -> Option<String> {
    if b.len() < 2 || b[1] != kind::STRING {
        return None;
    }
    let end = (b[0] as usize).min(b.len());
    let units = b.get(2..end)?.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c));
    let text: String = char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .filter(|c| !c.is_control())
        .collect();
    Some(String::from(text.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wireless keyboard and mouse receiver: two boot HID interfaces.
    const RECEIVER: [u8; 59] = [
        0x09, 0x02, 0x3B, 0x00, 0x02, 0x01, 0x00, 0xA0, 0x32, // configuration 1, 2 interfaces
        0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, // interface 0: HID, boot keyboard
        0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x3B, 0x00, // HID 1.11, report descriptor 59 bytes
        0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0A, // EP 1 IN, interrupt, 8 bytes, 10 ms
        0x09, 0x04, 0x01, 0x00, 0x01, 0x03, 0x01, 0x02, 0x00, // interface 1: HID, boot mouse
        0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0xB1, 0x00, // report descriptor 177 bytes
        0x07, 0x05, 0x82, 0x03, 0x08, 0x00, 0x02, // EP 2 IN, interrupt, 8 bytes, 2 ms
    ];

    #[test]
    fn device() {
        let b = [
            0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x08, 0x6D, 0x04, 0x2B, 0xC5, 0x05, 0x12, 0x01, 0x02, 0x00, 0x01,
        ];
        let d = DeviceDescriptor::parse(&b).unwrap();
        assert_eq!((d.usb, d.class, d.vendor, d.product, d.release), (0x0200, 0, 0x046D, 0xC52B, 0x1205));
        assert_eq!((d.max_packet_size0, d.manufacturer_string, d.product_string, d.configurations), (8, 1, 2, 1));
        assert_eq!(DeviceDescriptor::parse(&b[..17]), None);
        let mut wrong = b;
        wrong[1] = kind::CONFIGURATION;
        assert_eq!(DeviceDescriptor::parse(&wrong), None);
    }

    #[test]
    fn control_packet_sizes() {
        assert_eq!(control_packet_size(Speed::Full, 64), Some(64));
        assert_eq!(control_packet_size(Speed::Full, 12), None);
        assert_eq!(control_packet_size(Speed::Low, 8), Some(8));
        assert_eq!(control_packet_size(Speed::High, 64), Some(64));
        assert_eq!(control_packet_size(Speed::Super, 9), Some(512));
        assert_eq!(control_packet_size(Speed::Super, 64), None);
    }

    #[test]
    fn composite_configuration() {
        assert_eq!(Configuration::total_length(&RECEIVER[..9]), Some(59));
        let c = Configuration::parse(&RECEIVER).unwrap();
        assert_eq!((c.value, c.attributes, c.max_power), (1, 0xA0, 0x32));
        assert_eq!(c.interfaces.len(), 2);
        let kb = &c.interfaces[0];
        assert_eq!((kb.number, kb.class, kb.subclass, kb.protocol), (0, 3, 1, 1));
        assert_eq!(kb.hid_report_length, Some(59));
        let ep = kb.endpoints[0];
        assert_eq!((ep.number(), ep.is_in(), ep.transfer_type()), (1, true, TransferType::Interrupt));
        assert_eq!((ep.packet_size(), ep.interval), (8, 10));
        let mouse = &c.interfaces[1];
        assert_eq!((mouse.number, mouse.protocol, mouse.hid_report_length), (1, 2, Some(177)));
        assert_eq!(mouse.endpoints[0].address, 0x82);
    }

    #[test]
    fn truncated_and_malformed_configurations() {
        // Cut inside the second interface descriptor, and inside the HID
        // descriptor after it: what is complete is kept.
        assert_eq!(Configuration::parse(&RECEIVER[..40]).unwrap().interfaces.len(), 1);
        let c = Configuration::parse(&RECEIVER[..45]).unwrap();
        assert_eq!(c.interfaces.len(), 2);
        assert!(c.interfaces[1].endpoints.is_empty());
        assert_eq!(c.interfaces[1].hid_report_length, None);
        // A zero-length descriptor stops the walk instead of looping.
        let mut bad = RECEIVER;
        bad[9] = 0;
        assert!(Configuration::parse(&bad).unwrap().interfaces.is_empty());
        assert_eq!(Configuration::parse(&RECEIVER[..8]), None);
    }

    #[test]
    fn superspeed_companion_and_alternates() {
        let b = [
            0x09, 0x02, 0x2C, 0x00, 0x01, 0x01, 0x00, 0x80, 0x70, // configuration
            0x09, 0x04, 0x00, 0x00, 0x02, 0x08, 0x06, 0x50, 0x00, // mass storage, bulk-only
            0x07, 0x05, 0x81, 0x02, 0x00, 0x04, 0x00, // bulk IN 1024
            0x06, 0x30, 0x0F, 0x00, 0x00, 0x00, // companion: burst 16
            0x07, 0x05, 0x02, 0x02, 0x00, 0x04, 0x00, // bulk OUT 1024
            0x06, 0x30, 0x0F, 0x00, 0x00, 0x00, //
            0x09, 0x04, 0x00, 0x01, 0x00, 0x08, 0x06, 0x62, 0x00, // alternate 1 (UAS), cut short
        ];
        let c = Configuration::parse(&b).unwrap();
        // wTotalLength (44) ends the walk before the alternate setting.
        assert_eq!(c.interfaces.len(), 1);
        let e = c.interfaces[0].endpoints[0];
        assert_eq!((e.transfer_type(), e.packet_size(), e.max_burst), (TransferType::Bulk, 1024, 15));
        assert_eq!(c.default_interfaces().count(), 1);
    }

    #[test]
    fn high_bandwidth_endpoint() {
        let e = Endpoint {
            address: 0x81,
            attributes: 3,
            max_packet_size: 0x1400,
            interval: 1,
            max_burst: 0,
            bytes_per_interval: 0,
        };
        assert_eq!((e.packet_size(), e.extra_transactions()), (1024, 2));
    }

    #[test]
    fn strings() {
        assert_eq!(first_language(&[4, 3, 0x09, 0x04]), Some(0x0409));
        assert_eq!(first_language(&[2, 3]), None);
        let s = [16, 3, b'U', 0, b'S', 0, b'B', 0, b' ', 0, b'K', 0, b'B', 0, b' ', 0];
        assert_eq!(parse_string(&s).as_deref(), Some("USB KB"));
        // Odd lengths and short buffers are tolerated.
        assert_eq!(parse_string(&[5, 3, b'A', 0, b'B']).as_deref(), Some("A"));
        assert_eq!(parse_string(&[2, 3]).as_deref(), Some(""));
        assert_eq!(parse_string(&[2, 1]), None);
    }
}
