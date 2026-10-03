//! Tests of the simulated environment, with `vwlan`'s station state machine
//! playing the guest. Every message between the "guest" and the world goes
//! through the radio link encoding, as it does under QEMU.

use std::collections::{BTreeMap, VecDeque};

use vradiolink::{Message, Reader, VERSION};
use vwlan::frame::{self, Mac};
use vwlan::rsn::Security;
use vwlan::scan::{Bss, parse_bss, probe_request};
use vwlan::station::{Action, Credential, Failure, StaEvent, Station, Target};

use crate::control;
use crate::world::{CHANNELS, DEFAULT_GUEST_MAC, Output, SimRandom, World, default_networks};

const PASSWORD: &str = "test-password";
/// QEMU's user-mode network gateway.
const GATEWAY_MAC: Mac = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];

struct Harness {
    now: u64,
    world: World,
    sta: Station,
    rng: SimRandom,
    mac: Mac,
    channel: u8,
    next_id: u32,
    /// Messages from the guest not yet given to the world.
    to_world: VecDeque<Message>,
    /// Access points heard, by BSSID.
    heard: BTreeMap<Mac, Bss>,
    /// Ethernet frames the station passed to its network stack.
    delivered: Vec<Vec<u8>>,
    /// Ethernet frames sent to QEMU's network.
    wired: Vec<Vec<u8>>,
    events: Vec<StaEvent>,
    tx_status: Vec<(u32, bool)>,
    rx_count: usize,
    log: Vec<String>,
}

impl Harness {
    fn new() -> Harness {
        let mut rng = SimRandom::from_seed(1);
        let networks = default_networks(0, &mut rng, PASSWORD);
        let mut world = World::new(networks, SimRandom::from_seed(2));
        world.guest_connected();
        let mut h = Harness {
            now: 0,
            world,
            sta: Station::new([0; 6]),
            rng: SimRandom::from_seed(3),
            mac: [0; 6],
            channel: CHANNELS[0],
            next_id: 1,
            to_world: VecDeque::new(),
            heard: BTreeMap::new(),
            delivered: Vec::new(),
            wired: Vec::new(),
            events: Vec::new(),
            tx_status: Vec::new(),
            rx_count: 0,
            log: Vec::new(),
        };
        h.send(Message::Hello { version: VERSION, mac: [0; 6] });
        assert_eq!(h.mac, DEFAULT_GUEST_MAC);
        h.sta = Station::new(h.mac);
        h.send(Message::SetPower { on: true });
        h
    }

    /// Queues a message from the guest and lets everything settle.
    fn send(&mut self, m: Message) {
        self.to_world.push_back(m);
        self.pump();
    }

    fn pump(&mut self) {
        for _ in 0..10_000 {
            if let Some(m) = self.to_world.pop_front() {
                // Through the wire format, as under QEMU.
                let mut r = Reader::new();
                r.push(&m.encode());
                let m = r.next_message().unwrap().unwrap();
                self.world.guest_message(m, self.now);
            }
            let out = self.world.take_output();
            if out.is_empty() && self.to_world.is_empty() {
                return;
            }
            for o in out {
                self.output(o);
            }
        }
        panic!("messages keep bouncing");
    }

    fn output(&mut self, o: Output) {
        match o {
            Output::Guest(Message::HelloAck { version, mac, channels }) => {
                assert_eq!(version, VERSION);
                assert_eq!(channels, CHANNELS);
                self.mac = mac;
            }
            Output::Guest(Message::Rx { channel, signal_dbm, frame }) => {
                assert_eq!(channel, self.channel, "a frame from another channel was delivered");
                self.rx_count += 1;
                if let Some(b) = parse_bss(&frame, channel, signal_dbm, self.now) {
                    self.heard.insert(b.bssid, b);
                }
                let actions = self.sta.receive(&frame, signal_dbm, self.now, &mut self.rng);
                self.actions(actions);
            }
            Output::Guest(Message::TxStatus { id, acked }) => self.tx_status.push((id, acked)),
            Output::Guest(other) => panic!("unexpected message for the guest: {other:?}"),
            Output::Wired(f) => self.wired.push(f),
            Output::Log(line) => self.log.push(line),
        }
    }

