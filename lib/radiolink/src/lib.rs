//! The radio link: messages between the virtual Wi-Fi radio of a Veda
//! guest (`airlink`, in the driver VM, which makes it a radio of Linux's)
//! and the radio medium simulated on the host (`airsim`).
//!
//! The transport is a byte stream (a virtio-serial port on the guest side,
//! a TCP connection on the host side). Every message is
//!
//! ```text
//! length: u32 LE   (bytes that follow: type + body)
//! type:   u8
//! body:   [u8; length - 1]
//! ```
//!
//! [`Reader`] reassembles messages from arbitrary chunks of the stream and
//! refuses lengths above [`MAX_MESSAGE`], so a broken peer cannot make the
//! other side allocate without bound; after a bad length the stream is
//! unusable and the connection must be restarted.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::vec::Vec;

/// Version of this protocol (2: [`msg::LISTEN`]).
pub const VERSION: u16 = 2;
/// Largest message (type and body), enough for any 802.11 MPDU we send.
pub const MAX_MESSAGE: usize = 8192;

/// Message types.
pub mod msg {
    /// Guest: hello { version: u16, mac: [u8; 6] (zeros: assign one) }.
    pub const HELLO: u8 = 1;
    /// Guest: transmit { id: u32, no_ack: u8, frame }.
    pub const TX: u8 = 2;
    /// Guest: tune { channel: u8 }.
    pub const SET_CHANNEL: u8 = 3;
    /// Guest: transmitter and receiver { on: u8 }.
    pub const SET_POWER: u8 = 4;
    /// Guest: what the receiver hears { all: u8 }: the channel it is tuned
    /// to (0, the start), or every channel (1), each frame with its own,
    /// for a radio that filters itself (Linux's simulated one).
    pub const LISTEN: u8 = 5;
    /// Host: welcome { version: u16, mac: [u8; 6], channels: [u8] }.
    pub const HELLO_ACK: u8 = 0x81;
    /// Host: received { channel: u8, signal_dbm: i8, frame }.
    pub const RX: u8 = 0x82;
    /// Host: transmit status { id: u32, acked: u8 }.
    pub const TX_STATUS: u8 = 0x83;
}

/// A decoded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello { version: u16, mac: [u8; 6] },
    Tx { id: u32, no_ack: bool, frame: Vec<u8> },
    SetChannel { channel: u8 },
    SetPower { on: bool },
    Listen { all: bool },
    HelloAck { version: u16, mac: [u8; 6], channels: Vec<u8> },
    Rx { channel: u8, signal_dbm: i8, frame: Vec<u8> },
    TxStatus { id: u32, acked: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// A length of zero or above [`MAX_MESSAGE`]: the stream is broken.
    BadLength,
    /// An unknown type or a body of the wrong size (the message is
    /// skipped; the stream stays usable).
    BadMessage,
}

impl Message {
    /// Encodes the message with its length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        let ty = match self {
            Message::Hello { version, mac } => {
                body.extend_from_slice(&version.to_le_bytes());
                body.extend_from_slice(mac);
                msg::HELLO
            }
            Message::Tx { id, no_ack, frame } => {
                body.extend_from_slice(&id.to_le_bytes());
                body.push(*no_ack as u8);
                body.extend_from_slice(frame);
                msg::TX
            }
            Message::SetChannel { channel } => {
                body.push(*channel);
                msg::SET_CHANNEL
            }
            Message::SetPower { on } => {
                body.push(*on as u8);
                msg::SET_POWER
            }
            Message::Listen { all } => {
                body.push(*all as u8);
                msg::LISTEN
            }
            Message::HelloAck { version, mac, channels } => {
                body.extend_from_slice(&version.to_le_bytes());
                body.extend_from_slice(mac);
                body.extend_from_slice(channels);
                msg::HELLO_ACK
            }
            Message::Rx { channel, signal_dbm, frame } => {
                body.push(*channel);
                body.push(*signal_dbm as u8);
                body.extend_from_slice(frame);
                msg::RX
            }
            Message::TxStatus { id, acked } => {
                body.extend_from_slice(&id.to_le_bytes());
                body.push(*acked as u8);
                msg::TX_STATUS
            }
        };
        let mut out = Vec::with_capacity(5 + body.len());
        out.extend_from_slice(&((body.len() + 1) as u32).to_le_bytes());
        out.push(ty);
        out.extend_from_slice(&body);
        out
    }

    /// Decodes a message from its type and body.
    pub fn decode(ty: u8, b: &[u8]) -> Result<Message, LinkError> {
        let mac = |s: &[u8]| -> [u8; 6] { s.try_into().unwrap() };
        Ok(match ty {
            msg::HELLO if b.len() == 8 => {
                Message::Hello { version: u16::from_le_bytes([b[0], b[1]]), mac: mac(&b[2..8]) }
            }
            msg::TX if b.len() >= 5 => Message::Tx {
                id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                no_ack: b[4] != 0,
                frame: b[5..].to_vec(),
            },
            msg::SET_CHANNEL if b.len() == 1 => Message::SetChannel { channel: b[0] },
            msg::SET_POWER if b.len() == 1 => Message::SetPower { on: b[0] != 0 },
            msg::LISTEN if b.len() == 1 => Message::Listen { all: b[0] != 0 },
            msg::HELLO_ACK if b.len() >= 8 => Message::HelloAck {
                version: u16::from_le_bytes([b[0], b[1]]),
                mac: mac(&b[2..8]),
                channels: b[8..].to_vec(),
            },
            msg::RX if b.len() >= 2 => Message::Rx { channel: b[0], signal_dbm: b[1] as i8, frame: b[2..].to_vec() },
            msg::TX_STATUS if b.len() == 5 => {
                Message::TxStatus { id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]), acked: b[4] != 0 }
            }
            _ => return Err(LinkError::BadMessage),
        })
    }
}

