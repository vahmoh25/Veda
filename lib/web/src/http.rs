//! HTTP/1.1 requests over a [`Connection`].
//!
//! One request per connection (`Connection: close`), which is all the
//! system's clients need (API calls and downloads). Response heads are
//! parsed by `httparse`; bodies may be `Content-Length`, chunked or
//! read-to-close, and are limited to a caller-chosen size.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vrt::time::Duration;

use crate::{Connection, Url, WebError};

/// Largest response head.
const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 64;

/// A response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first header called `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The body as text (invalid UTF-8 replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// `Ok(self)` for a 2xx status, otherwise [`WebError::Status`].
    pub fn ok(self) -> Result<Response, WebError> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(WebError::Status { code: self.status, body: self.text() })
        }
    }
}

/// Writes a request. `headers` must not contain line breaks (they are
/// rejected); `Host`, `Connection` and `Content-Length` are added.
pub fn write_request<C: Connection>(
    conn: &mut C,
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(), WebError> {
    let mut head = format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", url.path, url.host_header());
    let mut has_agent = false;
    for (k, v) in headers {
        if k.is_empty() || [k, v].iter().any(|s| s.contains(['\r', '\n'])) {
            return Err(WebError::Protocol("invalid request header".into()));
        }
        has_agent |= k.eq_ignore_ascii_case("user-agent");
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_agent {
        head.push_str("User-Agent: Veda/0.1\r\n");
    }
    if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    conn.write_all(head.as_bytes())?;
    if !body.is_empty() {
        conn.write_all(body)?;
    }
    Ok(())
}

/// Reads through a connection with a buffer of bytes that arrived early.
pub(crate) struct Reader<'a, C: Connection> {
    pub conn: &'a mut C,
    pub buf: Vec<u8>,
}

impl<C: Connection> Reader<'_, C> {
    /// Reads more into the buffer; `Ok(false)` at the end of the stream.
    pub fn fill(&mut self) -> Result<bool, WebError> {
        let mut chunk = [0u8; 4096];
        let n = self.conn.read(&mut chunk)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n > 0)
    }

    /// Reads until the buffer holds a complete head; returns its length.
    pub fn read_head(&mut self) -> Result<usize, WebError> {
        loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(i + 4);
            }
            if self.buf.len() > MAX_HEAD {
                return Err(WebError::TooLarge);
            }
            if !self.fill()? {
                return Err(if self.buf.is_empty() {
                    WebError::Closed
                } else {
                    WebError::Protocol("connection closed in the response head".into())
                });
            }
        }
    }
}

/// A parsed response head: status, headers and the head's length.
pub(crate) type Head = (u16, Vec<(String, String)>, usize);

/// Parses a response head.
pub(crate) fn parse_head(bytes: &[u8]) -> Result<Head, WebError> {
    let mut raw = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut resp = httparse::Response::new(&mut raw);
    match resp.parse(bytes) {
        Ok(httparse::Status::Complete(len)) => {
            let status = resp.code.ok_or_else(|| WebError::Protocol("no status".into()))?;
            let headers = resp
                .headers
                .iter()
                .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string()))
                .collect();
            Ok((status, headers, len))
        }
        Ok(httparse::Status::Partial) => Err(WebError::Protocol("incomplete head".into())),
        Err(e) => Err(WebError::Protocol(format!("{e}"))),
    }
}

/// Reads a response to a `method` request (bodies at most `max_body`
/// bytes).
pub fn read_response<C: Connection>(conn: &mut C, method: &str, max_body: usize) -> Result<Response, WebError> {
    let mut r = Reader { conn, buf: Vec::new() };
    loop {
        let head_len = r.read_head()?;
        let (status, headers, _) = parse_head(&r.buf[..head_len])?;
        r.buf.drain(..head_len);
        // Interim responses (100 Continue) precede the real one.
        if (100..200).contains(&status) {
            continue;
        }
        let find = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
        let no_body = method == "HEAD" || status == 204 || status == 304;
        let body = if no_body {
            Vec::new()
        } else if find("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
            read_chunked(&mut r, max_body)?
        } else if let Some(len) = find("content-length") {
            let len: usize = len.parse().map_err(|_| WebError::Protocol("bad Content-Length".into()))?;
            if len > max_body {
                return Err(WebError::TooLarge);
            }
            while r.buf.len() < len {
                if !r.fill()? {
                    return Err(WebError::Protocol("connection closed in the body".into()));
                }
            }
            r.buf.truncate(len);
            core::mem::take(&mut r.buf)
        } else {
            while r.fill()? {
                if r.buf.len() > max_body {
                    return Err(WebError::TooLarge);
                }
            }
            core::mem::take(&mut r.buf)
        };
        return Ok(Response { status, headers, body });
    }
}