    fn actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Transmit { frame, no_ack } => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.to_world.push_back(Message::Tx { id, no_ack, frame });
                }
                Action::SetChannel(c) => self.tune(c),
                Action::Deliver(eth) => self.delivered.push(eth),
                Action::Event(e) => self.events.push(e),
            }
        }
    }

    fn tune(&mut self, channel: u8) {
        self.channel = channel;
        self.to_world.push_back(Message::SetChannel { channel });
    }

    /// Advances the clock by `ms` milliseconds in 2 ms steps.
    fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now = (self.now + 2).min(end);
            self.world.tick(self.now);
            if self.sta.next_deadline().is_some_and(|d| d <= self.now) {
                let a = self.sta.tick(self.now, &mut self.rng);
                self.actions(a);
            }
            self.pump();
        }
    }

    fn run_until(&mut self, max_ms: u64, mut done: impl FnMut(&Harness) -> bool) -> bool {
        let end = self.now + max_ms;
        while self.now < end {
            if done(self) {
                return true;
            }
            self.run(10);
        }
        done(self)
    }

    fn control(&mut self, line: &str) -> String {
        let answer = control::run(&mut self.world, line, self.now).unwrap_or_else(|e| panic!("{line}: {e}"));
        self.pump();
        answer
    }

    /// Visits every channel, probing and listening for beacons.
    fn scan(&mut self) {
        self.heard.clear();
        for c in CHANNELS {
            self.tune(c);
            let probe = probe_request(&self.mac, None, 1);
            let id = self.next_id;
            self.next_id += 1;
            self.send(Message::Tx { id, no_ack: true, frame: probe });
            self.run(120);
        }
    }

    fn found(&self, ssid: &str) -> Vec<&Bss> {
        let mut v: Vec<&Bss> = self.heard.values().filter(|b| b.ssid == ssid.as_bytes()).collect();
        v.sort_by_key(|b| -(b.signal_dbm as i32));
        v
    }

    fn target(&self, ssid: &str, password: &str) -> Target {
        let bss = self
            .found(ssid)
            .first()
            .map(|b| (*b).clone())
            .or_else(|| self.heard.values().find(|b| b.hidden()).cloned().filter(|_| ssid == "Vindows Hidden"))
            .unwrap_or_else(|| panic!("{ssid} not heard"));
        Target {
            bss,
            ssid: ssid.as_bytes().to_vec(),
            credential: if password.is_empty() { Credential::None } else { Credential::Passphrase(password.into()) },
            allow_sae: true,
        }
    }

    /// Joins `ssid` (after a scan) and returns the event that ended the
    /// attempt.
    fn join(&mut self, ssid: &str, password: &str) -> StaEvent {
        if self.found(ssid).is_empty() {
            self.scan();
        }
        let t = self.target(ssid, password);
        self.events.clear();
        let a = self.sta.connect(t, self.now, &mut self.rng);
        self.actions(a);
        self.pump();
        let finished =
            |h: &Harness| h.events.iter().any(|e| matches!(e, StaEvent::Connected { .. } | StaEvent::JoinFailed(_)));
        assert!(self.run_until(20_000, finished), "joining {ssid} did not finish: {:?}", self.events);
        self.events.iter().find(|e| matches!(e, StaEvent::Connected { .. } | StaEvent::JoinFailed(_))).cloned().unwrap()
    }

    fn joined(&mut self, ssid: &str, password: &str) -> (Mac, Security) {
        match self.join(ssid, password) {
            StaEvent::Connected { bssid, security, .. } => (bssid, security),
            other => panic!("joining {ssid}: {other:?}\n{}", self.log.join("\n")),
        }
    }

    /// An IPv4 frame from the guest to the gateway.
    fn frame_to_gateway(&self, payload: &[u8]) -> Vec<u8> {
        let mut eth = Vec::new();
        eth.extend_from_slice(&GATEWAY_MAC);
        eth.extend_from_slice(&self.mac);
        eth.extend_from_slice(&0x0800u16.to_be_bytes());
        eth.extend_from_slice(payload);
        eth
    }

    /// An IPv4 frame from the gateway to the guest.
    fn frame_from_gateway(&self, payload: &[u8]) -> Vec<u8> {
        let mut eth = Vec::new();
        eth.extend_from_slice(&self.mac);
        eth.extend_from_slice(&GATEWAY_MAC);
        eth.extend_from_slice(&0x0800u16.to_be_bytes());
        eth.extend_from_slice(payload);
        eth
    }

    /// Sends a frame from the guest; returns whether it reached the wired
    /// side unchanged.
    fn send_ethernet(&mut self, eth: &[u8]) -> bool {
        self.wired.clear();
        let a = self.sta.send_ethernet(eth);
        self.actions(a);
        self.pump();
        self.wired.iter().any(|f| f == eth)
    }

    /// Sends a frame from the wired side; returns whether the guest's
    /// network stack received it unchanged.
    fn receive_ethernet(&mut self, eth: &[u8]) -> bool {
        self.delivered.clear();
        self.world.wired_frame(eth.to_vec(), self.now);
        self.pump();
        self.delivered.iter().any(|f| f == eth)
    }

    fn disconnected(&self) -> Option<Failure> {
        self.events.iter().find_map(|e| match e {
            StaEvent::Disconnected(f) => Some(*f),
            _ => None,
        })
    }
}

