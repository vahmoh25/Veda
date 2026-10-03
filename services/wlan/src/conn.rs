//! Joining networks: requests, the station, automatic connection, retries,
//! recovery after a lost connection and roaming.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vproto::netring::{RxInfo, SlotMeta};
use vproto::wlan::{ConnState, FailReason, WlanError, WlanEvent, ssid_display};
use vwlan::policy;
use vwlan::profile::{self, MAX_PROFILES, Profile};
use vwlan::rsn::Security;
use vwlan::scan::{Bss, parse_bss};
use vwlan::station::{Action, Credential, Failure, StaEvent, Target};

use crate::Wlan;
use crate::scan::Scan;

/// Background scans while disconnected: every 10 s at first...
const IDLE_SCAN_FAST_MS: u64 = 10_000;
const IDLE_SCAN_FAST_COUNT: u32 = 6;
/// ...then every 30 s.
const IDLE_SCAN_SLOW_MS: u64 = 30_000;
/// While connected: every 5 minutes (to keep the list fresh), or every 30 s
/// when the signal is weak (to find a better access point).
const CONNECTED_SCAN_MS: u64 = 300_000;
const WEAK_SCAN_MS: u64 = 30_000;
/// Attempts at a network the user asked for before giving up.
const MANUAL_ATTEMPTS: u32 = 3;
/// The loop wakes at least this often (retry delays, status).
const MAX_SLEEP_MS: u64 = 1_000;

pub fn mac_string(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// A network to join.
#[derive(Clone)]
pub struct Request {
    pub ssid: Vec<u8>,
    /// Required security (`None`: whatever the network offers, if
    /// supported).
    pub security: Option<Security>,
    pub passphrase: Option<String>,
    /// Remember the network after joining it.
    pub save: bool,
    pub auto_connect: bool,
    pub hidden: bool,
    /// WPA3 must be used (the saved network has used it before).
    pub sae_only: bool,
    /// Asked for by the user: failures are reported and not retried
    /// forever.
    pub manual: bool,
    pub attempts: u32,
    /// A scan was made to find it.
    pub scanned: bool,
}

impl Request {
    fn from_profile(p: &Profile) -> Request {
        Request {
            ssid: p.ssid.clone(),
            security: Some(p.security),
            passphrase: p.passphrase.clone(),
            save: true,
            auto_connect: p.auto_connect,
            hidden: p.hidden,
            sae_only: p.sae_used,
            manual: false,
            attempts: 0,
            scanned: false,
        }
    }

    /// Whether `bss` may be used to join this network.
    fn accepts(&self, bss: &Bss) -> bool {
        let named = bss.ssid == self.ssid;
        let security_ok = match self.security {
            Some(s) => policy::security_compatible(s, self.sae_only, bss.security),
            None => bss.security.supported(),
        };
        named && security_ok
    }
}

/// The network being joined or joined.
pub struct Current {
    pub request: Request,
    pub bss: Bss,
    pub state: ConnState,
    pub connected_ms: Option<u64>,
    /// WPA3 is in use.
    pub sae: bool,
    /// Moving between access points of the network (the link stays up).
    pub roaming: bool,
}

fn fail_reason(f: Failure) -> FailReason {
    match f {
        Failure::WrongPassword => FailReason::WrongPassword,
        Failure::NoResponse => FailReason::Timeout,
        Failure::AuthRejected(_) => FailReason::AuthRejected,
        Failure::AssocRejected(_) => FailReason::AssocRejected,
        Failure::Unsupported => FailReason::Unsupported,
        Failure::PasswordRequired => FailReason::PasswordRequired,
        Failure::Deauthenticated(_) => FailReason::Disconnected,
        Failure::SignalLost => FailReason::SignalLost,
        Failure::HandshakeFailed => FailReason::HandshakeFailed,
        Failure::Local => FailReason::Disconnected,
    }
}

fn security_label(s: Security, sae: bool) -> &'static str {
    match s {
        Security::Open => "open",
        Security::Wpa2Wpa3Personal if sae => "WPA3 (transition mode)",
        Security::Wpa2Wpa3Personal => "WPA2 (transition mode)",
        Security::Wpa3Personal => "WPA3",
        Security::Wpa2Personal => "WPA2",
        _ => "unsupported security",
    }
}

