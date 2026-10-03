//! The 4-way handshake and the group key handshake (IEEE 802.11-2020,
//! 12.7.6 and 12.7.7): the station side ([`Supplicant`]) and the access
//! point side ([`Authenticator`]).
//!
//! The supplicant applies the checks the standard requires before it uses
//! anything from the AP:
//!
//! * message 3 must carry message 1's ANonce, a larger replay counter and
//!   a valid MIC;
//! * the RSN element in message 3 must equal the one the AP advertised in
//!   its beacon or probe response (and likewise the RSN Extension);
//! * the key descriptor version must match the negotiated AKM;
//! * key data must be encrypted and contain the GTK (and the IGTK when
//!   management frames are protected);
//! * a key that is already installed is not installed again, so a
//!   retransmitted message 3 or group message 1 is answered without
//!   resetting packet numbers.

use alloc::vec::Vec;

use zeroize::Zeroize;

use crate::crypto::{KeyAlgo, Ptk, Random, aes_unwrap, aes_wrap, derive_ptk};
use crate::eapol::{self, Gtk, Igtk, KeyData, KeyFrame, info, verify_mic};
use crate::frame::Mac;

/// What a handshake asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Send this EAPOL frame to the peer (as an EAPOL data frame).
    Send(Vec<u8>),
    /// Install the pairwise temporal key (CCMP).
    InstallPtk { tk: [u8; 16] },
    /// Install a group temporal key with its receive sequence counter.
    InstallGtk { key_id: u8, key: Vec<u8>, rsc: u64 },
    /// Install an integrity group temporal key (BIP).
    InstallIgtk { key_id: u16, key: [u8; 16], ipn: u64 },
    /// The 4-way handshake finished: data may flow.
    Completed,
}

/// Why a handshake message was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    Malformed,
    /// Not the message this state expects.
    Unexpected,
    /// The replay counter did not increase.
    Replay,
    /// The MIC did not verify (with a passphrase: usually a wrong one).
    BadMic,
    /// The key descriptor version does not match the AKM.
    BadVersion,
    NonceMismatch,
    /// The RSN element differs from the advertised one.
    RsnMismatch,
    /// Key data could not be decrypted, or a required key is missing.
    BadKeyData,
    /// Too many retransmissions without an answer.
    Timeout,
}

/// What the supplicant needs to know.
#[derive(Clone)]
pub struct SupplicantConfig {
    pub algo: KeyAlgo,
    /// Key descriptor version of the AKM (2 PSK, 3 PSK-SHA256, 0 SAE).
    pub descriptor_version: u16,
    pub pmk: [u8; 32],
    /// The AP's address (authenticator).
    pub aa: Mac,
    /// Our address (supplicant).
    pub spa: Mac,
    /// Our RSN element (whole), as sent in the association request.
    pub own_rsne: Vec<u8>,
    pub own_rsnxe: Option<Vec<u8>>,
    /// The AP's RSN element (whole) from its beacon or probe response.
    pub ap_rsne: Vec<u8>,
    pub ap_rsnxe: Option<Vec<u8>>,
    /// Management frame protection negotiated (an IGTK is required).
    pub pmf: bool,
}

