//! The network protocols served by `netd`, the network service.
//!
//! * [`net`] (service `"net"`): what applications use — TCP and UDP sockets,
//!   ping, name resolution, the state of interfaces and routes, and (for
//!   the Settings app and the Terminal) interface configuration. Most
//!   applications use the `vnet` library, which wraps this protocol in
//!   `TcpStream`-like types.
//! * [`netdev`] (service `"netdev"`): network devices (Ethernet drivers, and
//!   the Wi-Fi service for each wireless interface) attach to the stack.
//!   Frames travel through a [`crate::netring::Link`], never in messages.
//!
//! # Sockets
//!
//! Every socket is its own channel. The client creates the channel pair,
//! keeps one end and passes the other in the request that opens the socket
//! ([`net::Client::tcp_connect`], ...). Both sides then exchange
//! [`SocketRequest`]s (client to `netd`) and [`SocketEvent`]s (`netd` to
//! client) as plain messages on that channel. Closing the channel closes the
//! socket (a TCP connection is shut down gracefully).
//!
//! Data flow is credit-based in both directions, which bounds the memory a
//! socket can pin in the kernel:
//!
//! * **Sending.** `netd` grants send credit in bytes
//!   ([`SocketEvent::Connected`], [`SocketEvent::SendCredit`]). A client may
//!   only send that many bytes; `netd` grants more as the data enters the
//!   stack.
//! * **Receiving.** `netd` keeps at most [`RECEIVE_WINDOW`] bytes of
//!   [`SocketEvent::Data`] (or datagrams) unacknowledged; the client returns
//!   credit with [`SocketRequest::Consumed`] as its reader takes the data.
//!   When the client stops reading, TCP's own flow control slows the peer.

use alloc::string::String;
use alloc::vec::Vec;
use core::net::{IpAddr, SocketAddr};

use vipc::{Bytes, enumeration, message, protocol, union};
use vrt::object::Channel;

use crate::netring::LinkEndpoints;

/// Largest payload of one [`SocketRequest::Send`] / `SendTo` message.
pub const MAX_SEND: usize = 16 * 1024;
/// Unacknowledged received bytes `netd` allows per socket.
pub const RECEIVE_WINDOW: u32 = 256 * 1024;
/// Largest UDP payload (an IPv4 datagram of 65535 bytes minus headers).
pub const MAX_DATAGRAM: usize = 65507;

enumeration! {
    /// Why a network operation failed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum NetError {
        /// The network service or the interface is not available.
        Unavailable = 1,
        /// No route to the destination network (not connected?).
        NoRoute = 2,
        /// The destination host did not answer address resolution.
        HostUnreachable = 3,
        /// The remote host refused the connection.
        ConnectionRefused = 4,
        /// The remote host reset the connection.
        ConnectionReset = 5,
        TimedOut = 6,
        /// The local address and port are already in use.
        AddressInUse = 7,
        /// The local address does not belong to this machine.
        AddressNotAvailable = 8,
        InvalidArgument = 9,
        /// The name does not exist (DNS NXDOMAIN) or has no addresses.
        NameNotFound = 10,
        /// The DNS servers failed or did not answer.
        DnsFailure = 11,
        /// Too many sockets.
        LimitReached = 12,
        /// The socket is closed.
        Closed = 13,
        PermissionDenied = 14,
        NotSupported = 15,
        /// The connection was dropped locally (for example its interface
        /// disappeared).
        Aborted = 16,
        /// The message is larger than the protocol allows.
        MessageTooLarge = 17,
        /// The interface is down (for example Wi-Fi disconnected).
        NetworkDown = 18,
        /// The destination is unreachable (an ICMP error).
        DestinationUnreachable = 19,
        /// A packet's time to live ran out (an ICMP error; used by ping).
        TimeExceeded = 20,
        /// The client broke the socket protocol (for example sent more than
        /// its credit).
        ProtocolViolation = 21,
        Internal = 22,
    }
}

impl NetError {
    pub const fn description(self) -> &'static str {
        match self {
            NetError::Unavailable => "the network is not available",
            NetError::NoRoute => "no route to the network",
            NetError::HostUnreachable => "host unreachable",
            NetError::ConnectionRefused => "connection refused",
            NetError::ConnectionReset => "connection reset by peer",
            NetError::TimedOut => "timed out",
            NetError::AddressInUse => "address already in use",
            NetError::AddressNotAvailable => "address not available",
            NetError::InvalidArgument => "invalid argument",
            NetError::NameNotFound => "name not found",
            NetError::DnsFailure => "DNS lookup failed",
            NetError::LimitReached => "too many sockets",
            NetError::Closed => "socket closed",
            NetError::PermissionDenied => "permission denied",
            NetError::NotSupported => "not supported",
            NetError::Aborted => "connection aborted",
            NetError::MessageTooLarge => "message too large",
            NetError::NetworkDown => "network is down",
            NetError::DestinationUnreachable => "destination unreachable",
            NetError::TimeExceeded => "time to live exceeded",
            NetError::ProtocolViolation => "socket protocol violation",
            NetError::Internal => "internal network error",
        }
    }
}

