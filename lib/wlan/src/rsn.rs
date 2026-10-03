//! The RSN element (IEEE 802.11-2020, 9.4.2.24): cipher and AKM suites,
//! capabilities, and the choice of security for a connection.

use alloc::vec::Vec;

const OUI: [u8; 3] = [0x00, 0x0F, 0xAC];

/// Cipher suites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cipher {
    UseGroup,
    Wep40,
    Tkip,
    Ccmp128,
    Wep104,
    BipCmac128,
    GroupNotAllowed,
    Gcmp128,
    Gcmp256,
    Ccmp256,
    BipGmac128,
    BipGmac256,
    BipCmac256,
    /// Anything else (vendor or unknown suites), as the raw selector.
    Other(u32),
}

impl Cipher {
    fn from_suite(s: [u8; 4]) -> Cipher {
        if s[..3] != OUI {
            return Cipher::Other(u32::from_be_bytes(s));
        }
        match s[3] {
            0 => Cipher::UseGroup,
            1 => Cipher::Wep40,
            2 => Cipher::Tkip,
            4 => Cipher::Ccmp128,
            5 => Cipher::Wep104,
            6 => Cipher::BipCmac128,
            7 => Cipher::GroupNotAllowed,
            8 => Cipher::Gcmp128,
            9 => Cipher::Gcmp256,
            10 => Cipher::Ccmp256,
            11 => Cipher::BipGmac128,
            12 => Cipher::BipGmac256,
            13 => Cipher::BipCmac256,
            _ => Cipher::Other(u32::from_be_bytes(s)),
        }
    }

    fn suite(self) -> [u8; 4] {
        let n = match self {
            Cipher::UseGroup => 0,
            Cipher::Wep40 => 1,
            Cipher::Tkip => 2,
            Cipher::Ccmp128 => 4,
            Cipher::Wep104 => 5,
            Cipher::BipCmac128 => 6,
            Cipher::GroupNotAllowed => 7,
            Cipher::Gcmp128 => 8,
            Cipher::Gcmp256 => 9,
            Cipher::Ccmp256 => 10,
            Cipher::BipGmac128 => 11,
            Cipher::BipGmac256 => 12,
            Cipher::BipCmac256 => 13,
            Cipher::Other(v) => return v.to_be_bytes(),
        };
        [OUI[0], OUI[1], OUI[2], n]
    }

    fn is_mgmt(self) -> bool {
        matches!(self, Cipher::BipCmac128 | Cipher::BipGmac128 | Cipher::BipGmac256 | Cipher::BipCmac256)
    }
}

/// Authentication and key management suites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Akm {
    Ieee8021x,
    Psk,
    FtIeee8021x,
    FtPsk,
    Ieee8021xSha256,
    PskSha256,
    Sae,
    FtSae,
    Ieee8021xSuiteB,
    Ieee8021xSuiteB192,
    Owe,
    SaeExtKey,
    Other(u32),
}

impl Akm {
    fn from_suite(s: [u8; 4]) -> Akm {
        if s[..3] != OUI {
            return Akm::Other(u32::from_be_bytes(s));
        }
        match s[3] {
            1 => Akm::Ieee8021x,
            2 => Akm::Psk,
            3 => Akm::FtIeee8021x,
            4 => Akm::FtPsk,
            5 => Akm::Ieee8021xSha256,
            6 => Akm::PskSha256,
            8 => Akm::Sae,
            9 => Akm::FtSae,
            11 => Akm::Ieee8021xSuiteB,
            12 => Akm::Ieee8021xSuiteB192,
            18 => Akm::Owe,
            24 => Akm::SaeExtKey,
            _ => Akm::Other(u32::from_be_bytes(s)),
        }
    }

    fn suite(self) -> [u8; 4] {
        let n = match self {
            Akm::Ieee8021x => 1,
            Akm::Psk => 2,
            Akm::FtIeee8021x => 3,
            Akm::FtPsk => 4,
            Akm::Ieee8021xSha256 => 5,
            Akm::PskSha256 => 6,
            Akm::Sae => 8,
            Akm::FtSae => 9,
            Akm::Ieee8021xSuiteB => 11,
            Akm::Ieee8021xSuiteB192 => 12,
            Akm::Owe => 18,
            Akm::SaeExtKey => 24,
            Akm::Other(v) => return v.to_be_bytes(),
        };
        [OUI[0], OUI[1], OUI[2], n]
    }

