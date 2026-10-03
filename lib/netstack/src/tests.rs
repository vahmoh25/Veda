//! Integration tests: whole conversations between two stacks over a
//! simulated cable, with a DHCP server in the harness and a DNS server on
//! the far stack. The cable can drop frames at random or go dark.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet,
    Ipv4Repr, UdpPacket, UdpRepr,
};
use vproto::net::{AddrFamily, Connectivity, DeviceInfo, InterfaceKind, IpConfig, NetError, ResolveResult, TcpOptions};

use crate::dns::tests::{encode_name, response};
use crate::{IfaceId, PingEvent, SockId, Stack, TcpState};

const CLIENT_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x15];
const SERVER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];
const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const CLIENT_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

/// A deterministic random source for frame loss.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// What the harness's DHCP server hands out.
struct DhcpServer {
    enabled: bool,
    address: Ipv4Addr,
    dns: Vec<Ipv4Addr>,
    lease_s: u32,
    requests_seen: u32,
}

struct Net {
    a: Stack,
    b: Stack,
    a_eth: IfaceId,
    b_eth: IfaceId,
    now: u64,
    loss: u64,
    dark: bool,
    rng: Rng,
    dhcp: DhcpServer,
    /// DNS server socket on `b` and how it answers.
    dns_sock: SockId,
    dns_mode: DnsMode,
    dns_queries: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DnsMode {
    Answer,
    Silent,
    ServFail,
}

fn device(mac: [u8; 6]) -> DeviceInfo {
    DeviceInfo { kind: InterfaceKind::Ethernet, mac, mtu: 1500, driver: "test".into(), location: "cable".into() }
}

impl Net {
    fn new() -> Net {
        let mut a = Stack::new(b"client seed for tests, 32+ bytes long", 1_000_000);
        let mut b = Stack::new(b"server seed for tests, 32+ bytes long", 1_000_000);
        let a_eth = a.add_interface(&device(CLIENT_MAC), true).unwrap();
        let b_eth = b.add_interface(&device(SERVER_MAC), true).unwrap();
        b.configure(b_eth, IpConfig::Static { address: SERVER_IP.into(), prefix_len: 24, gateway: None, dns: vec![] })
            .unwrap();
        let (dns_sock, _) = b.udp_bind("0.0.0.0:53".parse().unwrap(), 0).unwrap();
        Net {
            a,
            b,
            a_eth,
            b_eth,
            now: 1_000_000,
            loss: 0,
            dark: false,
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            dhcp: DhcpServer {
                enabled: true,
                address: CLIENT_IP,
                dns: vec![SERVER_IP],
                lease_s: 3600,
                requests_seen: 0,
            },
            dns_sock,
            dns_mode: DnsMode::Answer,
            dns_queries: 0,
        }
    }

