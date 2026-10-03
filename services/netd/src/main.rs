//! `netd` — the network service.
//!
//! * Network devices (Ethernet drivers, and the Wi-Fi service for each
//!   wireless connection) attach through the `netdev` protocol; their frames
//!   flow through shared rings.
//! * Applications use the `net` protocol: sockets (each its own channel),
//!   name resolution, status and configuration.
//! * The protocol work itself — interfaces, DHCP, IPv6 autoconfiguration,
//!   routing, DNS, TCP, UDP, ICMP — happens in `vnetstack` (smoltcp
//!   underneath).
//!
//! Everything runs on one thread around a single wait: on device rings,
//! device and client channels, socket channels and the stack's next timer.
//! Every peer is untrusted: malformed messages fail only the offending
//! socket or connection, and per-client limits bound what a client can
//! consume.

#![no_std]
#![no_main]

extern crate alloc;

mod socket;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{IpAddr, SocketAddr};

use vabi::signals;
use vnetstack::{IfaceId, ResolveId, Stack, StackEvent};
use vproto::net::{
    AddrFamily, DeviceInfo, DnsCacheEntry, InterfaceInfo, IpConfig, NET_EVENT, NetError, NetEvent, NetStatus,
    NetdevError, RESOLVE_RESULT, RouteInfo, SocketInfo, TcpOptions, net, netdev,
};
use vproto::netring::{Link, LinkEndpoints, SlotMeta, kind};
use vrt::object::Channel;
use vrt::println;

use socket::{Conn, Kind, SEND_WINDOW};

vrt::entry!(main);

/// Sockets one client connection may own.
const MAX_SOCKETS_PER_CLIENT: usize = 256;
/// Status watchers.
const MAX_WATCHERS: usize = 32;
/// Frames taken from one device per loop iteration.
const RX_BUDGET: usize = 256;
/// Requests handled per client per loop iteration.
const CLIENT_BUDGET: usize = 32;
/// Reseed the stack's generator with kernel entropy this often.
const RESEED_NS: u64 = 300_000_000_000;

/// A device attached through `netdev`.
struct Device {
    channel: Channel,
    link: Option<Link>,
    iface: Option<IfaceId>,
    name: String,
}

/// A `net` client connection.
struct Client {
    channel: Channel,
    /// Identifies the client in socket listings.
    id: u64,
}

struct Netd {
    stack: Stack,
    net_listener: Channel,
    dev_listener: Channel,
    clients: BTreeMap<u64, Client>,
    devices: BTreeMap<u64, Device>,
    sockets: BTreeMap<u64, Conn>,
    resolves: BTreeMap<ResolveId, Channel>,
    watchers: Vec<Channel>,
    next_key: u64,
    frame: Vec<u8>,
    last_reseed: u64,
}

fn now_us() -> u64 {
    vrt::time::now_ns() / 1000
}

