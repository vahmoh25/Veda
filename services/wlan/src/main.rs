//! `wlan` — the Wi-Fi service.
//!
//! * Radio drivers offer their radios through `wlanphy`; the driver only
//!   moves raw 802.11 frames and tunes the radio ("soft MAC").
//! * The service does the rest with `vwlan`: scanning, open system and SAE
//!   authentication, association, the 4-way and group key handshakes,
//!   CCMP encryption and management frame protection.
//! * Each connected radio appears to `netd` as the Ethernet-like interface
//!   `wlan0` (`netdev` protocol); the link goes up when a network is joined,
//!   and `netd` takes care of DHCP, DNS and routing as for any interface.
//! * Applications (the desktop's Wi-Fi menu, Settings, the Terminal) use
//!   the `wlan` protocol: networks in range, connect and disconnect, saved
//!   networks, the radio switch, status events and diagnostics.
//!
//! Saved networks are joined automatically. Failed attempts are retried
//! with growing delays, a lost connection is re-established (on another
//! access point of the same network if the first is gone), and a weak
//! connection moves to a clearly stronger access point. A saved password
//! that stops working is not retried until the user connects again.
//!
//! Everything runs on one thread around a single wait. Calls to the driver
//! have a timeout, so a hung driver cannot freeze the service.

#![no_std]
#![no_main]

extern crate alloc;

mod api;
mod conn;
mod radio;
mod scan;
mod store;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vfiles::fs::Fs;
use vipc::WaitSet;
use vproto::net::{DeviceAttachment, DeviceInfo, InterfaceKind};
use vproto::netring::{Link, LinkEndpoints, RxInfo, kind};
use vproto::wlan::{
    ConnState, FailReason, LogEntry, PhyError, PhyInfo, RADIO_STATE_EVENT, RadioState, WLAN_EVENT, WlanCounters,
    WlanEvent, WlanStatus, wlan, wlanphy,
};
use vrt::object::Channel;
use vrt::println;
use vwlan::policy::Backoff;
use vwlan::profile::Profile;
use vwlan::station::Station;

use conn::{Current, Request};
use radio::Radio;
use scan::{Scan, Table};

vrt::entry!(main);

/// Lines kept in the connection log.
const LOG_LINES: usize = 200;
/// Status watchers.
const MAX_WATCHERS: usize = 32;
/// Client connections.
const MAX_CLIENTS: usize = 64;
/// Requests handled per client per loop iteration.
const CLIENT_BUDGET: usize = 16;
/// Frames taken from each link per loop iteration.
const FRAME_BUDGET: usize = 128;
/// Slots and slot size of the link to `netd` (Ethernet frames).
const NETD_SLOTS: u32 = 256;
const NETD_SLOT_SIZE: u32 = 2048;
/// How long a call to `netd` may take, and how often to retry attaching.
const NETD_TIMEOUT_NS: u64 = 5_000_000_000;
const NETD_RETRY_MS: u64 = 2_000;
/// Radio drivers whose attach request has not arrived yet.
const MAX_PENDING_PHYS: usize = 8;

/// Wait-set keys.
const KEY_PHY_LISTENER: u64 = 1;
const KEY_API_LISTENER: u64 = 2;
const KEY_RADIO: u64 = 3;
const KEY_RADIO_LINK: u64 = 4;
const KEY_NETD: u64 = 5;
const KEY_NETD_LINK: u64 = 6;
const KEY_DYNAMIC: u64 = 100;

pub fn now_ms() -> u64 {
    vrt::time::now_ns() / 1_000_000
}

/// The kernel's random number generator, for nonces and SAE.
pub struct KernelRandom;

impl vwlan::crypto::Random for KernelRandom {
    fn fill(&mut self, buf: &mut [u8]) {
        vrt::object::random_bytes(buf);
    }
}

pub struct Wlan {
    phy_listener: Channel,
    api_listener: Channel,
    /// Driver connections waiting for their `attach`.
    pending_phys: BTreeMap<u64, Channel>,
    clients: BTreeMap<u64, Channel>,
    watchers: Vec<Channel>,
    next_key: u64,
    fs: Fs,
    pub rng: KernelRandom,

