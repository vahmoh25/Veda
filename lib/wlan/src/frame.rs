//! IEEE 802.11 MAC frames: the frame control field, MAC headers,
//! management frame bodies, and data frames with LLC/SNAP encapsulation.
//!
//! Parsers take untrusted bytes and return `None` on anything malformed;
//! builders produce frames without the FCS (radios add it).

use alloc::vec::Vec;

/// A MAC address.
pub type Mac = [u8; 6];

pub const BROADCAST: Mac = [0xFF; 6];

/// Whether an address is group-addressed (multicast or broadcast).
pub fn is_group(mac: &Mac) -> bool {
    mac[0] & 1 != 0
}

/// Frame types.
pub mod ftype {
    pub const MANAGEMENT: u8 = 0;
    pub const CONTROL: u8 = 1;
    pub const DATA: u8 = 2;
}

/// Management frame subtypes.
pub mod mgmt {
    pub const ASSOC_REQ: u8 = 0;
    pub const ASSOC_RESP: u8 = 1;
    pub const REASSOC_REQ: u8 = 2;
    pub const REASSOC_RESP: u8 = 3;
    pub const PROBE_REQ: u8 = 4;
    pub const PROBE_RESP: u8 = 5;
    pub const BEACON: u8 = 8;
    pub const DISASSOC: u8 = 10;
    pub const AUTH: u8 = 11;
    pub const DEAUTH: u8 = 12;
    pub const ACTION: u8 = 13;
}

/// Data frame subtypes.
pub mod data {
    pub const DATA: u8 = 0;
    pub const NULL: u8 = 4;
    pub const QOS_DATA: u8 = 8;
    pub const QOS_NULL: u8 = 12;
}

/// Status codes (IEEE 802.11-2020, Table 9-50) used here.
pub mod status {
    pub const SUCCESS: u16 = 0;
    pub const UNSPECIFIED_FAILURE: u16 = 1;
    pub const CAPS_UNSUPPORTED: u16 = 10;
    pub const NOT_SUPPORTED_AUTH_ALG: u16 = 13;
    pub const UNKNOWN_AUTH_TRANSACTION: u16 = 14;
    pub const CHALLENGE_FAIL: u16 = 15;
    pub const AP_UNABLE_TO_HANDLE_NEW_STA: u16 = 17;
    pub const ASSOC_REJECTED_TEMPORARILY: u16 = 30;
    pub const ROBUST_MGMT_FRAME_POLICY_VIOLATION: u16 = 31;
    pub const INVALID_IE: u16 = 40;
    pub const INVALID_GROUP_CIPHER: u16 = 41;
    pub const INVALID_PAIRWISE_CIPHER: u16 = 42;
    pub const INVALID_AKMP: u16 = 43;
    pub const UNSUPPORTED_RSN_IE_VERSION: u16 = 44;
    pub const INVALID_RSN_IE_CAPAB: u16 = 45;
    pub const ANTI_CLOGGING_TOKEN_REQ: u16 = 76;
    pub const FINITE_CYCLIC_GROUP_NOT_SUPPORTED: u16 = 77;
    pub const UNKNOWN_PASSWORD_IDENTIFIER: u16 = 123;
    pub const SAE_HASH_TO_ELEMENT: u16 = 126;
}

/// Reason codes (IEEE 802.11-2020, Table 9-49) used here.
pub mod reason {
    pub const UNSPECIFIED: u16 = 1;
    pub const PREV_AUTH_NOT_VALID: u16 = 2;
    pub const DEAUTH_LEAVING: u16 = 3;
    pub const DISASSOC_INACTIVITY: u16 = 4;
    pub const DISASSOC_AP_BUSY: u16 = 5;
    pub const CLASS2_FRAME_FROM_NONAUTH_STA: u16 = 6;
    pub const CLASS3_FRAME_FROM_NONASSOC_STA: u16 = 7;
    pub const DISASSOC_STA_HAS_LEFT: u16 = 8;
    pub const INVALID_IE: u16 = 13;
    pub const MICHAEL_MIC_FAILURE: u16 = 14;
    pub const FOURWAY_HANDSHAKE_TIMEOUT: u16 = 15;
    pub const GROUP_KEY_UPDATE_TIMEOUT: u16 = 16;
    pub const IE_IN_4WAY_DIFFERS: u16 = 17;
    pub const INVALID_RSN_IE_CAPAB: u16 = 22;
    pub const IEEE_802_1X_AUTH_FAILED: u16 = 23;
}

