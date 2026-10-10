//! nl80211, the part a station of Veda's needs: the radio's interface and
//! channels, scans and their results, joining (authentication,
//! association, keys and the control port that carries EAPOL) and leaving,
//! and the events of all that.
//!
//! One socket per radio: it owns the connection (`SOCKET_OWNER`), so that
//! Linux leaves the network when the program that joined it is gone, and
//! the control port's frames come to it.

use std::io;
use std::os::fd::RawFd;

use guest_netlink::{Attrs, Builder, Family, NETLINK_GENERIC, NLM_F_DUMP, Received, Socket};

mod cmd {
    pub const GET_WIPHY: u8 = 1;
    pub const GET_INTERFACE: u8 = 5;
    pub const SET_INTERFACE: u8 = 6;
    pub const NEW_KEY: u8 = 11;
    pub const GET_STATION: u8 = 17;
    pub const SET_STATION: u8 = 18;
    pub const GET_SCAN: u8 = 32;
    pub const TRIGGER_SCAN: u8 = 33;
    pub const NEW_SCAN_RESULTS: u8 = 34;
    pub const SCAN_ABORTED: u8 = 35;
    pub const AUTHENTICATE: u8 = 37;
    pub const ASSOCIATE: u8 = 38;
    pub const DEAUTHENTICATE: u8 = 39;
    pub const DISASSOCIATE: u8 = 40;
    pub const CONNECT: u8 = 46;
    pub const DISCONNECT: u8 = 48;
    pub const REGISTER_FRAME: u8 = 58;
    pub const FRAME: u8 = 59;
    pub const UNPROT_DEAUTHENTICATE: u8 = 70;
    pub const UNPROT_DISASSOCIATE: u8 = 71;
    pub const CONTROL_PORT_FRAME: u8 = 129;
}

mod attr {
    pub const WIPHY: u16 = 1;
    pub const IFINDEX: u16 = 3;
    pub const IFTYPE: u16 = 5;
    pub const MAC: u16 = 6;
    pub const STA_INFO: u16 = 21;
    pub const WIPHY_BANDS: u16 = 22;
    pub const WIPHY_FREQ: u16 = 38;
    pub const IE: u16 = 42;
    pub const MAX_NUM_SCAN_SSIDS: u16 = 43;
    pub const SCAN_FREQUENCIES: u16 = 44;
    pub const SCAN_SSIDS: u16 = 45;
    pub const BSS: u16 = 47;
    pub const FRAME: u16 = 51;
    pub const SSID: u16 = 52;
    pub const AUTH_TYPE: u16 = 53;
    pub const REASON_CODE: u16 = 54;
    pub const TIMED_OUT: u16 = 65;
    pub const USE_MFP: u16 = 66;
    pub const STA_FLAGS2: u16 = 67;
    pub const CONTROL_PORT: u16 = 68;
    pub const STATUS_CODE: u16 = 72;
    pub const CIPHER_SUITES_PAIRWISE: u16 = 73;
    pub const CIPHER_SUITE_GROUP: u16 = 74;
    pub const WPA_VERSIONS: u16 = 75;
    pub const AKM_SUITES: u16 = 76;
    pub const KEY: u16 = 80;
    pub const FRAME_MATCH: u16 = 91;
    pub const FRAME_TYPE: u16 = 101;
    pub const CONTROL_PORT_ETHERTYPE: u16 = 102;
    pub const CONTROL_PORT_NO_ENCRYPT: u16 = 103;
    pub const DONT_WAIT_FOR_ACK: u16 = 142;
    pub const AUTH_DATA: u16 = 156;
    pub const SCAN_FLAGS: u16 = 158;
    pub const SPLIT_WIPHY_DUMP: u16 = 174;
    pub const SOCKET_OWNER: u16 = 204;
    pub const CONTROL_PORT_OVER_NL80211: u16 = 264;
}

