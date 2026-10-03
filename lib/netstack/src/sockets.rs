//! Sockets: the TCP, UDP and ping operations behind `netd`'s socket
//! channels.
//!
//! smoltcp sockets live in the socket set of one interface, so:
//!
//! * a TCP connection is created on the interface its route picks;
//! * a TCP listener or UDP socket bound to the unspecified address gets one
//!   smoltcp socket per interface (and new interfaces get one too), so it
//!   is reachable everywhere;
//! * a ping socket gets an ICMP socket on each interface it sends through.
//!
//! smoltcp closes a socket the same way after a reset and after a timeout;
//! the stack tells them apart (refused, reset, timed out) from its own
//! bookkeeping, so applications get accurate errors.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use smoltcp::iface::SocketHandle;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::{icmp, tcp, udp};
use smoltcp::time::Duration;
use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr, Icmpv6Packet, Icmpv6Repr, IpAddress, IpEndpoint, IpListenEndpoint};
use vproto::net::{NetError, SocketInfo, SocketKind, TcpOptions};

use crate::{IfaceId, Stack};

/// Identifies a socket.
pub type SockId = u32;

pub const TCP_RX_BUFFER: usize = 64 * 1024;
pub const TCP_TX_BUFFER: usize = 64 * 1024;
const UDP_PACKETS: usize = 64;
const UDP_BYTES: usize = 64 * 1024;
const ICMP_PACKETS: usize = 16;
const ICMP_BYTES: usize = 16 * 1024;
/// Most sockets of all kinds.
pub const MAX_SOCKETS: usize = 1024;
/// Most listening sockets per interface for one listener.
const LISTEN_PER_IFACE: usize = 8;
const DEFAULT_CONNECT_TIMEOUT_US: u64 = 20_000_000;
/// Give up on an established connection whose peer acknowledged nothing
/// for this long.
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(120);
const PING_TIMEOUT_US: u64 = 4_000_000;
const EPHEMERAL_FIRST: u16 = 49152;
const EPHEMERAL_COUNT: u32 = 16384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Proto {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Connecting,
    Open,
    Closed,
}

pub(crate) struct TcpSock {
    pub iface: IfaceId,
    pub handle: Option<SocketHandle>,
    /// Local port reserved in the table (0: owned by a listener).
    pub port: u16,
    pub phase: Phase,
    pub connect_deadline_us: u64,
    /// We closed or aborted it (so a later `Closed` is expected).
    pub closed_by_us: bool,
    /// The client is gone: remove the socket once it is closed.
    pub orphan: bool,
    pub progress_us: u64,
    pub last_rx_queue: usize,
    pub last_tx_queue: usize,
    pub error: Option<NetError>,
    pub local: Option<SocketAddr>,
    pub remote: Option<SocketAddr>,
    pub idle_timeout_us: u64,
    pub owner: u64,
    /// Peer finished sending.
    pub peer_fin: bool,
}

pub(crate) struct Listener {
    pub local: SocketAddr,
    pub backlog: usize,
    pub members: Vec<(IfaceId, SocketHandle)>,
    pub accepted: VecDeque<SockId>,
    pub opts: TcpOptions,
    pub owner: u64,
}

pub(crate) struct UdpSock {
    pub local: SocketAddr,
    pub members: Vec<(IfaceId, SocketHandle)>,
    pub owner: u64,
}

pub(crate) struct PingSock {
    pub ident: u16,
    pub members: Vec<(IfaceId, SocketHandle)>,
    /// Outstanding requests: seq -> (sent at, destination).
    pub pending: BTreeMap<u16, (u64, IpAddr)>,
    pub events: VecDeque<PingEvent>,
    pub owner: u64,
}

pub(crate) enum Entry {
    Tcp(TcpSock),
    Listener(Listener),
    Udp(UdpSock),
    Ping(PingSock),
}

/// The socket table.
pub(crate) struct Table {
    pub map: BTreeMap<SockId, Entry>,
    next: SockId,
    /// Reserved local ports and how many sockets use each.
    ports: BTreeMap<(Proto, u16), u32>,
}

impl Table {
    pub fn new() -> Table {
        Table { map: BTreeMap::new(), next: 1, ports: BTreeMap::new() }
    }

    fn insert(&mut self, e: Entry) -> SockId {
        loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1).max(1);
            if !self.map.contains_key(&id) {
                self.map.insert(id, e);
                return id;
            }
        }
    }

    fn reserve(&mut self, proto: Proto, port: u16) {
        *self.ports.entry((proto, port)).or_insert(0) += 1;
    }

    fn release(&mut self, proto: Proto, port: u16) {
        if port == 0 {
            return;
        }
        if let Some(n) = self.ports.get_mut(&(proto, port)) {
            *n -= 1;
            if *n == 0 {
                self.ports.remove(&(proto, port));
            }
        }
    }

    fn in_use(&self, proto: Proto, port: u16) -> bool {
        self.ports.contains_key(&(proto, port))
    }
}

