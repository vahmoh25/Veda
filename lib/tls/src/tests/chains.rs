//! Certificate chains of real servers, captured on 2026-10-03 and verified
//! against the built-in roots (webpki-roots) at that time, as the client
//! does during a handshake.
//!
//! * api.deepgram.com (agent.deepgram.com serves the same certificate):
//!   RSA 2048, Let's Encrypt YR1, "Root YR" cross-signed by ISRG Root X1
//!   (RSA 4096); PKCS#1 v1.5 SHA-256 throughout.
//! * example.com: Cloudflare's ECDSA chain: P-256/SHA-256 and P-384/SHA-384
//!   signatures (SSL.com roots).
//! * www.google.com: a P-256 key certified by Google Trust Services' RSA
//!   chain (WR2, GTS Root R1).

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::time::Duration;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{
    AlgorithmIdentifier, CertificateDer, InvalidSignature, ServerName, SignatureVerificationAlgorithm, UnixTime,
};
use rustls::{CertificateError, RootCertStore};

use super::support::TEST_RANDOM;
use crate::TlsError;
use crate::provider::{self, sign_verify};

/// 2026-10-03 12:00 UTC, when the chains were captured.
const CAPTURED: u64 = 1_791_028_800;
const DAY: u64 = 86_400;

struct Capture {
    /// Names the leaf certificate is valid for.
    names: &'static [&'static str],
    /// The chain as the server sent it, leaf first.
    chain: &'static [&'static [u8]],
}

const DEEPGRAM: Capture = Capture {
    names: &["api.deepgram.com", "agent.deepgram.com"],
    chain: &[
        include_bytes!("../../testdata/chains/deepgram/0.der"),
        include_bytes!("../../testdata/chains/deepgram/1.der"),
        include_bytes!("../../testdata/chains/deepgram/2.der"),
    ],
};

const EXAMPLE: Capture = Capture {
    names: &["example.com", "www.example.com"],
    chain: &[
        include_bytes!("../../testdata/chains/example.com/0.der"),
        include_bytes!("../../testdata/chains/example.com/1.der"),
        include_bytes!("../../testdata/chains/example.com/2.der"),
        include_bytes!("../../testdata/chains/example.com/3.der"),
    ],
};

const GOOGLE: Capture = Capture {
    names: &["www.google.com"],
    chain: &[
        include_bytes!("../../testdata/chains/google/0.der"),
        include_bytes!("../../testdata/chains/google/1.der"),
        include_bytes!("../../testdata/chains/google/2.der"),
    ],
};

std::thread_local! {
    /// The algorithms that accepted a signature on this thread.
    static USED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// One of the provider's algorithms, recording its successes in `USED`.
#[derive(Debug)]
struct Recording(&'static dyn SignatureVerificationAlgorithm);

impl SignatureVerificationAlgorithm for Recording {
    fn verify_signature(&self, public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), InvalidSignature> {
        let result = self.0.verify_signature(public_key, message, signature);
        if result.is_ok() {
            USED.with(|used| used.borrow_mut().push(format!("{:?}", self.0)));
        }
        result
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.0.public_key_alg_id()
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.0.signature_alg_id()
    }
}

/// The client's verifier: the built-in roots and the provider's algorithms
/// (recording which ones verify signatures).
fn verifier() -> Arc<WebPkiServerVerifier> {
    let all: Vec<&'static dyn SignatureVerificationAlgorithm> = sign_verify::SUPPORTED_ALGORITHMS
        .all
        .iter()
        .map(|alg| Box::leak(Box::new(Recording(*alg))) as &'static dyn SignatureVerificationAlgorithm)
        .collect();
    let mut provider = provider::provider(&TEST_RANDOM);
    provider.signature_verification_algorithms = WebPkiSupportedAlgorithms {
        all: Box::leak(all.into_boxed_slice()),
        mapping: sign_verify::SUPPORTED_ALGORITHMS.mapping,
    };
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(provider)).build().unwrap()
}

fn verify(chain: &[&[u8]], name: &str, at: u64) -> Result<(), rustls::Error> {
    let certs: Vec<CertificateDer<'_>> = chain.iter().map(|der| CertificateDer::from(*der)).collect();
    let name = ServerName::try_from(name).unwrap();
    verifier()
        .verify_server_cert(&certs[0], &certs[1..], &name, &[], UnixTime::since_unix_epoch(Duration::from_secs(at)))
        .map(|_| ())
}

#[test]
fn real_chains_verify() {
    let mut used = BTreeSet::new();
    for capture in [&DEEPGRAM, &EXAMPLE, &GOOGLE] {
        for name in capture.names {
            USED.with(|u| u.borrow_mut().clear());
            verify(capture.chain, name, CAPTURED).unwrap_or_else(|e| panic!("{name}: {e}"));
            USED.with(|u| used.extend(u.borrow().iter().cloned()));
        }
    }
    // RSA 2048 and 4096 with PKCS#1 v1.5, ECDSA on both curves.
    for alg in ["RSA_PKCS1_SHA256", "ECDSA_P256_SHA256", "ECDSA_P384_SHA384"] {
        assert!(used.contains(alg), "{alg} not used: {used:?}");
    }
}

#[test]
fn real_chains_reject_what_they_should() {
    for capture in [&DEEPGRAM, &EXAMPLE, &GOOGLE] {
        let name = capture.names[0];
        let reject = |chain: &[&[u8]], name: &str, at: u64| -> CertificateError {
            match verify(chain, name, at) {
                Err(rustls::Error::InvalidCertificate(e)) => e,
                other => panic!("{name}: {other:?}"),
            }
        };
        // Another name.
        assert!(matches!(
            reject(capture.chain, "deepgram.example.org", CAPTURED),
            CertificateError::NotValidForNameContext { .. }
        ));
        // A year later the leaf has expired; a year earlier it did not exist.
        assert!(matches!(reject(capture.chain, name, CAPTURED + 365 * DAY), CertificateError::ExpiredContext { .. }));
        assert!(matches!(
            reject(capture.chain, name, CAPTURED - 365 * DAY),
            CertificateError::NotValidYetContext { .. }
        ));
        // Without the intermediates, nothing leads to a root.
        assert_eq!(reject(&capture.chain[..1], name, CAPTURED), CertificateError::UnknownIssuer);
        // A damaged signature on the leaf.
        let mut leaf = capture.chain[0].to_vec();
        let last = leaf.len() - 3;
        leaf[last] ^= 0x40;
        let mut chain: Vec<&[u8]> = capture.chain.to_vec();
        chain[0] = &leaf;
        assert_eq!(reject(&chain, name, CAPTURED), CertificateError::BadSignature);
    }
}

#[test]
fn expiry_is_reported_with_dates() {
    // The deepgram certificate's end of validity, as the user sees it.
    let err = match verify(DEEPGRAM.chain, "api.deepgram.com", CAPTURED + 60 * DAY) {
        Err(e) => TlsError::from(e),
        Ok(()) => panic!("an expired certificate was accepted"),
    };
    assert_eq!(
        err,
        TlsError::BadCertificate(String::from("it expired on 2026-11-08 (the system clock reads 2026-12-02)"))
    );
}
