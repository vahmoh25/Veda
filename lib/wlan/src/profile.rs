//! Saved networks and their file format.
//!
//! The file is plain text, one `key=value` per line, one block per network:
//!
//! ```text
//! # Vindows saved Wi-Fi networks
//! version=1
//! network
//! ssid=56696e646f777320486f6d65
//! security=wpa2
//! passphrase=636f727265637420686f727365
//! auto=1
//! hidden=0
//! sae=0
//! last=1790000000
//! end
//! ```
//!
//! SSIDs and passphrases are stored as hexadecimal, so any bytes survive and
//! no escaping is needed. The parser accepts anything: unknown keys are
//! ignored, a malformed block is skipped, and the number and size of entries
//! are bounded, so a damaged file loses at most the damaged entries.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::rsn::Security;

/// Most networks remembered.
pub const MAX_PROFILES: usize = 64;
/// Longest file accepted.
pub const MAX_FILE: usize = 64 * 1024;

/// A remembered network.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    pub ssid: Vec<u8>,
    /// The security the network had when it was saved.
    pub security: Security,
    /// The passphrase (8-63 characters or 64 hex digits); `None` for open
    /// networks.
    pub passphrase: Option<String>,
    /// Join automatically when in range.
    pub auto_connect: bool,
    /// The network does not broadcast its SSID.
    pub hidden: bool,
    /// WPA3 (SAE) was used successfully: from then on the network is only
    /// joined with WPA3, so an access point offering only WPA2 under the
    /// same name is not trusted.
    pub sae_used: bool,
    /// Unix time of the last successful connection (0 = never).
    pub last_connected: u64,
}

impl core::fmt::Debug for Profile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Profile")
            .field("ssid", &String::from_utf8_lossy(&self.ssid))
            .field("security", &self.security)
            .field("passphrase", &self.passphrase.as_ref().map(|_| ".."))
            .field("auto_connect", &self.auto_connect)
            .field("hidden", &self.hidden)
            .field("sae_used", &self.sae_used)
            .field("last_connected", &self.last_connected)
            .finish()
    }
}