/// The state of a TCP connection as seen by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Connecting,
    Open {
        local: SocketAddr,
        remote: SocketAddr,
    },
    /// Closed, with the reason unless it closed normally.
    Closed {
        error: Option<NetError>,
    },
}

/// What a client of a TCP socket needs to know after a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpView {
    pub state: TcpState,
    /// Bytes ready to be received.
    pub recv_queued: usize,
    /// The peer finished sending and everything was received.
    pub eof: bool,
    /// Bytes the send buffer can take now.
    pub send_space: usize,
    /// Bytes sent but not yet acknowledged, or not yet sent.
    pub send_queued: usize,
}

/// A received UDP datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpDatagram {
    pub from: SocketAddr,
    pub len: usize,
}

/// The outcome of an echo request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingEvent {
    Reply { from: IpAddr, seq: u16, size: u16, rtt_us: u32 },
    Failed { seq: u16, error: NetError },
}

fn tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(alloc::vec![0u8; TCP_RX_BUFFER]),
        tcp::SocketBuffer::new(alloc::vec![0u8; TCP_TX_BUFFER]),
    )
}

fn udp_socket() -> udp::Socket<'static> {
    udp::Socket::new(
        udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], alloc::vec![0u8; UDP_BYTES]),
        udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], alloc::vec![0u8; UDP_BYTES]),
    )
}

fn icmp_socket() -> icmp::Socket<'static> {
    icmp::Socket::new(
        icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; ICMP_PACKETS], alloc::vec![0u8; ICMP_BYTES]),
        icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; ICMP_PACKETS], alloc::vec![0u8; ICMP_BYTES]),
    )
}

fn apply_options(s: &mut tcp::Socket, opts: &TcpOptions) {
    s.set_nagle_enabled(!opts.nodelay);
    s.set_keep_alive(if opts.keepalive_ms > 0 { Some(Duration::from_millis(opts.keepalive_ms as u64)) } else { None });
    s.set_timeout(Some(TCP_USER_TIMEOUT));
    s.set_ack_delay(Some(Duration::from_millis(10)));
}

fn state_name(s: tcp::State) -> &'static str {
    match s {
        tcp::State::Closed => "CLOSED",
        tcp::State::Listen => "LISTEN",
        tcp::State::SynSent => "SYN-SENT",
        tcp::State::SynReceived => "SYN-RECEIVED",
        tcp::State::Established => "ESTABLISHED",
        tcp::State::FinWait1 => "FIN-WAIT-1",
        tcp::State::FinWait2 => "FIN-WAIT-2",
        tcp::State::CloseWait => "CLOSE-WAIT",
        tcp::State::Closing => "CLOSING",
        tcp::State::LastAck => "LAST-ACK",
        tcp::State::TimeWait => "TIME-WAIT",
    }
}

/// Whether a local address is acceptable for binding: unspecified, or one
/// of the machine's addresses.
fn bind_ok(stack: &Stack, addr: IpAddr) -> bool {
    addr.is_unspecified() || stack.iface_ids().iter().any(|&id| stack.iface(id).unwrap().iface.has_ip_addr(addr))
}

/// Interfaces a socket bound to `local` should be present on.
fn ifaces_for(stack: &Stack, local: IpAddr) -> Vec<IfaceId> {
    stack
        .iface_ids()
        .into_iter()
        .filter(|&id| local.is_unspecified() || stack.iface(id).unwrap().iface.has_ip_addr(local))
        .collect()
}

impl Stack {
    fn alloc_port(&mut self, proto: Proto) -> Result<u16, NetError> {
        let start = self.random_u32() % EPHEMERAL_COUNT;
        for k in 0..EPHEMERAL_COUNT {
            let port = EPHEMERAL_FIRST + ((start + k) % EPHEMERAL_COUNT) as u16;
            if !self.socks.in_use(proto, port) {
                return Ok(port);
            }
        }
        Err(NetError::AddressInUse)
    }

    fn check_limit(&self) -> Result<(), NetError> {
        if self.socks.map.len() >= MAX_SOCKETS { Err(NetError::LimitReached) } else { Ok(()) }
    }

    // ---------------------------------------------------------------
    // TCP

