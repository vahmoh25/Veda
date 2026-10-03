//! An access point: what the simulated Wi-Fi networks of `airsim` run, and
//! the counterpart of [`crate::station::Station`] in the tests.
//!
//! Like the station it is a state machine without I/O: the caller feeds
//! frames from the air, Ethernet frames from the wired side and timer
//! ticks, and carries out the returned [`ApAction`]s. It implements
//! beacons and probe responses, open system and SAE authentication,
//! association with RSN negotiation, the authenticator side of the 4-way
//! and group key handshakes, CCMP for unicast and group data, management
//! frame protection, and bridging between stations and the wired side.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ccmp;
use crate::crypto::{KeyAlgo, Random, pbkdf2_psk, psk_from_hex};
use crate::eapol;
use crate::frame::{
    self, AssocReqBody, AssocRespBody, AuthBody, BeaconBody, Header, Mac, action, auth_alg, capab, is_group, mgmt,
    reason, status,
};
use crate::handshake::{self, Authenticator, AuthenticatorConfig, GroupKeys, HandshakeError};
use crate::ie::{self, Elements};
use crate::rsn::{Akm, Cipher, Rsne, Security, ap_rsne};
use crate::sae::{self, Pt, Sae};

/// How the access point is set up.
#[derive(Clone)]
pub struct ApConfig {
    pub bssid: Mac,
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub security: Security,
    pub passphrase: Option<String>,
    /// Beacons carry an empty SSID.
    pub hidden: bool,
    pub pmf_required: bool,
    /// Advertise and accept SAE hash-to-element.
    pub h2e: bool,
    /// Beacon interval in time units of 1.024 ms.
    pub beacon_interval: u16,
}

impl core::fmt::Debug for ApConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ApConfig({:?}, channel {}, {:?})", String::from_utf8_lossy(&self.ssid), self.channel, self.security)
    }
}

