//! End-to-end tests: a [`Station`] and an [`AccessPoint`] joined by a
//! simulated radio channel, with a millisecond clock.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ap::{AccessPoint, ApAction, ApConfig, ApEvent};
use crate::frame::{self, Mac, mgmt, reason};
use crate::handshake::tests::TestRandom;
use crate::rsn::Security;
use crate::scan::parse_bss;
use crate::station::{Action, Credential, Failure, StaEvent, Station, Target};

const STA_MAC: Mac = [0x02, 0x00, 0x00, 0x57, 0x4C, 0x01];
const AP_MAC: Mac = [0x02, 0x00, 0x00, 0xA9, 0x00, 0x01];
const HOST_MAC: Mac = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];

struct Sim {
    now: u64,
    sta: Station,
    ap: AccessPoint,
    rng: TestRandom,
    sta_channel: u8,
    /// Frames get through.
    air: bool,
    to_ap: VecDeque<Vec<u8>>,
    to_sta: VecDeque<Vec<u8>>,
    /// Ethernet frames the station delivered to its network stack.
    delivered: Vec<Vec<u8>>,
    /// Ethernet frames the AP sent to the wired side.
    uplink: Vec<Vec<u8>>,
    events: Vec<StaEvent>,
    ap_events: Vec<ApEvent>,
    /// Every frame the AP transmitted (for replay tests).
    ap_frames: Vec<Vec<u8>>,
}

fn ap_config(security: Security, pass: &str) -> ApConfig {
    ApConfig {
        bssid: AP_MAC,
        ssid: b"VindowsNet".to_vec(),
        channel: 6,
        security,
        passphrase: if pass.is_empty() { None } else { Some(String::from(pass)) },
        hidden: false,
        pmf_required: false,
        h2e: false,
        beacon_interval: 100,
    }
}

impl Sim {
    fn new(cfg: ApConfig) -> Sim {
        let mut rng = TestRandom(17);
        let ap = AccessPoint::new(cfg, 0, &mut rng);
        Sim {
            now: 0,
            sta: Station::new(STA_MAC),
            ap,
            rng,
            sta_channel: 1,
            air: true,
            to_ap: VecDeque::new(),
            to_sta: VecDeque::new(),
            delivered: Vec::new(),
            uplink: Vec::new(),
            events: Vec::new(),
            ap_events: Vec::new(),
            ap_frames: Vec::new(),
        }
    }

    fn sta_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Transmit { frame, .. } => {
                    if self.air && self.sta_channel == self.ap.cfg.channel {
                        self.to_ap.push_back(frame);
                    }
                }
                Action::SetChannel(c) => self.sta_channel = c,
                Action::Deliver(eth) => self.delivered.push(eth),
                Action::Event(e) => self.events.push(e),
            }
        }
    }

    fn ap_actions(&mut self, actions: Vec<ApAction>) {
        for a in actions {
            match a {
                ApAction::Transmit { frame, .. } => {
                    self.ap_frames.push(frame.clone());
                    if self.air && self.sta_channel == self.ap.cfg.channel {
                        self.to_sta.push_back(frame);
                    }
                }
                ApAction::Uplink(eth) => self.uplink.push(eth),
                ApAction::Event(e) => self.ap_events.push(e),
            }
        }
    }

    fn pump(&mut self) {
        for _ in 0..1000 {
            if let Some(f) = self.to_ap.pop_front() {
                let a = self.ap.receive(&f, self.now, &mut self.rng);
                self.ap_actions(a);
            } else if let Some(f) = self.to_sta.pop_front() {
                let a = self.sta.receive(&f, -50, self.now, &mut self.rng);
                self.sta_actions(a);
            } else {
                return;
            }
        }
        panic!("frames keep bouncing");
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.now += 1;
            let a = self.ap.tick(self.now);
            self.ap_actions(a);
            let a = self.sta.tick(self.now, &mut self.rng);
            self.sta_actions(a);
            self.pump();
        }
    }

    fn run_until(&mut self, max_ms: u64, mut f: impl FnMut(&Sim) -> bool) -> bool {
        for _ in 0..max_ms {
            if f(self) {
                return true;
            }
            self.run(1);
        }
        f(self)
    }

    fn target(&mut self, pass: &str, allow_sae: bool) -> Target {
        let beacon = self.ap.beacon(self.now);
        let bss = parse_bss(&beacon, self.ap.cfg.channel, -50, self.now).expect("beacon parses");
        Target {
            bss,
            ssid: self.ap.cfg.ssid.clone(),
            credential: if pass.is_empty() { Credential::None } else { Credential::Passphrase(String::from(pass)) },
            allow_sae,
        }
    }

    fn connect(&mut self, pass: &str, allow_sae: bool) {
        let t = self.target(pass, allow_sae);
        let a = self.sta.connect(t, self.now, &mut self.rng);
        self.sta_actions(a);
        self.pump();
    }

    fn connected(&self) -> bool {
        self.events.iter().any(|e| matches!(e, StaEvent::Connected { .. }))
    }

    /// Sends an IPv4-looking frame from the station to the host and one
    /// back, and checks both arrive intact.
    fn exchange_data(&mut self) {
        let mut eth = Vec::new();
        eth.extend_from_slice(&HOST_MAC);
        eth.extend_from_slice(&STA_MAC);
        eth.extend_from_slice(&0x0800u16.to_be_bytes());
        eth.extend_from_slice(b"hello from the station");
        let a = self.sta.send_ethernet(&eth);
        self.sta_actions(a);
        self.pump();
        assert_eq!(self.uplink.last(), Some(&eth), "uplink frame missing");
        let mut back = Vec::new();
        back.extend_from_slice(&STA_MAC);
        back.extend_from_slice(&HOST_MAC);
        back.extend_from_slice(&0x0800u16.to_be_bytes());
        back.extend_from_slice(b"hello from the host");
        let a = self.ap.downlink(&back);
        self.ap_actions(a);
        self.pump();
        assert_eq!(self.delivered.last(), Some(&back), "downlink frame missing");
    }

    fn broadcast_from_host(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&frame::BROADCAST);
        b.extend_from_slice(&HOST_MAC);
        b.extend_from_slice(&0x0806u16.to_be_bytes());
        b.extend_from_slice(b"who has 10.0.2.15");
        let a = self.ap.downlink(&b);
        self.ap_actions(a);
        self.pump();
        b
    }
}