    /// Starts a TCP connection to `remote`.
    pub fn tcp_connect(&mut self, remote: SocketAddr, opts: &TcpOptions, owner: u64) -> Result<SockId, NetError> {
        if remote.port() == 0 {
            return Err(NetError::InvalidArgument);
        }
        self.check_limit()?;
        let id_if = self.route(remote.ip())?;
        let port = self.alloc_port(Proto::Tcp)?;
        let now = self.now_us;
        let i = self.iface_mut(id_if).ok_or(NetError::NoRoute)?;
        let mut s = tcp_socket();
        apply_options(&mut s, opts);
        s.connect(i.iface.context(), IpEndpoint::from(remote), port).map_err(|e| match e {
            tcp::ConnectError::Unaddressable => NetError::NoRoute,
            tcp::ConnectError::InvalidState => NetError::Internal,
        })?;
        let handle = i.sockets.add(s);
        self.socks.reserve(Proto::Tcp, port);
        let timeout = if opts.connect_timeout_ms > 0 {
            opts.connect_timeout_ms as u64 * 1000
        } else {
            DEFAULT_CONNECT_TIMEOUT_US
        };
        Ok(self.socks.insert(Entry::Tcp(TcpSock {
            iface: id_if,
            handle: Some(handle),
            port,
            phase: Phase::Connecting,
            connect_deadline_us: now + timeout,
            closed_by_us: false,
            orphan: false,
            progress_us: now,
            last_rx_queue: 0,
            last_tx_queue: 0,
            error: None,
            local: None,
            remote: Some(remote),
            idle_timeout_us: opts.idle_timeout_ms as u64 * 1000,
            owner,
            peer_fin: false,
        })))
    }

    /// Listens on `local` (port 0 picks a free one). Returns the listener
    /// and the address bound.
    pub fn tcp_listen(
        &mut self,
        local: SocketAddr,
        backlog: u32,
        opts: &TcpOptions,
        owner: u64,
    ) -> Result<(SockId, SocketAddr), NetError> {
        self.check_limit()?;
        if !bind_ok(self, local.ip()) {
            return Err(NetError::AddressNotAvailable);
        }
        let port = if local.port() == 0 { self.alloc_port(Proto::Tcp)? } else { local.port() };
        let listening = self.socks.map.values().any(|e| matches!(e, Entry::Listener(l) if l.local.port() == port));
        if listening {
            return Err(NetError::AddressInUse);
        }
        let local = SocketAddr::new(local.ip(), port);
        let backlog = (backlog as usize).clamp(1, LISTEN_PER_IFACE);
        let mut l = Listener { local, backlog, members: Vec::new(), accepted: VecDeque::new(), opts: *opts, owner };
        for id in ifaces_for(self, local.ip()) {
            self.add_listen_members(&mut l, id);
        }
        if l.members.is_empty() {
            return Err(NetError::AddressNotAvailable);
        }
        self.socks.reserve(Proto::Tcp, port);
        Ok((self.socks.insert(Entry::Listener(l)), local))
    }

    fn add_listen_members(&mut self, l: &mut Listener, id: IfaceId) {
        let have = l.members.iter().filter(|(i, _)| *i == id).count();
        let Some(i) = self.iface_mut(id) else { return };
        for _ in have..l.backlog {
            let mut s = tcp_socket();
            apply_options(&mut s, &l.opts);
            let ep = if l.local.ip().is_unspecified() {
                IpListenEndpoint { addr: None, port: l.local.port() }
            } else {
                IpListenEndpoint { addr: Some(l.local.ip().into()), port: l.local.port() }
            };
            if s.listen(ep).is_ok() {
                l.members.push((id, i.sockets.add(s)));
            }
        }
    }

    /// Takes the next accepted connection of a listener.
    pub fn tcp_accept(&mut self, listener: SockId) -> Option<SockId> {
        match self.socks.map.get_mut(&listener) {
            Some(Entry::Listener(l)) => l.accepted.pop_front(),
            _ => None,
        }
    }

    fn tcp_sock(&self, id: SockId) -> Option<&TcpSock> {
        match self.socks.map.get(&id) {
            Some(Entry::Tcp(t)) => Some(t),
            _ => None,
        }
    }

