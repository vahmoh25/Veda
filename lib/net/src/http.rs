//! A small HTTP/1.1 client: `GET` over plain HTTP, following redirects,
//! with `Content-Length`, chunked and read-to-close bodies. Response heads
//! are parsed by `httparse`.
//!
//! HTTPS needs TLS, which a later version adds on top of
//! [`crate::TcpStream`]; `https://` URLs are refused with
//! [`HttpError::TlsUnsupported`] rather than silently fetched in the clear.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vproto::net::NetError;
use vrt::time::Duration;

use crate::TcpStream;

/// Largest response head.
const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Net(NetError),
    /// The URL is malformed.
    BadUrl,
    /// `https://` (TLS is not available yet).
    TlsUnsupported,
    /// The server sent something that is not valid HTTP.
    Protocol(&'static str),
    /// The body exceeds the caller's limit.
    TooLarge,
    TooManyRedirects,
}

impl From<NetError> for HttpError {
    fn from(e: NetError) -> Self {
        HttpError::Net(e)
    }
}

impl core::fmt::Display for HttpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HttpError::Net(e) => write!(f, "{e}"),
            HttpError::BadUrl => f.write_str("invalid URL"),
            HttpError::TlsUnsupported => f.write_str("HTTPS is not supported yet"),
            HttpError::Protocol(what) => write!(f, "invalid HTTP response ({what})"),
            HttpError::TooLarge => f.write_str("response too large"),
            HttpError::TooManyRedirects => f.write_str("too many redirects"),
        }
    }
}

/// A parsed `http://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, HttpError> {
        let url = url.trim();
        let rest = if let Some(r) = url.strip_prefix("http://") {
            r
        } else if url.starts_with("https://") {
            return Err(HttpError::TlsUnsupported);
        } else if url.contains("://") {
            return Err(HttpError::BadUrl);
        } else {
            url
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let path = path.split('#').next().unwrap_or("/");
        let path = if path.starts_with('?') { alloc::format!("/{path}") } else { String::from(path) };
        if authority.contains('@') || authority.is_empty() {
            return Err(HttpError::BadUrl);
        }
        let (host, port) = if let Some(h) = authority.strip_prefix('[') {
            // [IPv6]:port
            let end = h.find(']').ok_or(HttpError::BadUrl)?;
            let port = match &h[end + 1..] {
                "" => 80,
                p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or(HttpError::BadUrl)?,
            };
            (String::from(&h[..end]), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (String::from(h), p.parse().map_err(|_| HttpError::BadUrl)?),
                None => (String::from(authority), 80),
            }
        };
        if host.is_empty() || port == 0 || path.bytes().any(|b| b <= b' ' || b == 0x7F) {
            return Err(HttpError::BadUrl);
        }
        Ok(Url { host, port, path })
    }

    /// The `Host` header value.
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') { alloc::format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == 80 { host } else { alloc::format!("{host}:{}", self.port) }
    }
}

/// A response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The URL that answered (after redirects).
    pub url: String,
}