#[test]
fn open_network() {
    let mut s = Sim::new(ap_config(Security::Open, ""));
    s.connect("", true);
    assert!(s.run_until(2000, |s| s.connected()), "events: {:?}", s.events);
    s.exchange_data();
    let b = s.broadcast_from_host();
    assert_eq!(s.delivered.last(), Some(&b));
}

#[test]
fn wpa2_personal() {
    let mut s = Sim::new(ap_config(Security::Wpa2Personal, "correct horse battery"));
    s.connect("correct horse battery", true);
    assert!(s.run_until(5000, |s| s.connected()), "events: {:?}", s.events);
    assert!(s.events.contains(&StaEvent::Securing));
    assert!(matches!(
        s.events.last(),
        Some(StaEvent::Connected { security: Security::Wpa2Personal, pmf: true, sae: false, .. })
    ));
    s.exchange_data();
    let b = s.broadcast_from_host();
    assert_eq!(s.delivered.last(), Some(&b));
    // A new group key: broadcasts keep arriving.
    let a = s.ap.rekey_group(&mut s.rng, s.now);
    s.ap_actions(a);
    s.pump();
    s.run(10);
    let b2 = s.broadcast_from_host();
    assert_eq!(s.delivered.last(), Some(&b2));
    assert_eq!(s.sta.counters.group_rekeys, 1);
}

#[test]
fn wpa3_personal_both_password_element_methods() {
    for h2e in [false, true] {
        let mut cfg = ap_config(Security::Wpa3Personal, "bright blue sky");
        cfg.h2e = h2e;
        let mut s = Sim::new(cfg);
        s.connect("bright blue sky", true);
        assert!(s.run_until(8000, |s| s.connected()), "h2e={h2e}: {:?}", s.events);
        assert!(matches!(
            s.events.last(),
            Some(StaEvent::Connected { security: Security::Wpa3Personal, pmf: true, sae: true, .. })
        ));
        s.exchange_data();
    }
}

#[test]
fn transition_mode_prefers_wpa3() {
    let mut s = Sim::new(ap_config(Security::Wpa2Wpa3Personal, "mixed mode pass"));
    s.connect("mixed mode pass", true);
    assert!(s.run_until(8000, |s| s.connected()), "{:?}", s.events);
    assert!(matches!(s.events.last(), Some(StaEvent::Connected { sae: true, .. })));
    s.exchange_data();
    // With WPA3 not allowed for this network, WPA2 is used.
    let mut s = Sim::new(ap_config(Security::Wpa2Wpa3Personal, "mixed mode pass"));
    s.connect("mixed mode pass", false);
    assert!(s.run_until(8000, |s| s.connected()), "{:?}", s.events);
    assert!(matches!(s.events.last(), Some(StaEvent::Connected { sae: false, .. })));
    s.exchange_data();
}

