//! End-to-end tests: a [`Station`] and an [`AccessPoint`] joined by a
//! simulated radio channel, with a millisecond clock. A managed station's
//! radio is simulated too ([`Mlme`]): it makes the frames of the station's
//! commands, and protects and checks frames with the keys it is given.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use crate::ap::{AccessPoint, ApAction, ApConfig, ApEvent};
use crate::ccmp;
use crate::eapol;
use crate::frame::{self, Header, Mac, capab, is_group, mgmt, reason};
use crate::handshake::tests::TestRandom;
use crate::ie;
use crate::rsn::Security;
use crate::scan::parse_bss;
use crate::station::{Action, Credential, Failure, KeyKind, StaEvent, Station, Target};

const STA_MAC: Mac = [0x02, 0x00, 0x00, 0x57, 0x4C, 0x01];
const AP_MAC: Mac = [0x02, 0x00, 0x00, 0xA9, 0x00, 0x01];
const HOST_MAC: Mac = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];

/// A managed radio, as far as the tests need one: Linux's mac80211, say.
#[derive(Default)]
struct Mlme {
    tk: Option<[u8; 16]>,
    tx_pn: u64,
    gtk: BTreeMap<u8, [u8; 16]>,
    seq: u16,
    authorized: bool,
    /// The commands the station gave, by name, in order.
    commands: Vec<&'static str>,
}

impl Mlme {
    fn next_seq(&mut self) -> u16 {
        self.seq = (self.seq + 1) & 0xFFF;
        self.seq
    }

    /// A frame from the station, encrypted if `encrypt`.
    fn protect(&mut self, plain: Vec<u8>, encrypt: bool) -> Option<Vec<u8>> {
        match (encrypt, self.tk) {
            (true, Some(tk)) => {
                self.tx_pn += 1;
                ccmp::ccmp_encrypt(&tk, &plain, self.tx_pn, 0)
            }
            (true, None) => None,
            (false, _) => Some(plain),
        }
    }

    /// A data frame of the station's: Ethernet as 802.11.
    fn data(&mut self, bssid: &Mac, eth: &[u8], encrypt: bool) -> Option<Vec<u8>> {
        let (da, sa) = (eth[0..6].try_into().ok()?, eth[6..12].try_into().ok()?);
        let seq = self.next_seq();
        let f = frame::data_to_ds(bssid, &sa, &da, seq, u16::from_be_bytes([eth[12], eth[13]]), &eth[14..]);
        self.protect(f, encrypt)
    }
}

/// What the managed radio hands the station of a frame from the AP.
enum Up {
    Frame(Vec<u8>),
    Ethernet(Vec<u8>),
}

struct Sim {
    now: u64,
    sta: Station,
    /// The station's radio is managed.
    mlme: Option<Mlme>,
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
        ssid: b"VedaNet".to_vec(),
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
            mlme: None,
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

    /// A station of a managed radio, which follows the AP's channel.
    fn managed(cfg: ApConfig) -> Sim {
        let mut s = Sim::new(cfg);
        s.sta = Station::managed(STA_MAC);
        s.mlme = Some(Mlme::default());
        s.sta_channel = s.ap.cfg.channel;
        s
    }

