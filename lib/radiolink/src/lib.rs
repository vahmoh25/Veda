//! The radio link: messages between the virtual Wi-Fi radio of a Vindows
//! guest (the `vwifi` driver) and the radio medium simulated on the host
//! (`airsim`).
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

/// Version of this protocol.
pub const VERSION: u16 = 1;
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
            msg::HELLO if b.len() == 8 => Message::Hello { version: u16::from_le_bytes([b[0], b[1]]), mac: mac(&b[2..8]) },
            msg::TX if b.len() >= 5 => {
                Message::Tx { id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]), no_ack: b[4] != 0, frame: b[5..].to_vec() }
            }
            msg::SET_CHANNEL if b.len() == 1 => Message::SetChannel { channel: b[0] },
            msg::SET_POWER if b.len() == 1 => Message::SetPower { on: b[0] != 0 },
            msg::HELLO_ACK if b.len() >= 8 => {
                Message::HelloAck { version: u16::from_le_bytes([b[0], b[1]]), mac: mac(&b[2..8]), channels: b[8..].to_vec() }
            }
            msg::RX if b.len() >= 2 => Message::Rx { channel: b[0], signal_dbm: b[1] as i8, frame: b[2..].to_vec() },
            msg::TX_STATUS if b.len() == 5 => {
                Message::TxStatus { id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]), acked: b[4] != 0 }
            }
            _ => return Err(LinkError::BadMessage),
        })
    }
}

/// Reassembles messages from a byte stream.
#[derive(Debug, Default)]
pub struct Reader {
    buf: Vec<u8>,
    broken: bool,
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

    /// The next complete message, `Ok(None)` if more bytes are needed.
    /// After `Err(BadLength)` the reader stays broken until [`Reader::reset`].
    pub fn next(&mut self) -> Result<Option<Message>, LinkError> {
        loop {
            if self.broken {
                return Err(LinkError::BadLength);
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
                while let Some(m) = r.next().unwrap() {
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
        assert_eq!(r.next(), Err(LinkError::BadLength));
        r.push(&Message::SetChannel { channel: 1 }.encode());
        assert_eq!(r.next(), Err(LinkError::BadLength));
        r.reset();
        r.push(&((MAX_MESSAGE + 1) as u32).to_le_bytes());
        assert_eq!(r.next(), Err(LinkError::BadLength));
        r.reset();
        // Unknown type, then a wrong-size body, then a good message.
        r.push(&[2, 0, 0, 0, 0x7F, 0]);
        r.push(&[3, 0, 0, 0, msg::SET_CHANNEL, 1, 2]);
        r.push(&Message::SetPower { on: true }.encode());
        assert_eq!(r.next(), Ok(Some(Message::SetPower { on: true })));
        assert_eq!(r.next(), Ok(None));
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
                while let Ok(Some(_)) = r.next() {}
            }
        }
    }
}