/// An IPv4 + UDP packet (checksums zero) for the frame helpers.
fn udp4(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let total = (20 + 8 + payload.len()) as u16;
    let mut p = vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 1, 0, 0, 64, 17, 0, 0];
    p.extend_from_slice(&[10, 0, 2, 15, 10, 0, 2, 3]);
    p.extend_from_slice(&src_port.to_be_bytes());
    p.extend_from_slice(&dst_port.to_be_bytes());
    p.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(payload);
    p
}

fn dns_query() -> Vec<u8> {
    let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["example", "com"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

#[test]
fn guest_gets_an_address_and_hears_only_its_channel() {
    let mut h = Harness::new();
    assert_eq!(h.mac, DEFAULT_GUEST_MAC);
    h.tune(1);
    h.run(500);
    // Beacons of `home` (channel 1) only.
    assert!(h.rx_count >= 3, "{} frames", h.rx_count);
    assert!(h.heard.values().all(|b| b.channel == 1));
    assert_eq!(h.heard.len(), 1);
    // A radio that is off hears nothing.
    h.send(Message::SetPower { on: false });
    let before = h.rx_count;
    h.run(500);
    assert_eq!(h.rx_count, before);
    // An unsupported channel is refused (the radio stays where it was).
    h.send(Message::SetPower { on: true });
    h.send(Message::SetChannel { channel: 13 });
    assert_eq!(h.world.radio.channel, 1);
    assert!(h.log.iter().any(|l| l.contains("unsupported channel 13")));
}

#[test]
fn a_guest_provided_address_is_kept_and_group_addresses_are_replaced() {
    let mut h = Harness::new();
    h.send(Message::Hello { version: VERSION, mac: [0x02, 1, 2, 3, 4, 5] });
    assert_eq!(h.mac, [0x02, 1, 2, 3, 4, 5]);
    h.send(Message::Hello { version: VERSION, mac: [0x03, 1, 2, 3, 4, 5] });
    assert_eq!(h.mac, DEFAULT_GUEST_MAC);
    // A hello also switches the radio off until the guest turns it on.
    assert!(!h.world.radio.on);
}

#[test]
fn scanning_finds_the_default_networks() {
    let mut h = Harness::new();
    h.scan();
    let sec = |h: &Harness, ssid: &str| h.found(ssid).first().map(|b| b.security);
    assert_eq!(h.found("Vindows Home").len(), 2, "two access points of Vindows Home");
    assert_eq!(sec(&h, "Vindows Home"), Some(Security::Wpa2Personal));
    assert_eq!(sec(&h, "Vindows WPA3"), Some(Security::Wpa3Personal));
    assert_eq!(sec(&h, "Vindows Mixed"), Some(Security::Wpa2Wpa3Personal));
    assert_eq!(sec(&h, "Vindows Guest"), Some(Security::Open));
    assert_eq!(sec(&h, "Vindows Corp"), Some(Security::Enterprise));
    assert_eq!(h.found("Vindows Home")[0].signal_dbm, -48);
    assert_eq!(h.found("Vindows Home")[0].channel, 1);
    // The hidden network beacons without its name.
    let hidden: Vec<_> = h.heard.values().filter(|b| b.hidden()).collect();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0].channel, 6);
    assert_eq!(h.heard.len(), 7);
}