    fn is_enterprise(self) -> bool {
        matches!(
            self,
            Akm::Ieee8021x | Akm::FtIeee8021x | Akm::Ieee8021xSha256 | Akm::Ieee8021xSuiteB | Akm::Ieee8021xSuiteB192
        )
    }

    /// The key derivation and MIC algorithms of this AKM (for the AKMs this
    /// stack implements).
    pub fn key_algo(self) -> Option<crate::crypto::KeyAlgo> {
        match self {
            Akm::Psk => Some(crate::crypto::KeyAlgo::Sha1),
            Akm::PskSha256 | Akm::Sae => Some(crate::crypto::KeyAlgo::Sha256),
            _ => None,
        }
    }
}

/// RSN capability bits.
pub mod caps {
    pub const PREAUTH: u16 = 1 << 0;
    /// Management frame protection required.
    pub const MFPR: u16 = 1 << 6;
    /// Management frame protection capable.
    pub const MFPC: u16 = 1 << 7;
    pub const SPP_AMSDU_CAPABLE: u16 = 1 << 10;
    pub const EXT_KEY_ID: u16 = 1 << 13;
}

/// Why an RSN element was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsnError {
    TooShort,
    BadVersion,
    Truncated,
    NoPairwise,
    NoAkm,
    BadPairwise,
    BadGroupMgmt,
}

/// A parsed RSN element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rsne {
    pub group: Cipher,
    pub pairwise: Vec<Cipher>,
    pub akms: Vec<Akm>,
    pub capabilities: u16,
    pub pmkids: Vec<[u8; 16]>,
    pub group_mgmt: Option<Cipher>,
}

impl Rsne {
    /// Parses an RSN element body. Omitted trailing fields take their
    /// defaults (CCMP, PSK... per the standard: CCMP-128 and 802.1X);
    /// extra trailing bytes are ignored.
    pub fn parse(body: &[u8]) -> Result<Rsne, RsnError> {
        if body.len() < 2 {
            return Err(RsnError::TooShort);
        }
        if u16::from_le_bytes([body[0], body[1]]) != 1 {
            return Err(RsnError::BadVersion);
        }
        let mut r = Rsne {
            group: Cipher::Ccmp128,
            pairwise: alloc::vec![Cipher::Ccmp128],
            akms: alloc::vec![Akm::Ieee8021x],
            capabilities: 0,
            pmkids: Vec::new(),
            group_mgmt: None,
        };
        let mut p = &body[2..];
        let suite = |p: &mut &[u8]| -> Result<[u8; 4], RsnError> {
            if p.len() < 4 {
                return Err(RsnError::Truncated);
            }
            let s = [p[0], p[1], p[2], p[3]];
            *p = &p[4..];
            Ok(s)
        };
        let count = |p: &mut &[u8], each: usize| -> Result<usize, RsnError> {
            if p.len() < 2 {
                return Err(RsnError::Truncated);
            }
            let n = u16::from_le_bytes([p[0], p[1]]) as usize;
            *p = &p[2..];
            if n.checked_mul(each).is_none_or(|need| need > p.len()) {
                return Err(RsnError::Truncated);
            }
            Ok(n)
        };
        if p.is_empty() {
            return Ok(r);
        }
        r.group = Cipher::from_suite(suite(&mut p)?);
        if p.is_empty() {
            return Ok(r);
        }
        let n = count(&mut p, 4)?;
        if n == 0 {
            return Err(RsnError::NoPairwise);
        }
        r.pairwise = (0..n).map(|_| Cipher::from_suite(suite(&mut p).unwrap())).collect();
        if r.pairwise.iter().any(|c| c.is_mgmt() || *c == Cipher::GroupNotAllowed) {
            return Err(RsnError::BadPairwise);
        }
        if p.is_empty() {
            return Ok(r);
        }
        let n = count(&mut p, 4)?;
        if n == 0 {
            return Err(RsnError::NoAkm);
        }
        r.akms = (0..n).map(|_| Akm::from_suite(suite(&mut p).unwrap())).collect();
        if p.len() < 2 {
            return Ok(r);
        }
        r.capabilities = u16::from_le_bytes([p[0], p[1]]);
        p = &p[2..];
        if p.len() < 2 {
            return Ok(r);
        }
        let n = count(&mut p, 16)?;
        for _ in 0..n {
            let mut id = [0u8; 16];
            id.copy_from_slice(&p[..16]);
            p = &p[16..];
            r.pmkids.push(id);
        }
        if p.len() >= 4 {
            let c = Cipher::from_suite(suite(&mut p)?);
            if !c.is_mgmt() {
                return Err(RsnError::BadGroupMgmt);
            }
            r.group_mgmt = Some(c);
        }
        Ok(r)
    }

