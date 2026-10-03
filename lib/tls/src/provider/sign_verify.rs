//! Signature verification for certificates (through webpki) and for the
//! handshake (CertificateVerify in TLS 1.3, ServerKeyExchange in TLS 1.2):
//! ECDSA on P-256 and P-384, Ed25519, RSA PKCS#1 v1.5 and RSA-PSS.
//!
//! The algorithm set and its mapping to TLS signature schemes is the one
//! rustls uses with *ring*.

use core::fmt;

use p256::ecdsa::signature::hazmat::PrehashVerifier;
use rustls::SignatureScheme;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{AlgorithmIdentifier, InvalidSignature, SignatureVerificationAlgorithm, alg_id};
use sha2::{Digest, Sha256, Sha384, Sha512};

use super::rsa::{self, Padding};

/// A hash function for signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageHash {
    Sha256,
    Sha384,
    Sha512,
}

/// A hash value of up to 64 bytes.
pub(crate) struct HashValue {
    bytes: [u8; 64],
    len: usize,
}

impl AsRef<[u8]> for HashValue {
    fn as_ref(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl MessageHash {
    pub(crate) const fn len(self) -> usize {
        match self {
            MessageHash::Sha256 => 32,
            MessageHash::Sha384 => 48,
            MessageHash::Sha512 => 64,
        }
    }

    /// The hash of the concatenation of `parts`.
    pub(crate) fn digest(self, parts: &[&[u8]]) -> HashValue {
        fn run<D: Digest>(parts: &[&[u8]], out: &mut [u8]) {
            let mut d = D::new();
            for part in parts {
                d.update(part);
            }
            out.copy_from_slice(&d.finalize());
        }
        let mut value = HashValue { bytes: [0; 64], len: self.len() };
        let out = &mut value.bytes[..self.len()];
        match self {
            MessageHash::Sha256 => run::<Sha256>(parts, out),
            MessageHash::Sha384 => run::<Sha384>(parts, out),
            MessageHash::Sha512 => run::<Sha512>(parts, out),
        }
        value
    }

    /// The DER DigestInfo prefix for PKCS#1 v1.5 signatures (RFC 8017
    /// section 9.2, note 1): the hash's AlgorithmIdentifier, then the
    /// OCTET STRING header of the hash value.
    pub(crate) const fn digest_info_prefix(self) -> &'static [u8] {
        match self {
            MessageHash::Sha256 => &[
                0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00,
                0x04, 0x20,
            ],
            MessageHash::Sha384 => &[
                0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00,
                0x04, 0x30,
            ],
            MessageHash::Sha512 => &[
                0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00,
                0x04, 0x40,
            ],
        }
    }
}

/// How a signature algorithm checks a signature.
#[derive(Debug, Clone, Copy)]
enum Kind {
    EcdsaP256(MessageHash),
    EcdsaP384(MessageHash),
    Ed25519,
    Rsa(Padding, MessageHash),
}

/// A signature verification algorithm for webpki and rustls.
struct Algorithm {
    name: &'static str,
    public_key: AlgorithmIdentifier,
    signature: AlgorithmIdentifier,
    kind: Kind,
}

impl fmt::Debug for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

impl SignatureVerificationAlgorithm for Algorithm {
    fn verify_signature(&self, public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), InvalidSignature> {
        let valid = match self.kind {
            Kind::EcdsaP256(hash) => ecdsa_p256(public_key, hash.digest(&[message]).as_ref(), signature),
            Kind::EcdsaP384(hash) => ecdsa_p384(public_key, hash.digest(&[message]).as_ref(), signature),
            Kind::Ed25519 => ed25519(public_key, message, signature),
            Kind::Rsa(padding, hash) => rsa::verify(public_key, message, signature, padding, hash),
        };
        if valid { Ok(()) } else { Err(InvalidSignature) }
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.public_key
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.signature
    }
}

/// Whether `point` is an uncompressed SEC1 point for coordinates of
/// `field_len` bytes (the only form certificates and TLS use).
fn is_uncompressed(point: &[u8], field_len: usize) -> bool {
    point.len() == 1 + 2 * field_len && point[0] == 0x04
}

fn ecdsa_p256(public_key: &[u8], prehash: &[u8], signature: &[u8]) -> bool {
    use p256::ecdsa::{Signature, VerifyingKey};
    if !is_uncompressed(public_key, 32) {
        return false;
    }
    let (Ok(key), Ok(signature)) = (VerifyingKey::from_sec1_bytes(public_key), Signature::from_der(signature)) else {
        return false;
    };
    key.verify_prehash(prehash, &signature).is_ok()
}

fn ecdsa_p384(public_key: &[u8], prehash: &[u8], signature: &[u8]) -> bool {
    use p384::ecdsa::{Signature, VerifyingKey};
    if !is_uncompressed(public_key, 48) {
        return false;
    }
    let (Ok(key), Ok(signature)) = (VerifyingKey::from_sec1_bytes(public_key), Signature::from_der(signature)) else {
        return false;
    };
    key.verify_prehash(prehash, &signature).is_ok()
}

fn ed25519(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let (Ok(public_key), Ok(signature)) = (<&[u8; 32]>::try_from(public_key), <&[u8; 64]>::try_from(signature)) else {
        return false;
    };
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(public_key) else { return false };
    // Strict: also rejects small-order keys and signature points.
    key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(signature)).is_ok()
}

