//! Decisions of the Wi-Fi service: which access point to join, whether an
//! access point may be used for a saved network, when to try again after a
//! failure, and when to move to a better access point.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::profile::Profile;
use crate::rsn::Security;
use crate::scan::Bss;

/// Access points weaker than this are not joined automatically.
pub const MIN_JOIN_DBM: i8 = -85;
/// Below this signal level the service looks for a better access point...
pub const ROAM_BELOW_DBM: i8 = -72;
/// ...and moves if one is at least this much stronger.
pub const ROAM_MARGIN_DB: i8 = 10;
/// Retry delays after failures to join automatically (milliseconds).
const BACKOFF_MS: [u64; 6] = [2_000, 4_000, 8_000, 16_000, 32_000, 60_000];

/// Whether an access point offering `offered` may be used for a network
/// saved with `saved` security.
///
/// A secured network is never joined through an open access point, a
/// network saved as WPA3 (or one where WPA3 was used) never through an
/// access point offering only WPA2, and unsupported security never.
pub fn security_compatible(saved: Security, sae_used: bool, offered: Security) -> bool {
    if !offered.supported() {
        return false;
    }
    match saved {
        Security::Open => offered == Security::Open,
        Security::Wpa2Personal if !sae_used => {
            matches!(offered, Security::Wpa2Personal | Security::Wpa2Wpa3Personal)
        }
        Security::Wpa2Personal | Security::Wpa3Personal | Security::Wpa2Wpa3Personal if sae_used => {
            matches!(offered, Security::Wpa3Personal | Security::Wpa2Wpa3Personal)
        }
        Security::Wpa3Personal => matches!(offered, Security::Wpa3Personal | Security::Wpa2Wpa3Personal),
        Security::Wpa2Wpa3Personal => {
            matches!(offered, Security::Wpa2Personal | Security::Wpa3Personal | Security::Wpa2Wpa3Personal)
        }
        _ => false,
    }
}

/// Whether `bss` belongs to the saved network `p` (by name, or for a hidden
/// network an access point that hides its name and has compatible
/// security).
pub fn matches(p: &Profile, bss: &Bss) -> bool {
    let named = bss.ssid == p.ssid || (p.hidden && bss.hidden());
    named && security_compatible(p.security, p.sae_used, bss.security)
}

/// How attractive an access point is (higher is better).
pub fn score(bss: &Bss) -> i32 {
    let mut s = bss.signal_dbm as i32;
    // 5 GHz is usually less crowded.
    if bss.channel > 14 {
        s += 5;
    }
    s
}

/// The best access point for the saved network `p` among `seen`.
pub fn best_for<'a>(p: &Profile, seen: &'a [Bss]) -> Option<&'a Bss> {
    seen.iter().filter(|b| matches(p, b) && b.signal_dbm >= MIN_JOIN_DBM).max_by_key(|b| score(b))
}

/// Failed attempts and the earliest next attempt, per network.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backoff {
    pub failures: u32,
    pub next_ms: u64,
}

impl Backoff {
    /// Records a failure at `now`.
    pub fn failed(&mut self, now: u64) {
        let delay = BACKOFF_MS[(self.failures as usize).min(BACKOFF_MS.len() - 1)];
        self.failures = self.failures.saturating_add(1);
        self.next_ms = now + delay;
    }

    pub fn ready(&self, now: u64) -> bool {
        now >= self.next_ms
    }
}

/// Picks the saved network and access point to join automatically: among
/// the auto-connect networks in range whose retry delay has passed, the
/// most recently used one, then the one with the best access point.
/// Returns the index into `profiles`.
pub fn choose<'a>(
    profiles: &[Profile],
    seen: &'a [Bss],
    backoff: &BTreeMap<Vec<u8>, Backoff>,
    now: u64,
) -> Option<(usize, &'a Bss)> {
    profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| p.auto_connect)
        .filter(|(_, p)| backoff.get(&p.ssid).is_none_or(|b| b.ready(now)))
        .filter_map(|(i, p)| best_for(p, seen).map(|b| (i, b)))
        .max_by_key(|(i, b)| (profiles[*i].last_connected, score(b)))
}