    fn with_tcp<R>(&mut self, id: SockId, f: impl FnOnce(&mut tcp::Socket<'static>) -> R) -> Option<R> {
        let (iface, handle) = match self.socks.map.get(&id) {
            Some(Entry::Tcp(t)) => (t.iface, t.handle?),
            _ => return None,
        };
        let i = self.iface_mut(iface)?;
        Some(f(i.sockets.get_mut::<tcp::Socket>(handle)))
    }

    /// The client's view of a TCP connection.
    pub fn tcp_view(&mut self, id: SockId) -> Option<TcpView> {
        let t = self.tcp_sock(id)?;
        let (phase, error, local, remote, peer_fin) = (t.phase, t.error, t.local, t.remote, t.peer_fin);
        let numbers =
            self.with_tcp(id, |s| (s.recv_queue(), s.send_capacity() - s.send_queue(), s.send_queue(), s.may_send()));
        let (recv_queued, send_space, send_queued, may_send) = numbers.unwrap_or((0, 0, 0, false));
        let state = match phase {
            Phase::Connecting => TcpState::Connecting,
            Phase::Open => match (local, remote) {
                (Some(local), Some(remote)) => TcpState::Open { local, remote },
                _ => TcpState::Connecting,
            },
            Phase::Closed => TcpState::Closed { error },
        };
        Some(TcpView {
            state,
            recv_queued,
            eof: peer_fin && recv_queued == 0,
            send_space: if may_send { send_space } else { 0 },
            send_queued,
        })
    }

    /// Appends data to a connection's send buffer; returns how much fit.
    pub fn tcp_send(&mut self, id: SockId, data: &[u8]) -> Result<usize, NetError> {
        let t = self.tcp_sock(id).ok_or(NetError::Closed)?;
        if t.phase == Phase::Closed {
            return Err(t.error.unwrap_or(NetError::Closed));
        }
        match self.with_tcp(id, |s| s.send_slice(data)) {
            Some(Ok(n)) => Ok(n),
            Some(Err(_)) | None => Err(NetError::Closed),
        }
    }

    /// Takes received data.
    pub fn tcp_recv(&mut self, id: SockId, buf: &mut [u8]) -> Result<usize, NetError> {
        match self.with_tcp(id, |s| if s.can_recv() { s.recv_slice(buf).unwrap_or(0) } else { 0 }) {
            Some(n) => Ok(n),
            None => Err(NetError::Closed),
        }
    }

    /// Finishes sending (FIN); receiving continues.
    pub fn tcp_shutdown(&mut self, id: SockId) {
        if let Some(Entry::Tcp(t)) = self.socks.map.get_mut(&id) {
            t.closed_by_us = true;
        }
        self.with_tcp(id, |s| s.close());
    }

    /// Drops a connection at once (RST).
    pub fn tcp_abort(&mut self, id: SockId) {
        if let Some(Entry::Tcp(t)) = self.socks.map.get_mut(&id) {
            t.closed_by_us = true;
        }
        self.with_tcp(id, |s| s.abort());
    }

    pub fn tcp_set_nodelay(&mut self, id: SockId, nodelay: bool) {
        self.with_tcp(id, |s| s.set_nagle_enabled(!nodelay));
    }

    // ---------------------------------------------------------------
    // UDP

    /// Opens a UDP socket bound to `local` (port 0 picks a free one).
    pub fn udp_bind(&mut self, local: SocketAddr, owner: u64) -> Result<(SockId, SocketAddr), NetError> {
        self.check_limit()?;
        if !bind_ok(self, local.ip()) {
            return Err(NetError::AddressNotAvailable);
        }
        let port = if local.port() == 0 { self.alloc_port(Proto::Udp)? } else { local.port() };
        if self.socks.in_use(Proto::Udp, port) {
            return Err(NetError::AddressInUse);
        }
        let local = SocketAddr::new(local.ip(), port);
        let mut u = UdpSock { local, members: Vec::new(), owner };
        for id in ifaces_for(self, local.ip()) {
            self.add_udp_member(&mut u, id);
        }
        self.socks.reserve(Proto::Udp, port);
        Ok((self.socks.insert(Entry::Udp(u)), local))
    }

    fn add_udp_member(&mut self, u: &mut UdpSock, id: IfaceId) {
        if u.members.iter().any(|(i, _)| *i == id) {
            return;
        }
        let Some(i) = self.iface_mut(id) else { return };
        let mut s = udp_socket();
        let ep = if u.local.ip().is_unspecified() {
            IpListenEndpoint { addr: None, port: u.local.port() }
        } else {
            IpListenEndpoint { addr: Some(u.local.ip().into()), port: u.local.port() }
        };
        if s.bind(ep).is_ok() {
            u.members.push((id, i.sockets.add(s)));
        }
    }

    /// Sends a datagram. `Err(LimitReached)` means the send buffer is full
    /// (try again after a poll).
    pub fn udp_send(&mut self, id: SockId, dest: SocketAddr, data: &[u8]) -> Result<(), NetError> {
        if dest.port() == 0 || data.len() > vproto::net::MAX_DATAGRAM {
            return Err(NetError::InvalidArgument);
        }
        let via = self.route(dest.ip())?;
        let member = match self.socks.map.get(&id) {
            Some(Entry::Udp(u)) => u.members.iter().find(|(i, _)| *i == via).map(|(_, h)| *h),
            _ => return Err(NetError::Closed),
        };
        let handle = member.ok_or(NetError::NoRoute)?;
        let i = self.iface_mut(via).ok_or(NetError::NoRoute)?;
        let s = i.sockets.get_mut::<udp::Socket>(handle);
        if data.len() > s.payload_send_capacity() {
            return Err(NetError::MessageTooLarge);
        }
        s.send_slice(data, IpEndpoint::from(dest)).map_err(|e| match e {
            udp::SendError::BufferFull => NetError::LimitReached,
            udp::SendError::Unaddressable => NetError::NoRoute,
        })
    }

    /// Takes the next received datagram.
    pub fn udp_recv(&mut self, id: SockId, buf: &mut [u8]) -> Option<UdpDatagram> {
        let members = match self.socks.map.get(&id) {
            Some(Entry::Udp(u)) => u.members.clone(),
            _ => return None,
        };
        for (iface, handle) in members {
            let Some(i) = self.iface_mut(iface) else { continue };
            let s = i.sockets.get_mut::<udp::Socket>(handle);
            if s.can_recv()
                && let Ok((n, meta)) = s.recv_slice(buf)
            {
                return Some(UdpDatagram { from: SocketAddr::from(meta.endpoint), len: n });
            }
        }
        None
    }

    // ---------------------------------------------------------------
    // Ping

    /// Opens a ping socket.
    pub fn ping_open(&mut self, owner: u64) -> Result<SockId, NetError> {
        self.check_limit()?;
        let ident = self.random_u32() as u16;
        Ok(self.socks.insert(Entry::Ping(PingSock {
            ident,
            members: Vec::new(),
            pending: BTreeMap::new(),
            events: VecDeque::new(),
            owner,
        })))
    }

    /// Sends an ICMP echo request with `size` payload bytes.
    pub fn ping_send(&mut self, id: SockId, dest: IpAddr, seq: u16, ttl: u8, size: u16) -> Result<(), NetError> {
        if size as usize > 8192 {
            return Err(NetError::MessageTooLarge);
        }
        let via = self.route(dest)?;
        let now = self.now_us;
        let (ident, handle) = {
            let ident = match self.socks.map.get(&id) {
                Some(Entry::Ping(p)) => p.ident,
                _ => return Err(NetError::Closed),
            };
            let existing = match self.socks.map.get(&id) {
                Some(Entry::Ping(p)) => p.members.iter().find(|(i, _)| *i == via).map(|(_, h)| *h),
                _ => None,
            };
            let handle = match existing {
                Some(h) => h,
                None => {
                    let i = self.iface_mut(via).ok_or(NetError::NoRoute)?;
                    let mut s = icmp_socket();
                    s.bind(icmp::Endpoint::Ident(ident)).map_err(|_| NetError::Internal)?;
                    let h = i.sockets.add(s);
                    if let Some(Entry::Ping(p)) = self.socks.map.get_mut(&id) {
                        p.members.push((via, h));
                    }
                    h
                }
            };
            (ident, handle)
        };
        let payload: Vec<u8> = (0..size).map(|n| (n as u8).wrapping_add(0x20)).collect();
        let i = self.iface_mut(via).ok_or(NetError::NoRoute)?;
        let caps = ChecksumCapabilities::default();
        let src6 = match dest {
            IpAddr::V6(d) => Some(i.iface.get_source_address_ipv6(&d)),
            IpAddr::V4(_) => None,
        };
        let s = i.sockets.get_mut::<icmp::Socket>(handle);
        s.set_hop_limit(if ttl == 0 { None } else { Some(ttl) });
        let sent = match dest {
            IpAddr::V4(d) => {
                let repr = Icmpv4Repr::EchoRequest { ident, seq_no: seq, data: &payload };
                s.send(repr.buffer_len(), IpAddress::Ipv4(d)).map(|buf| {
                    repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &caps);
                })
            }
            IpAddr::V6(d) => {
                let repr = Icmpv6Repr::EchoRequest { ident, seq_no: seq, data: &payload };
                let src = src6.unwrap_or(Ipv6Addr::UNSPECIFIED);
                s.send(repr.buffer_len(), IpAddress::Ipv6(d)).map(|buf| {
                    repr.emit(&src, &d, &mut Icmpv6Packet::new_unchecked(buf), &caps);
                })
            }
        };
        sent.map_err(|e| match e {
            icmp::SendError::BufferFull => NetError::LimitReached,
            icmp::SendError::Unaddressable => NetError::NoRoute,
        })?;
        if let Some(Entry::Ping(p)) = self.socks.map.get_mut(&id) {
            p.pending.insert(seq, (now, dest));
        }
        Ok(())
    }