const fn algorithm(
    name: &'static str,
    public_key: AlgorithmIdentifier,
    signature: AlgorithmIdentifier,
    kind: Kind,
) -> Algorithm {
    Algorithm { name, public_key, signature, kind }
}

static ECDSA_P256_SHA256: Algorithm =
    algorithm("ECDSA_P256_SHA256", alg_id::ECDSA_P256, alg_id::ECDSA_SHA256, Kind::EcdsaP256(MessageHash::Sha256));
static ECDSA_P256_SHA384: Algorithm =
    algorithm("ECDSA_P256_SHA384", alg_id::ECDSA_P256, alg_id::ECDSA_SHA384, Kind::EcdsaP256(MessageHash::Sha384));
static ECDSA_P384_SHA256: Algorithm =
    algorithm("ECDSA_P384_SHA256", alg_id::ECDSA_P384, alg_id::ECDSA_SHA256, Kind::EcdsaP384(MessageHash::Sha256));
static ECDSA_P384_SHA384: Algorithm =
    algorithm("ECDSA_P384_SHA384", alg_id::ECDSA_P384, alg_id::ECDSA_SHA384, Kind::EcdsaP384(MessageHash::Sha384));
static ED25519: Algorithm = algorithm("ED25519", alg_id::ED25519, alg_id::ED25519, Kind::Ed25519);

const fn rsa_pkcs1(name: &'static str, signature: AlgorithmIdentifier, hash: MessageHash) -> Algorithm {
    algorithm(name, alg_id::RSA_ENCRYPTION, signature, Kind::Rsa(Padding::Pkcs1, hash))
}

static RSA_PKCS1_SHA256: Algorithm = rsa_pkcs1("RSA_PKCS1_SHA256", alg_id::RSA_PKCS1_SHA256, MessageHash::Sha256);
static RSA_PKCS1_SHA384: Algorithm = rsa_pkcs1("RSA_PKCS1_SHA384", alg_id::RSA_PKCS1_SHA384, MessageHash::Sha384);
static RSA_PKCS1_SHA512: Algorithm = rsa_pkcs1("RSA_PKCS1_SHA512", alg_id::RSA_PKCS1_SHA512, MessageHash::Sha512);

