//! Scanning: turning beacons and probe responses into BSS descriptions,
//! and building probe requests.

use alloc::vec::Vec;

use crate::frame::{self, BeaconBody, Header, Mac, mgmt};
use crate::ie::{self, Elements};
use crate::rsn::{Rsne, Security, classify};

/// An access point heard during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bss {
    pub bssid: Mac,
    /// The SSID (empty for a hidden network's beacons).
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub signal_dbm: i8,
    /// Beacon interval in time units (1.024 ms).
    pub beacon_interval: u16,
    pub capability: u16,
    pub rates: Vec<u8>,
    /// The RSN element, whole (as it must reappear in message 3).
    pub rsne: Option<Vec<u8>>,
    /// The RSN Extension element, whole.
    pub rsnx: Option<Vec<u8>>,
    pub security: Security,
    /// Management frame protection: 0 none, 1 capable, 2 required.
    pub pmf: u8,
    /// SAE hash-to-element is supported.
    pub h2e: bool,
    /// When it was last heard (milliseconds, caller's clock).
    pub seen_ms: u64,
}

impl Bss {
    /// Whether the SSID is hidden in this frame (empty or all zeros).
    pub fn hidden(&self) -> bool {
        self.ssid.iter().all(|&b| b == 0)
    }
}

fn whole(eid: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 2);
    v.push(eid);
    v.push(body.len() as u8);
    v.extend_from_slice(body);
    v
}

/// Parses a beacon or probe response heard on `channel` at `signal_dbm`.
pub fn parse_bss(f: &[u8], channel: u8, signal_dbm: i8, now_ms: u64) -> Option<Bss> {
    let h = Header::parse(f)?;
    if !h.fc.is_management() || !matches!(h.fc.subtype(), mgmt::BEACON | mgmt::PROBE_RESP) || h.fc.protected() {
        return None;
    }
    let body = BeaconBody::parse(&f[h.len..])?;
    let e = Elements::parse(body.ies).ok()?;
    let rsne_parsed = e.rsn.and_then(|b| Rsne::parse(b).ok());
    let privacy = body.capability & frame::capab::PRIVACY != 0;
    let security = if e.rsn.is_some() && rsne_parsed.is_none() {
        // An RSN element we cannot parse: never treat it as open.
        Security::Enterprise
    } else {
        classify(privacy, rsne_parsed.as_ref(), e.wpa1)
    };
    let pmf = match &rsne_parsed {
        Some(r) if r.mfpr() => 2,
        Some(r) if r.mfpc() => 1,
        _ => 0,
    };
    Some(Bss {
        bssid: h.addr3,
        ssid: e.ssid.unwrap_or(&[]).to_vec(),
        channel: e.channel().unwrap_or(channel),
        signal_dbm,
        beacon_interval: body.interval,
        capability: body.capability,
        rates: e.rates.clone(),
        rsne: e.rsn.map(|b| whole(ie::id::RSN, b)),
        rsnx: e.rsnx.map(|b| whole(ie::id::RSNX, b)),
        security,
        pmf,
        h2e: e.sae_h2e(),
        seen_ms: now_ms,
    })
}

/// A probe request on `channel` (broadcast, for one SSID or any).
pub fn probe_request(own: &Mac, ssid: Option<&[u8]>, channel: u8, seq: u16) -> Vec<u8> {
    let ies = ie::Builder::new().ssid(ssid.unwrap_or(&[])).rates(ie::rates_for(channel)).build();
    frame::management(mgmt::PROBE_REQ, &frame::BROADCAST, own, &frame::BROADCAST, seq, &ies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::capab;
    use crate::rsn::ap_rsne;

    fn beacon(ssid: &[u8], channel: u8, rsne: Option<Vec<u8>>) -> Vec<u8> {
        let mut b = ie::Builder::new().ssid(ssid).rates(&ie::RATES_G).ds_channel(channel);
        let mut cap = capab::ESS;
        if let Some(r) = &rsne {
            b = b.raw(r);
            cap |= capab::PRIVACY;
        }
        let body = BeaconBody::build(1, 100, cap, &b.build());
        frame::management(mgmt::BEACON, &frame::BROADCAST, &[2, 0, 0, 0, 0, 9], &[2, 0, 0, 0, 0, 9], 1, &body)
    }

    #[test]
    fn beacons_become_bss_descriptions() {
        let rsne = ap_rsne(Security::Wpa3Personal, false).unwrap().element();
        let b = parse_bss(&beacon(b"VedaNet", 6, Some(rsne.clone())), 1, -50, 10).unwrap();
        assert_eq!(b.ssid, b"VedaNet");
        assert_eq!((b.channel, b.signal_dbm, b.security, b.pmf), (6, -50, Security::Wpa3Personal, 2));
        assert_eq!(b.rsne, Some(rsne));
        let open = parse_bss(&beacon(b"Cafe", 11, None), 11, -70, 0).unwrap();
        assert_eq!(open.security, Security::Open);
        let hidden = parse_bss(&beacon(b"", 1, None), 1, -60, 0).unwrap();
        assert!(hidden.hidden());
        // An unparsable RSN element is not mistaken for an open network.
        let weird = parse_bss(&beacon(b"Odd", 1, Some(alloc::vec![48, 2, 9, 9])), 1, -60, 0).unwrap();
        assert!(!weird.security.supported());
        assert!(parse_bss(&probe_request(&[2; 6], None, 1, 1), 1, -40, 0).is_none());
    }
}
