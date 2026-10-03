//! Interoperability with real servers over the host's network (ignored by
//! default; run with `cargo test -p vtls -- --ignored --nocapture`).
//!
//! Each server gets an HTTP/1.1 request with the default configuration,
//! with TLS 1.2 only, with each cipher suite alone and with each key
//! exchange group alone. Every suite and group must work with at least one
//! of the servers (not every server offers every suite).

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use rustls::pki_types::UnixTime;
use rustls::time_provider::TimeProvider;
use sha2::{Digest, Sha256};

use crate::provider::kx::Group;
use crate::provider::{CIPHER_SUITES, RandomSource};
use crate::{ClientConfig, ClientConfigBuilder, TlsError, TlsStream, Transport};

/// The host's clock.
#[derive(Debug)]
struct HostClock;

impl TimeProvider for HostClock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).ok()?))
    }
}

/// Random bytes for talking to real servers: SHA-256 over a seed that
/// differs between runs, and a counter.
fn host_fill(buf: &mut [u8]) {
    static SEED: OnceLock<[u8; 32]> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = SEED.get_or_init(|| {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let local = 0u8;
        Sha256::new()
            .chain_update(now.as_nanos().to_le_bytes())
            .chain_update(std::process::id().to_le_bytes())
            .chain_update((&local as *const u8 as usize).to_le_bytes())
            .chain_update(format!("{:?}", Instant::now()))
            .finalize()
            .into()
    });
    for chunk in buf.chunks_mut(32) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let block = Sha256::new().chain_update(seed).chain_update(n.to_le_bytes()).finalize();
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}

static HOST_RANDOM: RandomSource = RandomSource::new(host_fill);

/// A TCP connection of the host.
struct Tcp(TcpStream);

fn io_error(e: std::io::Error) -> TlsError {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => TlsError::TimedOut,
        _ => TlsError::Io(e.to_string()),
    }
}

impl Transport for Tcp {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TlsError> {
        self.0.read(buf).map_err(io_error)
    }

    fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, TlsError> {
        self.0.set_nonblocking(true).map_err(io_error)?;
        let result = self.0.read(buf);
        self.0.set_nonblocking(false).map_err(io_error)?;
        match result {
            Ok(n) => Ok(Some(n)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(io_error(e)),
        }
    }

    fn write_all(&mut self, data: &[u8]) -> Result<(), TlsError> {
        self.0.write_all(data).map_err(io_error)
    }
}

fn base() -> ClientConfigBuilder {
    ClientConfig::builder().random(&HOST_RANDOM).clock(Arc::new(HostClock)).alpn(&[b"http/1.1"])
}

/// What one request showed.
struct Outcome {
    version: &'static str,
    suite: &'static str,
    group: &'static str,
    status: String,
    handshake: Duration,
}

/// Connects, sends `GET path`, reads the whole response.
fn fetch(host: &str, path: &str, config: &ClientConfig) -> Result<Outcome, TlsError> {
    let addr = (host, 443).to_socket_addrs().map_err(io_error)?.next().ok_or(TlsError::Io("no address".into()))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(15)).map_err(io_error)?;
    tcp.set_read_timeout(Some(Duration::from_secs(20))).map_err(io_error)?;
    let start = Instant::now();
    let mut tls = TlsStream::connect(Tcp(tcp), host, config)?;
    let handshake = start.elapsed();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: vtls-test\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = tls.read(&mut buf)?;
        if n == 0 || response.len() > 4 << 20 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
    }
    tls.close();
    let head = String::from_utf8_lossy(&response[..response.len().min(200)]).to_string();
    let status = head.lines().next().unwrap_or_default().to_string();
    if !status.starts_with("HTTP/1.1 ") {
        return Err(TlsError::Tls(format!("not an HTTP response: {head:?}")));
    }
    Ok(Outcome {
        version: tls.protocol_version(),
        suite: tls.cipher_suite(),
        group: tls.key_exchange_group(),
        status,
        handshake,
    })
}

#[test]
#[ignore = "connects to servers on the Internet"]
fn real_servers() {
    let targets = [
        ("agent.deepgram.com", "/", None),
        ("api.deepgram.com", "/v1/projects", Some("401")),
        ("example.com", "/", Some("200")),
        ("www.google.com", "/", Some("200")),
    ];
    let mut suites = BTreeSet::new();
    let mut groups = BTreeSet::new();
    let mut default_failures = Vec::new();
    for (host, path, expected) in targets {
        let mut configs: Vec<(String, ClientConfig)> = Vec::new();
        configs.push((String::from("default"), base().build()));
        configs.push((String::from("TLS 1.2"), base().tls12_only().build()));
        for suite in CIPHER_SUITES {
            let name = suite.suite();
            configs.push((
                format!("{:?}", name),
                base().adjust(move |p| p.cipher_suites.retain(|s| s.suite() == name)).build(),
            ));
        }
        for group in Group::ALL {
            for (label, tls12) in [("1.3", false), ("1.2", true)] {
                let name = super::support::named_group(group);
                let builder = base().adjust(move |p| p.kx_groups.retain(|g| g.name() == name));
                let builder = if tls12 { builder.tls12_only() } else { builder };
                configs.push((format!("{name:?} TLS {label}"), builder.build()));
            }
        }
        for (label, config) in configs {
            match fetch(host, path, &config) {
                Ok(o) => {
                    std::println!(
                        "{host:20} {label:48} ok: {} {} {} handshake {} ms, {}",
                        o.version,
                        o.suite,
                        o.group,
                        o.handshake.as_millis(),
                        o.status
                    );
                    if let Some(code) = expected {
                        assert!(o.status.contains(code), "{host}: {}", o.status);
                    }
                    suites.insert(o.suite);
                    groups.insert(o.group);
                }
                Err(e) => {
                    std::println!("{host:20} {label:48} FAILED: {e}");
                    if label == "default" || label == "TLS 1.2" {
                        default_failures.push(format!("{host} {label}: {e}"));
                    }
                }
            }
        }
    }
    std::println!("cipher suites that worked: {suites:?}");
    std::println!("groups that worked: {groups:?}");
    assert!(default_failures.is_empty(), "{default_failures:?}");
    for suite in CIPHER_SUITES {
        assert!(suites.contains(suite.suite().as_str().unwrap()), "{:?} never worked", suite.suite());
    }
    assert_eq!(groups.len(), 3, "{groups:?}");
}