/// Whether `p` is acceptable as a passphrase: 8 to 63 printable ASCII
/// characters, or exactly 64 hexadecimal digits (the PSK itself).
pub fn valid_passphrase(p: &str) -> bool {
    let printable = p.bytes().all(|b| (0x20..=0x7E).contains(&b));
    (printable && (8..=63).contains(&p.len())) || (p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Whether `ssid` is a valid SSID (1 to 32 bytes).
pub fn valid_ssid(ssid: &[u8]) -> bool {
    (1..=32).contains(&ssid.len())
}

fn security_name(s: Security) -> &'static str {
    match s {
        Security::Open => "open",
        Security::Wep => "wep",
        Security::WpaTkip => "wpa",
        Security::Wpa2Personal => "wpa2",
        Security::Wpa3Personal => "wpa3",
        Security::Wpa2Wpa3Personal => "wpa2-wpa3",
        Security::Enterprise => "enterprise",
        Security::Owe => "owe",
    }
}

fn parse_security(s: &str) -> Option<Security> {
    Some(match s {
        "open" => Security::Open,
        "wpa2" => Security::Wpa2Personal,
        "wpa3" => Security::Wpa3Personal,
        "wpa2-wpa3" => Security::Wpa2Wpa3Personal,
        // Networks we cannot join are never saved.
        _ => return None,
    })
}

fn hex(out: &mut String, bytes: &[u8]) {
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Writes the file for `profiles`.
pub fn serialize(profiles: &[Profile]) -> String {
    let mut s = String::from("# Vindows saved Wi-Fi networks\nversion=1\n");
    for p in profiles.iter().take(MAX_PROFILES) {
        s.push_str("network\nssid=");
        hex(&mut s, &p.ssid);
        let _ = write!(s, "\nsecurity={}\n", security_name(p.security));
        if let Some(pass) = &p.passphrase {
            s.push_str("passphrase=");
            hex(&mut s, pass.as_bytes());
            s.push('\n');
        }
        let _ = write!(
            s,
            "auto={}\nhidden={}\nsae={}\nlast={}\nend\n",
            p.auto_connect as u8, p.hidden as u8, p.sae_used as u8, p.last_connected
        );
    }
    s
}

/// Reads the file, keeping every well-formed network (the first of any
/// duplicates).
pub fn parse(data: &[u8]) -> Vec<Profile> {
    let mut out: Vec<Profile> = Vec::new();
    let text = String::from_utf8_lossy(&data[..data.len().min(MAX_FILE)]);
    let mut cur: Option<Draft> = None;
    for line in text.lines() {
        let line = line.trim();
        match line {
            "network" => cur = Some(Draft::default()),
            "end" => {
                if let Some(p) = cur.take().and_then(Draft::finish)
                    && out.len() < MAX_PROFILES
                    && !out.iter().any(|q| q.ssid == p.ssid)
                {
                    out.push(p);
                }
            }
            _ => {
                if let (Some(d), Some((k, v))) = (cur.as_mut(), line.split_once('=')) {
                    d.set(k.trim(), v.trim());
                }
            }
        }
    }
    out
}

#[derive(Default)]
struct Draft {
    ssid: Option<Vec<u8>>,
    security: Option<Security>,
    passphrase: Option<String>,
    bad: bool,
    auto_connect: bool,
    hidden: bool,
    sae_used: bool,
    last_connected: u64,
}

impl Draft {
    fn set(&mut self, k: &str, v: &str) {
        let flag = |v: &str| v == "1";
        match k {
            "ssid" => self.ssid = unhex(v).filter(|s| valid_ssid(s)),
            "security" => self.security = parse_security(v),
            "passphrase" => match unhex(v).and_then(|b| String::from_utf8(b).ok()) {
                Some(p) if valid_passphrase(&p) => self.passphrase = Some(p),
                _ => self.bad = true,
            },
            "auto" => self.auto_connect = flag(v),
            "hidden" => self.hidden = flag(v),
            "sae" => self.sae_used = flag(v),
            "last" => self.last_connected = v.parse().unwrap_or(0),
            _ => {}
        }
    }

    fn finish(self) -> Option<Profile> {
        let security = self.security?;
        let needs_password = security != Security::Open;
        if self.bad || needs_password != self.passphrase.is_some() {
            return None;
        }
        Some(Profile {
            ssid: self.ssid?,
            security,
            passphrase: self.passphrase,
            auto_connect: self.auto_connect,
            hidden: self.hidden,
            sae_used: self.sae_used && security != Security::Open,
            last_connected: self.last_connected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample() -> Vec<Profile> {
        vec![
            Profile {
                ssid: b"Vindows Home".to_vec(),
                security: Security::Wpa2Personal,
                passphrase: Some("correct horse battery".into()),
                auto_connect: true,
                hidden: false,
                sae_used: false,
                last_connected: 1_790_000_000,
            },
            Profile {
                ssid: vec![0xFF, 0x00, b'=', b'\n'],
                security: Security::Wpa3Personal,
                passphrase: Some("=\n# not a comment".replace('\n', " ")),
                auto_connect: false,
                hidden: true,
                sae_used: true,
                last_connected: 0,
            },
            Profile {
                ssid: b"Cafe".to_vec(),
                security: Security::Open,
                passphrase: None,
                auto_connect: true,
                hidden: false,
                sae_used: false,
                last_connected: 5,
            },
        ]
    }

    #[test]
    fn profiles_round_trip() {
        let p = sample();
        let text = serialize(&p);
        assert_eq!(parse(text.as_bytes()), p);
        assert!(!text.contains("correct horse"), "passphrases are not stored as plain text lines");
    }

    #[test]
    fn damaged_entries_are_skipped() {
        let mut text = serialize(&sample());
        // A block without an end, a block with a bad SSID, one with a
        // passphrase that is too short, one secured without a passphrase,
        // an unsupported security, a duplicate, and garbage.
        text.push_str("network\nssid=zz\nsecurity=wpa2\npassphrase=3132333435363738\nend\n");
        text.push_str("network\nssid=41\nsecurity=wpa2\npassphrase=31\nend\n");
        text.push_str("network\nssid=42\nsecurity=wpa2\nend\n");
        text.push_str("network\nssid=43\nsecurity=wep\npassphrase=3132333435363738\nend\n");
        text.push_str("network\nssid=4361666500\nsecurity=open\nend\n");
        text.push_str(&serialize(&sample()[..1]));
        text.push_str("\u{0}\u{1}garbage=\nnetwork\nssid=44\n");
        let got = parse(text.as_bytes());
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[3].ssid, b"Cafe\0");
        assert_eq!(&got[..3], &sample()[..]);
    }

    #[test]
    fn limits_are_enforced() {
        let many: Vec<Profile> = (0..100u32)
            .map(|i| Profile { ssid: alloc::format!("net{i}").into_bytes(), ..sample()[2].clone() })
            .collect();
        assert_eq!(parse(serialize(&many).as_bytes()).len(), MAX_PROFILES);
        // An oversized file is read only up to the limit.
        let mut big = serialize(&sample());
        big.push_str(&"#".repeat(MAX_FILE));
        assert_eq!(parse(big.as_bytes()).len(), 3);
        // Arbitrary bytes never panic.
        let mut s = 0x1234_5678u32;
        for _ in 0..500 {
            let mut buf = Vec::new();
            for _ in 0..(s % 400) {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                buf.push(b"network\nendssid=0123456789abcdef\n"[(s % 33) as usize]);
            }
            let _ = parse(&buf);
        }
    }

    #[test]
    fn passphrase_rules() {
        assert!(valid_passphrase("12345678"));
        assert!(valid_passphrase(&"a".repeat(63)));
        assert!(!valid_passphrase(&"g".repeat(64)));
        assert!(valid_passphrase(&"aB3f".repeat(16)));
        assert!(!valid_passphrase("1234567"));
        assert!(!valid_passphrase("tab\tinside"));
        assert!(!valid_passphrase("caf\u{e9} au lait"));
        assert!(valid_ssid(b"x") && valid_ssid(&[0; 32]));
        assert!(!valid_ssid(b"") && !valid_ssid(&[0; 33]));
    }
}
