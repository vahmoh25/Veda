//! `vtls` — TLS for Vindows applications: HTTPS, secure WebSockets and any
//! other protocol over TLS 1.3 or TLS 1.2.
//!
//! ```ignore
//! let config = vtls::ClientConfig::with_alpn(&[b"http/1.1"]);
//! let mut tls = vtls::connect("example.com", 443, Duration::from_secs(20), &config)?;
//! tls.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")?;
//! let n = tls.read(&mut buf)?;
//! ```
//!
//! * [`TlsStream`] — a client connection over any [`Transport`]
//!   (`vnet::TcpStream` is one): blocking [`read`](TlsStream::read) and
//!   [`write_all`](TlsStream::write_all), and the non-blocking
//!   [`try_read`](TlsStream::try_read) for event loops, which wait for the
//!   TCP stream's handle to become readable.
//! * [`ClientConfig`] — what a client trusts and offers, shared between
//!   connections: by default the certificate authorities of the Mozilla
//!   root program (webpki-roots), TLS 1.3 and 1.2, no ALPN.
//! * [`connect`] — name lookup, TCP and the handshake in one call.
//!
//! The protocol is [rustls](https://github.com/rustls/rustls) 0.23, through
//! its `no_std` unbuffered API; certificates are verified by rustls-webpki.
//! The cryptography is this crate's rustls provider (`provider`), built on
//! pure-Rust crates: RustCrypto (AES-GCM, ChaCha20-Poly1305, SHA-2, HMAC,
//! P-256 and P-384), x25519-dalek, ed25519-dalek, and RSA signature
//! verification on `crypto-bigint`. Random numbers come from the kernel
//! ([`SYSTEM_RANDOM`]) and the time from the system clock ([`SystemClock`]);
//! both can be replaced, which the host tests do.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod config;
mod error;
mod provider;
mod stream;

use core::time::Duration;

pub use config::{ClientConfig, ClientConfigBuilder, SystemClock};
pub use error::TlsError;
pub use provider::{RandomSource, SYSTEM_RANDOM};
pub use rustls::pki_types::UnixTime;
pub use rustls::time_provider::TimeProvider;
pub use stream::{TlsStream, Transport};

/// Resolves `host`, connects to it over TCP and runs the TLS handshake,
/// verifying the certificate for `host`, all within about `timeout`.
///
/// The returned stream's TCP connection has no read or write timeout; set
/// them through [`TlsStream::transport_mut`] if needed.
pub fn connect(
    host: &str,
    port: u16,
    timeout: Duration,
    config: &ClientConfig,
) -> Result<TlsStream<vnet::TcpStream>, TlsError> {
    let start = vrt::time::Instant::now();
    let mut tcp = vnet::TcpStream::connect_host(host, port, timeout)?;
    // The handshake gets what is left of the time (at least a second).
    let left = timeout.saturating_sub(start.elapsed()).max(Duration::from_secs(1));
    tcp.set_read_timeout(Some(left));
    tcp.set_write_timeout(Some(left));
    let mut tls = TlsStream::connect(tcp, host, config)?;
    tls.transport_mut().set_read_timeout(None);
    tls.transport_mut().set_write_timeout(None);
    Ok(tls)
}

#[cfg(test)]
mod tests;
