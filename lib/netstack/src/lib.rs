//! `vnetstack` — the network stack behind `netd`.
//!
//! The stack owns the interfaces (each a smoltcp [`Interface`] with its own
//! socket set and frame queues), configures them (DHCPv4, static addresses,
//! IPv6 link-local and stateless autoconfiguration), picks routes, resolves
//! names (a caching stub resolver) and implements the socket operations
//! that `netd` offers to applications.
//!
//! It has no platform code: time is passed in, frames are pushed and pulled
//! by the caller, and randomness comes from a seed. That keeps every piece
//! testable on the host, including whole conversations between two stacks
//! (see the tests).

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod device;
mod dns;
mod route;
mod sockets;
#[cfg(test)]
mod tests;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::Medium;
use smoltcp::socket::dhcpv4;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Cidr, Ipv6Cidr};
use ventropy::Generator;
use vproto::net::{
    AddressInfo, AddressOrigin, Connectivity, DeviceInfo, DhcpInfo, DhcpState, InterfaceInfo, InterfaceKind,
    InterfaceStats, IpConfig, NetError, NetStatus, NetdevError, RouteInfo,
};

pub use device::QueueDevice;
pub use dns::{MAX_NAME_LEN, ResolveId, validate_name};
pub use sockets::{PingEvent, SockId, TcpState, TcpView, UdpDatagram};

/// Index of an interface; [`LOOPBACK`] is always present.
pub type IfaceId = usize;
/// The loopback interface (`lo`, 127.0.0.1 and ::1).
pub const LOOPBACK: IfaceId = 0;

/// Route preference of each interface kind (lower wins).
const METRIC_LOOPBACK: u32 = 1;
const METRIC_ETHERNET: u32 = 100;
const METRIC_WIRELESS: u32 = 600;
/// How long an interface whose link went down keeps its addresses, so a
/// short Wi-Fi outage does not break TCP connections.
pub const LINK_GRACE_US: u64 = 30_000_000;
/// Most interfaces (besides loopback).
const MAX_INTERFACES: usize = 16;
/// The DHCP client retries discovery this often.
const DHCP_DISCOVER_TIMEOUT: Duration = Duration::from_secs(3);

/// Something the network service should tell its watchers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackEvent {
    InterfaceChanged(String),
    StatusChanged(NetStatus),
}

/// The IPv4 configuration currently applied to an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V4Config {
    pub address: Ipv4Cidr,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<IpAddr>,
    pub origin: AddressOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DhcpTrack {
    pub state: DhcpState,
    pub server: Option<Ipv4Addr>,
    pub lease_s: u32,
    pub bound_at_us: u64,
    /// When discovery started (to report "no server" after a while).
    pub started_us: u64,
}

/// One network interface.
pub(crate) struct Iface {
    pub name: String,
    pub kind: InterfaceKind,
    pub mac: [u8; 6],
    pub mtu: u32,
    pub driver: String,
    pub link_up: bool,
    pub metric: u32,
    pub config: IpConfig,
    pub dev: QueueDevice,
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    pub dhcp: Option<SocketHandle>,
    pub dhcp_track: DhcpTrack,
    pub v4: Option<V4Config>,
    pub down_since: Option<u64>,
    /// The last address DHCP gave this interface (survives a reset).
    pub last_dhcp_addr: Option<Ipv4Addr>,
    /// We restarted DHCP ourselves (link back up, renew) at this time: keep
    /// the address until the server confirms or replaces it, because
    /// smoltcp resets every connection whose address disappears.
    pub reacquiring_since: Option<u64>,
    /// Lease length of the last DHCP ACK seen on the wire.
    pub acked_lease_s: u32,
    pub stats: InterfaceStats,
    /// Snapshot used to detect changes worth an event.
    pub fingerprint: u64,
}

impl Iface {
    fn is_loopback(&self) -> bool {
        self.kind == InterfaceKind::Loopback
    }

