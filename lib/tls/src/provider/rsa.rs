//! RSA signature verification (RFC 8017): RSASSA-PKCS1-v1_5 and RSASSA-PSS.
//!
//! Verification only involves public values (the key, the message and the
//! signature), so the arithmetic need not be constant-time; it uses
//! `crypto-bigint`'s heap-allocated integers. The rules are those of *ring*,
//! which webpki and rustls normally use:
//!
//! * the modulus has 2048 to 8192 bits and is odd; the public exponent is
//!   odd and between 3 and 2^33 - 1;
//! * the signature is exactly as long as the modulus and smaller than it;
//! * PKCS#1 v1.5 encodings are compared in full (DigestInfo with explicit
//!   NULL parameters);
//! * PSS uses MGF1 with the message hash and a salt as long as the hash,
//!   as TLS 1.3 requires (RFC 8446 section 4.2.3).

use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ops::RangeInclusive;

use crypto_bigint::modular::{BoxedMontyForm, BoxedMontyParams};
use crypto_bigint::{BoxedUint, Odd};
use subtle::ConstantTimeEq;

use super::sign_verify::MessageHash;

/// Modulus sizes accepted for signatures (bits).
pub(crate) const MODULUS_BITS: RangeInclusive<u32> = 2048..=8192;
/// The public exponent must be below 2^33.
const MAX_EXPONENT_BITS: u32 = 33;

/// An RSA signature scheme's padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Padding {
    /// EMSA-PKCS1-v1_5.
    Pkcs1,
    /// EMSA-PSS with MGF1 and a salt as long as the hash.
    Pss,
}

/// Whether `signature` is a valid signature of `message` by the RSA key
/// `public_key` (a DER RSAPublicKey).
pub(crate) fn verify(public_key: &[u8], message: &[u8], signature: &[u8], padding: Padding, hash: MessageHash) -> bool {
    verify_with_sizes(public_key, message, signature, padding, hash, MODULUS_BITS)
}

fn verify_with_sizes(
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
    padding: Padding,
    hash: MessageHash,
    sizes: RangeInclusive<u32>,
) -> bool {
    let Some(key) = PublicKey::from_der(public_key, sizes) else { return false };
    let Some(encoded) = key.open(signature) else { return false };
    let digest = hash.digest(&[message]);
    match padding {
        Padding::Pkcs1 => pkcs1_matches(&encoded, hash, digest.as_ref()),
        Padding::Pss => pss_matches(&encoded, key.bits - 1, hash, digest.as_ref()),
    }
}

/// An RSA public key.
struct PublicKey {
    n: Odd<BoxedUint>,
    e: BoxedUint,
    e_bits: u32,
    /// The modulus length in bytes (`k` in RFC 8017).
    len: usize,
    /// The modulus length in bits.
    bits: u32,
}

impl PublicKey {
    /// Parses an RSAPublicKey (RFC 8017 appendix A.1.1) with a modulus of
    /// `sizes` bits.
    fn from_der(der: &[u8], sizes: RangeInclusive<u32>) -> Option<PublicKey> {
        let (key, rest) = der_element(der, TAG_SEQUENCE)?;
        let (n, key) = der_element(key, TAG_INTEGER)?;
        let (e, key) = der_element(key, TAG_INTEGER)?;
        if !rest.is_empty() || !key.is_empty() {
            return None;
        }
        let (n, e) = (positive_integer(n)?, positive_integer(e)?);

        // The first byte of a positive integer's magnitude is not zero.
        let bits = u32::try_from(n.len()).ok()?.checked_mul(8)? - n[0].leading_zeros();
        if !sizes.contains(&bits) || e.len() > 8 {
            return None;
        }
        let e = e.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let e_bits = 64 - e.leading_zeros();
        if e < 3 || e & 1 == 0 || e_bits > MAX_EXPONENT_BITS {
            return None;
        }
        let modulus = BoxedUint::from_be_slice(n, bits).ok()?;
        Some(PublicKey { n: Odd::new(modulus).into_option()?, e: BoxedUint::from(e), e_bits, len: n.len(), bits })
    }

