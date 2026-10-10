//! `wifi` — Veda's Wi-Fi driver for Linux: the guest's Wi-Fi radios (the
//! cards Linux's mac80211 drivers drive: Intel's, MediaTek's, Realtek's,
//! Qualcomm's) as Veda's *managed* radios, each offered to Veda's Wi-Fi
//! service (`wlanphy`) as Veda's own drivers offer theirs.
//!
//! Linux is the radio's MLME: on the service's commands (`wlanmlme_ctl`),
//! through nl80211, it scans, sends the authentication and association
//! frames, retries, encrypts and decrypts, and watches the link. What
//! decides and authenticates stays in the service: which network to join,
//! SAE, the key handshakes; Linux gets the session keys only, and the
//! driver VM has no supplicant. EAPOL comes and goes through nl80211's
//! control port, owned by this program's socket: if the program ends,
//! Linux leaves the network.
//!
//! Data is Ethernet, through a raw packet socket on the radio's interface,
//! as for `net`'s cards. Each radio has a thread, which waits on its
//! sockets and on Veda's handles at once (the bridge's watches); radios
//! that come later (USB) are found as they do.

mod nl80211;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use guest_sys::{POLLIN, POLLOUT, PollFd, poll_many};
use vabi::signals;
use vipc::Bytes;
use vproto::netring::{Link, RxInfo, SlotMeta, kind};
use vproto::wlan::{
    Band, ChannelInfo, KeyKind, MLME_EVENT, MlmeEvent, PhyError, PhyInfo, PhyStats, RadioState, channel_flags,
    phy_caps, wlanmlme_ctl, wlanphy,
};
use vrt::object::Channel;

use nl80211::{Bss, Event, Nl80211};

/// Frames each way in a radio's link, and their slots (a frame and its
/// header; a probe response may be 2 KiB).
const SLOTS: u32 = 256;
const SLOT_SIZE: u32 = 4096;
/// How often the guest's new radios, and a radio's presence, are looked
/// at.
const CHECK: Duration = Duration::from_millis(500);
/// How often the signal of the access point joined is asked for.
const SIGNAL_EVERY: Duration = Duration::from_secs(2);
/// Management frames of the access point's that come to the service:
/// SA Query responses (Linux answers the queries itself).
const ACTION: u16 = 0x00D0;
const SA_QUERY_RESPONSE: [u8; 2] = [8, 1];
/// Reason code of a station that leaves (IEEE 802.11-2020, 9.4.1.7).
const DEAUTH_LEAVING: u16 = 3;
/// The most EAPOL frames kept until the association's end (the access
/// point's first message, and its resending).
const EARLY_EAPOL: usize = 4;
const ENOENT: i32 = 2;
const EBUSY: i32 = 16;
const ENOLINK: i32 = 67;
const EOPNOTSUPP: i32 = 95;
const ENETDOWN: i32 = 100;
const ENOTCONN: i32 = 107;