/// Attributes of a band (`WIPHY_BANDS`), a channel of it, a BSS, a key
/// and a station's information.
mod band {
    pub const FREQS: u16 = 1;
}
mod freq {
    pub const FREQ: u16 = 1;
    pub const DISABLED: u16 = 2;
    pub const NO_IR: u16 = 3;
    pub const RADAR: u16 = 5;
    pub const MAX_TX_POWER: u16 = 6;
}
mod bss {
    pub const BSSID: u16 = 1;
    pub const FREQUENCY: u16 = 2;
    pub const TSF: u16 = 3;
    pub const BEACON_INTERVAL: u16 = 4;
    pub const CAPABILITY: u16 = 5;
    pub const INFORMATION_ELEMENTS: u16 = 6;
    pub const SIGNAL_MBM: u16 = 7;
    pub const SEEN_MS_AGO: u16 = 10;
    pub const BEACON_IES: u16 = 11;
    pub const PRESP_DATA: u16 = 14;
}
mod key {
    pub const DATA: u16 = 1;
    pub const IDX: u16 = 2;
    pub const CIPHER: u16 = 3;
    pub const SEQ: u16 = 4;
    pub const TYPE: u16 = 7;
}
mod sta_info {
    pub const SIGNAL: u16 = 7;
    pub const SIGNAL_AVG: u16 = 13;
}

const IFTYPE_STATION: u32 = 2;
const AUTHTYPE_OPEN_SYSTEM: u32 = 0;
const AUTHTYPE_SAE: u32 = 4;
const MFP_NO: u32 = 0;
const MFP_REQUIRED: u32 = 1;
const KEYTYPE_GROUP: u32 = 0;
const KEYTYPE_PAIRWISE: u32 = 1;
const WPA_VERSION_2: u32 = 2;
const STA_FLAG_AUTHORIZED: u32 = 1;
const SCAN_FLAG_FLUSH: u32 = 2;
const EALREADY: i32 = 114;
/// The bands of `WIPHY_BANDS` Veda's stations use: 2.4 and 5 GHz.
const BAND_2GHZ: u16 = 0;
const BAND_5GHZ: u16 = 1;

/// EAPOL's Ethernet type, the control port's.
pub const EAPOL: u16 = 0x888E;
/// Cipher suites (00-0F-AC, as nl80211 writes them).
pub const CCMP_128: u32 = 0x000F_AC04;
pub const BIP_CMAC_128: u32 = 0x000F_AC06;
/// Authentication algorithms (IEEE 802.11-2020, 9.4.1.1).
const AUTH_OPEN: u16 = 0;
const AUTH_SAE: u16 = 3;

/// A station interface of a radio.
pub struct Interface {
    pub wiphy: u32,
    pub iftype: u32,
}

/// A channel of the radio's.
pub struct Channel {
    pub freq: u32,
    pub disabled: bool,
    pub no_ir: bool,
    pub radar: bool,
    pub max_power_dbm: i8,
}

/// What a radio can do, of what Veda uses.
pub struct Wiphy {
    pub channels: Vec<Channel>,
    pub max_scan_ssids: usize,
}

/// An access point a scan found.
pub struct Bss {
    pub bssid: [u8; 6],
    pub freq: u32,
    pub signal_dbm: i8,
    pub tsf: u64,
    pub interval: u16,
    pub capability: u16,
    /// How long ago it was last heard.
    pub age_ms: u64,
    /// The elements of its latest probe response, if it answered one.
    pub probe_ies: Option<Vec<u8>>,
    /// The elements of its latest beacon.
    pub beacon_ies: Option<Vec<u8>>,
}

/// What the radio's interface reports.
#[derive(Debug)]
pub enum Event {
    /// A scan ended (or was given up): its results wait.
    ScanDone,
    /// An Authentication frame of an access point's, answering ours.
    Authentication(Vec<u8>),
    /// An Association Response frame.
    Association(Vec<u8>),
    /// The access point `.0` did not answer an authentication or an
    /// association.
    TimedOut([u8; 6]),
    /// A deauthentication or disassociation: the access point's, or one
    /// of Linux's own (its source the radio's address), when it gave up on
    /// the access point.
    Left(Vec<u8>),
    /// An unprotected deauthentication or disassociation was dropped (the
    /// connection protects management frames): its frame.
    Unprotected(Vec<u8>),
    /// cfg80211's view of the joining: it failed (`status` not 0) or
    /// succeeded.
    Connect { status: u16 },
    /// cfg80211's view: the connection ended.
    Disconnect,
    /// An EAPOL frame from `source`.
    Eapol { source: [u8; 6], frame: Vec<u8> },
    /// A management frame this socket registered for.
    Frame(Vec<u8>),
}

