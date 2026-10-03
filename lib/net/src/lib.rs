//! `vnet` — networking for Vindows applications.
//!
//! Blocking, `std::net`-like types on top of the network service's socket
//! protocol (`vproto::net`):
//!
//! * [`TcpStream`] and [`TcpListener`] — connections, with timeouts;
//!   [`TcpStream::connect_host`] resolves a name and tries each address.
//! * [`UdpSocket`] — datagrams.
//! * [`resolve`] / [`lookup`] — name resolution through the system's
//!   caching resolver.
//! * [`Pinger`] — ICMP echo (what `ping` uses).
//! * [`http`] — a small HTTP/1.1 client (plain HTTP; HTTPS arrives with TLS
//!   in a later version, layered on [`TcpStream`]).
//! * [`status`], [`interfaces`], ... — the state of the network, and
//!   configuration for the Settings app and the Terminal.
//! * [`wifi`] — Wi-Fi: networks in range, joining, saved networks, events.
//!
//! Addresses are the standard `core::net` types. Errors are
//! [`NetError`]s. Every socket owns a channel to the network service and is
//! closed when dropped (TCP connections are shut down gracefully).
//!
//! Applications with an event loop (like `vui` apps) can wait on
//! [`TcpStream::handle`] and call the non-blocking methods
//! ([`TcpStream::try_read`], [`TcpStream::poll_connect`]) when it is
//! readable.

#![no_std]

extern crate alloc;

pub mod http;
mod ping;
mod tcp;
mod udp;
pub mod wifi;

use alloc::string::String;
use alloc::vec::Vec;

pub use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
pub use ping::{PingReply, Pinger};
pub use tcp::{TcpListener, TcpStream};
pub use udp::UdpSocket;
pub use vproto::net::{
    AddrFamily, Connectivity, DnsCacheEntry, InterfaceInfo, InterfaceKind, IpConfig, NetError, NetEvent, NetStatus,
    RouteInfo, SocketInfo, TcpOptions,
};
pub use vrt::time::Duration;

use vipc::{Encode, Encoder, IpcError};
use vproto::net::{ResolveResult, SocketRequest, net};
use vrt::object::Channel;
use vrt::sync::Mutex;

/// Default time to wait for a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest a lookup may take (the service gives up after ten seconds).
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

static CLIENT: Mutex<Option<net::Client>> = Mutex::new(None);

/// Whether the network service is running (connecting to a service that
/// is not registered would wait until it is).
pub fn available() -> bool {
    vproto::with_registry(|r| r.list().map(|l| l.iter().any(|n| n == net::NAME)).unwrap_or(false)).unwrap_or(false)
}

/// Runs `f` with the connection to the network service, reconnecting once
/// if the service restarted.
pub(crate) fn with_client<R>(mut f: impl FnMut(&net::Client) -> Result<R, IpcError>) -> Result<R, NetError> {
    let mut slot = CLIENT.lock();
    for _ in 0..2 {
        if slot.is_none() {
            if !available() {
                return Err(NetError::Unavailable);
            }
            let ch = vproto::connect(net::NAME).map_err(|_| NetError::Unavailable)?;
            *slot = Some(net::Client::new(ch));
        }
        match f(slot.as_ref().unwrap()) {
            Ok(r) => return Ok(r),
            Err(IpcError::PeerClosed) => *slot = None,
            Err(_) => return Err(NetError::Internal),
        }
    }
    Err(NetError::Unavailable)
}

/// Sends a request on a socket channel.
pub(crate) fn send_request(ch: &Channel, req: SocketRequest) -> Result<(), NetError> {
    let mut e = Encoder::with_header(0, 0, vipc::FLAG_EVENT);
    req.encode(&mut e);
    loop {
        match ch.write(&e.bytes, Vec::new()) {
            Ok(()) => return Ok(()),
            Err(vabi::Error::ShouldWait) => {
                let s = ch.wait(vabi::signals::WRITABLE | vabi::signals::PEER_CLOSED, vabi::DEADLINE_INFINITE);
                if s.is_ok_and(|s| s & vabi::signals::PEER_CLOSED != 0) {
                    return Err(NetError::Closed);
                }
            }
            Err(_) => return Err(NetError::Closed),
        }
    }
}