impl Wlan {
    /// A frame from the radio.
    pub fn radio_frame(&mut self, frame: &[u8], info: RxInfo, now: u64) {
        if let Some(bss) = parse_bss(frame, info.channel, info.signal_dbm, now) {
            if self.current.as_ref().is_some_and(|c| c.bss.bssid == bss.bssid) {
                self.counters.beacons_rx += 1;
            }
            self.table.update(bss);
        }
        let actions = self.sta.receive(frame, info.signal_dbm, now, &mut self.rng);
        self.station_actions(actions, now);
    }

    pub fn station_actions(&mut self, actions: Vec<Action>, now: u64) {
        for a in actions {
            match a {
                Action::Transmit { frame, no_ack } => {
                    if let Some(r) = self.radio.as_mut() {
                        r.send(&frame, no_ack);
                    }
                }
                Action::SetChannel(c) => {
                    if let Some(r) = self.radio.as_mut() {
                        r.tune(c);
                    }
                }
                Action::Deliver(eth) => {
                    self.counters.data_rx += 1;
                    if let Some(nd) = &self.netdev {
                        nd.link.send(SlotMeta::ethernet(), &eth);
                    }
                }
                Action::Event(e) => self.station_event(e, now),
            }
        }
        self.counters.data_tx = self.sta.counters.data_tx;
        self.counters.decrypt_errors = self.sta.counters.decrypt_errors;
        self.counters.replays = self.sta.counters.replays;
        self.counters.unprotected_dropped = self.sta.counters.unprotected_dropped;
        self.counters.group_rekeys = self.sta.counters.group_rekeys;
    }

    fn station_event(&mut self, e: StaEvent, now: u64) {
        match e {
            StaEvent::Authenticating => self.set_phase(ConnState::Authenticating),
            StaEvent::Associating => self.set_phase(ConnState::Associating),
            StaEvent::Securing => self.set_phase(ConnState::Securing),
            StaEvent::Connected { bssid, channel, security, pmf, sae } => {
                let Some(cur) = self.current.as_mut() else { return };
                let roamed = cur.roaming;
                cur.state = ConnState::Connected;
                cur.sae = sae;
                cur.roaming = false;
                if cur.connected_ms.is_none() || !roamed {
                    cur.connected_ms = Some(now);
                }
                let request = cur.request.clone();
                let ssid = request.ssid.clone();
                if roamed {
                    self.counters.roams += 1;
                } else {
                    self.counters.connections += 1;
                }
                self.last_failure = None;
                self.backoff.remove(&ssid);
                self.rejected.remove(&ssid);
                self.idle_scans = 0;
                self.next_scan_ms = now + CONNECTED_SCAN_MS;
                self.log(format!(
                    "connected to {} ({}{}), access point {} on channel {}",
                    ssid_display(&ssid),
                    security_label(security, sae),
                    if pmf { ", protected management frames" } else { "" },
                    mac_string(&bssid),
                    channel
                ));
                self.remember(&request, security, sae);
                self.set_link(true);
            }
            StaEvent::JoinFailed(f) => self.connection_ended(f, false, now),
            StaEvent::Disconnected(f) => self.connection_ended(f, true, now),
        }
    }

    fn set_phase(&mut self, s: ConnState) {
        if let Some(c) = self.current.as_mut()
            && c.state != ConnState::Connected
        {
            c.state = s;
        }
    }