fn mac(v: &[u8]) -> Option<[u8; 6]> {
    v.get(..6)?.try_into().ok()
}

/// The suites of an RSN element in `ies` (group, pairwise, AKMs).
fn rsn_suites(ies: &[u8]) -> Option<(u32, Vec<u32>, Vec<u32>)> {
    let mut rest = ies;
    while rest.len() >= 2 {
        let (id, len) = (rest[0], rest[1] as usize);
        let body = rest.get(2..2 + len)?;
        rest = &rest[2 + len..];
        if id != 48 {
            continue;
        }
        let suite = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let list = |b: &[u8]| -> Option<(Vec<u32>, usize)> {
            let n = u16::from_le_bytes([*b.first()?, *b.get(1)?]) as usize;
            let s = b.get(2..2 + 4 * n)?;
            Some((s.chunks(4).map(suite).collect(), 2 + 4 * n))
        };
        let group = suite(body.get(2..6)?);
        let (pairwise, n) = list(body.get(6..)?)?;
        let (akms, _) = list(body.get(6 + n..)?)?;
        return Some((group, pairwise, akms));
    }
    None
}

/// The radio's interface through nl80211.
pub struct Nl80211 {
    socket: Socket,
    family: u16,
    ifindex: u32,
}

impl Nl80211 {
    /// A socket for interface `ifindex`, receiving its events.
    pub fn open(ifindex: u32) -> io::Result<Nl80211> {
        let mut socket = Socket::open(NETLINK_GENERIC)?;
        let family = Family::find(&mut socket, "nl80211")?;
        for group in ["scan", "mlme"] {
            let id = family.group(group).ok_or_else(|| io::Error::other(format!("nl80211 has no {group} group")))?;
            socket.join(id)?;
        }
        Ok(Nl80211 { socket, family: family.id, ifindex })
    }

    pub fn fd(&self) -> RawFd {
        self.socket.fd()
    }

    /// Whether events wait in the program (a poll would not see them).
    pub fn has_events(&self) -> bool {
        self.socket.has_events()
    }

    /// Why the last command failed, if Linux said.
    pub fn reason(&self) -> Option<&str> {
        self.socket.reason()
    }

    /// A command for the interface.
    fn command(&self, cmd: u8) -> Builder {
        Builder::genl(self.family, 0, cmd, 0).u32(attr::IFINDEX, self.ifindex)
    }

    fn call(&mut self, msg: Builder) -> io::Result<Vec<Received>> {
        self.socket.request(msg)
    }

    pub fn interface(&mut self) -> io::Result<Interface> {
        let replies = self.call(self.command(cmd::GET_INTERFACE))?;
        let a = replies.first().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?.attrs();
        Ok(Interface {
            wiphy: a.u32(attr::WIPHY).ok_or_else(|| io::Error::other("no wiphy"))?,
            iftype: a.u32(attr::IFTYPE).unwrap_or(0),
        })
    }

    /// Makes the interface a station's (it must be down).
    pub fn set_station(&mut self) -> io::Result<()> {
        self.call(self.command(cmd::SET_INTERFACE).u32(attr::IFTYPE, IFTYPE_STATION)).map(drop)
    }

    pub fn is_station(interface: &Interface) -> bool {
        interface.iftype == IFTYPE_STATION
    }