#[test]
fn wrong_passwords_are_reported() {
    let mut s = Sim::new(ap_config(Security::Wpa2Personal, "the right one"));
    s.connect("the wrong one", true);
    assert!(s.run_until(20_000, |s| s.events.iter().any(|e| matches!(e, StaEvent::JoinFailed(_)))));
    assert!(s.events.contains(&StaEvent::JoinFailed(Failure::WrongPassword)), "{:?}", s.events);
    assert!(s.ap.authorized().is_empty());
    let mut s = Sim::new(ap_config(Security::Wpa3Personal, "the right one"));
    s.connect("the wrong one", true);
    assert!(s.run_until(20_000, |s| s.events.iter().any(|e| matches!(e, StaEvent::JoinFailed(_)))));
    assert!(s.events.contains(&StaEvent::JoinFailed(Failure::WrongPassword)), "{:?}", s.events);
}

#[test]
fn a_silent_access_point_is_detected() {
    let mut s = Sim::new(ap_config(Security::Wpa2Personal, "password123"));
    s.connect("password123", true);
    assert!(s.run_until(5000, |s| s.connected()));
    s.air = false;
    assert!(s.run_until(10_000, |s| s.events.contains(&StaEvent::Disconnected(Failure::SignalLost))), "{:?}", s.events);
    assert!(s.sta.is_idle());
}

#[test]
fn deauthentication_with_and_without_protection() {
    let mut s = Sim::new(ap_config(Security::Wpa2Personal, "password123"));
    s.connect("password123", true);
    assert!(s.run_until(5000, |s| s.connected()));
    // An unprotected deauthentication claiming to come from the AP: the
    // station asks the AP (SA Query), gets an answer, and stays.
    let fake = frame::management(mgmt::DEAUTH, &STA_MAC, &AP_MAC, &AP_MAC, 99, &reason::UNSPECIFIED.to_le_bytes());
    let a = s.sta.receive(&fake, -40, s.now, &mut s.rng);
    s.sta_actions(a);
    s.pump();
    s.run(2000);
    assert!(s.sta.is_connected());
    assert!(s.sta.counters.unprotected_dropped >= 1);
    s.exchange_data();
    // The AP really disconnects the station (protected frame).
    let a = s.ap.deauthenticate(Some(STA_MAC), reason::DISASSOC_AP_BUSY);
    s.ap_actions(a);
    s.pump();
    assert!(
        s.events.contains(&StaEvent::Disconnected(Failure::Deauthenticated(reason::DISASSOC_AP_BUSY))),
        "{:?}",
        s.events
    );
}

#[test]
fn replayed_frames_are_dropped() {
    let mut s = Sim::new(ap_config(Security::Wpa2Personal, "password123"));
    s.connect("password123", true);
    assert!(s.run_until(5000, |s| s.connected()));
    s.exchange_data();
    let delivered = s.delivered.len();
    // Replay the AP's last frame (the downlink data frame).
    let last = s.ap_frames.last().unwrap().clone();
    let a = s.sta.receive(&last, -50, s.now, &mut s.rng);
    s.sta_actions(a);
    assert_eq!(s.delivered.len(), delivered);
    assert!(s.sta.counters.replays + s.sta.counters.duplicates >= 1);
    // A frame with a flipped bit is dropped too.
    let mut bad = last.clone();
    let n = bad.len();
    bad[n - 3] ^= 0x10;
    let a = s.sta.receive(&bad, -50, s.now, &mut s.rng);
    s.sta_actions(a);
    assert_eq!(s.delivered.len(), delivered);
}

#[test]
fn hidden_network() {
    let mut cfg = ap_config(Security::Wpa2Personal, "hidden pass");
    cfg.hidden = true;
    let mut s = Sim::new(cfg);
    let t = s.target("hidden pass", true);
    assert!(t.bss.hidden());
    let a = s.sta.connect(t, s.now, &mut s.rng);
    s.sta_actions(a);
    assert!(s.run_until(5000, |s| s.connected()), "{:?}", s.events);
}

#[test]
fn unsupported_and_missing_credentials() {
    let mut s = Sim::new(ap_config(Security::Wpa2Personal, "password123"));
    s.connect("", true);
    assert!(s.events.contains(&StaEvent::JoinFailed(Failure::PasswordRequired)));
    let mut t = s.target("x", true);
    t.bss.security = Security::Wep;
    let a = s.sta.connect(t, s.now, &mut s.rng);
    s.sta_actions(a);
    assert!(s.events.contains(&StaEvent::JoinFailed(Failure::Unsupported)));
}

#[test]
fn reconnects_after_disconnecting() {
    let mut s = Sim::new(ap_config(Security::Wpa3Personal, "again and again"));
    for round in 0..3 {
        s.events.clear();
        s.connect("again and again", true);
        assert!(s.run_until(8000, |s| s.connected()), "round {round}: {:?}", s.events);
        s.exchange_data();
        let a = s.sta.disconnect(s.now);
        s.sta_actions(a);
        s.pump();
        assert!(s.sta.is_idle());
        assert!(s.ap.stations().is_empty(), "the AP still lists the station");
    }
}
