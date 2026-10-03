//! Test helpers: hex decoding, deterministic randomness, a fixed clock, the
//! test PKI with signing keys, and a rustls server that runs inside a
//! [`Transport`].

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::io::{Read, Write};

use crypto_bigint::modular::{BoxedMontyForm, BoxedMontyParams};
use crypto_bigint::{BoxedUint, Odd};
use p256::ecdsa::signature::hazmat::PrehashSigner;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::ServerConnection;
use rustls::sign::{CertifiedKey, Signer, SigningKey, SingleCertAndKey};
use rustls::time_provider::TimeProvider;
use rustls::{SignatureAlgorithm, SignatureScheme, SupportedCipherSuite, SupportedProtocolVersion};
use sha2::{Digest, Sha256};

use crate::provider::kx::Group;
use crate::provider::rsa::Padding;
use crate::provider::sign_verify::MessageHash;
use crate::provider::{self, RandomSource};
use crate::{ClientConfig, ClientConfigBuilder, TlsError, Transport};

/// Decodes hex, ignoring whitespace.
pub(crate) fn hex(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len().is_multiple_of(2), "odd number of hex digits");
    digits
        .chunks(2)
        .map(|pair| {
            let text = core::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap_or_else(|_| panic!("bad hex {text:?}"))
        })
        .collect()
}

/// Deterministic, never-repeating "random" bytes: SHA-256 of a counter.
pub(crate) fn test_fill(buf: &mut [u8]) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for chunk in buf.chunks_mut(32) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let block = Sha256::new().chain_update(b"vtls test randomness").chain_update(n.to_le_bytes()).finalize();
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}

/// The randomness of the tests.
pub(crate) static TEST_RANDOM: RandomSource = RandomSource::new(test_fill);

/// A clock stopped at a given time.
#[derive(Debug)]
pub(crate) struct FixedClock(pub(crate) u64);

impl TimeProvider for FixedClock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(self.0)))
    }
}

/// TLS 1.3 alone, for servers.
pub(crate) static TLS13: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];
/// TLS 1.2 alone, for servers.
pub(crate) static TLS12: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];

/// The name in the test certificates.
pub(crate) const TEST_NAME: &str = "test.vindows.local";
/// 2026-06-01: the test certificates are valid from 2026 to 2036.
pub(crate) const TEST_TIME: u64 = 1_780_272_000;

/// A file of the test PKI (`testdata/pki`, made by its `make.sh`).
pub(crate) fn pki(name: &str) -> &'static [u8] {
    match name {
        "ca-ecdsa" => include_bytes!("../../testdata/pki/ca-ecdsa.der"),
        "ca-rsa" => include_bytes!("../../testdata/pki/ca-rsa.der"),
        "int-ecdsa" => include_bytes!("../../testdata/pki/int-ecdsa.der"),
        "leaf-p256" => include_bytes!("../../testdata/pki/leaf-p256.der"),
        "leaf-p384" => include_bytes!("../../testdata/pki/leaf-p384.der"),
        "leaf-chain" => include_bytes!("../../testdata/pki/leaf-chain.der"),
        "leaf-ed25519" => include_bytes!("../../testdata/pki/leaf-ed25519.der"),
        "leaf-rsa" => include_bytes!("../../testdata/pki/leaf-rsa.der"),
        "leaf-rsa-pss" => include_bytes!("../../testdata/pki/leaf-rsa-pss.der"),
        "leaf-expired" => include_bytes!("../../testdata/pki/leaf-expired.der"),
        "leaf-future" => include_bytes!("../../testdata/pki/leaf-future.der"),
        "leaf-other-name" => include_bytes!("../../testdata/pki/leaf-other-name.der"),
        "leaf-untrusted" => include_bytes!("../../testdata/pki/leaf-untrusted.der"),
        "leaf-client-only" => include_bytes!("../../testdata/pki/leaf-client-only.der"),
        "leaf-p256.key" => include_bytes!("../../testdata/pki/leaf-p256.key.der"),
        "leaf-p384.key" => include_bytes!("../../testdata/pki/leaf-p384.key.der"),
        "leaf-ed25519.key" => include_bytes!("../../testdata/pki/leaf-ed25519.key.der"),
        "leaf-rsa.key" => include_bytes!("../../testdata/pki/leaf-rsa.key.der"),
        _ => panic!("no test PKI file {name}"),
    }
}