    /// Encodes the element body.
    pub fn body(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32);
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&self.group.suite());
        b.extend_from_slice(&(self.pairwise.len() as u16).to_le_bytes());
        for c in &self.pairwise {
            b.extend_from_slice(&c.suite());
        }
        b.extend_from_slice(&(self.akms.len() as u16).to_le_bytes());
        for a in &self.akms {
            b.extend_from_slice(&a.suite());
        }
        b.extend_from_slice(&self.capabilities.to_le_bytes());
        if !self.pmkids.is_empty() || self.group_mgmt.is_some() {
            b.extend_from_slice(&(self.pmkids.len() as u16).to_le_bytes());
            for id in &self.pmkids {
                b.extend_from_slice(id);
            }
            if let Some(g) = self.group_mgmt {
                b.extend_from_slice(&g.suite());
            }
        }
        b
    }

    /// The whole element (ID, length, body).
    pub fn element(&self) -> Vec<u8> {
        let body = self.body();
        let mut e = Vec::with_capacity(body.len() + 2);
        e.push(crate::ie::id::RSN);
        e.push(body.len() as u8);
        e.extend_from_slice(&body);
        e
    }

    pub fn mfpc(&self) -> bool {
        self.capabilities & caps::MFPC != 0
    }

    pub fn mfpr(&self) -> bool {
        self.capabilities & caps::MFPR != 0
    }
}

/// The security of a network as shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Security {
    Open,
    Wep,
    WpaTkip,
    Wpa2Personal,
    Wpa3Personal,
    Wpa2Wpa3Personal,
    Enterprise,
    Owe,
}

impl Security {
    pub fn supported(self) -> bool {
        matches!(self, Security::Open | Security::Wpa2Personal | Security::Wpa3Personal | Security::Wpa2Wpa3Personal)
    }
}

/// Classifies a BSS from its capability privacy bit and its RSN/WPA
/// elements.
pub fn classify(privacy: bool, rsne: Option<&Rsne>, wpa1: bool) -> Security {
    let Some(r) = rsne else {
        return if wpa1 {
            Security::WpaTkip
        } else if privacy {
            Security::Wep
        } else {
            Security::Open
        };
    };
    if r.akms.iter().any(|a| a.is_enterprise()) {
        return Security::Enterprise;
    }
    let sae = r.akms.iter().any(|a| matches!(a, Akm::Sae | Akm::SaeExtKey | Akm::FtSae));
    let psk = r.akms.iter().any(|a| matches!(a, Akm::Psk | Akm::PskSha256 | Akm::FtPsk));
    // A TKIP group cipher (WPA/WPA2 mixed mode) needs TKIP, which this
    // stack does not implement (it is broken anyway).
    let ccmp = r.pairwise.contains(&Cipher::Ccmp128) && r.group == Cipher::Ccmp128;
    if !ccmp && (sae || psk) {
        return Security::WpaTkip;
    }
    match (sae, psk) {
        (true, true) => Security::Wpa2Wpa3Personal,
        (true, false) => Security::Wpa3Personal,
        (false, true) => Security::Wpa2Personal,
        _ if r.akms.contains(&Akm::Owe) => Security::Owe,
        _ => Security::Enterprise,
    }
}

/// Management frame protection on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pmf {
    Off,
    /// Negotiated (both capable).
    On,
}

/// The negotiated security parameters of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub akm: Akm,
    pub pmf: Pmf,
    /// Our RSN element for the association request (whole element).
    pub own_rsne: Vec<u8>,
}

/// Why a network cannot be joined with the credentials at hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectError {
    /// Ciphers or AKMs this stack does not implement.
    Unsupported,
    /// The access point's RSN element is inconsistent (e.g. SAE without
    /// management frame protection).
    Invalid,
}

