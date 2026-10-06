//! The USB 2.0 hub class (USB 2.0 sections 11.23 and 11.24): the hub
//! descriptor, port status and the status change report.

use crate::descriptor::kind;
use crate::{Speed, le16};

/// Hub and port features for SET_FEATURE and CLEAR_FEATURE.
pub mod feature {
    /// The hub's own change features.
    pub const C_HUB_LOCAL_POWER: u16 = 0;
    pub const C_HUB_OVER_CURRENT: u16 = 1;
    pub const PORT_RESET: u16 = 4;
    pub const PORT_POWER: u16 = 8;
    /// The change features, acknowledged with CLEAR_FEATURE.
    pub const C_PORT_CONNECTION: u16 = 16;
    pub const C_PORT_ENABLE: u16 = 17;
    pub const C_PORT_SUSPEND: u16 = 18;
    pub const C_PORT_OVER_CURRENT: u16 = 19;
    pub const C_PORT_RESET: u16 = 20;
}

/// The hub descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubDescriptor {
    /// Number of downstream ports.
    pub ports: u8,
    /// `wHubCharacteristics`.
    pub characteristics: u16,
    /// Time from powering a port until its power is good, in ms.
    pub power_on_delay_ms: u32,
}

impl HubDescriptor {
    /// The hub descriptor of a hub with up to 7 ports is 9 bytes long;
    /// every port beyond that needs more room for the bitmaps.
    pub const MAX_LEN: u16 = 71;

    /// Parses a hub descriptor (`None`: too short or not one).
    pub fn parse(b: &[u8]) -> Option<HubDescriptor> {
        if b.len() < 7 || b[0] < 7 || b[1] != kind::HUB {
            return None;
        }
        Some(HubDescriptor { ports: b[2], characteristics: le16(b, 3), power_on_delay_ms: b[5] as u32 * 2 })
    }

    /// The transaction translator's think time, as encoded for xHCI (0 to
    /// 3: 8, 16, 24 or 32 full-speed bit times).
    pub fn tt_think_time(&self) -> u8 {
        ((self.characteristics >> 5) & 3) as u8
    }
}

/// A port's status and its changes (GET_STATUS on a port).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PortStatus {
    pub status: u16,
    pub change: u16,
}

impl PortStatus {
    /// Parses the 4 bytes GET_STATUS returns (`None`: fewer).
    pub fn parse(b: &[u8]) -> Option<PortStatus> {
        (b.len() >= 4).then(|| PortStatus { status: le16(b, 0), change: le16(b, 2) })
    }

    /// A device is connected.
    pub fn connected(&self) -> bool {
        self.status & 1 != 0
    }

    /// The port is enabled (after a successful reset).
    pub fn enabled(&self) -> bool {
        self.status & 2 != 0
    }

    /// A reset is in progress.
    pub fn resetting(&self) -> bool {
        self.status & 0x10 != 0
    }

    /// The speed of the device on the port.
    pub fn speed(&self) -> Speed {
        if self.status & 0x200 != 0 {
            Speed::Low
        } else if self.status & 0x400 != 0 {
            Speed::High
        } else {
            Speed::Full
        }
    }

    /// A device came or went since the change was last acknowledged.
    pub fn connection_changed(&self) -> bool {
        self.change & 1 != 0
    }

    /// A reset finished.
    pub fn reset_changed(&self) -> bool {
        self.change & 0x10 != 0
    }

    /// The CLEAR_FEATURE selectors that acknowledge the reported changes.
    pub fn change_features(&self) -> impl Iterator<Item = u16> + use<> {
        let change = self.change;
        (0..5).filter(move |bit| change & (1 << bit) != 0).map(|bit| feature::C_PORT_CONNECTION + bit)
    }
}

/// The ports a hub's status change report names (bit 0 is the hub
/// itself, bit `n` port `n`).
pub fn changed_ports(report: &[u8], ports: u8) -> impl Iterator<Item = u8> + '_ {
    (1..=ports).filter(move |&p| report.get(p as usize / 8).is_some_and(|b| b & (1 << (p % 8)) != 0))
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn descriptor() {
        // 4 ports, individual power switching, TT think time 16 bit times,
        // 100 ms to power good.
        let b = [0x09, 0x29, 0x04, 0x29, 0x00, 0x32, 0x64, 0x00, 0xFF];
        let d = HubDescriptor::parse(&b).unwrap();
        assert_eq!((d.ports, d.power_on_delay_ms, d.tt_think_time()), (4, 100, 1));
        assert_eq!(HubDescriptor::parse(&b[..6]), None);
    }

    #[test]
    fn port_status() {
        // Connected, enabled, powered, high speed; connection changed.
        let s = PortStatus::parse(&[0x03, 0x05, 0x01, 0x00]).unwrap();
        assert!(s.connected() && s.enabled() && !s.resetting());
        assert_eq!(s.speed(), Speed::High);
        assert!(s.connection_changed() && !s.reset_changed());
        assert_eq!(s.change_features().collect::<Vec<_>>(), [feature::C_PORT_CONNECTION]);
        let low = PortStatus { status: 0x0303, change: 0x0011 };
        assert_eq!(low.speed(), Speed::Low);
        assert_eq!(low.change_features().collect::<Vec<_>>(), [feature::C_PORT_CONNECTION, feature::C_PORT_RESET]);
        assert_eq!(PortStatus { status: 0x0103, change: 0 }.speed(), Speed::Full);
        assert_eq!(PortStatus::parse(&[1, 2, 3]), None);
    }

    #[test]
    fn change_report() {
        assert_eq!(changed_ports(&[0b0001_0100], 4).collect::<Vec<_>>(), [2, 4]);
        assert_eq!(changed_ports(&[0x00, 0x02], 10).collect::<Vec<_>>(), [9]);
        // Bits beyond the report or the port count are ignored.
        assert_eq!(changed_ports(&[0xFF], 10).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(changed_ports(&[0x01], 4).count(), 0);
    }
}
