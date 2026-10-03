//! Handshakes against a rustls server in memory: every cipher suite, key
//! exchange group and certificate type, data in both directions, closing,
//! key updates, rejected certificates and broken servers.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use std::io::Read;

use rustls::{AlertDescription, SignatureScheme, SupportedCipherSuite};

use super::support::*;
use crate::provider::kx::Group;
use crate::provider::{self as p};
use crate::{ClientConfig, TlsError, TlsStream, Transport};

fn connect(server: &mut TestServer) -> Result<TlsStream<&mut TestServer>, TlsError> {
    connect_with(server, &client_config().build())
}

fn connect_with<'a>(
    server: &'a mut TestServer,
    config: &ClientConfig,
) -> Result<TlsStream<&'a mut TestServer>, TlsError> {
    TlsStream::connect(server, TEST_NAME, config)
}

/// Reads exactly `len` bytes.
fn read_exact(tls: &mut TlsStream<&mut TestServer>, len: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4000];
    while got.len() < len {
        let n = tls.read(&mut buf[..(len - got.len()).min(4000)]).unwrap();
        assert!(n > 0, "end of stream after {} of {len} bytes", got.len());
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// Data both ways (the server echoes), more than a record's worth, then a
/// clean close.
fn exercise(tls: &mut TlsStream<&mut TestServer>, big: usize) {
    tls.write_all(b"hello").unwrap();
    assert_eq!(read_exact(tls, 5), b"hello");
    let data: Vec<u8> = (0..big as u32).map(|i| (i % 251) as u8).collect();
    tls.write_all(&data).unwrap();
    assert_eq!(read_exact(tls, big), data);
    let mut buf = [0u8; 16];
    assert_eq!(tls.try_read(&mut buf), Ok(None));
    tls.close();
    assert_eq!(tls.write_all(b"more"), Err(TlsError::Closed));
}

/// After `exercise`: the server got everything and the close_notify.
fn check_server_end(server: &mut TestServer, big: usize) {
    assert!(server.error.is_none(), "server error: {:?}", server.error);
    assert_eq!(server.received.len(), 5 + big);
    let mut buf = [0u8; 1];
    assert_eq!(server.conn.reader().read(&mut buf).unwrap(), 0, "no close_notify");
}

const TLS13_SUITES: [(SupportedCipherSuite, &str); 3] = [
    (p::TLS13_AES_128_GCM_SHA256, "TLS13_AES_128_GCM_SHA256"),
    (p::TLS13_AES_256_GCM_SHA384, "TLS13_AES_256_GCM_SHA384"),
    (p::TLS13_CHACHA20_POLY1305_SHA256, "TLS13_CHACHA20_POLY1305_SHA256"),
];

#[test]
fn tls13_every_cipher_suite() {
    for (suite, name) in TLS13_SUITES {
        let mut server = ServerSetup::new().suite(suite).start();
        let mut tls = connect(&mut server).unwrap();
        assert_eq!((tls.protocol_version(), tls.cipher_suite(), tls.key_exchange_group()), ("TLS 1.3", name, "X25519"));
        exercise(&mut tls, 70_000);
        drop(tls);
        check_server_end(&mut server, 70_000);
    }
}

#[test]
fn tls12_every_cipher_suite() {
    let ecdsa = [
        (p::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"),
        (p::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384, "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"),
        (p::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"),
    ];
    let rsa = [
        (p::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256, "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"),
        (p::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"),
        (p::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256, "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"),
    ];
    let mut runs = Vec::new();
    for (suite, name) in ecdsa {
        runs.push((suite, name, ServerSetup::new()));
    }
    for (suite, name) in rsa {
        runs.push((suite, name, ServerSetup::new().chain(&["leaf-rsa"], TestKey::rsa(RSA_SCHEMES))));
    }
    for (suite, name, setup) in runs {
        let mut server = setup.suite(suite).start();
        let mut tls = connect(&mut server).unwrap();
        assert_eq!((tls.protocol_version(), tls.cipher_suite()), ("TLS 1.2", name));
        exercise(&mut tls, 40_000);
        drop(tls);
        check_server_end(&mut server, 40_000);
    }
}

#[test]
fn every_key_exchange_group() {
    for (group, name) in [(Group::X25519, "X25519"), (Group::Secp256r1, "secp256r1"), (Group::Secp384r1, "secp384r1")] {
        for versions in [TLS13, TLS12] {
            // The client's first key share is X25519: TLS 1.3 servers that
            // only take a NIST curve ask for another (HelloRetryRequest).
            let mut server = ServerSetup::new().group(group).versions(versions).start();
            let mut tls = connect(&mut server).unwrap();
            assert_eq!(tls.key_exchange_group(), name);
            exercise(&mut tls, 1000);
            drop(tls);
            check_server_end(&mut server, 1000);
        }
    }
}

/// Every kind of certificate and handshake signature the provider verifies.
#[test]
fn every_certificate_type() {
    let v13 = TLS13;
    let v12 = TLS12;
    let runs: Vec<(&str, ServerSetup)> = alloc::vec![
        // Certificate signature ECDSA P-256/SHA-384; CertificateVerify P-384.
        ("p384 tls1.3", ServerSetup::new().chain(&["leaf-p384"], TestKey::p384()).versions(v13)),
        ("p384 tls1.2", ServerSetup::new().chain(&["leaf-p384"], TestKey::p384()).versions(v12)),
        // An intermediate (P-384) signing with SHA-384.
        ("chain", ServerSetup::new().chain(&["leaf-chain", "int-ecdsa"], TestKey::p256())),
        // Ed25519 handshake signatures, certificate signed P-384/SHA-256.
        ("ed25519 tls1.3", ServerSetup::new().chain(&["leaf-ed25519", "int-ecdsa"], TestKey::ed25519()).versions(v13)),
        ("ed25519 tls1.2", ServerSetup::new().chain(&["leaf-ed25519", "int-ecdsa"], TestKey::ed25519()).versions(v12)),
        // RSA: PSS in TLS 1.3, each PKCS#1 and PSS scheme in TLS 1.2; the
        // certificate is signed with PKCS#1/SHA-384.
        ("rsa tls1.3", ServerSetup::new().chain(&["leaf-rsa"], TestKey::rsa(RSA_SCHEMES)).versions(v13)),
        ("rsa-pss-sha256", ServerSetup::new().chain(&["leaf-rsa"], TestKey::rsa(&[SignatureScheme::RSA_PSS_SHA256]))),
        ("rsa-pss-sha384 tls1.2", rsa_only(SignatureScheme::RSA_PSS_SHA384).versions(v12)),
        ("rsa-pss-sha512 tls1.2", rsa_only(SignatureScheme::RSA_PSS_SHA512).versions(v12)),
        ("rsa-pkcs1-sha256 tls1.2", rsa_only(SignatureScheme::RSA_PKCS1_SHA256).versions(v12)),
        ("rsa-pkcs1-sha384 tls1.2", rsa_only(SignatureScheme::RSA_PKCS1_SHA384).versions(v12)),
        ("rsa-pkcs1-sha512 tls1.2", rsa_only(SignatureScheme::RSA_PKCS1_SHA512).versions(v12)),
        // A certificate signed with RSA-PSS.
        ("rsa-pss certificate", ServerSetup::new().chain(&["leaf-rsa-pss"], TestKey::rsa(RSA_SCHEMES))),
    ];
    for (what, setup) in runs {
        let mut server = setup.start();
        let mut tls = connect(&mut server).unwrap_or_else(|e| panic!("{what}: {e}"));
        exercise(&mut tls, 1000);
        drop(tls);
        check_server_end(&mut server, 1000);
    }
}

fn rsa_only(scheme: SignatureScheme) -> ServerSetup {
    let schemes: &'static [SignatureScheme] = Box::leak(Box::new([scheme]));
    ServerSetup::new().chain(&["leaf-rsa"], TestKey::rsa(schemes))
}

/// The certificates the client must refuse, the reason it gives, and the
/// alert the server receives.
#[test]
fn rejected_certificates() {
    let cases: [(&'static [&'static str], &str, AlertDescription); 5] = [
        (
            &["leaf-expired"],
            "it expired on 2025-01-01 (the system clock reads 2026-06-01)",
            AlertDescription::CertificateExpired,
        ),
        (&["leaf-future"], "it is only valid from 2030-01-01", AlertDescription::CertificateExpired),
        (&["leaf-other-name"], "it is not valid for test.vindows.local", AlertDescription::BadCertificate),
        (&["leaf-untrusted"], "not issued by a trusted certificate authority", AlertDescription::UnknownCA),
        (&["leaf-client-only"], "not meant for TLS servers", AlertDescription::UnsupportedCertificate),
    ];
    for (chain, reason, alert) in cases {
        for versions in [TLS13, TLS12] {
            let mut server = ServerSetup::new().chain(chain, TestKey::p256()).versions(versions).start();
            match connect(&mut server) {
                Err(TlsError::BadCertificate(why)) => assert!(why.contains(reason), "{chain:?}: {why}"),
                other => panic!("{chain:?}: {other:?}"),
            }
            assert_eq!(server.error, Some(rustls::Error::AlertReceived(alert)), "{chain:?}");
        }
    }
    // A chain missing its intermediate, and a client trusting nothing.
    let mut server = ServerSetup::new().chain(&["leaf-chain"], TestKey::p256()).start();
    assert!(matches!(connect(&mut server), Err(TlsError::BadCertificate(_))));
    let mut server = ServerSetup::new().start();
    let empty = client_config().trust_only(&[]).unwrap().build();
    let result = connect_with(&mut server, &empty).map(|_| ());
    assert!(matches!(result, Err(TlsError::BadCertificate(_))), "{result:?}");
    // The default configuration (the Mozilla roots) does not trust the test
    // root either.
    let mut server = ServerSetup::new().start();
    let defaults =
        ClientConfig::builder().random(&TEST_RANDOM).clock(alloc::sync::Arc::new(FixedClock(TEST_TIME))).build();
    let err = connect_with(&mut server, &defaults).unwrap_err();
    assert!(err.to_string().contains("not issued by a trusted certificate authority"), "{err}");
}

/// A failed handshake leaves the stream unusable, and says why.
#[test]
fn handshake_failures_are_errors() {
    // TLS 1.3 only against TLS 1.2 only, both ways.
    let mut server = ServerSetup::new().versions(TLS12).start();
    let config = client_config().tls13_only().build();
    assert!(matches!(connect_with(&mut server, &config), Err(TlsError::Tls(_))));
    let mut server = ServerSetup::new().versions(TLS13).start();
    let config = client_config().tls12_only().build();
    assert!(matches!(connect_with(&mut server, &config), Err(TlsError::Tls(_))));

    // Names that are neither DNS names nor IP addresses.
    let mut server = ServerSetup::new().start();
    for name in ["", "bad name", "a..b", "-"] {
        let result = TlsStream::connect(&mut server, name, &client_config().build());
        assert_eq!(result.unwrap_err(), TlsError::InvalidServerName(String::from(name)));
    }
}

#[test]
fn ip_address_names() {
    let mut server = ServerSetup::new().start();
    let mut tls = TlsStream::connect(&mut server, "127.0.0.1", &client_config().build()).unwrap();
    exercise(&mut tls, 100);
    let mut server = ServerSetup::new().start();
    match TlsStream::connect(&mut server, "127.0.0.2", &client_config().build()) {
        Err(TlsError::BadCertificate(why)) => assert!(why.contains("not valid for 127.0.0.2"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn alpn() {
    let offer = client_config().alpn(&[b"h2", b"http/1.1"]).build();
    let mut server = ServerSetup::new().alpn(&[b"http/1.1"]).start();
    let tls = connect_with(&mut server, &offer).unwrap();
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    // No ALPN offered: none chosen.
    let mut server = ServerSetup::new().alpn(&[b"http/1.1"]).start();
    assert_eq!(connect(&mut server).unwrap().alpn_protocol(), None);
    // Nothing in common: the server refuses.
    let mut server = ServerSetup::new().alpn(&[b"h2"]).start();
    let only_http1 = client_config().alpn(&[b"http/1.1"]).build();
    let err = connect_with(&mut server, &only_http1).unwrap_err();
    assert_eq!(err, TlsError::Tls(String::from("the server supports none of the requested application protocols")));
}

/// Records split across reads (down to a byte at a time), and many records
/// per read.
#[test]
fn records_in_pieces() {
    for chunk in [1, 2, 7, 300, 5000] {
        let mut server = ServerSetup::new().start();
        server.chunk = chunk;
        let mut tls = connect(&mut server).unwrap();
        let big = if chunk < 10 { 3000 } else { 40_000 };
        exercise(&mut tls, big);
        drop(tls);
        check_server_end(&mut server, big);
    }
    // Many small records arriving together.
    let mut server = ServerSetup::new().start();
    let mut tls = connect(&mut server).unwrap();
    for i in 0..200u8 {
        tls.write_all(&[i]).unwrap();
    }
    assert_eq!(read_exact(&mut tls, 200), (0..200u8).collect::<Vec<u8>>());
}

/// Data the server sends first, without being asked.
#[test]
fn server_speaks_first() {
    let mut server = ServerSetup::new().start();
    server.greeting = Some(b"220 welcome\r\n".to_vec());
    let mut tls = connect(&mut server).unwrap();
    let mut buf = [0u8; 64];
    let n = tls.try_read(&mut buf).unwrap().unwrap();
    assert_eq!(&buf[..n], b"220 welcome\r\n");
    assert_eq!(tls.try_read(&mut buf), Ok(None));
}

/// The server closes: close_notify ends the stream for reading; the
/// client may still write (half close) and then close too.
#[test]
fn server_closes() {
    for versions in [TLS13, TLS12] {
        let mut server = ServerSetup::new().versions(versions).start();
        let mut tls = connect(&mut server).unwrap();
        tls.write_all(b"last words").unwrap();
        tls.transport_mut().conn.send_close_notify();
        tls.transport_mut().flush();
        assert_eq!(read_exact(&mut tls, 10), b"last words");
        let mut buf = [0u8; 16];
        assert_eq!(tls.read(&mut buf), Ok(0));
        assert_eq!(tls.try_read(&mut buf), Ok(Some(0)));
        assert_eq!(tls.read(&mut buf), Ok(0));
        tls.write_all(b"after").unwrap();
        tls.close();
        drop(tls);
        assert!(server.received.ends_with(b"after"));
    }

    // The transport ends without close_notify: the end of the stream too.
    let mut server = ServerSetup::new().start();
    let mut tls = connect(&mut server).unwrap();
    tls.write_all(b"bye").unwrap();
    tls.transport_mut().hang_up = true;
    assert_eq!(read_exact(&mut tls, 3), b"bye");
    let mut buf = [0u8; 16];
    assert_eq!(tls.read(&mut buf), Ok(0));
    assert_eq!(tls.try_read(&mut buf), Ok(Some(0)));
}

/// A read with nothing to read times out (the transport's timeout), and
/// the connection stays usable.
#[test]
fn timeouts_are_not_fatal() {
    let mut server = ServerSetup::new().start();
    let mut tls = connect(&mut server).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(tls.read(&mut buf), Err(TlsError::TimedOut));
    assert_eq!(tls.try_read(&mut buf), Ok(None));
    exercise(&mut tls, 100);
}

/// TLS 1.3 key updates from the server, which asks the client to update
/// its keys too.
#[test]
fn key_updates() {
    let mut server = ServerSetup::new().versions(TLS13).start();
    let mut tls = connect(&mut server).unwrap();
    for round in 0..5u8 {
        let server = tls.transport_mut();
        server.conn.refresh_traffic_keys().unwrap();
        std::io::Write::write_all(&mut server.conn.writer(), &[round; 100]).unwrap();
        server.flush();
        assert_eq!(read_exact(&mut tls, 100), [round; 100]);
        // The client answered with its own key update: the server reads
        // what it sends now with the new keys.
        tls.write_all(&[round]).unwrap();
        assert_eq!(read_exact(&mut tls, 1), [round]);
    }
    drop(tls);
    assert!(server.error.is_none(), "{:?}", server.error);
}

/// A server whose first flight is damaged (one bit at a time): never a
/// panic, and an error unless the bit is one TLS does not protect: the
/// version field of record headers, which receivers ignore (RFC 8446
/// section 5.1; TLS 1.3 authenticates records with a fixed version).
#[test]
fn damaged_server_flights() {
    /// What happened to the flight of one run.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Damage {
        /// Not damaged (yet, or the flight was shorter than the offset).
        None,
        /// A record header's version field.
        Unprotected,
        Protected,
    }
    // The offsets of the version fields of the record headers of `flight`.
    fn version_fields(flight: &[u8]) -> Vec<usize> {
        let mut fields = Vec::new();
        let mut at = 0;
        while at + 5 <= flight.len() {
            fields.extend([at + 1, at + 2]);
            at += 5 + usize::from(u16::from_be_bytes([flight[at + 3], flight[at + 4]]));
        }
        fields
    }

    for versions in [TLS13, TLS12] {
        let mut damaged = 0;
        // The first flight of the P-256 server is under 800 bytes.
        for offset in 0..800 {
            let mut server = ServerSetup::new().versions(versions).start();
            let damage = alloc::rc::Rc::new(core::cell::Cell::new(Damage::None));
            let record = damage.clone();
            let mut first = true;
            server.tamper = Some(Box::new(move |flight: &mut Vec<u8>| {
                if core::mem::take(&mut first) && offset < flight.len() {
                    let kind =
                        if version_fields(flight).contains(&offset) { Damage::Unprotected } else { Damage::Protected };
                    record.set(kind);
                    flight[offset] ^= 1 << (offset % 8);
                }
            }));
            let result = connect(&mut server);
            match (result, damage.get()) {
                (Err(_), _) => damaged += 1,
                (Ok(_), Damage::Protected) => panic!("damage at offset {offset} went unnoticed"),
                (Ok(mut tls), _) => exercise(&mut tls, 100),
            }
        }
        assert!(damaged > 700, "only {damaged} runs failed");
    }
}

/// Servers that do not speak TLS, or stop in the middle of the handshake.
#[test]
fn broken_servers() {
    /// Answers anything with fixed bytes, then ends the stream.
    struct Canned(Vec<u8>, usize);
    impl Transport for Canned {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, TlsError> {
            Ok(self.try_read(buf)?.unwrap_or(0))
        }
        fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, TlsError> {
            let n = (self.0.len() - self.1).min(buf.len());
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(Some(n))
        }
        fn write_all(&mut self, _data: &[u8]) -> Result<(), TlsError> {
            Ok(())
        }
    }
    let config = client_config().build();
    for reply in [
        &b"HTTP/1.1 400 Bad Request\r\n\r\n"[..],
        &b""[..],
        &[0x16, 0x03, 0x03, 0x00, 0x05, 0x02, 0x00, 0x00, 0x01, 0x00][..],
        &[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28][..], // a fatal handshake_failure alert
        &[0x16, 0x03, 0x03, 0xff, 0xff][..],
    ] {
        let result = TlsStream::connect(Canned(reply.to_vec(), 0), TEST_NAME, &config);
        assert!(result.is_err(), "{reply:02x?}");
    }
    // Pseudo-random replies of every length up to a few hundred bytes.
    let mut noise = alloc::vec![0u8; 400];
    for len in 0..400 {
        test_fill(&mut noise);
        // Half of them look like the start of a handshake record.
        if len % 2 == 0 && len >= 5 {
            noise[..3].copy_from_slice(&[0x16, 0x03, 0x03]);
        }
        assert!(TlsStream::connect(Canned(noise[..len].to_vec(), 0), TEST_NAME, &config).is_err());
    }
    // The server hangs up in the middle of its first flight.
    let mut server = ServerSetup::new().start();
    server.hang_up = true;
    server.tamper = Some(Box::new(|b: &mut Vec<u8>| b.truncate(b.len() / 2)));
    assert_eq!(
        connect(&mut server).unwrap_err(),
        TlsError::Tls(String::from("the server closed the connection during the handshake"))
    );
}

/// Data damaged in transit breaks the connection: the client tells the
/// server why, and every later operation reports the error.
#[test]
fn errors_are_sticky() {
    for versions in [TLS13, TLS12] {
        let mut server = ServerSetup::new().versions(versions).start();
        let mut tls = connect(&mut server).unwrap();
        let server_side = tls.transport_mut();
        std::io::Write::write_all(&mut server_side.conn.writer(), b"data").unwrap();
        server_side.tamper = Some(Box::new(|b: &mut Vec<u8>| {
            let last = b.len() - 1;
            b[last] ^= 1;
        }));
        server_side.flush();
        let mut buf = [0u8; 16];
        let first = tls.read(&mut buf).unwrap_err();
        assert_eq!(first, TlsError::Tls(String::from("data from the server failed its integrity check")));
        assert_eq!(tls.read(&mut buf), Err(first.clone()));
        assert_eq!(tls.try_read(&mut buf), Err(first.clone()));
        assert_eq!(tls.write_all(b"x"), Err(first.clone()));
        tls.close();
        drop(tls);
        assert_eq!(server.error, Some(rustls::Error::AlertReceived(AlertDescription::BadRecordMac)));
    }
}