impl core::fmt::Display for NetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.description())
    }
}

enumeration! {
    /// Which address families a name lookup should return.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AddrFamily {
        /// IPv4 and IPv6 (IPv6 first when the machine has IPv6 routes).
        Any = 0,
        V4 = 4,
        V6 = 6,
    }
}

message! {
    /// Options for a TCP connection.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct TcpOptions {
        /// Send small segments immediately (disables Nagle's algorithm).
        pub nodelay: bool,
        /// Keep-alive probe interval in milliseconds (0 = off).
        pub keepalive_ms: u32,
        /// Give up connecting after this many milliseconds (0 = 20 s).
        pub connect_timeout_ms: u32,
        /// Abort the connection when it is idle (nothing received) this long
        /// in milliseconds (0 = never).
        pub idle_timeout_ms: u32,
    }
}

union! {
    /// Messages a client sends on a socket channel.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum SocketRequest {
        /// TCP: append bytes to the stream (at most [`MAX_SEND`] and the
        /// remaining send credit).
        1 => Send { data: Bytes },
        /// UDP: send one datagram (counts against the send credit).
        2 => SendTo { dest: SocketAddr, data: Bytes },
        /// The client has taken this many received bytes (returns receive
        /// credit).
        3 => Consumed { bytes: u32 },
        /// TCP: no more data will be sent (sends FIN; receiving continues).
        4 => Shutdown {},
        /// TCP: drop the connection at once (sends RST).
        5 => Abort {},
        /// TCP: change the Nagle setting.
        6 => SetNoDelay { nodelay: bool },
        /// Ping socket: send an ICMP echo request of `size` payload bytes.
        /// `ttl` 0 means the default (64).
        7 => Echo { dest: IpAddr, seq: u16, ttl: u8, size: u16 },
    }
}

union! {
    /// Messages `netd` sends on a socket channel.
    #[derive(Debug)]
    pub enum SocketEvent {
        /// TCP: the connection is established. `credit` is the initial send
        /// credit in bytes.
        1 => Connected { local: SocketAddr, remote: SocketAddr, credit: u32 },
        /// TCP: received bytes.
        2 => Data { data: Bytes },
        /// UDP: a received datagram.
        3 => Datagram { from: SocketAddr, data: Bytes },
        /// More send credit.
        4 => SendCredit { bytes: u32 },
        /// TCP: the peer finished sending (end of stream). Sending may
        /// continue.
        5 => Eof {},
        /// The socket failed and is closed (connection refused or reset,
        /// timeout, interface gone, ...).
        6 => Error { error: NetError },
        /// TCP listener: a new connection. `socket` speaks this same
        /// protocol and starts with `credit` bytes of send credit.
        7 => Accepted { socket: Channel, local: SocketAddr, remote: SocketAddr, credit: u32 },
        /// Ping socket: an echo reply. `rtt_us` is the round-trip time.
        8 => EchoReply { from: IpAddr, seq: u16, ttl: u8, size: u16, rtt_us: u32 },
        /// Ping socket: an echo request failed (`DestinationUnreachable`,
        /// `TimeExceeded`, `NoRoute`, `TimedOut`, ...).
        9 => EchoFailed { seq: u16, from: Option<IpAddr>, error: NetError },
        /// TCP: everything sent was acknowledged and the connection closed
        /// in both directions.
        10 => Finished {},
    }
}

union! {
    /// The answer to a name lookup ([`net::Client::resolve`]), sent once on
    /// the reply channel.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ResolveResult {
        /// The addresses (preferred first) and the time they may be cached.
        1 => Found { addresses: Vec<IpAddr>, ttl_s: u32, canonical: String },
        2 => Failed { error: NetError },
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InterfaceKind {
        Loopback = 1,
        Ethernet = 2,
        Wireless = 3,
    }
}