/// Authentication algorithm numbers.
pub mod auth_alg {
    pub const OPEN: u16 = 0;
    pub const SAE: u16 = 3;
}

/// Action frame categories.
pub mod action {
    pub const SA_QUERY: u8 = 8;
    pub const SA_QUERY_REQUEST: u8 = 0;
    pub const SA_QUERY_RESPONSE: u8 = 1;
}

/// The frame control field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameControl(pub u16);

impl FrameControl {
    pub fn new(ftype: u8, subtype: u8) -> FrameControl {
        FrameControl(((ftype as u16 & 3) << 2) | ((subtype as u16 & 0xF) << 4))
    }

    pub fn from_bytes(b: [u8; 2]) -> FrameControl {
        FrameControl(u16::from_le_bytes(b))
    }

    pub fn to_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    pub fn protocol_version(self) -> u8 {
        (self.0 & 3) as u8
    }

    pub fn ftype(self) -> u8 {
        ((self.0 >> 2) & 3) as u8
    }

    pub fn subtype(self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }

    fn bit(self, n: u16) -> bool {
        self.0 & (1 << n) != 0
    }

    pub fn to_ds(self) -> bool {
        self.bit(8)
    }

    pub fn from_ds(self) -> bool {
        self.bit(9)
    }

    pub fn more_frag(self) -> bool {
        self.bit(10)
    }

    pub fn retry(self) -> bool {
        self.bit(11)
    }

    pub fn power_mgmt(self) -> bool {
        self.bit(12)
    }

    pub fn protected(self) -> bool {
        self.bit(14)
    }

    pub fn order(self) -> bool {
        self.bit(15)
    }

    pub fn is_management(self) -> bool {
        self.ftype() == ftype::MANAGEMENT
    }

    pub fn is_data(self) -> bool {
        self.ftype() == ftype::DATA
    }

    pub fn is_qos_data(self) -> bool {
        self.is_data() && self.subtype() & 0x8 != 0
    }

    pub fn with(mut self, bit: u16, on: bool) -> FrameControl {
        if on {
            self.0 |= 1 << bit;
        } else {
            self.0 &= !(1 << bit);
        }
        self
    }

    pub fn set_to_ds(self, on: bool) -> FrameControl {
        self.with(8, on)
    }

    pub fn set_from_ds(self, on: bool) -> FrameControl {
        self.with(9, on)
    }

    pub fn set_retry(self, on: bool) -> FrameControl {
        self.with(11, on)
    }

    pub fn set_power_mgmt(self, on: bool) -> FrameControl {
        self.with(12, on)
    }

    pub fn set_protected(self, on: bool) -> FrameControl {
        self.with(14, on)
    }
}

/// Length of the MAC header of a management or data frame (0 for control
/// frames, which this stack does not parse).
pub fn header_len(fc: FrameControl) -> usize {
    match fc.ftype() {
        ftype::MANAGEMENT => 24 + if fc.order() { 4 } else { 0 },
        ftype::DATA => {
            let mut n = 24;
            if fc.to_ds() && fc.from_ds() {
                n += 6;
            }
            if fc.is_qos_data() {
                n += 2;
                if fc.order() {
                    n += 4;
                }
            }
            n
        }
        _ => 0,
    }
}

/// A parsed MAC header of a management or data frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub fc: FrameControl,
    pub duration: u16,
    pub addr1: Mac,
    pub addr2: Mac,
    pub addr3: Mac,
    pub addr4: Option<Mac>,
    /// Sequence number (12 bits).
    pub seq: u16,
    /// Fragment number (4 bits).
    pub frag: u8,
    /// QoS control field of QoS data frames.
    pub qos: Option<u16>,
    /// Header length in bytes.
    pub len: usize,
}