    /// Advances time by `ms` milliseconds in 1 ms steps.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step();
        }
    }

    fn step(&mut self) {
        self.now += 1000;
        self.a.poll(self.now);
        self.b.poll(self.now);
        let mut to_b = Vec::new();
        let mut to_a = Vec::new();
        self.a.transmit_frames(self.a_eth, |f| {
            to_b.push(f.to_vec());
            true
        });
        self.b.transmit_frames(self.b_eth, |f| {
            to_a.push(f.to_vec());
            true
        });
        for f in to_b {
            if self.dark || self.rng.chance(self.loss) {
                continue;
            }
            if let Some(reply) = self.dhcp_answer(&f) {
                if !reply.is_empty() {
                    to_a.push(reply);
                }
                continue;
            }
            self.b.receive_frame(self.b_eth, &f);
        }
        for f in to_a {
            if self.dark || self.rng.chance(self.loss) {
                continue;
            }
            self.a.receive_frame(self.a_eth, &f);
        }
        self.serve_dns();
    }

    fn run_until(&mut self, max_ms: u64, mut done: impl FnMut(&mut Net) -> bool) -> bool {
        for _ in 0..max_ms {
            self.step();
            if done(self) {
                return true;
            }
        }
        false
    }

    /// The harness DHCP server: answers DISCOVER with OFFER and REQUEST
    /// with ACK.
    fn dhcp_answer(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let eth = EthernetFrame::new_checked(frame).ok()?;
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return None;
        }
        let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
        if ip.next_header() != IpProtocol::Udp {
            return None;
        }
        let udp = UdpPacket::new_checked(ip.payload()).ok()?;
        if udp.dst_port() != 67 {
            return None;
        }
        let dhcp = DhcpPacket::new_checked(udp.payload()).ok()?;
        let req = DhcpRepr::parse(&dhcp).ok()?;
        if !self.dhcp.enabled {
            // Swallow the request: no server on the network.
            return Some(Vec::new());
        }
        let kind = match req.message_type {
            DhcpMessageType::Discover => DhcpMessageType::Offer,
            DhcpMessageType::Request => {
                self.dhcp.requests_seen += 1;
                DhcpMessageType::Ack
            }
            _ => return None,
        };
        let mut dns = heapless::Vec::new();
        for d in &self.dhcp.dns {
            let _ = dns.push(*d);
        }
        let repr = DhcpRepr {
            message_type: kind,
            transaction_id: req.transaction_id,
            secs: 0,
            client_hardware_address: req.client_hardware_address,
            client_ip: Ipv4Addr::UNSPECIFIED,
            your_ip: self.dhcp.address,
            server_ip: SERVER_IP,
            router: Some(SERVER_IP),
            subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            broadcast: false,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(SERVER_IP),
            parameter_request_list: None,
            dns_servers: Some(dns),
            max_size: None,
            lease_duration: Some(self.dhcp.lease_s),
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let dhcp_len = repr.buffer_len();
        let mut out = vec![0u8; 14 + 20 + 8 + dhcp_len];
        let mut eth = EthernetFrame::new_unchecked(&mut out[..]);
        eth.set_dst_addr(EthernetAddress::BROADCAST);
        eth.set_src_addr(EthernetAddress(SERVER_MAC));
        eth.set_ethertype(EthernetProtocol::Ipv4);
        let caps = ChecksumCapabilities::default();
        let ip_repr = Ipv4Repr {
            src_addr: SERVER_IP,
            dst_addr: Ipv4Addr::BROADCAST,
            next_header: IpProtocol::Udp,
            payload_len: 8 + dhcp_len,
            hop_limit: 64,
        };
        ip_repr.emit(&mut Ipv4Packet::new_unchecked(&mut out[14..]), &caps);
        let udp_repr = UdpRepr { src_port: 67, dst_port: 68 };
        udp_repr.emit(
            &mut UdpPacket::new_unchecked(&mut out[34..]),
            &SERVER_IP.into(),
            &Ipv4Addr::BROADCAST.into(),
            dhcp_len,
            |buf| {
                let _ = repr.emit(&mut DhcpPacket::new_unchecked(buf));
            },
            &caps,
        );
        Some(out)
    }

    /// The DNS server on `b`: `*.example` names resolve, `nx.example`
    /// does not exist.
    fn serve_dns(&mut self) {
        let mut buf = vec![0u8; 2048];
        while let Some(d) = self.b.udp_recv(self.dns_sock, &mut buf) {
            self.dns_queries += 1;
            let q = &buf[..d.len];
            let reply = match self.dns_mode {
                DnsMode::Silent => continue,
                DnsMode::ServFail => response(q, 2, &[], &[]),
                DnsMode::Answer => {
                    let name = qname(q);
                    let qtype = u16::from_be_bytes([q[q.len() - 15], q[q.len() - 14]]);
                    if name == "nx.example" {
                        response(q, 3, &[], &[])
                    } else if name == "alias.example" {
                        response(
                            q,
                            0,
                            &[
                                ("alias.example", 5, 600, encode_name("host.example")),
                                ("host.example", 1, 120, vec![10, 0, 2, 2]),
                            ],
                            &[],
                        )
                    } else if qtype == 1 {
                        response(q, 0, &[(name.as_str(), 1, 300, vec![10, 0, 2, 2])], &[])
                    } else {
                        response(q, 0, &[], &[])
                    }
                }
            };
            let _ = self.b.udp_send(self.dns_sock, d.from, &reply);
        }
    }

    fn configured(&mut self) -> bool {
        self.run_until(10_000, |n| n.a.status().connectivity == Connectivity::Routable)
    }

    fn resolve(&mut self, name: &str) -> ResolveResult {
        let id = self.a.resolve(name, AddrFamily::Any).unwrap();
        for _ in 0..20_000 {
            if let Some(r) = self.a.resolve_result(id) {
                return r;
            }
            self.step();
        }
        panic!("lookup of {name} never finished");
    }
}

