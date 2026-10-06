//! Control requests: standard (USB 2.0 section 9.4), HID class (HID 1.11
//! section 7.2) and hub class (USB 2.0 section 11.24.2).

use crate::descriptor::kind;

/// `bmRequestType`: the data stage goes from the device to the host.
const DEVICE_TO_HOST: u8 = 0x80;
/// `bmRequestType`: a class request.
const CLASS: u8 = 0x20;
/// `bmRequestType` recipients.
const TO_INTERFACE: u8 = 0x01;
const TO_ENDPOINT: u8 = 0x02;
const TO_OTHER: u8 = 0x03;
/// The ENDPOINT_HALT feature.
const ENDPOINT_HALT: u16 = 0;

/// `bRequest` codes.
mod code {
    pub const GET_STATUS: u8 = 0;
    pub const CLEAR_FEATURE: u8 = 1;
    pub const SET_FEATURE: u8 = 3;
    pub const GET_DESCRIPTOR: u8 = 6;
    pub const SET_CONFIGURATION: u8 = 9;
    pub const HID_SET_REPORT: u8 = 0x09;
    pub const HID_SET_IDLE: u8 = 0x0A;
    pub const HID_SET_PROTOCOL: u8 = 0x0B;
}

/// HID report types (the high byte of `wValue` in report requests).
const OUTPUT_REPORT: u16 = 2;

/// The setup packet that starts a control transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    /// Bytes in the data stage.
    pub length: u16,
}

impl Setup {
    /// The packet as sent on the bus.
    pub fn to_bytes(&self) -> [u8; 8] {
        let [v0, v1] = self.value.to_le_bytes();
        let [i0, i1] = self.index.to_le_bytes();
        let [l0, l1] = self.length.to_le_bytes();
        [self.request_type, self.request, v0, v1, i0, i1, l0, l1]
    }

    /// Whether the data stage goes from the device to the host.
    pub fn is_in(&self) -> bool {
        self.request_type & DEVICE_TO_HOST != 0
    }