impl Drop for SupplicantConfig {
    fn drop(&mut self) {
        self.pmk.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SState {
    /// Waiting for message 1.
    Idle,
    /// Message 2 sent.
    Msg2Sent,
    /// Keys installed.
    Done,
}

/// The station side of the 4-way and group key handshakes.
pub struct Supplicant {
    cfg: SupplicantConfig,
    state: SState,
    /// Replay counter of the last message whose MIC verified.
    last_replay: Option<u64>,
    anonce: Option<[u8; 32]>,
    snonce: [u8; 32],
    /// The PTK derived from the current nonces, not yet confirmed.
    tptk: Option<Ptk>,
    /// The confirmed PTK.
    ptk: Option<Ptk>,
    installed_tk: Option<[u8; 16]>,
    installed_gtk: Option<(u8, Vec<u8>)>,
    installed_igtk: Option<(u16, [u8; 16])>,
    /// MIC failures seen (with a passphrase: probably a wrong one).
    pub mic_failures: u32,
}

impl Supplicant {
    pub fn new(cfg: SupplicantConfig) -> Supplicant {
        Supplicant {
            cfg,
            state: SState::Idle,
            last_replay: None,
            anonce: None,
            snonce: [0; 32],
            tptk: None,
            ptk: None,
            installed_tk: None,
            installed_gtk: None,
            installed_igtk: None,
            mic_failures: 0,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.state == SState::Done
    }

    /// Whether message 2 went out and message 3 is awaited.
    pub fn awaiting_msg3(&self) -> bool {
        self.state == SState::Msg2Sent
    }

    /// The confirmed PTK.
    pub fn ptk(&self) -> Option<&Ptk> {
        self.ptk.as_ref()
    }

    /// Handles an EAPOL frame from the AP.
    pub fn receive(&mut self, raw: &[u8], rng: &mut dyn Random) -> Result<Vec<Event>, HandshakeError> {
        let f = KeyFrame::parse(raw).map_err(|_| HandshakeError::Malformed)?;
        if f.has(info::REQUEST) || f.has(info::ERROR) || f.has(info::SMK) || !f.has(info::ACK) {
            return Err(HandshakeError::Unexpected);
        }
        if f.descriptor_version() != self.cfg.descriptor_version {
            return Err(HandshakeError::BadVersion);
        }
        if self.last_replay.is_some_and(|r| f.replay <= r) {
            return Err(HandshakeError::Replay);
        }
        match (f.has(info::PAIRWISE), f.has(info::MIC), f.has(info::INSTALL)) {
            (true, false, false) => self.msg1(&f, rng),
            (true, true, true) => self.msg3(&f, raw),
            (false, true, false) if f.has(info::SECURE) => self.group1(&f, raw),
            _ => Err(HandshakeError::Unexpected),
        }
    }

    fn msg1(&mut self, f: &KeyFrame, rng: &mut dyn Random) -> Result<Vec<Event>, HandshakeError> {
        // A retransmitted message 1 (same ANonce) keeps our SNonce, so a
        // message 3 for either copy verifies.
        if self.anonce != Some(f.nonce) {
            rng.fill(&mut self.snonce);
            self.anonce = Some(f.nonce);
            self.tptk =
                Some(derive_ptk(self.cfg.algo, &self.cfg.pmk, &self.cfg.aa, &self.cfg.spa, &f.nonce, &self.snonce));
        }
        let tptk = self.tptk.as_ref().ok_or(HandshakeError::Unexpected)?;
        let mut reply = KeyFrame::new(self.cfg.descriptor_version | info::PAIRWISE | info::MIC, f.replay);
        reply.version = f.version.clamp(1, 2);
        reply.nonce = self.snonce;
        reply.data = self.cfg.own_rsne.clone();
        if let Some(x) = &self.cfg.own_rsnxe {
            reply.data.extend_from_slice(x);
        }
        if self.state != SState::Done {
            self.state = SState::Msg2Sent;
        }
        Ok(alloc::vec![Event::Send(reply.signed(self.cfg.algo, &tptk.kck))])
    }

    fn decrypt_key_data(f: &KeyFrame, kek: &[u8; 16]) -> Result<KeyData, HandshakeError> {
        if !f.has(info::ENCRYPTED) {
            return Err(HandshakeError::BadKeyData);
        }
        let plain = aes_unwrap(kek, &f.data).ok_or(HandshakeError::BadKeyData)?;
        KeyData::parse(&plain).map_err(|_| HandshakeError::BadKeyData)
    }

    fn msg3(&mut self, f: &KeyFrame, raw: &[u8]) -> Result<Vec<Event>, HandshakeError> {
        let Some(anonce) = self.anonce else { return Err(HandshakeError::Unexpected) };
        if f.nonce != anonce {
            return Err(HandshakeError::NonceMismatch);
        }
        let tptk = self.tptk.as_ref().ok_or(HandshakeError::Unexpected)?;
        if !verify_mic(raw, self.cfg.algo, &tptk.kck) {
            self.mic_failures += 1;
            return Err(HandshakeError::BadMic);
        }
        let kd = Self::decrypt_key_data(f, &tptk.kek)?;
        if kd.rsne.first() != Some(&self.cfg.ap_rsne) {
            return Err(HandshakeError::RsnMismatch);
        }
        if let Some(adv) = &self.cfg.ap_rsnxe
            && kd.rsnxe.as_ref() != Some(adv)
        {
            return Err(HandshakeError::RsnMismatch);
        }
        let gtk = kd.gtk.ok_or(HandshakeError::BadKeyData)?;
        if gtk.key.len() != 16 || (self.cfg.pmf && kd.igtk.is_none()) {
            return Err(HandshakeError::BadKeyData);
        }
        // Accepted: the PTK is confirmed.
        self.last_replay = Some(f.replay);
        let ptk = self.tptk.clone().ok_or(HandshakeError::Unexpected)?;
        let mut reply =
            KeyFrame::new(self.cfg.descriptor_version | info::PAIRWISE | info::MIC | info::SECURE, f.replay);
        reply.version = f.version.clamp(1, 2);
        let mut out = alloc::vec![Event::Send(reply.signed(self.cfg.algo, &ptk.kck))];
        if self.installed_tk != Some(ptk.tk) {
            self.installed_tk = Some(ptk.tk);
            out.push(Event::InstallPtk { tk: ptk.tk });
        }
        out.extend(self.install_group(gtk, kd.igtk, f.rsc));
        self.ptk = Some(ptk);
        if self.state != SState::Done {
            self.state = SState::Done;
            out.push(Event::Completed);
        }
        Ok(out)
    }

    fn install_group(&mut self, gtk: Gtk, igtk: Option<Igtk>, rsc: u64) -> Vec<Event> {
        let mut out = Vec::new();
        if self.installed_gtk.as_ref().map(|(id, k)| (*id, k.as_slice())) != Some((gtk.key_id, gtk.key.as_slice())) {
            self.installed_gtk = Some((gtk.key_id, gtk.key.clone()));
            out.push(Event::InstallGtk { key_id: gtk.key_id, key: gtk.key, rsc });
        }
        if let Some(i) = igtk
            && self.installed_igtk != Some((i.key_id, i.key))
        {
            self.installed_igtk = Some((i.key_id, i.key));
            out.push(Event::InstallIgtk { key_id: i.key_id, key: i.key, ipn: i.ipn });
        }
        out
    }

    fn group1(&mut self, f: &KeyFrame, raw: &[u8]) -> Result<Vec<Event>, HandshakeError> {
        if self.state != SState::Done {
            return Err(HandshakeError::Unexpected);
        }
        let ptk = self.ptk.clone().ok_or(HandshakeError::Unexpected)?;
        if !verify_mic(raw, self.cfg.algo, &ptk.kck) {
            self.mic_failures += 1;
            return Err(HandshakeError::BadMic);
        }
        let kd = Self::decrypt_key_data(f, &ptk.kek)?;
        let gtk = kd.gtk.ok_or(HandshakeError::BadKeyData)?;
        if gtk.key.len() != 16 || (self.cfg.pmf && kd.igtk.is_none()) {
            return Err(HandshakeError::BadKeyData);
        }
        self.last_replay = Some(f.replay);
        let mut reply = KeyFrame::new(self.cfg.descriptor_version | info::MIC | info::SECURE, f.replay);
        reply.version = f.version.clamp(1, 2);
        let mut out = alloc::vec![Event::Send(reply.signed(self.cfg.algo, &ptk.kck))];
        out.extend(self.install_group(gtk, kd.igtk, f.rsc));
        Ok(out)
    }
}

/// The group keys an access point hands out.
#[derive(Clone)]
pub struct GroupKeys {
    pub gtk_id: u8,
    pub gtk: [u8; 16],
    /// The GTK's current transmit packet number (stations start from it).
    pub gtk_pn: u64,
    pub igtk: Option<(u16, [u8; 16], u64)>,
}

impl Drop for GroupKeys {
    fn drop(&mut self) {
        self.gtk.zeroize();
        if let Some((_, k, _)) = &mut self.igtk {
            k.zeroize();
        }
    }
}

/// What the authenticator needs to know.
#[derive(Clone)]
pub struct AuthenticatorConfig {
    pub algo: KeyAlgo,
    pub descriptor_version: u16,
    pub pmk: [u8; 32],
    pub aa: Mac,
    pub spa: Mac,
    /// The AP's RSN element (whole), as in its beacon.
    pub ap_rsne: Vec<u8>,
    pub ap_rsnxe: Option<Vec<u8>>,
    /// The station's RSN element (whole) from its association request.
    pub sta_rsne: Vec<u8>,
}

impl Drop for AuthenticatorConfig {
    fn drop(&mut self) {
        self.pmk.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AState {
    Idle,
    Msg1Sent,
    Msg3Sent,
    Done,
    GroupSent,
}

/// Most transmissions of one handshake message.
pub const MAX_ATTEMPTS: u32 = 4;

/// The access point side of the handshakes.
pub struct Authenticator {
    cfg: AuthenticatorConfig,
    state: AState,
    replay: u64,
    anonce: [u8; 32],
    ptk: Option<Ptk>,
    attempts: u32,
    group: Option<GroupKeys>,
}

impl Authenticator {
    pub fn new(cfg: AuthenticatorConfig) -> Authenticator {
        Authenticator { cfg, state: AState::Idle, replay: 0, anonce: [0; 32], ptk: None, attempts: 0, group: None }
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.state, AState::Done | AState::GroupSent)
    }

    /// The temporal key once the handshake completed.
    pub fn tk(&self) -> Option<[u8; 16]> {
        if self.is_complete() { self.ptk.as_ref().map(|p| p.tk) } else { None }
    }

    fn next_replay(&mut self) -> u64 {
        self.replay += 1;
        self.replay
    }

    /// Starts the 4-way handshake: returns message 1.
    pub fn start(&mut self, group: GroupKeys, rng: &mut dyn Random) -> Vec<u8> {
        rng.fill(&mut self.anonce);
        self.group = Some(group);
        self.state = AState::Msg1Sent;
        self.attempts = 1;
        self.ptk = None;
        self.msg1()
    }

    fn msg1(&mut self) -> Vec<u8> {
        let mut f = KeyFrame::new(self.cfg.descriptor_version | info::PAIRWISE | info::ACK, self.next_replay());
        f.key_len = 16;
        f.nonce = self.anonce;
        f.encode()
    }

    fn group_key_data(&self) -> Vec<u8> {
        let mut kd = Vec::new();
        if let Some(g) = &self.group {
            kd.extend(eapol::gtk_kde(g.gtk_id, true, &g.gtk));
            if let Some((id, key, ipn)) = g.igtk {
                kd.extend(eapol::igtk_kde(id, ipn, &key));
            }
        }
        kd
    }

    fn msg3(&mut self) -> Option<Vec<u8>> {
        let ptk = self.ptk.clone()?;
        let gtk_pn = self.group.as_ref()?.gtk_pn;
        let mut kd = self.cfg.ap_rsne.clone();
        if let Some(x) = &self.cfg.ap_rsnxe {
            kd.extend_from_slice(x);
        }
        kd.extend(self.group_key_data());
        eapol::pad(&mut kd);
        let bits = self.cfg.descriptor_version
            | info::PAIRWISE
            | info::INSTALL
            | info::ACK
            | info::MIC
            | info::SECURE
            | info::ENCRYPTED;
        let mut f = KeyFrame::new(bits, self.next_replay());
        f.key_len = 16;
        f.nonce = self.anonce;
        f.rsc = gtk_pn;
        f.data = aes_wrap(&ptk.kek, &kd)?;
        kd.zeroize();
        Some(f.signed(self.cfg.algo, &ptk.kck))
    }

    fn group_msg1(&mut self) -> Option<Vec<u8>> {
        let ptk = self.ptk.clone()?;
        let gtk_pn = self.group.as_ref()?.gtk_pn;
        let mut kd = self.group_key_data();
        eapol::pad(&mut kd);
        let bits = self.cfg.descriptor_version | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED;
        let mut f = KeyFrame::new(bits, self.next_replay());
        f.rsc = gtk_pn;
        f.data = aes_wrap(&ptk.kek, &kd)?;
        kd.zeroize();
        Some(f.signed(self.cfg.algo, &ptk.kck))
    }

    /// Handles an EAPOL frame from the station.
    pub fn receive(&mut self, raw: &[u8]) -> Result<Vec<Event>, HandshakeError> {
        let f = KeyFrame::parse(raw).map_err(|_| HandshakeError::Malformed)?;
        if f.has(info::ACK) || !f.has(info::MIC) {
            return Err(HandshakeError::Unexpected);
        }
        if f.descriptor_version() != self.cfg.descriptor_version {
            return Err(HandshakeError::BadVersion);
        }
        if f.replay != self.replay {
            return Err(HandshakeError::Replay);
        }
        match self.state {
            AState::Msg1Sent if f.has(info::PAIRWISE) && !f.has(info::SECURE) => {
                let ptk = derive_ptk(self.cfg.algo, &self.cfg.pmk, &self.cfg.aa, &self.cfg.spa, &self.anonce, &f.nonce);
                if !verify_mic(raw, self.cfg.algo, &ptk.kck) {
                    // The passphrases differ; message 1 will be retried.
                    return Err(HandshakeError::BadMic);
                }
                let kd = KeyData::parse(&f.data).map_err(|_| HandshakeError::Malformed)?;
                if kd.rsne.first() != Some(&self.cfg.sta_rsne) {
                    return Err(HandshakeError::RsnMismatch);
                }
                self.ptk = Some(ptk);
                self.state = AState::Msg3Sent;
                self.attempts = 1;
                let m3 = self.msg3().ok_or(HandshakeError::BadKeyData)?;
                Ok(alloc::vec![Event::Send(m3)])
            }
            AState::Msg3Sent if f.has(info::PAIRWISE) && f.has(info::SECURE) => {
                let ptk = self.ptk.as_ref().ok_or(HandshakeError::Unexpected)?;
                if !verify_mic(raw, self.cfg.algo, &ptk.kck) {
                    return Err(HandshakeError::BadMic);
                }
                self.state = AState::Done;
                Ok(alloc::vec![Event::InstallPtk { tk: ptk.tk }, Event::Completed])
            }
            AState::GroupSent if !f.has(info::PAIRWISE) => {
                let ptk = self.ptk.as_ref().ok_or(HandshakeError::Unexpected)?;
                if !verify_mic(raw, self.cfg.algo, &ptk.kck) {
                    return Err(HandshakeError::BadMic);
                }
                self.state = AState::Done;
                Ok(Vec::new())
            }
            _ => Err(HandshakeError::Unexpected),
        }
    }

    /// The handshake timer expired: the message to send again (with a new
    /// replay counter), `Ok(None)` if nothing is pending, or
    /// `Err(Timeout)` once the attempts are used up.
    pub fn retransmit(&mut self) -> Result<Option<Vec<u8>>, HandshakeError> {
        if !matches!(self.state, AState::Msg1Sent | AState::Msg3Sent | AState::GroupSent) {
            return Ok(None);
        }
        if self.attempts >= MAX_ATTEMPTS {
            return Err(HandshakeError::Timeout);
        }
        self.attempts += 1;
        let msg = match self.state {
            AState::Msg1Sent => Some(self.msg1()),
            AState::Msg3Sent => self.msg3(),
            _ => self.group_msg1(),
        };
        Ok(msg)
    }

    /// Distributes new group keys (the group key handshake). Returns group
    /// message 1, or `None` before the 4-way handshake completed.
    pub fn rekey_group(&mut self, group: GroupKeys) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        self.group = Some(group);
        self.state = AState::GroupSent;
        self.attempts = 1;
        self.group_msg1()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rsn::{Security, ap_rsne, select};

    /// A deterministic stand-in for the system's random generator.
    pub struct TestRandom(pub u8);

    impl Random for TestRandom {
        fn fill(&mut self, buf: &mut [u8]) {
            for b in buf.iter_mut() {
                self.0 = self.0.wrapping_mul(29).wrapping_add(7);
                *b = self.0;
            }
        }
    }

    const AA: Mac = [0x02, 0, 0, 0, 0, 0xAA];
    const SPA: Mac = [0x02, 0, 0, 0, 0, 0x5A];

    fn group(id: u8, fill: u8) -> GroupKeys {
        GroupKeys { gtk_id: id, gtk: [fill; 16], gtk_pn: 5, igtk: Some((4, [fill ^ 0xFF; 16], 9)) }
    }

    /// A supplicant and an authenticator for a network of `sec`, with the
    /// given PMKs on each side.
    fn pair(sec: Security, sta_pmk: [u8; 32], ap_pmk: [u8; 32]) -> (Supplicant, Authenticator) {
        let ap = ap_rsne(sec, false).unwrap();
        let sel = select(&ap, true).unwrap();
        let algo = sel.akm.key_algo().unwrap();
        let version = eapol::descriptor_version(sel.akm);
        let sta = Supplicant::new(SupplicantConfig {
            algo,
            descriptor_version: version,
            pmk: sta_pmk,
            aa: AA,
            spa: SPA,
            own_rsne: sel.own_rsne.clone(),
            own_rsnxe: None,
            ap_rsne: ap.element(),
            ap_rsnxe: None,
            pmf: true,
        });
        let auth = Authenticator::new(AuthenticatorConfig {
            algo,
            descriptor_version: version,
            pmk: ap_pmk,
            aa: AA,
            spa: SPA,
            ap_rsne: ap.element(),
            ap_rsnxe: None,
            sta_rsne: sel.own_rsne,
        });
        (sta, auth)
    }

    fn sent(events: &[Event]) -> Vec<u8> {
        events
            .iter()
            .find_map(|e| match e {
                Event::Send(f) => Some(f.clone()),
                _ => None,
            })
            .expect("a frame to send")
    }

    /// Runs the 4-way handshake; returns the supplicant's events of
    /// message 3.
    fn run(sta: &mut Supplicant, auth: &mut Authenticator, rng: &mut TestRandom) -> Vec<Event> {
        let m1 = auth.start(group(1, 0x61), rng);
        let m2 = sent(&sta.receive(&m1, rng).unwrap());
        let m3 = sent(&auth.receive(&m2).unwrap());
        let ev = sta.receive(&m3, rng).unwrap();
        let m4 = sent(&ev);
        let done = auth.receive(&m4).unwrap();
        assert!(done.contains(&Event::Completed));
        ev
    }

    #[test]
    fn handshake_completes_for_each_akm() {
        for sec in [Security::Wpa2Personal, Security::Wpa3Personal] {
            let mut rng = TestRandom(1);
            let (mut sta, mut auth) = pair(sec, [7; 32], [7; 32]);
            let ev = run(&mut sta, &mut auth, &mut rng);
            assert!(sta.is_complete() && auth.is_complete());
            let tk = sta.ptk().unwrap().tk;
            assert_eq!(auth.tk(), Some(tk));
            assert!(ev.contains(&Event::InstallPtk { tk }));
            assert!(ev.contains(&Event::InstallGtk { key_id: 1, key: alloc::vec![0x61; 16], rsc: 5 }));
            assert!(ev.iter().any(|e| matches!(e, Event::InstallIgtk { key_id: 4, ipn: 9, .. })));
            assert!(ev.contains(&Event::Completed));
        }
    }

    #[test]
    fn different_pmks_do_not_complete() {
        let mut rng = TestRandom(2);
        let (mut sta, mut auth) = pair(Security::Wpa2Personal, [1; 32], [2; 32]);
        let m1 = auth.start(group(1, 0x61), &mut rng);
        let m2 = sent(&sta.receive(&m1, &mut rng).unwrap());
        assert_eq!(auth.receive(&m2), Err(HandshakeError::BadMic));
        // The authenticator retries message 1 a few times, then gives up.
        let mut retries = 0;
        loop {
            match auth.retransmit() {
                Ok(Some(m)) => {
                    retries += 1;
                    let m2 = sent(&sta.receive(&m, &mut rng).unwrap());
                    assert_eq!(auth.receive(&m2), Err(HandshakeError::BadMic));
                }
                Ok(None) => panic!("nothing to retransmit"),
                Err(e) => {
                    assert_eq!(e, HandshakeError::Timeout);
                    break;
                }
            }
        }
        assert_eq!(retries, MAX_ATTEMPTS - 1);
        assert!(!sta.is_complete() && !auth.is_complete());
    }

    #[test]
    fn retransmitted_messages_are_handled() {
        let mut rng = TestRandom(3);
        let (mut sta, mut auth) = pair(Security::Wpa2Personal, [5; 32], [5; 32]);
        // Message 1 is sent twice; the supplicant answers both with the
        // same SNonce, and the second message 2 is the one used.
        let _first = auth.start(group(1, 0x61), &mut rng);
        let m1b = auth.retransmit().unwrap().unwrap();
        let m2 = sent(&sta.receive(&m1b, &mut rng).unwrap());
        let m3 = sent(&auth.receive(&m2).unwrap());
        let ev = sta.receive(&m3, &mut rng).unwrap();
        assert!(ev.contains(&Event::Completed));
        // The AP did not get message 4 and sends message 3 again: the
        // supplicant answers, but installs nothing a second time.
        let m3b = auth.retransmit().unwrap().unwrap();
        let ev = sta.receive(&m3b, &mut rng).unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], Event::Send(_)));
        // An exact copy of an accepted message is refused.
        assert_eq!(sta.receive(&m3b, &mut rng), Err(HandshakeError::Replay));
    }

    #[test]
    fn group_rekey_installs_new_keys_once() {
        let mut rng = TestRandom(4);
        let (mut sta, mut auth) = pair(Security::Wpa2Personal, [6; 32], [6; 32]);
        run(&mut sta, &mut auth, &mut rng);
        let g1 = auth.rekey_group(group(2, 0x62)).unwrap();
        let ev = sta.receive(&g1, &mut rng).unwrap();
        assert!(ev.contains(&Event::InstallGtk { key_id: 2, key: alloc::vec![0x62; 16], rsc: 5 }));
        auth.receive(&sent(&ev)).unwrap();
        // The same keys sent again with a fresh counter: answered, not
        // reinstalled.
        let again = auth.rekey_group(group(2, 0x62)).unwrap();
        let ev = sta.receive(&again, &mut rng).unwrap();
        assert!(ev.iter().all(|e| matches!(e, Event::Send(_))));
    }

    #[test]
    fn message_3_must_match_the_advertised_rsn_element() {
        let mut rng = TestRandom(5);
        let (mut sta, _) = pair(Security::Wpa2Personal, [8; 32], [8; 32]);
        // The authenticator puts a different RSN element into message 3
        // than the one the supplicant saw in the beacon.
        let other = ap_rsne(Security::Wpa2Wpa3Personal, false).unwrap();
        let sel = select(&ap_rsne(Security::Wpa2Personal, false).unwrap(), true).unwrap();
        let mut auth = Authenticator::new(AuthenticatorConfig {
            algo: KeyAlgo::Sha1,
            descriptor_version: 2,
            pmk: [8; 32],
            aa: AA,
            spa: SPA,
            ap_rsne: other.element(),
            ap_rsnxe: None,
            sta_rsne: sel.own_rsne,
        });
        let m1 = auth.start(group(1, 0x61), &mut rng);
        let m2 = sent(&sta.receive(&m1, &mut rng).unwrap());
        let m3 = sent(&auth.receive(&m2).unwrap());
        assert_eq!(sta.receive(&m3, &mut rng), Err(HandshakeError::RsnMismatch));
        assert!(!sta.is_complete());
    }

    #[test]
    fn wrong_descriptor_version_and_garbage_are_refused() {
        let mut rng = TestRandom(6);
        let (mut sta, mut auth) = pair(Security::Wpa3Personal, [9; 32], [9; 32]);
        let m1 = auth.start(group(1, 0x61), &mut rng);
        let mut f = KeyFrame::parse(&m1).unwrap();
        f.info = (f.info & !info::VERSION_MASK) | 2;
        assert_eq!(sta.receive(&f.encode(), &mut rng), Err(HandshakeError::BadVersion));
        assert_eq!(sta.receive(&[1, 2, 3], &mut rng), Err(HandshakeError::Malformed));
        let mut seed = 11u32;
        for n in 0..3000usize {
            let mut junk = m1.clone();
            for _ in 0..(n % 5) + 1 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let pos = seed as usize % junk.len();
                junk[pos] = (seed >> 8) as u8;
            }
            let _ = sta.receive(&junk, &mut rng);
        }
    }
}