fn qname(q: &[u8]) -> String {
    let mut i = 12;
    let mut parts = Vec::new();
    while q[i] != 0 {
        let n = q[i] as usize;
        parts.push(String::from_utf8_lossy(&q[i + 1..i + 1 + n]).into_owned());
        i += 1 + n;
    }
    parts.join(".")
}

fn connect(n: &mut Net, to: SocketAddr) -> SockId {
    let s = n.a.tcp_connect(to, &TcpOptions::default(), 1).unwrap();
    let ok = n.run_until(5_000, |n| matches!(n.a.tcp_view(s).unwrap().state, TcpState::Open { .. }));
    assert!(ok, "connection to {to} not established: {:?}", n.a.tcp_view(s));
    s
}

/// Moves `data` from `a` to `b` over connection (`sa`, `sb`) and checks it
/// arrives intact.
fn transfer(n: &mut Net, sa: SockId, sb: SockId, data: &[u8], max_ms: u64) {
    let mut sent = 0;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 8192];
    for _ in 0..max_ms {
        if sent < data.len() {
            sent += n.a.tcp_send(sa, &data[sent..]).unwrap();
        }
        n.step();
        loop {
            let k = n.b.tcp_recv(sb, &mut buf).unwrap();
            if k == 0 {
                break;
            }
            got.extend_from_slice(&buf[..k]);
        }
        if got.len() == data.len() {
            break;
        }
    }
    assert_eq!(got.len(), data.len(), "transfer incomplete");
    assert!(got == data, "data corrupted");
}

fn accept(n: &mut Net, listener: SockId) -> SockId {
    let mut acc = None;
    n.run_until(5_000, |n| {
        acc = n.b.tcp_accept(listener);
        acc.is_some()
    });
    acc.expect("no connection accepted")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn dhcp_configures_the_interface() {
    let mut n = Net::new();
    assert!(n.configured());
    let info = n.a.interfaces().into_iter().find(|i| i.name == "eth0").unwrap();
    assert!(info.addresses.iter().any(|a| a.address == IpAddr::V4(CLIENT_IP) && a.prefix_len == 24));
    assert_eq!(info.gateways.first(), Some(&IpAddr::V4(SERVER_IP)));
    assert_eq!(info.dns, vec![IpAddr::V4(SERVER_IP)]);
    assert_eq!(info.dhcp.lease_s, 3600);
    let st = n.a.status();
    assert_eq!(st.default_interface, "eth0");
    assert_eq!(st.default_gateway, Some(IpAddr::V4(SERVER_IP)));
    // IPv6 link-local from the MAC.
    assert!(info.addresses.iter().any(|a| matches!(a.address, IpAddr::V6(v) if v.segments()[0] == 0xfe80)));
}

#[test]
fn dhcp_retries_until_a_server_answers() {
    let mut n = Net::new();
    n.dhcp.enabled = false;
    n.run(10_000);
    assert_eq!(n.a.status().connectivity, Connectivity::Offline);
    n.dhcp.enabled = true;
    assert!(n.configured(), "no lease after the server came back");
}

#[test]
fn tcp_round_trip() {
    let mut n = Net::new();
    assert!(n.configured());
    let (l, _) = n.b.tcp_listen("0.0.0.0:7".parse().unwrap(), 4, &TcpOptions::default(), 0).unwrap();
    let sa = connect(&mut n, "10.0.2.2:7".parse().unwrap());
    let sb = accept(&mut n, l);
    let data = pattern(300_000);
    transfer(&mut n, sa, sb, &data, 60_000);
    // And back the other way.
    let back = pattern(50_000);
    let mut sent = 0;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 4096];
    for _ in 0..30_000 {
        if sent < back.len() {
            sent += n.b.tcp_send(sb, &back[sent..]).unwrap();
        }
        n.step();
        while let Ok(k) = n.a.tcp_recv(sa, &mut buf) {
            if k == 0 {
                break;
            }
            got.extend_from_slice(&buf[..k]);
        }
        if got.len() == back.len() {
            break;
        }
    }
    assert!(got == back);
    // A graceful close: the server sees end of stream.
    n.a.close(sa);
    let eof = n.run_until(5_000, |n| n.b.tcp_view(sb).unwrap().eof);
    assert!(eof);
}