    /// RSAVP1 and I2OSP: the encoded message `signature^e mod n`, as many
    /// bytes long as the modulus, or `None` if `signature` is not a
    /// signature representative.
    fn open(&self, signature: &[u8]) -> Option<Vec<u8>> {
        if signature.len() != self.len {
            return None;
        }
        let s = BoxedUint::from_be_slice(signature, self.n.bits_precision()).ok()?;
        if s.cmp(self.n.as_ref()) != Ordering::Less {
            return None;
        }
        let params = BoxedMontyParams::new_vartime(self.n.clone());
        let m = BoxedMontyForm::new(s, &params).pow_bounded_exp(&self.e, self.e_bits).retrieve();
        // `m` < n has at most `len` significant bytes.
        let bytes = m.to_be_bytes();
        Some(bytes.get(bytes.len().checked_sub(self.len)?..)?.to_vec())
    }
}

/// EMSA-PKCS1-v1_5 (RFC 8017 section 9.2): whether `encoded` is
/// `00 01 FF..FF 00 DigestInfo(digest)`.
fn pkcs1_matches(encoded: &[u8], hash: MessageHash, digest: &[u8]) -> bool {
    let prefix = hash.digest_info_prefix();
    let t_len = prefix.len() + digest.len();
    // At least 8 bytes of padding.
    if encoded.len() < t_len + 11 {
        return false;
    }
    let mut expected = Vec::with_capacity(encoded.len());
    expected.extend_from_slice(&[0x00, 0x01]);
    expected.resize(encoded.len() - t_len - 1, 0xff);
    expected.push(0x00);
    expected.extend_from_slice(prefix);
    expected.extend_from_slice(digest);
    expected.ct_eq(encoded).into()
}

/// EMSA-PSS-VERIFY (RFC 8017 section 9.1.2) with MGF1 over `hash` and a salt
/// as long as the hash. `encoded` is the modulus-length octet string; the
/// encoded message is its last `ceil(em_bits / 8)` bytes.
fn pss_matches(encoded: &[u8], em_bits: u32, hash: MessageHash, m_hash: &[u8]) -> bool {
    let em_len = em_bits.div_ceil(8) as usize;
    let Some(extra) = encoded.len().checked_sub(em_len) else { return false };
    let (zeros, em) = encoded.split_at(extra);
    if zeros.iter().any(|&b| b != 0) {
        return false;
    }
    let h_len = hash.len();
    let s_len = h_len;
    if em_len < h_len + s_len + 2 || em[em_len - 1] != 0xbc {
        return false;
    }
    let (masked_db, rest) = em.split_at(em_len - h_len - 1);
    let h = &rest[..h_len];
    // The bits of the first byte above `em_bits` must be zero.
    let top_mask = 0xffu8 >> (8 * em_len - em_bits as usize);
    if masked_db[0] & !top_mask != 0 {
        return false;
    }
    let mut db = masked_db.to_vec();
    mgf1_xor(hash, h, &mut db);
    db[0] &= top_mask;
    // DB = PS (zeros) || 0x01 || salt.
    let ps_len = em_len - h_len - s_len - 2;
    if db[..ps_len].iter().any(|&b| b != 0) || db[ps_len] != 0x01 {
        return false;
    }
    let salt = &db[ps_len + 1..];
    let expected = hash.digest(&[&[0u8; 8], m_hash, salt]);
    expected.as_ref().ct_eq(h).into()
}

/// XORs `out` with MGF1(`seed`) (RFC 8017 appendix B.2.1).
fn mgf1_xor(hash: MessageHash, seed: &[u8], out: &mut [u8]) {
    for (counter, chunk) in out.chunks_mut(hash.len()).enumerate() {
        let mask = hash.digest(&[seed, &(counter as u32).to_be_bytes()]);
        for (o, m) in chunk.iter_mut().zip(mask.as_ref()) {
            *o ^= m;
        }
    }
}

