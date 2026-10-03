//! TCP streams and listeners.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::net::SocketAddr;

use vabi::RawHandle;
use vipc::{Bytes, IpcError};
use vproto::net::{MAX_SEND, NetError, SocketEvent, SocketRequest, TcpOptions};
use vrt::object::Channel;
use vrt::time::Duration;

use crate::{CONNECT_TIMEOUT, deadline, send_request, with_client};

/// Return receive credit once this much was read.
const CONSUMED_BATCH: u32 = 32 * 1024;

/// A TCP connection.
pub struct TcpStream {
    ch: Channel,
    local: Option<SocketAddr>,
    remote: SocketAddr,
    connected: bool,
    credit: u32,
    rx: VecDeque<u8>,
    /// Bytes read but not yet reported to the service.
    unacked: u32,
    eof: bool,
    error: Option<NetError>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl core::fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TcpStream({:?} -> {})", self.local, self.remote)
    }
}

impl TcpStream {
    /// Connects to `addr`, waiting at most [`CONNECT_TIMEOUT`].
    pub fn connect(addr: SocketAddr) -> Result<TcpStream, NetError> {
        Self::connect_with(addr, &TcpOptions::default(), CONNECT_TIMEOUT)
    }

    /// Connects with a time limit.
    pub fn connect_timeout(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, NetError> {
        Self::connect_with(addr, &TcpOptions::default(), timeout)
    }

    /// Connects with options and a time limit.
    pub fn connect_with(addr: SocketAddr, opts: &TcpOptions, timeout: Duration) -> Result<TcpStream, NetError> {
        let mut opts = *opts;
        opts.connect_timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut s = Self::begin_connect(addr, &opts)?;
        let end = vrt::time::deadline_after(timeout);
        loop {
            if s.poll_connect()? {
                return Ok(s);
            }
            if !s.wait_event(end)? {
                return Err(NetError::TimedOut);
            }
        }
    }

    /// Resolves `host` and connects to the first address that answers,
    /// within `timeout` overall.
    pub fn connect_host(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, NetError> {
        let start = vrt::time::now_ns();
        let addrs = crate::lookup(host, crate::AddrFamily::Any, timeout.min(crate::RESOLVE_TIMEOUT))?.addresses;
        let mut last = NetError::NameNotFound;
        for (i, ip) in addrs.iter().enumerate() {
            let spent = Duration::from_nanos(vrt::time::now_ns() - start);
            let Some(left) = timeout.checked_sub(spent) else { break };
            // Leave time for the remaining addresses.
            let share = if i + 1 < addrs.len() { left / 2 } else { left };
            match Self::connect_timeout(SocketAddr::new(*ip, port), share.max(Duration::from_millis(500))) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Starts connecting and returns at once; finish with
    /// [`TcpStream::poll_connect`] when [`TcpStream::handle`] is readable.
    pub fn begin_connect(addr: SocketAddr, opts: &TcpOptions) -> Result<TcpStream, NetError> {
        let ch = with_client(|c| {
            let (ours, theirs) = Channel::create().map_err(IpcError::Kernel)?;
            Ok(c.tcp_connect(addr, *opts, theirs)?.map(|()| ours))
        })??;
        Ok(TcpStream {
            ch,
            local: None,
            remote: addr,
            connected: false,
            credit: 0,
            rx: VecDeque::new(),
            unacked: 0,
            eof: false,
            error: None,
            read_timeout: None,
            write_timeout: None,
        })
    }

    fn accepted(ch: Channel, local: SocketAddr, remote: SocketAddr, credit: u32) -> TcpStream {
        TcpStream {
            ch,
            local: Some(local),
            remote,
            connected: true,
            credit,
            rx: VecDeque::new(),
            unacked: 0,
            eof: false,
            error: None,
            read_timeout: None,
            write_timeout: None,
        }
    }

    /// Absorbs one event.
    fn absorb(&mut self, ev: SocketEvent) {
        match ev {
            SocketEvent::Connected { local, remote, credit } => {
                self.local = Some(local);
                self.remote = remote;
                self.credit = self.credit.saturating_add(credit);
                self.connected = true;
            }
            SocketEvent::Data { data } => self.rx.extend(data.0),
            SocketEvent::SendCredit { bytes } => self.credit = self.credit.saturating_add(bytes),
            SocketEvent::Eof {} => self.eof = true,
            SocketEvent::Error { error } => {
                self.error.get_or_insert(error);
            }
            _ => {}
        }
    }

    /// Takes every queued event without blocking. Returns `false` if the
    /// service closed the socket.
    fn drain(&mut self) -> bool {
        loop {
            match self.ch.read() {
                Ok(msg) => {
                    if let Ok((_, ev)) = vipc::decode_event::<SocketEvent>(msg) {
                        self.absorb(ev);
                    }
                }
                Err(vabi::Error::ShouldWait) => return true,
                Err(_) => {
                    self.error.get_or_insert(NetError::Closed);
                    return false;
                }
            }
        }
    }

    /// Waits for at least one event until `end`. Returns `false` on
    /// timeout.
    fn wait_event(&mut self, end: u64) -> Result<bool, NetError> {
        match self.ch.read_blocking(end) {
            Ok(msg) => {
                if let Ok((_, ev)) = vipc::decode_event::<SocketEvent>(msg) {
                    self.absorb(ev);
                }
                self.drain();
                Ok(true)
            }
            Err(vabi::Error::TimedOut) => Ok(false),
            Err(_) => {
                self.error.get_or_insert(NetError::Closed);
                Ok(true)
            }
        }
    }

    /// Non-blocking: whether the connection is established. Errors if it
    /// failed.
    pub fn poll_connect(&mut self) -> Result<bool, NetError> {
        self.drain();
        if self.connected {
            return Ok(true);
        }
        match self.error {
            Some(e) => Err(e),
            None => Ok(false),
        }
    }

    /// The channel handle: readable whenever something arrived (data,
    /// credit, the connection result). For event loops.
    pub fn handle(&self) -> RawHandle {
        self.ch.raw()
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.remote
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local
    }

    pub fn set_read_timeout(&mut self, t: Option<Duration>) {
        self.read_timeout = t;
    }

    pub fn set_write_timeout(&mut self, t: Option<Duration>) {
        self.write_timeout = t;
    }

    /// Disables (or re-enables) Nagle's algorithm.
    pub fn set_nodelay(&mut self, nodelay: bool) -> Result<(), NetError> {
        send_request(&self.ch, SocketRequest::SetNoDelay { nodelay })
    }

    fn take_rx(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.rx.len());
        for (dst, src) in buf.iter_mut().zip(self.rx.drain(..n)) {
            *dst = src;
        }
        self.unacked += n as u32;
        if self.unacked >= CONSUMED_BATCH || (self.rx.is_empty() && self.unacked > 0) {
            let bytes = core::mem::take(&mut self.unacked);
            let _ = send_request(&self.ch, SocketRequest::Consumed { bytes });
        }
        n
    }

    /// Non-blocking read: `Ok(Some(0))` at end of stream, `Ok(None)` if
    /// nothing is available yet.
    pub fn try_read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        self.drain();
        if !self.rx.is_empty() {
            return Ok(Some(self.take_rx(buf)));
        }
        if self.eof {
            return Ok(Some(0));
        }
        match self.error {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    /// Reads into `buf`, waiting for data (up to the read timeout). Returns
    /// 0 at the end of the stream.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, NetError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let end = deadline(self.read_timeout);
        loop {
            if let Some(n) = self.try_read(buf)? {
                return Ok(n);
            }
            if !self.wait_event(end)? {
                return Err(NetError::TimedOut);
            }
        }
    }

    /// Reads exactly `buf.len()` bytes.
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), NetError> {
        let mut done = 0;
        while done < buf.len() {
            let n = self.read(&mut buf[done..])?;
            if n == 0 {
                return Err(NetError::Closed);
            }
            done += n;
        }
        Ok(())
    }

    /// Reads until the end of the stream (at most `limit` bytes).
    pub fn read_to_end(&mut self, out: &mut Vec<u8>, limit: usize) -> Result<usize, NetError> {
        let start = out.len();
        let mut buf = alloc::vec![0u8; 16 * 1024];
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                return Ok(out.len() - start);
            }
            if out.len() - start + n > limit {
                return Err(NetError::MessageTooLarge);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Non-blocking write: how much was queued (`Ok(0)` if there is no
    /// send credit right now).
    pub fn try_write(&mut self, data: &[u8]) -> Result<usize, NetError> {
        self.drain();
        if let Some(e) = self.error {
            return Err(e);
        }
        if !self.connected {
            return Ok(0);
        }
        let n = data.len().min(self.credit as usize).min(MAX_SEND);
        if n == 0 {
            return Ok(0);
        }
        send_request(&self.ch, SocketRequest::Send { data: Bytes(data[..n].to_vec()) })?;
        self.credit -= n as u32;
        Ok(n)
    }

    /// Writes some of `data`, waiting for send credit (up to the write
    /// timeout).
    pub fn write(&mut self, data: &[u8]) -> Result<usize, NetError> {
        if data.is_empty() {
            return Ok(0);
        }
        let end = deadline(self.write_timeout);
        loop {
            let n = self.try_write(data)?;
            if n > 0 {
                return Ok(n);
            }
            if !self.wait_event(end)? {
                return Err(NetError::TimedOut);
            }
        }
    }

    /// Writes all of `data`.
    pub fn write_all(&mut self, mut data: &[u8]) -> Result<(), NetError> {
        while !data.is_empty() {
            let n = self.write(data)?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Finishes sending (the peer sees end of stream); reading continues.
    pub fn shutdown(&mut self) -> Result<(), NetError> {
        send_request(&self.ch, SocketRequest::Shutdown {})
    }

    /// Drops the connection at once (the peer sees a reset).
    pub fn abort(self) {
        let _ = send_request(&self.ch, SocketRequest::Abort {});
    }

    /// Bytes received and not read yet.
    pub fn available(&mut self) -> usize {
        self.drain();
        self.rx.len()
    }
}

/// A listening TCP socket.
pub struct TcpListener {
    ch: Channel,
    local: SocketAddr,
}

impl TcpListener {
    /// Listens on `addr` (port 0 picks a free port).
    pub fn bind(addr: SocketAddr, backlog: u32) -> Result<TcpListener, NetError> {
        let (ch, local) = with_client(|c| {
            let (ours, theirs) = Channel::create().map_err(IpcError::Kernel)?;
            Ok(c.tcp_listen(addr, backlog, TcpOptions::default(), theirs)?.map(|local| (ours, local)))
        })??;
        Ok(TcpListener { ch, local })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn handle(&self) -> RawHandle {
        self.ch.raw()
    }

    /// Waits for a connection (up to `timeout`).
    pub fn accept_timeout(&self, timeout: Option<Duration>) -> Result<(TcpStream, SocketAddr), NetError> {
        let end = deadline(timeout);
        loop {
            let msg = self.ch.read_blocking(end).map_err(|e| match e {
                vabi::Error::TimedOut => NetError::TimedOut,
                _ => NetError::Closed,
            })?;
            match vipc::decode_event::<SocketEvent>(msg) {
                Ok((_, SocketEvent::Accepted { socket, local, remote, credit })) => {
                    return Ok((TcpStream::accepted(socket, local, remote, credit), remote));
                }
                Ok((_, SocketEvent::Error { error })) => return Err(error),
                _ => {}
            }
        }
    }

    /// Waits for a connection.
    pub fn accept(&self) -> Result<(TcpStream, SocketAddr), NetError> {
        self.accept_timeout(None)
    }
}
