//! The station: joining an access point and exchanging data with it.
//!
//! [`Station`] is a state machine without I/O. The caller feeds it
//! received frames, timer ticks and commands, and carries out the
//! [`Action`]s it returns (transmit a frame, tune the radio, deliver an
//! Ethernet frame, report an event). Time is in milliseconds on the
//! caller's monotonic clock.
//!
//! Joining: authentication (open system, or SAE for WPA3), association,
//! then the 4-way handshake for protected networks. Connected: data frames
//! are converted to and from Ethernet frames and protected with CCMP;
//! received frames are checked for the right key, replays and
//! duplicates; deauthentication and disassociation end the connection
//! (when management frames are protected only if they are protected, with
//! an SA Query to check on the AP otherwise); and a connection whose AP
//! stops being heard is dropped.
//!
//! A station of a *managed* radio ([`Station::managed`]) decides and
//! authenticates the same, but the radio is the MLME: it sends the
//! authentication and association frames ([`Action::Authenticate`],
//! [`Action::Associate`]), protects and checks frames, retries, and
//! watches the link. The station still does SAE and the key handshakes:
//! the radio gets the session keys only ([`Action::InstallKey`]). Data is
//! Ethernet both ways; EAPOL goes through the radio's control
//! ([`Action::SendEapol`]), in order with the keys.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ccmp::{self, CCMP_OVERHEAD};
use crate::crypto::{KeyAlgo, Random, pbkdf2_psk, psk_from_hex};
use crate::eapol;
use crate::frame::{
    self, AssocRespBody, AuthBody, Header, Mac, action, auth_alg, capab, is_group, mgmt, reason, status,
};
use crate::handshake::{Event as HsEvent, HandshakeError, Supplicant, SupplicantConfig};
use crate::ie::{self, Elements};
use crate::rsn::{Akm, Pmf, Rsne, Security, select};
use crate::sae::{self, Sae, SaeError};
use crate::scan::Bss;

/// Retries of each authentication or association frame.
const MAX_TRIES: u32 = 4;
/// Wait for an authentication or association response.
const RESPONSE_TIMEOUT_MS: u64 = 400;
/// SAE needs more time (the peer does elliptic curve work).
const SAE_TIMEOUT_MS: u64 = 1500;
/// Wait for the 4-way handshake to finish after association.
const HANDSHAKE_TIMEOUT_MS: u64 = 6000;
/// A managed radio retries and times out itself; this is in case it never
/// says.
const MLME_TIMEOUT_MS: u64 = 8000;
/// Without any frame from the AP for this long, probe it...
const LINK_IDLE_MS: u64 = 3000;
/// ...and give up if it still does not answer.
const LINK_PROBE_MS: u64 = 1500;
/// How long to wait for an SA Query response.
const SA_QUERY_TIMEOUT_MS: u64 = 1000;

/// The credentials for a network.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    None,
    /// A passphrase (8-63 characters) or 64 hexadecimal digits.
    Passphrase(String),
}

impl core::fmt::Debug for Credential {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Credential::None => "None",
            Credential::Passphrase(_) => "Passphrase(..)",
        })
    }
}

/// What to join.
#[derive(Debug, Clone)]
pub struct Target {
    pub bss: Bss,
    /// The network's SSID (needed when the beacon hides it).
    pub ssid: Vec<u8>,
    pub credential: Credential,
    /// Allow WPA3 (SAE) when the network offers it.
    pub allow_sae: bool,
}

/// Why joining failed or a connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The password is wrong (the handshake or SAE confirm failed).
    WrongPassword,
    /// No answer from the AP.
    NoResponse,
    /// The AP refused authentication (status code).
    AuthRejected(u16),
    /// The AP refused association (status code).
    AssocRejected(u16),
    /// The network's security cannot be used.
    Unsupported,
    /// A password is needed.
    PasswordRequired,
    /// The AP ended the connection (reason code).
    Deauthenticated(u16),
    /// The AP stopped answering.
    SignalLost,
    /// The handshake failed for another reason (e.g. the RSN element in
    /// message 3 differed from the advertised one).
    HandshakeFailed,
    /// We disconnected.
    Local,
}

/// Progress reported to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaEvent {
    Authenticating,
    Associating,
    Securing,
    Connected {
        bssid: Mac,
        channel: u8,
        security: Security,
        pmf: bool,
        /// WPA3 (SAE) was used.
        sae: bool,
    },
    /// A connection attempt failed.
    JoinFailed(Failure),
    /// An established connection ended.
    Disconnected(Failure),
}

/// The keys a managed radio is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// The pairwise key (CCMP-128), index 0.
    Pairwise,
    /// A group key (CCMP-128), index 1 to 3.
    Group,
    /// The integrity group key (BIP-CMAC-128), index 4 or 5.
    Integrity,
}

/// What the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Transmit an 802.11 frame (`no_ack`: group addressed).
    Transmit {
        frame: Vec<u8>,
        no_ack: bool,
    },
    /// Tune the radio.
    SetChannel(u8),
    /// A received Ethernet frame for the network stack.
    Deliver(Vec<u8>),
    Event(StaEvent),
    /// Managed radios: authenticate with `body` (an Authentication frame's:
    /// algorithm, transaction, status, the algorithm's data).
    Authenticate {
        bssid: Mac,
        channel: u8,
        ssid: Vec<u8>,
        body: Vec<u8>,
    },
    /// Managed radios: associate, with `ies` (the RSN and RSNX elements).
    Associate {
        bssid: Mac,
        channel: u8,
        ssid: Vec<u8>,
        ies: Vec<u8>,
        pmf: bool,
    },
    /// Managed radios: leave the access point.
    Deauthenticate {
        bssid: Mac,
        reason: u16,
    },
    /// Managed radios: send an EAPOL frame (encrypted with the pairwise
    /// key if `encrypt`).
    SendEapol {
        peer: Mac,
        frame: Vec<u8>,
        encrypt: bool,
    },
    /// Managed radios: install a key for the connection to `peer`.
    InstallKey {
        kind: KeyKind,
        index: u8,
        key: Vec<u8>,
        rsc: u64,
        peer: Mac,
    },
    /// Managed radios: data other than EAPOL may pass.
    Authorize {
        peer: Mac,
    },
    /// Managed radios: send a management frame (protected by the radio).
    SendManagement(Vec<u8>),
    /// Managed radios: send an Ethernet frame.
    SendEthernet(Vec<u8>),
}

