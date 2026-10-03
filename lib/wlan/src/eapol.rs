//! EAPOL-Key frames (IEEE 802.11-2020, 12.7.2) and key data
//! encapsulations (KDEs).

use alloc::vec::Vec;

use crate::crypto::{KeyAlgo, ct_eq, eapol_mic};

/// EtherType of EAPOL frames.
pub const ETHERTYPE: u16 = 0x888E;
/// 802.1X packet type of an EAPOL-Key frame.
const TYPE_KEY: u8 = 3;
/// Descriptor type of the IEEE 802.11 (RSN) key descriptor.
const DESC_RSN: u8 = 2;
/// Bytes before the key data (802.1X header included), with a 16-byte MIC.
pub const FIXED_LEN: usize = 99;
const MIC_OFFSET: usize = 81;

/// Key information bits.
pub mod info {
    pub const VERSION_MASK: u16 = 0x0007;
    pub const PAIRWISE: u16 = 1 << 3;
    pub const INSTALL: u16 = 1 << 6;
    pub const ACK: u16 = 1 << 7;
    pub const MIC: u16 = 1 << 8;
    pub const SECURE: u16 = 1 << 9;
    pub const ERROR: u16 = 1 << 10;
    pub const REQUEST: u16 = 1 << 11;
    pub const ENCRYPTED: u16 = 1 << 12;
    pub const SMK: u16 = 1 << 13;
}

/// The key descriptor version an AKM uses.
pub fn descriptor_version(akm: crate::rsn::Akm) -> u16 {
    match akm {
        crate::rsn::Akm::Psk => 2,
        crate::rsn::Akm::PskSha256 => 3,
        // AKM-defined (SAE).
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EapolError {
    Malformed,
    /// Not an RSN EAPOL-Key frame.
    NotKey,
}

/// An EAPOL-Key frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFrame {
    /// 802.1X protocol version.
    pub version: u8,
    pub info: u16,
    pub key_len: u16,
    pub replay: u64,
    pub nonce: [u8; 32],
    pub iv: [u8; 16],
    /// Key RSC (little-endian on the wire).
    pub rsc: u64,
    pub mic: [u8; 16],
    pub data: Vec<u8>,
}

impl KeyFrame {
    pub fn new(info: u16, replay: u64) -> KeyFrame {
        KeyFrame {
            version: 2,
            info,
            key_len: 0,
            replay,
            nonce: [0; 32],
            iv: [0; 16],
            rsc: 0,
            mic: [0; 16],
            data: Vec::new(),
        }
    }

    pub fn has(&self, bit: u16) -> bool {
        self.info & bit != 0
    }

    pub fn descriptor_version(&self) -> u16 {
        self.info & info::VERSION_MASK
    }

    /// Parses an EAPOL frame (the payload after the LLC/SNAP header).
    /// Bytes beyond the 802.1X body length are ignored.
    pub fn parse(eapol: &[u8]) -> Result<KeyFrame, EapolError> {
        if eapol.len() < 4 {
            return Err(EapolError::Malformed);
        }
        if eapol[1] != TYPE_KEY {
            return Err(EapolError::NotKey);
        }
        let body_len = u16::from_be_bytes([eapol[2], eapol[3]]) as usize;
        if body_len + 4 > eapol.len() || body_len + 4 < FIXED_LEN {
            return Err(EapolError::Malformed);
        }
        let f = &eapol[..body_len + 4];
        if f[4] != DESC_RSN {
            return Err(EapolError::NotKey);
        }
        let data_len = u16::from_be_bytes([f[97], f[98]]) as usize;
        if FIXED_LEN + data_len != f.len() {
            return Err(EapolError::Malformed);
        }
        Ok(KeyFrame {
            version: f[0],
            info: u16::from_be_bytes([f[5], f[6]]),
            key_len: u16::from_be_bytes([f[7], f[8]]),
            replay: u64::from_be_bytes(f[9..17].try_into().unwrap()),
            nonce: f[17..49].try_into().unwrap(),
            iv: f[49..65].try_into().unwrap(),
            rsc: u64::from_le_bytes(f[65..73].try_into().unwrap()),
            mic: f[81..97].try_into().unwrap(),
            data: f[99..].to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let body_len = (FIXED_LEN - 4 + self.data.len()) as u16;
        let mut f = Vec::with_capacity(FIXED_LEN + self.data.len());
        f.push(self.version);
        f.push(TYPE_KEY);
        f.extend_from_slice(&body_len.to_be_bytes());
        f.push(DESC_RSN);
        f.extend_from_slice(&self.info.to_be_bytes());
        f.extend_from_slice(&self.key_len.to_be_bytes());
        f.extend_from_slice(&self.replay.to_be_bytes());
        f.extend_from_slice(&self.nonce);
        f.extend_from_slice(&self.iv);
        f.extend_from_slice(&self.rsc.to_le_bytes());
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&self.mic);
        f.extend_from_slice(&(self.data.len() as u16).to_be_bytes());
        f.extend_from_slice(&self.data);
        f
    }

    /// Encodes the frame with its MIC computed under `kck`.
    pub fn signed(&self, algo: KeyAlgo, kck: &[u8; 16]) -> Vec<u8> {
        let mut f = KeyFrame { mic: [0; 16], ..self.clone() }.encode();
        let mic = eapol_mic(algo, kck, &f);
        f[MIC_OFFSET..MIC_OFFSET + 16].copy_from_slice(&mic);
        f
    }
}

/// Verifies the MIC of a received frame over its exact bytes.
pub fn verify_mic(raw: &[u8], algo: KeyAlgo, kck: &[u8; 16]) -> bool {
    if raw.len() < FIXED_LEN {
        return false;
    }
    let body_len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    if body_len + 4 > raw.len() {
        return false;
    }
    let mut copy = raw[..body_len + 4].to_vec();
    let received: [u8; 16] = copy[MIC_OFFSET..MIC_OFFSET + 16].try_into().unwrap();
    copy[MIC_OFFSET..MIC_OFFSET + 16].fill(0);
    ct_eq(&eapol_mic(algo, kck, &copy), &received)
}

const KDE_OUI: [u8; 3] = [0x00, 0x0F, 0xAC];
const KDE_GTK: u8 = 1;
const KDE_PMKID: u8 = 4;
const KDE_IGTK: u8 = 9;

/// A GTK from a GTK KDE.
#[derive(Clone, PartialEq, Eq)]
pub struct Gtk {
    pub key_id: u8,
    pub tx: bool,
    pub key: Vec<u8>,
}

impl core::fmt::Debug for Gtk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Gtk {{ key_id: {}, .. }}", self.key_id)
    }
}

