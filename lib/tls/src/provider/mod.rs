//! The cryptography behind TLS: a rustls [`CryptoProvider`] built on the
//! RustCrypto and dalek crates (pure Rust, no C or assembly).
//!
//! * [`aead`] — record protection: AES-128-GCM, AES-256-GCM and
//!   ChaCha20-Poly1305, for TLS 1.3 and TLS 1.2.
//! * [`hash`] — SHA-256, SHA-384 and HMAC; the TLS 1.3 key schedule (HKDF)
//!   and the TLS 1.2 PRF are rustls' generic constructions over that HMAC.
//! * [`kx`] — ephemeral key exchange: X25519, secp256r1, secp384r1.
//! * [`sign_verify`] — certificate and handshake signatures: ECDSA (P-256,
//!   P-384), Ed25519, and RSA PKCS#1 v1.5 and PSS (`rsa`).
//! * [`RandomSource`] — where the random numbers come from.
//!
//! The provider has no private-key support: Vindows is a TLS client without
//! client certificates.

pub(crate) mod aead;
pub(crate) mod hash;
pub(crate) mod kx;
pub(crate) mod rsa;
pub(crate) mod sign_verify;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use rustls::crypto::tls12::PrfUsingHmac;
use rustls::crypto::tls13::HkdfUsingHmac;
use rustls::crypto::{CipherSuiteCommon, CryptoProvider, GetRandomFailed, KeyExchangeAlgorithm, KeyProvider};
use rustls::crypto::{SecureRandom, SupportedKxGroup};
use rustls::pki_types::PrivateKeyDer;
use rustls::sign::SigningKey;
use rustls::{CipherSuite, Error, SignatureScheme, SupportedCipherSuite, Tls12CipherSuite, Tls13CipherSuite};

use aead::{Algorithm, Tls12Aead, Tls13Aead};
use kx::{Group, KxGroup};

/// A source of cryptographically secure random numbers for TLS: client
/// randoms, key exchange secrets, and everything else rustls needs.
///
/// rustls keeps `'static` references to the key exchange groups, which take
/// their secrets from this source, so a `RandomSource` is a `static`:
///
/// ```ignore
/// static RANDOM: vtls::RandomSource = vtls::RandomSource::new(my_fill);
/// ```
pub struct RandomSource {
    fill: fn(&mut [u8]),
    kx_groups: [KxGroup; 3],
}

impl RandomSource {
    /// A source that fills buffers with `fill`, which must produce
    /// cryptographically secure random bytes and cannot fail.
    pub const fn new(fill: fn(&mut [u8])) -> RandomSource {
        RandomSource {
            fill,
            kx_groups: [
                KxGroup::new(Group::ALL[0], fill),
                KxGroup::new(Group::ALL[1], fill),
                KxGroup::new(Group::ALL[2], fill),
            ],
        }
    }

    /// Fills `buf` with random bytes.
    pub fn fill(&self, buf: &mut [u8]) {
        (self.fill)(buf);
    }

    /// The key exchange groups, in order of preference.
    fn kx_groups(&'static self) -> Vec<&'static dyn SupportedKxGroup> {
        self.kx_groups.iter().map(|g| g as &'static dyn SupportedKxGroup).collect()
    }
}

impl fmt::Debug for RandomSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RandomSource")
    }
}

impl SecureRandom for RandomSource {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        (self.fill)(buf);
        Ok(())
    }
}

/// The kernel's random number generator: a ChaCha20 stream seeded from the
/// firmware's RNG, RDSEED/RDRAND and timing jitter.
pub static SYSTEM_RANDOM: RandomSource = RandomSource::new(vrt::object::random_bytes);

/// The cipher suites, in order of preference: AES-128-GCM is the cheapest
/// here (it uses AES-NI when the processor has it), ChaCha20-Poly1305 the
/// fallback.
pub(crate) static CIPHER_SUITES: &[SupportedCipherSuite] = &[
    TLS13_AES_128_GCM_SHA256,
    TLS13_AES_256_GCM_SHA384,
    TLS13_CHACHA20_POLY1305_SHA256,
    TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
];

/// AES-GCM's confidentiality limit in records (2^24, see rustls'
/// `CipherSuiteCommon::confidentiality_limit`).
const GCM_LIMIT: u64 = 1 << 24;

const fn tls13(
    suite: CipherSuite,
    hash: &'static dyn rustls::crypto::hash::Hash,
    hkdf: &'static dyn rustls::crypto::tls13::Hkdf,
    aead: &'static Tls13Aead,
    limit: u64,
) -> Tls13CipherSuite {
    Tls13CipherSuite {
        common: CipherSuiteCommon { suite, hash_provider: hash, confidentiality_limit: limit },
        hkdf_provider: hkdf,
        aead_alg: aead,
        quic: None,
    }
}

static HKDF_SHA256: HkdfUsingHmac<'static> = HkdfUsingHmac(&hash::HMAC_SHA256);
static HKDF_SHA384: HkdfUsingHmac<'static> = HkdfUsingHmac(&hash::HMAC_SHA384);
static PRF_SHA256: PrfUsingHmac<'static> = PrfUsingHmac(&hash::HMAC_SHA256);
static PRF_SHA384: PrfUsingHmac<'static> = PrfUsingHmac(&hash::HMAC_SHA384);