enumeration! {
    /// Where an address came from.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AddressOrigin {
        Static = 1,
        Dhcp = 2,
        /// IPv6 stateless autoconfiguration.
        Slaac = 3,
        /// IPv6 link-local (fe80::/64).
        LinkLocal = 4,
        Loopback = 5,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AddressInfo {
        pub address: IpAddr,
        pub prefix_len: u8,
        pub origin: AddressOrigin,
        /// Seconds the address stays valid (0 = forever).
        pub valid_s: u32,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DhcpState {
        /// DHCP is not used on this interface.
        Off = 0,
        /// Looking for a server.
        Discovering = 1,
        /// Waiting for the server to confirm an address.
        Requesting = 2,
        /// An address is leased.
        Bound = 3,
        /// No server answered (still retrying).
        NoServer = 4,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DhcpInfo {
        pub state: DhcpState,
        pub server: Option<IpAddr>,
        /// Length of the lease in seconds.
        pub lease_s: u32,
        /// Seconds since the lease was obtained.
        pub age_s: u32,
    }
}

message! {
    /// Traffic counters of an interface.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct InterfaceStats {
        pub rx_packets: u64,
        pub rx_bytes: u64,
        pub tx_packets: u64,
        pub tx_bytes: u64,
        /// Frames lost because a queue was full.
        pub rx_dropped: u64,
        pub tx_dropped: u64,
        /// Malformed frames.
        pub rx_errors: u64,
    }
}

union! {
    /// How an interface gets its IPv4 configuration.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum IpConfig {
        1 => Dhcp {},
        2 => Static { address: IpAddr, prefix_len: u8, gateway: Option<IpAddr>, dns: Vec<IpAddr> },
        /// No IPv4 address (IPv6 link-local and autoconfiguration only).
        3 => Disabled {},
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InterfaceInfo {
        /// `lo`, `eth0`, `wlan0`, ...
        pub name: String,
        pub kind: InterfaceKind,
        pub mac: [u8; 6],
        pub mtu: u32,
        /// The link is up (cable connected, Wi-Fi associated).
        pub link_up: bool,
        /// Driver (or service) behind the interface.
        pub driver: String,
        pub config: IpConfig,
        pub addresses: Vec<AddressInfo>,
        pub gateways: Vec<IpAddr>,
        pub dns: Vec<IpAddr>,
        pub dhcp: DhcpInfo,
        /// Route preference: lower wins (Ethernet 100, Wi-Fi 600).
        pub metric: u32,
        pub stats: InterfaceStats,
    }
}

enumeration! {
    /// How far the machine can reach.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Connectivity {
        /// No interface with an address.
        Offline = 0,
        /// An address, but no default route (local network only).
        Local = 1,
        /// A default route (whether the Internet answers is checked by
        /// diagnostics).
        Routable = 2,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct NetStatus {
        pub connectivity: Connectivity,
        /// Interface of the default route ("" if none).
        pub default_interface: String,
        pub default_gateway: Option<IpAddr>,
        pub dns_servers: Vec<IpAddr>,
        /// Seconds since boot when anything last changed.
        pub changed_s: u32,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RouteInfo {
        /// Destination network.
        pub destination: IpAddr,
        pub prefix_len: u8,
        /// Next hop (None: directly connected).
        pub gateway: Option<IpAddr>,
        pub interface: String,
        pub metric: u32,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SocketKind {
        Tcp = 1,
        TcpListener = 2,
        Udp = 3,
        Ping = 4,
    }
}

message! {
    /// A socket as listed by `netstat`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SocketInfo {
        pub kind: SocketKind,
        pub local: Option<SocketAddr>,
        pub remote: Option<SocketAddr>,
        /// TCP state name ("ESTABLISHED", ...) or "".
        pub state: String,
        /// Process id of the owner.
        pub owner: u64,
        pub interface: String,
        pub rx_queued: u32,
        pub tx_queued: u32,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DnsCacheEntry {
        pub name: String,
        pub addresses: Vec<IpAddr>,
        /// Seconds left before the entry expires.
        pub ttl_s: u32,
        /// A cached "name not found".
        pub negative: bool,
    }
}

union! {
    /// Notifications on a [`net::Client::watch`] channel.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum NetEvent {
        /// The overall status changed.
        1 => StatusChanged { status: NetStatus },
        /// An interface appeared, disappeared or changed (link, addresses).
        2 => InterfaceChanged { name: String },
    }
}

/// Event ordinal of [`NetEvent`]s on a watch channel.
pub const NET_EVENT: u32 = 1;

protocol! {
    /// The network service.
    pub mod net = "net" {
        /// Connects to `remote`. `socket` is the service's end of the new
        /// socket channel; the result arrives on it ([`SocketEvent::Connected`]
        /// or [`SocketEvent::Error`]). An immediate error means the request
        /// itself was rejected.
        1 => fn tcp_connect(remote: SocketAddr, options: TcpOptions, socket: Channel) -> Result<(), NetError>;
        /// Listens on `local` (port 0 picks a free port). Connections
        /// arrive as [`SocketEvent::Accepted`] on `listener`. Returns the
        /// address actually bound.
        2 => fn tcp_listen(local: SocketAddr, backlog: u32, options: TcpOptions, listener: Channel) -> Result<SocketAddr, NetError>;
        /// Opens a UDP socket bound to `local` (port 0 picks a free port).
        /// The initial send credit arrives as a [`SocketEvent::SendCredit`].
        3 => fn udp_bind(local: SocketAddr, socket: Channel) -> Result<SocketAddr, NetError>;
        /// Opens a ping socket (ICMP echo).
        4 => fn ping_open(socket: Channel) -> Result<(), NetError>;
        /// Looks up `name` (a host name, or an address literal). One
        /// [`ResolveResult`] arrives on `reply` as an event with ordinal
        /// [`RESOLVE_RESULT`].
        5 => fn resolve(name: String, family: AddrFamily, reply: Channel) -> Result<(), NetError>;

        /// The overall state.
        10 => fn status() -> NetStatus;
        11 => fn interfaces() -> Vec<InterfaceInfo>;
        12 => fn routes() -> Vec<RouteInfo>;
        13 => fn sockets() -> Vec<SocketInfo>;
        /// Sends [`NetEvent`]s (ordinal [`NET_EVENT`]) on `events` until it
        /// is closed.
        14 => fn watch(events: Channel) -> Result<(), NetError>;
        15 => fn dns_cache() -> Vec<DnsCacheEntry>;

        /// Sets how `interface` obtains its IPv4 configuration.
        20 => fn configure(interface: String, config: IpConfig) -> Result<(), NetError>;
        /// Restarts DHCP on `interface` (asks for a new lease).
        21 => fn renew(interface: String) -> Result<(), NetError>;
        /// Overrides the DNS servers (empty: use the ones DHCP provides).
        22 => fn set_dns_servers(servers: Vec<IpAddr>) -> Result<(), NetError>;
        23 => fn flush_dns_cache() -> ();
    }
}

/// Event ordinal of the [`ResolveResult`] on a resolve reply channel.
pub const RESOLVE_RESULT: u32 = 1;

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum NetdevError {
        /// The device description is invalid (bad MAC, MTU, ...).
        Invalid = 1,
        /// The frame link could not be attached.
        BadLink = 2,
        /// Too many interfaces.
        LimitReached = 3,
    }
}

message! {
    /// A network device offered to the stack.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DeviceInfo {
        pub kind: InterfaceKind,
        pub mac: [u8; 6],
        /// Largest IP packet (1500 for Ethernet).
        pub mtu: u32,
        /// Driver or service name, e.g. `virtio-net`.
        pub driver: String,
        /// Where the device is, e.g. `pci 00:03.0`.
        pub location: String,
    }
}

