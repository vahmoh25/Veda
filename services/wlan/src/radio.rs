//! The radio a driver offered through `wlanphy`: one that moves frames, or
//! a managed one, which joins access points on the service's commands.

use alloc::vec::Vec;

use vipc::Bytes;
use vproto::netring::{Link, SlotMeta, TxInfo, kind};
use vproto::wlan::{
    Band, ChannelInfo, KeyKind, PhyError, PhyInfo, PhyStats, RadioState, channel_flags, phy_caps, wlanmlme_ctl,
    wlanphy_ctl,
};
use vrt::object::Channel;
use vwlan::frame::Mac;

/// How long a control call to the driver may take before the radio is
/// considered broken (a hung driver must not freeze the service).
const CONTROL_TIMEOUT_NS: u64 = 2_000_000_000;

pub fn band_of(channel: u8) -> Band {
    if channel <= 14 { Band::Ghz2 } else { Band::Ghz5 }
}

/// The radio's control channel: what its kind speaks.
enum Control {
    SoftMac(wlanphy_ctl::Client),
    Managed(wlanmlme_ctl::Client),
}

pub struct Radio {
    /// The `wlanphy` channel: state events, and `PEER_CLOSED` when the
    /// driver goes away.
    pub channel: Channel,
    ctl: Control,
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
        let ctl = if info.caps & phy_caps::MANAGED != 0 {
            let c = wlanmlme_ctl::Client::new(control);
            c.set_timeout(CONTROL_TIMEOUT_NS);
            Control::Managed(c)
        } else {
            let c = wlanphy_ctl::Client::new(control);
            c.set_timeout(CONTROL_TIMEOUT_NS);
            Control::SoftMac(c)
        };
        Radio { channel, ctl, link, info, state, tuned: None, powered: false, broken: false, next_id: 1 }
    }

    /// The managed radio's control (`None` for one that moves frames).
    fn mlme(&self) -> Option<&wlanmlme_ctl::Client> {
        match &self.ctl {
            Control::Managed(c) => Some(c),
            Control::SoftMac(_) => None,
        }
    }

    pub fn usable(&self) -> bool {
        self.state == RadioState::Ready && !self.broken
    }

    /// The radio joins access points itself (see `vproto::wlan`).
    pub fn managed(&self) -> bool {
        self.info.caps & phy_caps::MANAGED != 0
    }

    /// Whether a control call did what it asked; a call that failed or
    /// timed out breaks the radio.
    fn done<T>(&mut self, result: Result<Result<T, PhyError>, vipc::IpcError>) -> bool {
        match result {
            Ok(Ok(_)) => true,
            Ok(Err(_)) => false,
            Err(_) => {
                self.broken = true;
                false
            }
        }
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
        let Control::SoftMac(ctl) = &self.ctl else { return false };
        let r = ctl.set_channel(band_of(channel), channel);
        let tuned = self.done(r);
        if tuned {
            self.tuned = Some(channel);
        }
        tuned
    }

    pub fn set_power(&mut self, on: bool) -> bool {
        let r = match &self.ctl {
            Control::SoftMac(c) => c.set_power(on),
            Control::Managed(c) => c.set_power(on),
        };
        let done = self.done(r);
        if done {
            self.powered = on;
            if !on {
                self.tuned = None;
            }
        }
        done
    }

    // Managed radios' commands: whether the radio took them.

    pub fn scan(&mut self, ssids: &[Vec<u8>]) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.scan(ssids.iter().map(|s| Bytes(s.clone())).collect());
        self.done(r)
    }

    pub fn authenticate(&mut self, bssid: Mac, channel: u8, ssid: &[u8], body: &[u8]) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.authenticate(bssid, band_of(channel), channel, Bytes(ssid.to_vec()), Bytes(body.to_vec()));
        self.done(r)
    }

    pub fn associate(&mut self, bssid: Mac, channel: u8, ssid: &[u8], ies: &[u8], pmf: bool) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.associate(bssid, band_of(channel), channel, Bytes(ssid.to_vec()), Bytes(ies.to_vec()), pmf);
        self.done(r)
    }

    pub fn deauthenticate(&mut self, bssid: Mac, reason: u16) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.deauthenticate(bssid, reason);
        self.done(r)
    }

    pub fn send_eapol(&mut self, peer: Mac, frame: &[u8], encrypt: bool) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.send_eapol(peer, Bytes(frame.to_vec()), encrypt);
        self.done(r)
    }

    pub fn install_key(&mut self, kind: vwlan::station::KeyKind, index: u8, key: &[u8], rsc: u64, peer: Mac) -> bool {
        use vwlan::station::KeyKind as K;
        let kind = match kind {
            K::Pairwise => KeyKind::Pairwise,
            K::Group => KeyKind::Group,
            K::Integrity => KeyKind::Integrity,
        };
        let Some(c) = self.mlme() else { return false };
        let r = c.install_key(kind, index, Bytes(key.to_vec()), rsc, peer);
        self.done(r)
    }

    pub fn authorize(&mut self, peer: Mac) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.authorize(peer);
        self.done(r)
    }

    pub fn send_management(&mut self, frame: &[u8]) -> bool {
        let Some(c) = self.mlme() else { return false };
        let r = c.send_management(Bytes(frame.to_vec()));
        self.done(r)
    }

    /// Queues an Ethernet frame for a managed radio.
    pub fn send_ethernet(&mut self, eth: &[u8]) -> bool {
        self.link.send(SlotMeta::ethernet(), eth)
    }

    pub fn stats(&self) -> Option<PhyStats> {
        match &self.ctl {
            Control::SoftMac(c) => c.stats().ok(),
            Control::Managed(c) => c.stats().ok(),
        }
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