fn sysfs(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn link_name(path: String) -> Option<String> {
    Some(std::fs::read_link(path).ok()?.file_name()?.to_string_lossy().into_owned())
}

/// The channel and band of a frequency (2.4 and 5 GHz).
fn channel_of(freq: u32) -> Option<(Band, u8)> {
    match freq {
        2484 => Some((Band::Ghz2, 14)),
        2412..=2472 => Some((Band::Ghz2, ((freq - 2407) / 5) as u8)),
        5160..=5885 => Some((Band::Ghz5, ((freq - 5000) / 5) as u8)),
        _ => None,
    }
}

fn freq_of(band: Band, channel: u8) -> u32 {
    let c = channel as u32;
    match band {
        Band::Ghz2 if c == 14 => 2484,
        Band::Ghz2 => 2407 + 5 * c,
        Band::Ghz5 => 5000 + 5 * c,
        Band::Ghz6 if c == 2 => 5935,
        Band::Ghz6 => 5950 + 5 * c,
    }
}

fn phy_error(e: &io::Error) -> PhyError {
    match e.raw_os_error() {
        // The radio is off, or joined nothing.
        Some(ENETDOWN | ENOTCONN | ENOLINK) => PhyError::NotReady,
        Some(EOPNOTSUPP) => PhyError::NotSupported,
        _ if e.kind() == io::ErrorKind::Unsupported => PhyError::NotSupported,
        _ => PhyError::Io,
    }
}

/// A Wi-Fi radio of Linux's: its station interface.
struct Device {
    /// The radio (`phy0`), and its interface (`wlan0`).
    phy: String,
    interface: String,
    index: i32,
    mac: [u8; 6],
    /// Linux's driver, and where the radio is (`pci 00:14.3`).
    driver: String,
    location: String,
}

impl Device {
    fn of(interface: &str) -> Option<Device> {
        let dir = format!("/sys/class/net/{interface}");
        let phy = link_name(format!("{dir}/phy80211"))?;
        let device = link_name(format!("{dir}/device"))?;
        let bus = link_name(format!("{dir}/device/subsystem")).unwrap_or_default();
        let location = match bus.as_str() {
            "pci" => format!("pci {}", device.trim_start_matches("0000:")),
            _ => format!("{bus} {device}"),
        };
        // A virtual radio has no driver of a bus's: its class says.
        let driver = link_name(format!("{dir}/device/driver")).unwrap_or(bus);
        let digits: Vec<u8> =
            sysfs(&format!("{dir}/address")).split(':').filter_map(|d| u8::from_str_radix(d, 16).ok()).collect();
        let mac: [u8; 6] = digits.try_into().ok()?;
        let index = sysfs(&format!("{dir}/ifindex")).parse().ok()?;
        Some(Device { phy, interface: interface.to_string(), index, mac, driver, location })
    }

    /// Whether the interface is still the radio's.
    fn present(&self) -> bool {
        let dir = format!("/sys/class/net/{}", self.interface);
        std::fs::metadata(format!("{dir}/phy80211")).is_ok()
            && sysfs(&format!("{dir}/ifindex")) == self.index.to_string()
    }

    fn stat(&self, name: &str) -> u64 {
        sysfs(&format!("/sys/class/net/{}/statistics/{name}", self.interface)).parse().unwrap_or(0)
    }
}

/// Where joining an access point is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Authenticating,
    Associating,
    Associated,
}

/// The access point being joined, or joined.
#[derive(Clone, Copy)]
struct Peer {
    bssid: [u8; 6],
    freq: u32,
}

/// An authentication waiting for a scan: Linux joins only access points it
/// heard itself, lately.
struct Pending {
    ssid: Vec<u8>,
    body: Vec<u8>,
}

/// What the service is told, in order.
enum Out {
    Frame(Vec<u8>, RxInfo),
    Ethernet(Vec<u8>),
    Event(MlmeEvent),
}

/// A radio, as the service's.
struct Radio {
    dev: Device,
    nl: Nl80211,
    data: File,
    info: PhyInfo,
    /// The frequencies scans visit, and how many networks a scan may
    /// probe for by name.
    freqs: Vec<u32>,
    max_ssids: usize,
    powered: bool,
    /// The service's scan, since when.
    scanning: Option<Instant>,
    phase: Phase,
    peer: Option<Peer>,
    pending: Option<Pending>,
    /// EAPOL that came before the association's end: Linux takes the
    /// access point's first handshake message at once, and reports the
    /// association after it.
    early_eapol: Vec<Vec<u8>>,
    signal: Option<i8>,
    /// When to ask for the signal next.
    signal_due: Instant,
    out: Vec<Out>,
}