protocol! {
    /// Network devices attaching to the stack. One device per channel;
    /// closing the channel removes the interface.
    pub mod netdev = "netdev" {
        /// Attaches a device whose frames flow through `link` (slots of kind
        /// [`crate::netring::kind::ETHERNET`]). Returns the interface name.
        1 => fn attach(info: DeviceInfo, link: LinkEndpoints, link_up: bool) -> Result<String, NetdevError>;
        /// The link went up or down (cable, Wi-Fi association).
        2 => fn set_link(up: bool) -> ();
    }
}

/// Why a device could not attach to the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    /// The network service is not running (or went away).
    Unavailable,
    NoMemory,
    /// The stack refused the device.
    Rejected(NetdevError),
}

impl core::fmt::Display for AttachError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AttachError::Unavailable => f.write_str("the network service is not available"),
            AttachError::NoMemory => f.write_str("out of memory"),
            AttachError::Rejected(e) => write!(f, "the network service refused the device ({e:?})"),
        }
    }
}

/// A network device's connection to the stack, for drivers and the Wi-Fi
/// service: the `netdev` channel plus the frame link.
pub struct DeviceAttachment {
    client: netdev::Client,
    /// The frame link (slots of kind [`crate::netring::kind::ETHERNET`]).
    pub link: crate::netring::Link,
    /// The interface name the stack assigned (`eth0`, `wlan0`, ...).
    pub name: String,
}