/// Counters for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaCounters {
    pub beacons: u64,
    pub data_tx: u64,
    pub data_rx: u64,
    pub decrypt_errors: u64,
    pub replays: u64,
    pub duplicates: u64,
    pub unprotected_dropped: u64,
    pub group_rekeys: u64,
}

/// Keys and counters of an established connection.
struct Keys {
    tk: [u8; 16],
    tx_pn: u64,
    /// Highest PN received per traffic identifier (index 16: management).
    rx_pn: [Option<u64>; 17],
    gtk: BTreeMap<u8, ([u8; 16], Option<u64>)>,
    igtk: Option<(u16, [u8; 16], u64)>,
}

impl Drop for Keys {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.tk.zeroize();
        for (k, _) in self.gtk.values_mut() {
            k.zeroize();
        }
        if let Some((_, k, _)) = &mut self.igtk {
            k.zeroize();
        }
    }
}

enum Phase {
    Idle,
    OpenAuth { tries: u32, deadline: u64 },
    SaeCommit { sae: Sae, tries: u32, deadline: u64, token: Option<Vec<u8>> },
    SaeConfirm { sae: Sae, tries: u32, deadline: u64 },
    Assoc { tries: u32, deadline: u64 },
    Handshake { deadline: u64 },
    Connected,
}

/// The station state machine.
pub struct Station {
    mac: Mac,
    /// The radio is the MLME ([`Station::managed`]).
    managed: bool,
    /// Managed: the pairwise key is the radio's (EAPOL goes encrypted).
    ptk_installed: bool,
    phase: Phase,
    target: Option<Target>,
    /// PMK of the connection (PSK or SAE).
    pmk: Option<[u8; 32]>,
    akm: Option<Akm>,
    pmf: bool,
    own_rsne: Option<Vec<u8>>,
    supplicant: Option<Supplicant>,
    keys: Option<Keys>,
    seq: u16,
    last_heard: u64,
    probing_since: Option<u64>,
    sa_query: Option<(u16, u64)>,
    /// Last sequence number per (source, TID) for duplicate detection.
    last_seq: BTreeMap<(Mac, u8), u16>,
    pub counters: StaCounters,
    pub signal_dbm: i8,
    msg1_seen: u32,
}

impl Drop for Station {
    fn drop(&mut self) {
        self.forget_pmk();
    }
}

impl Station {
    /// A station of a radio that moves frames (soft MAC).
    pub fn new(mac: Mac) -> Station {
        Station {
            mac,
            managed: false,
            ptk_installed: false,
            phase: Phase::Idle,
            target: None,
            pmk: None,
            akm: None,
            pmf: false,
            own_rsne: None,
            supplicant: None,
            keys: None,
            seq: 0,
            last_heard: 0,
            probing_since: None,
            sa_query: None,
            last_seq: BTreeMap::new(),
            counters: StaCounters::default(),
            signal_dbm: 0,
            msg1_seen: 0,
        }
    }

    /// A station of a managed radio (the MLME).
    pub fn managed(mac: Mac) -> Station {
        let mut s = Station::new(mac);
        s.managed = true;
        s
    }

    pub fn mac(&self) -> Mac {
        self.mac
    }

    pub fn is_managed(&self) -> bool {
        self.managed
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.phase, Phase::Connected)
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    /// The BSS joined or being joined.
    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    fn next_seq(&mut self) -> u16 {
        self.seq = (self.seq + 1) & 0xFFF;
        self.seq
    }

    fn bssid(&self) -> Mac {
        self.target.as_ref().map(|t| t.bss.bssid).unwrap_or([0; 6])
    }

    fn mgmt_frame(&mut self, subtype: u8, body: &[u8]) -> Vec<u8> {
        let bssid = self.bssid();
        let seq = self.next_seq();
        frame::management(subtype, &bssid, &self.mac, &bssid, seq, body)
    }

    fn tx(frame: Vec<u8>) -> Action {
        Action::Transmit { frame, no_ack: false }
    }

    fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.ptk_installed = false;
        self.forget_pmk();
        self.akm = None;
        self.pmf = false;
        self.own_rsne = None;
        self.supplicant = None;
        self.keys = None;
        self.probing_since = None;
        self.sa_query = None;
        self.last_seq.clear();
        self.msg1_seen = 0;
    }

    fn forget_pmk(&mut self) {
        use zeroize::Zeroize;
        if let Some(p) = self.pmk.as_mut() {
            p.zeroize();
        }
        self.pmk = None;
    }

    fn fail(&mut self, why: Failure) -> Vec<Action> {
        let connected = self.is_connected();
        self.reset();
        let ev = if connected { StaEvent::Disconnected(why) } else { StaEvent::JoinFailed(why) };
        alloc::vec![Action::Event(ev)]
    }

    /// Starts joining `target`.
    pub fn connect(&mut self, target: Target, now: u64, rng: &mut dyn Random) -> Vec<Action> {
        let mut out = self.disconnect(now);
        out.retain(|a| matches!(a, Action::Transmit { .. }));
        self.reset();
        // As the scan heard it, until the access point's frames (or a
        // managed radio) tell.
        self.signal_dbm = target.bss.signal_dbm;
        let sec = target.bss.security;
        if !sec.supported() {
            self.target = Some(target);
            out.extend(self.fail(Failure::Unsupported));
            return out;
        }
        if !self.managed {
            out.push(Action::SetChannel(target.bss.channel));
        }
        self.last_heard = now;
        if sec == Security::Open {
            self.target = Some(target);
            out.extend(self.start_open_auth(now));
            return out;
        }
        let Some(rsne) = target.bss.rsne.as_ref().and_then(|e| Rsne::parse(e.get(2..)?).ok()) else {
            self.target = Some(target);
            out.extend(self.fail(Failure::Unsupported));
            return out;
        };
        let Credential::Passphrase(pass) = &target.credential else {
            self.target = Some(target);
            out.extend(self.fail(Failure::PasswordRequired));
            return out;
        };
        let pass = pass.clone();
        let Ok(sel) = select(&rsne, target.allow_sae) else {
            self.target = Some(target);
            out.extend(self.fail(Failure::Unsupported));
            return out;
        };
        self.akm = Some(sel.akm);
        self.pmf = sel.pmf == Pmf::On;
        self.own_rsne = Some(sel.own_rsne);
        let ssid = target.ssid.clone();
        let (bssid, h2e) = (target.bss.bssid, target.bss.h2e);
        self.target = Some(target);
        if sel.akm == Akm::Sae {
            let pwe = if h2e {
                let pt = sae::derive_pt(&ssid, pass.as_bytes(), None);
                sae::pwe_from_pt(&pt, &self.mac, &bssid)
            } else {
                sae::pwe_hunting_and_pecking(&self.mac, &bssid, pass.as_bytes(), None)
            };
            let Ok(pwe) = pwe else {
                out.extend(self.fail(Failure::HandshakeFailed));
                return out;
            };
            let sae = Sae::new(pwe, h2e, rng);
            out.push(Action::Event(StaEvent::Authenticating));
            out.push(self.sae_commit_frame(&sae, None));
            self.phase = Phase::SaeCommit { sae, tries: 1, deadline: now + self.sae_timeout(), token: None };
        } else {
            let psk = psk_from_hex(&pass).unwrap_or_else(|| pbkdf2_psk(pass.as_bytes(), &ssid));
            self.pmk = Some(psk);
            out.extend(self.start_open_auth(now));
        }
        out
    }

    /// How long to wait for an answer to an authentication or an
    /// association (a managed radio retries itself).
    fn response_timeout(&self) -> u64 {
        if self.managed { MLME_TIMEOUT_MS } else { RESPONSE_TIMEOUT_MS }
    }

    fn sae_timeout(&self) -> u64 {
        if self.managed { MLME_TIMEOUT_MS } else { SAE_TIMEOUT_MS }
    }

    /// An Authentication frame with `body`: transmitted, or (managed) the
    /// radio's to send.
    fn auth(&mut self, body: Vec<u8>) -> Action {
        if self.managed {
            let t = self.target.as_ref().expect("target");
            Action::Authenticate { bssid: t.bss.bssid, channel: t.bss.channel, ssid: t.ssid.clone(), body }
        } else {
            Self::tx(self.mgmt_frame(mgmt::AUTH, &body))
        }
    }

    fn start_open_auth(&mut self, now: u64) -> Vec<Action> {
        let body = AuthBody::build(auth_alg::OPEN, 1, status::SUCCESS, &[]);
        let a = self.auth(body);
        self.phase = Phase::OpenAuth { tries: 1, deadline: now + self.response_timeout() };
        alloc::vec![Action::Event(StaEvent::Authenticating), a]
    }

    fn sae_commit_frame(&mut self, sae: &Sae, token: Option<&[u8]>) -> Action {
        let st = if sae.h2e { status::SAE_HASH_TO_ELEMENT } else { status::SUCCESS };
        let body = AuthBody::build(auth_alg::SAE, 1, st, &sae.commit(token));
        self.auth(body)
    }

    fn sae_confirm_frame(&mut self, sae: &mut Sae) -> Option<Action> {
        let confirm = sae.confirm().ok()?;
        let body = AuthBody::build(auth_alg::SAE, 2, status::SUCCESS, &confirm);
        Some(self.auth(body))
    }

    fn assoc_frame(&mut self) -> Vec<u8> {
        let t = self.target.as_ref().expect("target");
        let mut cap = capab::ESS | capab::SHORT_SLOT_TIME;
        if t.bss.security != Security::Open {
            cap |= capab::PRIVACY;
        }
        let rates = if t.bss.rates.is_empty() { ie::rates_for(t.bss.channel).to_vec() } else { t.bss.rates.clone() };
        let mut b = ie::Builder::new().ssid(&t.ssid).rates(&rates);
        if let Some(r) = &self.own_rsne {
            b = b.raw(r);
        }
        if self.akm == Some(Akm::Sae) && t.bss.h2e {
            b = b.element(ie::id::RSNX, &[ie::rsnx::SAE_H2E]);
        }
        let body = frame::AssocReqBody::build(cap, 10, &b.build());
        self.mgmt_frame(mgmt::ASSOC_REQ, &body)
    }

    /// The association: a frame, or (managed) the radio's to make, with
    /// the elements that are the station's.
    fn assoc(&mut self) -> Action {
        if !self.managed {
            return Self::tx(self.assoc_frame());
        }
        let t = self.target.as_ref().expect("target");
        let mut ies = self.own_rsne.clone().unwrap_or_default();
        if self.akm == Some(Akm::Sae) && t.bss.h2e {
            ies.extend_from_slice(&[ie::id::RSNX, 1, ie::rsnx::SAE_H2E]);
        }
        Action::Associate { bssid: t.bss.bssid, channel: t.bss.channel, ssid: t.ssid.clone(), ies, pmf: self.pmf }
    }

    fn start_assoc(&mut self, now: u64) -> Vec<Action> {
        let a = self.assoc();
        self.phase = Phase::Assoc { tries: 1, deadline: now + self.response_timeout() };
        alloc::vec![Action::Event(StaEvent::Associating), a]
    }

    /// Leaves the network (sends a deauthentication when connected).
    pub fn disconnect(&mut self, _now: u64) -> Vec<Action> {
        let mut out = Vec::new();
        if matches!(self.phase, Phase::Idle) {
            return out;
        }
        if self.managed {
            // The radio forgets whatever it had with the access point.
            out.push(Action::Deauthenticate { bssid: self.bssid(), reason: reason::DEAUTH_LEAVING });
        } else if matches!(self.phase, Phase::Assoc { .. } | Phase::Handshake { .. } | Phase::Connected) {
            let body = reason::DEAUTH_LEAVING.to_le_bytes();
            let f = self.mgmt_frame(mgmt::DEAUTH, &body);
            if let Some(p) = self.protect_mgmt(f) {
                out.push(Self::tx(p));
            }
        }
        out.extend(self.fail(Failure::Local));
        out
    }

    /// Protects a robust management frame when PMF is in use.
    fn protect_mgmt(&mut self, f: Vec<u8>) -> Option<Vec<u8>> {
        match (&mut self.keys, self.pmf) {
            (Some(k), true) => {
                k.tx_pn += 1;
                ccmp::ccmp_encrypt(&k.tk, &f, k.tx_pn, 0)
            }
            _ => Some(f),
        }
    }

    /// Sends an Ethernet frame (destination, source, EtherType, payload)
    /// to the network. Frames are dropped unless connected.
    pub fn send_ethernet(&mut self, eth: &[u8]) -> Vec<Action> {
        if !self.is_connected() || eth.len() < 14 {
            return Vec::new();
        }
        if self.managed {
            self.counters.data_tx += 1;
            return alloc::vec![Action::SendEthernet(eth.to_vec())];
        }
        let dst: Mac = eth[0..6].try_into().unwrap();
        let ethertype = u16::from_be_bytes([eth[12], eth[13]]);
        self.send_data(&dst, ethertype, &eth[14..])
    }

    fn send_data(&mut self, dst: &Mac, ethertype: u16, payload: &[u8]) -> Vec<Action> {
        let bssid = self.bssid();
        let seq = self.next_seq();
        let plain = frame::data_to_ds(&bssid, &self.mac, dst, seq, ethertype, payload);
        let f = match &mut self.keys {
            Some(k) => {
                k.tx_pn += 1;
                match ccmp::ccmp_encrypt(&k.tk, &plain, k.tx_pn, 0) {
                    Some(f) => f,
                    None => return Vec::new(),
                }
            }
            None if self.target.as_ref().is_some_and(|t| t.bss.security == Security::Open) => plain,
            // Before the keys are in place only EAPOL may go out, in clear.
            None if ethertype == eapol::ETHERTYPE => plain,
            None => return Vec::new(),
        };
        self.counters.data_tx += 1;
        alloc::vec![Self::tx(f)]
    }

    /// When [`Station::tick`] should run next.
    pub fn next_deadline(&self) -> Option<u64> {
        let phase = match &self.phase {
            Phase::Idle => None,
            Phase::OpenAuth { deadline, .. }
            | Phase::SaeCommit { deadline, .. }
            | Phase::SaeConfirm { deadline, .. }
            | Phase::Assoc { deadline, .. }
            | Phase::Handshake { deadline } => Some(*deadline),
            // A managed radio watches the link itself.
            Phase::Connected if self.managed => None,
            Phase::Connected => Some(match self.probing_since {
                Some(t) => t + LINK_PROBE_MS,
                None => self.last_heard + LINK_IDLE_MS,
            }),
        };
        match (phase, self.sa_query) {
            (Some(a), Some((_, b))) => Some(a.min(b)),
            (a, b) => a.or(b.map(|(_, t)| t)),
        }
    }

    /// Handles timeouts.
    pub fn tick(&mut self, now: u64, _rng: &mut dyn Random) -> Vec<Action> {
        if let Some((_, deadline)) = self.sa_query
            && now >= deadline
        {
            // The AP did not answer the SA Query: the deauthentication
            // that started it was genuine.
            self.sa_query = None;
            return self.fail(Failure::Deauthenticated(reason::PREV_AUTH_NOT_VALID));
        }
        if self.managed {
            return match &self.phase {
                Phase::OpenAuth { deadline, .. }
                | Phase::SaeCommit { deadline, .. }
                | Phase::SaeConfirm { deadline, .. }
                | Phase::Assoc { deadline, .. }
                    if now >= *deadline =>
                {
                    self.mlme_timeout(now)
                }
                Phase::Handshake { deadline } if now >= *deadline => self.handshake_timed_out(),
                _ => Vec::new(),
            };
        }
        match &mut self.phase {
            Phase::Idle => Vec::new(),
            Phase::OpenAuth { tries, deadline } if now >= *deadline => {
                if *tries >= MAX_TRIES {
                    return self.fail(Failure::NoResponse);
                }
                *tries += 1;
                *deadline = now + RESPONSE_TIMEOUT_MS;
                let body = AuthBody::build(auth_alg::OPEN, 1, status::SUCCESS, &[]);
                alloc::vec![Self::tx(self.mgmt_frame(mgmt::AUTH, &body))]
            }
            Phase::SaeCommit { tries, deadline, .. } if now >= *deadline => {
                if *tries >= MAX_TRIES {
                    return self.fail(Failure::NoResponse);
                }
                let Phase::SaeCommit { sae, tries, token, .. } = core::mem::replace(&mut self.phase, Phase::Idle)
                else {
                    unreachable!()
                };
                let a = self.sae_commit_frame(&sae, token.as_deref());
                self.phase = Phase::SaeCommit { sae, tries: tries + 1, deadline: now + SAE_TIMEOUT_MS, token };
                alloc::vec![a]
            }
            Phase::SaeConfirm { tries, deadline, .. } if now >= *deadline => {
                if *tries >= MAX_TRIES {
                    // The AP answered our commit but never confirmed: the
                    // passwords differ.
                    return self.fail(Failure::WrongPassword);
                }
                *tries += 1;
                *deadline = now + SAE_TIMEOUT_MS;
                let Phase::SaeConfirm { mut sae, tries, deadline } = core::mem::replace(&mut self.phase, Phase::Idle)
                else {
                    unreachable!()
                };
                let a = self.sae_confirm_frame(&mut sae);
                self.phase = Phase::SaeConfirm { sae, tries, deadline };
                a.into_iter().collect()
            }
            Phase::Assoc { tries, deadline } if now >= *deadline => {
                if *tries >= MAX_TRIES {
                    return self.fail(Failure::NoResponse);
                }
                *tries += 1;
                *deadline = now + RESPONSE_TIMEOUT_MS;
                alloc::vec![Self::tx(self.assoc_frame())]
            }
            Phase::Handshake { deadline } if now >= *deadline => self.handshake_timed_out(),
            Phase::Connected => {
                if let Some(since) = self.probing_since {
                    if now >= since + LINK_PROBE_MS {
                        return self.fail(Failure::SignalLost);
                    }
                    Vec::new()
                } else if now >= self.last_heard + LINK_IDLE_MS {
                    // Nothing from the AP for a while: ask it directly.
                    self.probing_since = Some(now);
                    let bssid = self.bssid();
                    let seq = self.next_seq();
                    let f = frame::null_to_ds(&bssid, &self.mac, seq, false);
                    let mut out = alloc::vec![Self::tx(f)];
                    let (ssid, channel) =
                        self.target.as_ref().map(|t| (t.ssid.clone(), t.bss.channel)).unwrap_or_default();
                    let ies = ie::Builder::new().ssid(&ssid).rates(ie::rates_for(channel)).build();
                    let seq = self.next_seq();
                    out.push(Self::tx(frame::management(mgmt::PROBE_REQ, &bssid, &self.mac, &bssid, seq, &ies)));
                    out
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn handshake_timed_out(&mut self) -> Vec<Action> {
        // Message 1 kept coming but message 3 never did: the AP could not
        // verify our message 2, i.e. the passphrase.
        let wrong = self.supplicant.as_ref().is_some_and(|s| s.awaiting_msg3()) && self.msg1_seen >= 2;
        self.fail(if wrong { Failure::WrongPassword } else { Failure::HandshakeFailed })
    }

    /// Managed radios: the access point did not answer an authentication
    /// or an association.
    pub fn mlme_timeout(&mut self, _now: u64) -> Vec<Action> {
        match self.phase {
            // It answered our commit but never confirmed: the passwords
            // differ.
            Phase::SaeConfirm { .. } => self.fail(Failure::WrongPassword),
            Phase::OpenAuth { .. } | Phase::SaeCommit { .. } | Phase::Assoc { .. } => self.fail(Failure::NoResponse),
            _ => Vec::new(),
        }
    }

    /// Managed radios: the radio lost the access point.
    pub fn link_lost(&mut self, _now: u64) -> Vec<Action> {
        if matches!(self.phase, Phase::Idle) {
            return Vec::new();
        }
        self.fail(Failure::SignalLost)
    }

    /// Managed radios: an unprotected deauthentication or disassociation
    /// came while management frames are protected: ask the access point
    /// (SA Query) whether it really ended the association.
    pub fn unprotected_deauth(&mut self, now: u64) -> Vec<Action> {
        self.counters.unprotected_dropped += 1;
        self.start_sa_query(now)
    }

    /// Managed radios: the signal of the access point joined.
    pub fn set_signal(&mut self, dbm: i8) {
        self.signal_dbm = dbm;
    }

    /// Managed radios: an Ethernet frame the radio received (checked and
    /// decrypted by it): EAPOL for the handshake, data for the network.
    pub fn receive_ethernet(&mut self, eth: &[u8], now: u64, rng: &mut dyn Random) -> Vec<Action> {
        if !self.managed || eth.len() < 14 || self.target.is_none() {
            return Vec::new();
        }
        let ethertype = u16::from_be_bytes([eth[12], eth[13]]);
        if ethertype == eapol::ETHERTYPE {
            if eth[6..12] != self.bssid() {
                return Vec::new();
            }
            return self.receive_eapol(&eth[14..], now, rng);
        }
        if !self.is_connected() {
            return Vec::new();
        }
        self.counters.data_rx += 1;
        alloc::vec![Action::Deliver(eth.to_vec())]
    }

    /// Handles a received frame.
    pub fn receive(&mut self, f: &[u8], signal_dbm: i8, now: u64, rng: &mut dyn Random) -> Vec<Action> {
        let Some(h) = Header::parse(f) else { return Vec::new() };
        if self.target.is_none() || h.addr2 != self.bssid() {
            return Vec::new();
        }
        if !is_group(&h.addr1) && h.addr1 != self.mac {
            return Vec::new();
        }
        self.last_heard = now;
        self.probing_since = None;
        if !self.managed {
            self.signal_dbm = signal_dbm;
        }
        if h.fc.is_management() {
            self.receive_mgmt(&h, f, now, rng)
        } else if h.fc.is_data() && !self.managed {
            self.receive_data(&h, f, now, rng)
        } else {
            Vec::new()
        }
    }

    fn receive_mgmt(&mut self, h: &Header, f: &[u8], now: u64, rng: &mut dyn Random) -> Vec<Action> {
        match h.fc.subtype() {
            mgmt::BEACON => {
                self.counters.beacons += 1;
                Vec::new()
            }
            mgmt::AUTH if !h.fc.protected() => self.receive_auth(&f[h.len..], now, rng),
            mgmt::ASSOC_RESP | mgmt::REASSOC_RESP if !h.fc.protected() => self.receive_assoc_resp(&f[h.len..], now),
            mgmt::DEAUTH | mgmt::DISASSOC => self.receive_deauth(h, f, now),
            mgmt::ACTION => self.receive_action(h, f),
            _ => Vec::new(),
        }
    }

    fn receive_auth(&mut self, body: &[u8], now: u64, _rng: &mut dyn Random) -> Vec<Action> {
        let Some(a) = AuthBody::parse(body) else { return Vec::new() };
        match &mut self.phase {
            Phase::OpenAuth { .. } if a.algorithm == auth_alg::OPEN && a.transaction == 2 => {
                if a.status != status::SUCCESS {
                    return self.fail(Failure::AuthRejected(a.status));
                }
                self.start_assoc(now)
            }
            Phase::SaeCommit { .. } if a.algorithm == auth_alg::SAE && a.transaction == 1 => {
                match a.status {
                    status::ANTI_CLOGGING_TOKEN_REQ => {
                        // Send the commit again with the AP's token.
                        let token = if a.rest.len() > 2 { a.rest[2..].to_vec() } else { Vec::new() };
                        let Phase::SaeCommit { sae, tries, .. } = core::mem::replace(&mut self.phase, Phase::Idle)
                        else {
                            unreachable!()
                        };
                        let token = if sae.h2e {
                            Elements::parse(&a.rest[2.min(a.rest.len())..])
                                .ok()
                                .and_then(|e| e.anti_clogging_token.map(|t| t.to_vec()))
                                .unwrap_or(token)
                        } else {
                            token
                        };
                        let act = self.sae_commit_frame(&sae, Some(&token));
                        self.phase =
                            Phase::SaeCommit { sae, tries, deadline: now + SAE_TIMEOUT_MS, token: Some(token) };
                        alloc::vec![act]
                    }
                    status::SUCCESS | status::SAE_HASH_TO_ELEMENT => {
                        let Phase::SaeCommit { mut sae, .. } = core::mem::replace(&mut self.phase, Phase::Idle) else {
                            unreachable!()
                        };
                        if (a.status == status::SAE_HASH_TO_ELEMENT) != sae.h2e {
                            return self.fail(Failure::HandshakeFailed);
                        }
                        match sae.process_commit(a.rest, 0) {
                            Ok(()) => {
                                let act = self.sae_confirm_frame(&mut sae);
                                self.phase = Phase::SaeConfirm { sae, tries: 1, deadline: now + SAE_TIMEOUT_MS };
                                act.into_iter().collect()
                            }
                            Err(SaeError::UnsupportedGroup) => self.fail(Failure::Unsupported),
                            Err(_) => self.fail(Failure::HandshakeFailed),
                        }
                    }
                    status::FINITE_CYCLIC_GROUP_NOT_SUPPORTED => self.fail(Failure::Unsupported),
                    other => self.fail(Failure::AuthRejected(other)),
                }
            }
            Phase::SaeConfirm { sae, .. } if a.algorithm == auth_alg::SAE && a.transaction == 2 => {
                if a.status != status::SUCCESS {
                    return self.fail(Failure::AuthRejected(a.status));
                }
                match sae.verify_confirm(a.rest) {
                    Ok(()) => {
                        let (pmk, _) = sae.keys().expect("keys after commit");
                        self.pmk = Some(pmk);
                        self.start_assoc(now)
                    }
                    Err(_) => self.fail(Failure::WrongPassword),
                }
            }
            Phase::SaeConfirm { .. } if a.algorithm == auth_alg::SAE && a.transaction == 1 => {
                // The AP repeated its commit: resend our confirm.
                let Phase::SaeConfirm { mut sae, tries, deadline } = core::mem::replace(&mut self.phase, Phase::Idle)
                else {
                    unreachable!()
                };
                let act = self.sae_confirm_frame(&mut sae);
                self.phase = Phase::SaeConfirm { sae, tries, deadline };
                act.into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    fn receive_assoc_resp(&mut self, body: &[u8], now: u64) -> Vec<Action> {
        if !matches!(self.phase, Phase::Assoc { .. }) {
            return Vec::new();
        }
        let Some(r) = AssocRespBody::parse(body) else { return Vec::new() };
        if r.status != status::SUCCESS {
            return self.fail(Failure::AssocRejected(r.status));
        }
        let t = self.target.as_ref().expect("target");
        if t.bss.security == Security::Open {
            return self.connected();
        }
        let (Some(akm), Some(pmk), Some(own)) = (self.akm, self.pmk, self.own_rsne.clone()) else {
            return self.fail(Failure::HandshakeFailed);
        };
        let algo = akm.key_algo().unwrap_or(KeyAlgo::Sha1);
        let rsnx_own = (akm == Akm::Sae && t.bss.h2e).then(|| alloc::vec![ie::id::RSNX, 1, ie::rsnx::SAE_H2E]);
        self.supplicant = Some(Supplicant::new(SupplicantConfig {
            algo,
            descriptor_version: eapol::descriptor_version(akm),
            pmk,
            aa: t.bss.bssid,
            spa: self.mac,
            own_rsne: own,
            own_rsnxe: rsnx_own,
            ap_rsne: t.bss.rsne.clone().unwrap_or_default(),
            ap_rsnxe: if akm == Akm::Sae { t.bss.rsnx.clone() } else { None },
            pmf: self.pmf,
        }));
        self.phase = Phase::Handshake { deadline: now + HANDSHAKE_TIMEOUT_MS };
        alloc::vec![Action::Event(StaEvent::Securing)]
    }

    fn connected(&mut self) -> Vec<Action> {
        self.phase = Phase::Connected;
        let t = self.target.as_ref().expect("target");
        alloc::vec![Action::Event(StaEvent::Connected {
            bssid: t.bss.bssid,
            channel: t.bss.channel,
            security: t.bss.security,
            pmf: self.pmf,
            sae: self.akm == Some(Akm::Sae),
        })]
    }

    fn receive_deauth(&mut self, h: &Header, f: &[u8], now: u64) -> Vec<Action> {
        if matches!(self.phase, Phase::Idle) {
            return Vec::new();
        }
        let keys_ready = self.keys.is_some();
        let body: Vec<u8> = if self.managed {
            // The radio checked it (and reports an unprotected one where
            // it must be protected apart).
            f[h.len..].to_vec()
        } else if self.pmf && keys_ready {
            if is_group(&h.addr1) {
                // Group addressed: must carry a valid BIP MIC.
                let Some(k) = &mut self.keys else { return Vec::new() };
                let Some((key_id, ipn)) = ccmp::bip_mmie(f) else {
                    self.counters.unprotected_dropped += 1;
                    return Vec::new();
                };
                let Some((id, igtk, last)) = &mut k.igtk else { return Vec::new() };
                if *id != key_id || ipn <= *last {
                    self.counters.replays += 1;
                    return Vec::new();
                }
                match ccmp::bip_verify(igtk, f) {
                    Some(plain) => {
                        *last = ipn;
                        plain[h.len..].to_vec()
                    }
                    None => {
                        self.counters.decrypt_errors += 1;
                        return Vec::new();
                    }
                }
            } else if h.fc.protected() {
                match self.decrypt_unicast_mgmt(f) {
                    Some(plain) => plain[h.len..].to_vec(),
                    None => return Vec::new(),
                }
            } else {
                // Unprotected while frames should be protected: ask the AP
                // whether it really ended the association (SA Query).
                self.counters.unprotected_dropped += 1;
                return self.start_sa_query(now);
            }
        } else {
            f[h.len..].to_vec()
        };
        let code = frame::reason_code(&body).unwrap_or(reason::UNSPECIFIED);
        // A deauthentication during the handshake with "4-way handshake
        // timeout" after repeated message 1s means the AP rejected our
        // message 2: the passphrase is wrong.
        if matches!(self.phase, Phase::Handshake { .. })
            && self.msg1_seen >= 1
            && matches!(
                code,
                reason::FOURWAY_HANDSHAKE_TIMEOUT | reason::PREV_AUTH_NOT_VALID | reason::IEEE_802_1X_AUTH_FAILED
            )
        {
            return self.fail(Failure::WrongPassword);
        }
        self.fail(Failure::Deauthenticated(code))
    }

    fn start_sa_query(&mut self, now: u64) -> Vec<Action> {
        if self.sa_query.is_some() || !self.is_connected() {
            return Vec::new();
        }
        let id = (now as u16) ^ 0x5A5A;
        self.sa_query = Some((id, now + SA_QUERY_TIMEOUT_MS));
        let mut body = alloc::vec![action::SA_QUERY, action::SA_QUERY_REQUEST];
        body.extend_from_slice(&id.to_le_bytes());
        let f = self.mgmt_frame(mgmt::ACTION, &body);
        if self.managed {
            return alloc::vec![Action::SendManagement(f)];
        }
        self.protect_mgmt(f).map(Self::tx).into_iter().collect()
    }

    fn decrypt_unicast_mgmt(&mut self, f: &[u8]) -> Option<Vec<u8>> {
        let k = self.keys.as_mut()?;
        let (pn, _) = ccmp::ccmp_header(f)?;
        if k.rx_pn[16].is_some_and(|last| pn <= last) {
            self.counters.replays += 1;
            return None;
        }
        match ccmp::ccmp_decrypt(&k.tk, f) {
            Ok((plain, pn)) => {
                k.rx_pn[16] = Some(pn);
                Some(plain)
            }
            Err(_) => {
                self.counters.decrypt_errors += 1;
                None
            }
        }
    }

    fn receive_action(&mut self, h: &Header, f: &[u8]) -> Vec<Action> {
        if is_group(&h.addr1) || !self.is_connected() {
            return Vec::new();
        }
        let plain = if self.managed {
            // Checked and decrypted by the radio.
            f.to_vec()
        } else if self.pmf {
            if !h.fc.protected() {
                self.counters.unprotected_dropped += 1;
                return Vec::new();
            }
            match self.decrypt_unicast_mgmt(f) {
                Some(p) => p,
                None => return Vec::new(),
            }
        } else {
            f.to_vec()
        };
        let body = &plain[h.len..];
        if body.len() < 4 || body[0] != action::SA_QUERY {
            return Vec::new();
        }
        let id = u16::from_le_bytes([body[2], body[3]]);
        match body[1] {
            // A managed radio answers the AP's queries itself.
            action::SA_QUERY_REQUEST if self.managed => Vec::new(),
            action::SA_QUERY_REQUEST => {
                let mut resp = alloc::vec![action::SA_QUERY, action::SA_QUERY_RESPONSE];
                resp.extend_from_slice(&id.to_le_bytes());
                let f = self.mgmt_frame(mgmt::ACTION, &resp);
                self.protect_mgmt(f).map(Self::tx).into_iter().collect()
            }
            action::SA_QUERY_RESPONSE => {
                if self.sa_query.is_some_and(|(q, _)| q == id) {
                    // The AP is still associated with us: the unprotected
                    // deauthentication was not from it.
                    self.sa_query = None;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn receive_data(&mut self, h: &Header, f: &[u8], now: u64, rng: &mut dyn Random) -> Vec<Action> {
        if !h.fc.from_ds() || h.fc.to_ds() {
            return Vec::new();
        }
        if !matches!(h.fc.subtype() & 0x7, frame::data::DATA) || h.fc.subtype() & 0x4 != 0 {
            // Null and CF frames carry no data.
            return Vec::new();
        }
        if h.fc.more_frag() || h.frag != 0 || h.amsdu() {
            // Fragments and A-MSDUs are not negotiated: drop them.
            return Vec::new();
        }
        // Duplicate detection (retransmissions keep the sequence number).
        let key = (h.addr3, h.tid());
        if h.fc.retry() && self.last_seq.get(&key) == Some(&h.seq) {
            self.counters.duplicates += 1;
            return Vec::new();
        }
        let plain: Vec<u8> = if h.fc.protected() {
            match self.decrypt_data(h, f) {
                Some(p) => p,
                None => return Vec::new(),
            }
        } else {
            f.to_vec()
        };
        self.last_seq.insert(key, h.seq);
        let Some((ethertype, payload)) = frame::parse_snap(&plain[h.len..]) else { return Vec::new() };
        if !h.fc.protected() {
            let open = self.target.as_ref().is_some_and(|t| t.bss.security == Security::Open);
            // Without protection only EAPOL before the keys are installed,
            // or anything on an open network.
            if !open && (self.keys.is_some() || ethertype != eapol::ETHERTYPE) {
                self.counters.unprotected_dropped += 1;
                return Vec::new();
            }
        }
        if ethertype == eapol::ETHERTYPE {
            return self.receive_eapol(payload, now, rng);
        }
        if !self.is_connected() || h.addr3 == self.mac {
            return Vec::new();
        }
        self.counters.data_rx += 1;
        let mut eth = Vec::with_capacity(14 + payload.len());
        eth.extend_from_slice(&h.addr1);
        eth.extend_from_slice(&h.addr3);
        eth.extend_from_slice(&ethertype.to_be_bytes());
        eth.extend_from_slice(payload);
        alloc::vec![Action::Deliver(eth)]
    }

    fn decrypt_data(&mut self, h: &Header, f: &[u8]) -> Option<Vec<u8>> {
        let k = self.keys.as_mut()?;
        let (pn, key_id) = ccmp::ccmp_header(f)?;
        if f.len() < h.len + CCMP_OVERHEAD {
            return None;
        }
        if is_group(&h.addr1) {
            let (gtk, last) = k.gtk.get_mut(&key_id)?;
            if last.is_some_and(|l| pn <= l) {
                self.counters.replays += 1;
                return None;
            }
            match ccmp::ccmp_decrypt(gtk, f) {
                Ok((p, pn)) => {
                    *last = Some(pn);
                    Some(p)
                }
                Err(_) => {
                    self.counters.decrypt_errors += 1;
                    None
                }
            }
        } else {
            let tid = h.tid() as usize & 0xF;
            if k.rx_pn[tid].is_some_and(|l| pn <= l) {
                self.counters.replays += 1;
                return None;
            }
            match ccmp::ccmp_decrypt(&k.tk, f) {
                Ok((p, pn)) => {
                    k.rx_pn[tid] = Some(pn);
                    Some(p)
                }
                Err(_) => {
                    self.counters.decrypt_errors += 1;
                    None
                }
            }
        }
    }

    fn receive_eapol(&mut self, payload: &[u8], now: u64, rng: &mut dyn Random) -> Vec<Action> {
        let Some(sup) = self.supplicant.as_mut() else { return Vec::new() };
        if let Ok(kf) = eapol::KeyFrame::parse(payload)
            && kf.has(eapol::info::PAIRWISE)
            && !kf.has(eapol::info::MIC)
        {
            self.msg1_seen += 1;
        }
        let events = match sup.receive(payload, rng) {
            Ok(ev) => ev,
            Err(HandshakeError::RsnMismatch) => return self.fail(Failure::HandshakeFailed),
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        let mut completed = false;
        let bssid = self.bssid();
        for ev in events {
            if self.managed {
                out.extend(self.handshake_managed(ev, &mut completed));
                continue;
            }
            match ev {
                HsEvent::Send(frame) => out.extend(self.send_data(&bssid, eapol::ETHERTYPE, &frame)),
                HsEvent::InstallPtk { tk } => {
                    self.keys = Some(Keys { tk, tx_pn: 0, rx_pn: [None; 17], gtk: BTreeMap::new(), igtk: None });
                }
                HsEvent::InstallGtk { key_id, key, rsc } => {
                    if let (Some(k), Ok(key)) = (self.keys.as_mut(), <[u8; 16]>::try_from(key.as_slice())) {
                        // The RSC is the last PN the AP used: frames must
                        // carry a larger one.
                        let last = if rsc == 0 { None } else { Some(rsc.saturating_sub(1)) };
                        k.gtk.insert(key_id, (key, last));
                        if self.is_connected() {
                            self.counters.group_rekeys += 1;
                        }
                    }
                }
                HsEvent::InstallIgtk { key_id, key, ipn } => {
                    if let Some(k) = self.keys.as_mut() {
                        k.igtk = Some((key_id, key, ipn.saturating_sub(1)));
                    }
                }
                HsEvent::Completed => completed = true,
            }
        }
        let _ = now;
        if completed && matches!(self.phase, Phase::Handshake { .. }) {
            if self.managed {
                out.push(Action::Authorize { peer: bssid });
            }
            out.extend(self.connected());
        }
        out
    }

    /// What a handshake event is for a managed radio: frames and keys for
    /// it.
    fn handshake_managed(&mut self, ev: HsEvent, completed: &mut bool) -> Vec<Action> {
        let peer = self.bssid();
        let key = |kind, index: u8, key: &[u8], rsc| Action::InstallKey { kind, index, key: key.to_vec(), rsc, peer };
        match ev {
            // Message 4 goes in clear before the pairwise key is the
            // radio's: the action that installs it comes after.
            HsEvent::Send(frame) => {
                alloc::vec![Action::SendEapol { peer, frame, encrypt: self.ptk_installed }]
            }
            HsEvent::InstallPtk { tk } => {
                self.ptk_installed = true;
                alloc::vec![key(KeyKind::Pairwise, 0, &tk, 0)]
            }
            HsEvent::InstallGtk { key_id, key: gtk, rsc } => {
                if self.is_connected() {
                    self.counters.group_rekeys += 1;
                }
                alloc::vec![key(KeyKind::Group, key_id, &gtk, rsc)]
            }
            HsEvent::InstallIgtk { key_id, key: igtk, ipn } => {
                alloc::vec![key(KeyKind::Integrity, key_id as u8, &igtk, ipn)]
            }
            HsEvent::Completed => {
                *completed = true;
                Vec::new()
            }
        }
    }
}
