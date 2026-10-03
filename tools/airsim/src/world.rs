//! The simulated Wi-Fi environment: access points, the radio of the
//! Vindows guest, and the wired network behind the access points.
//!
//! [`World`] has no I/O. `main.rs` feeds it messages from the guest's radio
//! (over the QEMU virtio-serial port), Ethernet frames from QEMU's user-mode
//! network and timer ticks, and carries out the returned [`Output`]s. That
//! keeps the whole simulation testable on the host (see `tests.rs`).
//!
//! The radio medium is simple: a frame sent by the guest on channel *c*
//! reaches every access point that is switched on, tuned to *c* and in range;
//! a frame sent by such an access point reaches the guest if its radio is on
//! and tuned to *c*. Each access point has a signal level and a frame loss
//! rate that tests can change at run time, as can the state of the wired
//! network (see [`crate::control`]).

use std::fmt::Write as _;

use vradiolink::{Message, VERSION};
use vwlan::ap::{AccessPoint, ApAction, ApConfig, ApEvent};
use vwlan::crypto::Random;
use vwlan::frame::Mac;
use vwlan::rsn::Security;

/// Channels the simulated radio offers (2.4 GHz 1, 6, 11 and 5 GHz 36, 44).
pub const CHANNELS: [u8; 5] = [1, 6, 11, 36, 44];

/// Below this signal level frames are not received at all.
pub const SENSITIVITY_DBM: i8 = -92;

/// Randomness for the access points (nonces, group keys) and for frame loss.
pub struct SimRandom(ventropy::Generator);

impl SimRandom {
    /// A generator seeded from the operating system (through the standard
    /// library's randomly keyed hasher), the time and the process ID.
    pub fn from_os() -> SimRandom {
        use std::hash::{BuildHasher, Hasher};
        let mut g = ventropy::Generator::new();
        for i in 0..4u64 {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(i);
            g.mix(&h.finish().to_le_bytes());
        }
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        g.mix(&t.as_nanos().to_le_bytes());
        g.mix(&std::process::id().to_le_bytes());
        g.reseed();
        SimRandom(g)
    }

    /// A reproducible generator (for tests and `--seed`).
    pub fn from_seed(seed: u64) -> SimRandom {
        let mut g = ventropy::Generator::new();
        g.mix(&seed.to_le_bytes());
        g.reseed();
        SimRandom(g)
    }

    /// A number in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u32) -> u32 {
        (self.0.next_u64() % n as u64) as u32
    }
}

impl Random for SimRandom {
    fn fill(&mut self, buf: &mut [u8]) {
        self.0.fill(buf);
    }
}

/// How the wired network treats DNS queries from the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsMode {
    /// Forwarded to QEMU's DNS proxy (normal operation).
    Normal,
    /// Not answered (a DNS server that is down).
    Unanswered,
    /// Answered with SERVFAIL (a DNS server that cannot resolve).
    ServFail,
}

/// The state of the wired network behind the access points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wired {
    /// Frames pass between the access points and QEMU's network.
    pub up: bool,
    /// DHCP (and DHCPv6) messages pass.
    pub dhcp: bool,
    pub dns: DnsMode,
}

impl Default for Wired {
    fn default() -> Wired {
        Wired { up: true, dhcp: true, dns: DnsMode::Normal }
    }
}

/// One simulated access point.
pub struct Network {
    /// Short name used by control commands (`home`, `wpa3`, ...).
    pub name: String,
    pub ap: AccessPoint,
    /// Switched on (beaconing and answering).
    pub on: bool,
    /// Signal level at the guest.
    pub signal_dbm: i8,
    /// Percentage of frames lost in each direction.
    pub loss_pct: u8,
}

impl Network {
    pub fn new(name: &str, cfg: ApConfig, signal_dbm: i8, now: u64, rng: &mut SimRandom) -> Network {
        Network { name: name.into(), ap: AccessPoint::new(cfg, now, rng), on: true, signal_dbm, loss_pct: 0 }
    }

    fn reachable(&self, channel: u8) -> bool {
        self.on && self.ap.cfg.channel == channel && self.signal_dbm >= SENSITIVITY_DBM
    }
}

/// The guest's radio as seen from the medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Radio {
    /// The guest's driver is connected (QEMU's port is open).
    pub connected: bool,
    pub mac: Mac,
    pub channel: u8,
    /// Transmitter and receiver switched on.
    pub on: bool,
}

/// What the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Send a message to the guest's radio.
    Guest(Message),
    /// Send an Ethernet frame to QEMU's network.
    Wired(Vec<u8>),
    /// Print a line to the log.
    Log(String),
}

/// The MAC address given to a guest radio that does not have one.
pub const DEFAULT_GUEST_MAC: Mac = [0x02, 0x56, 0x57, 0xAA, 0x00, 0x01];

