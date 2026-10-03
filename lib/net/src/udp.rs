//! UDP sockets.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::net::SocketAddr;

use vabi::RawHandle;
use vipc::{Bytes, IpcError};
use vproto::net::{MAX_DATAGRAM, NetError, SocketEvent, SocketRequest};
use vrt::object::Channel;
use vrt::time::Duration;

use crate::{deadline, send_request, with_client};

/// A UDP socket.
pub struct UdpSocket {
    ch: Channel,
    local: SocketAddr,
    credit: u32,
    queue: VecDeque<(SocketAddr, Vec<u8>)>,
    unacked: u32,
    error: Option<NetError>,
}

impl UdpSocket {
    /// Binds to `addr` (port 0 picks a free port).
    pub fn bind(addr: SocketAddr) -> Result<UdpSocket, NetError> {
        let (ch, local) = with_client(|c| {
            let (ours, theirs) = Channel::create().map_err(IpcError::Kernel)?;
            Ok(c.udp_bind(addr, theirs)?.map(|local| (ours, local)))
        })??;
        Ok(UdpSocket { ch, local, credit: 0, queue: VecDeque::new(), unacked: 0, error: None })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn handle(&self) -> RawHandle {
        self.ch.raw()
    }

    fn absorb(&mut self, ev: SocketEvent) {
        match ev {
            SocketEvent::Datagram { from, data } => self.queue.push_back((from, data.0)),
            SocketEvent::SendCredit { bytes } => self.credit = self.credit.saturating_add(bytes),
            SocketEvent::Error { error } => {
                self.error.get_or_insert(error);
            }
            _ => {}
        }
    }

    fn drain(&mut self) {
        loop {
            match self.ch.read() {
                Ok(msg) => {
                    if let Ok((_, ev)) = vipc::decode_event::<SocketEvent>(msg) {
                        self.absorb(ev);
                    }
                }
                Err(vabi::Error::ShouldWait) => return,
                Err(_) => {
                    self.error.get_or_insert(NetError::Closed);
                    return;
                }
            }
        }
    }

    fn wait(&mut self, end: u64) -> Result<bool, NetError> {
        match self.ch.read_blocking(end) {
            Ok(msg) => {
                if let Ok((_, ev)) = vipc::decode_event::<SocketEvent>(msg) {
                    self.absorb(ev);
                }
                self.drain();
                Ok(true)
            }
            Err(vabi::Error::TimedOut) => Ok(false),
            Err(_) => Err(NetError::Closed),
        }
    }

    /// Sends a datagram, waiting for send credit if needed.
    pub fn send_to(&mut self, data: &[u8], dest: SocketAddr) -> Result<(), NetError> {
        if data.len() > MAX_DATAGRAM {
            return Err(NetError::MessageTooLarge);
        }
        let end = vrt::time::deadline_after(Duration::from_secs(10));
        loop {
            self.drain();
            if let Some(e) = self.error {
                return Err(e);
            }
            if self.credit as usize >= data.len() {
                self.credit -= data.len() as u32;
                return send_request(&self.ch, SocketRequest::SendTo { dest, data: Bytes(data.to_vec()) });
            }
            if !self.wait(end)? {
                return Err(NetError::TimedOut);
            }
        }
    }

    /// Takes the next datagram without blocking.
    pub fn try_recv_from(&mut self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>, NetError> {
        self.drain();
        if let Some((from, data)) = self.queue.pop_front() {
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            self.unacked += data.len() as u32;
            if self.unacked >= 16 * 1024 || self.queue.is_empty() {
                let bytes = core::mem::take(&mut self.unacked);
                let _ = send_request(&self.ch, SocketRequest::Consumed { bytes });
            }
            return Ok(Some((n, from)));
        }
        match self.error {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    /// Waits for a datagram (up to `timeout`). Datagrams longer than `buf`
    /// are truncated.
    pub fn recv_from(&mut self, buf: &mut [u8], timeout: Option<Duration>) -> Result<(usize, SocketAddr), NetError> {
        let end = deadline(timeout);
        loop {
            if let Some(r) = self.try_recv_from(buf)? {
                return Ok(r);
            }
            if !self.wait(end)? {
                return Err(NetError::TimedOut);
            }
        }
    }
}
