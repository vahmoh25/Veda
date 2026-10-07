//! Sockets: bidirectional byte streams.
//!
//! A socket has two endpoints. Bytes written to one are read from the other,
//! in order, through a bounded buffer per direction. Unlike channels, socket
//! handles can be duplicated, so several processes can share an endpoint as
//! they share a Unix pipe or terminal: the peer sees `PEER_CLOSED` only once
//! every handle to an endpoint is closed. An endpoint can also stop writing
//! ([`SocketEnd::shutdown`]); its peer then reads what is buffered and after
//! that the end of the stream, while the other direction keeps working.

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use vabi::Error;
use vabi::signals::{PEER_CLOSED, PEER_WRITE_DISABLED, READABLE, WRITABLE};

use super::Signals;
use crate::sync::SpinLock;

/// Bytes buffered per direction before writers get `ShouldWait`.
pub const CAPACITY: usize = 256 * 1024;
/// Writes of at most this many bytes are never split: they go in whole or
/// wait (POSIX `PIPE_BUF`). An endpoint is writable while this much fits.
pub const ATOMIC_WRITE: usize = vabi::SOCKET_ATOMIC_WRITE;
/// A buffer that has emptied keeps its memory up to this size.
const KEPT_BUFFER: usize = 64 * 1024;

/// Makes room for `n` more bytes in `buf`, growing it by doubling but never
/// beyond [`CAPACITY`]; `NoMemory` (not a panic) if the kernel heap cannot.
fn grow(buf: &mut VecDeque<u8>, n: usize) -> Result<(), Error> {
    let need = buf.len() + n;
    if need <= buf.capacity() {
        return Ok(());
    }
    let target = need.max(buf.capacity() * 2).min(CAPACITY);
    buf.try_reserve_exact(target - buf.len()).map_err(|_| Error::NoMemory)
}

struct EndState {
    /// Bytes written by the peer, waiting to be read here.
    rx: VecDeque<u8>,
    /// Every handle to this endpoint is closed.
    closed: bool,
    /// This endpoint will write no more.
    write_disabled: bool,
}

struct Pair {
    ends: SpinLock<[EndState; 2]>,
    signals: [Signals; 2],
    koids: [u64; 2],
}

/// One endpoint of a socket.
pub struct SocketEnd {
    pair: Arc<Pair>,
    side: usize,
}

/// Creates a connected pair of endpoints.
pub fn create() -> (Arc<SocketEnd>, Arc<SocketEnd>) {
    let empty = || EndState { rx: VecDeque::new(), closed: false, write_disabled: false };
    let pair = Arc::new(Pair {
        ends: SpinLock::new([empty(), empty()]),
        signals: [Signals::new(WRITABLE), Signals::new(WRITABLE)],
        koids: [super::new_koid(), super::new_koid()],
    });
    (Arc::new(SocketEnd { pair: pair.clone(), side: 0 }), Arc::new(SocketEnd { pair, side: 1 }))
}

impl SocketEnd {
    pub fn koid(&self) -> u64 {
        self.pair.koids[self.side]
    }

    pub fn signals(&self) -> &Signals {
        &self.pair.signals[self.side]
    }

    fn peer(&self) -> usize {
        1 - self.side
    }

    /// How many bytes [`write`](Self::write) would accept now.
    pub fn writable_bytes(&self) -> usize {
        let ends = self.pair.ends.lock();
        let peer = &ends[self.peer()];
        if peer.closed || ends[self.side].write_disabled { 0 } else { CAPACITY - peer.rx.len() }
    }

    /// Bytes waiting to be read from this endpoint.
    pub fn readable_bytes(&self) -> usize {
        self.pair.ends.lock()[self.side].rx.len()
    }

    /// Appends as much of `data` as fits to the peer's buffer and returns
    /// how much that was. Writes of up to [`ATOMIC_WRITE`] bytes go in whole
    /// or not at all.
    pub fn write(&self, data: &[u8]) -> Result<usize, Error> {
        let peer = self.peer();
        let (n, now_full) = {
            let mut ends = self.pair.ends.lock();
            if ends[self.side].write_disabled {
                return Err(Error::BadState);
            }
            let pe = &mut ends[peer];
            if pe.closed {
                return Err(Error::PeerClosed);
            }
            let room = CAPACITY - pe.rx.len();
            if room == 0 || (data.len() <= ATOMIC_WRITE && room < data.len()) {
                return Err(Error::ShouldWait);
            }
            let n = data.len().min(room);
            grow(&mut pe.rx, n)?;
            pe.rx.extend(&data[..n]);
            (n, CAPACITY - pe.rx.len() < ATOMIC_WRITE)
        };
        if n > 0 {
            self.pair.signals[peer].update(0, READABLE);
        }
        if now_full {
            self.signals().update(WRITABLE, 0);
        }
        Ok(n)
    }

    /// Moves up to `out.len()` buffered bytes into `out`. At the end of the
    /// stream (the peer closed or stopped writing, and nothing is left) the
    /// result is `PeerClosed`.
    pub fn read(&self, out: &mut [u8]) -> Result<usize, Error> {
        let peer = self.peer();
        let (n, now_empty, peer_writable) = {
            let mut ends = self.pair.ends.lock();
            let ended = ends[peer].closed || ends[peer].write_disabled;
            let me = &mut ends[self.side];
            if me.rx.is_empty() {
                return Err(if ended { Error::PeerClosed } else { Error::ShouldWait });
            }
            let n = out.len().min(me.rx.len());
            let (front, back) = me.rx.as_slices();
            let from_front = n.min(front.len());
            out[..from_front].copy_from_slice(&front[..from_front]);
            out[from_front..n].copy_from_slice(&back[..n - from_front]);
            me.rx.drain(..n);
            if me.rx.is_empty() && me.rx.capacity() > KEPT_BUFFER {
                // A reader that fell behind made it large; give it back.
                me.rx = VecDeque::new();
            }
            let (room, now_empty) = (CAPACITY - me.rx.len(), me.rx.is_empty());
            (n, now_empty, room >= ATOMIC_WRITE && !ends[peer].write_disabled)
        };
        if now_empty {
            self.signals().update(READABLE, 0);
        }
        if peer_writable {
            self.pair.signals[peer].update(0, WRITABLE);
        }
        Ok(n)
    }

    /// Stops writing from this endpoint: the peer reads what is buffered
    /// and then the end of the stream. Reading here is unaffected.
    pub fn shutdown(&self) {
        let peer = self.peer();
        let changed = {
            let mut ends = self.pair.ends.lock();
            !core::mem::replace(&mut ends[self.side].write_disabled, true)
        };
        if changed {
            self.signals().update(WRITABLE, 0);
            self.pair.signals[peer].update(0, PEER_WRITE_DISABLED);
        }
    }
}

impl Drop for SocketEnd {
    fn drop(&mut self) {
        let peer = self.peer();
        let unread = {
            let mut ends = self.pair.ends.lock();
            ends[self.side].closed = true;
            core::mem::take(&mut ends[self.side].rx)
        };
        drop(unread);
        self.pair.signals[peer].update(WRITABLE, PEER_CLOSED);
    }
}