/// A client configuration trusting only the test roots, with test
/// randomness and the test time.
pub(crate) fn client_config() -> ClientConfigBuilder {
    ClientConfig::builder()
        .trust_only(&[pki("ca-ecdsa"), pki("ca-rsa")])
        .unwrap()
        .random(&TEST_RANDOM)
        .clock(Arc::new(FixedClock(TEST_TIME)))
}

/// Splits DER elements off the front of `input`: (tag, contents, rest).
/// Test-only and trusting: the input is our own test data.
fn der(input: &[u8]) -> (u8, &[u8], &[u8]) {
    let (tag, len_byte) = (input[0], input[1]);
    let (len, header) = match len_byte {
        0x81 => (usize::from(input[2]), 3),
        0x82 => (usize::from(u16::from_be_bytes([input[2], input[3]])), 4),
        n => (usize::from(n), 2),
    };
    (tag, &input[header..header + len], &input[header + len..])
}

/// The contents of the INTEGERs (and other elements) of a SEQUENCE.
fn der_sequence(input: &[u8]) -> Vec<&[u8]> {
    let (_, mut contents, _) = der(input);
    let mut items = Vec::new();
    while !contents.is_empty() {
        let (_, item, rest) = der(contents);
        items.push(item);
        contents = rest;
    }
    items
}

/// An RSA private key (only what signing needs).
pub(crate) struct RsaKey {
    n: Odd<BoxedUint>,
    d: BoxedUint,
    len: usize,
    bits: u32,
}

impl RsaKey {
    /// Reads a PKCS#1 RSAPrivateKey.
    fn from_pkcs1(der_bytes: &[u8]) -> RsaKey {
        let items = der_sequence(der_bytes);
        let strip = |v: &[u8]| -> Vec<u8> { v.iter().copied().skip_while(|&b| b == 0).collect() };
        let (n, d) = (strip(items[1]), strip(items[3]));
        let bits = (n.len() * 8) as u32 - n[0].leading_zeros();
        let precision = (n.len() * 8) as u32;
        let modulus = Odd::new(BoxedUint::from_be_slice(&n, precision).unwrap()).unwrap();
        RsaKey { len: n.len(), bits, d: BoxedUint::from_be_slice(&d, precision).unwrap(), n: modulus }
    }