fn mac_string(m: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

impl Netd {
    fn key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    fn client_sockets(&self, owner: u64) -> usize {
        self.sockets.values().filter(|c| c.owner == owner).count()
    }

    fn add_socket(&mut self, conn: Conn) {
        let k = self.key();
        self.sockets.insert(k, conn);
    }

    // ---------------------------------------------------------------
    // Devices

    fn accept_devices(&mut self) {
        while let Some(ch) = vproto::accept(&self.dev_listener) {
            let k = self.key();
            self.devices.insert(k, Device { channel: ch, link: None, iface: None, name: String::new() });
        }
    }

    fn handle_device(&mut self, key: u64) {
        struct Session<'a> {
            netd: &'a mut Netd,
            key: u64,
        }
        impl netdev::Server for Session<'_> {
            fn attach(&mut self, info: DeviceInfo, link: LinkEndpoints, link_up: bool) -> Result<String, NetdevError> {
                if self.netd.devices.get(&self.key).is_some_and(|d| d.iface.is_some()) {
                    return Err(NetdevError::Invalid);
                }
                let link = Link::attach(link).map_err(|_| NetdevError::BadLink)?;
                let id = self.netd.stack.add_interface(&info, link_up)?;
                let name = String::from(self.netd.stack.interface_name(id).unwrap_or("?"));
                println!(
                    "{}: {} {} ({}, {}), link {}",
                    name,
                    match info.kind {
                        vproto::net::InterfaceKind::Wireless => "wireless",
                        _ => "ethernet",
                    },
                    mac_string(&info.mac),
                    info.driver,
                    info.location,
                    if link_up { "up" } else { "down" }
                );
                let dev = self.netd.devices.get_mut(&self.key).unwrap();
                dev.link = Some(link);
                dev.iface = Some(id);
                dev.name = name.clone();
                Ok(name)
            }

            fn set_link(&mut self, up: bool) {
                let Some(dev) = self.netd.devices.get(&self.key) else { return };
                if let Some(id) = dev.iface {
                    println!("{}: link {}", dev.name, if up { "up" } else { "down" });
                    self.netd.stack.set_link(id, up);
                }
            }
        }
        for _ in 0..CLIENT_BUDGET {
            let Some(dev) = self.devices.get(&key) else { return };
            let msg = match dev.channel.read() {
                Ok(m) => m,
                Err(_) => return,
            };
            let mut s = Session { netd: self, key };
            match netdev::dispatch(&mut s, msg) {
                Ok(reply) => {
                    if let Some(dev) = self.devices.get(&key) {
                        let _ = reply.send(&dev.channel);
                    }
                }
                Err(e) => println!("bad netdev request: {}", e),
            }
        }
    }

    fn remove_device(&mut self, key: u64) {
        if let Some(dev) = self.devices.remove(&key)
            && let Some(id) = dev.iface
        {
            println!("{}: device gone", dev.name);
            self.stack.remove_interface(id);
        }
    }

    /// Feeds received frames into the stack.
    fn receive_frames(&mut self) {
        for dev in self.devices.values() {
            let (Some(link), Some(id)) = (&dev.link, dev.iface) else { continue };
            for _ in 0..RX_BUDGET {
                let Some((meta, len)) = link.recv(&mut self.frame) else { break };
                if meta.kind == kind::ETHERNET {
                    self.stack.receive_frame(id, &self.frame[..len]);
                }
            }
        }
    }

    /// Hands the stack's outgoing frames to the devices.
    fn transmit_frames(&mut self) {
        for dev in self.devices.values() {
            let (Some(link), Some(id)) = (&dev.link, dev.iface) else { continue };
            self.stack.transmit_frames(id, |f| link.send(SlotMeta::ethernet(), f));
        }
    }

    // ---------------------------------------------------------------
    // Clients

    fn accept_clients(&mut self) {
        while let Some(ch) = vproto::accept(&self.net_listener) {
            let k = self.key();
            self.clients.insert(k, Client { channel: ch, id: k });
        }
    }

    fn handle_client(&mut self, key: u64) {
        struct Session<'a> {
            netd: &'a mut Netd,
            owner: u64,
        }
        impl Session<'_> {
            fn check_quota(&self) -> Result<(), NetError> {
                if self.netd.client_sockets(self.owner) >= MAX_SOCKETS_PER_CLIENT {
                    Err(NetError::LimitReached)
                } else {
                    Ok(())
                }
            }
        }
        impl net::Server for Session<'_> {
            fn tcp_connect(
                &mut self,
                remote: SocketAddr,
                options: TcpOptions,
                socket: Channel,
            ) -> Result<(), NetError> {
                self.check_quota()?;
                let id = self.netd.stack.tcp_connect(remote, &options, self.owner)?;
                let kind = Kind::Tcp {
                    announced: false,
                    eof_sent: false,
                    pending: VecDeque::new(),
                    shutdown_requested: false,
                };
                self.netd.add_socket(Conn::new(socket, id, kind, self.owner));
                Ok(())
            }

            fn tcp_listen(
                &mut self,
                local: SocketAddr,
                backlog: u32,
                options: TcpOptions,
                listener: Channel,
            ) -> Result<SocketAddr, NetError> {
                self.check_quota()?;
                let (id, bound) = self.netd.stack.tcp_listen(local, backlog, &options, self.owner)?;
                self.netd.add_socket(Conn::new(listener, id, Kind::Listener, self.owner));
                Ok(bound)
            }

            fn udp_bind(&mut self, local: SocketAddr, socket: Channel) -> Result<SocketAddr, NetError> {
                self.check_quota()?;
                let (id, bound) = self.netd.stack.udp_bind(local, self.owner)?;
                let mut conn = Conn::new(socket, id, Kind::Udp { pending: VecDeque::new() }, self.owner);
                conn.grant_initial(SEND_WINDOW);
                self.netd.add_socket(conn);
                Ok(bound)
            }

            fn ping_open(&mut self, socket: Channel) -> Result<(), NetError> {
                self.check_quota()?;
                let id = self.netd.stack.ping_open(self.owner)?;
                self.netd.add_socket(Conn::new(socket, id, Kind::Ping, self.owner));
                Ok(())
            }

            fn resolve(&mut self, name: String, family: AddrFamily, reply: Channel) -> Result<(), NetError> {
                if self.netd.resolves.len() >= 512 {
                    return Err(NetError::LimitReached);
                }
                let id = self.netd.stack.resolve(&name, family)?;
                self.netd.resolves.insert(id, reply);
                Ok(())
            }

            fn status(&mut self) -> NetStatus {
                self.netd.stack.status()
            }

            fn interfaces(&mut self) -> Vec<InterfaceInfo> {
                self.netd.stack.interfaces()
            }

            fn routes(&mut self) -> Vec<RouteInfo> {
                self.netd.stack.routes()
            }

            fn sockets(&mut self) -> Vec<SocketInfo> {
                self.netd.stack.socket_infos()
            }

            fn watch(&mut self, events: Channel) -> Result<(), NetError> {
                if self.netd.watchers.len() >= MAX_WATCHERS {
                    return Err(NetError::LimitReached);
                }
                let status = self.netd.stack.status();
                let _ = vipc::send_event(&events, NET_EVENT, NetEvent::StatusChanged { status });
                self.netd.watchers.push(events);
                Ok(())
            }

            fn dns_cache(&mut self) -> Vec<DnsCacheEntry> {
                self.netd.stack.dns_cache()
            }

            fn configure(&mut self, interface: String, config: IpConfig) -> Result<(), NetError> {
                let id = self.netd.stack.find_interface(&interface).ok_or(NetError::InvalidArgument)?;
                println!("{}: configuration set to {:?}", interface, config);
                self.netd.stack.configure(id, config)
            }

            fn renew(&mut self, interface: String) -> Result<(), NetError> {
                let id = self.netd.stack.find_interface(&interface).ok_or(NetError::InvalidArgument)?;
                self.netd.stack.renew(id)
            }

            fn set_dns_servers(&mut self, servers: Vec<IpAddr>) -> Result<(), NetError> {
                self.netd.stack.set_dns_servers(servers)
            }

            fn flush_dns_cache(&mut self) {
                self.netd.stack.flush_dns_cache();
            }
        }
        for _ in 0..CLIENT_BUDGET {
            let Some(c) = self.clients.get(&key) else { return };
            let owner = c.id;
            let msg = match c.channel.read() {
                Ok(m) => m,
                Err(_) => return,
            };
            let mut s = Session { netd: self, owner };
            match net::dispatch(&mut s, msg) {
                Ok(reply) => {
                    if let Some(c) = self.clients.get(&key) {
                        let _ = reply.send(&c.channel);
                    }
                }
                Err(_) => {
                    // A malformed request: drop the client.
                    self.clients.remove(&key);
                    return;
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Sockets, lookups, events

    /// Moves data between socket channels and the stack. Returns `true` if
    /// the stack got new data to send.
    fn pump_sockets(&mut self) -> bool {
        let mut fed = false;
        let mut accepted = Vec::new();
        for conn in self.sockets.values_mut() {
            fed |= conn.pump(&mut self.stack, &mut accepted);
        }
        for conn in accepted {
            self.add_socket(conn);
        }
        fed
    }

    fn deliver_lookups(&mut self) {
        for id in self.stack.resolve_ready() {
            let Some(result) = self.stack.resolve_result(id) else { continue };
            if let Some(reply) = self.resolves.remove(&id) {
                let _ = vipc::send_event(&reply, RESOLVE_RESULT, result);
            }
        }
        // Lookups whose client went away.
        let gone: Vec<ResolveId> = self
            .resolves
            .iter()
            .filter(|(_, ch)| ch.wait(signals::PEER_CLOSED, 0).is_ok_and(|s| s & signals::PEER_CLOSED != 0))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.resolves.remove(&id);
            self.stack.resolve_cancel(id);
        }
    }

    fn publish_events(&mut self) {
        for ev in self.stack.take_events() {
            let ev = match ev {
                StackEvent::StatusChanged(status) => {
                    println!(
                        "status: {:?}{}{}",
                        status.connectivity,
                        if status.default_interface.is_empty() {
                            String::new()
                        } else {
                            alloc::format!(" via {}", status.default_interface)
                        },
                        match status.default_gateway {
                            Some(g) => alloc::format!(" (gateway {g})"),
                            None => String::new(),
                        }
                    );
                    NetEvent::StatusChanged { status }
                }
                StackEvent::InterfaceChanged(name) => {
                    if let Some(info) = self.stack.interfaces().into_iter().find(|i| i.name == name) {
                        let addrs: Vec<String> =
                            info.addresses.iter().map(|a| alloc::format!("{}/{}", a.address, a.prefix_len)).collect();
                        let dns: Vec<String> = info.dns.iter().map(|d| alloc::format!("{d}")).collect();
                        println!(
                            "{}: link {}, addresses [{}], DNS [{}], DHCP {:?}",
                            name,
                            if info.link_up { "up" } else { "down" },
                            addrs.join(", "),
                            dns.join(", "),
                            info.dhcp.state
                        );
                    }
                    NetEvent::InterfaceChanged { name }
                }
            };
            self.watchers
                .retain(|w| !matches!(vipc::send_event(w, NET_EVENT, ev.clone()), Err(vipc::IpcError::PeerClosed)));
        }
    }

    // ---------------------------------------------------------------
    // The loop

    fn run(&mut self) -> ! {
        const NET_LISTENER: u64 = 0;
        const DEV_LISTENER: u64 = 1;
        // Keys of the three tables are disjoint (`self.key()`); these bits
        // tell wait results apart.
        const DEVICE_LINK: u64 = 1 << 62;
        loop {
            let now = now_us();
            if vrt::time::now_ns().saturating_sub(self.last_reseed) >= RESEED_NS {
                let mut seed = [0u8; 32];
                vrt::object::random_bytes(&mut seed);
                self.stack.add_entropy(&seed);
                self.last_reseed = vrt::time::now_ns();
            }
            self.receive_frames();
            self.stack.poll(now);
            if self.pump_sockets() {
                self.stack.poll(now_us());
            }
            self.transmit_frames();
            self.deliver_lookups();
            self.publish_events();

            // Wait for the next thing to do.
            let mut ws = vipc::WaitSet::new();
            ws.add(self.net_listener.raw(), signals::READABLE, NET_LISTENER);
            ws.add(self.dev_listener.raw(), signals::READABLE, DEV_LISTENER);
            let mut busy = false;
            for (&k, d) in &self.devices {
                ws.add(d.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
                if let Some(link) = &d.link {
                    if link.prepare_wait(false) {
                        ws.add(link.wake_event().raw(), signals::SIGNALED, k | DEVICE_LINK);
                    } else {
                        busy = true;
                    }
                }
            }
            for (&k, c) in &self.clients {
                ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
            }
            for (&k, s) in &self.sockets {
                let mut want = signals::READABLE | signals::PEER_CLOSED;
                if s.blocked {
                    want |= signals::WRITABLE;
                }
                ws.add(s.channel.raw(), want, k);
            }
            let deadline = if busy {
                0
            } else {
                match self.stack.next_deadline() {
                    Some(t) => t.saturating_mul(1000),
                    None => vabi::DEADLINE_INFINITE,
                }
            };
            let ready = ws.wait(deadline).unwrap_or_default();
            for d in self.devices.values() {
                if let Some(link) = &d.link {
                    link.finish_wait();
                }
            }
            for (key, observed) in ready {
                match key {
                    NET_LISTENER => self.accept_clients(),
                    DEV_LISTENER => self.accept_devices(),
                    k if k & DEVICE_LINK != 0 => {}
                    k if self.devices.contains_key(&k) => {
                        if observed & signals::READABLE != 0 {
                            self.handle_device(k);
                        }
                        if observed & signals::PEER_CLOSED != 0 && observed & signals::READABLE == 0 {
                            self.remove_device(k);
                        }
                    }
                    k if self.clients.contains_key(&k) => {
                        if observed & signals::READABLE != 0 {
                            self.handle_client(k);
                        } else if observed & signals::PEER_CLOSED != 0 {
                            self.clients.remove(&k);
                        }
                    }
                    k => {
                        let Some(conn) = self.sockets.get_mut(&k) else { continue };
                        let open = if observed & signals::READABLE != 0 {
                            conn.handle_requests(&mut self.stack)
                        } else {
                            true
                        };
                        if !open || (observed & signals::PEER_CLOSED != 0 && observed & signals::READABLE == 0) {
                            let conn = self.sockets.remove(&k).unwrap();
                            self.stack.close(conn.id);
                        }
                    }
                }
            }
            // Finished sockets whose client is gone.
            self.sockets.retain(|_, c| {
                !(c.done && c.channel.wait(signals::PEER_CLOSED, 0).is_ok_and(|s| s & signals::PEER_CLOSED != 0))
            });
        }
    }
}

fn main() -> i32 {
    let mut seed = [0u8; 64];
    vrt::object::random_bytes(&mut seed);
    let stack = Stack::new(&seed, now_us());
    let (Ok(net_listener), Ok(dev_listener)) = (vproto::register(net::NAME), vproto::register(netdev::NAME)) else {
        println!("cannot register the network services");
        return 1;
    };
    println!("network service ready");
    let mut netd = Netd {
        stack,
        net_listener,
        dev_listener,
        clients: BTreeMap::new(),
        devices: BTreeMap::new(),
        sockets: BTreeMap::new(),
        resolves: BTreeMap::new(),
        watchers: Vec::new(),
        next_key: 16,
        frame: alloc::vec![0u8; 16384],
        last_reseed: vrt::time::now_ns(),
    };
    netd.run()
}