/// Reassembles messages from a byte stream.
///
/// After a bad length the reader is out of step with the stream. Rather
/// than giving up on the connection, the reader can be told to
/// [`Reader::resync`]: it then discards bytes until it finds the start of a
/// hello (guest side: a hello acknowledgement) of the current version, which
/// the peer sends in answer to our own hello. This also handles a guest
/// driver that restarts and begins reading in the middle of a message.
#[derive(Debug, Default)]
pub struct Reader {
    buf: Vec<u8>,
    broken: bool,
    /// Discarding bytes until a message of this type starts.
    hunting: Option<u8>,
}

impl Reader {
    pub fn new() -> Reader {
        Reader::default()
    }

    /// Adds bytes received from the stream.
    pub fn push(&mut self, data: &[u8]) {
        if !self.broken {
            self.buf.extend_from_slice(data);
        }
    }

    /// Whether `b` starts a hello-type message (`ty` is [`msg::HELLO`] or
    /// [`msg::HELLO_ACK`]) of this protocol version.
    fn hello_starts(b: &[u8], ty: u8) -> bool {
        // type (1) + version (2) + MAC (6) + up to 64 channels.
        let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let len_ok = if ty == msg::HELLO { len == 9 } else { (9..=9 + 64).contains(&len) };
        len_ok && b[4] == ty && b[5..7] == VERSION.to_le_bytes()
    }