/// What the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApAction {
    /// Transmit an 802.11 frame on the access point's channel.
    Transmit {
        frame: Vec<u8>,
        no_ack: bool,
    },
    /// An Ethernet frame from a station for the wired side.
    Uplink(Vec<u8>),
    Event(ApEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApEvent {
    /// A station completed joining (keys installed).
    Joined(Mac),
    /// A station left or was removed.
    Left(Mac, u16),
    /// A station's authentication or handshake failed.
    AuthFailed(Mac),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaState {
    Authenticated,
    Associated,
    Authorized,
}

struct StaEntry {
    state: StaState,
    sae: Option<Sae>,
    pmk: Option<[u8; 32]>,
    akm: Option<Akm>,
    pmf: bool,
    auth: Option<Authenticator>,
    hs_deadline: u64,
    tk: Option<[u8; 16]>,
    tx_pn: u64,
    rx_pn: [Option<u64>; 17],
    last_seq: Option<u16>,
}

/// Time between handshake retransmissions.
const HANDSHAKE_RETRY_MS: u64 = 1000;

/// A simulated access point.
pub struct AccessPoint {
    pub cfg: ApConfig,
    rsne: Option<Rsne>,
    psk: Option<[u8; 32]>,
    pt: Option<Pt>,
    stations: BTreeMap<Mac, StaEntry>,
    group: GroupKeys,
    gtk_tx_pn: u64,
    igtk_ipn: u64,
    seq: u16,
    next_beacon: u64,
    tsf_start: u64,
}

impl AccessPoint {
    pub fn new(cfg: ApConfig, now: u64, rng: &mut dyn Random) -> AccessPoint {
        let rsne = ap_rsne(cfg.security, cfg.pmf_required);
        let pass = cfg.passphrase.clone().unwrap_or_default();
        let psk = match cfg.security {
            Security::Wpa2Personal | Security::Wpa2Wpa3Personal => {
                Some(psk_from_hex(&pass).unwrap_or_else(|| pbkdf2_psk(pass.as_bytes(), &cfg.ssid)))
            }
            _ => None,
        };
        let pt = (cfg.h2e && matches!(cfg.security, Security::Wpa3Personal | Security::Wpa2Wpa3Personal))
            .then(|| sae::derive_pt(&cfg.ssid, pass.as_bytes(), None));
        let mut gtk = [0u8; 16];
        rng.fill(&mut gtk);
        let mut igtk = [0u8; 16];
        rng.fill(&mut igtk);
        let pmf = rsne.as_ref().is_some_and(|r| r.mfpc());
        AccessPoint {
            cfg,
            rsne,
            psk,
            pt,
            stations: BTreeMap::new(),
            group: GroupKeys { gtk_id: 1, gtk, gtk_pn: 0, igtk: pmf.then_some((4, igtk, 0)) },
            gtk_tx_pn: 0,
            igtk_ipn: 0,
            seq: 0,
            next_beacon: now,
            tsf_start: now,
        }
    }

    fn next_seq(&mut self) -> u16 {
        self.seq = (self.seq + 1) & 0xFFF;
        self.seq
    }

    fn mgmt_to(&mut self, subtype: u8, da: &Mac, body: &[u8]) -> Vec<u8> {
        let (bssid, seq) = (self.cfg.bssid, self.next_seq());
        frame::management(subtype, da, &bssid, &bssid, seq, body)
    }

    fn tx(frame: Vec<u8>, no_ack: bool) -> ApAction {
        ApAction::Transmit { frame, no_ack }
    }

    /// Stations currently associated.
    pub fn stations(&self) -> Vec<Mac> {
        self.stations.keys().copied().collect()
    }

    /// Stations that completed the handshake.
    pub fn authorized(&self) -> Vec<Mac> {
        self.stations.iter().filter(|(_, s)| s.state == StaState::Authorized).map(|(m, _)| *m).collect()
    }

    fn capability(&self) -> u16 {
        let mut c = capab::ESS | capab::SHORT_SLOT_TIME;
        if self.cfg.security != Security::Open {
            c |= capab::PRIVACY;
        }
        c
    }

    fn elements(&self, hide: bool) -> Vec<u8> {
        let ssid: &[u8] = if hide { &[] } else { &self.cfg.ssid };
        let mut b = ie::Builder::new().ssid(ssid).rates(&ie::RATES_G).ds_channel(self.cfg.channel);
        if let Some(r) = &self.rsne {
            b = b.raw(&r.element());
        }
        if self.pt.is_some() {
            b = b.element(ie::id::RSNX, &[ie::rsnx::SAE_H2E]);
        }
        b.build()
    }

    fn tsf(&self, now: u64) -> u64 {
        (now - self.tsf_start) * 1000
    }

    /// A beacon frame.
    pub fn beacon(&mut self, now: u64) -> Vec<u8> {
        let body = BeaconBody::build(
            self.tsf(now),
            self.cfg.beacon_interval,
            self.capability(),
            &self.elements(self.cfg.hidden),
        );
        self.mgmt_to(mgmt::BEACON, &frame::BROADCAST, &body)
    }

    /// When [`AccessPoint::tick`] should run next.
    pub fn next_deadline(&self) -> u64 {
        let hs = self
            .stations
            .values()
            .filter(|s| s.auth.is_some() && s.state == StaState::Associated)
            .map(|s| s.hs_deadline)
            .min();
        hs.map_or(self.next_beacon, |h| h.min(self.next_beacon))
    }

    /// Beacons and handshake retransmissions.
    pub fn tick(&mut self, now: u64) -> Vec<ApAction> {
        let mut out = Vec::new();
        if now >= self.next_beacon {
            let b = self.beacon(now);
            out.push(Self::tx(b, true));
            let interval = self.cfg.beacon_interval as u64 * 1024 / 1000;
            self.next_beacon = now + interval.max(10);
        }
        let due: Vec<Mac> = self
            .stations
            .iter()
            .filter(|(_, s)| s.state == StaState::Associated && s.auth.is_some() && now >= s.hs_deadline)
            .map(|(m, _)| *m)
            .collect();
        for mac in due {
            let retry = self.stations.get_mut(&mac).and_then(|s| s.auth.as_mut()).map(|a| a.retransmit());
            match retry {
                Some(Ok(Some(msg))) => {
                    if let Some(s) = self.stations.get_mut(&mac) {
                        s.hs_deadline = now + HANDSHAKE_RETRY_MS;
                    }
                    out.extend(self.send_eapol(&mac, &msg));
                }
                Some(Err(HandshakeError::Timeout)) => {
                    out.extend(self.remove(&mac, reason::FOURWAY_HANDSHAKE_TIMEOUT, true));
                    out.push(ApAction::Event(ApEvent::AuthFailed(mac)));
                }
                _ => {}
            }
        }
        out
    }

    /// Removes a station, optionally telling it (deauthentication).
    fn remove(&mut self, mac: &Mac, code: u16, notify: bool) -> Vec<ApAction> {
        let mut out = Vec::new();
        if notify && self.stations.contains_key(mac) {
            let f = self.mgmt_to(mgmt::DEAUTH, mac, &code.to_le_bytes());
            if let Some(f) = self.protect_unicast_mgmt(mac, f) {
                out.push(Self::tx(f, false));
            }
        }
        if self.stations.remove(mac).is_some() {
            out.push(ApAction::Event(ApEvent::Left(*mac, code)));
        }
        out
    }

    /// Deauthenticates a station (or all, with `None`).
    pub fn deauthenticate(&mut self, sta: Option<Mac>, code: u16) -> Vec<ApAction> {
        match sta {
            Some(m) => self.remove(&m, code, true),
            None => {
                let mut out = Vec::new();
                for m in self.stations() {
                    out.extend(self.remove(&m, code, true));
                }
                out
            }
        }
    }

    fn protect_unicast_mgmt(&mut self, mac: &Mac, f: Vec<u8>) -> Option<Vec<u8>> {
        match self.stations.get_mut(mac) {
            Some(s) if s.pmf && s.tk.is_some() => {
                s.tx_pn += 1;
                ccmp::ccmp_encrypt(s.tk.as_ref().unwrap(), &f, s.tx_pn, 0)
            }
            _ => Some(f),
        }
    }

    /// Distributes a new GTK (and IGTK) to every authorized station.
    pub fn rekey_group(&mut self, rng: &mut dyn Random, now: u64) -> Vec<ApAction> {
        let mut gtk = [0u8; 16];
        rng.fill(&mut gtk);
        self.group.gtk_id = if self.group.gtk_id == 1 { 2 } else { 1 };
        self.group.gtk = gtk;
        self.gtk_tx_pn = 0;
        self.group.gtk_pn = 0;
        if let Some((id, key, ipn)) = &mut self.group.igtk {
            *id = if *id == 4 { 5 } else { 4 };
            rng.fill(key);
            *ipn = 0;
            self.igtk_ipn = 0;
        }
        let group = self.group.clone();
        let mut out = Vec::new();
        for mac in self.authorized() {
            let msg =
                self.stations.get_mut(&mac).and_then(|s| s.auth.as_mut()).and_then(|a| a.rekey_group(group.clone()));
            if let Some(msg) = msg {
                if let Some(s) = self.stations.get_mut(&mac) {
                    s.hs_deadline = now + HANDSHAKE_RETRY_MS;
                }
                out.extend(self.send_eapol(&mac, &msg));
            }
        }
        out
    }

    /// Handles a frame from the air.
    pub fn receive(&mut self, f: &[u8], now: u64, rng: &mut dyn Random) -> Vec<ApAction> {
        let Some(h) = Header::parse(f) else { return Vec::new() };
        if h.fc.is_management() {
            if h.fc.subtype() == mgmt::PROBE_REQ {
                return self.probe(&h, &f[h.len..]);
            }
            if h.addr1 != self.cfg.bssid && !(is_group(&h.addr1) && h.addr3 == self.cfg.bssid) {
                return Vec::new();
            }
            return match h.fc.subtype() {
                mgmt::AUTH if !h.fc.protected() => self.auth(&h.addr2, &f[h.len..], rng),
                mgmt::ASSOC_REQ | mgmt::REASSOC_REQ if !h.fc.protected() => {
                    self.assoc(&h.addr2, &f[h.len..], h.fc.subtype() == mgmt::REASSOC_REQ, now, rng)
                }
                mgmt::DEAUTH | mgmt::DISASSOC => self.sta_left(&h, f),
                mgmt::ACTION => self.action(&h, f),
                _ => Vec::new(),
            };
        }
        if h.fc.is_data() && h.fc.to_ds() && !h.fc.from_ds() && h.addr1 == self.cfg.bssid {
            return self.data(&h, f, now, rng);
        }
        Vec::new()
    }

    fn probe(&mut self, h: &Header, body: &[u8]) -> Vec<ApAction> {
        let Ok(e) = Elements::parse(body) else { return Vec::new() };
        let wanted = e.ssid.unwrap_or(&[]);
        let matches = if wanted.is_empty() { !self.cfg.hidden } else { wanted == self.cfg.ssid.as_slice() };
        if !matches || (!is_group(&h.addr1) && h.addr1 != self.cfg.bssid) {
            return Vec::new();
        }
        let body = BeaconBody::build(0, self.cfg.beacon_interval, self.capability(), &self.elements(false));
        let f = self.mgmt_to(mgmt::PROBE_RESP, &h.addr2, &body);
        alloc::vec![Self::tx(f, false)]
    }

    fn auth_reply(&mut self, sta: &Mac, alg: u16, trans: u16, st: u16, rest: &[u8]) -> ApAction {
        let body = AuthBody::build(alg, trans, st, rest);
        Self::tx(self.mgmt_to(mgmt::AUTH, sta, &body), false)
    }

    fn new_entry() -> StaEntry {
        StaEntry {
            state: StaState::Authenticated,
            sae: None,
            pmk: None,
            akm: None,
            pmf: false,
            auth: None,
            hs_deadline: 0,
            tk: None,
            tx_pn: 0,
            rx_pn: [None; 17],
            last_seq: None,
        }
    }

    fn sae_allowed(&self) -> bool {
        matches!(self.cfg.security, Security::Wpa3Personal | Security::Wpa2Wpa3Personal)
    }

    fn auth(&mut self, sta: &Mac, body: &[u8], rng: &mut dyn Random) -> Vec<ApAction> {
        let Some(a) = AuthBody::parse(body) else { return Vec::new() };
        match (a.algorithm, a.transaction) {
            (auth_alg::OPEN, 1) => {
                if self.cfg.security == Security::Wpa3Personal {
                    return alloc::vec![self.auth_reply(sta, auth_alg::OPEN, 2, status::NOT_SUPPORTED_AUTH_ALG, &[])];
                }
                // A new authentication replaces any earlier association.
                self.stations.insert(*sta, Self::new_entry());
                alloc::vec![self.auth_reply(sta, auth_alg::OPEN, 2, status::SUCCESS, &[])]
            }
            (auth_alg::SAE, 1) if self.sae_allowed() => {
                let h2e = a.status == status::SAE_HASH_TO_ELEMENT;
                if h2e && self.pt.is_none() {
                    return alloc::vec![self.auth_reply(sta, auth_alg::SAE, 1, status::UNSPECIFIED_FAILURE, &[])];
                }
                let pass = self.cfg.passphrase.clone().unwrap_or_default();
                let pwe = match &self.pt {
                    Some(pt) if h2e => sae::pwe_from_pt(pt, &self.cfg.bssid, sta),
                    _ => sae::pwe_hunting_and_pecking(&self.cfg.bssid, sta, pass.as_bytes(), None),
                };
                let Ok(pwe) = pwe else { return Vec::new() };
                let mut s = Sae::new(pwe, h2e, rng);
                if s.process_commit(a.rest, 0).is_err() {
                    return alloc::vec![self.auth_reply(sta, auth_alg::SAE, 1, status::UNSPECIFIED_FAILURE, &[])];
                }
                let commit = s.commit(None);
                let st = if h2e { status::SAE_HASH_TO_ELEMENT } else { status::SUCCESS };
                let mut entry = Self::new_entry();
                entry.sae = Some(s);
                self.stations.insert(*sta, entry);
                alloc::vec![self.auth_reply(sta, auth_alg::SAE, 1, st, &commit)]
            }
            (auth_alg::SAE, 2) => {
                let verified = {
                    let Some(e) = self.stations.get_mut(sta) else { return Vec::new() };
                    let Some(s) = e.sae.as_mut() else { return Vec::new() };
                    match s.verify_confirm(a.rest) {
                        Ok(()) => {
                            e.pmk = s.keys().map(|(pmk, _)| pmk);
                            s.confirm().ok()
                        }
                        Err(_) => None,
                    }
                };
                match verified {
                    Some(confirm) => alloc::vec![self.auth_reply(sta, auth_alg::SAE, 2, status::SUCCESS, &confirm)],
                    None => {
                        // A confirm that does not verify (different
                        // password) is dropped silently.
                        self.stations.remove(sta);
                        alloc::vec![ApAction::Event(ApEvent::AuthFailed(*sta))]
                    }
                }
            }
            (auth_alg::SAE, _) => {
                alloc::vec![self.auth_reply(sta, auth_alg::SAE, a.transaction, status::NOT_SUPPORTED_AUTH_ALG, &[])]
            }
            (alg, _) => alloc::vec![self.auth_reply(sta, alg, 2, status::NOT_SUPPORTED_AUTH_ALG, &[])],
        }
    }

    fn assoc(&mut self, sta: &Mac, body: &[u8], reassoc: bool, now: u64, rng: &mut dyn Random) -> Vec<ApAction> {
        let Some(req) = AssocReqBody::parse(body, reassoc) else { return Vec::new() };
        let subtype = if reassoc { mgmt::REASSOC_RESP } else { mgmt::ASSOC_RESP };
        let reply = |ap: &mut AccessPoint, st: u16, aid: u16| {
            let body = AssocRespBody::build(ap.capability(), st, aid, &ie::Builder::new().rates(&ie::RATES_G).build());
            let f = ap.mgmt_to(subtype, sta, &body);
            Self::tx(f, false)
        };
        let Some(entry) = self.stations.get(sta) else {
            return alloc::vec![reply(self, status::UNSPECIFIED_FAILURE, 0)];
        };
        let sae_done = entry.pmk.is_some();
        let Ok(e) = Elements::parse(req.ies) else { return alloc::vec![reply(self, status::INVALID_IE, 0)] };
        if e.ssid != Some(self.cfg.ssid.as_slice()) {
            return alloc::vec![reply(self, status::UNSPECIFIED_FAILURE, 0)];
        }
        let aid = (self.stations.keys().position(|m| m == sta).unwrap_or(0) + 1) as u16;
        if self.cfg.security == Security::Open {
            if let Some(s) = self.stations.get_mut(sta) {
                s.state = StaState::Authorized;
            }
            return alloc::vec![reply(self, status::SUCCESS, aid), ApAction::Event(ApEvent::Joined(*sta))];
        }
        // Protected network: the station's RSN element must be acceptable.
        let Some(sta_rsne_body) = e.rsn else { return alloc::vec![reply(self, status::INVALID_IE, 0)] };
        let Ok(sr) = Rsne::parse(sta_rsne_body) else { return alloc::vec![reply(self, status::INVALID_IE, 0)] };
        let ours = self.rsne.clone().expect("RSN element of a protected network");
        if sr.group != Cipher::Ccmp128 || sr.pairwise != alloc::vec![Cipher::Ccmp128] {
            return alloc::vec![reply(self, status::INVALID_PAIRWISE_CIPHER, 0)];
        }
        let akm = match sr.akms.as_slice() {
            [a] if ours.akms.contains(a) => *a,
            _ => return alloc::vec![reply(self, status::INVALID_AKMP, 0)],
        };
        if (akm == Akm::Sae) != sae_done {
            return alloc::vec![reply(self, status::INVALID_AKMP, 0)];
        }
        let pmf = ours.mfpc() && sr.mfpc();
        if (ours.mfpr() && !sr.mfpc()) || (sr.mfpr() && !ours.mfpc()) || (akm == Akm::Sae && !pmf) {
            return alloc::vec![reply(self, status::ROBUST_MGMT_FRAME_POLICY_VIOLATION, 0)];
        }
        let pmk = match akm {
            Akm::Sae => self.stations.get(sta).and_then(|s| s.pmk),
            _ => self.psk,
        };
        let Some(pmk) = pmk else { return alloc::vec![reply(self, status::UNSPECIFIED_FAILURE, 0)] };
        let algo = akm.key_algo().unwrap_or(KeyAlgo::Sha1);
        let mut sta_rsne = alloc::vec![ie::id::RSN, sta_rsne_body.len() as u8];
        sta_rsne.extend_from_slice(sta_rsne_body);
        let ap_rsnxe = (akm == Akm::Sae && self.pt.is_some()).then(|| alloc::vec![ie::id::RSNX, 1, ie::rsnx::SAE_H2E]);
        let mut auth = Authenticator::new(AuthenticatorConfig {
            algo,
            descriptor_version: eapol::descriptor_version(akm),
            pmk,
            aa: self.cfg.bssid,
            spa: *sta,
            ap_rsne: ours.element(),
            ap_rsnxe,
            sta_rsne,
        });
        let mut group = self.group.clone();
        group.gtk_pn = self.gtk_tx_pn + 1;
        if !pmf {
            group.igtk = None;
        }
        let msg1 = auth.start(group, rng);
        if let Some(s) = self.stations.get_mut(sta) {
            s.state = StaState::Associated;
            s.akm = Some(akm);
            s.pmf = pmf;
            s.auth = Some(auth);
            s.hs_deadline = now + HANDSHAKE_RETRY_MS;
            s.tk = None;
            s.tx_pn = 0;
            s.rx_pn = [None; 17];
        }
        let mut out = alloc::vec![reply(self, status::SUCCESS, aid)];
        out.extend(self.send_eapol(sta, &msg1));
        out
    }

    /// Sends an EAPOL frame to a station (protected once it has a TK).
    fn send_eapol(&mut self, sta: &Mac, msg: &[u8]) -> Vec<ApAction> {
        self.send_unicast(sta, &self.cfg.bssid.clone(), eapol::ETHERTYPE, msg, true)
    }

    fn send_unicast(
        &mut self,
        sta: &Mac,
        src: &Mac,
        ethertype: u16,
        payload: &[u8],
        allow_clear: bool,
    ) -> Vec<ApAction> {
        let bssid = self.cfg.bssid;
        let seq = self.next_seq();
        let plain = frame::data_from_ds(sta, &bssid, src, seq, ethertype, payload);
        let Some(s) = self.stations.get_mut(sta) else { return Vec::new() };
        let f = match s.tk {
            Some(tk) => {
                s.tx_pn += 1;
                match ccmp::ccmp_encrypt(&tk, &plain, s.tx_pn, 0) {
                    Some(f) => f,
                    None => return Vec::new(),
                }
            }
            None if allow_clear || self.cfg.security == Security::Open => plain,
            None => return Vec::new(),
        };
        alloc::vec![Self::tx(f, false)]
    }

    fn sta_left(&mut self, h: &Header, f: &[u8]) -> Vec<ApAction> {
        let sta = h.addr2;
        let Some(s) = self.stations.get_mut(&sta) else { return Vec::new() };
        let body = if s.pmf && s.tk.is_some() {
            if !h.fc.protected() {
                return Vec::new();
            }
            let tk = s.tk.unwrap();
            match ccmp::ccmp_decrypt(&tk, f) {
                Ok((plain, _)) => plain[h.len..].to_vec(),
                Err(_) => return Vec::new(),
            }
        } else {
            f[h.len..].to_vec()
        };
        let code = frame::reason_code(&body).unwrap_or(reason::UNSPECIFIED);
        self.remove(&sta, code, false)
    }

    fn action(&mut self, h: &Header, f: &[u8]) -> Vec<ApAction> {
        let sta = h.addr2;
        let Some(s) = self.stations.get(&sta) else { return Vec::new() };
        let plain = if s.pmf && s.tk.is_some() {
            if !h.fc.protected() {
                return Vec::new();
            }
            match ccmp::ccmp_decrypt(s.tk.as_ref().unwrap(), f) {
                Ok((p, _)) => p,
                Err(_) => return Vec::new(),
            }
        } else {
            f.to_vec()
        };
        let body = &plain[h.len..];
        if body.len() >= 4 && body[0] == action::SA_QUERY && body[1] == action::SA_QUERY_REQUEST {
            let mut resp = alloc::vec![action::SA_QUERY, action::SA_QUERY_RESPONSE, body[2], body[3]];
            resp.truncate(4);
            let f = self.mgmt_to(mgmt::ACTION, &sta, &resp);
            return self.protect_unicast_mgmt(&sta, f).map(|f| Self::tx(f, false)).into_iter().collect();
        }
        Vec::new()
    }

    fn data(&mut self, h: &Header, f: &[u8], now: u64, rng: &mut dyn Random) -> Vec<ApAction> {
        let sta = h.addr2;
        let Some(s) = self.stations.get_mut(&sta) else {
            // Data from a station that is not associated.
            let f = self.mgmt_to(mgmt::DEAUTH, &sta, &reason::CLASS3_FRAME_FROM_NONASSOC_STA.to_le_bytes());
            return alloc::vec![Self::tx(f, false)];
        };
        if h.fc.subtype() & 0x4 != 0 {
            // Null data: nothing to deliver.
            return Vec::new();
        }
        if h.fc.retry() && s.last_seq == Some(h.seq) {
            return Vec::new();
        }
        let plain = if h.fc.protected() {
            let Some(tk) = s.tk else { return Vec::new() };
            let Some((pn, _)) = ccmp::ccmp_header(f) else { return Vec::new() };
            let tid = h.tid() as usize & 0xF;
            if s.rx_pn[tid].is_some_and(|l| pn <= l) {
                return Vec::new();
            }
            match ccmp::ccmp_decrypt(&tk, f) {
                Ok((p, pn)) => {
                    s.rx_pn[tid] = Some(pn);
                    p
                }
                Err(_) => return Vec::new(),
            }
        } else {
            f.to_vec()
        };
        s.last_seq = Some(h.seq);
        let Some((ethertype, payload)) = frame::parse_snap(&plain[h.len..]) else { return Vec::new() };
        let authorized = s.state == StaState::Authorized;
        if !h.fc.protected() && self.cfg.security != Security::Open && (s.tk.is_some() || ethertype != eapol::ETHERTYPE)
        {
            return Vec::new();
        }
        if ethertype == eapol::ETHERTYPE {
            return self.eapol(&sta, payload, now, rng);
        }
        if !authorized {
            return Vec::new();
        }
        let dst = h.addr3;
        let mut eth = Vec::with_capacity(14 + payload.len());
        eth.extend_from_slice(&dst);
        eth.extend_from_slice(&sta);
        eth.extend_from_slice(&ethertype.to_be_bytes());
        eth.extend_from_slice(payload);
        let mut out = Vec::new();
        if is_group(&dst) {
            // To the wired side and to the other stations.
            out.extend(self.downlink(&eth));
            out.push(ApAction::Uplink(eth));
        } else if self.stations.get(&dst).is_some_and(|d| d.state == StaState::Authorized) {
            out.extend(self.downlink(&eth));
        } else {
            out.push(ApAction::Uplink(eth));
        }
        out
    }

    fn eapol(&mut self, sta: &Mac, payload: &[u8], now: u64, _rng: &mut dyn Random) -> Vec<ApAction> {
        let Some(s) = self.stations.get_mut(sta) else { return Vec::new() };
        let Some(auth) = s.auth.as_mut() else { return Vec::new() };
        let Ok(events) = auth.receive(payload) else { return Vec::new() };
        let mut out = Vec::new();
        for ev in events {
            match ev {
                handshake::Event::Send(msg) => {
                    if let Some(s) = self.stations.get_mut(sta) {
                        s.hs_deadline = now + HANDSHAKE_RETRY_MS;
                    }
                    out.extend(self.send_eapol(sta, &msg));
                }
                handshake::Event::InstallPtk { tk } => {
                    if let Some(s) = self.stations.get_mut(sta) {
                        s.tk = Some(tk);
                        s.tx_pn = 0;
                    }
                }
                handshake::Event::Completed => {
                    if let Some(s) = self.stations.get_mut(sta) {
                        s.state = StaState::Authorized;
                    }
                    out.push(ApAction::Event(ApEvent::Joined(*sta)));
                }
                _ => {}
            }
        }
        out
    }

    /// Delivers an Ethernet frame from the wired side (or from another
    /// station) to the stations.
    pub fn downlink(&mut self, eth: &[u8]) -> Vec<ApAction> {
        if eth.len() < 14 {
            return Vec::new();
        }
        let dst: Mac = eth[0..6].try_into().unwrap();
        let src: Mac = eth[6..12].try_into().unwrap();
        let ethertype = u16::from_be_bytes([eth[12], eth[13]]);
        let payload = &eth[14..];
        if !is_group(&dst) {
            if self.stations.get(&dst).is_some_and(|s| s.state == StaState::Authorized) {
                return self.send_unicast(&dst, &src, ethertype, payload, false);
            }
            return Vec::new();
        }
        if self.authorized().is_empty() {
            return Vec::new();
        }
        let bssid = self.cfg.bssid;
        let seq = self.next_seq();
        let plain = frame::data_from_ds(&dst, &bssid, &src, seq, ethertype, payload);
        let f = if self.cfg.security == Security::Open {
            plain
        } else {
            self.gtk_tx_pn += 1;
            match ccmp::ccmp_encrypt(&self.group.gtk, &plain, self.gtk_tx_pn, self.group.gtk_id) {
                Some(f) => f,
                None => return Vec::new(),
            }
        };
        alloc::vec![Self::tx(f, true)]
    }

    /// A broadcast deauthentication protected with BIP (for tests of
    /// management frame protection).
    pub fn broadcast_deauth(&mut self, code: u16, protect: bool) -> Vec<ApAction> {
        let f = self.mgmt_to(mgmt::DEAUTH, &frame::BROADCAST, &code.to_le_bytes());
        let f = match (&self.group.igtk, protect) {
            (Some((id, key, _)), true) => {
                self.igtk_ipn += 1;
                match ccmp::bip_protect(key, &f, *id, self.igtk_ipn) {
                    Some(f) => f,
                    None => return Vec::new(),
                }
            }
            _ => f,
        };
        if protect {
            self.stations.clear();
        }
        alloc::vec![Self::tx(f, true)]
    }
}