    /// The attempt or connection ended with `f`.
    pub fn connection_ended(&mut self, f: Failure, was_connected: bool, now: u64) {
        let Some(cur) = self.current.take() else { return };
        let name = ssid_display(&cur.request.ssid);
        if f == Failure::Local {
            if let Some((req, bss)) = self.roam_to.take() {
                // Moving to another access point: keep the link up.
                self.start_join(req, bss, true, now);
                if let Some(c) = self.current.as_mut() {
                    c.connected_ms = cur.connected_ms;
                }
                return;
            }
            self.log(format!("left {}", name));
            self.set_link(false);
            return;
        }
        let reason = fail_reason(f);
        self.last_failure = Some(reason);
        self.set_link(false);
        if was_connected {
            self.counters.disconnections += 1;
            self.log(format!("lost the connection to {}: {}", name, reason));
        } else {
            if matches!(f, Failure::WrongPassword | Failure::HandshakeFailed | Failure::AuthRejected(_)) {
                self.counters.auth_failures += 1;
            }
            self.log(format!("could not join {}: {}", name, reason));
        }
        let mut req = cur.request;
        match f {
            Failure::WrongPassword | Failure::Unsupported | Failure::PasswordRequired => {
                if req.manual {
                    self.report_failure(&req.ssid, reason);
                } else if f == Failure::WrongPassword {
                    // The saved password no longer works: wait for the user.
                    self.rejected.insert(req.ssid.clone());
                }
            }
            _ => {
                if was_connected {
                    // Try again straight away, after a fresh look around
                    // (the access point may be gone, another one of the
                    // network may be there).
                    self.backoff.remove(&req.ssid);
                    self.next_scan_ms = now;
                    req.attempts = 0;
                } else {
                    self.backoff.entry(req.ssid.clone()).or_default().failed(now);
                    req.attempts += 1;
                }
                let saved_auto = self.profiles.iter().any(|p| p.ssid == req.ssid && p.auto_connect);
                if req.manual && req.attempts >= MANUAL_ATTEMPTS {
                    self.report_failure(&req.ssid, reason);
                } else if !saved_auto {
                    // Saved networks come back through automatic
                    // connection; others are retried as a request.
                    req.scanned = false;
                    self.request = Some(req);
                }
            }
        }
    }

    fn report_failure(&mut self, ssid: &[u8], reason: FailReason) {
        self.notify(WlanEvent::ConnectFailed { ssid: vipc::Bytes(ssid.to_vec()), name: ssid_display(ssid), reason });
    }

    /// Records a successful connection in the saved networks.
    fn remember(&mut self, req: &Request, security: Security, sae: bool) {
        let unix_s = vrt::time::unix_time_ns() / 1_000_000_000;
        let changed = if let Some(p) = self.profiles.iter_mut().find(|p| p.ssid == req.ssid) {
            p.last_connected = unix_s;
            if let Some(pass) = &req.passphrase {
                p.passphrase = Some(pass.clone());
            }
            p.sae_used |= sae;
            if req.manual {
                p.auto_connect = req.auto_connect;
                p.hidden |= req.hidden;
            }
            true
        } else if req.save && self.profiles.len() < MAX_PROFILES {
            self.profiles.push(Profile {
                ssid: req.ssid.clone(),
                security,
                passphrase: if security == Security::Open { None } else { req.passphrase.clone() },
                auto_connect: req.auto_connect,
                hidden: req.hidden,
                sae_used: sae,
                last_connected: unix_s,
            });
            true
        } else {
            false
        };
        if changed {
            self.save_profiles();
        }
    }

    /// Starts joining `bss` for `req`.
    pub fn start_join(&mut self, req: Request, bss: Bss, roaming: bool, now: u64) {
        let credential = match &req.passphrase {
            Some(p) => Credential::Passphrase(p.clone()),
            None => Credential::None,
        };
        let target = Target { bss: bss.clone(), ssid: req.ssid.clone(), credential, allow_sae: true };
        self.counters.connect_attempts += 1;
        self.log(format!(
            "{} {} via {} on channel {} ({} dBm)",
            if roaming { "moving within" } else { "joining" },
            ssid_display(&req.ssid),
            mac_string(&bss.bssid),
            bss.channel,
            bss.signal_dbm
        ));
        self.current = Some(Current {
            request: req,
            bss,
            state: ConnState::Authenticating,
            connected_ms: None,
            sae: false,
            roaming,
        });
        let actions = self.sta.connect(target, now, &mut self.rng);
        self.station_actions(actions, now);
    }