fn bssid(n: u8) -> Mac {
    [0x02, 0x56, 0x57, 0x00, 0x00, n]
}

fn config(n: u8, ssid: &str, channel: u8, security: Security, password: &str) -> ApConfig {
    ApConfig {
        bssid: bssid(n),
        ssid: ssid.as_bytes().to_vec(),
        channel,
        security,
        passphrase: (security != Security::Open).then(|| password.to_string()),
        hidden: false,
        pmf_required: security == Security::Wpa3Personal,
        h2e: matches!(security, Security::Wpa3Personal | Security::Wpa2Wpa3Personal),
        beacon_interval: 100,
    }
}

/// The networks of the default environment. Every secured network uses
/// `password`.
///
/// | name    | SSID             | security           | channel | signal  |
/// |---------|------------------|--------------------|---------|---------|
/// | home    | Vindows Home     | WPA2-Personal      | 1       | -48 dBm |
/// | home2   | Vindows Home     | WPA2-Personal      | 11      | -66 dBm |
/// | wpa3    | Vindows WPA3     | WPA3-Personal      | 6       | -55 dBm |
/// | mixed   | Vindows Mixed    | WPA2/WPA3-Personal | 44      | -60 dBm |
/// | guest   | Vindows Guest    | Open               | 36      | -71 dBm |
/// | hidden  | (Vindows Hidden) | WPA2-Personal      | 6       | -63 dBm |
/// | corp    | Vindows Corp     | Enterprise         | 11      | -78 dBm |
///
/// `home` and `home2` are two access points of one network, so a station
/// can move to `home2` when `home` disappears.
pub fn default_networks(now: u64, rng: &mut SimRandom, password: &str) -> Vec<Network> {
    let mut hidden = config(6, "Vindows Hidden", 6, Security::Wpa2Personal, password);
    hidden.hidden = true;
    vec![
        Network::new("home", config(1, "Vindows Home", 1, Security::Wpa2Personal, password), -48, now, rng),
        Network::new("home2", config(2, "Vindows Home", 11, Security::Wpa2Personal, password), -66, now, rng),
        Network::new("wpa3", config(3, "Vindows WPA3", 6, Security::Wpa3Personal, password), -55, now, rng),
        Network::new("mixed", config(4, "Vindows Mixed", 44, Security::Wpa2Wpa3Personal, password), -60, now, rng),
        Network::new("guest", config(5, "Vindows Guest", 36, Security::Open, password), -71, now, rng),
        Network::new("hidden", hidden, -63, now, rng),
        Network::new("corp", config(7, "Vindows Corp", 11, Security::Enterprise, password), -78, now, rng),
    ]
}

/// The simulated environment.
pub struct World {
    pub networks: Vec<Network>,
    pub radio: Radio,
    pub wired: Wired,
    /// The MAC address assigned to a guest that asks for one.
    pub guest_mac: Mac,
    pub rng: SimRandom,
    /// Counters for the `status` command.
    pub stats: Stats,
    out: Vec<Output>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub guest_tx: u64,
    pub guest_rx: u64,
    pub lost: u64,
    pub wired_tx: u64,
    pub wired_rx: u64,
    pub filtered: u64,
}

impl World {
    pub fn new(networks: Vec<Network>, rng: SimRandom) -> World {
        World {
            networks,
            radio: Radio { connected: false, mac: [0; 6], channel: CHANNELS[0], on: false },
            wired: Wired::default(),
            guest_mac: DEFAULT_GUEST_MAC,
            rng,
            stats: Stats::default(),
            out: Vec::new(),
        }
    }

    /// Takes the outputs produced so far.
    pub fn take_output(&mut self) -> Vec<Output> {
        core::mem::take(&mut self.out)
    }

    fn log(&mut self, line: String) {
        self.out.push(Output::Log(line));
    }

    /// Whether a frame survives a link with `loss_pct` percent loss.
    fn survives(&mut self, loss_pct: u8) -> bool {
        loss_pct == 0 || self.rng.below(100) >= loss_pct as u32
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.networks.iter().position(|n| n.name.eq_ignore_ascii_case(name))
    }
}

/// Formats a MAC address.
pub fn mac_str(m: &Mac) -> String {
    let mut s = String::new();
    for (i, b) in m.iter().enumerate() {
        let _ = write!(s, "{}{b:02x}", if i > 0 { ":" } else { "" });
    }
    s
}

fn describe(e: &ApEvent) -> String {
    match e {
        ApEvent::Joined(m) => format!("{} joined", mac_str(m)),
        ApEvent::Left(m, code) => format!("{} left (reason {code})", mac_str(m)),
        ApEvent::AuthFailed(m) => format!("{} did not complete authentication", mac_str(m)),
    }
}