#[test]
fn joins_every_supported_network_and_passes_traffic() {
    for (ssid, password, expected) in [
        ("Vindows Home", PASSWORD, Security::Wpa2Personal),
        ("Vindows WPA3", PASSWORD, Security::Wpa3Personal),
        ("Vindows Mixed", PASSWORD, Security::Wpa2Wpa3Personal),
        ("Vindows Guest", "", Security::Open),
        ("Vindows Hidden", PASSWORD, Security::Wpa2Personal),
    ] {
        let mut h = Harness::new();
        let (_, security) = h.joined(ssid, password);
        assert_eq!(security, expected, "{ssid}");
        let up = h.frame_to_gateway(&udp4(40000, 80, b"GET / HTTP/1.1"));
        assert!(h.send_ethernet(&up), "{ssid}: frame did not reach the wired side");
        let down = h.frame_from_gateway(&udp4(80, 40000, b"HTTP/1.1 200 OK"));
        assert!(h.receive_ethernet(&down), "{ssid}: frame did not reach the guest");
        // Broadcasts from the wired side (ARP) reach the guest too.
        let mut arp = frame::BROADCAST.to_vec();
        arp.extend_from_slice(&GATEWAY_MAC);
        arp.extend_from_slice(&0x0806u16.to_be_bytes());
        arp.extend_from_slice(&[0; 28]);
        assert!(h.receive_ethernet(&arp), "{ssid}: broadcast did not reach the guest");
        assert!(h.log.iter().any(|l| l.contains("joined")), "{ssid}");
    }
}

#[test]
fn a_wrong_password_is_reported() {
    for ssid in ["Vindows Home", "Vindows WPA3"] {
        let mut h = Harness::new();
        assert_eq!(h.join(ssid, "not-the-password"), StaEvent::JoinFailed(Failure::WrongPassword), "{ssid}");
        // The right one still works afterwards.
        h.joined(ssid, PASSWORD);
    }
}

#[test]
fn enterprise_networks_are_declined_without_transmitting() {
    let mut h = Harness::new();
    h.scan();
    let sent = h.world.stats.guest_tx;
    assert_eq!(h.join("Vindows Corp", PASSWORD), StaEvent::JoinFailed(Failure::Unsupported));
    assert_eq!(h.world.stats.guest_tx, sent);
}

#[test]
fn transmit_status_reports_acknowledgements() {
    let mut h = Harness::new();
    h.tune(1);
    h.run(200);
    let home = h.found("Vindows Home")[0].bssid;
    // A unicast frame to a reachable access point is acknowledged...
    let null = frame::management(frame::mgmt::ACTION, &home, &h.mac, &home, 1, &[127, 0, 0, 0]);
    h.send(Message::Tx { id: 900, no_ack: false, frame: null.clone() });
    assert!(h.tx_status.contains(&(900, true)));
    // ...a frame to an absent one is not...
    let nowhere = frame::management(frame::mgmt::ACTION, &[2, 9, 9, 9, 9, 9], &h.mac, &home, 2, &[127, 0, 0, 0]);
    h.send(Message::Tx { id: 901, no_ack: false, frame: nowhere });
    assert!(h.tx_status.contains(&(901, false)));
    // ...and group frames get no status.
    h.send(Message::Tx { id: 902, no_ack: true, frame: probe_request(&h.mac, None, 3) });
    assert!(!h.tx_status.iter().any(|(id, _)| *id == 902));
    // With the radio off nothing is acknowledged.
    h.send(Message::SetPower { on: false });
    h.send(Message::Tx { id: 903, no_ack: false, frame: null });
    assert!(h.tx_status.contains(&(903, false)));
}

#[test]
fn an_access_point_switched_off_is_noticed_and_its_twin_takes_over() {
    let mut h = Harness::new();
    let (first, _) = h.joined("Vindows Home", PASSWORD);
    assert_eq!(first, [0x02, 0x56, 0x57, 0, 0, 1], "the stronger access point is chosen");
    h.control("ap home off");
    assert!(h.run_until(15_000, |h| h.disconnected().is_some()), "{:?}", h.events);
    assert_eq!(h.disconnected(), Some(Failure::SignalLost));
    // A new scan only finds the second access point of the network.
    h.scan();
    assert_eq!(h.found("Vindows Home").len(), 1);
    let (second, _) = h.joined("Vindows Home", PASSWORD);
    assert_eq!(second, [0x02, 0x56, 0x57, 0, 0, 2]);
    let up = h.frame_to_gateway(&udp4(40000, 443, b"hello"));
    assert!(h.send_ethernet(&up));
    // The first one comes back, freshly started.
    h.control("ap home on");
    h.scan();
    assert_eq!(h.found("Vindows Home").len(), 2);
}