    /// IPv6 addresses (link-local and autoconfigured) with their origin.
    fn v6_addresses(&self) -> Vec<(Ipv6Cidr, AddressOrigin)> {
        self.iface
            .ip_addrs()
            .iter()
            .filter_map(|c| match c {
                IpCidr::Ipv6(c) => {
                    let origin = if self.is_loopback() {
                        AddressOrigin::Loopback
                    } else if c.address().is_unicast_link_local() {
                        AddressOrigin::LinkLocal
                    } else {
                        AddressOrigin::Slaac
                    };
                    Some((*c, origin))
                }
                _ => None,
            })
            .collect()
    }

    /// The IPv6 default router (from router advertisements).
    fn v6_gateway(&self) -> Option<Ipv6Addr> {
        self.iface.routes().get_default_ipv6_route().and_then(|r| match r.via_router {
            smoltcp::wire::IpAddress::Ipv6(a) => Some(a),
            #[allow(unreachable_patterns)]
            _ => None,
        })
    }

    /// Whether the interface has a global (non-link-local) IPv6 address.
    fn has_global_v6(&self) -> bool {
        self.v6_addresses().iter().any(|(c, o)| *o == AddressOrigin::Slaac && !c.address().is_loopback())
    }

    fn compute_fingerprint(&self) -> u64 {
        // FNV-1a over the facts the user can see.
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |b: &[u8]| {
            for &x in b {
                h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
            }
        };
        eat(&[self.link_up as u8, self.dhcp_track.state as u8]);
        if let Some(v4) = &self.v4 {
            eat(&v4.address.address().octets());
            eat(&[v4.address.prefix_len()]);
            if let Some(g) = v4.gateway {
                eat(&g.octets());
            }
            for d in &v4.dns {
                eat(format!("{d}").as_bytes());
            }
        }
        for (c, _) in self.v6_addresses() {
            eat(&c.address().octets());
        }
        if let Some(g) = self.v6_gateway() {
            eat(&g.octets());
        }
        h
    }
}

/// The network stack.
pub struct Stack {
    pub(crate) ifaces: Vec<Option<Iface>>,
    pub(crate) now_us: u64,
    pub(crate) rng: Generator,
    pub(crate) socks: sockets::Table,
    pub(crate) dns: dns::Resolver,
    pub(crate) dns_override: Vec<IpAddr>,
    events: Vec<StackEvent>,
    last_status: Option<NetStatus>,
    changed_us: u64,
}

impl Stack {
    /// Creates a stack with only the loopback interface. `seed` (at least
    /// 32 bytes from a cryptographic source) keys every random choice:
    /// TCP sequence numbers, ports, DNS ids.
    pub fn new(seed: &[u8], now_us: u64) -> Stack {
        let mut rng = Generator::new();
        rng.mix(seed);
        rng.reseed();
        let mut stack = Stack {
            ifaces: Vec::new(),
            now_us,
            rng,
            socks: sockets::Table::new(),
            dns: dns::Resolver::new(),
            dns_override: Vec::new(),
            events: Vec::new(),
            last_status: None,
            changed_us: now_us,
        };
        stack.add_loopback();
        stack
    }

    pub(crate) fn instant(&self) -> Instant {
        Instant::from_micros(self.now_us as i64)
    }

    pub(crate) fn random_key(&mut self) -> [u8; 32] {
        let mut k = [0u8; 32];
        self.rng.fill(&mut k);
        k
    }