const TAG_INTEGER: u8 = 0x02;
const TAG_SEQUENCE: u8 = 0x30;

/// Splits a DER element with tag `tag` off the front of `input`: its
/// contents and what follows. Lengths must be minimally encoded and at
/// most 65535.
fn der_element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&t, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    if t != tag {
        return None;
    }
    let (len, rest) = match first {
        0x00..=0x7f => (usize::from(first), rest),
        0x81 => {
            let (&len, rest) = rest.split_first()?;
            (len >= 0x80).then_some((usize::from(len), rest))?
        }
        0x82 => {
            let (len, rest) = rest.split_first_chunk::<2>()?;
            let len = usize::from(u16::from_be_bytes(*len));
            (len >= 0x100).then_some((len, rest))?
        }
        _ => return None,
    };
    (len <= rest.len()).then(|| rest.split_at(len))
}

/// The magnitude of a DER INTEGER that must be positive (minimal encoding,
/// no sign bit, not zero).
fn positive_integer(contents: &[u8]) -> Option<&[u8]> {
    match contents {
        [first, ..] if first & 0x80 != 0 => None,
        [0, second, ..] if second & 0x80 != 0 => Some(&contents[1..]),
        [0, ..] => None,
        [_, ..] => Some(contents),
        [] => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::hex;

    fn data(name: &str) -> &'static [u8] {
        match name {
            "message" => include_bytes!("../../testdata/rsa/message.bin"),
            "rsa2048" => include_bytes!("../../testdata/rsa/rsa2048.pub.der"),
            "rsa2057" => include_bytes!("../../testdata/rsa/rsa2057.pub.der"),
            "rsa3072" => include_bytes!("../../testdata/rsa/rsa3072.pub.der"),
            "rsa4096" => include_bytes!("../../testdata/rsa/rsa4096.pub.der"),
            "rsa2048-pkcs1-sha256" => include_bytes!("../../testdata/rsa/rsa2048-pkcs1-sha256.sig"),
            "rsa2048-pkcs1-sha384" => include_bytes!("../../testdata/rsa/rsa2048-pkcs1-sha384.sig"),
            "rsa2048-pkcs1-sha512" => include_bytes!("../../testdata/rsa/rsa2048-pkcs1-sha512.sig"),
            "rsa2048-pss-sha256" => include_bytes!("../../testdata/rsa/rsa2048-pss-sha256.sig"),
            "rsa2048-pss-sha384" => include_bytes!("../../testdata/rsa/rsa2048-pss-sha384.sig"),
            "rsa2048-pss-sha512" => include_bytes!("../../testdata/rsa/rsa2048-pss-sha512.sig"),
            "rsa2048-pss-sha256-salt20" => include_bytes!("../../testdata/rsa/rsa2048-pss-sha256-salt20.sig"),
            "rsa2057-pkcs1-sha256" => include_bytes!("../../testdata/rsa/rsa2057-pkcs1-sha256.sig"),
            "rsa2057-pkcs1-sha384" => include_bytes!("../../testdata/rsa/rsa2057-pkcs1-sha384.sig"),
            "rsa2057-pkcs1-sha512" => include_bytes!("../../testdata/rsa/rsa2057-pkcs1-sha512.sig"),
            "rsa2057-pss-sha256" => include_bytes!("../../testdata/rsa/rsa2057-pss-sha256.sig"),
            "rsa2057-pss-sha384" => include_bytes!("../../testdata/rsa/rsa2057-pss-sha384.sig"),
            "rsa2057-pss-sha512" => include_bytes!("../../testdata/rsa/rsa2057-pss-sha512.sig"),
            "rsa3072-pkcs1-sha384" => include_bytes!("../../testdata/rsa/rsa3072-pkcs1-sha384.sig"),
            "rsa3072-pss-sha384" => include_bytes!("../../testdata/rsa/rsa3072-pss-sha384.sig"),
            "rsa4096-pkcs1-sha512" => include_bytes!("../../testdata/rsa/rsa4096-pkcs1-sha512.sig"),
            "rsa4096-pss-sha512" => include_bytes!("../../testdata/rsa/rsa4096-pss-sha512.sig"),
            _ => panic!("no test data {name}"),
        }
    }

    const HASHES: [(MessageHash, &str); 3] =
        [(MessageHash::Sha256, "sha256"), (MessageHash::Sha384, "sha384"), (MessageHash::Sha512, "sha512")];
    const PADDINGS: [(Padding, &str); 2] = [(Padding::Pkcs1, "pkcs1"), (Padding::Pss, "pss")];

    /// Signatures made by OpenSSL verify, and only with the right hash and
    /// padding.
    #[test]
    fn openssl_signatures() {
        let message = data("message");
        let mut checked = 0;
        for key in ["rsa2048", "rsa2057"] {
            for (hash, hash_name) in HASHES {
                for (padding, padding_name) in PADDINGS {
                    let sig = data(&alloc::format!("{key}-{padding_name}-{hash_name}"));
                    assert!(verify(data(key), message, sig, padding, hash), "{key} {padding_name} {hash_name}");
                    for (other_hash, _) in HASHES.iter().filter(|(h, _)| *h != hash) {
                        assert!(!verify(data(key), message, sig, padding, *other_hash));
                    }
                    let other_padding = if padding == Padding::Pss { Padding::Pkcs1 } else { Padding::Pss };
                    assert!(!verify(data(key), message, sig, other_padding, hash));
                    assert!(!verify(data(key), b"another message", sig, padding, hash));
                    checked += 1;
                }
            }
        }
        for (key, hash, hash_name) in
            [("rsa3072", MessageHash::Sha384, "sha384"), ("rsa4096", MessageHash::Sha512, "sha512")]
        {
            for (padding, padding_name) in PADDINGS {
                let sig = data(&alloc::format!("{key}-{padding_name}-{hash_name}"));
                assert!(verify(data(key), message, sig, padding, hash), "{key} {padding_name}");
                assert!(!verify(data("rsa2048"), message, sig, padding, hash));
                checked += 1;
            }
        }
        assert_eq!(checked, 16);
    }

    #[test]
    fn rejects_damaged_signatures() {
        let (key, message) = (data("rsa2048"), data("message"));
        for (name, padding) in [("rsa2048-pkcs1-sha256", Padding::Pkcs1), ("rsa2048-pss-sha256", Padding::Pss)] {
            let sig = data(name);
            assert!(verify(key, message, sig, padding, MessageHash::Sha256));
            for i in [0, 1, 100, 255] {
                let mut bad = sig.to_vec();
                bad[i] ^= 0x10;
                assert!(!verify(key, message, &bad, padding, MessageHash::Sha256));
            }
            // Too short (a leading byte dropped), too long, empty.
            assert!(!verify(key, message, &sig[1..], padding, MessageHash::Sha256));
            assert!(!verify(key, message, &[sig, &[0]].concat(), padding, MessageHash::Sha256));
            assert!(!verify(key, message, &[], padding, MessageHash::Sha256));
        }
        // A signature representative not below the modulus: the modulus
        // itself, and all ones.
        let modulus = &key[9..9 + 256];
        assert_eq!(modulus.len(), 256);
        assert!(!verify(key, message, modulus, Padding::Pkcs1, MessageHash::Sha256));
        assert!(!verify(key, message, &[0xff; 256], Padding::Pkcs1, MessageHash::Sha256));
        // PSS with a shorter salt than the hash is valid PSS, but not for TLS.
        assert!(!verify(key, message, data("rsa2048-pss-sha256-salt20"), Padding::Pss, MessageHash::Sha256));
    }

    #[test]
    fn key_rules() {
        let key = data("rsa2048");
        assert!(PublicKey::from_der(key, MODULUS_BITS).is_some());
        // Trailing data, truncation, and a wrong outer tag.
        assert!(PublicKey::from_der(&[key, &[0]].concat(), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&key[..key.len() - 1], MODULUS_BITS).is_none());
        let mut retagged = key.to_vec();
        retagged[0] = 0x31;
        assert!(PublicKey::from_der(&retagged, MODULUS_BITS).is_none());
        // Every truncation is rejected without a panic.
        for len in 0..key.len() {
            assert!(PublicKey::from_der(&key[..len], MODULUS_BITS).is_none());
        }

        fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
            let mut out = alloc::vec![tag];
            match contents.len() {
                len @ 0..0x80 => out.push(len as u8),
                len @ 0x80..0x100 => out.extend_from_slice(&[0x81, len as u8]),
                len => out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]),
            }
            out.extend_from_slice(contents);
            out
        }
        // An RSAPublicKey with the INTEGER contents given as they are.
        fn rsa_key_raw(n: &[u8], e: &[u8]) -> Vec<u8> {
            tlv(TAG_SEQUENCE, &[tlv(TAG_INTEGER, n), tlv(TAG_INTEGER, e)].concat())
        }
        // An RSAPublicKey from big-endian magnitudes (sign bytes added).
        fn rsa_key(n: &[u8], e: &[u8]) -> Vec<u8> {
            let signed = |v: &[u8]| {
                let mut contents = Vec::new();
                if v.first().is_some_and(|b| b & 0x80 != 0) {
                    contents.push(0);
                }
                contents.extend_from_slice(v);
                contents
            };
            rsa_key_raw(&signed(n), &signed(e))
        }
        let n = &key[9..9 + 256];
        assert_eq!(rsa_key(n, &[1, 0, 1]), key);
        // Exponents: even, 1, too large.
        assert!(PublicKey::from_der(&rsa_key(n, &[1, 0, 0]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key(n, &[1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key(n, &[2, 0, 0, 0, 1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key(n, &[1, 0, 0, 0, 1]), MODULUS_BITS).is_some());
        // An even modulus; moduli that are too small or too large.
        let mut even = n.to_vec();
        even[255] &= 0xfe;
        assert!(PublicKey::from_der(&rsa_key(&even, &[1, 0, 1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key(&n[1..], &[1, 0, 1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key(&[0xc5; 1025], &[1, 0, 1]), MODULUS_BITS).is_none());
        // Non-minimal, negative and empty integers.
        assert!(PublicKey::from_der(&rsa_key_raw(&[&[0, 0][..], n].concat(), &[1, 0, 1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key_raw(n, &[1, 0, 1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key_raw(&[&[0][..], n].concat(), &[0, 1, 0, 1]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key_raw(&[&[0][..], n].concat(), &[]), MODULUS_BITS).is_none());
        assert!(PublicKey::from_der(&rsa_key_raw(&[], &[1, 0, 1]), MODULUS_BITS).is_none());
    }

    /// The RSA signatures in RFC 8448 section 3, made with a 1024-bit key
    /// (accepted here only by lowering the size limit).
    #[test]
    fn rfc8448_signatures() {
        let certificate_message = hex(RFC8448_CERTIFICATE);
        let cert = &certificate_message[11..11 + 432];
        let spki_key = {
            let at = cert.windows(7).position(|w| w == hex("30818902818100")).unwrap();
            &cert[at..at + 140]
        };
        let small = 1024..=8192;

        // The self-signed certificate: sha256WithRSAEncryption over the
        // TBSCertificate.
        let (tbs, signature) = (&cert[4..4 + 281], &cert[432 - 128..]);
        assert!(verify_with_sizes(spki_key, tbs, signature, Padding::Pkcs1, MessageHash::Sha256, small.clone()));
        assert!(!verify(spki_key, tbs, signature, Padding::Pkcs1, MessageHash::Sha256));

        // The server's CertificateVerify: rsa_pss_rsae_sha256 over the
        // transcript hash (RFC 8446 section 4.4.3).
        let transcript = MessageHash::Sha256.digest(&[
            &hex(RFC8448_CLIENT_HELLO),
            &hex(RFC8448_SERVER_HELLO),
            &hex("080000240022000a00140012001d00170018001901000101010201030104001c0002400100000000"),
            &certificate_message,
        ]);
        let mut signed = alloc::vec![0x20u8; 64];
        signed.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
        signed.extend_from_slice(transcript.as_ref());
        let certificate_verify = hex(RFC8448_CERTIFICATE_VERIFY);
        assert_eq!(certificate_verify[4..8], [0x08, 0x04, 0x00, 0x80]);
        let signature = &certificate_verify[8..];
        assert!(verify_with_sizes(spki_key, &signed, signature, Padding::Pss, MessageHash::Sha256, small.clone()));
        assert!(!verify_with_sizes(spki_key, &signed, signature, Padding::Pkcs1, MessageHash::Sha256, small));
    }

    const RFC8448_CLIENT_HELLO: &str = concat!(
        "010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef6283024dece700000613011303130201",
        "0000910000000b0009000006736572766572ff01000100000a00140012001d0017001800190100010101020103010400",
        "230000003300260024001d002099381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c002b00",
        "03020304000d0020001e040305030603020308040805080604010501060102010402050206020202002d00020101001c",
        "00024001",
    );

    const RFC8448_SERVER_HELLO: &str = concat!(
        "020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e2692800130100002e00330024",
        "001d0020c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e751f0f002b00020304",
    );

    const RFC8448_CERTIFICATE: &str = concat!(
        "0b0001b9000001b50001b0308201ac30820115a003020102020102300d06092a864886f70d01010b0500300e310c300a",
        "06035504031303727361301e170d3136303733303031323335395a170d3236303733303031323335395a300e310c300a",
        "0603550403130372736130819f300d06092a864886f70d010101050003818d0030818902818100b4bb498f8279303d98",
        "0836399b36c6988c0c68de55e1bdb826d3901a2461eafd2de49a91d015abbc9a95137ace6c1af19eaa6af98c7ced4312",
        "0998e187a80ee0ccb0524b1b018c3e0b63264d449a6d38e22a5fda430846748030530ef0461c8ca9d9efbfae8ea6d1d0",
        "3e2bd193eff0ab9a8002c47428a6d35a8d88d79f7f1e3f0203010001a31a301830090603551d1304023000300b060355",
        "1d0f0404030205a0300d06092a864886f70d01010b05000381810085aad2a0e5b9276b908c65f73a7267170618a54c5f",
        "8a7b337d2df7a594365417f2eae8f8a58c8f8172f9319cf36b7fd6c55b80f21a03015156726096fd335e5e67f2dbf102",
        "702e608ccae6bec1fc63a42a99be5c3eb7107c3c54e9b9eb2bd5203b1c3b84e0a8b2f759409ba3eac9d91d402dcc0cc8",
        "f8961229ac9187b42b4de10000",
    );

    const RFC8448_CERTIFICATE_VERIFY: &str = concat!(
        "0f000084080400805a747c5d88fa9bd2e55ab085a61015b7211f824cd484145ab3ff52f1fda8477b0b7abc90db78e2d3",
        "3a5c141a078653fa6bef780c5ea248eeaaa785c4f394cab6d30bbe8d4859ee511f602957b15411ac027671459e46445c",
        "9ea58c181e818e95b8c3fb0bf3278409d3be152a3da5043e063dda65cdf5aea20d53dfacd42f74f3",
    );
}