#[test]
fn moving_out_of_range_ends_the_connection() {
    let mut h = Harness::new();
    h.joined("Vindows WPA3", PASSWORD);
    h.control("ap wpa3 signal -95");
    assert!(h.run_until(15_000, |h| h.disconnected().is_some()));
    assert_eq!(h.disconnected(), Some(Failure::SignalLost));
    h.scan();
    assert!(h.found("Vindows WPA3").is_empty());
    h.control("ap wpa3 signal -70");
    h.scan();
    assert_eq!(h.found("Vindows WPA3")[0].signal_dbm, -70);
    h.joined("Vindows WPA3", PASSWORD);
}

#[test]
fn deauthentication_reaches_the_guest_with_its_reason() {
    for (ssid, name) in [("Vindows Home", "home"), ("Vindows WPA3", "wpa3")] {
        let mut h = Harness::new();
        h.joined(ssid, PASSWORD);
        h.control(&format!("ap {name} deauth 3"));
        assert_eq!(h.disconnected(), Some(Failure::Deauthenticated(3)), "{ssid}");
        assert!(h.world.networks[h.world.find(name).unwrap()].ap.stations().is_empty());
        h.joined(ssid, PASSWORD);
    }
}

#[test]
fn group_rekeying_keeps_broadcasts_flowing() {
    let mut h = Harness::new();
    h.joined("Vindows Home", PASSWORD);
    let mut arp = frame::BROADCAST.to_vec();
    arp.extend_from_slice(&GATEWAY_MAC);
    arp.extend_from_slice(&0x0806u16.to_be_bytes());
    arp.extend_from_slice(&[7; 28]);
    for _ in 0..3 {
        h.control("ap home rekey");
        h.run(50);
        assert!(h.receive_ethernet(&arp), "broadcast lost after rekeying");
    }
    assert_eq!(h.disconnected(), None);
}

#[test]
fn a_changed_password_requires_the_new_one() {
    let mut h = Harness::new();
    h.joined("Vindows Home", PASSWORD);
    h.control("ap home password brand-new-password");
    // The restarted access point no longer knows the guest: it answers the
    // guest's next frame with an (unprotected) deauthentication, which the
    // guest verifies with an SA Query that goes unanswered.
    let up = h.frame_to_gateway(&udp4(40000, 80, b"anyone there?"));
    assert!(!h.send_ethernet(&up));
    assert!(h.run_until(15_000, |h| h.disconnected().is_some()), "{:?}", h.events);
    h.control("ap home2 off");
    h.scan();
    assert_eq!(h.join("Vindows Home", PASSWORD), StaEvent::JoinFailed(Failure::WrongPassword));
    h.joined("Vindows Home", "brand-new-password");
}

#[test]
fn the_wired_side_can_go_down_and_come_back() {
    let mut h = Harness::new();
    h.joined("Vindows Guest", "");
    let up = h.frame_to_gateway(&udp4(40000, 80, b"request"));
    let down = h.frame_from_gateway(&udp4(80, 40000, b"answer"));
    h.control("wired down");
    assert!(!h.send_ethernet(&up));
    assert!(!h.receive_ethernet(&down));
    // The Wi-Fi connection itself is unaffected.
    h.run(5000);
    assert_eq!(h.disconnected(), None);
    h.control("wired up");
    assert!(h.send_ethernet(&up));
    assert!(h.receive_ethernet(&down));
}

#[test]
fn dhcp_can_be_withheld() {
    let mut h = Harness::new();
    h.joined("Vindows Home", PASSWORD);
    let mut discover = frame::BROADCAST.to_vec();
    discover.extend_from_slice(&h.mac);
    discover.extend_from_slice(&0x0800u16.to_be_bytes());
    discover.extend_from_slice(&udp4(68, 67, &[1; 240]));
    let other = h.frame_to_gateway(&udp4(40000, 80, b"x"));
    h.control("dhcp off");
    assert!(!h.send_ethernet(&discover));
    assert!(h.send_ethernet(&other), "only DHCP is withheld");
    let mut offer = h.mac.to_vec();
    offer.extend_from_slice(&GATEWAY_MAC);
    offer.extend_from_slice(&0x0800u16.to_be_bytes());
    offer.extend_from_slice(&udp4(67, 68, &[2; 240]));
    assert!(!h.receive_ethernet(&offer));
    h.control("dhcp on");
    assert!(h.send_ethernet(&discover));
    assert!(h.receive_ethernet(&offer));
}