    /// Handles a `connect` request from an application.
    pub fn connect_request(&mut self, r: vproto::wlan::ConnectRequest, now: u64) -> Result<(), WlanError> {
        if !self.radio_usable() {
            return Err(WlanError::NoAdapter);
        }
        if !self.radio_on {
            return Err(WlanError::RadioOff);
        }
        let ssid = r.ssid.0;
        if !profile::valid_ssid(&ssid) {
            return Err(WlanError::BadSsid);
        }
        let saved = self.profiles.iter().find(|p| p.ssid == ssid).cloned();
        if let Some(p) = &r.passphrase
            && !profile::valid_passphrase(p)
        {
            return Err(WlanError::BadPassphrase);
        }
        let security = r.security.map(crate::scan::from_proto);
        if security.is_some_and(|s| !s.supported()) {
            return Err(WlanError::Unsupported);
        }
        let passphrase = r.passphrase.or_else(|| saved.as_ref().and_then(|p| p.passphrase.clone()));
        if saved.is_none() && r.save && self.profiles.len() >= MAX_PROFILES {
            return Err(WlanError::LimitReached);
        }
        // Know the network already? Then check the password is there.
        let seen = self.table.fresh(now);
        let offered = seen.iter().filter(|b| b.ssid == ssid).map(|b| b.security).find(|s| s.supported());
        let effective = security.or(offered).or(saved.as_ref().map(|p| p.security));
        if effective.is_some_and(|s| s != Security::Open) && passphrase.is_none() {
            return Err(WlanError::PasswordRequired);
        }
        if offered.is_none() && seen.iter().any(|b| b.ssid == ssid) {
            return Err(WlanError::Unsupported);
        }
        let sae_only = saved.as_ref().is_some_and(|p| p.sae_used)
            && passphrase == saved.as_ref().and_then(|p| p.passphrase.clone());
        let req = Request {
            ssid: ssid.clone(),
            security: security.or(saved.as_ref().map(|p| p.security)),
            passphrase,
            save: r.save,
            auto_connect: r.auto_connect,
            hidden: r.hidden || saved.as_ref().is_some_and(|p| p.hidden),
            sae_only,
            manual: true,
            attempts: 0,
            scanned: false,
        };
        // Leave the current network first.
        self.user_disconnected = false;
        self.rejected.remove(&ssid);
        self.backoff.remove(&ssid);
        self.roam_to = None;
        if self.current.is_some() {
            let actions = self.sta.disconnect(now);
            self.station_actions(actions, now);
        }
        self.request = Some(req);
        self.try_request(now);
        Ok(())
    }

    /// Leaves the network and stays disconnected until the next `connect`.
    pub fn user_disconnect(&mut self, now: u64) {
        self.user_disconnected = true;
        self.request = None;
        self.roam_to = None;
        if self.current.is_some() {
            let actions = self.sta.disconnect(now);
            self.station_actions(actions, now);
        }
        self.current = None;
        self.set_link(false);
    }

    /// The Wi-Fi switch.
    pub fn set_radio(&mut self, on: bool, now: u64) {
        if on == self.radio_on {
            return;
        }
        self.radio_on = on;
        if on {
            self.log("Wi-Fi turned on".into());
            self.user_disconnected = false;
            if let Some(r) = self.radio.as_mut() {
                r.set_power(true);
            }
            self.idle_scans = 0;
            self.next_scan_ms = now;
        } else {
            self.log("Wi-Fi turned off".into());
            self.request = None;
            self.roam_to = None;
            if self.current.is_some() {
                let actions = self.sta.disconnect(now);
                self.station_actions(actions, now);
            }
            self.current = None;
            self.set_link(false);
            self.scan = None;
            self.table.clear();
            if let Some(r) = self.radio.as_mut() {
                r.set_power(false);
            }
        }
    }