    /// Takes the ping results since the last call.
    pub fn ping_events(&mut self, id: SockId) -> Vec<PingEvent> {
        match self.socks.map.get_mut(&id) {
            Some(Entry::Ping(p)) => p.events.drain(..).collect(),
            _ => Vec::new(),
        }
    }

    // ---------------------------------------------------------------
    // Closing and housekeeping

    /// The client closed the socket. TCP connections are shut down
    /// gracefully and disappear once closed; everything else goes now.
    pub fn close(&mut self, id: SockId) {
        let Some(entry) = self.socks.map.get_mut(&id) else { return };
        if let Entry::Tcp(t) = entry
            && t.phase != Phase::Closed
            && t.handle.is_some()
        {
            t.orphan = true;
            t.closed_by_us = true;
            self.with_tcp(id, |s| s.close());
            return;
        }
        self.remove_socket(id);
    }

    fn remove_socket(&mut self, id: SockId) {
        let Some(entry) = self.socks.map.remove(&id) else { return };
        match entry {
            Entry::Tcp(t) => {
                if let Some(h) = t.handle
                    && let Some(i) = self.iface_mut(t.iface)
                {
                    i.sockets.remove(h);
                }
                self.socks.release(Proto::Tcp, t.port);
            }
            Entry::Listener(l) => {
                for (iface, h) in l.members {
                    if let Some(i) = self.iface_mut(iface) {
                        i.sockets.remove(h);
                    }
                }
                for a in l.accepted {
                    self.remove_socket(a);
                }
                self.socks.release(Proto::Tcp, l.local.port());
            }
            Entry::Udp(u) => {
                for (iface, h) in u.members {
                    if let Some(i) = self.iface_mut(iface) {
                        i.sockets.remove(h);
                    }
                }
                self.socks.release(Proto::Udp, u.local.port());
            }
            Entry::Ping(p) => {
                for (iface, h) in p.members {
                    if let Some(i) = self.iface_mut(iface) {
                        i.sockets.remove(h);
                    }
                }
            }
        }
    }