// RFC 4055 requires NULL parameters in these AlgorithmIdentifiers, but also
// that implementations accept them absent (as some certificates have them).
static RSA_PKCS1_SHA256_ABSENT_PARAMS: Algorithm = rsa_pkcs1(
    "RSA_PKCS1_SHA256_ABSENT_PARAMS",
    AlgorithmIdentifier::from_slice(&[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
    MessageHash::Sha256,
);
static RSA_PKCS1_SHA384_ABSENT_PARAMS: Algorithm = rsa_pkcs1(
    "RSA_PKCS1_SHA384_ABSENT_PARAMS",
    AlgorithmIdentifier::from_slice(&[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c]),
    MessageHash::Sha384,
);
static RSA_PKCS1_SHA512_ABSENT_PARAMS: Algorithm = rsa_pkcs1(
    "RSA_PKCS1_SHA512_ABSENT_PARAMS",
    AlgorithmIdentifier::from_slice(&[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d]),
    MessageHash::Sha512,
);

// RSA-PSS with an rsaEncryption key ("rsae" in TLS 1.3).
static RSA_PSS_SHA256: Algorithm = algorithm(
    "RSA_PSS_SHA256",
    alg_id::RSA_ENCRYPTION,
    alg_id::RSA_PSS_SHA256,
    Kind::Rsa(Padding::Pss, MessageHash::Sha256),
);
static RSA_PSS_SHA384: Algorithm = algorithm(
    "RSA_PSS_SHA384",
    alg_id::RSA_ENCRYPTION,
    alg_id::RSA_PSS_SHA384,
    Kind::Rsa(Padding::Pss, MessageHash::Sha384),
);
static RSA_PSS_SHA512: Algorithm = algorithm(
    "RSA_PSS_SHA512",
    alg_id::RSA_ENCRYPTION,
    alg_id::RSA_PSS_SHA512,
    Kind::Rsa(Padding::Pss, MessageHash::Sha512),
);

/// Every algorithm, and the TLS signature schemes they implement (in order
/// of preference, which is what the client advertises).
pub(crate) static SUPPORTED_ALGORITHMS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[
        &ECDSA_P256_SHA256,
        &ECDSA_P256_SHA384,
        &ECDSA_P384_SHA256,
        &ECDSA_P384_SHA384,
        &ED25519,
        &RSA_PSS_SHA256,
        &RSA_PSS_SHA384,
        &RSA_PSS_SHA512,
        &RSA_PKCS1_SHA256,
        &RSA_PKCS1_SHA384,
        &RSA_PKCS1_SHA512,
        &RSA_PKCS1_SHA256_ABSENT_PARAMS,
        &RSA_PKCS1_SHA384_ABSENT_PARAMS,
        &RSA_PKCS1_SHA512_ABSENT_PARAMS,
    ],
    mapping: &[
        // TLS 1.3 fixes the curve by the scheme and uses only the first
        // entry; TLS 1.2 tries them all.
        (SignatureScheme::ECDSA_NISTP384_SHA384, &[&ECDSA_P384_SHA384, &ECDSA_P256_SHA384]),
        (SignatureScheme::ECDSA_NISTP256_SHA256, &[&ECDSA_P256_SHA256, &ECDSA_P384_SHA256]),
        (SignatureScheme::ED25519, &[&ED25519]),
        (SignatureScheme::RSA_PSS_SHA512, &[&RSA_PSS_SHA512]),
        (SignatureScheme::RSA_PSS_SHA384, &[&RSA_PSS_SHA384]),
        (SignatureScheme::RSA_PSS_SHA256, &[&RSA_PSS_SHA256]),
        (SignatureScheme::RSA_PKCS1_SHA512, &[&RSA_PKCS1_SHA512]),
        (SignatureScheme::RSA_PKCS1_SHA384, &[&RSA_PKCS1_SHA384]),
        (SignatureScheme::RSA_PKCS1_SHA256, &[&RSA_PKCS1_SHA256]),
    ],
};

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use p256::ecdsa::signature::hazmat::PrehashSigner;

    use super::*;
    use crate::tests::support::hex;

    fn check(alg: &Algorithm, public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
        alg.verify_signature(public_key, message, signature).is_ok()
    }

    #[test]
    fn ecdsa() {
        let message = b"ecdsa test message";
        let key256 = p256::ecdsa::SigningKey::from_slice(&[0x11; 32]).unwrap();
        let key384 = p384::ecdsa::SigningKey::from_slice(&[0x22; 48]).unwrap();
        let pub256 = key256.verifying_key().to_sec1_point(false).as_bytes().to_vec();
        let pub384 = key384.verifying_key().to_sec1_point(false).as_bytes().to_vec();
        let sign256 = |hash: MessageHash| -> Vec<u8> {
            let sig: p256::ecdsa::Signature = key256.sign_prehash(hash.digest(&[message]).as_ref()).unwrap();
            sig.to_der().as_bytes().to_vec()
        };
        let sign384 = |hash: MessageHash| -> Vec<u8> {
            let sig: p384::ecdsa::Signature = key384.sign_prehash(hash.digest(&[message]).as_ref()).unwrap();
            sig.to_der().as_bytes().to_vec()
        };
        for (alg, key, sig) in [
            (&ECDSA_P256_SHA256, &pub256, sign256(MessageHash::Sha256)),
            (&ECDSA_P256_SHA384, &pub256, sign256(MessageHash::Sha384)),
            (&ECDSA_P384_SHA256, &pub384, sign384(MessageHash::Sha256)),
            (&ECDSA_P384_SHA384, &pub384, sign384(MessageHash::Sha384)),
        ] {
            assert!(check(alg, key, message, &sig), "{alg:?}");
            assert!(!check(alg, key, b"other message", &sig));
            let mut bad = sig.clone();
            let last = bad.len() - 1;
            bad[last] ^= 1;
            assert!(!check(alg, key, message, &bad));
            // Truncations and garbage are rejected without a panic.
            for len in 0..sig.len() {
                assert!(!check(alg, key, message, &sig[..len]));
            }
            for len in 0..key.len() {
                assert!(!check(alg, &key[..len], message, &sig));
            }
        }
        // The hash must match the algorithm, and the key the curve.
        assert!(!check(&ECDSA_P256_SHA256, &pub256, message, &sign256(MessageHash::Sha384)));
        assert!(!check(&ECDSA_P384_SHA384, &pub256, message, &sign256(MessageHash::Sha384)));
        // Compressed points are not accepted.
        let compressed = key256.verifying_key().to_sec1_point(true).as_bytes().to_vec();
        assert!(!check(&ECDSA_P256_SHA256, &compressed, message, &sign256(MessageHash::Sha256)));
    }

    #[test]
    fn ed25519_rfc8032() {
        // RFC 8032 section 7.1, test 2.
        let public = hex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
        let message = hex("72");
        let signature = hex(concat!(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da",
            "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"
        ));
        assert!(check(&ED25519, &public, &message, &signature));
        assert!(!check(&ED25519, &public, &hex("73"), &signature));
        let mut bad = signature.clone();
        bad[63] ^= 0x10;
        assert!(!check(&ED25519, &public, &message, &bad));
        assert!(!check(&ED25519, &public[..31], &message, &signature));
        assert!(!check(&ED25519, &public, &message, &signature[..63]));
        // A small-order public key is refused.
        let mut weak = [0u8; 32];
        weak[0] = 1;
        assert!(!check(&ED25519, &weak, &message, &signature));
    }

    #[test]
    fn scheme_mapping() {
        // Every TLS scheme maps to algorithms that are in `all`.
        for (scheme, algs) in SUPPORTED_ALGORITHMS.mapping {
            assert!(!algs.is_empty(), "{scheme:?}");
            for alg in algs.iter() {
                assert!(
                    SUPPORTED_ALGORITHMS.all.iter().any(|a| core::ptr::addr_eq(*a, *alg)),
                    "{scheme:?} uses an algorithm missing from `all`"
                );
            }
        }
        assert_eq!(SUPPORTED_ALGORITHMS.mapping.len(), 9);
    }

    #[test]
    fn digest_info_prefixes_have_the_right_lengths() {
        for hash in [MessageHash::Sha256, MessageHash::Sha384, MessageHash::Sha512] {
            let prefix = hash.digest_info_prefix();
            // SEQUENCE length covers the rest of the prefix and the hash.
            assert_eq!(usize::from(prefix[1]), prefix.len() - 2 + hash.len());
            assert_eq!(usize::from(prefix[prefix.len() - 1]), hash.len());
            assert_eq!(hash.digest(&[b"x"]).as_ref().len(), hash.len());
        }
    }
}