impl Header {
    /// Parses the header; `None` for control frames or truncated data.
    pub fn parse(frame: &[u8]) -> Option<Header> {
        if frame.len() < 24 {
            return None;
        }
        let fc = FrameControl::from_bytes([frame[0], frame[1]]);
        if fc.protocol_version() != 0 || fc.ftype() == ftype::CONTROL || fc.ftype() == 3 {
            return None;
        }
        let len = header_len(fc);
        if frame.len() < len {
            return None;
        }
        let mac = |o: usize| -> Mac { frame[o..o + 6].try_into().unwrap() };
        let sc = u16::from_le_bytes([frame[22], frame[23]]);
        let addr4 = if fc.is_data() && fc.to_ds() && fc.from_ds() { Some(mac(24)) } else { None };
        let qos = if fc.is_qos_data() {
            let o = if addr4.is_some() { 30 } else { 24 };
            Some(u16::from_le_bytes([frame[o], frame[o + 1]]))
        } else {
            None
        };
        Some(Header {
            fc,
            duration: u16::from_le_bytes([frame[2], frame[3]]),
            addr1: mac(4),
            addr2: mac(10),
            addr3: mac(16),
            addr4,
            seq: sc >> 4,
            frag: (sc & 0xF) as u8,
            qos,
            len,
        })
    }

    /// The traffic identifier of a QoS data frame (0 otherwise).
    pub fn tid(&self) -> u8 {
        self.qos.map(|q| (q & 0xF) as u8).unwrap_or(0)
    }

    /// Whether a QoS data frame carries an A-MSDU.
    pub fn amsdu(&self) -> bool {
        self.qos.is_some_and(|q| q & 0x80 != 0)
    }
}

/// Builds a management frame.
pub fn management(subtype: u8, da: &Mac, sa: &Mac, bssid: &Mac, seq: u16, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(24 + body.len());
    f.extend_from_slice(&FrameControl::new(ftype::MANAGEMENT, subtype).to_bytes());
    f.extend_from_slice(&0u16.to_le_bytes());
    f.extend_from_slice(da);
    f.extend_from_slice(sa);
    f.extend_from_slice(bssid);
    f.extend_from_slice(&((seq & 0xFFF) << 4).to_le_bytes());
    f.extend_from_slice(body);
    f
}

/// Beacon and probe response bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeaconBody<'a> {
    pub timestamp: u64,
    /// Beacon interval in time units (1.024 ms).
    pub interval: u16,
    pub capability: u16,
    pub ies: &'a [u8],
}

/// Capability information bits.
pub mod capab {
    pub const ESS: u16 = 1 << 0;
    pub const PRIVACY: u16 = 1 << 4;
    pub const SHORT_PREAMBLE: u16 = 1 << 5;
    pub const SHORT_SLOT_TIME: u16 = 1 << 10;
}

impl<'a> BeaconBody<'a> {
    pub fn parse(body: &'a [u8]) -> Option<BeaconBody<'a>> {
        if body.len() < 12 {
            return None;
        }
        Some(BeaconBody {
            timestamp: u64::from_le_bytes(body[..8].try_into().unwrap()),
            interval: u16::from_le_bytes([body[8], body[9]]),
            capability: u16::from_le_bytes([body[10], body[11]]),
            ies: &body[12..],
        })
    }

    pub fn build(timestamp: u64, interval: u16, capability: u16, ies: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(12 + ies.len());
        b.extend_from_slice(&timestamp.to_le_bytes());
        b.extend_from_slice(&interval.to_le_bytes());
        b.extend_from_slice(&capability.to_le_bytes());
        b.extend_from_slice(ies);
        b
    }
}

/// An authentication frame body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthBody<'a> {
    pub algorithm: u16,
    pub transaction: u16,
    pub status: u16,
    /// Algorithm-specific fields (SAE commit/confirm).
    pub rest: &'a [u8],
}

impl<'a> AuthBody<'a> {
    pub fn parse(body: &'a [u8]) -> Option<AuthBody<'a>> {
        if body.len() < 6 {
            return None;
        }
        Some(AuthBody {
            algorithm: u16::from_le_bytes([body[0], body[1]]),
            transaction: u16::from_le_bytes([body[2], body[3]]),
            status: u16::from_le_bytes([body[4], body[5]]),
            rest: &body[6..],
        })
    }

    pub fn build(algorithm: u16, transaction: u16, status: u16, rest: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(6 + rest.len());
        b.extend_from_slice(&algorithm.to_le_bytes());
        b.extend_from_slice(&transaction.to_le_bytes());
        b.extend_from_slice(&status.to_le_bytes());
        b.extend_from_slice(rest);
        b
    }
}

/// An (re)association request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssocReqBody<'a> {
    pub capability: u16,
    pub listen_interval: u16,
    /// The current AP (reassociation only).
    pub current_ap: Option<Mac>,
    pub ies: &'a [u8],
}