    /// Marks a TCP connection closed with an error and frees its smoltcp
    /// socket.
    fn fail_tcp(&mut self, id: SockId, error: NetError) {
        let (iface, handle, orphan) = match self.socks.map.get_mut(&id) {
            Some(Entry::Tcp(t)) => {
                t.phase = Phase::Closed;
                t.error.get_or_insert(error);
                (t.iface, t.handle.take(), t.orphan)
            }
            _ => return,
        };
        if let Some(h) = handle
            && let Some(i) = self.iface_mut(iface)
        {
            i.sockets.get_mut::<tcp::Socket>(h).abort();
            i.sockets.remove(h);
        }
        if orphan {
            self.remove_socket(id);
        }
    }

    /// Socket housekeeping after the interfaces ran.
    pub(crate) fn socks_maintain(&mut self) {
        let now = self.now_us;
        let ids: Vec<SockId> = self.socks.map.keys().copied().collect();
        for id in ids {
            match self.socks.map.get(&id) {
                Some(Entry::Tcp(_)) => self.maintain_tcp(id, now),
                Some(Entry::Listener(_)) => self.maintain_listener(id, now),
                Some(Entry::Ping(_)) => self.maintain_ping(id, now),
                _ => {}
            }
        }
    }

    fn maintain_tcp(&mut self, id: SockId, now: u64) {
        let Some(snapshot) = self.with_tcp(id, |s| {
            (s.state(), s.local_endpoint(), s.remote_endpoint(), s.recv_queue(), s.send_queue(), s.may_recv())
        }) else {
            // Already closed (no smoltcp socket); orphans are removed.
            if matches!(self.socks.map.get(&id), Some(Entry::Tcp(t)) if t.orphan) {
                self.remove_socket(id);
            }
            return;
        };
        let (state, local, remote, rxq, txq, may_recv) = snapshot;
        let Some(Entry::Tcp(t)) = self.socks.map.get_mut(&id) else { return };
        if rxq != t.last_rx_queue || txq < t.last_tx_queue {
            t.progress_us = now;
        }
        t.last_rx_queue = rxq;
        t.last_tx_queue = txq;
        if let (Some(l), Some(r)) = (local, remote) {
            t.local = Some(l.into());
            t.remote = Some(r.into());
        }
        let established =
            !matches!(state, tcp::State::Closed | tcp::State::Listen | tcp::State::SynSent | tcp::State::SynReceived);
        if t.phase == Phase::Connecting && established {
            t.phase = Phase::Open;
            t.progress_us = now;
        }
        if established && !may_recv {
            t.peer_fin = true;
        }
        match state {
            tcp::State::Closed => {
                let error = match t.phase {
                    Phase::Connecting => Some(if now >= t.connect_deadline_us {
                        NetError::TimedOut
                    } else {
                        NetError::ConnectionRefused
                    }),
                    Phase::Open if t.closed_by_us => None,
                    Phase::Open => {
                        Some(if now.saturating_sub(t.progress_us) + 1_000_000 >= TCP_USER_TIMEOUT.total_micros() {
                            NetError::TimedOut
                        } else {
                            NetError::ConnectionReset
                        })
                    }
                    Phase::Closed => t.error,
                };
                match error {
                    Some(e) => self.fail_tcp(id, e),
                    None => {
                        // Closed normally.
                        let orphan = t.orphan;
                        t.phase = Phase::Closed;
                        let (iface, handle) = (t.iface, t.handle.take());
                        if let Some(h) = handle
                            && let Some(i) = self.iface_mut(iface)
                        {
                            i.sockets.remove(h);
                        }
                        if orphan {
                            self.remove_socket(id);
                        }
                    }
                }
            }
            tcp::State::TimeWait if t.orphan => {
                // Nothing left to deliver: free it now (the peer saw our ACK).
                self.remove_socket(id);
            }
            tcp::State::SynSent if now >= t.connect_deadline_us => self.fail_tcp(id, NetError::TimedOut),
            _ if t.idle_timeout_us > 0 && established && now.saturating_sub(t.progress_us) >= t.idle_timeout_us => {
                self.fail_tcp(id, NetError::TimedOut)
            }
            _ => {}
        }
    }