static AES_128_GCM_13: Tls13Aead = Tls13Aead(Algorithm::Aes128Gcm);
static AES_256_GCM_13: Tls13Aead = Tls13Aead(Algorithm::Aes256Gcm);
static CHACHA20_POLY1305_13: Tls13Aead = Tls13Aead(Algorithm::ChaCha20Poly1305);
static AES_128_GCM_12: Tls12Aead = Tls12Aead(Algorithm::Aes128Gcm);
static AES_256_GCM_12: Tls12Aead = Tls12Aead(Algorithm::Aes256Gcm);
static CHACHA20_POLY1305_12: Tls12Aead = Tls12Aead(Algorithm::ChaCha20Poly1305);

/// TLS_AES_128_GCM_SHA256 (TLS 1.3).
pub(crate) static TLS13_AES_128_GCM_SHA256: SupportedCipherSuite = SupportedCipherSuite::Tls13(&tls13(
    CipherSuite::TLS13_AES_128_GCM_SHA256,
    &hash::SHA256,
    &HKDF_SHA256,
    &AES_128_GCM_13,
    GCM_LIMIT,
));

/// TLS_AES_256_GCM_SHA384 (TLS 1.3).
pub(crate) static TLS13_AES_256_GCM_SHA384: SupportedCipherSuite = SupportedCipherSuite::Tls13(&tls13(
    CipherSuite::TLS13_AES_256_GCM_SHA384,
    &hash::SHA384,
    &HKDF_SHA384,
    &AES_256_GCM_13,
    GCM_LIMIT,
));

/// TLS_CHACHA20_POLY1305_SHA256 (TLS 1.3).
pub(crate) static TLS13_CHACHA20_POLY1305_SHA256: SupportedCipherSuite = SupportedCipherSuite::Tls13(&tls13(
    CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
    &hash::SHA256,
    &HKDF_SHA256,
    &CHACHA20_POLY1305_13,
    u64::MAX,
));

/// The signature schemes of ECDSA-authenticated TLS 1.2 suites, for a
/// server to choose from.
static TLS12_ECDSA_SCHEMES: &[SignatureScheme] =
    &[SignatureScheme::ED25519, SignatureScheme::ECDSA_NISTP384_SHA384, SignatureScheme::ECDSA_NISTP256_SHA256];

/// The signature schemes of RSA-authenticated TLS 1.2 suites.
static TLS12_RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];

const fn tls12(
    suite: CipherSuite,
    hash: &'static dyn rustls::crypto::hash::Hash,
    prf: &'static dyn rustls::crypto::tls12::Prf,
    sign: &'static [SignatureScheme],
    aead: &'static Tls12Aead,
    limit: u64,
) -> Tls12CipherSuite {
    Tls12CipherSuite {
        common: CipherSuiteCommon { suite, hash_provider: hash, confidentiality_limit: limit },
        prf_provider: prf,
        kx: KeyExchangeAlgorithm::ECDHE,
        sign,
        aead_alg: aead,
    }
}

/// TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 (TLS 1.2).
pub(crate) static TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256: SupportedCipherSuite = SupportedCipherSuite::Tls12(&tls12(
    CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    &hash::SHA256,
    &PRF_SHA256,
    TLS12_ECDSA_SCHEMES,
    &AES_128_GCM_12,
    GCM_LIMIT,
));

/// TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 (TLS 1.2).
pub(crate) static TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256: SupportedCipherSuite = SupportedCipherSuite::Tls12(&tls12(
    CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    &hash::SHA256,
    &PRF_SHA256,
    TLS12_RSA_SCHEMES,
    &AES_128_GCM_12,
    GCM_LIMIT,
));

/// TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 (TLS 1.2).
pub(crate) static TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384: SupportedCipherSuite = SupportedCipherSuite::Tls12(&tls12(
    CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    &hash::SHA384,
    &PRF_SHA384,
    TLS12_ECDSA_SCHEMES,
    &AES_256_GCM_12,
    GCM_LIMIT,
));

/// TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 (TLS 1.2).
pub(crate) static TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384: SupportedCipherSuite = SupportedCipherSuite::Tls12(&tls12(
    CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    &hash::SHA384,
    &PRF_SHA384,
    TLS12_RSA_SCHEMES,
    &AES_256_GCM_12,
    GCM_LIMIT,
));

/// TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 (TLS 1.2).
pub(crate) static TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls12(&tls12(
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        &hash::SHA256,
        &PRF_SHA256,
        TLS12_ECDSA_SCHEMES,
        &CHACHA20_POLY1305_12,
        u64::MAX,
    ));

/// TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 (TLS 1.2).
pub(crate) static TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls12(&tls12(
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        &hash::SHA256,
        &PRF_SHA256,
        TLS12_RSA_SCHEMES,
        &CHACHA20_POLY1305_12,
        u64::MAX,
    ));

/// Refuses to load private keys: the client authenticates no one.
#[derive(Debug)]
struct NoPrivateKeys;

impl KeyProvider for NoPrivateKeys {
    fn load_private_key(&self, _key_der: PrivateKeyDer<'static>) -> Result<Arc<dyn SigningKey>, Error> {
        Err(Error::General("vtls does not use private keys".into()))
    }
}

/// The provider, with its randomness from `random`.
pub(crate) fn provider(random: &'static RandomSource) -> CryptoProvider {
    CryptoProvider {
        cipher_suites: CIPHER_SUITES.to_vec(),
        kx_groups: random.kx_groups(),
        signature_verification_algorithms: sign_verify::SUPPORTED_ALGORITHMS,
        secure_random: random,
        key_provider: &NoPrivateKeys,
    }
}