/// Chooses the AKM, ciphers and PMF for joining a network with RSN element
/// `ap` using a password: SAE (WPA3) when offered, else PSK-SHA256, else
/// PSK. `allow_sae` is false when only WPA2 may be used (a WPA2-only saved
/// network).
pub fn select(ap: &Rsne, allow_sae: bool) -> Result<Selection, SelectError> {
    if ap.group != Cipher::Ccmp128 || !ap.pairwise.contains(&Cipher::Ccmp128) {
        return Err(SelectError::Unsupported);
    }
    // Management frame protection is possible when the AP offers it with
    // the one group management cipher implemented here (BIP-CMAC-128).
    let pmf_possible = ap.mfpc() && ap.group_mgmt.is_none_or(|c| c == Cipher::BipCmac128);
    if ap.mfpr() && !pmf_possible {
        return Err(SelectError::Unsupported);
    }
    let akm = if allow_sae && ap.akms.contains(&Akm::Sae) && pmf_possible {
        Akm::Sae
    } else if ap.akms.contains(&Akm::PskSha256) {
        Akm::PskSha256
    } else if ap.akms.contains(&Akm::Psk) {
        Akm::Psk
    } else if ap.akms.contains(&Akm::Sae) && !ap.mfpc() {
        // SAE requires management frame protection.
        return Err(SelectError::Invalid);
    } else {
        return Err(SelectError::Unsupported);
    };
    let pmf = if pmf_possible { Pmf::On } else { Pmf::Off };
    let mut capabilities = 0;
    if pmf == Pmf::On {
        capabilities |= caps::MFPC;
        // SAE requires protection; so does an AP that requires it.
        if akm == Akm::Sae || ap.mfpr() {
            capabilities |= caps::MFPR;
        }
    }
    let own = Rsne {
        group: Cipher::Ccmp128,
        pairwise: alloc::vec![Cipher::Ccmp128],
        akms: alloc::vec![akm],
        capabilities,
        pmkids: Vec::new(),
        group_mgmt: None,
    };
    Ok(Selection { akm, pmf, own_rsne: own.element() })
}

