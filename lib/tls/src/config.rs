//! Client configuration: trusted certificate authorities, protocol versions,
//! application protocols, and the system's randomness and clock.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use rustls::client::Resumption;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::{RootCertStore, SupportedProtocolVersion};

use crate::error::TlsError;
use crate::provider::{self, RandomSource, SYSTEM_RANDOM};

/// Seconds since 1970 at 2025-01-01: a clock that reads earlier was never set
/// (the firmware had no time), and every certificate would look "not yet
/// valid".
const EARLIEST_PLAUSIBLE_TIME: u64 = 1_735_689_600;

static TLS13_AND_TLS12: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13, &rustls::version::TLS12];
static TLS13_ONLY: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];
#[cfg(test)]
static TLS12_ONLY: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];

/// The system's wall clock (`vrt::time::unix_time_ns`), for certificate
/// validity checks.
#[derive(Debug)]
pub struct SystemClock;

impl TimeProvider for SystemClock {
    fn current_time(&self) -> Option<UnixTime> {
        let secs = vrt::time::unix_time_ns() / 1_000_000_000;
        (secs >= EARLIEST_PLAUSIBLE_TIME).then(|| UnixTime::since_unix_epoch(Duration::from_secs(secs)))
    }
}

/// A TLS client configuration, shared by any number of connections (cloning
/// is cheap).
///
/// [`ClientConfig::new`] trusts the certificate authorities of the Mozilla
/// root program (webpki-roots), offers TLS 1.3 and TLS 1.2, and takes its
/// randomness and time from the system.
#[derive(Clone)]
pub struct ClientConfig {
    inner: Arc<rustls::ClientConfig>,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig").field("alpn_protocols", &self.inner.alpn_protocols).finish()
    }
}

impl ClientConfig {
    /// The default configuration, without application protocol negotiation.
    pub fn new() -> ClientConfig {
        ClientConfig::builder().build()
    }

    /// The default configuration, offering the application protocols
    /// `protocols` (ALPN), most preferred first: for example
    /// `&[b"http/1.1"]`.
    pub fn with_alpn(protocols: &[&[u8]]) -> ClientConfig {
        ClientConfig::builder().alpn(protocols).build()
    }

    /// A builder starting from the defaults.
    pub fn builder() -> ClientConfigBuilder {
        ClientConfigBuilder {
            roots: RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() },
            alpn: Vec::new(),
            random: &SYSTEM_RANDOM,
            clock: Arc::new(SystemClock),
            versions: TLS13_AND_TLS12,
            #[cfg(test)]
            adjust: None,
        }
    }

    pub(crate) fn rustls(&self) -> &Arc<rustls::ClientConfig> {
        &self.inner
    }
}

impl Default for ClientConfig {
    fn default() -> ClientConfig {
        ClientConfig::new()
    }
}

/// Builds a [`ClientConfig`] that differs from the defaults.
pub struct ClientConfigBuilder {
    roots: RootCertStore,
    alpn: Vec<Vec<u8>>,
    random: &'static RandomSource,
    clock: Arc<dyn TimeProvider>,
    versions: &'static [&'static SupportedProtocolVersion],
    /// Tests: changes to the provider (to force a cipher suite or group).
    #[cfg(test)]
    adjust: Option<AdjustProvider>,
}

/// A change to the provider, for tests.
#[cfg(test)]
type AdjustProvider = alloc::boxed::Box<dyn Fn(&mut rustls::crypto::CryptoProvider)>;

impl fmt::Debug for ClientConfigBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfigBuilder")
            .field("roots", &self.roots.len())
            .field("alpn", &self.alpn)
            .field("versions", &self.versions)
            .finish()
    }
}

impl ClientConfigBuilder {
    /// Offers the application protocols `protocols` (ALPN), most preferred
    /// first.
    pub fn alpn(mut self, protocols: &[&[u8]]) -> Self {
        self.alpn = protocols.iter().map(|p| p.to_vec()).collect();
        self
    }

    /// Trusts only the given certificate authorities (DER certificates)
    /// instead of the built-in ones.
    pub fn trust_only(mut self, certificates: &[&[u8]]) -> Result<Self, TlsError> {
        self.roots = RootCertStore::empty();
        for der in certificates {
            self = self.add_trusted(der)?;
        }
        Ok(self)
    }

    /// Also trusts the certificate authority `der` (a DER certificate).
    pub fn add_trusted(mut self, der: &[u8]) -> Result<Self, TlsError> {
        self.roots
            .add(CertificateDer::from(der.to_vec()))
            .map_err(|e| TlsError::BadCertificate(alloc::format!("cannot trust it: {e}")))?;
        Ok(self)
    }

    /// Takes random numbers from `random` instead of the kernel.
    pub fn random(mut self, random: &'static RandomSource) -> Self {
        self.random = random;
        self
    }

    /// Checks certificate validity against `clock` instead of the system
    /// clock.
    pub fn clock(mut self, clock: Arc<dyn TimeProvider>) -> Self {
        self.clock = clock;
        self
    }

    /// Offers only TLS 1.3.
    pub fn tls13_only(mut self) -> Self {
        self.versions = TLS13_ONLY;
        self
    }

    /// Offers only TLS 1.2 (for tests: TLS 1.3 is better in every way).
    #[cfg(test)]
    pub(crate) fn tls12_only(mut self) -> Self {
        self.versions = TLS12_ONLY;
        self
    }

    /// Changes the provider, for example to offer a single cipher suite.
    #[cfg(test)]
    pub(crate) fn adjust(mut self, adjust: impl Fn(&mut rustls::crypto::CryptoProvider) + 'static) -> Self {
        self.adjust = Some(alloc::boxed::Box::new(adjust));
        self
    }

    /// The configuration.
    pub fn build(self) -> ClientConfig {
        #[allow(unused_mut)]
        let mut provider = provider::provider(self.random);
        #[cfg(test)]
        if let Some(adjust) = &self.adjust {
            adjust(&mut provider);
        }
        let mut config = rustls::ClientConfig::builder_with_details(Arc::new(provider), self.clock)
            .with_protocol_versions(self.versions)
            .expect("the provider has cipher suites for TLS 1.3 and TLS 1.2")
            .with_root_certificates(self.roots)
            .with_no_client_auth();
        config.alpn_protocols = self.alpn;
        // Every connection is a full handshake (no session cache yet).
        config.resumption = Resumption::disabled();
        ClientConfig { inner: Arc::new(config) }
    }
}