    /// GET_DESCRIPTOR for a descriptor of the device.
    pub fn get_descriptor(kind: u8, index: u8, length: u16) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST,
            request: code::GET_DESCRIPTOR,
            value: (kind as u16) << 8 | index as u16,
            index: 0,
            length,
        }
    }

    /// GET_DESCRIPTOR for string `index` in `language` (index 0: the list
    /// of languages).
    pub fn get_string(index: u8, language: u16, length: u16) -> Setup {
        Setup { index: language, ..Setup::get_descriptor(kind::STRING, index, length) }
    }

    /// SET_CONFIGURATION: selects configuration `value` (its
    /// `bConfigurationValue`), which makes its interfaces work.
    pub fn set_configuration(value: u8) -> Setup {
        Setup { request_type: 0, request: code::SET_CONFIGURATION, value: value as u16, index: 0, length: 0 }
    }

    /// CLEAR_FEATURE(ENDPOINT_HALT): restarts a halted (stalled) endpoint
    /// of the device, with its data toggle back at DATA0.
    pub fn clear_halt(endpoint_address: u8) -> Setup {
        Setup {
            request_type: TO_ENDPOINT,
            request: code::CLEAR_FEATURE,
            value: ENDPOINT_HALT,
            index: endpoint_address as u16,
            length: 0,
        }
    }

    /// GET_DESCRIPTOR for a class descriptor of an interface, such as the
    /// HID report descriptor.
    pub fn get_interface_descriptor(kind: u8, interface: u8, length: u16) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | TO_INTERFACE,
            request: code::GET_DESCRIPTOR,
            value: (kind as u16) << 8,
            index: interface as u16,
            length,
        }
    }

    /// SET_IDLE with duration 0: send reports only when something changes.
    pub fn hid_set_idle(interface: u8) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: code::HID_SET_IDLE,
            value: 0,
            index: interface as u16,
            length: 0,
        }
    }

    /// SET_PROTOCOL: the report protocol (described by the report
    /// descriptor) or the fixed boot protocol.
    pub fn hid_set_protocol(interface: u8, report_protocol: bool) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: code::HID_SET_PROTOCOL,
            value: report_protocol as u16,
            index: interface as u16,
            length: 0,
        }
    }

    /// SET_REPORT for an output report (keyboard lights) of `length` bytes,
    /// including the report ID byte when the device numbers its reports.
    pub fn hid_set_output_report(interface: u8, report_id: u8, length: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_INTERFACE,
            request: code::HID_SET_REPORT,
            value: OUTPUT_REPORT << 8 | report_id as u16,
            index: interface as u16,
            length,
        }
    }

    /// GET_DESCRIPTOR for the hub descriptor.
    pub fn hub_descriptor(length: u16) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | CLASS,
            request: code::GET_DESCRIPTOR,
            value: (kind::HUB as u16) << 8,
            index: 0,
            length,
        }
    }

    /// GET_STATUS of the hub itself: 4 bytes of status and changes.
    pub fn hub_status() -> Setup {
        Setup { request_type: DEVICE_TO_HOST | CLASS, request: code::GET_STATUS, value: 0, index: 0, length: 4 }
    }

    /// CLEAR_FEATURE on the hub itself (acknowledges a change).
    pub fn hub_clear_feature(feature: u16) -> Setup {
        Setup { request_type: CLASS, request: code::CLEAR_FEATURE, value: feature, index: 0, length: 0 }
    }

    /// GET_STATUS of a hub port: 4 bytes of status and changes.
    pub fn hub_port_status(port: u8) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | CLASS | TO_OTHER,
            request: code::GET_STATUS,
            value: 0,
            index: port as u16,
            length: 4,
        }
    }

    /// SET_FEATURE on a hub port (power, reset).
    pub fn hub_set_port_feature(port: u8, feature: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_OTHER,
            request: code::SET_FEATURE,
            value: feature,
            index: port as u16,
            length: 0,
        }
    }

    /// CLEAR_FEATURE on a hub port (acknowledges a change).
    pub fn hub_clear_port_feature(port: u8, feature: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_OTHER,
            request: code::CLEAR_FEATURE,
            value: feature,
            index: port as u16,
            length: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_requests() {
        let s = Setup::get_descriptor(kind::DEVICE, 0, 18);
        assert_eq!(s.to_bytes(), [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);
        assert!(s.is_in());
        assert_eq!(Setup::get_string(2, 0x0409, 255).to_bytes(), [0x80, 0x06, 0x02, 0x03, 0x09, 0x04, 0xFF, 0x00]);
        let c = Setup::set_configuration(1);
        assert_eq!(c.to_bytes(), [0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(!c.is_in());
        assert_eq!(Setup::clear_halt(0x81).to_bytes(), [0x02, 0x01, 0, 0, 0x81, 0, 0, 0]);
    }

    #[test]
    fn hid_requests() {
        let r = Setup::get_interface_descriptor(kind::HID_REPORT, 1, 63);
        assert_eq!(r.to_bytes(), [0x81, 0x06, 0x00, 0x22, 0x01, 0x00, 0x3F, 0x00]);
        assert_eq!(Setup::hid_set_idle(0).to_bytes(), [0x21, 0x0A, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Setup::hid_set_protocol(2, true).to_bytes(), [0x21, 0x0B, 1, 0, 2, 0, 0, 0]);
        assert_eq!(Setup::hid_set_output_report(0, 0, 1).to_bytes(), [0x21, 0x09, 0x00, 0x02, 0, 0, 1, 0]);
    }

    #[test]
    fn hub_requests() {
        assert_eq!(Setup::hub_descriptor(71).to_bytes(), [0xA0, 0x06, 0x00, 0x29, 0, 0, 71, 0]);
        assert_eq!(Setup::hub_status().to_bytes(), [0xA0, 0x00, 0, 0, 0, 0, 4, 0]);
        assert_eq!(Setup::hub_clear_feature(1).to_bytes(), [0x20, 0x01, 1, 0, 0, 0, 0, 0]);
        assert_eq!(Setup::hub_port_status(3).to_bytes(), [0xA3, 0x00, 0, 0, 3, 0, 4, 0]);
        assert_eq!(Setup::hub_set_port_feature(2, 4).to_bytes(), [0x23, 0x03, 4, 0, 2, 0, 0, 0]);
        assert_eq!(Setup::hub_clear_port_feature(2, 20).to_bytes(), [0x23, 0x01, 20, 0, 2, 0, 0, 0]);
    }
}