/// The RSN element of an access point offering `security` (for the
/// simulated access points and the tests).
pub fn ap_rsne(security: Security, pmf_required: bool) -> Option<Rsne> {
    let (akms, mut capabilities) = match security {
        Security::Wpa2Personal => {
            (alloc::vec![Akm::Psk], if pmf_required { caps::MFPC | caps::MFPR } else { caps::MFPC })
        }
        Security::Wpa3Personal => (alloc::vec![Akm::Sae], caps::MFPC | caps::MFPR),
        Security::Wpa2Wpa3Personal => (alloc::vec![Akm::Psk, Akm::Sae], caps::MFPC),
        // Advertised only (simulated networks that stations cannot join).
        Security::Enterprise => (alloc::vec![Akm::Ieee8021x], caps::MFPC),
        _ => return None,
    };
    if pmf_required {
        capabilities |= caps::MFPR | caps::MFPC;
    }
    Some(Rsne {
        group: Cipher::Ccmp128,
        pairwise: alloc::vec![Cipher::Ccmp128],
        akms,
        capabilities,
        pmkids: Vec::new(),
        group_mgmt: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(b: &[u8]) -> Result<Rsne, RsnError> {
        // Strip the element header used in hostap's test strings.
        Rsne::parse(&b[2..])
    }

    /// Cases from hostap's rsn_ie_parse tests (where both agree that the
    /// element is valid or invalid).
    #[test]
    fn hostap_parse_cases() {
        assert!(Rsne::parse(b"").is_err());
        assert!(parse(b"\x30\x02\x01\x00").is_ok());
        assert_eq!(parse(b"\x30\x02\x00\x00"), Err(RsnError::BadVersion));
        assert_eq!(parse(b"\x30\x02\x02\x00"), Err(RsnError::BadVersion));
        assert_eq!(parse(b"\x30\x03\x01\x00\x00"), Err(RsnError::Truncated));
        assert!(parse(b"\x30\x06\x01\x00\x00\x0f\xac\x04").is_ok());
        assert_eq!(parse(b"\x30\x07\x01\x00\x00\x0f\xac\x04\x00"), Err(RsnError::Truncated));
        assert_eq!(parse(b"\x30\x08\x01\x00\x00\x0f\xac\x04\x00\x00"), Err(RsnError::NoPairwise));
        assert_eq!(parse(b"\x30\x08\x01\x00\x00\x0f\xac\x04\x00\x01"), Err(RsnError::Truncated));
        assert!(parse(b"\x30\x0c\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04").is_ok());
        assert!(parse(b"\x30\x0c\x01\x00\x00\x0f\xac\x04\x00\x01\x00\x0f\xac\x04").is_err());
        assert_eq!(parse(b"\x30\x0c\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x06"), Err(RsnError::BadPairwise));
        assert!(parse(b"\x30\x10\x01\x00\x00\x0f\xac\x04\x02\x00\x00\x0f\xac\x04\x00\x0f\xac\x08").is_ok());
        assert_eq!(parse(b"\x30\x0d\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x00"), Err(RsnError::Truncated));
        assert_eq!(parse(b"\x30\x0e\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x00\x00"), Err(RsnError::NoAkm));
        assert_eq!(
            parse(b"\x30\x0e\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x00\x01"),
            Err(RsnError::Truncated)
        );
        let r =
            parse(b"\x30\x16\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x02\x00\x00\x0f\xac\x01\x00\x0f\xac\x02")
                .unwrap();
        assert_eq!(r.akms, alloc::vec![Akm::Ieee8021x, Akm::Psk]);
        assert!(
            parse(b"\x30\x14\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x01\x00\x00").is_ok()
        );
        assert_eq!(
            parse(b"\x30\x16\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x01\x00\x00\x01\x00"),
            Err(RsnError::Truncated)
        );
        assert_eq!(
            parse(b"\x30\x1a\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x01\x00\x00\x00\x00\x00\x00\x00\x00"),
            Err(RsnError::BadGroupMgmt)
        );
        let r = parse(b"\x30\x1a\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x04\x01\x00\x00\x0f\xac\x01\x00\x00\x00\x00\x00\x0f\xac\x06").unwrap();
        assert_eq!(r.group_mgmt, Some(Cipher::BipCmac128));
    }

    #[test]
    fn encode_round_trip() {
        for sec in [Security::Wpa2Personal, Security::Wpa3Personal, Security::Wpa2Wpa3Personal] {
            let r = ap_rsne(sec, false).unwrap();
            assert_eq!(Rsne::parse(&r.body()).unwrap(), r);
            assert_eq!(classify(true, Some(&r), false), sec);
        }
        let mut r = ap_rsne(Security::Wpa2Personal, false).unwrap();
        r.group_mgmt = Some(Cipher::BipCmac128);
        r.pmkids.push([7; 16]);
        assert_eq!(Rsne::parse(&r.body()).unwrap(), r);
    }

    #[test]
    fn classification() {
        assert_eq!(classify(false, None, false), Security::Open);
        assert_eq!(classify(true, None, false), Security::Wep);
        assert_eq!(classify(true, None, true), Security::WpaTkip);
        let mut r = ap_rsne(Security::Wpa2Personal, false).unwrap();
        r.group = Cipher::Tkip;
        assert_eq!(classify(true, Some(&r), true), Security::WpaTkip);
        r.group = Cipher::Ccmp128;
        r.akms = alloc::vec![Akm::Ieee8021x];
        assert_eq!(classify(true, Some(&r), false), Security::Enterprise);
        r.akms = alloc::vec![Akm::Owe];
        assert_eq!(classify(true, Some(&r), false), Security::Owe);
    }

    #[test]
    fn selection() {
        let wpa2 = ap_rsne(Security::Wpa2Personal, false).unwrap();
        let s = select(&wpa2, true).unwrap();
        assert_eq!((s.akm, s.pmf), (Akm::Psk, Pmf::On));
        let own = Rsne::parse(&s.own_rsne[2..]).unwrap();
        assert!(own.mfpc() && !own.mfpr());
        let wpa3 = ap_rsne(Security::Wpa3Personal, false).unwrap();
        let s = select(&wpa3, true).unwrap();
        assert_eq!(s.akm, Akm::Sae);
        assert!(Rsne::parse(&s.own_rsne[2..]).unwrap().mfpr());
        // Transition mode: WPA3 unless SAE is not allowed.
        let mixed = ap_rsne(Security::Wpa2Wpa3Personal, false).unwrap();
        assert_eq!(select(&mixed, true).unwrap().akm, Akm::Sae);
        assert_eq!(select(&mixed, false).unwrap().akm, Akm::Psk);
        // SAE without management frame protection is invalid.
        let mut bad = wpa3.clone();
        bad.capabilities = 0;
        assert_eq!(select(&bad, true), Err(SelectError::Invalid));
        // Unsupported ciphers.
        let mut gcmp = wpa2.clone();
        gcmp.pairwise = alloc::vec![Cipher::Gcmp256];
        assert_eq!(select(&gcmp, true), Err(SelectError::Unsupported));
        // An AP without PMF support: no PMF.
        let mut legacy = wpa2.clone();
        legacy.capabilities = 0;
        assert_eq!(select(&legacy, true).unwrap().pmf, Pmf::Off);
    }
}
