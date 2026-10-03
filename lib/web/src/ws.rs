//! A WebSocket client (RFC 6455).
//!
//! [`WebSocket::handshake`] upgrades an HTTP connection; then text and
//! binary messages flow both ways. Frames from the client are masked with
//! a fresh random key; frames from the server must not be. Pings are
//! answered at once and pongs swallowed, so callers only see data and the
//! close. Fragmented messages are reassembled up to a size limit, and text
//! must be UTF-8.
//!
//! For event loops, wait on [`WebSocket::handle`] and call
//! [`WebSocket::poll`] (which never blocks) until it returns `None`.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use sha1::{Digest, Sha1};
use vabi::RawHandle;
use vrt::time::Duration;

use crate::http::{Reader, parse_head};
use crate::{Conn, Connection, Url, WebError, base64, random_bytes};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Default limit for one message.
pub const DEFAULT_MAX_MESSAGE: usize = 16 << 20;

const OP_CONTINUATION: u8 = 0;
const OP_TEXT: u8 = 1;
const OP_BINARY: u8 = 2;
const OP_CLOSE: u8 = 8;
const OP_PING: u8 = 9;
const OP_PONG: u8 = 10;

/// Close codes.
pub mod close_code {
    pub const NORMAL: u16 = 1000;
    pub const GOING_AWAY: u16 = 1001;
    pub const PROTOCOL_ERROR: u16 = 1002;
    pub const INVALID_DATA: u16 = 1007;
    pub const TOO_BIG: u16 = 1009;
}

/// A message from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    /// The server closed the connection (code 1005 if it gave none).
    Close {
        code: u16,
        reason: String,
    },
}

/// One parsed frame.
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// An open WebSocket connection.
pub struct WebSocket<C: Connection> {
    conn: C,
    rx: Vec<u8>,
    /// A fragmented message being received: opcode and data so far.
    partial: Option<(u8, Vec<u8>)>,
    close_sent: bool,
    close_received: bool,
    max_message: usize,
}

/// `base64(SHA-1(key + GUID))`, the value the server must answer with.
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    base64::encode(&h.finalize())
}

impl WebSocket<Conn> {
    /// Connects to a `ws://` or `wss://` URL and performs the handshake.
    pub fn connect(url: &str, headers: &[(&str, &str)], timeout: Duration) -> Result<WebSocket<Conn>, WebError> {
        let url = Url::parse(url)?;
        if !matches!(url.scheme.as_str(), "ws" | "wss") {
            return Err(WebError::BadUrl);
        }
        let mut conn = crate::connect(&url, timeout)?;
        conn.set_read_timeout(Some(timeout));
        WebSocket::handshake(conn, &url, headers)
    }
}