    /// Looks for the requested network: joins it if it was heard, scans
    /// for it otherwise.
    pub fn try_request(&mut self, now: u64) {
        let Some(mut req) = self.request.take() else { return };
        if !self.backoff.get(&req.ssid).is_none_or(|b| b.ready(now)) {
            self.request = Some(req);
            return;
        }
        let seen = self.table.fresh(now);
        let best = seen.iter().filter(|b| req.accepts(b)).max_by_key(|b| policy::score(b)).cloned();
        match best {
            Some(bss) => self.start_join(req, bss, false, now),
            None if !req.scanned => {
                req.scanned = true;
                self.request = Some(req);
                self.start_scan(true, now);
            }
            None => {
                self.last_failure = Some(FailReason::NotFound);
                if req.manual {
                    self.log(format!("{} is not in range", ssid_display(&req.ssid)));
                    self.report_failure(&req.ssid, FailReason::NotFound);
                } else {
                    // Keep waiting for a lost network to come back.
                    self.backoff.entry(req.ssid.clone()).or_default().failed(now);
                    req.scanned = false;
                    self.request = Some(req);
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Scanning

    /// Starts a scan if possible. `requested`: on behalf of a user.
    pub fn start_scan(&mut self, requested: bool, _now: u64) -> bool {
        let joining = self.current.as_ref().is_some_and(|c| c.state != ConnState::Connected);
        if !self.radio_usable() || !self.radio_on || joining {
            if requested {
                self.scan_wanted = true;
            }
            return false;
        }
        if let Some(s) = self.scan.as_mut() {
            s.requested |= requested;
            return true;
        }
        let mut directed: Vec<Vec<u8>> = self.profiles.iter().filter(|p| p.hidden).map(|p| p.ssid.clone()).collect();
        if let Some(r) = &self.request
            && r.hidden
            && !directed.contains(&r.ssid)
        {
            directed.push(r.ssid.clone());
        }
        directed.truncate(8);
        let radio = self.radio.as_ref().expect("usable radio");
        self.scan = Some(Scan::new(radio, directed, requested));
        self.scan_wanted = false;
        true
    }

    fn scan_step(&mut self, now: u64) {
        let Some(scan) = self.scan.as_mut() else { return };
        if now < scan.dwell_until {
            return;
        }
        let Some(radio) = self.radio.as_mut().filter(|r| r.usable()) else {
            self.scan = None;
            return;
        };
        if !scan.step(radio, now) {
            self.finish_scan(now);
        }
    }

    fn finish_scan(&mut self, now: u64) {
        let Some(scan) = self.scan.take() else { return };
        self.counters.scans += 1;
        // Back to the network's channel.
        if let Some(c) = self.current.as_ref().map(|c| c.bss.channel)
            && let Some(r) = self.radio.as_mut()
        {
            r.tune(c);
        }
        let connected = self.sta.is_connected();
        self.next_scan_ms = now
            + if connected {
                if self.sta.signal_dbm < policy::ROAM_BELOW_DBM { WEAK_SCAN_MS } else { CONNECTED_SCAN_MS }
            } else {
                self.idle_scans = self.idle_scans.saturating_add(1);
                if self.idle_scans < IDLE_SCAN_FAST_COUNT { IDLE_SCAN_FAST_MS } else { IDLE_SCAN_SLOW_MS }
            };
        if scan.requested {
            self.notify(WlanEvent::ScanDone {});
        }
        if connected {
            self.consider_roaming(now);
        } else if self.request.is_some() {
            self.try_request(now);
        }
    }

    fn consider_roaming(&mut self, now: u64) {
        let Some(cur) = self.current.as_ref() else { return };
        let profile = Profile {
            ssid: cur.request.ssid.clone(),
            security: cur.bss.security,
            passphrase: cur.request.passphrase.clone(),
            auto_connect: true,
            hidden: cur.request.hidden,
            sae_used: cur.sae,
            last_connected: 0,
        };
        let current_bssid = cur.bss.bssid;
        let mut req = cur.request.clone();
        req.sae_only |= cur.sae;
        req.manual = false;
        let seen = self.table.fresh(now);
        let Some(target) = policy::roam_target(&profile, &current_bssid, self.sta.signal_dbm, &seen).cloned() else {
            return;
        };
        self.log(format!(
            "signal {} dBm: moving to {} ({} dBm)",
            self.sta.signal_dbm,
            mac_string(&target.bssid),
            target.signal_dbm
        ));
        self.roam_to = Some((req, target));
        let actions = self.sta.disconnect(now);
        self.station_actions(actions, now);
    }

    // ---------------------------------------------------------------
    // Timers and automatic decisions

    pub fn tick(&mut self, now: u64) {
        if self.sta.next_deadline().is_some_and(|d| d <= now) {
            let actions = self.sta.tick(now, &mut self.rng);
            self.station_actions(actions, now);
        }
        self.scan_step(now);
        self.table.expire(now);
        self.maintain(now);
    }

    /// Joins networks automatically and schedules background scans.
    fn maintain(&mut self, now: u64) {
        if !self.radio_usable() || !self.radio_on || self.scan.is_some() {
            return;
        }
        if self.scan_wanted {
            self.start_scan(true, now);
            if self.scan.is_some() {
                return;
            }
        }
        if self.current.is_some() {
            // Joined or joining: only background scans while connected.
            if self.sta.is_connected() && now >= self.next_scan_ms {
                self.start_scan(false, now);
            }
            return;
        }
        if self.request.is_some() {
            self.try_request(now);
        } else if !self.user_disconnected {
            let usable: Vec<Profile> =
                self.profiles.iter().filter(|p| !self.rejected.contains(&p.ssid)).cloned().collect();
            let seen = self.table.fresh(now);
            if let Some((i, bss)) = policy::choose(&usable, &seen, &self.backoff, now) {
                let req = Request::from_profile(&usable[i]);
                let bss = bss.clone();
                self.start_join(req, bss, false, now);
                return;
            }
        }
        if self.current.is_none() && self.scan.is_none() && now >= self.next_scan_ms {
            self.start_scan(false, now);
        }
    }

    /// When the loop must run again (milliseconds).
    pub fn next_deadline(&self, now: u64) -> u64 {
        let mut d = now + MAX_SLEEP_MS;
        if let Some(t) = self.sta.next_deadline() {
            d = d.min(t);
        }
        if let Some(s) = &self.scan {
            d = d.min(s.dwell_until);
        } else if self.radio_usable() && self.radio_on {
            d = d.min(self.next_scan_ms.max(now));
        }
        d.max(now)
    }

    /// Forgets a saved network (leaving it if connected).
    pub fn forget(&mut self, ssid: &[u8], now: u64) -> Result<(), WlanError> {
        let before = self.profiles.len();
        self.profiles.retain(|p| p.ssid != ssid);
        if self.profiles.len() == before {
            return Err(WlanError::NotFound);
        }
        self.rejected.remove(ssid);
        self.backoff.remove(ssid);
        let connected_to_it = self.current.as_ref().is_some_and(|c| c.request.ssid == ssid);
        if connected_to_it {
            self.user_disconnect(now);
            self.user_disconnected = false;
        }
        self.log(format!("forgot {}", ssid_display(ssid)));
        if self.save_profiles() { Ok(()) } else { Err(WlanError::Storage) }
    }

    pub fn set_auto_connect(&mut self, ssid: &[u8], enabled: bool) -> Result<(), WlanError> {
        let p = self.profiles.iter_mut().find(|p| p.ssid == ssid).ok_or(WlanError::NotFound)?;
        p.auto_connect = enabled;
        if self.save_profiles() { Ok(()) } else { Err(WlanError::Storage) }
    }
}