    pub(crate) fn random_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.rng.fill(&mut b);
        u32::from_le_bytes(b)
    }

    /// Mixes fresh entropy into the stack's generator (the service does
    /// this now and then with kernel randomness).
    pub fn add_entropy(&mut self, data: &[u8]) {
        self.rng.mix(data);
        self.rng.reseed();
    }

    fn add_loopback(&mut self) {
        let mut dev = QueueDevice::new(Medium::Ip, 65535, true);
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_key = Some(self.random_key());
        let mut iface = Interface::new(config, &mut dev, self.instant());
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(Ipv4Addr::LOCALHOST.into(), 8));
            let _ = a.push(IpCidr::new(Ipv6Addr::LOCALHOST.into(), 128));
        });
        let lo = Iface {
            name: "lo".into(),
            kind: InterfaceKind::Loopback,
            mac: [0; 6],
            mtu: 65535,
            driver: "loopback".into(),
            link_up: true,
            metric: METRIC_LOOPBACK,
            config: IpConfig::Static {
                address: Ipv4Addr::LOCALHOST.into(),
                prefix_len: 8,
                gateway: None,
                dns: Vec::new(),
            },
            dev,
            iface,
            sockets: SocketSet::new(Vec::new()),
            dhcp: None,
            dhcp_track: DhcpTrack { state: DhcpState::Off, server: None, lease_s: 0, bound_at_us: 0, started_us: 0 },
            v4: Some(V4Config {
                address: Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 8),
                gateway: None,
                dns: Vec::new(),
                origin: AddressOrigin::Loopback,
            }),
            down_since: None,
            last_dhcp_addr: None,
            reacquiring_since: None,
            acked_lease_s: 0,
            stats: InterfaceStats::default(),
            fingerprint: 0,
        };
        self.ifaces.push(Some(lo));
    }

    pub(crate) fn iface(&self, id: IfaceId) -> Option<&Iface> {
        self.ifaces.get(id).and_then(Option::as_ref)
    }

    pub(crate) fn iface_mut(&mut self, id: IfaceId) -> Option<&mut Iface> {
        self.ifaces.get_mut(id).and_then(Option::as_mut)
    }

    /// Ids of all present interfaces.
    pub(crate) fn iface_ids(&self) -> Vec<IfaceId> {
        self.ifaces.iter().enumerate().filter(|(_, i)| i.is_some()).map(|(n, _)| n).collect()
    }

    /// Looks up an interface by name.
    pub fn find_interface(&self, name: &str) -> Option<IfaceId> {
        self.ifaces.iter().position(|i| i.as_ref().is_some_and(|i| i.name == name))
    }

    /// The name of an interface.
    pub fn interface_name(&self, id: IfaceId) -> Option<&str> {
        self.iface(id).map(|i| i.name.as_str())
    }

    /// Adds an Ethernet-like interface (a wired card or a Wi-Fi
    /// connection). It starts with DHCP and IPv6 autoconfiguration.
    pub fn add_interface(&mut self, info: &DeviceInfo, link_up: bool) -> Result<IfaceId, NetdevError> {
        let unicast = info.mac[0] & 1 == 0 && info.mac != [0; 6];
        if !matches!(info.kind, InterfaceKind::Ethernet | InterfaceKind::Wireless)
            || !unicast
            || !(1280..=9000).contains(&info.mtu)
        {
            return Err(NetdevError::Invalid);
        }
        if self.iface_ids().len() > MAX_INTERFACES {
            return Err(NetdevError::LimitReached);
        }
        let prefix = if info.kind == InterfaceKind::Wireless { "wlan" } else { "eth" };
        let name = (0..).map(|n| format!("{prefix}{n}")).find(|n| self.find_interface(n).is_none()).unwrap_or_default();
        let mut dev = QueueDevice::new(Medium::Ethernet, info.mtu as usize + 14, false);
        let hw = HardwareAddress::Ethernet(EthernetAddress(info.mac));
        let mut config = Config::new(hw);
        config.random_key = Some(self.random_key());
        config.slaac = true;
        let mut iface = Interface::new(config, &mut dev, self.instant());
        let link_local = Ipv6Cidr::new(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 64);
        if let Some(ll) = Ipv6Cidr::from_link_prefix(&link_local, hw) {
            iface.update_ip_addrs(|a| {
                let _ = a.push(IpCidr::Ipv6(ll));
            });
        }
        let metric = if info.kind == InterfaceKind::Wireless { METRIC_WIRELESS } else { METRIC_ETHERNET };
        let mut new = Iface {
            name: name.clone(),
            kind: info.kind,
            mac: info.mac,
            mtu: info.mtu,
            driver: info.driver.clone(),
            link_up,
            metric,
            config: IpConfig::Dhcp {},
            dev,
            iface,
            sockets: SocketSet::new(Vec::new()),
            dhcp: None,
            dhcp_track: DhcpTrack {
                state: DhcpState::Off,
                server: None,
                lease_s: 0,
                bound_at_us: 0,
                started_us: self.now_us,
            },
            v4: None,
            down_since: if link_up { None } else { Some(self.now_us) },
            last_dhcp_addr: None,
            reacquiring_since: None,
            acked_lease_s: 0,
            stats: InterfaceStats::default(),
            fingerprint: 0,
        };
        start_dhcp(&mut new, self.now_us);
        let id = match self.ifaces.iter().position(Option::is_none) {
            Some(free) => {
                self.ifaces[free] = Some(new);
                free
            }
            None => {
                self.ifaces.push(Some(new));
                self.ifaces.len() - 1
            }
        };
        self.socks_on_iface_added(id);
        self.events.push(StackEvent::InterfaceChanged(name));
        Ok(id)
    }

    /// Removes an interface (its device went away). Connections through it
    /// fail with [`NetError::Aborted`].
    pub fn remove_interface(&mut self, id: IfaceId) {
        if id == LOOPBACK {
            return;
        }
        self.socks_on_iface_removed(id);
        if let Some(i) = self.ifaces.get_mut(id).and_then(Option::take) {
            self.events.push(StackEvent::InterfaceChanged(i.name));
        }
    }

    /// The link of an interface went up or down.
    pub fn set_link(&mut self, id: IfaceId, up: bool) {
        let now = self.now_us;
        let Some(i) = self.iface_mut(id) else { return };
        if i.link_up == up || i.is_loopback() {
            return;
        }
        i.link_up = up;
        if up {
            i.down_since = None;
            // We may have joined a different network: ask for an address
            // again. (A server that knows us hands back the same address,
            // which keeps connections alive.)
            if let Some(h) = i.dhcp {
                i.sockets.get_mut::<dhcpv4::Socket>(h).reset();
                i.dhcp_track.state = DhcpState::Discovering;
                i.dhcp_track.started_us = now;
                if i.v4.is_some() {
                    i.reacquiring_since = Some(now);
                }
            }
        } else {
            i.down_since = Some(now);
        }
    }

    /// A frame arrived on an interface.
    pub fn receive_frame(&mut self, id: IfaceId, frame: &[u8]) {
        if let Some(i) = self.iface_mut(id) {
            if i.dhcp.is_some()
                && let Some(lease) = snoop_dhcp_ack(frame)
            {
                i.acked_lease_s = lease;
            }
            if i.dev.push_rx(frame) {
                i.stats.rx_packets += 1;
                i.stats.rx_bytes += frame.len() as u64;
            } else {
                i.stats.rx_dropped += 1;
            }
        }
    }

    /// Hands every frame queued for transmission on `id` to `send`, which
    /// returns `false` if the device dropped it.
    pub fn transmit_frames(&mut self, id: IfaceId, mut send: impl FnMut(&[u8]) -> bool) {
        let Some(i) = self.iface_mut(id) else { return };
        while let Some(frame) = i.dev.tx.pop_front() {
            if send(&frame) {
                i.stats.tx_packets += 1;
                i.stats.tx_bytes += frame.len() as u64;
            } else {
                i.stats.tx_dropped += 1;
            }
        }
    }

    /// Frames waiting to be transmitted on `id`.
    pub fn pending_frames(&self, id: IfaceId) -> usize {
        self.iface(id).map(|i| i.dev.tx.len()).unwrap_or(0)
    }

    /// Runs the stack: processes received frames, timers, DHCP, DNS and
    /// socket housekeeping. Call it after feeding frames or socket
    /// operations, and again by [`Stack::next_deadline`].
    pub fn poll(&mut self, now_us: u64) {
        self.now_us = now_us.max(self.now_us);
        let now = self.instant();
        for id in self.iface_ids() {
            let i = self.iface_mut(id).unwrap();
            i.iface.poll(now, &mut i.dev, &mut i.sockets);
            self.handle_dhcp(id);
            self.check_link_grace(id);
        }
        self.socks_maintain();
        self.dns_maintain();
        // Send what housekeeping produced (DNS queries, accepted
        // connections' replies, ...).
        for id in self.iface_ids() {
            let i = self.iface_mut(id).unwrap();
            i.iface.poll(now, &mut i.dev, &mut i.sockets);
        }
        self.detect_changes();
    }

    /// When [`Stack::poll`] should run next at the latest (monotonic µs),
    /// or `None` if only new frames or requests need it.
    pub fn next_deadline(&mut self) -> Option<u64> {
        let now = self.instant();
        let mut best: Option<u64> = None;
        let mut take = |t: u64| best = Some(best.map_or(t, |b| b.min(t)));
        for id in self.iface_ids() {
            let i = self.iface_mut(id).unwrap();
            if let Some(at) = i.iface.poll_at(now, &i.sockets) {
                take(at.total_micros().max(0) as u64);
            }
            if let Some(since) = i.down_since
                && i.v4.is_some()
            {
                take(since + LINK_GRACE_US);
            }
        }
        if let Some(t) = self.socks_deadline() {
            take(t);
        }
        if let Some(t) = self.dns.deadline() {
            take(t);
        }
        best
    }

    /// Events since the last call.
    pub fn take_events(&mut self) -> Vec<StackEvent> {
        core::mem::take(&mut self.events)
    }

    fn handle_dhcp(&mut self, id: IfaceId) {
        let now = self.now_us;
        let i = self.iface_mut(id).unwrap();
        let Some(h) = i.dhcp else { return };
        enum Change {
            Configured(Ipv4Cidr, Option<Ipv4Addr>, Vec<IpAddr>, Ipv4Addr, u32),
            Deconfigured,
        }
        let change = match i.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
            Some(dhcpv4::Event::Configured(cfg)) => {
                let lease = i.acked_lease_s;
                Some(Change::Configured(
                    cfg.address,
                    cfg.router,
                    cfg.dns_servers.iter().map(|d| IpAddr::V4(*d)).collect(),
                    cfg.server.identifier,
                    lease,
                ))
            }
            Some(dhcpv4::Event::Deconfigured) => Some(Change::Deconfigured),
            None => None,
        };
        match change {
            Some(Change::Configured(address, gateway, dns, server, lease_s)) => {
                let v4 = V4Config { address, gateway, dns, origin: AddressOrigin::Dhcp };
                i.dhcp_track = DhcpTrack {
                    state: DhcpState::Bound,
                    server: Some(server),
                    lease_s,
                    bound_at_us: now,
                    started_us: i.dhcp_track.started_us,
                };
                i.reacquiring_since = None;
                // Connections made with an earlier lease survive a renewal
                // or reconnection only if the address stayed the same. Fail
                // them before smoltcp notices the address is gone (it would
                // reset them silently).
                let previous = i.last_dhcp_addr.replace(address.address());
                if let Some(prev) = previous
                    && prev != address.address()
                {
                    self.socks_address_lost(IpAddr::V4(prev));
                }
                if let Some(i) = self.iface_mut(id) {
                    apply_v4(i, Some(v4));
                }
            }
            Some(Change::Deconfigured) => {
                i.dhcp_track.state = DhcpState::Discovering;
                i.dhcp_track.started_us = now;
                if i.reacquiring_since.is_none() {
                    // The lease really ended: the address goes now.
                    let old = apply_v4(i, None);
                    i.last_dhcp_addr = None;
                    if let Some(old) = old {
                        self.socks_address_lost(IpAddr::V4(old));
                    }
                }
            }
            None => {
                if i.dhcp_track.state == DhcpState::Discovering
                    && now.saturating_sub(i.dhcp_track.started_us) > 20_000_000
                {
                    i.dhcp_track.state = DhcpState::NoServer;
                }
            }
        }
    }

    /// Drops the addresses of an interface whose link stayed down too long,
    /// or whose DHCP server stopped confirming the address.
    fn check_link_grace(&mut self, id: IfaceId) {
        let now = self.now_us;
        let i = self.iface_mut(id).unwrap();
        if i.reacquiring_since.is_some_and(|t| now.saturating_sub(t) >= LINK_GRACE_US) {
            i.reacquiring_since = None;
            i.last_dhcp_addr = None;
            if let Some(old) = apply_v4(i, None) {
                self.socks_address_lost(IpAddr::V4(old));
            }
            return;
        }
        let expired = i.down_since.is_some_and(|t| now.saturating_sub(t) >= LINK_GRACE_US);
        if !expired || i.v4.is_none() {
            return;
        }
        let static_cfg = matches!(i.config, IpConfig::Static { .. });
        if static_cfg {
            return;
        }
        i.last_dhcp_addr = None;
        i.reacquiring_since = None;
        if let Some(old) = apply_v4(i, None) {
            self.socks_address_lost(IpAddr::V4(old));
        }
        if let Some(i) = self.iface_mut(id)
            && let Some(h) = i.dhcp
        {
            i.sockets.get_mut::<dhcpv4::Socket>(h).reset();
            i.dhcp_track.state = DhcpState::Discovering;
            i.dhcp_track.started_us = now;
        }
    }

    // ---------------------------------------------------------------
    // Configuration

    /// Sets how an interface obtains its IPv4 configuration.
    pub fn configure(&mut self, id: IfaceId, config: IpConfig) -> Result<(), NetError> {
        let now = self.now_us;
        let i = self.iface_mut(id).ok_or(NetError::InvalidArgument)?;
        if i.is_loopback() {
            return Err(NetError::PermissionDenied);
        }
        let v4 = match &config {
            IpConfig::Static { address: IpAddr::V4(a), prefix_len, gateway, dns } => {
                if *prefix_len > 32 || a.is_unspecified() || a.is_multicast() || a.is_broadcast() || a.is_loopback() {
                    return Err(NetError::InvalidArgument);
                }
                let gateway = match gateway {
                    None => None,
                    Some(IpAddr::V4(g)) => Some(*g),
                    Some(_) => return Err(NetError::InvalidArgument),
                };
                Some(V4Config {
                    address: Ipv4Cidr::new(*a, *prefix_len),
                    gateway,
                    dns: dns.clone(),
                    origin: AddressOrigin::Static,
                })
            }
            IpConfig::Static { .. } => return Err(NetError::InvalidArgument),
            IpConfig::Dhcp {} | IpConfig::Disabled {} => None,
        };
        stop_dhcp(i);
        i.config = config.clone();
        let old = apply_v4(i, v4);
        if matches!(config, IpConfig::Dhcp {}) {
            start_dhcp(i, now);
        }
        if let Some(old) = old {
            self.socks_address_lost(IpAddr::V4(old));
        }
        Ok(())
    }

    /// Restarts DHCP on an interface.
    pub fn renew(&mut self, id: IfaceId) -> Result<(), NetError> {
        let now = self.now_us;
        let i = self.iface_mut(id).ok_or(NetError::InvalidArgument)?;
        let h = i.dhcp.ok_or(NetError::NotSupported)?;
        i.sockets.get_mut::<dhcpv4::Socket>(h).reset();
        i.dhcp_track.state = DhcpState::Discovering;
        i.dhcp_track.started_us = now;
        if i.v4.is_some() {
            i.reacquiring_since = Some(now);
        }
        Ok(())
    }

    /// Overrides the DNS servers (empty: use the configured ones).
    pub fn set_dns_servers(&mut self, servers: Vec<IpAddr>) -> Result<(), NetError> {
        if servers.len() > 8 || servers.iter().any(|s| s.is_unspecified() || s.is_multicast()) {
            return Err(NetError::InvalidArgument);
        }
        self.dns_override = servers;
        self.dns.flush();
        self.events.push(StackEvent::StatusChanged(self.status()));
        Ok(())
    }

    // ---------------------------------------------------------------
    // Status

    /// DNS servers in order of preference: the override, else those of
    /// the default interface, then of the others.
    pub fn dns_servers(&self) -> Vec<IpAddr> {
        if !self.dns_override.is_empty() {
            return self.dns_override.clone();
        }
        let mut ids = self.iface_ids();
        ids.sort_by_key(|&id| {
            let i = self.iface(id).unwrap();
            (!i.link_up, i.v4.as_ref().and_then(|v| v.gateway).is_none(), i.metric)
        });
        let mut out: Vec<IpAddr> = Vec::new();
        for id in ids {
            let i = self.iface(id).unwrap();
            if !i.link_up {
                continue;
            }
            for d in i.v4.iter().flat_map(|v| v.dns.iter()) {
                if !out.contains(d) {
                    out.push(*d);
                }
            }
        }
        out
    }

    pub fn status(&self) -> NetStatus {
        let default = self.default_interface();
        let (connectivity, name, gateway) = match default {
            Some((id, gw)) => (Connectivity::Routable, self.iface(id).unwrap().name.clone(), Some(gw)),
            None => {
                let local = self.ifaces.iter().flatten().any(|i| {
                    !i.is_loopback()
                        && i.link_up
                        && (i.v4.is_some()
                            || i.v6_addresses().iter().any(|(c, _)| !c.address().is_unicast_link_local()))
                });
                (if local { Connectivity::Local } else { Connectivity::Offline }, String::new(), None)
            }
        };
        NetStatus {
            connectivity,
            default_interface: name,
            default_gateway: gateway,
            dns_servers: self.dns_servers(),
            changed_s: (self.changed_us / 1_000_000) as u32,
        }
    }

    pub fn interfaces(&self) -> Vec<InterfaceInfo> {
        let now = self.now_us;
        self.ifaces
            .iter()
            .flatten()
            .map(|i| {
                let mut addresses = Vec::new();
                if let Some(v4) = &i.v4 {
                    let valid_s = if v4.origin == AddressOrigin::Dhcp && i.dhcp_track.lease_s > 0 {
                        let age = ((now - i.dhcp_track.bound_at_us) / 1_000_000) as u32;
                        i.dhcp_track.lease_s.saturating_sub(age).max(1)
                    } else {
                        0
                    };
                    addresses.push(AddressInfo {
                        address: IpAddr::V4(v4.address.address()),
                        prefix_len: v4.address.prefix_len(),
                        origin: v4.origin,
                        valid_s,
                    });
                }
                for (c, origin) in i.v6_addresses() {
                    addresses.push(AddressInfo {
                        address: IpAddr::V6(c.address()),
                        prefix_len: c.prefix_len(),
                        origin,
                        valid_s: 0,
                    });
                }
                let mut gateways = Vec::new();
                if let Some(g) = i.v4.as_ref().and_then(|v| v.gateway) {
                    gateways.push(IpAddr::V4(g));
                }
                if let Some(g) = i.v6_gateway() {
                    gateways.push(IpAddr::V6(g));
                }
                let mut stats = i.stats;
                stats.rx_dropped += i.dev.rx_dropped;
                stats.tx_dropped += i.dev.tx_dropped;
                InterfaceInfo {
                    name: i.name.clone(),
                    kind: i.kind,
                    mac: i.mac,
                    mtu: i.mtu,
                    link_up: i.link_up,
                    driver: i.driver.clone(),
                    config: i.config.clone(),
                    addresses,
                    gateways,
                    dns: i.v4.as_ref().map(|v| v.dns.clone()).unwrap_or_default(),
                    dhcp: DhcpInfo {
                        state: i.dhcp_track.state,
                        server: i.dhcp_track.server.map(IpAddr::V4),
                        lease_s: i.dhcp_track.lease_s,
                        age_s: if i.dhcp_track.state == DhcpState::Bound {
                            ((now - i.dhcp_track.bound_at_us) / 1_000_000) as u32
                        } else {
                            0
                        },
                    },
                    metric: i.metric,
                    stats,
                }
            })
            .collect()
    }

    pub fn routes(&self) -> Vec<RouteInfo> {
        let mut out = Vec::new();
        for i in self.ifaces.iter().flatten() {
            if let Some(v4) = &i.v4 {
                let net = v4.address.network();
                out.push(RouteInfo {
                    destination: IpAddr::V4(net.address()),
                    prefix_len: net.prefix_len(),
                    gateway: None,
                    interface: i.name.clone(),
                    metric: i.metric,
                });
                if let Some(g) = v4.gateway {
                    out.push(RouteInfo {
                        destination: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                        prefix_len: 0,
                        gateway: Some(IpAddr::V4(g)),
                        interface: i.name.clone(),
                        metric: i.metric,
                    });
                }
            }
            for (c, _) in i.v6_addresses() {
                let net = Ipv6Cidr::new(c.address(), c.prefix_len());
                let masked = mask_v6(net.address(), net.prefix_len());
                let route = RouteInfo {
                    destination: IpAddr::V6(masked),
                    prefix_len: net.prefix_len(),
                    gateway: None,
                    interface: i.name.clone(),
                    metric: i.metric,
                };
                if !out.contains(&route) {
                    out.push(route);
                }
            }
            if let Some(g) = i.v6_gateway() {
                out.push(RouteInfo {
                    destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    prefix_len: 0,
                    gateway: Some(IpAddr::V6(g)),
                    interface: i.name.clone(),
                    metric: i.metric,
                });
            }
        }
        out
    }

    fn detect_changes(&mut self) {
        let mut changed = Vec::new();
        for i in self.ifaces.iter_mut().flatten() {
            let f = i.compute_fingerprint();
            if f != i.fingerprint {
                i.fingerprint = f;
                changed.push(i.name.clone());
            }
        }
        for name in changed {
            self.events.push(StackEvent::InterfaceChanged(name));
        }
        let status = self.status();
        let mut cmp = status.clone();
        if let Some(last) = &self.last_status {
            cmp.changed_s = last.changed_s;
            if *last == cmp {
                return;
            }
        }
        self.changed_us = self.now_us;
        let status = NetStatus { changed_s: (self.now_us / 1_000_000) as u32, ..status };
        self.last_status = Some(status.clone());
        self.events.push(StackEvent::StatusChanged(status));
    }
}