impl Radio {
    fn open(dev: Device) -> io::Result<Radio> {
        let mut nl = Nl80211::open(dev.index as u32)?;
        let interface = nl.interface()?;
        // Off until the service switches it on (and a station's).
        guest_sys::netif::set_up(&dev.interface, false)?;
        if !Nl80211::is_station(&interface) {
            nl.set_station()?;
        }
        let wiphy = nl.wiphy(interface.wiphy)?;
        let mut channels = Vec::new();
        let mut freqs = Vec::new();
        for c in &wiphy.channels {
            let Some((band, number)) = channel_of(c.freq) else { continue };
            let mut flags = 0;
            if c.disabled {
                flags |= channel_flags::DISABLED;
            } else {
                freqs.push(c.freq);
            }
            if c.no_ir {
                flags |= channel_flags::NO_IR;
            }
            if c.radar {
                flags |= channel_flags::DFS;
            }
            channels.push(ChannelInfo { band, number, freq_mhz: c.freq as u16, flags, max_power_dbm: c.max_power_dbm });
        }
        let info = PhyInfo {
            driver: format!("{} (Linux)", dev.driver),
            location: dev.location.clone(),
            mac: dev.mac,
            channels,
            caps: phy_caps::MANAGED,
        };
        let data = guest_sys::netif::packet_socket(dev.index)?;
        Ok(Radio {
            dev,
            nl,
            data,
            info,
            freqs,
            max_ssids: wiphy.max_scan_ssids,
            powered: false,
            scanning: None,
            phase: Phase::Idle,
            peer: None,
            pending: None,
            early_eapol: Vec::new(),
            signal: None,
            signal_due: Instant::now(),
            out: Vec::new(),
        })
    }

    fn log(&self, what: &str, e: &io::Error) {
        match self.nl.reason() {
            Some(why) => println!("wifi: {}: {what}: {e} ({why})", self.dev.interface),
            None => println!("wifi: {}: {what}: {e}", self.dev.interface),
        }
    }

    fn event(&mut self, e: MlmeEvent) {
        self.out.push(Out::Event(e));
    }

    fn rx_info(&self, freq: u32, signal_dbm: i8) -> RxInfo {
        let (band, channel) = channel_of(freq).unwrap_or((Band::Ghz2, 0));
        RxInfo { channel, band: band as u8, signal_dbm, noise_dbm: 0, rate: 0 }
    }

    /// A management frame of the access point's, for the service.
    fn peer_frame(&mut self, frame: Vec<u8>) {
        let Some(p) = self.peer else { return };
        let info = self.rx_info(p.freq, self.signal.unwrap_or(0));
        self.out.push(Out::Frame(frame, info));
    }

    /// Whether a management frame is of the access point being joined.
    fn of_peer(&self, frame: &[u8]) -> bool {
        self.peer.is_some_and(|p| frame.get(16..22) == Some(&p.bssid[..]))
    }

    fn joined_nothing(&mut self) {
        self.phase = Phase::Idle;
        self.peer = None;
        self.pending = None;
        self.early_eapol.clear();
        self.signal = None;
    }

    /// Leaves the access point, if joining or joined.
    fn leave(&mut self, reason: u16) {
        if let (Some(p), true) = (self.peer, self.phase != Phase::Idle) {
            let _ = self.nl.deauthenticate(p.bssid, reason);
        }
        self.joined_nothing();
    }

    /// The access points a scan heard, as their frames: beacons, and the
    /// probe responses of the networks probed for by name.
    fn scan_results(&mut self, since: Instant) {
        let results = match self.nl.scan_results() {
            Ok(r) => r,
            Err(e) => return self.log("scan results", &e),
        };
        let age_limit = since.elapsed().as_millis() as u64 + 1000;
        for b in results {
            if b.age_ms > age_limit || channel_of(b.freq).is_none() {
                continue;
            }
            let info = self.rx_info(b.freq, b.signal_dbm);
            if let Some(ies) = &b.beacon_ies {
                self.out.push(Out::Frame(scan_frame(&b, [0xFF; 6], ies, false), info));
            }
            if let Some(ies) = &b.probe_ies {
                self.out.push(Out::Frame(scan_frame(&b, self.dev.mac, ies, true), info));
            }
        }
    }

    /// Authenticates with the access point being joined, if Linux knows it.
    fn try_authenticate(&mut self, ssid: &[u8], body: &[u8]) -> io::Result<()> {
        let Some(p) = self.peer else { return Err(io::Error::from(io::ErrorKind::NotConnected)) };
        self.nl.authenticate(p.bssid, p.freq, ssid, body)
    }