    /// The radio's 2.4 and 5 GHz channels, and how many networks a scan
    /// may probe for by name.
    pub fn wiphy(&mut self, wiphy: u32) -> io::Result<Wiphy> {
        let msg = Builder::genl(self.family, NLM_F_DUMP, cmd::GET_WIPHY, 0)
            .u32(attr::WIPHY, wiphy)
            .flag(attr::SPLIT_WIPHY_DUMP);
        let mut w = Wiphy { channels: Vec::new(), max_scan_ssids: 1 };
        // A split dump: each message has a part of the bands.
        for m in self.call(msg)? {
            let a = m.attrs();
            if let Some(n) = a.u8(attr::MAX_NUM_SCAN_SSIDS) {
                w.max_scan_ssids = n.max(1) as usize;
            }
            let Some(bands) = a.nested(attr::WIPHY_BANDS) else { continue };
            for (band, b) in bands.iter() {
                if band != BAND_2GHZ && band != BAND_5GHZ {
                    continue;
                }
                let Some(freqs) = Attrs(b).nested(band::FREQS) else { continue };
                for (_, f) in freqs.iter() {
                    let f = Attrs(f);
                    let Some(freq) = f.u32(freq::FREQ) else { continue };
                    w.channels.push(Channel {
                        freq,
                        disabled: f.has(freq::DISABLED),
                        no_ir: f.has(freq::NO_IR),
                        radar: f.has(freq::RADAR),
                        max_power_dbm: (f.u32(freq::MAX_TX_POWER).unwrap_or(2000) / 100).min(127) as i8,
                    });
                }
            }
        }
        Ok(w)
    }

    /// Receives the management frames of `kind` (frame control) whose
    /// bodies start with `prefix`.
    pub fn register_frame(&mut self, kind: u16, prefix: &[u8]) -> io::Result<()> {
        let msg = self.command(cmd::REGISTER_FRAME).u16(attr::FRAME_TYPE, kind).attr(attr::FRAME_MATCH, prefix);
        match self.call(msg) {
            // Registered already (the interface came back up).
            Err(e) if e.raw_os_error() == Some(EALREADY) => Ok(()),
            r => r.map(drop),
        }
    }

    /// Scans `freqs`, probing for `ssids` (an empty one: every network),
    /// forgetting the access points it does not find.
    pub fn trigger_scan(&mut self, ssids: &[&[u8]], freqs: &[u32]) -> io::Result<()> {
        let mut msg = self.command(cmd::TRIGGER_SCAN).nest(attr::SCAN_SSIDS);
        for (i, ssid) in ssids.iter().enumerate() {
            msg = msg.attr(i as u16 + 1, ssid);
        }
        msg = msg.end().nest(attr::SCAN_FREQUENCIES);
        for (i, &f) in freqs.iter().enumerate() {
            msg = msg.u32(i as u16 + 1, f);
        }
        self.call(msg.end().u32(attr::SCAN_FLAGS, SCAN_FLAG_FLUSH)).map(drop)
    }

    /// The access points the scans found.
    pub fn scan_results(&mut self) -> io::Result<Vec<Bss>> {
        let msg = Builder::genl(self.family, NLM_F_DUMP, cmd::GET_SCAN, 0).u32(attr::IFINDEX, self.ifindex);
        let mut out = Vec::new();
        for m in self.call(msg)? {
            let Some(b) = m.attrs().nested(attr::BSS) else { continue };
            let (Some(bssid), Some(freq)) = (b.get(bss::BSSID).and_then(mac), b.u32(bss::FREQUENCY)) else {
                continue;
            };
            let ies = b.get(bss::INFORMATION_ELEMENTS).map(<[u8]>::to_vec);
            let presp = b.has(bss::PRESP_DATA);
            out.push(Bss {
                bssid,
                freq,
                signal_dbm: (b.u32(bss::SIGNAL_MBM).map_or(-10_000, |s| s as i32) / 100).clamp(-128, 0) as i8,
                tsf: b.u64(bss::TSF).unwrap_or(0),
                interval: b.u16(bss::BEACON_INTERVAL).unwrap_or(100),
                capability: b.u16(bss::CAPABILITY).unwrap_or(0),
                age_ms: b.u32(bss::SEEN_MS_AGO).unwrap_or(0) as u64,
                probe_ies: if presp { ies.clone() } else { None },
                beacon_ies: b.get(bss::BEACON_IES).map(<[u8]>::to_vec).or(if presp { None } else { ies }),
            });
        }
        Ok(out)
    }