/// Whether to move from the current access point (heard at
/// `current_dbm`) to another one of the same network, and which.
pub fn roam_target<'a>(p: &Profile, current: &[u8; 6], current_dbm: i8, seen: &'a [Bss]) -> Option<&'a Bss> {
    if current_dbm >= ROAM_BELOW_DBM {
        return None;
    }
    seen.iter()
        .filter(|b| b.bssid != *current && matches(p, b))
        .filter(|b| b.signal_dbm as i16 >= current_dbm as i16 + ROAM_MARGIN_DB as i16)
        .max_by_key(|b| score(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;

    fn bss(n: u8, ssid: &str, security: Security, channel: u8, signal_dbm: i8) -> Bss {
        Bss {
            bssid: [2, 0, 0, 0, 0, n],
            ssid: ssid.as_bytes().to_vec(),
            channel,
            signal_dbm,
            beacon_interval: 100,
            capability: 0,
            rates: vec![],
            rsne: None,
            rsnx: None,
            security,
            pmf: 0,
            h2e: false,
            seen_ms: 0,
        }
    }

    fn profile(ssid: &str, security: Security) -> Profile {
        Profile {
            ssid: ssid.as_bytes().to_vec(),
            security,
            passphrase: (security != Security::Open).then(|| String::from("password123")),
            auto_connect: true,
            hidden: false,
            sae_used: false,
            last_connected: 0,
        }
    }

    #[test]
    fn security_downgrades_are_refused() {
        use Security::*;
        // Open profiles only match open networks and vice versa.
        assert!(security_compatible(Open, false, Open));
        assert!(!security_compatible(Open, false, Wpa2Personal));
        for saved in [Wpa2Personal, Wpa3Personal, Wpa2Wpa3Personal] {
            assert!(!security_compatible(saved, false, Open), "{saved:?} via open");
            assert!(!security_compatible(saved, false, Wep));
            assert!(!security_compatible(saved, false, WpaTkip));
            assert!(!security_compatible(saved, false, Enterprise));
        }
        // WPA2 profiles accept transition mode; WPA3 profiles refuse WPA2.
        assert!(security_compatible(Wpa2Personal, false, Wpa2Personal));
        assert!(security_compatible(Wpa2Personal, false, Wpa2Wpa3Personal));
        assert!(!security_compatible(Wpa2Personal, false, Wpa3Personal));
        assert!(security_compatible(Wpa3Personal, false, Wpa3Personal));
        assert!(security_compatible(Wpa3Personal, false, Wpa2Wpa3Personal));
        assert!(!security_compatible(Wpa3Personal, false, Wpa2Personal));
        // Once WPA3 has been used, WPA2-only access points are refused.
        assert!(security_compatible(Wpa2Wpa3Personal, false, Wpa2Personal));
        assert!(!security_compatible(Wpa2Wpa3Personal, true, Wpa2Personal));
        assert!(!security_compatible(Wpa2Personal, true, Wpa2Personal));
        assert!(security_compatible(Wpa2Personal, true, Wpa2Wpa3Personal));
    }

    #[test]
    fn the_strongest_compatible_access_point_is_chosen() {
        let p = profile("Home", Security::Wpa2Personal);
        let seen = vec![
            bss(1, "Home", Security::Wpa2Personal, 1, -70),
            bss(2, "Home", Security::Wpa2Personal, 36, -64),
            bss(3, "Home", Security::Open, 6, -30),
            bss(4, "Other", Security::Wpa2Personal, 11, -20),
            bss(5, "Home", Security::Wpa2Personal, 11, -90),
        ];
        assert_eq!(best_for(&p, &seen).unwrap().bssid[5], 2);
        // 5 GHz gets a small bonus: -66 at 5 GHz beats -63 at 2.4 GHz.
        let seen2 =
            vec![bss(1, "Home", Security::Wpa2Personal, 1, -63), bss(2, "Home", Security::Wpa2Personal, 36, -66)];
        assert_eq!(best_for(&p, &seen2).unwrap().bssid[5], 2);
        // Too weak.
        assert!(best_for(&p, &seen[4..]).is_none());
    }

    #[test]
    fn hidden_networks_match_hidden_access_points() {
        let mut p = profile("Secret", Security::Wpa2Personal);
        let seen = vec![bss(1, "", Security::Wpa2Personal, 6, -60), bss(2, "", Security::Open, 1, -40)];
        assert!(best_for(&p, &seen).is_none());
        p.hidden = true;
        assert_eq!(best_for(&p, &seen).unwrap().bssid[5], 1);
    }

    #[test]
    fn choice_respects_auto_connect_backoff_and_recency() {
        let mut a = profile("A", Security::Wpa2Personal);
        let mut b = profile("B", Security::Open);
        let seen = vec![bss(1, "A", Security::Wpa2Personal, 1, -70), bss(2, "B", Security::Open, 6, -50)];
        let mut backoff = BTreeMap::new();
        // Equal recency: the better access point wins.
        assert_eq!(choose(&[a.clone(), b.clone()], &seen, &backoff, 0).unwrap().0, 1);
        // The more recently used network wins.
        a.last_connected = 100;
        assert_eq!(choose(&[a.clone(), b.clone()], &seen, &backoff, 0).unwrap().0, 0);
        // A network that just failed waits for its retry time.
        let mut bo = Backoff::default();
        bo.failed(1000);
        backoff.insert(a.ssid.clone(), bo);
        assert_eq!(choose(&[a.clone(), b.clone()], &seen, &backoff, 1500).unwrap().0, 1);
        assert_eq!(choose(&[a.clone(), b.clone()], &seen, &backoff, 3000).unwrap().0, 0);
        // Networks without auto-connect are never chosen.
        b.auto_connect = false;
        assert_eq!(choose(&[b.clone()], &seen, &BTreeMap::new(), 0), None);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let mut b = Backoff::default();
        let mut delays = vec![];
        for _ in 0..10 {
            b.failed(0);
            delays.push(b.next_ms);
        }
        assert_eq!(&delays[..6], &[2_000, 4_000, 8_000, 16_000, 32_000, 60_000]);
        assert!(delays[6..].iter().all(|&d| d == 60_000));
        assert!(!b.ready(59_999) && b.ready(60_000));
    }

    #[test]
    fn roaming_needs_a_weak_signal_and_a_clearly_better_access_point() {
        let p = profile("Home", Security::Wpa2Personal);
        let me = [2, 0, 0, 0, 0, 1];
        let seen = vec![
            bss(1, "Home", Security::Wpa2Personal, 1, -80),
            bss(2, "Home", Security::Wpa2Personal, 11, -71),
            bss(3, "Home", Security::Wpa2Personal, 6, -65),
            bss(4, "Home", Security::Open, 36, -40),
        ];
        assert!(roam_target(&p, &me, -60, &seen).is_none(), "signal still good");
        assert_eq!(roam_target(&p, &me, -80, &seen).unwrap().bssid[5], 3);
        assert!(roam_target(&p, &me, -74, &seen[..2]).is_none(), "not 10 dB better");
    }
}