    /// What Linux reports.
    fn linux_event(&mut self, e: Event) {
        match e {
            Event::ScanDone => {
                if let Some(since) = self.scanning.take() {
                    self.scan_results(since);
                    self.event(MlmeEvent::ScanDone {});
                }
                // An authentication that waited for Linux to hear its access
                // point: now, or never.
                if let Some(p) = self.pending.take()
                    && let Err(e) = self.try_authenticate(&p.ssid, &p.body)
                {
                    self.log("authenticate", &e);
                    self.joined_nothing();
                    self.event(MlmeEvent::Timeout {});
                }
            }
            Event::Authentication(f) if self.of_peer(&f) => self.peer_frame(f),
            Event::Association(f) if self.of_peer(&f) => {
                // Capability, then the status: 0 is success.
                let status = f.get(26..28).map_or(1, |s| u16::from_le_bytes([s[0], s[1]]));
                self.phase = if status == 0 { Phase::Associated } else { Phase::Idle };
                self.signal_due = Instant::now();
                self.peer_frame(f);
                // The handshake's first message, after the association.
                let early = std::mem::take(&mut self.early_eapol);
                if status == 0 {
                    self.out.extend(early.into_iter().map(Out::Ethernet));
                }
            }
            Event::TimedOut(bssid) if self.peer.is_some_and(|p| p.bssid == bssid) => {
                self.joined_nothing();
                self.event(MlmeEvent::Timeout {});
            }
            Event::Left(f) if self.of_peer(&f) => {
                let ours = f.get(10..16) == Some(&self.dev.mac[..]);
                if !ours {
                    // The access point's: the service's station reads it.
                    self.peer_frame(f);
                } else if self.phase == Phase::Associated {
                    // Linux gave up on the access point (it stopped being
                    // heard).
                    self.event(MlmeEvent::LinkLost {});
                }
                self.joined_nothing();
            }
            Event::Unprotected(f) if self.of_peer(&f) => {
                let reason = f.get(24..26).map_or(0, |r| u16::from_le_bytes([r[0], r[1]]));
                self.event(MlmeEvent::UnprotectedDeauth { reason });
            }
            // Joining failed without an answer to report.
            Event::Connect { status } if status != 0 && self.phase == Phase::Associating => {
                self.joined_nothing();
                self.event(MlmeEvent::Timeout {});
            }
            Event::Disconnect if self.phase == Phase::Associated => {
                self.joined_nothing();
                self.event(MlmeEvent::LinkLost {});
            }
            Event::Eapol { source, frame } => {
                let mut eth = Vec::with_capacity(14 + frame.len());
                eth.extend_from_slice(&self.dev.mac);
                eth.extend_from_slice(&source);
                eth.extend_from_slice(&nl80211::EAPOL.to_be_bytes());
                eth.extend_from_slice(&frame);
                if self.phase == Phase::Associating {
                    if self.early_eapol.len() < EARLY_EAPOL {
                        self.early_eapol.push(eth);
                    }
                } else {
                    self.out.push(Out::Ethernet(eth));
                }
            }
            Event::Frame(f) if self.of_peer(&f) => self.peer_frame(f),
            _ => {}
        }
    }

    /// Reports the access point's signal when it changed.
    fn check_signal(&mut self) {
        let Some(p) = self.peer else { return };
        if self.phase != Phase::Associated || Instant::now() < self.signal_due {
            return;
        }
        self.signal_due = Instant::now() + SIGNAL_EVERY;
        if let Ok(Some(dbm)) = self.nl.signal(p.bssid)
            && self.signal != Some(dbm)
        {
            self.signal = Some(dbm);
            self.event(MlmeEvent::Signal { dbm });
        }
    }