    /// Sends access point `bssid` an Authentication frame of `body`'s: the
    /// algorithm, then (for SAE) the transaction, the status and the
    /// algorithm's data, which Linux puts after it.
    pub fn authenticate(&mut self, bssid: [u8; 6], freq: u32, ssid: &[u8], body: &[u8]) -> io::Result<()> {
        let algorithm = u16::from_le_bytes([body[0], body[1]]);
        let msg =
            self.command(cmd::AUTHENTICATE).attr(attr::MAC, &bssid).u32(attr::WIPHY_FREQ, freq).attr(attr::SSID, ssid);
        let msg = match algorithm {
            AUTH_OPEN => msg.u32(attr::AUTH_TYPE, AUTHTYPE_OPEN_SYSTEM),
            AUTH_SAE => msg.u32(attr::AUTH_TYPE, AUTHTYPE_SAE).attr(attr::AUTH_DATA, &body[2..]),
            _ => return Err(io::Error::from(io::ErrorKind::Unsupported)),
        };
        self.call(msg).map(drop)
    }

    /// Associates with `bssid` (authenticated), with `ies`. A protected
    /// network's (an RSN element) keeps its port closed but for EAPOL, which
    /// comes and goes through this socket, until [`Nl80211::authorize`].
    pub fn associate(&mut self, bssid: [u8; 6], freq: u32, ssid: &[u8], ies: &[u8], pmf: bool) -> io::Result<()> {
        let mut msg = self
            .command(cmd::ASSOCIATE)
            .attr(attr::MAC, &bssid)
            .u32(attr::WIPHY_FREQ, freq)
            .attr(attr::SSID, ssid)
            .attr(attr::IE, ies)
            .u32(attr::USE_MFP, if pmf { MFP_REQUIRED } else { MFP_NO })
            .flag(attr::SOCKET_OWNER);
        if let Some((group, pairwise, akms)) = rsn_suites(ies) {
            let words = |v: &[u32]| v.iter().flat_map(|s| s.to_ne_bytes()).collect::<Vec<u8>>();
            msg = msg
                .flag(attr::CONTROL_PORT)
                .u16(attr::CONTROL_PORT_ETHERTYPE, EAPOL)
                .flag(attr::CONTROL_PORT_OVER_NL80211)
                .u32(attr::WPA_VERSIONS, WPA_VERSION_2)
                .attr(attr::CIPHER_SUITES_PAIRWISE, &words(&pairwise))
                .u32(attr::CIPHER_SUITE_GROUP, group)
                .attr(attr::AKM_SUITES, &words(&akms));
        }
        self.call(msg).map(drop)
    }

    pub fn deauthenticate(&mut self, bssid: [u8; 6], reason: u16) -> io::Result<()> {
        self.call(self.command(cmd::DEAUTHENTICATE).attr(attr::MAC, &bssid).u16(attr::REASON_CODE, reason)).map(drop)
    }

    /// Sends `peer` an EAPOL frame through the control port, encrypted if
    /// `encrypt` (a frame in clear stays so even if the key that follows
    /// it comes first).
    pub fn send_eapol(&mut self, peer: [u8; 6], frame: &[u8], encrypt: bool) -> io::Result<()> {
        let mut msg = self
            .command(cmd::CONTROL_PORT_FRAME)
            .attr(attr::FRAME, frame)
            .attr(attr::MAC, &peer)
            .u16(attr::CONTROL_PORT_ETHERTYPE, EAPOL)
            .flag(attr::DONT_WAIT_FOR_ACK);
        if !encrypt {
            msg = msg.flag(attr::CONTROL_PORT_NO_ENCRYPT);
        }
        self.call(msg).map(drop)
    }