    /// RSASSA-PKCS1-v1_5 or RSASSA-PSS (salt as long as the hash).
    pub(crate) fn sign(&self, padding: Padding, hash: MessageHash, message: &[u8]) -> Vec<u8> {
        let digest = hash.digest(&[message]);
        let mut em = alloc::vec![0u8; self.len];
        match padding {
            Padding::Pkcs1 => {
                let t: Vec<u8> = [hash.digest_info_prefix(), digest.as_ref()].concat();
                em[1] = 1;
                let ps_end = self.len - t.len() - 1;
                em[2..ps_end].fill(0xff);
                em[ps_end + 1..].copy_from_slice(&t);
            }
            Padding::Pss => {
                // RFC 8017 section 9.1.1, written independently of the verifier.
                let em_bits = self.bits as usize - 1;
                let em_len = em_bits.div_ceil(8);
                let h_len = hash.len();
                let mut salt = alloc::vec![0u8; h_len];
                test_fill(&mut salt);
                let h = hash.digest(&[&[0u8; 8], digest.as_ref(), &salt]);
                let mut db = alloc::vec![0u8; em_len - h_len - 1];
                let one = db.len() - h_len - 1;
                db[one] = 1;
                db[one + 1..].copy_from_slice(&salt);
                let mut mask = Vec::new();
                for counter in 0u32.. {
                    if mask.len() >= db.len() {
                        break;
                    }
                    mask.extend_from_slice(hash.digest(&[h.as_ref(), &counter.to_be_bytes()]).as_ref());
                }
                for (b, m) in db.iter_mut().zip(&mask) {
                    *b ^= m;
                }
                db[0] &= 0xff >> (8 * em_len - em_bits);
                let start = self.len - em_len;
                em[start..start + db.len()].copy_from_slice(&db);
                em[start + db.len()..self.len - 1].copy_from_slice(h.as_ref());
                em[self.len - 1] = 0xbc;
            }
        }
        let m = BoxedUint::from_be_slice(&em, self.n.bits_precision()).unwrap();
        let params = BoxedMontyParams::new_vartime(self.n.clone());
        let s = BoxedMontyForm::new(m, &params).pow(&self.d).retrieve().to_be_bytes();
        s[s.len() - self.len..].to_vec()
    }
}

/// A server signing key of the test PKI.
#[derive(Clone)]
pub(crate) enum TestKey {
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
    Ed25519(ed25519_dalek::SigningKey),
    /// With the schemes it may use, most preferred first.
    Rsa(Arc<RsaKey>, &'static [SignatureScheme]),
}

impl core::fmt::Debug for TestKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            TestKey::P256(_) => "P256",
            TestKey::P384(_) => "P384",
            TestKey::Ed25519(_) => "Ed25519",
            TestKey::Rsa(..) => "Rsa",
        })
    }
}

/// Every RSA signature scheme, PSS first.
pub(crate) const RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];

impl TestKey {
    pub(crate) fn p256() -> TestKey {
        let items = der_sequence(pki("leaf-p256.key"));
        TestKey::P256(p256::ecdsa::SigningKey::from_slice(items[1]).unwrap())
    }

    pub(crate) fn p384() -> TestKey {
        let items = der_sequence(pki("leaf-p384.key"));
        TestKey::P384(p384::ecdsa::SigningKey::from_slice(items[1]).unwrap())
    }

    pub(crate) fn ed25519() -> TestKey {
        // PKCS#8 for Ed25519 ends with the 32-byte seed.
        let key = pki("leaf-ed25519.key");
        let seed: [u8; 32] = key[key.len() - 32..].try_into().unwrap();
        TestKey::Ed25519(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    pub(crate) fn rsa(schemes: &'static [SignatureScheme]) -> TestKey {
        TestKey::Rsa(Arc::new(RsaKey::from_pkcs1(pki("leaf-rsa.key"))), schemes)
    }

    fn schemes(&self) -> &[SignatureScheme] {
        match self {
            TestKey::P256(_) => &[SignatureScheme::ECDSA_NISTP256_SHA256],
            TestKey::P384(_) => &[SignatureScheme::ECDSA_NISTP384_SHA384],
            TestKey::Ed25519(_) => &[SignatureScheme::ED25519],
            TestKey::Rsa(_, schemes) => schemes,
        }
    }
}

impl SigningKey for TestKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        let scheme = *self.schemes().iter().find(|s| offered.contains(s))?;
        Some(Box::new(TestSigner { key: self.clone(), scheme }))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        match self {
            TestKey::P256(_) | TestKey::P384(_) => SignatureAlgorithm::ECDSA,
            TestKey::Ed25519(_) => SignatureAlgorithm::ED25519,
            TestKey::Rsa(..) => SignatureAlgorithm::RSA,
        }
    }
}

#[derive(Debug)]
struct TestSigner {
    key: TestKey,
    scheme: SignatureScheme,
}

