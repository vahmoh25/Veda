//! `vipc` â€” typed inter-process communication for Vindows.
//!
//! * [`codec`]: the binary encoding (`Encode`/`Decode`) plus the `message!`,
//!   `enumeration!` and `union!` macros for declaring wire types.
//! * [`protocol!`]: declares an RPC protocol and generates, in a module, a
//!   typed `Client`, a `Server` trait and a `dispatch` function.
//! * [`WaitSet`]: waits on many handles at once (the core of every service
//!   loop).
//!
//! Every message starts with a 12-byte [`Header`]: method ordinal,
//! transaction id and flags. Requests and their responses share a
//! transaction id; events (server â†’ client notifications) use a separate
//! channel in Vindows protocols, which keeps clients simple.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod codec;

use alloc::vec::Vec;
use core::fmt;

pub use codec::{Bytes, Decode, DecodeError, Decoder, Encode, Encoder};
pub use vrt::object::{Channel, Handle, Message};
use vabi::{Error, RawHandle, WaitItem};

/// Size of the message header.
pub const HEADER_LEN: usize = 12;
pub const FLAG_REQUEST: u32 = 1;
pub const FLAG_RESPONSE: u32 = 2;
pub const FLAG_EVENT: u32 = 4;

/// Re-exports used by macro-generated code.
#[doc(hidden)]
pub mod prelude {
    pub use alloc::boxed::Box;
    pub use alloc::string::String;
    pub use alloc::vec::Vec;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ordinal: u32,
    pub txid: u32,
    pub flags: u32,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Header, IpcError> {
        if bytes.len() < HEADER_LEN {
            return Err(IpcError::Decode(DecodeError::Truncated));
        }
        let w = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        Ok(Header { ordinal: w(0), txid: w(4), flags: w(8) })
    }
}

/// IPC failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// The other side closed the channel.
    PeerClosed,
    /// A kernel error while sending or receiving.
    Kernel(Error),
    /// The message was malformed.
    Decode(DecodeError),
    /// The server does not implement the requested method.
    UnknownMethod(u32),
}

impl From<DecodeError> for IpcError {
    fn from(e: DecodeError) -> Self {
        IpcError::Decode(e)
    }
}

impl From<Error> for IpcError {
    fn from(e: Error) -> Self {
        if e == Error::PeerClosed { IpcError::PeerClosed } else { IpcError::Kernel(e) }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::PeerClosed => f.write_str("peer closed the channel"),
            IpcError::Kernel(e) => write!(f, "kernel error: {e}"),
            IpcError::Decode(e) => write!(f, "bad message: {e}"),
            IpcError::UnknownMethod(o) => write!(f, "unknown method {o}"),
        }
    }
}

impl Encoder {
    /// Starts a message with the given header.
    pub fn with_header(ordinal: u32, txid: u32, flags: u32) -> Encoder {
        let mut e = Encoder::new();
        e.put_u32(ordinal);
        e.put_u32(txid);
        e.put_u32(flags);
        e
    }

    /// Writes the message to `ch`.
    pub fn send(self, ch: &Channel) -> Result<(), IpcError> {
        ch.write(&self.bytes, self.handles).map_err(IpcError::from)
    }
}

/// Splits a received message into its header and a decoder for the body.
pub fn open(msg: &mut Message) -> Result<(Header, Decoder<'_>), IpcError> {
    let header = Header::parse(&msg.bytes)?;
    let handles = core::mem::take(&mut msg.handles);
    Ok((header, Decoder::new(&msg.bytes[HEADER_LEN..], handles)))
}

/// Sends a request and waits for the response with the same transaction id.
pub fn call(ch: &Channel, request: Encoder, txid: u32) -> Result<Message, IpcError> {
    request.send(ch)?;
    loop {
        let msg = ch.read_blocking(vabi::DEADLINE_INFINITE)?;
        let h = Header::parse(&msg.bytes)?;
        if h.flags & FLAG_RESPONSE != 0 && h.txid == txid {
            return Ok(msg);
        }
        // Anything else (e.g. a late reply to an abandoned call) is dropped.
    }
}

/// Sends a one-way event message with the given ordinal and payload.
pub fn send_event<T: Encode>(ch: &Channel, ordinal: u32, payload: T) -> Result<(), IpcError> {
    let mut e = Encoder::with_header(ordinal, 0, FLAG_EVENT);
    payload.encode(&mut e);
    e.send(ch)
}

/// Decodes the payload of an event message.
pub fn decode_event<T: Decode>(mut msg: Message) -> Result<(u32, T), IpcError> {
    let (h, mut d) = open(&mut msg)?;
    let v = T::decode(&mut d)?;
    Ok((h.ordinal, v))
}