/// An IGTK from an IGTK KDE.
#[derive(Clone, PartialEq, Eq)]
pub struct Igtk {
    pub key_id: u16,
    pub ipn: u64,
    pub key: [u8; 16],
}

impl core::fmt::Debug for Igtk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Igtk {{ key_id: {}, ipn: {}, .. }}", self.key_id, self.ipn)
    }
}

/// The contents of decrypted (or plain) key data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyData {
    /// RSN elements (whole elements); message 3 may carry two.
    pub rsne: Vec<Vec<u8>>,
    pub rsnxe: Option<Vec<u8>>,
    pub gtk: Option<Gtk>,
    pub igtk: Option<Igtk>,
    pub pmkid: Option<[u8; 16]>,
}

impl KeyData {
    /// Parses key data. Stops at padding (0xDD followed by zeros).
    pub fn parse(data: &[u8]) -> Result<KeyData, EapolError> {
        let mut out = KeyData::default();
        let mut p = data;
        while p.len() >= 2 {
            let (id, len) = (p[0], p[1] as usize);
            if id == 0xDD && len == 0 {
                // Padding.
                break;
            }
            if p.len() < 2 + len {
                return Err(EapolError::Malformed);
            }
            let body = &p[2..2 + len];
            match id {
                crate::ie::id::RSN => out.rsne.push(p[..2 + len].to_vec()),
                crate::ie::id::RSNX => out.rsnxe = Some(p[..2 + len].to_vec()),
                0xDD if len >= 4 && body[..3] == KDE_OUI => match body[3] {
                    KDE_GTK if len >= 6 => {
                        out.gtk = Some(Gtk { key_id: body[4] & 3, tx: body[4] & 4 != 0, key: body[6..].to_vec() })
                    }
                    KDE_IGTK if len == 4 + 2 + 6 + 16 => {
                        let key_id = u16::from_le_bytes([body[4], body[5]]);
                        let ipn = u64::from_le_bytes([body[6], body[7], body[8], body[9], body[10], body[11], 0, 0]);
                        out.igtk = Some(Igtk { key_id, ipn, key: body[12..28].try_into().unwrap() });
                    }
                    KDE_PMKID if len == 4 + 16 => out.pmkid = Some(body[4..20].try_into().unwrap()),
                    _ => {}
                },
                _ => {}
            }
            p = &p[2 + len..];
        }
        Ok(out)
    }
}

