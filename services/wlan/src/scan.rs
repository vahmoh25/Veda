//! Scanning: visiting the channels in turn, probing where allowed, and the
//! table of access points heard (from beacons and probe responses, during
//! scans and in between).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use vproto::wlan::{BssInfo, NetworkInfo, Security, signal_bars, ssid_display};
use vwlan::frame::Mac;
use vwlan::profile::Profile;
use vwlan::scan::{Bss, probe_request};

use crate::radio::{Radio, band_of};

/// Time spent on each channel: long enough to hear a beacon sent every
/// 102.4 ms, and a probe response.
pub const DWELL_MS: u64 = 130;
/// Access points not heard for this long are forgotten...
const MAX_AGE_MS: u64 = 90_000;
/// ...and those not heard for this long are not offered as networks.
const FRESH_MS: u64 = 30_000;
/// Most access points remembered (a crowded place has a few hundred).
const MAX_ENTRIES: usize = 512;
/// A managed radio scans every channel in one go, and says when it is
/// done; this is in case it never says.
const MANAGED_SCAN_MS: u64 = 15_000;

/// A scan in progress.
pub struct Scan {
    channels: Vec<u8>,
    next: usize,
    /// When to move on from the current channel.
    pub dwell_until: u64,
    /// SSIDs to probe for by name (hidden networks).
    directed: Vec<Vec<u8>>,
    /// Asked for by a user (rather than a background scan).
    pub requested: bool,
    seq: u16,
    /// A managed radio's: asked for in one command.
    managed: bool,
    started: bool,
}

impl Scan {
    pub fn new(radio: &Radio, directed: Vec<Vec<u8>>, requested: bool) -> Scan {
        let channels = radio.channels().iter().map(|c| c.number).collect();
        Scan {
            channels,
            next: 0,
            dwell_until: 0,
            directed,
            requested,
            seq: 0,
            managed: radio.managed(),
            started: false,
        }
    }

    /// Moves to the next channel (tuning and probing). Returns `false` when
    /// every channel has been visited. A managed radio's scan starts on the
    /// first step and ends when the radio says (or on the next).
    pub fn step(&mut self, radio: &mut Radio, now: u64) -> bool {
        if self.managed {
            let start = !self.started;
            self.started = true;
            self.dwell_until = now + MANAGED_SCAN_MS;
            return start && radio.scan(&self.directed);
        }
        while let Some(&c) = self.channels.get(self.next) {
            self.next += 1;
            if !radio.tune(c) {
                continue;
            }
            if radio.may_probe(c) {
                let mac = radio.info.mac;
                self.seq = (self.seq + 1) & 0xFFF;
                radio.send(&probe_request(&mac, None, c, self.seq), true);
                for ssid in &self.directed {
                    self.seq = (self.seq + 1) & 0xFFF;
                    radio.send(&probe_request(&mac, Some(ssid), c, self.seq), true);
                }
            }
            self.dwell_until = now + DWELL_MS;
            return true;
        }
        false
    }
}

/// Access points heard, by BSSID.
#[derive(Default)]
pub struct Table {
    entries: BTreeMap<Mac, Bss>,
    /// Access points whose beacons hide their name.
    hiding: alloc::collections::BTreeSet<Mac>,
}