/// Declares an RPC protocol.
///
/// ```ignore
/// vipc::protocol! {
///     /// File system service.
///     pub mod vfs = "vfs" {
///         1 => fn stat(path: String) -> Result<FileInfo, FsError>;
///         2 => fn read(fd: u32, offset: u64, len: u32) -> Result<Bytes, FsError>;
///     }
/// }
/// ```
///
/// generates `vfs::NAME`, `vfs::Client` (one method per call, returning
/// `Result<ReturnType, IpcError>`), the `vfs::Server` trait and
/// `vfs::dispatch(&mut impl Server, Message) -> Result<Encoder, IpcError>`.
#[macro_export]
macro_rules! protocol {
    (
        $(#[$meta:meta])*
        $vis:vis mod $module:ident = $service:literal {
            $(
                $(#[$mmeta:meta])*
                $ord:literal => fn $method:ident ( $($arg:ident : $aty:ty),* $(,)? ) -> $ret:ty ;
            )*
        }
    ) => {
        $(#[$meta])*
        #[allow(clippy::too_many_arguments, unused_imports)]
        $vis mod $module {
            use super::*;
            use $crate::prelude::*;

            /// Name under which the service registers.
            pub const NAME: &str = $service;

            /// Typed client stub. Not thread-safe: wrap it in a mutex to
            /// share it.
            pub struct Client {
                channel: $crate::Channel,
                txid: core::cell::Cell<u32>,
            }

            impl Client {
                pub fn new(channel: $crate::Channel) -> Client {
                    Client { channel, txid: core::cell::Cell::new(1) }
                }

                pub fn channel(&self) -> &$crate::Channel {
                    &self.channel
                }

                pub fn into_channel(self) -> $crate::Channel {
                    self.channel
                }

                fn next_txid(&self) -> u32 {
                    let t = self.txid.get();
                    self.txid.set(t.wrapping_add(1).max(1));
                    t
                }

                $(
                    $(#[$mmeta])*
                    pub fn $method(&self, $($arg: $aty),*) -> Result<$ret, $crate::IpcError> {
                        let txid = self.next_txid();
                        #[allow(unused_mut)]
                        let mut e = $crate::Encoder::with_header($ord, txid, $crate::FLAG_REQUEST);
                        $( $crate::Encode::encode($arg, &mut e); )*
                        let mut reply = $crate::call(&self.channel, e, txid)?;
                        let (_, mut d) = $crate::open(&mut reply)?;
                        Ok(<$ret as $crate::Decode>::decode(&mut d)?)
                    }
                )*
            }

            /// Implemented by the service.
            pub trait Server {
                $(
                    $(#[$mmeta])*
                    fn $method(&mut self, $($arg: $aty),*) -> $ret;
                )*
            }

            /// Decodes one request, invokes the server and encodes the
            /// response (send it back on the same channel).
            pub fn dispatch<S: Server + ?Sized>(server: &mut S, mut msg: $crate::Message) -> Result<$crate::Encoder, $crate::IpcError> {
                let (header, mut d) = $crate::open(&mut msg)?;
                match header.ordinal {
                    $(
                        $ord => {
                            $( let $arg = <$aty as $crate::Decode>::decode(&mut d)?; )*
                            d.finish()?;
                            let r = server.$method($($arg),*);
                            let mut e = $crate::Encoder::with_header($ord, header.txid, $crate::FLAG_RESPONSE);
                            $crate::Encode::encode(r, &mut e);
                            Ok(e)
                        }
                    )*
                    other => Err($crate::IpcError::UnknownMethod(other)),
                }
            }

            /// Handles one request on `channel`: reads it, dispatches it and
            /// writes the response.
            pub fn serve_one<S: Server + ?Sized>(server: &mut S, channel: &$crate::Channel) -> Result<(), $crate::IpcError> {
                let msg = channel.read()?;
                let reply = dispatch(server, msg)?;
                reply.send(channel)
            }
        }
    };
}

/// A set of handles to wait on together, each tagged with a key.
#[derive(Default)]
pub struct WaitSet {
    items: Vec<WaitItem>,
    keys: Vec<u64>,
}

impl WaitSet {
    pub fn new() -> WaitSet {
        WaitSet::default()
    }

    pub fn add(&mut self, handle: RawHandle, signals: u32, key: u64) {
        self.items.push(WaitItem { handle, signals, observed: 0, _reserved: 0 });
        self.keys.push(key);
    }

    pub fn remove(&mut self, key: u64) {
        if let Some(i) = self.keys.iter().position(|&k| k == key) {
            self.items.swap_remove(i);
            self.keys.swap_remove(i);
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Waits until at least one handle is ready or `deadline` passes.
    /// Returns `(key, observed signals)` for every ready handle; an empty
    /// result means the deadline passed.
    pub fn wait(&mut self, deadline: u64) -> Result<Vec<(u64, u32)>, Error> {
        let mut ready = Vec::new();
        if self.items.is_empty() {
            vrt::time::sleep_until(deadline);
            return Ok(ready);
        }
        // The kernel accepts at most WAIT_MANY_MAX items per call; larger sets
        // are polled in chunks first, then blocked on in the first chunk.
        let max = vabi::WAIT_MANY_MAX;
        if self.items.len() > max {
            for chunk in self.items.chunks_mut(max) {
                let _ = vrt::object::wait_many(chunk, 0);
            }
        } else {
            match vrt::object::wait_many(&mut self.items, deadline) {
                Ok(_) | Err(Error::TimedOut) => {}
                Err(e) => return Err(e),
            }
        }
        for (it, &k) in self.items.iter().zip(&self.keys) {
            if it.observed & it.signals != 0 {
                ready.push((k, it.observed));
            }
        }
        if ready.is_empty() && self.items.len() > max {
            // Nothing ready among many handles: block on a short timeout.
            let short = vrt::time::now_ns().saturating_add(2_000_000).min(deadline);
            let _ = vrt::object::wait_many(&mut self.items[..max], short);
            for (it, &k) in self.items[..max].iter().zip(&self.keys) {
                if it.observed & it.signals != 0 {
                    ready.push((k, it.observed));
                }
            }
        }
        Ok(ready)
    }
}