#[test]
fn dns_conditions() {
    let mut h = Harness::new();
    h.joined("Vindows Home", PASSWORD);
    let query = h.frame_to_gateway(&udp4(50000, 53, &dns_query()));
    assert!(h.send_ethernet(&query), "queries pass normally");
    h.control("dns unanswered");
    h.delivered.clear();
    assert!(!h.send_ethernet(&query));
    assert!(h.delivered.is_empty());
    h.control("dns servfail");
    h.delivered.clear();
    assert!(!h.send_ethernet(&query));
    // The guest gets a SERVFAIL answer from the address of the DNS server.
    let answer = h.delivered.last().expect("an answer");
    assert_eq!(answer[0..6], h.mac);
    assert_eq!(answer[6..12], GATEWAY_MAC);
    assert_eq!(answer[26..30], [10, 0, 2, 3]);
    assert_eq!(answer[42 + 3] & 0x0F, 2);
    h.control("dns normal");
    assert!(h.send_ethernet(&query));
}

#[test]
fn joining_succeeds_on_a_lossy_link() {
    // Retransmissions on both sides carry the handshakes through moderate
    // frame loss.
    let mut h = Harness::new();
    h.scan();
    h.control("ap home loss 15");
    let mut connected = false;
    for _ in 0..4 {
        if matches!(h.join("Vindows Home", PASSWORD), StaEvent::Connected { .. }) {
            connected = true;
            break;
        }
    }
    assert!(connected, "{:?}", h.events);
    assert!(h.world.stats.lost > 0);
    // Total loss is noticed as a lost signal.
    h.control("ap home loss 100");
    assert!(h.run_until(15_000, |h| h.disconnected().is_some()));
    assert_eq!(h.disconnected(), Some(Failure::SignalLost));
}

#[test]
fn a_guest_that_goes_away_is_dropped_by_its_access_point() {
    let mut h = Harness::new();
    h.joined("Vindows Mixed", PASSWORD);
    let i = h.world.find("mixed").unwrap();
    assert_eq!(h.world.networks[i].ap.authorized(), vec![h.mac]);
    h.world.guest_disconnected(h.now);
    h.pump();
    assert!(h.world.networks[i].ap.stations().is_empty());
    // A fresh driver starts over and can join again.
    h.world.guest_connected();
    h.send(Message::Hello { version: VERSION, mac: [0; 6] });
    h.send(Message::SetPower { on: true });
    h.sta = Station::new(h.mac);
    h.joined("Vindows Mixed", PASSWORD);
}

#[test]
fn control_commands_are_validated() {
    let mut h = Harness::new();
    let mut err = |line: &str| control::run(&mut h.world, line, 0).unwrap_err();
    assert!(err("ap nowhere off").contains("no access point"));
    assert!(err("ap home signal loud").contains("bad signal"));
    assert!(err("ap home signal 5").contains("below -10"));
    assert!(err("ap home loss 101").contains("0 to 100"));
    assert!(err("ap guest password whatever1").contains("open network"));
    assert!(err("ap home password short").contains("8 to 63"));
    assert!(err("dns sometimes").contains("unknown DNS mode"));
    assert!(err("wired sideways").contains("expected up or down"));
    assert!(err("frobnicate").contains("unknown command"));
    assert!(err("ap home").contains("unknown access point command"));
    let list = h.control("list");
    assert_eq!(list.lines().count(), 7);
    assert!(list.contains("Vindows Hidden") && list.contains("hidden"));
    assert!(h.control("status").contains("radio connected"));
    assert!(h.control("help").contains("dns normal"));
    assert_eq!(h.control("ap HOME loss 5%"), "");
    assert_eq!(h.world.networks[0].loss_pct, 5);
}

#[test]
fn malformed_frames_from_the_guest_are_harmless() {
    let mut h = Harness::new();
    h.joined("Vindows Home", PASSWORD);
    let mut s = 0x2545_F491u32;
    for i in 0..3000u32 {
        let len = (s % 300) as usize;
        let mut f = Vec::with_capacity(len);
        for _ in 0..len {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            f.push(s as u8);
        }
        // Address half of them to the access point, so they get past its
        // address check.
        if f.len() >= 22 && i % 2 == 0 {
            f[4..10].copy_from_slice(&[0x02, 0x56, 0x57, 0, 0, 1]);
            f[16..22].copy_from_slice(&[0x02, 0x56, 0x57, 0, 0, 1]);
        }
        h.send(Message::Tx { id: i, no_ack: false, frame: f });
    }
    // Still connected and working.
    let up = h.frame_to_gateway(&udp4(40000, 80, b"still here"));
    assert!(h.send_ethernet(&up));
}