impl Table {
    /// Records an access point heard at `now`.
    pub fn update(&mut self, mut b: Bss) {
        if b.hidden() {
            self.hiding.insert(b.bssid);
        }
        if let Some(old) = self.entries.get(&b.bssid) {
            // A hidden network's beacons carry no name; keep the name a
            // probe response told us.
            if b.ssid.is_empty() && !old.ssid.is_empty() && old.security == b.security {
                b.ssid = old.ssid.clone();
            }
        } else if self.entries.len() >= MAX_ENTRIES {
            // Make room by dropping the access point heard longest ago.
            if let Some(oldest) = self.entries.values().min_by_key(|e| e.seen_ms).map(|e| e.bssid) {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(b.bssid, b);
    }

    pub fn expire(&mut self, now: u64) {
        self.entries.retain(|_, b| now.saturating_sub(b.seen_ms) <= MAX_AGE_MS);
        let entries = &self.entries;
        self.hiding.retain(|m| entries.contains_key(m));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.hiding.clear();
    }

    /// Whether the access point's beacons hide its name.
    pub fn beacon_hides_name(&self, bssid: &Mac) -> bool {
        self.hiding.contains(bssid)
    }

    pub fn get(&self, bssid: &Mac) -> Option<&Bss> {
        self.entries.get(bssid)
    }

    /// Access points heard recently.
    pub fn fresh(&self, now: u64) -> Vec<Bss> {
        self.recent(now, FRESH_MS)
    }

    /// Access points heard in the last `max_age_ms` milliseconds.
    pub fn recent(&self, now: u64, max_age_ms: u64) -> Vec<Bss> {
        self.entries.values().filter(|b| now.saturating_sub(b.seen_ms) <= max_age_ms).cloned().collect()
    }

    /// Every access point, strongest first, for diagnostics.
    pub fn access_points(&self, now: u64) -> Vec<BssInfo> {
        let mut v: Vec<BssInfo> = self
            .entries
            .values()
            .map(|b| BssInfo {
                ssid: vipc::Bytes(b.ssid.clone()),
                bssid: b.bssid,
                band: band_of(b.channel),
                channel: b.channel,
                signal_dbm: b.signal_dbm,
                security: to_proto(b.security),
                pmf: b.pmf,
                age_ms: now.saturating_sub(b.seen_ms).min(u32::MAX as u64) as u32,
            })
            .collect();
        v.sort_by_key(|b| -(b.signal_dbm as i32));
        v
    }

    /// Networks in range (access points grouped by SSID and security),
    /// strongest first. Hidden networks appear once their name is known.
    pub fn networks(&self, now: u64, profiles: &[Profile], connected: Option<&[u8]>) -> Vec<NetworkInfo> {
        let mut groups: BTreeMap<(Vec<u8>, Security), (i8, u32)> = BTreeMap::new();
        for b in self.fresh(now) {
            if b.ssid.is_empty() || b.ssid.iter().all(|&c| c == 0) {
                continue;
            }
            let e = groups.entry((b.ssid.clone(), to_proto(b.security))).or_insert((i8::MIN, 0));
            e.0 = e.0.max(b.signal_dbm);
            e.1 += 1;
        }
        let mut v: Vec<NetworkInfo> = groups
            .into_iter()
            .map(|((ssid, security), (signal_dbm, count))| NetworkInfo {
                name: ssid_display(&ssid),
                saved: profiles.iter().any(|p| p.ssid == ssid),
                connected: connected == Some(ssid.as_slice()),
                ssid: vipc::Bytes(ssid),
                security,
                signal_dbm,
                bars: signal_bars(signal_dbm),
                access_points: count,
            })
            .collect();
        v.sort_by_key(|n| (!n.connected, -(n.signal_dbm as i32)));
        v
    }
}

/// The `vproto` form of a security type.
pub fn to_proto(s: vwlan::rsn::Security) -> Security {
    use vwlan::rsn::Security as S;
    match s {
        S::Open => Security::Open,
        S::Wep => Security::Wep,
        S::WpaTkip => Security::WpaTkip,
        S::Wpa2Personal => Security::Wpa2Personal,
        S::Wpa3Personal => Security::Wpa3Personal,
        S::Wpa2Wpa3Personal => Security::Wpa2Wpa3Personal,
        S::Enterprise => Security::Enterprise,
        S::Owe => Security::Owe,
    }
}

/// The `vwlan` form of a security type.
pub fn from_proto(s: Security) -> vwlan::rsn::Security {
    use vwlan::rsn::Security as S;
    match s {
        Security::Open => S::Open,
        Security::Wep => S::Wep,
        Security::WpaTkip => S::WpaTkip,
        Security::Wpa2Personal => S::Wpa2Personal,
        Security::Wpa3Personal => S::Wpa3Personal,
        Security::Wpa2Wpa3Personal => S::Wpa2Wpa3Personal,
        Security::Enterprise => S::Enterprise,
        Security::Owe => S::Owe,
    }
}