pub fn gtk_kde(key_id: u8, tx: bool, key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + key.len());
    k.extend_from_slice(&[0xDD, (6 + key.len()) as u8, KDE_OUI[0], KDE_OUI[1], KDE_OUI[2], KDE_GTK]);
    k.push((key_id & 3) | if tx { 4 } else { 0 });
    k.push(0);
    k.extend_from_slice(key);
    k
}

pub fn igtk_kde(key_id: u16, ipn: u64, key: &[u8; 16]) -> Vec<u8> {
    let mut k = Vec::with_capacity(30);
    k.extend_from_slice(&[0xDD, 28, KDE_OUI[0], KDE_OUI[1], KDE_OUI[2], KDE_IGTK]);
    k.extend_from_slice(&key_id.to_le_bytes());
    k.extend_from_slice(&ipn.to_le_bytes()[..6]);
    k.extend_from_slice(key);
    k
}

pub fn pmkid_kde(pmkid: &[u8; 16]) -> Vec<u8> {
    let mut k = Vec::with_capacity(22);
    k.extend_from_slice(&[0xDD, 20, KDE_OUI[0], KDE_OUI[1], KDE_OUI[2], KDE_PMKID]);
    k.extend_from_slice(pmkid);
    k
}

/// Pads key data for AES key wrap: at least 16 bytes and a multiple of 8,
/// with 0xDD followed by zeros.
pub fn pad(data: &mut Vec<u8>) {
    if data.len() < 16 || !data.len().is_multiple_of(8) {
        data.push(0xDD);
        while data.len() < 16 || !data.len().is_multiple_of(8) {
            data.push(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_sign() {
        let mut f = KeyFrame::new(2 | info::PAIRWISE | info::ACK, 7);
        f.key_len = 16;
        f.nonce = [0xAB; 32];
        f.data = pmkid_kde(&[1; 16]);
        let raw = f.encode();
        assert_eq!(raw.len(), FIXED_LEN + 22);
        assert_eq!(KeyFrame::parse(&raw).unwrap(), f);
        let kck = [5u8; 16];
        for algo in [KeyAlgo::Sha1, KeyAlgo::Sha256] {
            let signed = f.signed(algo, &kck);
            assert!(verify_mic(&signed, algo, &kck));
            let mut bad = signed.clone();
            bad[20] ^= 1;
            assert!(!verify_mic(&bad, algo, &kck));
            assert!(!verify_mic(&signed, algo, &[6u8; 16]));
        }
        // Trailing bytes after the 802.1X body are ignored.
        let mut padded = raw.clone();
        padded.extend_from_slice(&[0, 0, 0]);
        assert_eq!(KeyFrame::parse(&padded).unwrap(), f);
    }

    #[test]
    fn malformed_frames() {
        assert_eq!(KeyFrame::parse(&[2, 3, 0]), Err(EapolError::Malformed));
        assert_eq!(KeyFrame::parse(&[2, 0, 0, 0]), Err(EapolError::NotKey));
        let mut raw = KeyFrame::new(0, 0).encode();
        raw[98] = 5; // key data length past the end
        assert_eq!(KeyFrame::parse(&raw), Err(EapolError::Malformed));
        let mut raw = KeyFrame::new(0, 0).encode();
        raw[4] = 254; // WPA1 descriptor
        assert_eq!(KeyFrame::parse(&raw), Err(EapolError::NotKey));
        let mut raw = KeyFrame::new(0, 0).encode();
        raw[3] = 0xFF; // body longer than the frame
        assert_eq!(KeyFrame::parse(&raw), Err(EapolError::Malformed));
    }

    #[test]
    fn key_data() {
        let rsne = alloc::vec![48u8, 2, 1, 0];
        let mut d = rsne.clone();
        d.extend(gtk_kde(2, true, &[9; 16]));
        d.extend(igtk_kde(4, 77, &[8; 16]));
        pad(&mut d);
        assert!(d.len().is_multiple_of(8));
        let k = KeyData::parse(&d).unwrap();
        assert_eq!(k.rsne, alloc::vec![rsne]);
        assert_eq!(k.gtk, Some(Gtk { key_id: 2, tx: true, key: alloc::vec![9; 16] }));
        assert_eq!(k.igtk, Some(Igtk { key_id: 4, ipn: 77, key: [8; 16] }));
        assert_eq!(KeyData::parse(&[0xDD, 40, 0, 0]), Err(EapolError::Malformed));
        let mut short = alloc::vec![1u8, 2, 3];
        pad(&mut short);
        assert_eq!(short.len(), 16);
    }
}
