//! Information elements: parsing the element list of beacons, probe
//! responses and association frames, and building our own.
//!
//! An element list from the air is untrusted. [`Elements::parse`] walks it
//! with bounds checks, keeps the first occurrence of each element it
//! understands, and rejects lists whose last element overruns the frame.

use alloc::vec::Vec;

/// Element IDs.
pub mod id {
    pub const SSID: u8 = 0;
    pub const SUPPORTED_RATES: u8 = 1;
    pub const DS_PARAMS: u8 = 3;
    pub const TIM: u8 = 5;
    pub const COUNTRY: u8 = 7;
    pub const HT_CAPABILITIES: u8 = 45;
    pub const RSN: u8 = 48;
    pub const EXTENDED_RATES: u8 = 50;
    pub const HT_OPERATION: u8 = 61;
    pub const MMIE: u8 = 76;
    pub const EXTENDED_CAPABILITIES: u8 = 127;
    pub const VENDOR: u8 = 221;
    pub const RSNX: u8 = 244;
    pub const EXTENSION: u8 = 255;
}

/// Extension element IDs (after element ID 255).
pub mod ext {
    pub const PASSWORD_IDENTIFIER: u8 = 33;
    pub const REJECTED_GROUPS: u8 = 92;
    pub const ANTI_CLOGGING_TOKEN: u8 = 93;
}

/// Bits of the RSN Extension element's capabilities.
pub mod rsnx {
    /// SAE hash-to-element (bit 5 of the first octet).
    pub const SAE_H2E: u8 = 1 << 5;
    pub const SAE_PK: u8 = 1 << 6;
}

/// Why an element list was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IeError {
    /// An element's length runs past the end of the list.
    Truncated,
}

/// Iterates over `(id, body)` pairs; stops at the first malformed element.
pub fn iter(data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = data;
    core::iter::from_fn(move || {
        if rest.len() < 2 {
            return None;
        }
        let (id, len) = (rest[0], rest[1] as usize);
        if rest.len() < 2 + len {
            rest = &[];
            return None;
        }
        let body = &rest[2..2 + len];
        rest = &rest[2 + len..];
        Some((id, body))
    })
}

/// The elements this stack uses, borrowed from the frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Elements<'a> {
    pub ssid: Option<&'a [u8]>,
    /// Supported and extended rates (500 kbit/s units; bit 7 = basic).
    pub rates: Vec<u8>,
    /// Channel from the DS Parameter Set.
    pub ds_channel: Option<u8>,
    /// Primary channel from the HT Operation element.
    pub ht_channel: Option<u8>,
    pub ht_capable: bool,
    /// RSN element body.
    pub rsn: Option<&'a [u8]>,
    /// RSN Extension element body.
    pub rsnx: Option<&'a [u8]>,
    /// A WPA (version 1) vendor element is present.
    pub wpa1: bool,
    /// A WMM vendor element is present.
    pub wmm: bool,
    pub country: Option<&'a [u8]>,
    pub extended_capabilities: Option<&'a [u8]>,
    /// Bodies of extension elements (without the extension ID byte).
    pub password_identifier: Option<&'a [u8]>,
    pub rejected_groups: Option<&'a [u8]>,
    pub anti_clogging_token: Option<&'a [u8]>,
}

impl<'a> Elements<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Elements<'a>, IeError> {
        let mut e = Elements::default();
        let mut consumed = 0usize;
        for (eid, body) in iter(data) {
            consumed += 2 + body.len();
            match eid {
                id::SSID if e.ssid.is_none() && body.len() <= 32 => e.ssid = Some(body),
                id::SUPPORTED_RATES | id::EXTENDED_RATES => {
                    for &r in body {
                        if e.rates.len() < 32 {
                            e.rates.push(r);
                        }
                    }
                }
                id::DS_PARAMS if body.len() == 1 => e.ds_channel = Some(body[0]),
                id::HT_OPERATION if !body.is_empty() => e.ht_channel = Some(body[0]),
                id::HT_CAPABILITIES => e.ht_capable = true,
                id::RSN if e.rsn.is_none() => e.rsn = Some(body),
                id::RSNX if e.rsnx.is_none() => e.rsnx = Some(body),
                id::COUNTRY if e.country.is_none() => e.country = Some(body),
                id::EXTENDED_CAPABILITIES if e.extended_capabilities.is_none() => e.extended_capabilities = Some(body),
                id::VENDOR if body.len() >= 4 && body[..3] == [0x00, 0x50, 0xF2] => match body[3] {
                    1 => e.wpa1 = true,
                    2 => e.wmm = true,
                    _ => {}
                },
                id::EXTENSION if !body.is_empty() => match body[0] {
                    ext::PASSWORD_IDENTIFIER => e.password_identifier = Some(&body[1..]),
                    ext::REJECTED_GROUPS => e.rejected_groups = Some(&body[1..]),
                    ext::ANTI_CLOGGING_TOKEN => e.anti_clogging_token = Some(&body[1..]),
                    _ => {}
                },
                _ => {}
            }
        }
        if consumed != data.len() {
            return Err(IeError::Truncated);
        }
        Ok(e)
    }

    /// The operating channel (HT Operation, else DS Parameter Set).
    pub fn channel(&self) -> Option<u8> {
        self.ht_channel.or(self.ds_channel)
    }

    /// Whether the RSNX element advertises SAE hash-to-element.
    pub fn sae_h2e(&self) -> bool {
        self.rsnx.is_some_and(|b| !b.is_empty() && b[0] & rsnx::SAE_H2E != 0)
    }
}