    /// The next complete message, `Ok(None)` if more bytes are needed.
    /// After `Err(BadLength)` the reader stays broken until
    /// [`Reader::reset`] or [`Reader::resync`].
    pub fn next_message(&mut self) -> Result<Option<Message>, LinkError> {
        loop {
            if self.broken {
                return Err(LinkError::BadLength);
            }
            if let Some(ty) = self.hunting {
                let found = (0..self.buf.len().saturating_sub(6)).find(|&i| Self::hello_starts(&self.buf[i..], ty));
                match found {
                    Some(i) => {
                        self.buf.drain(..i);
                        self.hunting = None;
                    }
                    None => {
                        // Keep the last bytes: a header may be split.
                        let keep = self.buf.len().min(6);
                        self.buf.drain(..self.buf.len() - keep);
                        return Ok(None);
                    }
                }
            }
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if len == 0 || len > MAX_MESSAGE {
                self.broken = true;
                self.buf.clear();
                return Err(LinkError::BadLength);
            }
            if self.buf.len() < 4 + len {
                return Ok(None);
            }
            let ty = self.buf[4];
            let result = Message::decode(ty, &self.buf[5..4 + len]);
            self.buf.drain(..4 + len);
            match result {
                Ok(m) => return Ok(Some(m)),
                // Skip messages we do not understand.
                Err(LinkError::BadMessage) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Starts over (after the connection was re-established).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.broken = false;
        self.hunting = None;
    }

    /// Gets back in step with the stream: discards bytes until a message of
    /// type `ty` ([`msg::HELLO`] on the host, [`msg::HELLO_ACK`] in the
    /// guest) begins. The caller sends its own hello so that the peer
    /// answers with one.
    pub fn resync(&mut self, ty: u8) {
        self.broken = false;
        self.hunting = Some(ty);
    }

    /// Whether the reader is discarding bytes until a hello.
    pub fn resyncing(&self) -> bool {
        self.hunting.is_some()
    }

    /// Bytes waiting for the rest of their message.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn samples() -> Vec<Message> {
        vec![
            Message::Hello { version: VERSION, mac: [2, 0, 0, 0x57, 0x4C, 1] },
            Message::Tx { id: 7, no_ack: true, frame: vec![0x80, 0, 1, 2, 3] },
            Message::SetChannel { channel: 11 },
            Message::SetPower { on: false },
            Message::Listen { all: true },
            Message::HelloAck { version: VERSION, mac: [2; 6], channels: vec![1, 6, 11] },
            Message::Rx { channel: 6, signal_dbm: -67, frame: vec![9; 300] },
            Message::TxStatus { id: 7, acked: true },
        ]
    }

    #[test]
    fn messages_round_trip_through_any_chunking() {
        let all: Vec<u8> = samples().iter().flat_map(|m| m.encode()).collect();
        for chunk in [1usize, 2, 3, 7, 64, all.len()] {
            let mut r = Reader::new();
            let mut got = Vec::new();
            for c in all.chunks(chunk) {
                r.push(c);
                while let Some(m) = r.next_message().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, samples(), "chunk size {chunk}");
            assert_eq!(r.pending(), 0);
        }
    }

    #[test]
    fn bad_lengths_break_the_stream_and_bad_bodies_are_skipped() {
        let mut r = Reader::new();
        r.push(&0u32.to_le_bytes());
        assert_eq!(r.next_message(), Err(LinkError::BadLength));
        r.push(&Message::SetChannel { channel: 1 }.encode());
        assert_eq!(r.next_message(), Err(LinkError::BadLength));
        r.reset();
        r.push(&((MAX_MESSAGE + 1) as u32).to_le_bytes());
        assert_eq!(r.next_message(), Err(LinkError::BadLength));
        r.reset();
        // Unknown type, then a wrong-size body, then a good message.
        r.push(&[2, 0, 0, 0, 0x7F, 0]);
        r.push(&[3, 0, 0, 0, msg::SET_CHANNEL, 1, 2]);
        r.push(&Message::SetPower { on: true }.encode());
        assert_eq!(r.next_message(), Ok(Some(Message::SetPower { on: true })));
        assert_eq!(r.next_message(), Ok(None));
    }

    #[test]
    fn random_streams_never_panic() {
        let mut s = 0x1234_5678u32;
        for _ in 0..200 {
            let mut r = Reader::new();
            for _ in 0..50 {
                let mut chunk = [0u8; 16];
                for b in chunk.iter_mut() {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    *b = (s % 9) as u8;
                }
                r.push(&chunk);
                while let Ok(Some(_)) = r.next_message() {}
            }
        }
    }

    #[test]
    fn resync_finds_the_next_hello() {
        let ack = Message::HelloAck { version: VERSION, mac: [2, 0x56, 0x57, 0xAA, 0, 1], channels: vec![1, 6, 11] };
        // The tail of a message, garbage that looks like a huge length, then
        // a receive message and the acknowledgement split across pushes.
        let mut stream = vec![0x33, 0x44, 0xFF, 0xFF, 0xFF, 0x7F, 0x81];
        stream.extend_from_slice(&Message::Rx { channel: 1, signal_dbm: -50, frame: vec![0x81; 40] }.encode());
        stream.extend_from_slice(&ack.encode());
        stream.extend_from_slice(&Message::TxStatus { id: 3, acked: false }.encode());
        for chunk in [1usize, 3, 5, 64] {
            let mut r = Reader::new();
            r.resync(msg::HELLO_ACK);
            assert!(r.resyncing());
            let mut got = Vec::new();
            for c in stream.chunks(chunk) {
                r.push(c);
                while let Some(m) = r.next_message().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, vec![ack.clone(), Message::TxStatus { id: 3, acked: false }], "chunk size {chunk}");
            assert!(!r.resyncing());
        }
        // A broken reader recovers through resync, on the host side too.
        let mut r = Reader::new();
        r.push(&[0xFF; 8]);
        assert_eq!(r.next_message(), Err(LinkError::BadLength));
        r.resync(msg::HELLO);
        r.push(&[1, 2, 3]);
        let hello = Message::Hello { version: VERSION, mac: [0; 6] };
        r.push(&hello.encode());
        assert_eq!(r.next_message(), Ok(Some(hello)));
        // A hello of another version is not taken for the start of a message.
        let mut r = Reader::new();
        r.resync(msg::HELLO);
        r.push(&Message::Hello { version: VERSION + 1, mac: [0; 6] }.encode());
        assert_eq!(r.next_message(), Ok(None));
        assert!(r.pending() <= 6);
    }
}