impl<'a> AssocReqBody<'a> {
    pub fn parse(body: &'a [u8], reassoc: bool) -> Option<AssocReqBody<'a>> {
        let fixed = if reassoc { 10 } else { 4 };
        if body.len() < fixed {
            return None;
        }
        Some(AssocReqBody {
            capability: u16::from_le_bytes([body[0], body[1]]),
            listen_interval: u16::from_le_bytes([body[2], body[3]]),
            current_ap: if reassoc { Some(body[4..10].try_into().unwrap()) } else { None },
            ies: &body[fixed..],
        })
    }

    pub fn build(capability: u16, listen_interval: u16, ies: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + ies.len());
        b.extend_from_slice(&capability.to_le_bytes());
        b.extend_from_slice(&listen_interval.to_le_bytes());
        b.extend_from_slice(ies);
        b
    }
}

/// An (re)association response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssocRespBody<'a> {
    pub capability: u16,
    pub status: u16,
    /// Association id (the two top bits are set on the air).
    pub aid: u16,
    pub ies: &'a [u8],
}

impl<'a> AssocRespBody<'a> {
    pub fn parse(body: &'a [u8]) -> Option<AssocRespBody<'a>> {
        if body.len() < 6 {
            return None;
        }
        Some(AssocRespBody {
            capability: u16::from_le_bytes([body[0], body[1]]),
            status: u16::from_le_bytes([body[2], body[3]]),
            aid: u16::from_le_bytes([body[4], body[5]]) & 0x3FFF,
            ies: &body[6..],
        })
    }

    pub fn build(capability: u16, status: u16, aid: u16, ies: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(6 + ies.len());
        b.extend_from_slice(&capability.to_le_bytes());
        b.extend_from_slice(&status.to_le_bytes());
        b.extend_from_slice(&(aid | 0xC000).to_le_bytes());
        b.extend_from_slice(ies);
        b
    }
}

/// The reason code of a deauthentication or disassociation body.
pub fn reason_code(body: &[u8]) -> Option<u16> {
    (body.len() >= 2).then(|| u16::from_le_bytes([body[0], body[1]]))
}

/// The LLC/SNAP header for an EtherType (RFC 1042; bridge tunnel for AARP
/// and IPX as IEEE 802.1H requires).
fn snap_header(ethertype: u16) -> [u8; 8] {
    let oui = if ethertype == 0x80F3 || ethertype == 0x8137 { [0x00, 0x00, 0xF8] } else { [0x00, 0x00, 0x00] };
    let t = ethertype.to_be_bytes();
    [0xAA, 0xAA, 0x03, oui[0], oui[1], oui[2], t[0], t[1]]
}

/// Splits an LLC/SNAP-encapsulated MSDU into EtherType and payload.
pub fn parse_snap(msdu: &[u8]) -> Option<(u16, &[u8])> {
    if msdu.len() < 8 || msdu[..3] != [0xAA, 0xAA, 0x03] {
        return None;
    }
    let oui = &msdu[3..6];
    if oui != [0, 0, 0] && oui != [0, 0, 0xF8] {
        return None;
    }
    Some((u16::from_be_bytes([msdu[6], msdu[7]]), &msdu[8..]))
}

/// Builds a data frame from a station to its AP (ToDS): A1 = BSSID,
/// A2 = our address, A3 = the final destination.
pub fn data_to_ds(bssid: &Mac, sa: &Mac, da: &Mac, seq: u16, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    data_frame(FrameControl::new(ftype::DATA, data::DATA).set_to_ds(true), bssid, sa, da, seq, ethertype, payload)
}

/// Builds a data frame from an AP to a station (FromDS): A1 = the
/// destination, A2 = BSSID, A3 = the original source.
pub fn data_from_ds(da: &Mac, bssid: &Mac, sa: &Mac, seq: u16, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    data_frame(FrameControl::new(ftype::DATA, data::DATA).set_from_ds(true), da, bssid, sa, seq, ethertype, payload)
}

fn data_frame(fc: FrameControl, a1: &Mac, a2: &Mac, a3: &Mac, seq: u16, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(24 + 8 + payload.len() + 16);
    f.extend_from_slice(&fc.to_bytes());
    f.extend_from_slice(&0u16.to_le_bytes());
    f.extend_from_slice(a1);
    f.extend_from_slice(a2);
    f.extend_from_slice(a3);
    f.extend_from_slice(&((seq & 0xFFF) << 4).to_le_bytes());
    f.extend_from_slice(&snap_header(ethertype));
    f.extend_from_slice(payload);
    f
}

/// A null data frame to the AP (power management signalling).
pub fn null_to_ds(bssid: &Mac, sa: &Mac, seq: u16, power_save: bool) -> Vec<u8> {
    let fc = FrameControl::new(ftype::DATA, data::NULL).set_to_ds(true).set_power_mgmt(power_save);
    let mut f = Vec::with_capacity(24);
    f.extend_from_slice(&fc.to_bytes());
    f.extend_from_slice(&0u16.to_le_bytes());
    f.extend_from_slice(bssid);
    f.extend_from_slice(sa);
    f.extend_from_slice(bssid);
    f.extend_from_slice(&((seq & 0xFFF) << 4).to_le_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_control_fields() {
        let fc = FrameControl::from_bytes([0x88, 0x42]);
        assert_eq!((fc.ftype(), fc.subtype()), (ftype::DATA, 8));
        assert!(fc.is_qos_data() && fc.from_ds() && !fc.to_ds() && fc.protected());
        assert_eq!(header_len(fc), 26);
        let fc = FrameControl::from_bytes([0x88, 0x03 | 0x80]);
        assert_eq!(header_len(fc), 36);
        assert_eq!(header_len(FrameControl::new(ftype::MANAGEMENT, mgmt::BEACON)), 24);
        assert_eq!(FrameControl::new(ftype::MANAGEMENT, mgmt::AUTH).to_bytes(), [0xB0, 0x00]);
    }

    #[test]
    fn headers_round_trip() {
        let f = data_to_ds(&[1; 6], &[2; 6], &[3; 6], 0x123, 0x0800, b"payload");
        let h = Header::parse(&f).unwrap();
        assert!(h.fc.to_ds());
        assert_eq!((h.addr1, h.addr2, h.addr3, h.seq, h.len), ([1; 6], [2; 6], [3; 6], 0x123, 24));
        assert_eq!(parse_snap(&f[24..]), Some((0x0800, &b"payload"[..])));
        assert!(Header::parse(&f[..23]).is_none());
        // Control frames and protocol version 1 are not parsed.
        assert!(Header::parse(&[0xD4, 0, 0, 0, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        let mut v1 = f.clone();
        v1[0] |= 1;
        assert!(Header::parse(&v1).is_none());
    }

    #[test]
    fn bodies() {
        let b = BeaconBody::build(77, 100, capab::ESS | capab::PRIVACY, &[0, 3, b'a', b'b', b'c']);
        let p = BeaconBody::parse(&b).unwrap();
        assert_eq!((p.timestamp, p.interval, p.capability, p.ies), (77, 100, 0x11, &[0, 3, b'a', b'b', b'c'][..]));
        let a = AuthBody::build(auth_alg::SAE, 1, status::SAE_HASH_TO_ELEMENT, &[9, 9]);
        assert_eq!(AuthBody::parse(&a).unwrap().rest, &[9, 9]);
        let r = AssocRespBody::build(0x11, 0, 5, &[]);
        assert_eq!(AssocRespBody::parse(&r).unwrap().aid, 5);
        assert_eq!(reason_code(&[3, 0]), Some(3));
        assert_eq!(reason_code(&[3]), None);
        assert!(AuthBody::parse(&[0; 5]).is_none());
        assert!(AssocReqBody::parse(&[0; 9], true).is_none());
    }

    #[test]
    fn snap() {
        assert_eq!(parse_snap(&snap_header(0x8137)), Some((0x8137, &[][..])));
        assert_eq!(snap_header(0x8137)[5], 0xF8);
        assert!(parse_snap(&[0xAA, 0xAA, 0x03, 0x00, 0x00, 0x01, 0x08, 0x00]).is_none());
        assert!(parse_snap(&[0xAA, 0xAA, 0x03]).is_none());
    }
}