#[test]
fn tcp_survives_heavy_loss() {
    let mut n = Net::new();
    assert!(n.configured());
    let (l, _) = n.b.tcp_listen("0.0.0.0:9000".parse().unwrap(), 2, &TcpOptions::default(), 0).unwrap();
    let sa = connect(&mut n, "10.0.2.2:9000".parse().unwrap());
    let sb = accept(&mut n, l);
    n.loss = 15;
    transfer(&mut n, sa, sb, &pattern(120_000), 600_000);
}

#[test]
fn refused_and_timed_out_connections() {
    let mut n = Net::new();
    assert!(n.configured());
    let s = n.a.tcp_connect("10.0.2.2:81".parse().unwrap(), &TcpOptions::default(), 1).unwrap();
    n.run_until(3_000, |n| matches!(n.a.tcp_view(s).unwrap().state, TcpState::Closed { .. }));
    assert_eq!(n.a.tcp_view(s).unwrap().state, TcpState::Closed { error: Some(NetError::ConnectionRefused) });
    // Nobody answers: the connect timeout fires.
    n.dark = true;
    let opts = TcpOptions { connect_timeout_ms: 2_000, ..TcpOptions::default() };
    let t = n.a.tcp_connect("10.0.2.2:80".parse().unwrap(), &opts, 1).unwrap();
    n.run(2_500);
    assert_eq!(n.a.tcp_view(t).unwrap().state, TcpState::Closed { error: Some(NetError::TimedOut) });
}

#[test]
fn reset_connections_report_reset() {
    let mut n = Net::new();
    assert!(n.configured());
    let (l, _) = n.b.tcp_listen("0.0.0.0:7".parse().unwrap(), 1, &TcpOptions::default(), 0).unwrap();
    let sa = connect(&mut n, "10.0.2.2:7".parse().unwrap());
    let sb = accept(&mut n, l);
    n.b.tcp_abort(sb);
    n.run_until(2_000, |n| matches!(n.a.tcp_view(sa).unwrap().state, TcpState::Closed { .. }));
    assert_eq!(n.a.tcp_view(sa).unwrap().state, TcpState::Closed { error: Some(NetError::ConnectionReset) });
}

#[test]
fn short_link_outage_keeps_connections() {
    let mut n = Net::new();
    assert!(n.configured());
    let (l, _) = n.b.tcp_listen("0.0.0.0:7".parse().unwrap(), 1, &TcpOptions::default(), 0).unwrap();
    let sa = connect(&mut n, "10.0.2.2:7".parse().unwrap());
    let sb = accept(&mut n, l);
    transfer(&mut n, sa, sb, &pattern(10_000), 10_000);
    // Wi-Fi drops for five seconds, then reconnects (DHCP runs again and
    // hands out the same address).
    n.a.set_link(n.a_eth, false);
    n.dark = true;
    n.run(5_000);
    n.dark = false;
    n.a.set_link(n.a_eth, true);
    assert!(n.configured());
    transfer(&mut n, sa, sb, &pattern(20_000), 120_000);
    assert!(n.dhcp.requests_seen >= 2);
}