    fn sta_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Transmit { frame, .. } => {
                    assert!(self.mlme.is_none(), "a managed station transmits frames itself");
                    if self.air && self.sta_channel == self.ap.cfg.channel {
                        self.to_ap.push_back(frame);
                    }
                }
                Action::SetChannel(c) => {
                    assert!(self.mlme.is_none(), "a managed station tunes the radio");
                    self.sta_channel = c;
                }
                Action::Deliver(eth) => self.delivered.push(eth),
                Action::Event(e) => self.events.push(e),
                managed => self.mlme_command(managed),
            }
        }
    }

    /// The managed radio carries out a command of the station's.
    fn mlme_command(&mut self, a: Action) {
        let air = self.air;
        let m = self.mlme.as_mut().expect("a soft MAC station gave a managed radio's command");
        let out = match a {
            Action::Authenticate { bssid, body, .. } => {
                m.commands.push("authenticate");
                let seq = m.next_seq();
                Some(frame::management(mgmt::AUTH, &bssid, &STA_MAC, &bssid, seq, &body))
            }
            Action::Associate { bssid, ssid, ies, pmf, .. } => {
                m.commands.push(if pmf { "associate (pmf)" } else { "associate" });
                let elements = ie::Builder::new().ssid(&ssid).rates(&ie::RATES_G).raw(&ies).build();
                let cap = capab::ESS | if ies.is_empty() { 0 } else { capab::PRIVACY };
                let seq = m.next_seq();
                let body = frame::AssocReqBody::build(cap, 10, &elements);
                Some(frame::management(mgmt::ASSOC_REQ, &bssid, &STA_MAC, &bssid, seq, &body))
            }
            Action::Deauthenticate { bssid, reason } => {
                m.commands.push("deauthenticate");
                let seq = m.next_seq();
                let f = frame::management(mgmt::DEAUTH, &bssid, &STA_MAC, &bssid, seq, &reason.to_le_bytes());
                let encrypt = m.authorized;
                m.protect(f, encrypt)
            }
            Action::SendEapol { peer, frame, encrypt } => {
                m.commands.push(if encrypt { "eapol (encrypted)" } else { "eapol" });
                let mut eth = Vec::new();
                eth.extend_from_slice(&peer);
                eth.extend_from_slice(&STA_MAC);
                eth.extend_from_slice(&eapol::ETHERTYPE.to_be_bytes());
                eth.extend_from_slice(&frame);
                m.data(&peer, &eth, encrypt)
            }
            Action::InstallKey { kind, index, key, .. } => {
                m.commands.push(match kind {
                    KeyKind::Pairwise => "pairwise key",
                    KeyKind::Group => "group key",
                    KeyKind::Integrity => "integrity key",
                });
                if let Ok(k) = <[u8; 16]>::try_from(key.as_slice()) {
                    match kind {
                        KeyKind::Pairwise => m.tk = Some(k),
                        KeyKind::Group => {
                            m.gtk.insert(index, k);
                        }
                        KeyKind::Integrity => {}
                    }
                }
                None
            }
            Action::Authorize { .. } => {
                m.commands.push("authorize");
                m.authorized = true;
                None
            }
            Action::SendManagement(f) => {
                m.commands.push("management");
                let encrypt = m.tk.is_some();
                m.protect(f, encrypt)
            }
            Action::SendEthernet(eth) => {
                let bssid = self.ap.cfg.bssid;
                let protected = self.ap.cfg.security != Security::Open;
                if protected && !m.authorized { None } else { m.data(&bssid, &eth, protected) }
            }
            _ => None,
        };
        if let Some(f) = out
            && air
        {
            self.to_ap.push_back(f);
        }
    }

    /// What the managed radio makes of a frame from the AP: management
    /// frames the station needs, checked; data as Ethernet, decrypted.
    fn mlme_receive(&mut self, f: &[u8]) -> Option<Up> {
        let m = self.mlme.as_mut()?;
        let h = Header::parse(f)?;
        let plain = if h.fc.protected() {
            let (_, key_id) = ccmp::ccmp_header(f)?;
            let key = if is_group(&h.addr1) { *m.gtk.get(&key_id)? } else { m.tk? };
            ccmp::ccmp_decrypt(&key, f).ok()?.0
        } else {
            f.to_vec()
        };
        if h.fc.is_management() {
            return match h.fc.subtype() {
                mgmt::BEACON | mgmt::PROBE_RESP => None,
                _ => Some(Up::Frame(plain)),
            };
        }
        let (ethertype, payload) = frame::parse_snap(plain.get(h.len..)?)?;
        if !h.fc.protected() && ethertype != eapol::ETHERTYPE && self.ap.cfg.security != Security::Open {
            return None;
        }
        let mut eth = Vec::new();
        eth.extend_from_slice(&h.addr1);
        eth.extend_from_slice(&h.addr3);
        eth.extend_from_slice(&ethertype.to_be_bytes());
        eth.extend_from_slice(payload);
        Some(Up::Ethernet(eth))
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
                let a = match self.mlme.is_some() {
                    false => self.sta.receive(&f, -50, self.now, &mut self.rng),
                    true => match self.mlme_receive(&f) {
                        Some(Up::Frame(f)) => self.sta.receive(&f, -50, self.now, &mut self.rng),
                        Some(Up::Ethernet(eth)) => self.sta.receive_ethernet(&eth, self.now, &mut self.rng),
                        None => Vec::new(),
                    },
                };
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

#[test]
fn random_and_altered_frames_leave_the_connection_intact() {
    for security in [Security::Wpa2Personal, Security::Wpa3Personal] {
        let mut cfg = ap_config(security, "sturdy password");
        cfg.pmf_required = security == Security::Wpa3Personal;
        let mut s = Sim::new(cfg);
        s.connect("sturdy password", true);
        assert!(s.run_until(8000, |s| s.connected()), "{security:?}: {:?}", s.events);
        s.exchange_data();
        let captured = s.ap_frames.clone();
        let mut x = 0x2468_ACE1u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for i in 0..4000u32 {
            let mut f: Vec<u8> = if i % 3 == 0 {
                let len = (next() % 400) as usize;
                (0..len).map(|_| next() as u8).collect()
            } else {
                // A copy of something the access point sent, with a few bits
                // flipped and sometimes cut short.
                let mut f = captured[next() as usize % captured.len()].clone();
                for _ in 0..1 + next() % 4 {
                    if !f.is_empty() {
                        let p = next() as usize % f.len();
                        f[p] ^= 1 << (next() % 8);
                    }
                }
                if i % 5 == 0 {
                    let keep = next() as usize % (f.len() + 1);
                    f.truncate(keep);
                }
                f
            };
            // Half of them appear to come from the access point to us.
            if i % 2 == 0 && f.len() >= 16 {
                f[4..10].copy_from_slice(&STA_MAC);
                f[10..16].copy_from_slice(&AP_MAC);
            }
            let a = s.sta.receive(&f, -50, s.now, &mut s.rng);
            s.sta_actions(a);
            s.pump();
        }
        assert!(!s.events.iter().any(|e| matches!(e, StaEvent::Disconnected(_))), "{security:?}: {:?}", s.events);
        assert!(s.sta.counters.decrypt_errors > 0, "altered protected frames were rejected");
        s.run(2000);
        s.exchange_data();
    }
}

#[test]
fn managed_radio_wpa2() {
    let mut s = Sim::managed(ap_config(Security::Wpa2Personal, "correct horse battery"));
    s.connect("correct horse battery", true);
    assert!(s.run_until(5000, |s| s.connected()), "events: {:?}", s.events);
    assert!(matches!(s.events.last(), Some(StaEvent::Connected { security: Security::Wpa2Personal, pmf: true, .. })));
    // The radio associated with the station's RSN element, sent messages
    // 2 and 4 in clear and got the keys after them, and opened the port.
    let commands = &s.mlme.as_ref().unwrap().commands;
    assert_eq!(
        commands[..],
        [
            "authenticate",
            "associate (pmf)",
            "eapol",
            "eapol",
            "pairwise key",
            "group key",
            "integrity key",
            "authorize"
        ],
    );
    s.exchange_data();
    let b = s.broadcast_from_host();
    assert_eq!(s.delivered.last(), Some(&b));
    // A new group key: the station answers encrypted, the radio gets it.
    let a = s.ap.rekey_group(&mut s.rng, s.now);
    s.ap_actions(a);
    s.pump();
    s.run(10);
    let b2 = s.broadcast_from_host();
    assert_eq!(s.delivered.last(), Some(&b2));
    assert!(s.mlme.as_ref().unwrap().commands.contains(&"eapol (encrypted)"));
    assert_eq!(s.sta.counters.group_rekeys, 1);
}

#[test]
fn managed_radio_wpa3_and_open() {
    for h2e in [false, true] {
        let mut cfg = ap_config(Security::Wpa3Personal, "bright blue sky");
        cfg.h2e = h2e;
        let mut s = Sim::managed(cfg);
        s.connect("bright blue sky", true);
        assert!(s.run_until(8000, |s| s.connected()), "h2e={h2e}: {:?}", s.events);
        assert!(matches!(s.events.last(), Some(StaEvent::Connected { sae: true, pmf: true, .. })));
        // SAE: the commit and the confirm, through the radio.
        let commands = &s.mlme.as_ref().unwrap().commands;
        assert_eq!(commands.iter().filter(|c| **c == "authenticate").count(), 2);
        s.exchange_data();
    }
    let mut s = Sim::managed(ap_config(Security::Open, ""));
    s.connect("", true);
    assert!(s.run_until(2000, |s| s.connected()), "events: {:?}", s.events);
    assert_eq!(s.mlme.as_ref().unwrap().commands[..], ["authenticate", "associate"]);
    s.exchange_data();
}

#[test]
fn managed_radio_failures() {
    // A wrong password: the AP never sends message 3.
    let mut s = Sim::managed(ap_config(Security::Wpa2Personal, "correct horse battery"));
    s.connect("wrong password!", true);
    assert!(s.run_until(10_000, |s| s.events.iter().any(|e| matches!(e, StaEvent::JoinFailed(_)))));
    assert!(s.events.contains(&StaEvent::JoinFailed(Failure::WrongPassword)), "{:?}", s.events);
    // The radio says the AP did not answer.
    let mut s = Sim::managed(ap_config(Security::Wpa2Personal, "correct horse battery"));
    s.air = false;
    s.connect("correct horse battery", true);
    let a = s.sta.mlme_timeout(s.now);
    s.sta_actions(a);
    assert_eq!(s.events.last(), Some(&StaEvent::JoinFailed(Failure::NoResponse)));
    // ...and if it never says, the station gives up anyway.
    let mut s = Sim::managed(ap_config(Security::Open, ""));
    s.air = false;
    s.connect("", true);
    assert!(s.run_until(10_000, |s| s.events.iter().any(|e| matches!(e, StaEvent::JoinFailed(_)))));
    // A lost link ends a connection; leaving deauthenticates through the
    // radio.
    let mut s = Sim::managed(ap_config(Security::Wpa2Personal, "correct horse battery"));
    s.connect("correct horse battery", true);
    assert!(s.run_until(5000, |s| s.connected()));
    let a = s.sta.link_lost(s.now);
    s.sta_actions(a);
    assert_eq!(s.events.last(), Some(&StaEvent::Disconnected(Failure::SignalLost)));
    let mut s = Sim::managed(ap_config(Security::Wpa2Personal, "correct horse battery"));
    s.connect("correct horse battery", true);
    assert!(s.run_until(5000, |s| s.connected()));
    let a = s.sta.disconnect(s.now);
    s.sta_actions(a);
    s.pump();
    assert_eq!(s.mlme.as_ref().unwrap().commands.last(), Some(&"deauthenticate"));
    assert_eq!(s.events.last(), Some(&StaEvent::Disconnected(Failure::Local)));
}