impl Signer for TestSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        use SignatureScheme as S;
        Ok(match (&self.key, self.scheme) {
            (TestKey::P256(key), _) => {
                let sig: p256::ecdsa::Signature = key.sign_prehash(&Sha256::digest(message)).unwrap();
                sig.to_der().as_bytes().to_vec()
            }
            (TestKey::P384(key), _) => {
                let sig: p384::ecdsa::Signature = key.sign_prehash(&sha2::Sha384::digest(message)).unwrap();
                sig.to_der().as_bytes().to_vec()
            }
            (TestKey::Ed25519(key), _) => {
                use ed25519_dalek::Signer as _;
                key.sign(message).to_bytes().to_vec()
            }
            (TestKey::Rsa(key, _), scheme) => {
                let (padding, hash) = match scheme {
                    S::RSA_PSS_SHA256 => (Padding::Pss, MessageHash::Sha256),
                    S::RSA_PSS_SHA384 => (Padding::Pss, MessageHash::Sha384),
                    S::RSA_PSS_SHA512 => (Padding::Pss, MessageHash::Sha512),
                    S::RSA_PKCS1_SHA256 => (Padding::Pkcs1, MessageHash::Sha256),
                    S::RSA_PKCS1_SHA384 => (Padding::Pkcs1, MessageHash::Sha384),
                    _ => (Padding::Pkcs1, MessageHash::Sha512),
                };
                key.sign(padding, hash, message)
            }
        })
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

/// How a test server is set up.
pub(crate) struct ServerSetup {
    /// The certificate chain (test PKI names), leaf first.
    pub(crate) chain: &'static [&'static str],
    pub(crate) key: TestKey,
    /// Cipher suites (all if empty).
    pub(crate) suites: Vec<SupportedCipherSuite>,
    /// Key exchange groups (all if empty).
    pub(crate) groups: Vec<Group>,
    pub(crate) versions: &'static [&'static SupportedProtocolVersion],
    pub(crate) alpn: Vec<Vec<u8>>,
}

impl ServerSetup {
    /// An ECDSA P-256 server, everything else default.
    pub(crate) fn new() -> ServerSetup {
        ServerSetup {
            chain: &["leaf-p256"],
            key: TestKey::p256(),
            suites: Vec::new(),
            groups: Vec::new(),
            versions: rustls::ALL_VERSIONS,
            alpn: Vec::new(),
        }
    }

    pub(crate) fn chain(mut self, chain: &'static [&'static str], key: TestKey) -> Self {
        self.chain = chain;
        self.key = key;
        self
    }

    pub(crate) fn suite(mut self, suite: SupportedCipherSuite) -> Self {
        self.suites = alloc::vec![suite];
        self
    }

    pub(crate) fn group(mut self, group: Group) -> Self {
        self.groups = alloc::vec![group];
        self
    }

    pub(crate) fn versions(mut self, versions: &'static [&'static SupportedProtocolVersion]) -> Self {
        self.versions = versions;
        self
    }

    pub(crate) fn alpn(mut self, protocols: &[&[u8]]) -> Self {
        self.alpn = protocols.iter().map(|p| p.to_vec()).collect();
        self
    }

    /// A server with this setup, our provider on its side too.
    pub(crate) fn start(self) -> TestServer {
        let mut provider: CryptoProvider = provider::provider(&TEST_RANDOM);
        if !self.suites.is_empty() {
            provider.cipher_suites = self.suites;
        }
        if !self.groups.is_empty() {
            provider.kx_groups.retain(|g| self.groups.iter().any(|ours| format_group(*ours) == g.name()));
        }
        let chain = self.chain.iter().map(|name| CertificateDer::from(pki(name).to_vec())).collect();
        let key = CertifiedKey::new(chain, Arc::new(self.key));
        let mut config =
            rustls::ServerConfig::builder_with_details(Arc::new(provider), Arc::new(FixedClock(TEST_TIME)))
                .with_protocol_versions(self.versions)
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(SingleCertAndKey::from(key)));
        config.alpn_protocols = self.alpn;
        TestServer {
            conn: ServerConnection::new(Arc::new(config)).unwrap(),
            to_client: Vec::new(),
            sent: 0,
            chunk: usize::MAX,
            echo: true,
            received: Vec::new(),
            error: None,
            hang_up: false,
            greeting: None,
            tamper: None,
        }
    }
}