#[test]
fn a_new_address_aborts_old_connections() {
    let mut n = Net::new();
    assert!(n.configured());
    let (l, _) = n.b.tcp_listen("0.0.0.0:7".parse().unwrap(), 1, &TcpOptions::default(), 0).unwrap();
    let sa = connect(&mut n, "10.0.2.2:7".parse().unwrap());
    let _sb = accept(&mut n, l);
    n.a.set_link(n.a_eth, false);
    n.run(100);
    n.dhcp.address = Ipv4Addr::new(10, 0, 2, 16);
    n.a.set_link(n.a_eth, true);
    n.run_until(10_000, |n| matches!(n.a.tcp_view(sa).unwrap().state, TcpState::Closed { .. }));
    assert_eq!(n.a.tcp_view(sa).unwrap().state, TcpState::Closed { error: Some(NetError::NetworkDown) });
}

#[test]
fn long_outage_drops_the_address() {
    let mut n = Net::new();
    assert!(n.configured());
    n.a.set_link(n.a_eth, false);
    n.run(crate::LINK_GRACE_US / 1000 + 100);
    assert_ne!(n.a.status().connectivity, Connectivity::Routable);
    let info = n.a.interfaces().into_iter().find(|i| i.name == "eth0").unwrap();
    assert!(!info.addresses.iter().any(|a| a.address.is_ipv4()));
    assert_eq!(n.a.tcp_connect("10.0.2.2:7".parse().unwrap(), &TcpOptions::default(), 1), Err(NetError::NetworkDown));
}

#[test]
fn removing_the_interface_aborts_connections() {
    let mut n = Net::new();
    assert!(n.configured());
    let (l, _) = n.b.tcp_listen("0.0.0.0:7".parse().unwrap(), 1, &TcpOptions::default(), 0).unwrap();
    let sa = connect(&mut n, "10.0.2.2:7".parse().unwrap());
    let _ = accept(&mut n, l);
    n.a.remove_interface(n.a_eth);
    n.a.poll(n.now + 1000);
    assert_eq!(n.a.tcp_view(sa).unwrap().state, TcpState::Closed { error: Some(NetError::Aborted) });
    assert_eq!(n.a.status().connectivity, Connectivity::Offline);
}