/// The radio medium.
impl World {
    /// The guest's end of the link is open (its driver can talk to us).
    pub fn guest_connected(&mut self) {
        self.radio = Radio { connected: true, mac: [0; 6], channel: CHANNELS[0], on: false };
        self.log("guest radio connected".into());
    }

    /// The guest's end of the link closed (QEMU exited, or the port was
    /// closed). The access points drop the guest, as real ones would after
    /// their inactivity timeout.
    pub fn guest_disconnected(&mut self, now: u64) {
        let mac = self.radio.mac;
        self.radio.connected = false;
        self.radio.on = false;
        for i in 0..self.networks.len() {
            if self.networks[i].ap.stations().contains(&mac) {
                let actions = self.networks[i].ap.deauthenticate(Some(mac), vwlan::frame::reason::DISASSOC_INACTIVITY);
                self.carry_out(i, actions, now);
            }
        }
        self.log("guest radio disconnected".into());
    }

    /// Handles a message from the guest's radio.
    pub fn guest_message(&mut self, m: Message, now: u64) {
        match m {
            Message::Hello { version, mac } => {
                if version != VERSION {
                    self.log(format!("guest speaks radio link version {version}, we speak {VERSION}"));
                }
                // A missing or group address gets the default one.
                let mac = if mac == [0; 6] || mac[0] & 1 != 0 { self.guest_mac } else { mac };
                self.radio = Radio { connected: true, mac, channel: CHANNELS[0], on: false };
                self.log(format!("guest radio {} ready", mac_str(&mac)));
                self.out.push(Output::Guest(Message::HelloAck { version: VERSION, mac, channels: CHANNELS.to_vec() }));
            }
            Message::SetChannel { channel } => {
                if CHANNELS.contains(&channel) {
                    self.radio.channel = channel;
                } else {
                    self.log(format!("guest asked for unsupported channel {channel}"));
                }
            }
            Message::SetPower { on } => {
                if self.radio.on != on {
                    self.log(format!("guest radio switched {}", if on { "on" } else { "off" }));
                }
                self.radio.on = on;
            }
            Message::Tx { id, no_ack, frame } => {
                let acked = self.transmit_from_guest(&frame, now);
                if !no_ack {
                    self.out.push(Output::Guest(Message::TxStatus { id, acked }));
                }
            }
            other => self.log(format!("unexpected message from the guest: {other:?}")),
        }
    }

    /// Delivers a frame sent by the guest. Returns whether it was
    /// acknowledged (a unicast frame received by its access point).
    fn transmit_from_guest(&mut self, frame: &[u8], now: u64) -> bool {
        if !self.radio.connected || !self.radio.on {
            return false;
        }
        self.stats.guest_tx += 1;
        let receiver: Option<Mac> = frame.get(4..10).and_then(|s| s.try_into().ok());
        let mut acked = false;
        for i in 0..self.networks.len() {
            if !self.networks[i].reachable(self.radio.channel) {
                continue;
            }
            let loss = self.networks[i].loss_pct;
            if !self.survives(loss) {
                self.stats.lost += 1;
                continue;
            }
            if receiver == Some(self.networks[i].ap.cfg.bssid) {
                acked = true;
            }
            let actions = self.networks[i].ap.receive(frame, now, &mut self.rng);
            self.carry_out(i, actions, now);
        }
        acked
    }

    fn carry_out(&mut self, i: usize, actions: Vec<ApAction>, now: u64) {
        for a in actions {
            match a {
                ApAction::Transmit { frame, .. } => self.deliver_to_guest(i, frame),
                ApAction::Uplink(eth) => self.send_wired(eth, now),
                ApAction::Event(e) => {
                    let line = format!("{}: {}", self.networks[i].name, describe(&e));
                    self.log(line);
                }
            }
        }
    }

    fn deliver_to_guest(&mut self, i: usize, frame: Vec<u8>) {
        if !self.radio.connected || !self.radio.on || !self.networks[i].reachable(self.radio.channel) {
            return;
        }
        let n = &self.networks[i];
        let (channel, signal_dbm, loss) = (n.ap.cfg.channel, n.signal_dbm, n.loss_pct);
        if !self.survives(loss) {
            self.stats.lost += 1;
            return;
        }
        self.stats.guest_rx += 1;
        self.out.push(Output::Guest(Message::Rx { channel, signal_dbm, frame }));
    }

    /// Beacons and retransmissions that are due.
    pub fn tick(&mut self, now: u64) {
        for i in 0..self.networks.len() {
            if self.networks[i].on && now >= self.networks[i].ap.next_deadline() {
                let actions = self.networks[i].ap.tick(now);
                self.carry_out(i, actions, now);
            }
        }
    }