    pub radio: Option<Radio>,
    pub netdev: Option<DeviceAttachment>,
    netdev_retry_ms: u64,
    /// Whether `netd` was told the link is up.
    pub link_up: bool,
    pub sta: Station,
    pub table: Table,
    pub scan: Option<Scan>,
    /// A user asked for a scan that could not start yet.
    pub scan_wanted: bool,
    pub next_scan_ms: u64,
    /// Background scans since the last connection (they slow down).
    pub idle_scans: u32,
    pub profiles: Vec<Profile>,
    pub backoff: BTreeMap<Vec<u8>, Backoff>,
    /// Saved networks whose password was refused: not joined automatically
    /// until the user connects to them again.
    pub rejected: BTreeSet<Vec<u8>>,
    /// A network to join once it is found (asked for by the user, or a
    /// lost connection to an unsaved network).
    pub request: Option<Request>,
    /// The network being joined or joined.
    pub current: Option<Current>,
    /// Join this access point once the current connection is left.
    pub roam_to: Option<(Request, vwlan::scan::Bss)>,
    /// Look around (scan) before joining anything automatically: set when
    /// a connection is lost, as the access point may be gone and others
    /// of the network may be on other channels.
    pub rescan_first: bool,
    /// Access points that stopped answering, and when: they are not joined
    /// again until heard after that.
    pub unresponsive: BTreeMap<vwlan::frame::Mac, u64>,
    /// The Wi-Fi switch.
    pub radio_on: bool,
    /// The user disconnected: do not join anything automatically.
    pub user_disconnected: bool,
    pub last_failure: Option<FailReason>,
    pub counters: WlanCounters,
    log: VecDeque<LogEntry>,
    last_status: Option<WlanStatus>,
    frame: Vec<u8>,
}