    fn maintain_listener(&mut self, id: SockId, now: u64) {
        let Some(Entry::Listener(l)) = self.socks.map.get_mut(&id) else { return };
        let members = core::mem::take(&mut l.members);
        let owner = l.owner;
        let mut keep = Vec::new();
        let mut accepted = Vec::new();
        for (iface, h) in members {
            let Some(i) = self.iface_mut(iface) else { continue };
            let s = i.sockets.get::<tcp::Socket>(h);
            match s.state() {
                tcp::State::Listen | tcp::State::SynReceived => keep.push((iface, h)),
                tcp::State::Closed => {
                    i.sockets.remove(h);
                }
                _ => accepted.push((iface, h, s.local_endpoint(), s.remote_endpoint())),
            }
        }
        let mut new_ids = Vec::new();
        for (iface, h, local, remote) in accepted {
            let new = self.socks.insert(Entry::Tcp(TcpSock {
                iface,
                handle: Some(h),
                port: 0,
                phase: Phase::Open,
                connect_deadline_us: now,
                closed_by_us: false,
                orphan: false,
                progress_us: now,
                last_rx_queue: 0,
                last_tx_queue: 0,
                error: None,
                local: local.map(SocketAddr::from),
                remote: remote.map(SocketAddr::from),
                idle_timeout_us: 0,
                owner,
                peer_fin: false,
            }));
            new_ids.push(new);
        }
        let Some(Entry::Listener(l)) = self.socks.map.get_mut(&id) else { return };
        l.members = keep;
        l.accepted.extend(new_ids);
        let mut l = match self.socks.map.remove(&id) {
            Some(Entry::Listener(l)) => l,
            _ => return,
        };
        // Replace the sockets that turned into connections.
        for iface in ifaces_for(self, l.local.ip()) {
            self.add_listen_members(&mut l, iface);
        }
        self.socks.map.insert(id, Entry::Listener(l));
    }

    fn maintain_ping(&mut self, id: SockId, now: u64) {
        let Some(Entry::Ping(p)) = self.socks.map.get(&id) else { return };
        let ident = p.ident;
        let members = p.members.clone();
        let mut replies = Vec::new();
        let mut buf = alloc::vec![0u8; ICMP_BYTES];
        for (iface, h) in members {
            let Some(i) = self.iface_mut(iface) else { continue };
            let s = i.sockets.get_mut::<icmp::Socket>(h);
            while s.can_recv() {
                let Ok((n, from)) = s.recv_slice(&mut buf) else { break };
                let caps = ChecksumCapabilities::default();
                let parsed = match from {
                    IpAddress::Ipv4(_) => Icmpv4Packet::new_checked(&buf[..n])
                        .ok()
                        .and_then(|p| Icmpv4Repr::parse(&p, &caps).ok())
                        .and_then(|r| match r {
                            Icmpv4Repr::EchoReply { ident: id_, seq_no, data } if id_ == ident => {
                                Some((seq_no, data.len()))
                            }
                            _ => None,
                        }),
                    IpAddress::Ipv6(_) => Icmpv6Packet::new_checked(&buf[..n]).ok().and_then(|p| {
                        // The checksum was verified when the packet arrived.
                        match p.msg_type() {
                            smoltcp::wire::Icmpv6Message::EchoReply if p.echo_ident() == ident => {
                                Some((p.echo_seq_no(), p.payload().len()))
                            }
                            _ => None,
                        }
                    }),
                };
                if let Some((seq, size)) = parsed {
                    replies.push((IpAddr::from(from), seq, size));
                }
            }
        }
        let Some(Entry::Ping(p)) = self.socks.map.get_mut(&id) else { return };
        for (from, seq, size) in replies {
            if let Some((sent, _)) = p.pending.remove(&seq) {
                let rtt = now.saturating_sub(sent).min(u32::MAX as u64) as u32;
                p.events.push_back(PingEvent::Reply { from, seq, size: size as u16, rtt_us: rtt });
            }
        }
        let expired: Vec<u16> =
            p.pending.iter().filter(|(_, (t, _))| now.saturating_sub(*t) >= PING_TIMEOUT_US).map(|(s, _)| *s).collect();
        for seq in expired {
            p.pending.remove(&seq);
            p.events.push_back(PingEvent::Failed { seq, error: NetError::TimedOut });
        }
    }

    /// The earliest socket timer.
    pub(crate) fn socks_deadline(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        let mut take = |t: u64| best = Some(best.map_or(t, |b| b.min(t)));
        for e in self.socks.map.values() {
            match e {
                Entry::Tcp(t) if t.phase == Phase::Connecting => take(t.connect_deadline_us),
                Entry::Tcp(t) if t.phase == Phase::Open && t.idle_timeout_us > 0 => {
                    take(t.progress_us + t.idle_timeout_us)
                }
                Entry::Ping(p) => {
                    for (sent, _) in p.pending.values() {
                        take(sent + PING_TIMEOUT_US);
                    }
                }
                _ => {}
            }
        }
        best
    }

