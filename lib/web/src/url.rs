//! URLs for the web clients: `http`, `https`, `ws` and `wss`.

use alloc::format;
use alloc::string::{String, ToString};

use crate::WebError;

/// A parsed URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// `http`, `https`, `ws` or `wss` (lower case).
    pub scheme: String,
    /// Host name or address (IPv6 without brackets).
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl Url {
    /// Parses `scheme://host[:port][/path][?query]`. User information and
    /// fragments are not accepted (they have no use for these clients).
    pub fn parse(text: &str) -> Result<Url, WebError> {
        let (scheme, rest) = text.split_once("://").ok_or(WebError::BadUrl)?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" | "ws" => 80,
            "https" | "wss" => 443,
            _ => return Err(WebError::BadUrl),
        };
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') || path.contains('#') {
            return Err(WebError::BadUrl);
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or(WebError::BadUrl)?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| WebError::BadUrl)?,
                None if after.is_empty() => default_port,
                None => return Err(WebError::BadUrl),
            };
            (h, port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().map_err(|_| WebError::BadUrl)?),
                None => (authority, default_port),
            }
        };
        if host.is_empty()
            || port == 0
            || host.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '?' | '#'))
        {
            return Err(WebError::BadUrl);
        }
        let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
        if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(WebError::BadUrl);
        }
        Ok(Url { scheme, host: host.to_ascii_lowercase(), port, path })
    }

    /// True for `https` and `wss`.
    pub fn secure(&self) -> bool {
        matches!(self.scheme.as_str(), "https" | "wss")
    }

    /// The `Host` header value (the port only when it is not the default).
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default = if self.secure() { 443 } else { 80 };
        if self.port == default { host } else { format!("{host}:{}", self.port) }
    }

    /// Appends `key=value` to the query string, percent-encoding both.
    pub fn with_query(mut self, key: &str, value: &str) -> Url {
        self.path.push(if self.path.contains('?') { '&' } else { '?' });
        self.path.push_str(&encode_component(key));
        self.path.push('=');
        self.path.push_str(&encode_component(value));
        self
    }
}

impl core::fmt::Display for Url {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}://{}{}", self.scheme, self.host_header(), self.path)
    }
}

/// Percent-encodes everything except unreserved characters (RFC 3986).
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn parses_urls() {
        let u = Url::parse("wss://agent.deepgram.com/v1/agent/converse").unwrap();
        assert_eq!(
            (u.scheme.as_str(), u.host.as_str(), u.port, u.path.as_str()),
            ("wss", "agent.deepgram.com", 443, "/v1/agent/converse")
        );
        assert!(u.secure());
        assert_eq!(u.host_header(), "agent.deepgram.com");
        let u = Url::parse("http://10.0.2.2:8080").unwrap();
        assert_eq!((u.port, u.path.as_str(), u.host_header().as_str()), (8080, "/", "10.0.2.2:8080"));
        let u = Url::parse("HTTP://[::1]:81?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("::1", 81, "/?x=1"));
        assert_eq!(u.host_header(), "[::1]:81");
        assert_eq!(u.to_string(), "http://[::1]:81/?x=1");
        for bad in [
            "ftp://x",
            "http://",
            "http://a b/",
            "http://u@h/",
            "http://h:99999/",
            "http://h/#f",
            "nourl",
            "http://h:0/",
        ] {
            assert_eq!(Url::parse(bad), Err(WebError::BadUrl), "{bad}");
        }
    }

    #[test]
    fn encodes_query_components() {
        let u = Url::parse("https://api.deepgram.com/v1/speak")
            .unwrap()
            .with_query("model", "aura-2-thalia-en")
            .with_query("q", "a b&c/é");
        assert_eq!(u.path, "/v1/speak?model=aura-2-thalia-en&q=a%20b%26c%2F%C3%A9");
    }
}