    /// Installs a key: the pairwise one for `peer`, or (`peer` `None`) a
    /// group one. Frames are taken with packet numbers above `rsc`.
    pub fn new_key(&mut self, cipher: u32, index: u8, data: &[u8], rsc: u64, peer: Option<[u8; 6]>) -> io::Result<()> {
        let mut msg = self.command(cmd::NEW_KEY);
        if let Some(peer) = peer {
            msg = msg.attr(attr::MAC, &peer);
        }
        let msg = msg
            .nest(attr::KEY)
            .attr(key::DATA, data)
            .u8(key::IDX, index)
            .u32(key::CIPHER, cipher)
            .attr(key::SEQ, &rsc.to_le_bytes()[..6])
            .u32(key::TYPE, if peer.is_some() { KEYTYPE_PAIRWISE } else { KEYTYPE_GROUP })
            .end();
        self.call(msg).map(drop)
    }

    /// Opens the port to `peer` for data.
    pub fn authorize(&mut self, peer: [u8; 6]) -> io::Result<()> {
        let flags = [1u32 << STA_FLAG_AUTHORIZED, 1 << STA_FLAG_AUTHORIZED];
        let update: Vec<u8> = flags.iter().flat_map(|f| f.to_ne_bytes()).collect();
        self.call(self.command(cmd::SET_STATION).attr(attr::MAC, &peer).attr(attr::STA_FLAGS2, &update)).map(drop)
    }

    /// Sends a management frame on `freq`.
    pub fn send_frame(&mut self, freq: u32, frame: &[u8]) -> io::Result<()> {
        let msg =
            self.command(cmd::FRAME).u32(attr::WIPHY_FREQ, freq).attr(attr::FRAME, frame).flag(attr::DONT_WAIT_FOR_ACK);
        self.call(msg).map(drop)
    }

    /// The signal of the access point `peer` (dBm).
    pub fn signal(&mut self, peer: [u8; 6]) -> io::Result<Option<i8>> {
        let replies = self.call(self.command(cmd::GET_STATION).attr(attr::MAC, &peer))?;
        let info = replies.first().and_then(|r| r.attrs().nested(attr::STA_INFO).map(|i| i.0.to_vec()));
        let info = info.unwrap_or_default();
        let i = Attrs(&info);
        Ok(i.u8(sta_info::SIGNAL_AVG).or(i.u8(sta_info::SIGNAL)).map(|s| s as i8))
    }

    /// The interface's events since last asked.
    pub fn events(&mut self) -> io::Result<Vec<Event>> {
        let mut out = Vec::new();
        for m in self.socket.events()? {
            if m.kind != self.family {
                continue;
            }
            let a = m.attrs();
            if a.u32(attr::IFINDEX) != Some(self.ifindex) {
                continue;
            }
            let frame = || a.get(attr::FRAME).map(<[u8]>::to_vec);
            let peer = a.get(attr::MAC).and_then(mac);
            let event = match m.cmd {
                cmd::NEW_SCAN_RESULTS | cmd::SCAN_ABORTED => Some(Event::ScanDone),
                cmd::AUTHENTICATE | cmd::ASSOCIATE if a.has(attr::TIMED_OUT) => peer.map(Event::TimedOut),
                cmd::AUTHENTICATE => frame().map(Event::Authentication),
                cmd::ASSOCIATE => frame().map(Event::Association),
                cmd::DEAUTHENTICATE | cmd::DISASSOCIATE => frame().map(Event::Left),
                cmd::UNPROT_DEAUTHENTICATE | cmd::UNPROT_DISASSOCIATE => frame().map(Event::Unprotected),
                cmd::CONNECT => Some(Event::Connect { status: a.u16(attr::STATUS_CODE).unwrap_or(1) }),
                cmd::DISCONNECT => Some(Event::Disconnect),
                cmd::CONTROL_PORT_FRAME if a.u16(attr::CONTROL_PORT_ETHERTYPE) == Some(EAPOL) => {
                    match (peer, frame()) {
                        (Some(source), Some(frame)) => Some(Event::Eapol { source, frame }),
                        _ => None,
                    }
                }
                cmd::FRAME => frame().map(Event::Frame),
                _ => None,
            };
            out.extend(event);
        }
        Ok(out)
    }
}