fn format_group(group: Group) -> rustls::NamedGroup {
    match group {
        Group::X25519 => rustls::NamedGroup::X25519,
        Group::Secp256r1 => rustls::NamedGroup::secp256r1,
        Group::Secp384r1 => rustls::NamedGroup::secp384r1,
    }
}

/// Changes a batch of bytes the server sends (for corruption tests).
pub(crate) type Tamper = Box<dyn FnMut(&mut Vec<u8>)>;

/// A rustls server that runs synchronously inside the client's transport:
/// what the client writes is processed at once, and what the server
/// answers is what the client reads next.
pub(crate) struct TestServer {
    pub(crate) conn: ServerConnection,
    /// Bytes for the client; `to_client[sent..]` are not read yet.
    to_client: Vec<u8>,
    sent: usize,
    /// At most this many bytes per client read (to split records).
    pub(crate) chunk: usize,
    /// Send application data back as it arrives.
    pub(crate) echo: bool,
    /// Application data the server received.
    pub(crate) received: Vec<u8>,
    /// The first error of the server's connection.
    pub(crate) error: Option<rustls::Error>,
    /// After its output, the server ends the TCP stream.
    pub(crate) hang_up: bool,
    /// Data the server sends as soon as the handshake is done.
    pub(crate) greeting: Option<Vec<u8>>,
    /// Applied to each batch of bytes sent to the client.
    pub(crate) tamper: Option<Tamper>,
}

impl TestServer {
    /// Processes what the client sent and queues the server's answer.
    fn pump(&mut self) {
        if let Err(e) = self.conn.process_new_packets() {
            self.error.get_or_insert(e);
        }
        let mut buf = alloc::vec![0u8; 65536];
        loop {
            match self.conn.reader().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    self.received.extend_from_slice(&buf[..n]);
                    if self.echo {
                        self.conn.writer().write_all(&buf[..n]).unwrap();
                    }
                }
            }
        }
        if !self.conn.is_handshaking()
            && let Some(greeting) = self.greeting.take()
        {
            self.conn.writer().write_all(&greeting).unwrap();
        }
        self.flush();
    }

    /// Moves the server's pending output to the client's side.
    pub(crate) fn flush(&mut self) {
        let start = self.to_client.len();
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut self.to_client).is_err() {
                break;
            }
        }
        if let Some(tamper) = &mut self.tamper
            && self.to_client.len() > start
        {
            let mut fresh = self.to_client.split_off(start);
            tamper(&mut fresh);
            self.to_client.extend_from_slice(&fresh);
        }
    }
}

impl Transport for TestServer {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TlsError> {
        // Nothing will ever arrive: what a real transport's timeout reports.
        self.try_read(buf)?.ok_or(TlsError::TimedOut)
    }

    fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, TlsError> {
        let pending = &self.to_client[self.sent..];
        if pending.is_empty() {
            return Ok(self.hang_up.then_some(0));
        }
        let n = pending.len().min(buf.len()).min(self.chunk);
        buf[..n].copy_from_slice(&pending[..n]);
        self.sent += n;
        Ok(Some(n))
    }

    fn write_all(&mut self, mut data: &[u8]) -> Result<(), TlsError> {
        while !data.is_empty() {
            match self.conn.read_tls(&mut data) {
                Ok(0) => break,
                Ok(_) => self.pump(),
                Err(e) => return Err(TlsError::Io(e.to_string())),
            }
        }
        Ok(())
    }
}