fn read_chunked<C: Connection>(r: &mut Reader<'_, C>, max_body: usize) -> Result<Vec<u8>, WebError> {
    let mut body = Vec::new();
    loop {
        // Chunk size line.
        let line_end = loop {
            if let Some(i) = r.buf.windows(2).position(|w| w == b"\r\n") {
                break i;
            }
            if r.buf.len() > 1024 || !r.fill()? {
                return Err(WebError::Protocol("bad chunk".into()));
            }
        };
        let line = core::str::from_utf8(&r.buf[..line_end]).map_err(|_| WebError::Protocol("bad chunk".into()))?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| WebError::Protocol("bad chunk size".into()))?;
        r.buf.drain(..line_end + 2);
        if size == 0 {
            // Trailers end with an empty line; the connection closes after.
            return Ok(body);
        }
        if body.len() + size > max_body {
            return Err(WebError::TooLarge);
        }
        while r.buf.len() < size + 2 {
            if !r.fill()? {
                return Err(WebError::Protocol("connection closed in a chunk".into()));
            }
        }
        body.extend_from_slice(&r.buf[..size]);
        r.buf.drain(..size + 2);
    }
}

/// Connects to the server of `url`, sends one request and reads the
/// response.
pub fn fetch(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    max_body: usize,
) -> Result<Response, WebError> {
    let url = Url::parse(url)?;
    let mut conn = crate::connect(&url, timeout)?;
    conn.set_read_timeout(Some(timeout));
    write_request(&mut conn, method, &url, headers, body)?;
    read_response(&mut conn, method, max_body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testconn::TestConn;

    #[test]
    fn writes_requests() {
        let url = Url::parse("https://api.deepgram.com/v1/models?x=1").unwrap();
        let mut c = TestConn::new(b"", 1);
        write_request(
            &mut c,
            "POST",
            &url,
            &[("Authorization", "Token abc"), ("Content-Type", "application/json")],
            b"{}",
        )
        .unwrap();
        let text = String::from_utf8(c.output).unwrap();
        assert!(text.starts_with("POST /v1/models?x=1 HTTP/1.1\r\nHost: api.deepgram.com\r\n"));
        assert!(text.contains("Authorization: Token abc\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("Connection: close\r\n\r\n{}"));
        let mut c = TestConn::new(b"", 1);
        assert!(write_request(&mut c, "GET", &url, &[("X", "a\r\nInjected: 1")], b"").is_err());
    }

    #[test]
    fn reads_bodies_of_every_kind() {
        for chunk in [1, 3, 64] {
            let mut c = TestConn::new(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b \r\n\r\nhelloEXTRA", chunk);
            let r = read_response(&mut c, "GET", 100).unwrap();
            assert_eq!((r.status, r.body.as_slice(), r.header("x-a")), (200, b"hello".as_slice(), Some("b")));

            let mut c = TestConn::new(
                b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n4;ext\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
                chunk,
            );
            let r = read_response(&mut c, "POST", 100).unwrap();
            assert_eq!((r.status, r.body.as_slice()), (201, b"Wikipedia".as_slice()));

            let mut c = TestConn::new(b"HTTP/1.0 401 Unauthorized\r\n\r\n{\"err\":\"bad key\"}", chunk);
            let r = read_response(&mut c, "GET", 100).unwrap();
            assert_eq!(r.status, 401);
            assert!(matches!(r.ok(), Err(WebError::Status { code: 401, .. })));
        }
    }

    #[test]
    fn rejects_bad_responses() {
        let mut c = TestConn::new(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\nshort", 7);
        assert!(matches!(read_response(&mut c, "GET", 1000), Err(WebError::Protocol(_))));
        let mut c = TestConn::new(b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n", 7);
        assert_eq!(read_response(&mut c, "GET", 100), Err(WebError::TooLarge));
        let mut c = TestConn::new(b"garbage\r\n\r\n", 7);
        assert!(matches!(read_response(&mut c, "GET", 100), Err(WebError::Protocol(_))));
        let mut c = TestConn::new(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 7);
        assert!(matches!(read_response(&mut c, "GET", 100), Err(WebError::Protocol(_))));
        let mut c = TestConn::new(b"", 7);
        assert_eq!(read_response(&mut c, "GET", 100), Err(WebError::Closed));
    }
}