impl DeviceAttachment {
    /// Connects to the stack and attaches a device with a link of `slots`
    /// slots of `slot_size` bytes per direction. Blocks until the network
    /// service answers (it may not have started yet).
    pub fn attach(
        info: DeviceInfo,
        slots: u32,
        slot_size: u32,
        link_up: bool,
    ) -> Result<DeviceAttachment, AttachError> {
        Self::attach_timeout(info, slots, slot_size, link_up, 0)
    }

    /// Like [`DeviceAttachment::attach`], but each call to the stack gives
    /// up after `timeout_ns` nanoseconds (0: wait forever), for services
    /// that must stay responsive while the network service restarts.
    pub fn attach_timeout(
        info: DeviceInfo,
        slots: u32,
        slot_size: u32,
        link_up: bool,
        timeout_ns: u64,
    ) -> Result<DeviceAttachment, AttachError> {
        let channel = crate::connect(netdev::NAME).map_err(|_| AttachError::Unavailable)?;
        let client = netdev::Client::new(channel);
        client.set_timeout(timeout_ns);
        let (link, ends) = crate::netring::Link::create(slots, slot_size).map_err(|_| AttachError::NoMemory)?;
        match client.attach(info, ends, link_up) {
            Ok(Ok(name)) => Ok(DeviceAttachment { client, link, name }),
            Ok(Err(e)) => Err(AttachError::Rejected(e)),
            Err(_) => Err(AttachError::Unavailable),
        }
    }

    /// Reports a link change. Returns `false` if the stack is gone.
    pub fn set_link(&self, up: bool) -> bool {
        self.client.set_link(up).is_ok()
    }

    /// The `netdev` channel: `PEER_CLOSED` on it means the network service
    /// went away, and the device should attach again.
    pub fn channel(&self) -> &Channel {
        self.client.channel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use vipc::{Decode, Decoder, Encode, Encoder};

    fn roundtrip<T: Encode + Decode>(v: T) -> T {
        let mut e = Encoder::new();
        v.encode(&mut e);
        let mut d = Decoder::new(&e.bytes, Vec::new());
        let out = T::decode(&mut d).unwrap();
        d.finish().unwrap();
        out
    }

    #[test]
    fn socket_messages_round_trip() {
        let dest: SocketAddr = "10.0.2.3:53".parse().unwrap();
        let r = SocketRequest::SendTo { dest, data: Bytes(vec![1, 2, 3]) };
        assert_eq!(roundtrip(r.clone()), r);
        let e = SocketEvent::EchoReply { from: "1.1.1.1".parse().unwrap(), seq: 7, ttl: 57, size: 56, rtt_us: 12_345 };
        let mut enc = Encoder::new();
        e.encode(&mut enc);
        let mut d = Decoder::new(&enc.bytes, Vec::new());
        assert!(matches!(
            SocketEvent::decode(&mut d).unwrap(),
            SocketEvent::EchoReply { seq: 7, ttl: 57, size: 56, rtt_us: 12_345, .. }
        ));
        let f = ResolveResult::Found {
            addresses: vec!["2001:db8::1".parse().unwrap(), "192.0.2.1".parse().unwrap()],
            ttl_s: 300,
            canonical: "example.com".into(),
        };
        assert_eq!(roundtrip(f.clone()), f);
    }

    #[test]
    fn errors_have_descriptions() {
        assert_eq!(NetError::ConnectionRefused.to_string(), "connection refused");
        assert_eq!(roundtrip(NetError::TimeExceeded), NetError::TimeExceeded);
    }

    #[test]
    fn interface_info_round_trips() {
        let info = InterfaceInfo {
            name: "wlan0".into(),
            kind: InterfaceKind::Wireless,
            mac: [2, 0, 0, 0x57, 0x4c, 1],
            mtu: 1500,
            link_up: true,
            driver: "wlan".into(),
            config: IpConfig::Dhcp {},
            addresses: vec![AddressInfo {
                address: "10.0.2.15".parse().unwrap(),
                prefix_len: 24,
                origin: AddressOrigin::Dhcp,
                valid_s: 86400,
            }],
            gateways: vec!["10.0.2.2".parse().unwrap()],
            dns: vec!["10.0.2.3".parse().unwrap()],
            dhcp: DhcpInfo {
                state: DhcpState::Bound,
                server: Some("10.0.2.2".parse().unwrap()),
                lease_s: 86400,
                age_s: 5,
            },
            metric: 600,
            stats: InterfaceStats::default(),
        };
        assert_eq!(roundtrip(info.clone()), info);
    }
}
