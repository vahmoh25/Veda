//! The cryptographic building blocks of IEEE 802.11 security, on top of
//! RustCrypto primitives: the passphrase-to-PSK mapping, the PRF and KDF
//! used to derive keys, EAPOL-Key MICs, AES key wrap and constant-time
//! comparison. CCMP and BIP live in [`crate::ccmp`].

use alloc::vec::Vec;

use aes::Aes128;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// A source of cryptographic randomness (the kernel's generator on
/// Veda; a seeded generator in tests).
pub trait Random {
    fn fill(&mut self, buf: &mut [u8]);
}

/// Compares two byte strings in constant time (for MICs and tags).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// HMAC-SHA1 over the concatenation of `parts`.
pub fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// HMAC-SHA256 over the concatenation of `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// AES-128-CMAC (RFC 4493) over the concatenation of `parts`.
pub fn aes_cmac(key: &[u8; 16], parts: &[&[u8]]) -> [u8; 16] {
    let mut mac = <cmac::Cmac<Aes128> as KeyInit>::new_from_slice(key).expect("16-byte key");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// The WPA passphrase mapping (IEEE 802.11-2020, J.4): PBKDF2-SHA1 with
/// the SSID as salt and 4096 iterations, 256 bits.
pub fn pbkdf2_psk(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let mut psk = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha1>(passphrase, ssid, 4096, &mut psk);
    psk
}

/// A PSK given directly as 64 hexadecimal digits.
pub fn psk_from_hex(text: &str) -> Option<[u8; 32]> {
    let b = text.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut psk = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        psk[i] = (hi * 16 + lo) as u8;
    }
    Some(psk)
}

/// The IEEE 802.11 PRF (12.7.1.2): `out` is filled with
/// `HMAC-SHA1(K, A || 0 || B || i)` for i = 0, 1, 2, ...
pub fn prf_sha1(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let block = hmac_sha1(key, &[label, &[0], data, &[i as u8]]);
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}

/// The IEEE 802.11 KDF (12.7.1.6.2) with SHA-256: `out` (whose length in
/// bits is the KDF length) is filled with
/// `HMAC-SHA256(K, i || Label || Context || Length)`, i = 1, 2, ...
pub fn kdf_sha256(key: &[u8], label: &[u8], context: &[u8], out: &mut [u8]) {
    let bits = (out.len() * 8) as u16;
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let counter = (i as u16 + 1).to_le_bytes();
        let block = hmac_sha256(key, &[&counter, label, context, &bits.to_le_bytes()]);
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}

/// AES key wrap (RFC 3394) of `data` (a multiple of 8 bytes, at least 16).
pub fn aes_wrap(kek: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 16 || !data.len().is_multiple_of(8) {
        return None;
    }
    let kw = aes_kw::KwAes128::new(kek.into());
    let mut out = alloc::vec![0u8; data.len() + 8];
    let n = kw.wrap_key(data, &mut out).ok()?.len();
    out.truncate(n);
    Some(out)
}

/// AES key unwrap (RFC 3394); `None` if the integrity check fails.
pub fn aes_unwrap(kek: &[u8; 16], wrapped: &[u8]) -> Option<Vec<u8>> {
    if wrapped.len() < 24 || !wrapped.len().is_multiple_of(8) {
        return None;
    }
    let kw = aes_kw::KwAes128::new(kek.into());
    let mut out = alloc::vec![0u8; wrapped.len() - 8];
    let n = kw.unwrap_key(wrapped, &mut out).ok()?.len();
    out.truncate(n);
    Some(out)
}

/// Which key derivation and integrity algorithms an AKM uses
/// (IEEE 802.11-2020, Table 12-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAlgo {
    /// AKM 00-0F-AC:2 (PSK): PRF-SHA1 key derivation, HMAC-SHA1-128 MIC
    /// (key descriptor version 2).
    Sha1,
    /// AKM 00-0F-AC:6 (PSK-SHA256) and :8 (SAE): KDF-SHA256 key
    /// derivation, AES-128-CMAC MIC (descriptor version 3 / AKM-defined).
    Sha256,
}

/// Pairwise transient key for CCMP-128.
#[derive(Clone)]
pub struct Ptk {
    /// Key confirmation key (EAPOL-Key MICs).
    pub kck: [u8; 16],
    /// Key encryption key (EAPOL-Key key data).
    pub kek: [u8; 16],
    /// Temporal key (CCMP).
    pub tk: [u8; 16],
}

impl Drop for Ptk {
    fn drop(&mut self) {
        self.kck.zeroize();
        self.kek.zeroize();
        self.tk.zeroize();
    }
}

impl core::fmt::Debug for Ptk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Ptk { .. }")
    }
}

/// Derives the PTK (12.7.1.3): "Pairwise key expansion" over
/// Min(AA,SPA) || Max(AA,SPA) || Min(ANonce,SNonce) || Max(ANonce,SNonce).
pub fn derive_ptk(algo: KeyAlgo, pmk: &[u8], aa: &[u8; 6], spa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> Ptk {
    let (a1, a2) = if aa < spa { (aa, spa) } else { (spa, aa) };
    let (n1, n2) = if anonce < snonce { (anonce, snonce) } else { (snonce, anonce) };
    let mut data = [0u8; 6 + 6 + 32 + 32];
    data[..6].copy_from_slice(a1);
    data[6..12].copy_from_slice(a2);
    data[12..44].copy_from_slice(n1);
    data[44..].copy_from_slice(n2);
    let mut out = [0u8; 48];
    match algo {
        KeyAlgo::Sha1 => prf_sha1(pmk, b"Pairwise key expansion", &data, &mut out),
        KeyAlgo::Sha256 => kdf_sha256(pmk, b"Pairwise key expansion", &data, &mut out),
    }
    let mut ptk = Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 16] };
    ptk.kck.copy_from_slice(&out[..16]);
    ptk.kek.copy_from_slice(&out[16..32]);
    ptk.tk.copy_from_slice(&out[32..48]);
    out.zeroize();
    ptk
}