impl Response {
    /// The value of a header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Fetches `url` with `GET`, following up to five redirects. `limit` caps
/// the body size and `timeout` each network operation.
pub fn get(url: &str, timeout: Duration, limit: usize) -> Result<Response, HttpError> {
    let mut current = String::from(url);
    for _ in 0..=MAX_REDIRECTS {
        let parsed = Url::parse(&current)?;
        let resp = fetch(&parsed, timeout, limit)?;
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp.header("location")
        {
            current = if loc.starts_with("http://") || loc.starts_with("https://") {
                String::from(loc)
            } else if loc.starts_with('/') {
                alloc::format!("http://{}{}", parsed.host_header(), loc)
            } else {
                return Err(HttpError::Protocol("unsupported redirect"));
            };
            continue;
        }
        return Ok(Response { url: current, ..resp });
    }
    Err(HttpError::TooManyRedirects)
}

fn fetch(url: &Url, timeout: Duration, limit: usize) -> Result<Response, HttpError> {
    let mut s = TcpStream::connect_host(&url.host, url.port, timeout)?;
    s.set_read_timeout(Some(timeout));
    s.set_write_timeout(Some(timeout));
    let request = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: Vindows/{}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        url.path,
        url.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    s.write_all(request.as_bytes())?;
    // Read until the head is complete.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = alloc::vec![0u8; 16 * 1024];
    let (status, reason, headers, head_len) = loop {
        let mut hdrs = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut resp = httparse::Response::new(&mut hdrs);
        match resp.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => {
                let headers = resp
                    .headers
                    .iter()
                    .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned()))
                    .collect::<Vec<_>>();
                break (resp.code.unwrap_or(0), resp.reason.unwrap_or("").to_string(), headers, n);
            }
            Ok(httparse::Status::Partial) => {}
            Err(_) => return Err(HttpError::Protocol("malformed head")),
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpError::Protocol("head too large"));
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(HttpError::Protocol("connection closed before the head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let mut rest = buf.split_off(head_len);
    let find = |name: &str| {
        headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_ascii_lowercase())
    };
    let body = if (100..200).contains(&status) || status == 204 || status == 304 {
        Vec::new()
    } else if find("transfer-encoding").is_some_and(|t| t.ends_with("chunked")) {
        read_chunked(&mut s, rest, limit)?
    } else if let Some(len) = find("content-length") {
        let len: usize = len.parse().map_err(|_| HttpError::Protocol("bad Content-Length"))?;
        if len > limit {
            return Err(HttpError::TooLarge);
        }
        while rest.len() < len {
            let n = s.read(&mut chunk)?;
            if n == 0 {
                return Err(HttpError::Protocol("body shorter than Content-Length"));
            }
            rest.extend_from_slice(&chunk[..n]);
        }
        rest.truncate(len);
        rest
    } else {
        // Until the server closes the connection.
        if rest.len() > limit {
            return Err(HttpError::TooLarge);
        }
        let mut more = Vec::new();
        s.read_to_end(&mut more, limit - rest.len()).map_err(|e| match e {
            NetError::MessageTooLarge => HttpError::TooLarge,
            e => HttpError::Net(e),
        })?;
        rest.extend_from_slice(&more);
        rest
    };
    Ok(Response { status, reason, headers, body, url: String::new() })
}

/// Decodes a chunked body; `pending` holds bytes already read.
fn read_chunked(s: &mut TcpStream, mut pending: Vec<u8>, limit: usize) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    let mut chunk = alloc::vec![0u8; 16 * 1024];
    let mut more = |pending: &mut Vec<u8>| -> Result<(), HttpError> {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(HttpError::Protocol("truncated chunked body"));
        }
        pending.extend_from_slice(&chunk[..n]);
        Ok(())
    };
    loop {
        let (consumed, size) = loop {
            match httparse::parse_chunk_size(&pending) {
                Ok(httparse::Status::Complete((n, size))) => break (n, size),
                Ok(httparse::Status::Partial) => {
                    if pending.len() > 1024 {
                        return Err(HttpError::Protocol("bad chunk size"));
                    }
                    more(&mut pending)?;
                }
                Err(_) => return Err(HttpError::Protocol("bad chunk size")),
            }
        };
        pending.drain(..consumed);
        if size == 0 {
            return Ok(body);
        }
        let size = usize::try_from(size).map_err(|_| HttpError::TooLarge)?;
        if body.len().saturating_add(size) > limit {
            return Err(HttpError::TooLarge);
        }
        while pending.len() < size + 2 {
            more(&mut pending)?;
        }
        body.extend_from_slice(&pending[..size]);
        if &pending[size..size + 2] != b"\r\n" {
            return Err(HttpError::Protocol("chunk not terminated"));
        }
        pending.drain(..size + 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            Url::parse("http://example.com"),
            Ok(Url { host: "example.com".into(), port: 80, path: "/".into() })
        );
        assert_eq!(
            Url::parse("example.com:8080/a/b?q=1#frag"),
            Ok(Url { host: "example.com".into(), port: 8080, path: "/a/b?q=1".into() })
        );
        assert_eq!(
            Url::parse("http://[2001:db8::1]:81/x"),
            Ok(Url { host: "2001:db8::1".into(), port: 81, path: "/x".into() })
        );
        assert_eq!(Url::parse("http://h?x=1").unwrap().path, "/?x=1");
        assert_eq!(Url::parse("https://example.com"), Err(HttpError::TlsUnsupported));
        assert_eq!(Url::parse("ftp://example.com"), Err(HttpError::BadUrl));
        assert_eq!(Url::parse("http://user@example.com/"), Err(HttpError::BadUrl));
        assert_eq!(Url::parse("http://example.com:0/"), Err(HttpError::BadUrl));
        assert_eq!(Url::parse("http://example.com/a b"), Err(HttpError::BadUrl));
        assert_eq!(Url::parse("http://"), Err(HttpError::BadUrl));
        assert_eq!(
            Url { host: "2001:db8::1".into(), port: 8080, path: "/".into() }.host_header(),
            "[2001:db8::1]:8080"
        );
    }
}
