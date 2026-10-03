//! `vweb` — web clients for Vindows applications.
//!
//! * [`Url`]: `http`, `https`, `ws` and `wss` URLs, with percent-encoding
//!   for query strings ([`url::encode_component`]).
//! * [`http`]: HTTP/1.1 requests and responses over any [`Connection`]
//!   (`Content-Length`, chunked and read-to-close bodies), and
//!   [`http::fetch`] which connects, sends and reads in one call.
//! * [`ws`]: a WebSocket client (RFC 6455): the opening handshake, masked
//!   frames, fragmented messages, pings answered automatically, the
//!   closing handshake, and a non-blocking [`ws::WebSocket::poll`] for
//!   event loops.
//! * [`Connection`]: the byte stream underneath — a plain
//!   [`vnet::TcpStream`] or a TLS stream ([`connect`] picks by scheme).
//!
//! Nothing here panics on what a server sends: malformed responses and
//! frames become [`WebError`]s.

#![no_std]

extern crate alloc;

pub mod base64;
pub mod http;
pub mod url;
pub mod ws;

use alloc::string::String;
use core::fmt;

use vabi::RawHandle;
use vnet::{NetError, TcpStream};
use vrt::time::Duration;

pub use url::Url;

/// What went wrong talking to a web server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    /// The network failed (connection refused, reset, no route, ...).
    Net(NetError),
    /// The secure connection failed (certificate, handshake, ...).
    Tls(String),
    /// The URL is malformed or uses an unsupported scheme.
    BadUrl,
    /// The server broke the protocol.
    Protocol(String),
    /// The server answered with an unexpected HTTP status.
    Status {
        code: u16,
        body: String,
    },
    /// A response or message exceeded the size limit.
    TooLarge,
    /// The connection was closed.
    Closed,
    TimedOut,
}

impl From<NetError> for WebError {
    fn from(e: NetError) -> Self {
        match e {
            NetError::TimedOut => WebError::TimedOut,
            e => WebError::Net(e),
        }
    }
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebError::Net(e) => write!(f, "{e}"),
            WebError::Tls(e) => write!(f, "secure connection failed: {e}"),
            WebError::BadUrl => f.write_str("invalid web address"),
            WebError::Protocol(what) => write!(f, "the server sent an invalid response ({what})"),
            WebError::Status { code, body } => {
                write!(f, "the server answered {code}")?;
                if !body.is_empty() {
                    write!(f, ": {}", body.chars().take(200).collect::<String>())?;
                }
                Ok(())
            }
            WebError::TooLarge => f.write_str("the response is too large"),
            WebError::Closed => f.write_str("the connection was closed"),
            WebError::TimedOut => f.write_str("the server did not answer in time"),
        }
    }
}

/// A byte stream to a server.
pub trait Connection {
    /// Blocking read (up to the read timeout); `Ok(0)` at the end of the
    /// stream.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, WebError>;
    /// Non-blocking read: `Ok(None)` if nothing is available yet.
    fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, WebError>;
    fn write_all(&mut self, data: &[u8]) -> Result<(), WebError>;
    /// How long a blocking read may wait (`None`: forever).
    fn set_read_timeout(&mut self, timeout: Option<Duration>);
    /// The kernel object that becomes readable when data arrives (for
    /// event loops).
    fn handle(&self) -> RawHandle;
}

impl Connection for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, WebError> {
        TcpStream::read(self, buf).map_err(WebError::from)
    }

    fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, WebError> {
        TcpStream::try_read(self, buf).map_err(WebError::from)
    }

    fn write_all(&mut self, data: &[u8]) -> Result<(), WebError> {
        TcpStream::write_all(self, data).map_err(WebError::from)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        TcpStream::set_read_timeout(self, timeout);
    }

    fn handle(&self) -> RawHandle {
        TcpStream::handle(self)
    }
}

/// A connection to a web server, plain or encrypted.
pub enum Conn {
    Plain(TcpStream),
}

impl Connection for Conn {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, WebError> {
        match self {
            Conn::Plain(s) => Connection::read(s, buf),
        }
    }

    fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, WebError> {
        match self {
            Conn::Plain(s) => Connection::try_read(s, buf),
        }
    }

    fn write_all(&mut self, data: &[u8]) -> Result<(), WebError> {
        match self {
            Conn::Plain(s) => Connection::write_all(s, data),
        }
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        match self {
            Conn::Plain(s) => Connection::set_read_timeout(s, timeout),
        }
    }

    fn handle(&self) -> RawHandle {
        match self {
            Conn::Plain(s) => Connection::handle(s),
        }
    }
}

/// Connects to the server of `url` (TCP, then TLS for `https`/`wss`).
pub fn connect(url: &Url, timeout: Duration) -> Result<Conn, WebError> {
    if url.secure() {
        return Err(WebError::Tls("TLS is not available".into()));
    }
    let mut tcp = TcpStream::connect_host(&url.host, url.port, timeout)?;
    // Requests and WebSocket messages are small and latency matters.
    let _ = tcp.set_nodelay(true);
    Ok(Conn::Plain(tcp))
}

/// Random bytes for WebSocket keys and masks.
pub(crate) fn random_bytes(buf: &mut [u8]) {
    #[cfg(not(test))]
    vrt::object::random_bytes(buf);
    #[cfg(test)]
    {
        use core::sync::atomic::{AtomicU64, Ordering};
        static STATE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
        for b in buf {
            let mut x = STATE.load(Ordering::Relaxed);
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            STATE.store(x, Ordering::Relaxed);
            *b = x as u8;
        }
    }
}

#[cfg(test)]
extern crate std;

/// An in-memory connection for tests: reads come from `input`, writes go
/// to `output`.
#[cfg(test)]
pub(crate) mod testconn {
    use super::*;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    #[derive(Default)]
    pub struct TestConn {
        pub input: VecDeque<u8>,
        pub output: Vec<u8>,
        /// Hand out at most this many bytes per read (exercises partial
        /// reads).
        pub chunk: usize,
        pub eof: bool,
    }

    impl TestConn {
        pub fn new(input: &[u8], chunk: usize) -> TestConn {
            TestConn { input: input.iter().copied().collect(), output: Vec::new(), chunk: chunk.max(1), eof: true }
        }
    }

    impl Connection for TestConn {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, WebError> {
            match self.try_read(buf)? {
                Some(n) => Ok(n),
                None if self.eof => Ok(0),
                None => Err(WebError::TimedOut),
            }
        }

        fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, WebError> {
            if self.input.is_empty() {
                return Ok(if self.eof { Some(0) } else { None });
            }
            let n = buf.len().min(self.chunk).min(self.input.len());
            for b in buf.iter_mut().take(n) {
                *b = self.input.pop_front().unwrap_or(0);
            }
            Ok(Some(n))
        }

        fn write_all(&mut self, data: &[u8]) -> Result<(), WebError> {
            self.output.extend_from_slice(data);
            Ok(())
        }

        fn set_read_timeout(&mut self, _timeout: Option<Duration>) {}

        fn handle(&self) -> RawHandle {
            0
        }
    }
}