/// The MIC of an EAPOL-Key frame (with its MIC field zeroed).
pub fn eapol_mic(algo: KeyAlgo, kck: &[u8; 16], frame: &[u8]) -> [u8; 16] {
    match algo {
        KeyAlgo::Sha1 => {
            let full = hmac_sha1(kck, &[frame]);
            let mut mic = [0u8; 16];
            mic.copy_from_slice(&full[..16]);
            mic
        }
        KeyAlgo::Sha256 => aes_cmac(kck, &[frame]),
    }
}

/// PMKID for a PSK/SAE PMK (12.7.1.3): the first 128 bits of
/// HMAC(PMK, "PMK Name" || AA || SPA).
pub fn pmkid(algo: KeyAlgo, pmk: &[u8], aa: &[u8; 6], spa: &[u8; 6]) -> [u8; 16] {
    let mut out = [0u8; 16];
    match algo {
        KeyAlgo::Sha1 => out.copy_from_slice(&hmac_sha1(pmk, &[b"PMK Name", aa, spa])[..16]),
        KeyAlgo::Sha256 => out.copy_from_slice(&hmac_sha256(pmk, &[b"PMK Name", aa, spa])[..16]),
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec::Vec;

    pub fn hex(s: &str) -> Vec<u8> {
        let s: alloc::string::String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// IEEE 802.11-2020, J.4 (via hostap's crypto module tests).
    #[test]
    fn passphrase_to_psk() {
        assert_eq!(
            pbkdf2_psk(b"password", b"IEEE").as_slice(),
            hex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e").as_slice()
        );
        assert_eq!(
            pbkdf2_psk(b"ThisIsAPassword", b"ThisIsASSID").as_slice(),
            hex("0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af").as_slice()
        );
        assert_eq!(
            pbkdf2_psk(&[b'a'; 32], &[b'Z'; 32]).as_slice(),
            hex("becb93866bb8c3832cb777c2f559807c8c59afcb6eae734885001300a981cc62").as_slice()
        );
        assert_eq!(psk_from_hex(&"0f".repeat(32)), Some([0x0f; 32]));
        assert_eq!(psk_from_hex("zz"), None);
    }

    /// IEEE 802.11-2020, J.3 PRF test vectors.
    #[test]
    fn prf() {
        let mut out = [0u8; 64];
        prf_sha1(&[0x0b; 20], b"prefix", b"Hi There", &mut out);
        assert_eq!(
            out.as_slice(),
            hex("bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a")
                .as_slice()
        );
        prf_sha1(b"Jefe", b"prefix", b"what do ya want for nothing?", &mut out);
        assert_eq!(
            out.as_slice(),
            hex("51f4de5b33f249adf81aeb713a3c20f4fe631446fabdfa58244759ae58ef9009a99abf4eac2ca5fa87e692c440eb40023e7babb206d61de7b92f41529092b8fc")
                .as_slice()
        );
        prf_sha1(&[0xaa; 20], b"prefix", &[0xdd; 50], &mut out);
        assert_eq!(
            out.as_slice(),
            hex("e1ac546ec4cb636f9976487be5c86be17a0252ca5d8d8df12cfb0473525249ce9dd8d177ead710bc9b590547239107aef7b4abd43d87f0a68f1cbd9e2b6f7607")
                .as_slice()
        );
    }

    /// RFC 3394, 4.1.
    #[test]
    fn key_wrap() {
        let kek: [u8; 16] = hex("000102030405060708090A0B0C0D0E0F").try_into().unwrap();
        let data = hex("00112233445566778899AABBCCDDEEFF");
        let wrapped = aes_wrap(&kek, &data).unwrap();
        assert_eq!(wrapped, hex("1FA68B0A8112B447AEF34BD8FB5A7B829D3E862371D2CFE5"));
        assert_eq!(aes_unwrap(&kek, &wrapped).unwrap(), data);
        let mut bad = wrapped.clone();
        bad[5] ^= 1;
        assert_eq!(aes_unwrap(&kek, &bad), None);
        assert_eq!(aes_unwrap(&kek, &wrapped[..16]), None);
        assert_eq!(aes_wrap(&kek, &data[..12]), None);
    }

    /// RFC 4493, example 2.
    #[test]
    fn cmac() {
        let key: [u8; 16] = hex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        assert_eq!(
            aes_cmac(&key, &[&hex("6bc1bee22e409f96e93d7e117393172a")]).as_slice(),
            hex("070a16b46b4d4144f79bdd9dd04a287c").as_slice()
        );
    }

    #[test]
    fn ptk_derivation_is_symmetric() {
        let pmk = [7u8; 32];
        let (a, b) = ([2u8, 0, 0, 0, 0, 1], [2u8, 0, 0, 0, 0, 2]);
        let (n1, n2) = ([1u8; 32], [9u8; 32]);
        for algo in [KeyAlgo::Sha1, KeyAlgo::Sha256] {
            let p = derive_ptk(algo, &pmk, &a, &b, &n1, &n2);
            let q = derive_ptk(algo, &pmk, &b, &a, &n2, &n1);
            assert_eq!((p.kck, p.kek, p.tk), (q.kck, q.kek, q.tk));
        }
        assert_ne!(
            derive_ptk(KeyAlgo::Sha1, &pmk, &a, &b, &n1, &n2).tk,
            derive_ptk(KeyAlgo::Sha256, &pmk, &a, &b, &n1, &n2).tk
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