#[test]
fn dns_lookups_cache_and_fail_correctly() {
    let mut n = Net::new();
    assert!(n.configured());
    match n.resolve("www.example") {
        ResolveResult::Found { addresses, ttl_s, .. } => {
            assert_eq!(addresses, vec![IpAddr::V4(SERVER_IP)]);
            assert_eq!(ttl_s, 300);
        }
        other => panic!("{other:?}"),
    }
    let before = n.dns_queries;
    assert!(matches!(n.resolve("WWW.Example."), ResolveResult::Found { .. }));
    assert_eq!(n.dns_queries, before, "second lookup should come from the cache");
    match n.resolve("alias.example") {
        ResolveResult::Found { canonical, ttl_s, .. } => {
            assert_eq!(canonical, "host.example");
            assert_eq!(ttl_s, 120);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(n.resolve("nx.example"), ResolveResult::Failed { error: NetError::NameNotFound });
    let before = n.dns_queries;
    assert_eq!(n.resolve("nx.example"), ResolveResult::Failed { error: NetError::NameNotFound });
    assert_eq!(n.dns_queries, before, "negative answers are cached");
    assert!(matches!(n.resolve("10.1.2.3"), ResolveResult::Found { .. }));
    assert!(matches!(n.resolve("localhost"), ResolveResult::Found { .. }));
    assert_eq!(n.a.resolve("bad name!", AddrFamily::Any), Err(NetError::InvalidArgument));
}

#[test]
fn dns_failures_and_failover() {
    let mut n = Net::new();
    // The first server does not exist; the second answers.
    n.dhcp.dns = vec![Ipv4Addr::new(10, 0, 2, 9), SERVER_IP];
    assert!(n.configured());
    let t0 = n.now;
    assert!(matches!(n.resolve("one.example"), ResolveResult::Found { .. }));
    assert!(n.now - t0 < 5_000_000, "failover took too long");
    n.a.flush_dns_cache();
    n.dns_mode = DnsMode::ServFail;
    assert_eq!(n.resolve("two.example"), ResolveResult::Failed { error: NetError::DnsFailure });
    n.dns_mode = DnsMode::Silent;
    let t0 = n.now;
    assert_eq!(n.resolve("three.example"), ResolveResult::Failed { error: NetError::DnsFailure });
    assert!(n.now - t0 <= crate::dns::LOOKUP_TIMEOUT_US + 10_000, "lookup must give up in time");
    // Every query socket was closed again.
    assert_eq!(n.a.socket_infos().len(), 0);
}

#[test]
fn udp_and_ping() {
    let mut n = Net::new();
    assert!(n.configured());
    let (sa, local) = n.a.udp_bind("0.0.0.0:0".parse().unwrap(), 1).unwrap();
    assert!(local.port() >= 49152);
    let (sb, _) = n.b.udp_bind("0.0.0.0:5000".parse().unwrap(), 0).unwrap();
    n.a.udp_send(sa, "10.0.2.2:5000".parse().unwrap(), b"hello").unwrap();
    let mut buf = [0u8; 64];
    let mut got = None;
    n.run_until(2_000, |n| {
        got = n.b.udp_recv(sb, &mut buf);
        got.is_some()
    });
    let d = got.expect("datagram lost");
    assert_eq!(&buf[..d.len], b"hello");
    assert_eq!(d.from, SocketAddr::new(IpAddr::V4(CLIENT_IP), local.port()));
    // Ping the server (smoltcp answers echo requests).
    let p = n.a.ping_open(1).unwrap();
    n.a.ping_send(p, IpAddr::V4(SERVER_IP), 1, 0, 56).unwrap();
    let mut events = Vec::new();
    n.run_until(2_000, |n| {
        events.extend(n.a.ping_events(p));
        !events.is_empty()
    });
    assert!(matches!(events[0], PingEvent::Reply { seq: 1, size: 56, .. }), "{events:?}");
    // An unanswered ping times out.
    n.dark = true;
    n.a.ping_send(p, IpAddr::V4(SERVER_IP), 2, 0, 56).unwrap();
    let mut events = Vec::new();
    n.run_until(6_000, |n| {
        events.extend(n.a.ping_events(p));
        !events.is_empty()
    });
    assert_eq!(events, vec![PingEvent::Failed { seq: 2, error: NetError::TimedOut }]);
}

#[test]
fn loopback_connections() {
    let mut n = Net::new();
    let (l, _) = n.a.tcp_listen("127.0.0.1:8080".parse().unwrap(), 2, &TcpOptions::default(), 0).unwrap();
    let c = n.a.tcp_connect("127.0.0.1:8080".parse().unwrap(), &TcpOptions::default(), 1).unwrap();
    let mut acc = None;
    n.run_until(2_000, |n| {
        acc = n.a.tcp_accept(l);
        acc.is_some()
    });
    let s = acc.expect("loopback accept");
    n.run(10);
    assert!(matches!(n.a.tcp_view(c).unwrap().state, TcpState::Open { .. }));
    assert_eq!(n.a.tcp_send(c, b"ping").unwrap(), 4);
    let mut buf = [0u8; 8];
    let mut k = 0;
    n.run_until(1_000, |n| {
        k = n.a.tcp_recv(s, &mut buf).unwrap();
        k > 0
    });
    assert_eq!(&buf[..k], b"ping");
}

#[test]
fn routes_prefer_wired_interfaces() {
    let mut n = Net::new();
    assert!(n.configured());
    let wlan = DeviceInfo { kind: InterfaceKind::Wireless, mac: [2, 0, 0, 0, 0, 0x77], ..device(CLIENT_MAC) };
    let w = n.a.add_interface(&wlan, true).unwrap();
    n.a.configure(
        w,
        IpConfig::Static {
            address: "192.168.1.5".parse().unwrap(),
            prefix_len: 24,
            gateway: Some("192.168.1.1".parse().unwrap()),
            dns: vec![],
        },
    )
    .unwrap();
    n.run(10);
    assert_eq!(n.a.status().default_interface, "eth0");
    assert_eq!(n.a.route("8.8.8.8".parse().unwrap()), Ok(n.a_eth));
    // Directly connected networks win over the default route.
    assert_eq!(n.a.route("192.168.1.77".parse().unwrap()), Ok(w));
    n.a.set_link(n.a_eth, false);
    n.run(10);
    assert_eq!(n.a.status().default_interface, "wlan0");
    assert_eq!(n.a.route("8.8.8.8".parse().unwrap()), Ok(w));
}

#[test]
fn hostile_frames_do_not_crash_the_stack() {
    let mut n = Net::new();
    assert!(n.configured());
    let mut rng = Rng(42);
    let mut frame = vec![0u8; 1514];
    for round in 0..20_000 {
        let len = (rng.next() % 1514) as usize;
        for b in frame[..len].iter_mut() {
            *b = rng.next() as u8;
        }
        // Make many of them look like IPv4/IPv6/ARP to reach deeper code.
        if len > 14 {
            frame[0..6].copy_from_slice(&CLIENT_MAC);
            let ty: [u8; 2] = match round % 4 {
                0 => [0x08, 0x00],
                1 => [0x86, 0xDD],
                2 => [0x08, 0x06],
                _ => [frame[12], frame[13]],
            };
            frame[12..14].copy_from_slice(&ty);
        }
        n.a.receive_frame(n.a_eth, &frame[..len]);
        if round % 64 == 0 {
            n.step();
        }
    }
    n.run(100);
    assert!(matches!(n.resolve("still.example"), ResolveResult::Found { .. }));
}

#[test]
fn bad_requests_are_rejected() {
    let mut n = Net::new();
    assert!(n.configured());
    let o = TcpOptions::default();
    assert_eq!(n.a.tcp_connect("10.0.2.2:0".parse().unwrap(), &o, 1), Err(NetError::InvalidArgument));
    assert_eq!(n.a.tcp_connect("0.0.0.0:80".parse().unwrap(), &o, 1), Err(NetError::InvalidArgument));
    assert_eq!(n.a.tcp_connect("224.0.0.1:80".parse().unwrap(), &o, 1), Err(NetError::InvalidArgument));
    assert_eq!(n.a.udp_bind("192.0.2.1:1".parse().unwrap(), 1), Err(NetError::AddressNotAvailable));
    let (_, _) = n.a.udp_bind("0.0.0.0:6000".parse().unwrap(), 1).unwrap();
    assert_eq!(n.a.udp_bind("0.0.0.0:6000".parse().unwrap(), 1), Err(NetError::AddressInUse));
    assert!(
        n.a.configure(
            n.a_eth,
            IpConfig::Static { address: "10.0.0.1".parse().unwrap(), prefix_len: 40, gateway: None, dns: vec![] }
        )
        .is_err()
    );
    assert_eq!(n.a.add_interface(&device([1, 2, 3, 4, 5, 6]), true), Err(vproto::net::NetdevError::Invalid));
}

#[test]
fn socket_limit_is_enforced() {
    let mut n = Net::new();
    let mut ids = Vec::new();
    loop {
        match n.a.ping_open(1) {
            Ok(id) => ids.push(id),
            Err(e) => {
                assert_eq!(e, NetError::LimitReached);
                break;
            }
        }
        assert!(ids.len() <= crate::sockets::MAX_SOCKETS);
    }
    for id in ids {
        n.a.close(id);
    }
    assert!(n.a.ping_open(1).is_ok());
}