    /// A new interface: sockets bound to the unspecified address join it.
    pub(crate) fn socks_on_iface_added(&mut self, id: IfaceId) {
        let ids: Vec<SockId> = self.socks.map.keys().copied().collect();
        for sid in ids {
            match self.socks.map.remove(&sid) {
                Some(Entry::Udp(mut u)) => {
                    if u.local.ip().is_unspecified() {
                        self.add_udp_member(&mut u, id);
                    }
                    self.socks.map.insert(sid, Entry::Udp(u));
                }
                Some(Entry::Listener(mut l)) => {
                    if l.local.ip().is_unspecified() {
                        self.add_listen_members(&mut l, id);
                    }
                    self.socks.map.insert(sid, Entry::Listener(l));
                }
                Some(other) => {
                    self.socks.map.insert(sid, other);
                }
                None => {}
            }
        }
    }

    /// An interface is going away: its connections fail and other sockets
    /// leave it.
    pub(crate) fn socks_on_iface_removed(&mut self, id: IfaceId) {
        let ids: Vec<SockId> = self.socks.map.keys().copied().collect();
        for sid in ids {
            let on_iface = matches!(self.socks.map.get(&sid), Some(Entry::Tcp(t)) if t.iface == id);
            if on_iface {
                self.fail_tcp(sid, NetError::Aborted);
                continue;
            }
            match self.socks.map.get_mut(&sid) {
                Some(Entry::Udp(u)) => u.members.retain(|(i, _)| *i != id),
                Some(Entry::Listener(l)) => l.members.retain(|(i, _)| *i != id),
                Some(Entry::Ping(p)) => p.members.retain(|(i, _)| *i != id),
                _ => {}
            }
        }
    }

    /// A local address disappeared: connections using it fail.
    pub(crate) fn socks_address_lost(&mut self, addr: IpAddr) {
        let ids: Vec<SockId> = self
            .socks
            .map
            .iter()
            .filter(|(_, e)| matches!(e, Entry::Tcp(t) if t.local.is_some_and(|l| l.ip() == addr) && t.phase != Phase::Closed))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.fail_tcp(id, NetError::NetworkDown);
        }
    }

    /// Records the owning process of a socket (for listings).
    pub fn set_owner(&mut self, id: SockId, owner: u64) {
        match self.socks.map.get_mut(&id) {
            Some(Entry::Tcp(t)) => t.owner = owner,
            Some(Entry::Listener(l)) => l.owner = owner,
            Some(Entry::Udp(u)) => u.owner = owner,
            Some(Entry::Ping(p)) => p.owner = owner,
            None => {}
        }
    }

    /// Whether a socket id is still in the table.
    pub fn socket_exists(&self, id: SockId) -> bool {
        self.socks.map.contains_key(&id)
    }

    /// Every socket, for `netstat`.
    pub fn socket_infos(&self) -> Vec<SocketInfo> {
        let mut out = Vec::new();
        for e in self.socks.map.values() {
            match e {
                Entry::Tcp(t) => {
                    let (state, rxq, txq) =
                        match t.handle.and_then(|h| self.iface(t.iface).map(|i| i.sockets.get::<tcp::Socket>(h))) {
                            Some(s) => (state_name(s.state()), s.recv_queue(), s.send_queue()),
                            None => ("CLOSED", 0, 0),
                        };
                    out.push(SocketInfo {
                        kind: SocketKind::Tcp,
                        local: t.local,
                        remote: t.remote,
                        state: state.into(),
                        owner: t.owner,
                        interface: self.iface(t.iface).map(|i| i.name.clone()).unwrap_or_default(),
                        rx_queued: rxq as u32,
                        tx_queued: txq as u32,
                    });
                }
                Entry::Listener(l) => out.push(SocketInfo {
                    kind: SocketKind::TcpListener,
                    local: Some(l.local),
                    remote: None,
                    state: "LISTEN".into(),
                    owner: l.owner,
                    interface: String::new(),
                    rx_queued: l.accepted.len() as u32,
                    tx_queued: 0,
                }),
                Entry::Udp(u) => out.push(SocketInfo {
                    kind: SocketKind::Udp,
                    local: Some(u.local),
                    remote: None,
                    state: String::new(),
                    owner: u.owner,
                    interface: String::new(),
                    rx_queued: 0,
                    tx_queued: 0,
                }),
                Entry::Ping(p) => out.push(SocketInfo {
                    kind: SocketKind::Ping,
                    local: None,
                    remote: None,
                    state: String::new(),
                    owner: p.owner,
                    interface: String::new(),
                    rx_queued: p.events.len() as u32,
                    tx_queued: p.pending.len() as u32,
                }),
            }
        }
        out
    }
}

/// The unspecified address of a family.
pub fn unspecified(v6: bool) -> IpAddr {
    if v6 { IpAddr::V6(Ipv6Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::UNSPECIFIED) }
}