impl<C: Connection> WebSocket<C> {
    /// Sends the opening handshake on `conn` and checks the answer.
    pub fn handshake(mut conn: C, url: &Url, headers: &[(&str, &str)]) -> Result<WebSocket<C>, WebError> {
        let mut nonce = [0u8; 16];
        random_bytes(&mut nonce);
        let key = base64::encode(&nonce);
        let mut all: Vec<(&str, &str)> =
            vec![("Upgrade", "websocket"), ("Connection", "Upgrade"), ("Sec-WebSocket-Version", "13")];
        all.push(("Sec-WebSocket-Key", &key));
        all.extend_from_slice(headers);
        let mut head = format!("GET {} HTTP/1.1\r\nHost: {}\r\n", url.path, url.host_header());
        for (k, v) in &all {
            if k.is_empty() || [k, v].iter().any(|s| s.contains(['\r', '\n'])) {
                return Err(WebError::Protocol("invalid request header".into()));
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        conn.write_all(head.as_bytes())?;
        let mut r = Reader { conn: &mut conn, buf: Vec::new() };
        let head_len = r.read_head()?;
        let (status, resp_headers, _) = parse_head(&r.buf[..head_len])?;
        let find = |name: &str| resp_headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
        if status != 101 {
            // Read whatever error the server explains itself with.
            let mut body = r.buf.split_off(head_len);
            let wanted = find("content-length").and_then(|l| l.parse::<usize>().ok()).unwrap_or(0).min(4096);
            while body.len() < wanted {
                let mut chunk = [0u8; 1024];
                match r.conn.read(&mut chunk) {
                    Ok(n) if n > 0 => body.extend_from_slice(&chunk[..n]),
                    _ => break,
                }
            }
            return Err(WebError::Status { code: status, body: String::from_utf8_lossy(&body).into_owned() });
        }
        let upgrade_ok = find("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        let connection_ok = find("connection").is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
        if !upgrade_ok || !connection_ok || find("sec-websocket-accept").as_deref() != Some(accept_key(&key).as_str()) {
            return Err(WebError::Protocol("the server did not accept the WebSocket upgrade".into()));
        }
        // Frames the server sent right after the head.
        let rx = r.buf.split_off(head_len);
        Ok(WebSocket {
            conn,
            rx,
            partial: None,
            close_sent: false,
            close_received: false,
            max_message: DEFAULT_MAX_MESSAGE,
        })
    }

    /// Limits the size of one received message.
    pub fn set_max_message(&mut self, bytes: usize) {
        self.max_message = bytes;
    }

    pub fn connection(&self) -> &C {
        &self.conn
    }

    pub fn connection_mut(&mut self) -> &mut C {
        &mut self.conn
    }

    /// Readable when data arrived (for event loops).
    pub fn handle(&self) -> RawHandle {
        self.conn.handle()
    }

    /// True once the server's close frame arrived or ours was sent.
    pub fn is_closed(&self) -> bool {
        self.close_received || self.close_sent
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebError> {
        if self.close_sent {
            return Err(WebError::Closed);
        }
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        random_bytes(&mut mask);
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        self.conn.write_all(&frame)
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), WebError> {
        self.send_frame(OP_TEXT, text.as_bytes())
    }

    pub fn send_binary(&mut self, data: &[u8]) -> Result<(), WebError> {
        self.send_frame(OP_BINARY, data)
    }

    pub fn ping(&mut self, data: &[u8]) -> Result<(), WebError> {
        self.send_frame(OP_PING, &data[..data.len().min(125)])
    }

    /// Starts the closing handshake (or answers the server's).
    pub fn close(&mut self, code: u16, reason: &str) -> Result<(), WebError> {
        if self.close_sent {
            return Ok(());
        }
        let mut payload = Vec::from(code.to_be_bytes());
        payload.extend(reason.as_bytes().iter().take(120));
        let r = self.send_frame(OP_CLOSE, &payload);
        self.close_sent = true;
        r
    }

    /// Fails the connection: sends a close with `code` and reports `why`.
    fn fail(&mut self, code: u16, why: &str) -> WebError {
        let _ = self.close(code, "");
        WebError::Protocol(why.into())
    }

    /// Parses one frame from the receive buffer, if complete.
    fn take_frame(&mut self) -> Result<Option<Frame>, WebError> {
        let b = &self.rx;
        if b.len() < 2 {
            return Ok(None);
        }
        let fin = b[0] & 0x80 != 0;
        let opcode = b[0] & 0x0F;
        if b[0] & 0x70 != 0 {
            return Err(self.fail(close_code::PROTOCOL_ERROR, "reserved frame bits set"));
        }
        if b[1] & 0x80 != 0 {
            return Err(self.fail(close_code::PROTOCOL_ERROR, "masked frame from the server"));
        }
        let (len, mut at) = match b[1] & 0x7F {
            126 => {
                if b.len() < 4 {
                    return Ok(None);
                }
                (u16::from_be_bytes([b[2], b[3]]) as u64, 4)
            }
            127 => {
                if b.len() < 10 {
                    return Ok(None);
                }
                let mut n = [0u8; 8];
                n.copy_from_slice(&b[2..10]);
                (u64::from_be_bytes(n), 10)
            }
            n => (n as u64, 2),
        };
        let control = opcode >= 8;
        if control && (!fin || len > 125) {
            return Err(self.fail(close_code::PROTOCOL_ERROR, "bad control frame"));
        }
        if !matches!(opcode, OP_CONTINUATION | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG) {
            return Err(self.fail(close_code::PROTOCOL_ERROR, "unknown frame type"));
        }
        if len > self.max_message as u64 {
            return Err(self.fail(close_code::TOO_BIG, "message too large"));
        }
        let len = len as usize;
        if self.rx.len() < at + len {
            return Ok(None);
        }
        let payload = self.rx[at..at + len].to_vec();
        at += len;
        self.rx.drain(..at);
        Ok(Some(Frame { fin, opcode, payload }))
    }

    /// Turns frames into messages; handles control frames.
    fn next_message(&mut self) -> Result<Option<Message>, WebError> {
        while let Some(f) = self.take_frame()? {
            match f.opcode {
                OP_PING => self.send_frame(OP_PONG, &f.payload)?,
                OP_PONG => {}
                OP_CLOSE => {
                    self.close_received = true;
                    let (code, reason) = if f.payload.len() >= 2 {
                        let code = u16::from_be_bytes([f.payload[0], f.payload[1]]);
                        (code, String::from_utf8_lossy(&f.payload[2..]).into_owned())
                    } else {
                        (1005, String::new())
                    };
                    let _ = self.close(close_code::NORMAL, "");
                    return Ok(Some(Message::Close { code, reason }));
                }
                OP_TEXT | OP_BINARY => {
                    if self.partial.is_some() {
                        return Err(self.fail(close_code::PROTOCOL_ERROR, "new message inside a fragmented one"));
                    }
                    if f.fin {
                        return self.finish(f.opcode, f.payload).map(Some);
                    }
                    self.partial = Some((f.opcode, f.payload));
                }
                _ => {
                    // Continuation.
                    let Some((op, mut data)) = self.partial.take() else {
                        return Err(self.fail(close_code::PROTOCOL_ERROR, "continuation without a message"));
                    };
                    if data.len() + f.payload.len() > self.max_message {
                        return Err(self.fail(close_code::TOO_BIG, "message too large"));
                    }
                    data.extend_from_slice(&f.payload);
                    if f.fin {
                        return self.finish(op, data).map(Some);
                    }
                    self.partial = Some((op, data));
                }
            }
        }
        Ok(None)
    }

    fn finish(&mut self, opcode: u8, data: Vec<u8>) -> Result<Message, WebError> {
        if opcode == OP_TEXT {
            match String::from_utf8(data) {
                Ok(s) => Ok(Message::Text(s)),
                Err(_) => Err(self.fail(close_code::INVALID_DATA, "text message is not UTF-8")),
            }
        } else {
            Ok(Message::Binary(data))
        }
    }

    /// Non-blocking: the next complete message, if one has arrived.
    /// `Err(Closed)` once the connection is gone.
    pub fn poll(&mut self) -> Result<Option<Message>, WebError> {
        if let Some(m) = self.next_message()? {
            return Ok(Some(m));
        }
        if self.close_received {
            return Err(WebError::Closed);
        }
        let mut chunk = [0u8; 8192];
        // Read what is there (bounded, so one busy connection cannot
        // starve the caller's other work).
        for _ in 0..64 {
            match self.conn.try_read(&mut chunk)? {
                Some(0) => {
                    return match self.next_message()? {
                        Some(m) => Ok(Some(m)),
                        None => Err(WebError::Closed),
                    };
                }
                Some(n) => {
                    self.rx.extend_from_slice(&chunk[..n]);
                    if let Some(m) = self.next_message()? {
                        return Ok(Some(m));
                    }
                }
                None => break,
            }
        }
        Ok(None)
    }

    /// Blocking: waits (up to the connection's read timeout) for the next
    /// message.
    pub fn read(&mut self) -> Result<Message, WebError> {
        loop {
            if let Some(m) = self.next_message()? {
                return Ok(m);
            }
            if self.close_received {
                return Err(WebError::Closed);
            }
            let mut chunk = [0u8; 8192];
            let n = self.conn.read(&mut chunk)?;
            if n == 0 {
                return Err(WebError::Closed);
            }
            self.rx.extend_from_slice(&chunk[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testconn::TestConn;
    use alloc::string::ToString;

    /// A server frame (unmasked).
    fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![(if fin { 0x80 } else { 0 }) | opcode];
        if payload.len() < 126 {
            f.push(payload.len() as u8);
        } else if payload.len() <= 0xFFFF {
            f.push(126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            f.push(127);
            f.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        f.extend_from_slice(payload);
        f
    }

    /// Decodes the client's (masked) frames.
    fn client_frames(mut b: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        while b.len() >= 2 {
            assert!(b[1] & 0x80 != 0, "client frames must be masked");
            let (len, mut at) = match b[1] & 0x7F {
                126 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
                127 => (u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize, 10),
                n => (n as usize, 2),
            };
            let mask = [b[at], b[at + 1], b[at + 2], b[at + 3]];
            at += 4;
            let payload = b[at..at + len].iter().enumerate().map(|(i, x)| x ^ mask[i & 3]).collect();
            out.push((b[0] & 0x0F, payload));
            b = &b[at + len..];
        }
        out
    }

    /// A socket that completed the handshake, with `server` bytes to read.
    fn open(server: &[u8], chunk: usize) -> WebSocket<TestConn> {
        WebSocket {
            conn: TestConn::new(server, chunk),
            rx: Vec::new(),
            partial: None,
            close_sent: false,
            close_received: false,
            max_message: 1000,
        }
    }

    #[test]
    fn computes_the_accept_key() {
        // The example from RFC 6455 section 1.3.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn handshake_checks_the_answer() {
        let url = Url::parse("wss://agent.deepgram.com/v1/agent/converse").unwrap();
        // The test RNG makes the key predictable: compute it the same way.
        let mut probe = TestConn::new(b"", 1);
        // A first attempt to learn the key from the request.
        let res = WebSocket::handshake(&mut probe as &mut TestConn, &url, &[("Authorization", "Token k")]);
        assert!(res.is_err());
        let request = String::from_utf8(probe.output.clone()).unwrap();
        assert!(request.starts_with("GET /v1/agent/converse HTTP/1.1\r\nHost: agent.deepgram.com\r\n"));
        assert!(request.contains("Authorization: Token k\r\n"));
        let key = request.lines().find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ")).unwrap().to_string();
        assert_eq!(base64::decode(&key).unwrap().len(), 16);

        let wrong = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: nope\r\n\r\n";
        assert!(matches!(WebSocket::handshake(TestConn::new(wrong, 5), &url, &[]), Err(WebError::Protocol(_))));
        let denied = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 11\r\n\r\nbad api key";
        match WebSocket::handshake(TestConn::new(denied, 5), &url, &[]) {
            Err(WebError::Status { code, body }) => assert_eq!((code, body.as_str()), (401, "bad api key")),
            _ => panic!("expected a status error"),
        }
    }

    impl Connection for &mut TestConn {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, WebError> {
            (**self).read(buf)
        }
        fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, WebError> {
            (**self).try_read(buf)
        }
        fn write_all(&mut self, data: &[u8]) -> Result<(), WebError> {
            (**self).write_all(data)
        }
        fn set_read_timeout(&mut self, _t: Option<Duration>) {}
        fn handle(&self) -> RawHandle {
            0
        }
    }

    #[test]
    fn receives_messages_and_answers_pings() {
        let mut server = Vec::new();
        server.extend(frame(true, OP_TEXT, br#"{"type":"Welcome"}"#));
        server.extend(frame(true, OP_PING, b"hi"));
        server.extend(frame(false, OP_BINARY, &[1, 2]));
        server.extend(frame(true, OP_PONG, b""));
        server.extend(frame(true, OP_CONTINUATION, &[3]));
        server.extend(frame(true, OP_BINARY, &vec![7u8; 300]));
        server.extend(frame(true, OP_CLOSE, &[0x03, 0xE8, b'b', b'y', b'e']));
        for chunk in [1, 2, 7, 4096] {
            let mut ws = open(&server, chunk);
            let mut got = Vec::new();
            loop {
                match ws.poll() {
                    Ok(Some(m)) => got.push(m),
                    Ok(None) => {}
                    Err(WebError::Closed) => break,
                    Err(e) => panic!("{e}"),
                }
                if matches!(got.last(), Some(Message::Close { .. })) {
                    break;
                }
            }
            assert_eq!(got[0], Message::Text("{\"type\":\"Welcome\"}".into()));
            assert_eq!(got[1], Message::Binary(vec![1, 2, 3]));
            assert_eq!(got[2], Message::Binary(vec![7; 300]));
            assert_eq!(got[3], Message::Close { code: 1000, reason: "bye".into() });
            let sent = client_frames(&ws.conn.output);
            assert_eq!(sent[0], (OP_PONG, b"hi".to_vec()));
            assert_eq!(sent[1].0, OP_CLOSE);
            assert!(ws.is_closed());
        }
    }

    #[test]
    fn sends_masked_frames_of_every_length() {
        let mut ws = open(b"", 1);
        ws.send_text("KeepAlive").unwrap();
        ws.send_binary(&[5u8; 200]).unwrap();
        ws.send_binary(&vec![6u8; 70_000]).unwrap();
        let sent = client_frames(&ws.conn.output);
        assert_eq!(sent[0], (OP_TEXT, b"KeepAlive".to_vec()));
        assert_eq!(sent[1], (OP_BINARY, vec![5u8; 200]));
        assert_eq!(sent[2], (OP_BINARY, vec![6u8; 70_000]));
        ws.close(close_code::NORMAL, "done").unwrap();
        assert_eq!(ws.send_text("late"), Err(WebError::Closed));
    }

    #[test]
    fn rejects_protocol_violations() {
        let cases: Vec<Vec<u8>> = vec![
            vec![0x81, 0x80, 0, 0, 0, 0],             // masked by the server
            vec![0xC1, 0x00],                         // reserved bit
            frame(false, OP_PING, b"x"),              // fragmented control frame
            frame(true, 3, b""),                      // unknown opcode
            frame(true, OP_CONTINUATION, b"x"),       // continuation without a start
            frame(true, OP_TEXT, &[0xFF, 0xFE]),      // invalid UTF-8
            frame(true, OP_BINARY, &vec![0u8; 2000]), // over the 1000-byte limit
        ];
        for (i, c) in cases.iter().enumerate() {
            let mut ws = open(c, 3);
            let mut result = Ok(None);
            for _ in 0..10 {
                result = ws.poll();
                if result.is_err() {
                    break;
                }
            }
            assert!(matches!(result, Err(WebError::Protocol(_))), "case {i}: {result:?}");
            assert!(ws.close_sent, "case {i}: the client should close");
        }
    }
}