impl Wlan {
    fn key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    /// Adds a line to the connection log (and the debug console).
    pub fn log(&mut self, message: String) {
        println!("{}", message);
        if self.log.len() >= LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry { time_ms: now_ms(), message });
    }

    pub fn log_lines(&self) -> Vec<LogEntry> {
        self.log.iter().cloned().collect()
    }

    pub fn save_profiles(&mut self) -> bool {
        let ok = store::save(&self.fs, &self.profiles);
        if !ok {
            self.log("could not save the list of networks".into());
        }
        ok
    }

    /// Sends an event to every watcher, dropping those that went away.
    pub fn notify(&mut self, ev: WlanEvent) {
        self.watchers.retain(|w| vipc::send_event(w, WLAN_EVENT, ev.clone()).is_ok());
    }

    pub fn add_watcher(&mut self, ch: Channel) -> bool {
        if self.watchers.len() >= MAX_WATCHERS {
            return false;
        }
        let status = self.status(now_ms());
        if vipc::send_event(&ch, WLAN_EVENT, WlanEvent::StatusChanged { status }).is_err() {
            return false;
        }
        self.watchers.push(ch);
        true
    }

    pub fn radio_usable(&self) -> bool {
        self.radio.as_ref().is_some_and(|r| r.usable())
    }

    /// The status reported to applications.
    pub fn status(&self, now: u64) -> WlanStatus {
        let (adapter, mac) = match &self.radio {
            Some(r) => (r.info.driver.clone(), r.info.mac),
            None => (String::new(), [0; 6]),
        };
        let state = if !self.radio_usable() {
            ConnState::NoAdapter
        } else if !self.radio_on {
            ConnState::RadioOff
        } else {
            self.current.as_ref().map_or(ConnState::Disconnected, |c| c.state)
        };
        let mut s = WlanStatus {
            state,
            adapter,
            mac,
            ssid: vipc::Bytes(Vec::new()),
            name: String::new(),
            bssid: [0; 6],
            band: vproto::wlan::Band::Ghz2,
            channel: 0,
            signal_dbm: 0,
            security: vproto::wlan::Security::Open,
            last_failure: self.last_failure,
            connected_s: 0,
            scanning: self.scan.is_some(),
        };
        if let Some(c) = &self.current
            && state != ConnState::NoAdapter
            && state != ConnState::RadioOff
        {
            s.ssid = vipc::Bytes(c.request.ssid.clone());
            s.name = vproto::wlan::ssid_display(&c.request.ssid);
            s.bssid = c.bss.bssid;
            s.band = radio::band_of(c.bss.channel);
            s.channel = c.bss.channel;
            s.signal_dbm = if c.state == ConnState::Connected { self.sta.signal_dbm } else { c.bss.signal_dbm };
            s.security = scan::to_proto(c.bss.security);
            s.connected_s = c.connected_ms.map_or(0, |t| (now.saturating_sub(t) / 1000) as u32);
        }
        s
    }

    /// Tells watchers about status changes (signal changes only when the
    /// number of bars changes).
    fn publish_status(&mut self, now: u64) {
        let s = self.status(now);
        let changed = match &self.last_status {
            None => true,
            Some(old) => {
                old.state != s.state
                    || old.ssid != s.ssid
                    || old.bssid != s.bssid
                    || old.last_failure != s.last_failure
                    || old.scanning != s.scanning
                    || old.adapter != s.adapter
                    || vproto::wlan::signal_bars(old.signal_dbm) != vproto::wlan::signal_bars(s.signal_dbm)
            }
        };
        if changed {
            self.last_status = Some(s.clone());
            self.notify(WlanEvent::StatusChanged { status: s });
        }
    }

    // ---------------------------------------------------------------
    // Radio drivers

    fn accept(&mut self) {
        while let Some(ch) = vproto::accept(&self.phy_listener) {
            if self.pending_phys.len() < MAX_PENDING_PHYS {
                let k = self.key();
                self.pending_phys.insert(k, ch);
            }
        }
        while let Some(ch) = vproto::accept(&self.api_listener) {
            if self.clients.len() < MAX_CLIENTS {
                let k = self.key();
                self.clients.insert(k, ch);
            }
        }
    }

    /// Handles the `attach` request of a driver connection.
    fn handle_pending_phy(&mut self, key: u64, now: u64) {
        struct Session<'a> {
            busy: bool,
            offer: &'a mut Option<(Channel, Link, PhyInfo, RadioState)>,
        }
        impl wlanphy::Server for Session<'_> {
            fn attach(
                &mut self,
                info: PhyInfo,
                link: LinkEndpoints,
                control: Channel,
                state: RadioState,
            ) -> Result<(), PhyError> {
                if self.busy {
                    // One radio at a time.
                    return Err(PhyError::NotSupported);
                }
                let mac_ok = info.mac != [0; 6] && info.mac[0] & 1 == 0;
                if !mac_ok || info.channels.is_empty() || info.channels.len() > 64 || info.driver.len() > 64 {
                    return Err(PhyError::NotSupported);
                }
                let link = Link::attach(link).map_err(|_| PhyError::Io)?;
                *self.offer = Some((control, link, info, state));
                Ok(())
            }
        }
        let Some(ch) = self.pending_phys.get(&key) else { return };
        let msg = match ch.read() {
            Ok(m) => m,
            Err(vabi::Error::ShouldWait) => return,
            Err(_) => {
                self.pending_phys.remove(&key);
                return;
            }
        };
        let mut offer = None;
        let mut s = Session { busy: self.radio.is_some(), offer: &mut offer };
        match wlanphy::dispatch(&mut s, msg) {
            Ok(reply) => {
                let _ = reply.send(ch);
            }
            Err(e) => println!("bad wlanphy request: {}", e),
        }
        if let Some((control, link, info, state)) = offer {
            let channel = self.pending_phys.remove(&key).expect("pending driver");
            self.log(alloc::format!(
                "Wi-Fi adapter {} ({}), MAC {}, {} channels",
                info.driver,
                info.location,
                conn::mac_string(&info.mac),
                info.channels.len()
            ));
            self.radio = Some(Radio::new(channel, control, link, info, state));
            self.radio_attached(now);
        }
    }

    fn radio_attached(&mut self, now: u64) {
        self.sta = Station::new(self.radio.as_ref().map(|r| r.info.mac).unwrap_or([0; 6]));
        self.table.clear();
        self.attach_netd(now);
        if self.radio_usable() {
            self.radio_ready(now);
        }
    }

    /// The radio became usable: switch it on (if Wi-Fi is on) and scan.
    fn radio_ready(&mut self, now: u64) {
        let on = self.radio_on;
        if let Some(r) = self.radio.as_mut() {
            r.tuned = None;
            r.set_power(on);
        }
        self.idle_scans = 0;
        self.next_scan_ms = now;
    }

    /// Events from the driver.
    fn handle_radio(&mut self, observed: u32, now: u64) {
        loop {
            let Some(r) = self.radio.as_ref() else { return };
            match r.channel.read() {
                Ok(msg) => match vipc::decode_event::<RadioState>(msg) {
                    Ok((RADIO_STATE_EVENT, state)) => self.radio_state(state, now),
                    _ => println!("unexpected message from the radio driver"),
                },
                Err(vabi::Error::ShouldWait) => break,
                Err(_) => {
                    self.radio_gone(now);
                    return;
                }
            }
        }
        if observed & signals::PEER_CLOSED != 0 {
            self.radio_gone(now);
        }
    }

    fn radio_state(&mut self, state: RadioState, now: u64) {
        let was = self.radio_usable();
        if let Some(r) = self.radio.as_mut() {
            r.state = state;
        }
        let usable = self.radio_usable();
        if was && !usable {
            self.log("the Wi-Fi adapter stopped working".into());
            self.radio_lost(now);
        } else if !was && usable {
            self.log("the Wi-Fi adapter is working again".into());
            self.radio_ready(now);
        }
    }

    /// The radio cannot be used (for now): leave the network.
    fn radio_lost(&mut self, now: u64) {
        self.scan = None;
        if self.current.is_some() {
            let connected = self.current.as_ref().is_some_and(|c| c.state == ConnState::Connected);
            // Nothing can be sent: forget the connection without telling
            // the access point.
            self.sta = Station::new(self.sta.mac());
            self.connection_ended(
                if connected { vwlan::station::Failure::SignalLost } else { vwlan::station::Failure::NoResponse },
                connected,
                now,
            );
        }
        self.last_failure = Some(FailReason::RadioFailure);
        self.table.clear();
    }

    fn radio_gone(&mut self, now: u64) {
        if self.radio.is_none() {
            return;
        }
        self.log("the Wi-Fi adapter was removed".into());
        self.radio_lost(now);
        self.radio = None;
        // Closing the attachment removes wlan0 from netd.
        self.netdev = None;
        self.link_up = false;
    }

    // ---------------------------------------------------------------
    // netd

    fn attach_netd(&mut self, now: u64) {
        let Some(r) = self.radio.as_ref() else { return };
        let info = DeviceInfo {
            kind: InterfaceKind::Wireless,
            mac: r.info.mac,
            mtu: 1500,
            driver: alloc::format!("wlan ({})", r.info.driver),
            location: r.info.location.clone(),
        };
        let up = self.current.as_ref().is_some_and(|c| c.state == ConnState::Connected);
        match DeviceAttachment::attach_timeout(info, NETD_SLOTS, NETD_SLOT_SIZE, up, NETD_TIMEOUT_NS) {
            Ok(att) => {
                self.log(alloc::format!("attached to the network service as {}", att.name));
                self.netdev = Some(att);
                self.link_up = up;
            }
            Err(e) => {
                println!("cannot attach to the network service: {}", e);
                self.netdev_retry_ms = now + NETD_RETRY_MS;
            }
        }
    }

    /// Tells `netd` whether the link is up.
    pub fn set_link(&mut self, up: bool) {
        if self.link_up == up {
            return;
        }
        self.link_up = up;
        if let Some(nd) = &self.netdev
            && !nd.set_link(up)
        {
            self.netdev = None;
        }
    }

    // ---------------------------------------------------------------
    // Frames

    fn receive_frames(&mut self, now: u64) {
        for _ in 0..FRAME_BUDGET {
            let Some(r) = self.radio.as_ref() else { return };
            let Some((meta, len)) = r.link.recv(&mut self.frame) else { return };
            if meta.kind == kind::IEEE80211 {
                let frame = self.frame[..len].to_vec();
                self.radio_frame(&frame, RxInfo::unpack(meta.meta), now);
            }
        }
    }

    /// Frames from `netd` for the network.
    fn forward_from_netd(&mut self, now: u64) {
        let on_channel = self.scan.is_none();
        for _ in 0..FRAME_BUDGET {
            let connected = self.sta.is_connected();
            if connected && !on_channel {
                // Off channel for a scan: frames wait in the ring.
                return;
            }
            let Some(nd) = self.netdev.as_ref() else { return };
            let Some((meta, len)) = nd.link.recv(&mut self.frame) else { return };
            if meta.kind != kind::ETHERNET || !connected {
                continue;
            }
            let eth = self.frame[..len].to_vec();
            let actions = self.sta.send_ethernet(&eth);
            self.station_actions(actions, now);
        }
    }

    // ---------------------------------------------------------------
    // Clients

    fn handle_client(&mut self, key: u64, now: u64) {
        for _ in 0..CLIENT_BUDGET {
            let Some(ch) = self.clients.get(&key) else { return };
            let msg = match ch.read() {
                Ok(m) => m,
                Err(vabi::Error::ShouldWait) => return,
                Err(_) => {
                    self.clients.remove(&key);
                    return;
                }
            };
            let mut s = api::Session { w: self, now };
            match wlan::dispatch(&mut s, msg) {
                Ok(reply) => {
                    if let Some(ch) = self.clients.get(&key) {
                        let _ = reply.send(ch);
                    }
                }
                Err(e) => {
                    println!("bad wlan request: {}", e);
                    self.clients.remove(&key);
                    return;
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // The loop

    fn wait(&mut self, now: u64) {
        let mut ws = WaitSet::new();
        ws.add(self.phy_listener.raw(), signals::READABLE, KEY_PHY_LISTENER);
        ws.add(self.api_listener.raw(), signals::READABLE, KEY_API_LISTENER);
        let mut sleep = true;
        if let Some(r) = &self.radio {
            ws.add(r.channel.raw(), signals::READABLE | signals::PEER_CLOSED, KEY_RADIO);
            ws.add(r.link.wake_event().raw(), signals::SIGNALED, KEY_RADIO_LINK);
            sleep &= r.link.prepare_wait(false);
        }
        if let Some(nd) = &self.netdev {
            ws.add(nd.channel().raw(), signals::PEER_CLOSED, KEY_NETD);
            ws.add(nd.link.wake_event().raw(), signals::SIGNALED, KEY_NETD_LINK);
            sleep &= nd.link.prepare_wait(false);
        }
        let mut dynamic: Vec<(u64, u64)> = Vec::new();
        for (&k, ch) in self.pending_phys.iter().chain(self.clients.iter()) {
            let wk = KEY_DYNAMIC + dynamic.len() as u64;
            ws.add(ch.raw(), signals::READABLE | signals::PEER_CLOSED, wk);
            dynamic.push((wk, k));
        }
        let ready = if sleep {
            let deadline_ms = self.next_deadline(now);
            ws.wait(deadline_ms.saturating_mul(1_000_000)).unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(r) = &self.radio {
            r.link.finish_wait();
        }
        if let Some(nd) = &self.netdev {
            nd.link.finish_wait();
        }
        let now = now_ms();
        for (wk, observed) in ready {
            match wk {
                KEY_RADIO => self.handle_radio(observed, now),
                KEY_NETD if observed & signals::PEER_CLOSED != 0 => {
                    self.log("the network service went away".into());
                    self.netdev = None;
                    self.link_up = false;
                    self.netdev_retry_ms = now + NETD_RETRY_MS;
                }
                k if k >= KEY_DYNAMIC => {
                    if let Some(&(_, key)) = dynamic.iter().find(|(w, _)| *w == k) {
                        if self.pending_phys.contains_key(&key) {
                            self.handle_pending_phy(key, now);
                        } else {
                            self.handle_client(key, now);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn run(&mut self) -> ! {
        loop {
            let now = now_ms();
            self.accept();
            self.receive_frames(now);
            self.forward_from_netd(now);
            self.tick(now);
            if self.netdev.is_none() && self.radio.is_some() && now >= self.netdev_retry_ms {
                self.attach_netd(now);
            }
            if self.radio.as_ref().is_some_and(|r| r.broken) {
                self.log("the Wi-Fi adapter's driver stopped responding".into());
                self.radio_gone(now);
            }
            self.publish_status(now);
            self.wait(now);
        }
    }
}

fn main() -> i32 {
    let (Ok(phy_listener), Ok(api_listener)) = (vproto::register(wlanphy::NAME), vproto::register(wlan::NAME)) else {
        println!("cannot register the Wi-Fi services");
        return 1;
    };
    let fs = Fs::connect();
    let profiles = store::load(&fs);
    println!("Wi-Fi service started ({} saved networks)", profiles.len());
    let mut w = Wlan {
        phy_listener,
        api_listener,
        pending_phys: BTreeMap::new(),
        clients: BTreeMap::new(),
        watchers: Vec::new(),
        next_key: KEY_DYNAMIC,
        fs,
        rng: KernelRandom,
        radio: None,
        netdev: None,
        netdev_retry_ms: 0,
        link_up: false,
        sta: Station::new([0; 6]),
        table: Table::default(),
        scan: None,
        scan_wanted: false,
        next_scan_ms: 0,
        idle_scans: 0,
        profiles,
        backoff: BTreeMap::new(),
        rejected: BTreeSet::new(),
        request: None,
        current: None,
        roam_to: None,
        rescan_first: false,
        unresponsive: BTreeMap::new(),
        radio_on: true,
        user_disconnected: false,
        last_failure: None,
        counters: WlanCounters::default(),
        log: VecDeque::new(),
        last_status: None,
        frame: alloc::vec![0u8; 4096],
    };
    w.run()
}