/// Applies (or removes, with `None`) an interface's IPv4 configuration:
/// address and default route. Returns the previous address if it changed.
fn apply_v4(i: &mut Iface, v4: Option<V4Config>) -> Option<Ipv4Addr> {
    let old = i.v4.as_ref().map(|v| v.address.address());
    let new_addr = v4.as_ref().map(|v| v.address);
    i.iface.update_ip_addrs(|addrs| {
        addrs.retain(|c| !matches!(c, IpCidr::Ipv4(_)));
        if let Some(a) = new_addr {
            // Keep IPv4 first so it is the preferred source address.
            let _ = addrs.insert(0, IpCidr::Ipv4(a));
        }
    });
    i.iface.routes_mut().remove_default_ipv4_route();
    if let Some(g) = v4.as_ref().and_then(|v| v.gateway) {
        let _ = i.iface.routes_mut().add_default_ipv4_route(g);
    }
    i.v4 = v4;
    let new = i.v4.as_ref().map(|v| v.address.address());
    if old != new { old } else { None }
}

fn start_dhcp(i: &mut Iface, now: u64) {
    if i.dhcp.is_some() || !matches!(i.config, IpConfig::Dhcp {}) {
        return;
    }
    let mut socket = dhcpv4::Socket::new();
    let mut retry = dhcpv4::RetryConfig::default();
    retry.discover_timeout = DHCP_DISCOVER_TIMEOUT;
    retry.initial_request_timeout = Duration::from_secs(2);
    retry.min_renew_timeout = Duration::from_secs(30);
    retry.max_renew_timeout = Duration::from_secs(300);
    socket.set_retry_config(retry);
    i.dhcp = Some(i.sockets.add(socket));
    i.dhcp_track.state = DhcpState::Discovering;
    i.dhcp_track.started_us = now;
}

fn stop_dhcp(i: &mut Iface) {
    if let Some(h) = i.dhcp.take() {
        i.sockets.remove(h);
    }
    i.dhcp_track = DhcpTrack { state: DhcpState::Off, server: None, lease_s: 0, bound_at_us: 0, started_us: 0 };
}

/// Clears the host bits of an IPv6 address.
fn mask_v6(a: Ipv6Addr, prefix: u8) -> Ipv6Addr {
    let bits = u128::from_be_bytes(a.octets());
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix.min(128) as u32) };
    Ipv6Addr::from((bits & mask).to_be_bytes())
}

/// The lease length of a DHCP ACK (the DHCP client does not report it).
fn snoop_dhcp_ack(frame: &[u8]) -> Option<u32> {
    use smoltcp::wire::{
        DhcpMessageType, DhcpPacket, DhcpRepr, EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, UdpPacket,
    };
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Udp {
        return None;
    }
    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
    if udp.src_port() != 67 || udp.dst_port() != 68 {
        return None;
    }
    let packet = DhcpPacket::new_checked(udp.payload()).ok()?;
    let repr = DhcpRepr::parse(&packet).ok()?;
    if repr.message_type != DhcpMessageType::Ack {
        return None;
    }
    repr.lease_duration
}
