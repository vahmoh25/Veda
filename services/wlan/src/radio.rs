//! The radio a driver offered through `wlanphy`.

use alloc::vec::Vec;

use vproto::netring::{Link, SlotMeta, TxInfo, kind};
use vproto::wlan::{Band, ChannelInfo, PhyInfo, PhyStats, RadioState, channel_flags, wlanphy_ctl};
use vrt::object::Channel;

/// How long a control call to the driver may take before the radio is
/// considered broken (a hung driver must not freeze the service).
const CONTROL_TIMEOUT_NS: u64 = 2_000_000_000;

pub fn band_of(channel: u8) -> Band {
    if channel <= 14 { Band::Ghz2 } else { Band::Ghz5 }
}

pub struct Radio {
    /// The `wlanphy` channel: state events, and `PEER_CLOSED` when the
    /// driver goes away.
    pub channel: Channel,
    ctl: wlanphy_ctl::Client,
    pub link: Link,
    pub info: PhyInfo,
    pub state: RadioState,
    /// The channel the radio is tuned to.
    pub tuned: Option<u8>,
    pub powered: bool,
    /// A control call failed or timed out: the driver is not trusted to
    /// work any more and the radio is dropped.
    pub broken: bool,
    next_id: u32,
}

impl Radio {
    pub fn new(channel: Channel, control: Channel, link: Link, info: PhyInfo, state: RadioState) -> Radio {
        let ctl = wlanphy_ctl::Client::new(control);
        ctl.set_timeout(CONTROL_TIMEOUT_NS);
        Radio { channel, ctl, link, info, state, tuned: None, powered: false, broken: false, next_id: 1 }
    }

    pub fn usable(&self) -> bool {
        self.state == RadioState::Ready && !self.broken
    }

    /// The channels the radio may use, in scanning order.
    pub fn channels(&self) -> Vec<&ChannelInfo> {
        let mut v: Vec<&ChannelInfo> =
            self.info.channels.iter().filter(|c| c.flags & channel_flags::DISABLED == 0).collect();
        v.sort_by_key(|c| (c.band, c.number));
        v
    }

    pub fn has_channel(&self, channel: u8) -> bool {
        self.channels().iter().any(|c| c.number == channel)
    }

    /// Whether the radio may transmit on `channel` before hearing an
    /// access point there.
    pub fn may_probe(&self, channel: u8) -> bool {
        self.info.channels.iter().any(|c| c.number == channel && c.flags & channel_flags::NO_IR == 0)
    }

    /// Tunes the radio. Returns whether it is now on `channel`.
    pub fn tune(&mut self, channel: u8) -> bool {
        if self.tuned == Some(channel) {
            return true;
        }
        match self.ctl.set_channel(band_of(channel), channel) {
            Ok(Ok(())) => {
                self.tuned = Some(channel);
                true
            }
            Ok(Err(_)) => false,
            Err(_) => {
                self.broken = true;
                false
            }
        }
    }

    pub fn set_power(&mut self, on: bool) -> bool {
        match self.ctl.set_power(on) {
            Ok(Ok(())) => {
                self.powered = on;
                if !on {
                    self.tuned = None;
                }
                true
            }
            Ok(Err(_)) => false,
            Err(_) => {
                self.broken = true;
                false
            }
        }
    }

    pub fn stats(&self) -> Option<PhyStats> {
        self.ctl.stats().ok()
    }

    /// Queues an 802.11 frame for transmission.
    pub fn send(&mut self, frame: &[u8], no_ack: bool) -> bool {
        let id = if no_ack {
            0
        } else {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.next_id
        };
        let meta = TxInfo { id, rate: 0, retries: 0, no_ack }.pack();
        self.link.send(SlotMeta { kind: kind::IEEE80211, flags: 0, meta }, frame)
    }
}