    /// Tells the service what came: frames on the link (dropped if it is
    /// full, as a radio drops), events on the channel. `false` if the
    /// service is gone.
    fn flush(&mut self, att: &Attachment) -> bool {
        for o in self.out.drain(..) {
            match o {
                Out::Frame(frame, info) => {
                    att.link.send(SlotMeta { kind: kind::IEEE80211, flags: 0, meta: info.pack() }, &frame);
                }
                Out::Ethernet(eth) => {
                    att.link.send(SlotMeta::ethernet(), &eth);
                }
                Out::Event(e) => {
                    if vipc::send_event(att.client.channel(), MLME_EVENT, e).is_err() {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// A Beacon (or, `probe`, a Probe Response to `to`) of what a scan heard.
fn scan_frame(b: &Bss, to: [u8; 6], ies: &[u8], probe: bool) -> Vec<u8> {
    let mut f = Vec::with_capacity(36 + ies.len());
    f.extend_from_slice(&[if probe { 0x50 } else { 0x80 }, 0, 0, 0]);
    f.extend_from_slice(&to);
    f.extend_from_slice(&b.bssid);
    f.extend_from_slice(&b.bssid);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&b.tsf.to_le_bytes());
    f.extend_from_slice(&b.interval.to_le_bytes());
    f.extend_from_slice(&b.capability.to_le_bytes());
    f.extend_from_slice(ies);
    f
}

/// The service's commands.
impl wlanmlme_ctl::Server for Radio {
    fn set_power(&mut self, on: bool) -> Result<(), PhyError> {
        if on == self.powered {
            return Ok(());
        }
        if !on {
            self.leave(DEAUTH_LEAVING);
            self.scanning = None;
        }
        if let Err(e) = guest_sys::netif::set_up(&self.dev.interface, on) {
            self.log("switching the radio", &e);
            return Err(phy_error(&e));
        }
        self.powered = on;
        if on && let Err(e) = self.nl.register_frame(ACTION, &SA_QUERY_RESPONSE) {
            self.log("SA Query responses", &e);
        }
        Ok(())
    }

    fn stats(&mut self) -> PhyStats {
        let d = &self.dev;
        PhyStats {
            tx_frames: d.stat("tx_packets"),
            tx_failed: d.stat("tx_errors") + d.stat("tx_dropped"),
            rx_frames: d.stat("rx_packets"),
            rx_dropped: d.stat("rx_dropped"),
        }
    }

    fn scan(&mut self, ssids: Vec<Bytes>) -> Result<(), PhyError> {
        if !self.powered {
            return Err(PhyError::NotReady);
        }
        let mut probe: Vec<&[u8]> =
            ssids.iter().map(|s| &s.0[..]).filter(|s| !s.is_empty()).take(self.max_ssids.saturating_sub(1)).collect();
        // And every network.
        probe.push(&[]);
        match self.nl.trigger_scan(&probe, &self.freqs) {
            Ok(()) => {}
            // Scanning already (for an authentication): its results do.
            Err(e) if e.raw_os_error() == Some(EBUSY) => {}
            Err(e) => {
                self.log("scan", &e);
                return Err(phy_error(&e));
            }
        }
        self.scanning.get_or_insert_with(Instant::now);
        Ok(())
    }

    fn authenticate(
        &mut self,
        bssid: [u8; 6],
        band: Band,
        channel: u8,
        ssid: Bytes,
        body: Bytes,
    ) -> Result<(), PhyError> {
        if !self.powered {
            return Err(PhyError::NotReady);
        }
        if body.0.len() < 6 {
            return Err(PhyError::NotSupported);
        }
        let freq = freq_of(band, channel);
        if self.peer.is_some_and(|p| p.bssid != bssid) {
            self.leave(DEAUTH_LEAVING);
        }
        self.pending = None;
        self.peer = Some(Peer { bssid, freq });
        self.phase = Phase::Authenticating;
        match self.try_authenticate(&ssid.0, &body.0) {
            Ok(()) => Ok(()),
            // Linux has not heard the access point lately: it listens for
            // it first.
            Err(e) if e.raw_os_error() == Some(ENOENT) => {
                match self.nl.trigger_scan(&[&ssid.0, &[]], &[freq]) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(EBUSY) => {}
                    Err(e) => {
                        self.log("scan for the access point", &e);
                        self.joined_nothing();
                        return Err(phy_error(&e));
                    }
                }
                self.pending = Some(Pending { ssid: ssid.0, body: body.0 });
                Ok(())
            }
            Err(e) => {
                self.log("authenticate", &e);
                self.joined_nothing();
                Err(phy_error(&e))
            }
        }
    }

    fn associate(
        &mut self,
        bssid: [u8; 6],
        band: Band,
        channel: u8,
        ssid: Bytes,
        ies: Bytes,
        pmf: bool,
    ) -> Result<(), PhyError> {
        if !self.powered {
            return Err(PhyError::NotReady);
        }
        let freq = freq_of(band, channel);
        self.peer = Some(Peer { bssid, freq });
        if let Err(e) = self.nl.associate(bssid, freq, &ssid.0, &ies.0, pmf) {
            self.log("associate", &e);
            self.joined_nothing();
            return Err(phy_error(&e));
        }
        self.phase = Phase::Associating;
        Ok(())
    }

    fn deauthenticate(&mut self, _bssid: [u8; 6], reason: u16) -> Result<(), PhyError> {
        self.leave(reason);
        Ok(())
    }

    fn send_eapol(&mut self, peer: [u8; 6], frame: Bytes, encrypt: bool) -> Result<(), PhyError> {
        self.nl.send_eapol(peer, &frame.0, encrypt).map_err(|e| {
            self.log("EAPOL", &e);
            phy_error(&e)
        })
    }

    fn install_key(&mut self, kind: KeyKind, index: u8, key: Bytes, rsc: u64, peer: [u8; 6]) -> Result<(), PhyError> {
        let (cipher, peer) = match kind {
            KeyKind::Pairwise => (nl80211::CCMP_128, Some(peer)),
            KeyKind::Group => (nl80211::CCMP_128, None),
            KeyKind::Integrity => (nl80211::BIP_CMAC_128, None),
        };
        self.nl.new_key(cipher, index, &key.0, rsc, peer).map_err(|e| {
            self.log("installing a key", &e);
            phy_error(&e)
        })
    }

    fn authorize(&mut self, peer: [u8; 6]) -> Result<(), PhyError> {
        self.nl.authorize(peer).map_err(|e| {
            self.log("opening the port", &e);
            phy_error(&e)
        })
    }

    fn send_management(&mut self, frame: Bytes) -> Result<(), PhyError> {
        let Some(p) = self.peer else { return Err(PhyError::NotReady) };
        self.nl.send_frame(p.freq, &frame.0).map_err(|e| {
            self.log("management frame", &e);
            phy_error(&e)
        })
    }
}

/// The radio, offered to the Wi-Fi service.
struct Attachment {
    client: wlanphy::Client,
    link: Link,
    control: Channel,
}

fn attach(r: &Radio) -> Result<Attachment, String> {
    let ch = vproto::connect(wlanphy::NAME).map_err(|e| format!("cannot reach the Wi-Fi service: {e:?}"))?;
    let client = wlanphy::Client::new(ch);
    let (link, ends) = Link::create(SLOTS, SLOT_SIZE).map_err(|e| format!("cannot create the link: {e}"))?;
    let (control, theirs) = Channel::create().map_err(|_| String::from("cannot create a channel"))?;
    match client.attach(r.info.clone(), ends, theirs, RadioState::Ready) {
        Ok(Ok(())) => Ok(Attachment { client, link, control }),
        Ok(Err(e)) => Err(format!("the Wi-Fi service refused the radio: {e:?}")),
        Err(_) => Err("the Wi-Fi service went away".into()),
    }
}

/// Serves the radio while it lasts: offered to the Wi-Fi service, again
/// whenever the service comes back.
fn serve(dev: Device) {
    let name = dev.interface.clone();
    let mut radio = match Radio::open(dev) {
        Ok(r) => r,
        Err(e) => {
            println!("wifi: {name}: cannot use the radio: {e}");
            return;
        }
    };
    let mac = radio.dev.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
    let usable = radio.info.channels.iter().filter(|c| c.flags & channel_flags::DISABLED == 0).count();
    println!("wifi: {name} ({}, {}): MAC {mac}, {usable} channels", radio.dev.driver, radio.dev.location);
    loop {
        match attach(&radio) {
            Ok(att) => {
                println!("wifi: {name}: offered to the Wi-Fi service");
                if let Err(e) = run(&mut radio, &att) {
                    println!("wifi: {name}: the radio is gone: {e}");
                    return;
                }
                println!("wifi: {name}: the Wi-Fi service went away; offering the radio again");
                // Whoever comes next starts from a radio switched off.
                let _ = wlanmlme_ctl::Server::set_power(&mut radio, false);
                radio.out.clear();
            }
            Err(e) => {
                println!("wifi: {name}: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Serves the service until it goes away (`Ok`) or the radio does (`Err`).
fn run(radio: &mut Radio, att: &Attachment) -> io::Result<()> {
    let watch = |handle, signals| vrt::guest::watch(handle, signals).map_err(|e| io::Error::other(format!("{e}")));
    let mut control = File::from(watch(att.control.raw(), signals::READABLE | signals::PEER_CLOSED)?);
    let mut wake = File::from(watch(att.link.wake_event().raw(), signals::SIGNALED)?);
    let closed = watch(att.client.channel().raw(), signals::PEER_CLOSED)?;
    let (mut to_radio, mut from_radio) = (vec![0u8; SLOT_SIZE as usize], vec![0u8; SLOT_SIZE as usize]);
    // A frame of Veda's the radio had no room for yet.
    let mut held: Option<usize> = None;
    let mut checked = Instant::now();
    loop {
        // The service's commands, then what Linux reports.
        loop {
            match att.control.read() {
                Ok(m) => match wlanmlme_ctl::dispatch(radio, m) {
                    Ok(reply) => {
                        if reply.send(&att.control).is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => println!("wifi: {}: bad request from the Wi-Fi service: {e}", radio.dev.interface),
                },
                Err(vabi::Error::ShouldWait) => break,
                Err(_) => return Ok(()),
            }
        }
        for e in radio.nl.events()? {
            radio.linux_event(e);
        }
        radio.check_signal();
        if checked.elapsed() >= CHECK {
            checked = Instant::now();
            if !radio.dev.present() {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
        }
        if !radio.flush(att) {
            return Ok(());
        }
        // Veda's frames to the radio, while it takes them.
        loop {
            if let Some(len) = held {
                match (&radio.data).write(&to_radio[..len]) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    // Dropped, as a radio drops what it cannot send.
                    _ => held = None,
                }
            }
            match att.link.recv(&mut to_radio) {
                Some((meta, len)) if meta.kind == kind::ETHERNET => held = Some(len),
                Some(_) => {}
                None => break,
            }
        }
        // The radio's frames to Veda (dropped if its ring is full).
        loop {
            match (&radio.data).read(&mut from_radio) {
                Ok(n) => {
                    att.link.send(SlotMeta::ethernet(), &from_radio[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // The interface is down.
                Err(_) => break,
            }
        }
        // Sleep until Linux, the service or the link has something: with a
        // frame held, the service's frames wait for the radio.
        let from_veda = held.is_none();
        if from_veda && !att.link.prepare_wait(false) {
            continue;
        }
        let timeout = if radio.nl.has_events() { 0 } else { CHECK.as_millis() as i32 };
        let mut fds = [
            PollFd::new(radio.data.as_raw_fd(), POLLIN | if from_veda { 0 } else { POLLOUT }),
            PollFd::new(radio.nl.fd(), POLLIN),
            PollFd::new(control.as_raw_fd(), POLLIN),
            PollFd::new(closed.as_raw_fd(), POLLIN),
            PollFd::new(if from_veda { wake.as_raw_fd() } else { -1 }, POLLIN),
        ];
        let _ = poll_many(&mut fds, timeout);
        att.link.finish_wait();
        if fds[3].revents != 0 {
            return Ok(());
        }
        // Taken: the next poll waits for them again.
        if fds[2].revents & POLLIN != 0 {
            let _ = control.read(&mut [0u8; 4]);
        }
        if fds[4].revents & POLLIN != 0 {
            let _ = wake.read(&mut [0u8; 4]);
        }
    }
}

fn main() {
    // The radios served, by name: Linux never gives a name twice, and a
    // radio's second interface (a monitor's) is not another radio.
    let mut serving = BTreeSet::new();
    loop {
        let interfaces: Vec<String> = std::fs::read_dir("/sys/class/net")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        for interface in interfaces {
            if let Some(dev) = Device::of(&interface)
                && serving.insert(dev.phy.clone())
            {
                std::thread::spawn(move || serve(dev));
            }
        }
        std::thread::sleep(CHECK);
    }
}