/// A deadline `timeout` from now (`None`: forever).
pub(crate) fn deadline(timeout: Option<Duration>) -> u64 {
    match timeout {
        Some(t) => vrt::time::deadline_after(t),
        None => vabi::DEADLINE_INFINITE,
    }
}

/// The result of a lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    /// Addresses in order of preference.
    pub addresses: Vec<IpAddr>,
    /// The canonical name (after CNAMEs).
    pub canonical: String,
    /// Seconds the answer may be cached.
    pub ttl_s: u32,
}

/// Looks up a host name (or parses an address literal).
pub fn lookup(name: &str, family: AddrFamily, timeout: Duration) -> Result<Lookup, NetError> {
    let name = String::from(name);
    // A fresh reply channel per attempt (a retry after a service restart
    // cannot reuse the one sent to the old instance).
    let ours = with_client(|c| {
        let (ours, theirs) = Channel::create().map_err(IpcError::Kernel)?;
        Ok(c.resolve(name.clone(), family, theirs)?.map(|()| ours))
    })??;
    let msg = ours.read_blocking(vrt::time::deadline_after(timeout)).map_err(|e| match e {
        vabi::Error::TimedOut => NetError::TimedOut,
        _ => NetError::Unavailable,
    })?;
    match vipc::decode_event::<ResolveResult>(msg) {
        Ok((_, ResolveResult::Found { addresses, ttl_s, canonical })) => Ok(Lookup { addresses, canonical, ttl_s }),
        Ok((_, ResolveResult::Failed { error })) => Err(error),
        Err(_) => Err(NetError::Internal),
    }
}

/// The addresses of a host (IPv4 first), or of an address literal.
pub fn resolve(name: &str) -> Result<Vec<IpAddr>, NetError> {
    lookup(name, AddrFamily::Any, RESOLVE_TIMEOUT).map(|l| l.addresses)
}

/// The overall network state.
pub fn status() -> Result<NetStatus, NetError> {
    with_client(|c| c.status())
}

pub fn interfaces() -> Result<Vec<InterfaceInfo>, NetError> {
    with_client(|c| c.interfaces())
}

pub fn routes() -> Result<Vec<RouteInfo>, NetError> {
    with_client(|c| c.routes())
}

/// Every open socket in the system.
pub fn sockets() -> Result<Vec<SocketInfo>, NetError> {
    with_client(|c| c.sockets())
}

pub fn dns_cache() -> Result<Vec<DnsCacheEntry>, NetError> {
    with_client(|c| c.dns_cache())
}

pub fn flush_dns_cache() -> Result<(), NetError> {
    with_client(|c| c.flush_dns_cache())
}

/// Sets how an interface obtains its IPv4 configuration.
pub fn configure(interface: &str, config: IpConfig) -> Result<(), NetError> {
    with_client(|c| c.configure(String::from(interface), config.clone()))?
}

/// Asks an interface's DHCP server for a new lease.
pub fn renew(interface: &str) -> Result<(), NetError> {
    with_client(|c| c.renew(String::from(interface)))?
}

/// Overrides the DNS servers (empty: automatic).
pub fn set_dns_servers(servers: &[IpAddr]) -> Result<(), NetError> {
    with_client(|c| c.set_dns_servers(servers.to_vec()))?
}

/// A channel that receives [`NetEvent`]s whenever the network changes. Wait
/// for it to be readable, then call [`next_event`].
pub fn watch() -> Result<Channel, NetError> {
    with_client(|c| {
        let (ours, theirs) = Channel::create().map_err(IpcError::Kernel)?;
        Ok(c.watch(theirs)?.map(|()| ours))
    })?
}

/// Takes the next event from a [`watch`] channel without blocking.
/// `Err(Closed)` means the service went away (watch again later).
pub fn next_event(ch: &Channel) -> Result<Option<NetEvent>, NetError> {
    match ch.read() {
        Ok(msg) => match vipc::decode_event::<NetEvent>(msg) {
            Ok((_, ev)) => Ok(Some(ev)),
            Err(_) => Ok(None),
        },
        Err(vabi::Error::ShouldWait) => Ok(None),
        Err(_) => Err(NetError::Closed),
    }
}

/// Formats a MAC address.
pub fn format_mac(m: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}