/// Builds element lists.
#[derive(Debug, Clone, Default)]
pub struct Builder {
    pub buf: Vec<u8>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder { buf: Vec::with_capacity(128) }
    }

    /// Adds an element (bodies longer than 255 bytes are truncated).
    pub fn element(mut self, eid: u8, body: &[u8]) -> Builder {
        let n = body.len().min(255);
        self.buf.push(eid);
        self.buf.push(n as u8);
        self.buf.extend_from_slice(&body[..n]);
        self
    }

    pub fn extension(self, ext_id: u8, body: &[u8]) -> Builder {
        let mut b = Vec::with_capacity(body.len() + 1);
        b.push(ext_id);
        b.extend_from_slice(body);
        self.element(id::EXTENSION, &b)
    }

    pub fn ssid(self, ssid: &[u8]) -> Builder {
        self.element(id::SSID, ssid)
    }

    /// Supported rates (the first eight) and extended rates (the rest).
    pub fn rates(self, rates: &[u8]) -> Builder {
        let (a, b) = rates.split_at(rates.len().min(8));
        let mut s = self.element(id::SUPPORTED_RATES, a);
        if !b.is_empty() {
            s = s.element(id::EXTENDED_RATES, b);
        }
        s
    }

    pub fn ds_channel(self, channel: u8) -> Builder {
        self.element(id::DS_PARAMS, &[channel])
    }

    /// Appends raw, already encoded elements.
    pub fn raw(mut self, bytes: &[u8]) -> Builder {
        self.buf.extend_from_slice(bytes);
        self
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// The rates of 802.11g (ERP-OFDM and DSSS/CCK) in 500 kbit/s units, with
/// the 802.11b rates marked basic.
pub const RATES_G: [u8; 12] = [0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6C];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_elements() {
        let list = Builder::new()
            .ssid(b"VindowsNet")
            .rates(&RATES_G)
            .ds_channel(6)
            .element(id::RSN, &[1, 0, 0, 0x0F, 0xAC, 4])
            .element(id::RSNX, &[rsnx::SAE_H2E])
            .element(id::VENDOR, &[0x00, 0x50, 0xF2, 2, 0, 1])
            .extension(ext::ANTI_CLOGGING_TOKEN, b"token")
            .build();
        let e = Elements::parse(&list).unwrap();
        assert_eq!(e.ssid, Some(&b"VindowsNet"[..]));
        assert_eq!(e.rates, RATES_G.to_vec());
        assert_eq!(e.channel(), Some(6));
        assert_eq!(e.rsn, Some(&[1, 0, 0, 0x0F, 0xAC, 4][..]));
        assert!(e.sae_h2e() && e.wmm && !e.wpa1);
        assert_eq!(e.anti_clogging_token, Some(&b"token"[..]));
    }

    #[test]
    fn rejects_overruns_and_keeps_first_ssid() {
        // From hostap's element parser tests.
        assert_eq!(Elements::parse(b" "), Err(IeError::Truncated));
        assert_eq!(Elements::parse(b"\xff\x01"), Err(IeError::Truncated));
        assert!(Elements::parse(b"").is_ok());
        assert!(Elements::parse(b"\xff\x00").is_ok());
        assert!(Elements::parse(b"\xdd\x03\x01\x02\x03").is_ok());
        let two = [0u8, 1, b'a', 0, 1, b'b'];
        assert_eq!(Elements::parse(&two).unwrap().ssid, Some(&b"a"[..]));
        // An over-long SSID is ignored rather than trusted.
        let mut long = alloc::vec![0u8, 33];
        long.extend_from_slice(&[b'x'; 33]);
        assert_eq!(Elements::parse(&long).unwrap().ssid, None);
    }

    #[test]
    fn random_lists_never_panic() {
        let mut s = 0x2545_F491u32;
        for _ in 0..50_000 {
            let mut buf = [0u8; 64];
            let n = (s % 64) as usize;
            for b in buf[..n].iter_mut() {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                *b = s as u8;
            }
            let _ = Elements::parse(&buf[..n]);
            s = s.wrapping_add(0x9E37_79B9);
        }
    }
}