    /// When [`World::tick`] should run next.
    pub fn next_deadline(&self) -> u64 {
        self.networks.iter().filter(|n| n.on).map(|n| n.ap.next_deadline()).min().unwrap_or(u64::MAX)
    }
}

/// The wired network behind the access points.
impl World {
    /// An Ethernet frame from a station, bound for QEMU's network.
    fn send_wired(&mut self, eth: Vec<u8>, now: u64) {
        if !self.wired.up {
            self.stats.filtered += 1;
            return;
        }
        match crate::packet::classify(&eth) {
            crate::packet::Kind::Dhcp if !self.wired.dhcp => {
                self.stats.filtered += 1;
                return;
            }
            crate::packet::Kind::DnsQuery if self.wired.dns != DnsMode::Normal => {
                self.stats.filtered += 1;
                if self.wired.dns == DnsMode::ServFail
                    && let Some(answer) = crate::packet::dns_servfail(&eth)
                {
                    self.wired_frame(answer, now);
                }
                return;
            }
            _ => {}
        }
        self.stats.wired_tx += 1;
        self.out.push(Output::Wired(eth));
    }

    /// An Ethernet frame from QEMU's network: every access point that is on
    /// passes it to its stations.
    pub fn wired_frame(&mut self, eth: Vec<u8>, now: u64) {
        if !self.wired.up || (!self.wired.dhcp && crate::packet::classify(&eth) == crate::packet::Kind::Dhcp) {
            self.stats.filtered += 1;
            return;
        }
        self.stats.wired_rx += 1;
        for i in 0..self.networks.len() {
            if self.networks[i].on {
                let actions = self.networks[i].ap.downlink(&eth);
                self.carry_out(i, actions, now);
            }
        }
    }
}

/// Changes made by control commands.
impl World {
    /// Starts an access point afresh with its configuration: it forgets its
    /// stations and picks new group keys, like a rebooted access point.
    pub fn restart(&mut self, i: usize, now: u64) {
        let cfg = self.networks[i].ap.cfg.clone();
        self.networks[i].ap = AccessPoint::new(cfg, now, &mut self.rng);
    }

    /// Switches an access point on or off. Switching it on restarts it.
    pub fn set_on(&mut self, i: usize, on: bool, now: u64) {
        if on && !self.networks[i].on {
            self.restart(i, now);
        }
        self.networks[i].on = on;
        let line = format!("{} switched {}", self.networks[i].name, if on { "on" } else { "off" });
        self.log(line);
    }

    /// Disconnects the access point's stations (with a deauthentication
    /// frame carrying `reason`).
    pub fn deauthenticate(&mut self, i: usize, reason: u16, now: u64) {
        let actions = self.networks[i].ap.deauthenticate(None, reason);
        self.carry_out(i, actions, now);
    }

    /// Distributes new group keys to the access point's stations.
    pub fn rekey(&mut self, i: usize, now: u64) {
        let actions = self.networks[i].ap.rekey_group(&mut self.rng, now);
        self.carry_out(i, actions, now);
    }

    /// Changes the password of a secured access point, which restarts it.
    pub fn set_password(&mut self, i: usize, password: &str, now: u64) {
        self.networks[i].ap.cfg.passphrase = Some(password.to_string());
        self.restart(i, now);
    }

    /// One line per access point, for the `list` command.
    pub fn describe_networks(&self) -> String {
        let mut s = String::new();
        for n in &self.networks {
            let c = &n.ap.cfg;
            let _ = writeln!(
                s,
                "{:<7} {:<3} ch {:<2} {:>4} dBm loss {:>3}% {:?}{} \"{}\" {} station(s), {} joined",
                n.name,
                if n.on { "on" } else { "off" },
                c.channel,
                n.signal_dbm,
                n.loss_pct,
                c.security,
                if c.hidden { " hidden" } else { "" },
                String::from_utf8_lossy(&c.ssid),
                n.ap.stations().len(),
                n.ap.authorized().len(),
            );
        }
        s
    }

    /// The guest radio, the wired network and the counters, for `status`.
    pub fn describe_status(&self) -> String {
        let r = &self.radio;
        let w = &self.wired;
        let t = &self.stats;
        format!(
            "radio {} {} ch {} {}\nwired {} dhcp {} dns {:?}\nguest tx {} rx {} lost {}; wired tx {} rx {} filtered {}\n",
            if r.connected { "connected" } else { "disconnected" },
            mac_str(&r.mac),
            r.channel,
            if r.on { "on" } else { "off" },
            if w.up { "up" } else { "down" },
            if w.dhcp { "on" } else { "off" },
            w.dns,
            t.guest_tx,
            t.guest_rx,
            t.lost,
            t.wired_tx,
            t.wired_rx,
            t.filtered,
        )
    }
}
