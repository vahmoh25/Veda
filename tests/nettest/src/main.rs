//! `nettest` — network checks that run inside Veda.
//!
//! * `nettest` / `nettest online`: waits for a network connection, then
//!   checks Internet access end to end — DNS, TCP and HTTP to public
//!   servers, UDP (a DNS query sent by hand) and ping. Needs a real
//!   Internet connection on the host.
//! * `nettest local HOST PORT`: fetches `http://HOST:PORT/hello` and expects
//!   "hello from the host", for a test server on the host machine (QEMU's
//!   NAT makes the host reachable at the gateway address, 10.0.2.2).
//! * `nettest lan`: the online checks, after checking that the address came
//!   from a real network rather than a hypervisor's NAT (a PC's own), plus
//!   TCP over IPv6 when there is a global address.
//! * `nettest ipv6`: TCP over IPv6 to example.com.
//! * `nettest https`: HTTPS requests with `vtls`: `GET /` from example.com
//!   (expects 200 and the page) and `GET /v1/projects` from
//!   api.deepgram.com without a key (expects 401), reporting the TLS
//!   version, cipher suite, key exchange and the handshake time.
//! * `nettest wifi [SSID [PASSWORD]]`: joins a Wi-Fi network through the
//!   Wi-Fi service (by default the simulated "Veda Home" network of
//!   `cargo xtask run --net wifi`), then runs the online checks over it.
//! * `nettest wifi-watch [SSID [PASSWORD]]`: joins (and saves) the network,
//!   then reports every change of what a user would notice — the Wi-Fi
//!   state and access point, whether the gateway answers pings, whether DNS
//!   answers — as `watch: wifi=... ping=... dns=...` lines, forever. Tests
//!   break the simulated network meanwhile and wait for the recovery.
//!
//! * `nettest route-watch [SSID [PASSWORD]]`: like `wifi-watch`, but reports
//!   the interface of the default route instead of the Wi-Fi state
//!   (`watch: route=... ping=... dns=...`), for machines with both a wired
//!   card and Wi-Fi.
//!
//! In SSID arguments `+` stands for a space (the kernel command line
//! splits arguments at spaces).
//!
//! Every check prints `nettest: <name> ... ok` or `... FAILED: <why>`, and
//! the run ends with `nettest: PASS` or `nettest: FAIL (n failed)`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vnet::{Connectivity, Duration, IpAddr, NetError, SocketAddr, TcpStream, UdpSocket, wifi};
use vrt::println;

vrt::entry!(main);

type Check = Result<String, String>;

fn net(e: NetError) -> String {
    format!("{e}")
}

/// Waits until the network has a default route (through `via`, if given).
fn wait_online_via(timeout: Duration, via: Option<&str>) -> Check {
    let end = vrt::time::now_ns() + timeout.as_nanos() as u64;
    loop {
        if let Ok(st) = vnet::status()
            && st.connectivity == Connectivity::Routable
            && via.is_none_or(|v| st.default_interface == v)
        {
            return Ok(format!("via {} (gateway {:?})", st.default_interface, st.default_gateway));
        }
        if vrt::time::now_ns() > end {
            return Err(String::from("no network connection"));
        }
        vrt::time::sleep(Duration::from_millis(250));
    }
}

fn wait_online(timeout: Duration) -> Check {
    wait_online_via(timeout, None)
}

fn wifi(e: wifi::WifiError) -> String {
    format!("{e}")
}

/// TCP over IPv6 to example.com, when the machine has a global IPv6
/// address. Reported but never a failure: many networks have no IPv6.
fn check_ipv6_internet() -> Check {
    match ipv6_tcp(true) {
        Ok(s) => Ok(s),
        Err(e) => Ok(format!("unavailable ({e}); IPv4 is what counts here")),
    }
}

/// TCP over IPv6 to example.com from any routable IPv6 address (QEMU's NAT
/// uses site-local ones).
fn ipv6_tcp(global_only: bool) -> Check {
    let end = vrt::time::now_ns() + 20_000_000_000;
    let global = loop {
        let found = vnet::interfaces().map_err(net)?.iter().flat_map(|i| i.addresses.iter().map(|a| a.address)).find(
            |a| match a {
                IpAddr::V6(v) if global_only => (v.segments()[0] & 0xE000) == 0x2000,
                IpAddr::V6(v) => !v.is_loopback() && (v.segments()[0] & 0xFFC0) != 0xFE80,
                _ => false,
            },
        );
        if found.is_some() || vrt::time::now_ns() > end {
            break found;
        }
        vrt::time::sleep(Duration::from_millis(250));
    };
    let Some(own) = global else { return Ok(String::from("skipped: no global IPv6 address")) };
    let l = vnet::lookup("example.com", vnet::AddrFamily::V6, Duration::from_secs(15)).map_err(net)?;
    let target = *l.addresses.first().ok_or("example.com has no IPv6 address")?;
    let s = TcpStream::connect_timeout(SocketAddr::new(target, 80), Duration::from_secs(20)).map_err(net)?;
    Ok(format!("connected to {} from {:?} (own address {})", s.peer_addr(), s.local_addr(), own))
}

/// Waits (up to a minute) for an IPv4 lease on the wired interface, shows
/// the configuration, and fails if it is a hypervisor's NAT (10.0.2.0/24,
/// or 10.0.3.0/24 for the simulated Wi-Fi).
fn check_lan() -> Check {
    let end = vrt::time::now_ns() + 60_000_000_000;
    let ifaces = loop {
        let ifaces = vnet::interfaces().map_err(net)?;
        let leased = ifaces.iter().any(|i| {
            i.kind == vnet::InterfaceKind::Ethernet && i.link_up && i.addresses.iter().any(|a| a.address.is_ipv4())
        });
        if leased {
            break ifaces;
        }
        if vrt::time::now_ns() > end {
            let states: Vec<String> = ifaces.iter().map(|i| format!("{}: DHCP {:?}", i.name, i.dhcp.state)).collect();
            return Err(format!("no IPv4 lease after 60 s ({})", states.join(", ")));
        }
        vrt::time::sleep(Duration::from_millis(250));
    };
    let eth = ifaces
        .iter()
        .find(|i| i.kind == vnet::InterfaceKind::Ethernet && i.link_up)
        .ok_or("no wired interface is up")?;
    let v4 = eth
        .addresses
        .iter()
        .find_map(|a| match a.address {
            IpAddr::V4(v) => Some((v, a.prefix_len)),
            _ => None,
        })
        .ok_or("no IPv4 address")?;
    let o = v4.0.octets();
    if o[0] == 10 && o[1] == 0 && (o[2] == 2 || o[2] == 3) {
        return Err(format!("{}/{} is a hypervisor's NAT, not a real network", v4.0, v4.1));
    }
    let gw: Vec<String> = eth.gateways.iter().map(|g| format!("{g}")).collect();
    let dns: Vec<String> = eth.dns.iter().map(|d| format!("{d}")).collect();
    Ok(format!(
        "{} has {}/{} from the network's DHCP server; gateway {}, DNS {}",
        eth.name,
        v4.0,
        v4.1,
        gw.join(", "),
        dns.join(", ")
    ))
}

/// Waits for the Wi-Fi service and its adapter (both may still be starting).
fn check_wifi_adapter() -> Check {
    let end = vrt::time::now_ns() + 60_000_000_000;
    while !wifi::available() {
        if vrt::time::now_ns() > end {
            return Err(String::from("the Wi-Fi service is not running"));
        }
        vrt::time::sleep(Duration::from_millis(250));
    }
    let s = wifi::wait_for(Duration::from_secs(60), |s| s.state != wifi::ConnState::NoAdapter).map_err(wifi)?;
    if s.state == wifi::ConnState::NoAdapter {
        return Err(String::from("no Wi-Fi adapter"));
    }
    Ok(format!("{} {:02x?}", s.adapter, s.mac))
}

/// Scans until `ssid` shows up.
fn check_wifi_scan(ssid: &str) -> Check {
    let end = vrt::time::now_ns() + 60_000_000_000;
    loop {
        let _ = wifi::scan();
        let w = wifi::Watcher::new().map_err(wifi)?;
        while let Ok(Some(ev)) = w.next(Duration::from_secs(5)) {
            if matches!(ev, wifi::WlanEvent::ScanDone {}) {
                break;
            }
        }
        let nets = wifi::networks().map_err(wifi)?;
        if let Some(n) = nets.iter().find(|n| n.name == ssid) {
            let names: Vec<&str> = nets.iter().map(|n| n.name.as_str()).collect();
            return Ok(format!(
                "{} ({}, {} dBm, {} access points); in range: {}",
                n.name,
                n.security.label(),
                n.signal_dbm,
                n.access_points,
                names.join(", ")
            ));
        }
        if vrt::time::now_ns() > end {
            return Err(format!("{ssid} not found"));
        }
    }
}

/// Joins `ssid` and waits for the connection.
fn check_wifi_connect(ssid: &str, password: &str) -> Check {
    let pass = (!password.is_empty()).then_some(password);
    wifi::connect(ssid, pass, false).map_err(wifi)?;
    let t0 = vrt::time::now_ns();
    let s = wifi::wait_for(Duration::from_secs(45), |s| {
        s.state == wifi::ConnState::Connected || (s.state == wifi::ConnState::Disconnected && s.last_failure.is_some())
    })
    .map_err(wifi)?;
    if s.state != wifi::ConnState::Connected {
        return Err(match s.last_failure {
            Some(f) => format!("{f}"),
            None => format!("still {:?}", s.state),
        });
    }
    Ok(format!(
        "{} via {:02x?}, channel {}, {} dBm, {} in {} ms",
        s.name,
        s.bssid,
        s.channel,
        s.signal_dbm,
        s.security.label(),
        (vrt::time::now_ns() - t0) / 1_000_000
    ))
}

fn check_dns(host: &str) -> Check {
    let l = vnet::lookup(host, vnet::AddrFamily::Any, Duration::from_secs(15)).map_err(net)?;
    if l.addresses.is_empty() {
        return Err(String::from("no addresses"));
    }
    let list: Vec<String> = l.addresses.iter().map(|a| format!("{a}")).collect();
    Ok(format!("{} -> {} (ttl {} s)", host, list.join(", "), l.ttl_s))
}

fn check_http(url: &str, expect: &str) -> Check {
    let t0 = vrt::time::now_ns();
    let r = vnet::http::get(url, Duration::from_secs(20), 4 << 20).map_err(|e| format!("{e}"))?;
    let ms = (vrt::time::now_ns() - t0) / 1_000_000;
    if r.status != 200 {
        return Err(format!("HTTP {} {}", r.status, r.reason));
    }
    let body = String::from_utf8_lossy(&r.body);
    if !body.contains(expect) {
        return Err(format!("body does not contain {expect:?} ({} bytes)", r.body.len()));
    }
    Ok(format!("{} bytes in {} ms", r.body.len(), ms))
}

/// How [`check_https`] connects.
#[derive(Clone, Copy)]
enum Connect {
    /// `vtls::connect`: name lookup, TCP and the TLS handshake in one call.
    Together,
    /// TCP first, then the handshake (`TlsStream::connect`), timed alone.
    Separately,
}

fn ms_since(t0: u64) -> u64 {
    (vrt::time::now_ns() - t0) / 1_000_000
}

/// `GET path` from `host` over HTTPS: expects HTTP status `status` and, if
/// given, `text` in the response.
fn check_https(host: &str, path: &str, status: u16, text: Option<&str>, how: Connect) -> Check {
    let config = vtls::ClientConfig::with_alpn(&[b"http/1.1"]);
    let t0 = vrt::time::now_ns();
    let (mut tls, timing) = match how {
        Connect::Together => {
            let tls = vtls::connect(host, 443, Duration::from_secs(60), &config).map_err(|e| format!("{e}"))?;
            (tls, format!("connected in {} ms (lookup, TCP, handshake)", ms_since(t0)))
        }
        Connect::Separately => {
            let mut tcp = TcpStream::connect_host(host, 443, Duration::from_secs(20)).map_err(net)?;
            let tcp_ms = ms_since(t0);
            tcp.set_read_timeout(Some(Duration::from_secs(60)));
            let t1 = vrt::time::now_ns();
            let tls = vtls::TlsStream::connect(tcp, host, &config).map_err(|e| format!("{e}"))?;
            (tls, format!("TCP {tcp_ms} ms, TLS handshake {} ms", ms_since(t1)))
        }
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Veda-nettest\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(request.as_bytes()).map_err(|e| format!("sending the request: {e}"))?;
    tls.transport_mut().set_read_timeout(Some(Duration::from_secs(30)));
    let mut response = Vec::new();
    let mut buf = alloc::vec![0u8; 16 * 1024];
    loop {
        let n = tls.read(&mut buf).map_err(|e| format!("reading the response: {e}"))?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
        if response.len() > 4 << 20 {
            return Err(String::from("response larger than 4 MiB"));
        }
    }
    tls.close();
    let total_ms = ms_since(t0);
    let text_of = String::from_utf8_lossy(&response);
    let status_line = text_of.lines().next().unwrap_or_default();
    let code: Option<u16> = status_line.strip_prefix("HTTP/1.1 ").and_then(|rest| rest.get(..3)?.parse().ok());
    if code != Some(status) {
        return Err(format!("expected HTTP {status}, got {status_line:?}"));
    }
    if let Some(text) = text
        && !text_of.contains(text)
    {
        return Err(format!("the response ({} bytes) does not contain {text:?}", response.len()));
    }
    Ok(format!(
        "{status_line}, {} bytes; {}, {}, {}; {timing}, total {total_ms} ms",
        response.len(),
        tls.protocol_version(),
        tls.cipher_suite(),
        tls.key_exchange_group()
    ))
}

fn check_tcp(host: &str, port: u16) -> Check {
    let s = TcpStream::connect_host(host, port, Duration::from_secs(20)).map_err(net)?;
    Ok(format!("connected to {} from {:?}", s.peer_addr(), s.local_addr()))
}

/// Sends a DNS query for `example.com` by hand to the first DNS server.
fn check_udp() -> Check {
    let st = vnet::status().map_err(net)?;
    let server = *st.dns_servers.first().ok_or("no DNS server")?;
    let mut sock = UdpSocket::bind(SocketAddr::new(IpAddr::V4(vnet::Ipv4Addr::UNSPECIFIED), 0)).map_err(net)?;
    let mut q: Vec<u8> = alloc::vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["example", "com"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    sock.send_to(&q, SocketAddr::new(server, 53)).map_err(net)?;
    let mut buf = [0u8; 1500];
    let (n, from) = sock.recv_from(&mut buf, Some(Duration::from_secs(5))).map_err(net)?;
    if n < 12 || buf[0..2] != [0x12, 0x34] || buf[2] & 0x80 == 0 {
        return Err(String::from("not a DNS answer"));
    }
    Ok(format!("{n}-byte answer from {from}"))
}

fn check_ping(target: IpAddr) -> Check {
    let mut p = vnet::Pinger::new().map_err(net)?;
    let mut got = Vec::new();
    for seq in 1..=3 {
        match p.ping(target, seq, 56, 0) {
            Ok(r) => got.push(r.rtt_us),
            Err(e) => return Err(format!("echo {seq}: {e}")),
        }
    }
    let avg = got.iter().map(|&x| x as u64).sum::<u64>() / got.len() as u64;
    Ok(format!("3/3 replies from {target}, average {}.{:03} ms", avg / 1000, avg % 1000))
}

/// Asks the DNS server for `example.com` by hand: "ok", "servfail" (or
/// another error code) or "fail" (no answer).
fn dns_probe(server: IpAddr, id: u16) -> String {
    let Ok(mut sock) = UdpSocket::bind(SocketAddr::new(IpAddr::V4(vnet::Ipv4Addr::UNSPECIFIED), 0)) else {
        return String::from("fail");
    };
    let mut q: Vec<u8> = alloc::vec![(id >> 8) as u8, id as u8, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["example", "com"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    if sock.send_to(&q, SocketAddr::new(server, 53)).is_err() {
        return String::from("fail");
    }
    let mut buf = [0u8; 1500];
    let end = vrt::time::now_ns() + 3_000_000_000;
    while vrt::time::now_ns() < end {
        match sock.recv_from(&mut buf, Some(Duration::from_millis(500))) {
            Ok((n, _)) if n >= 12 && buf[0..2] == q[0..2] && buf[2] & 0x80 != 0 => {
                return match buf[3] & 0x0F {
                    0 => String::from("ok"),
                    2 => String::from("servfail"),
                    c => format!("rcode{c}"),
                };
            }
            _ => {}
        }
    }
    String::from("fail")
}

/// `nettest wifi-watch` / `route-watch`: joins the network, then reports
/// changes forever.
fn watch(ssid: &str, password: &str, by_route: bool) -> i32 {
    for (name, check) in
        [("wifi adapter", &check_wifi_adapter as &dyn Fn() -> Check), ("wifi scan", &|| check_wifi_scan(ssid))]
    {
        match check() {
            Ok(d) => println!("{} ... ok: {}", name, d),
            Err(e) => {
                println!("{} ... FAILED: {}", name, e);
                println!("FAIL (1 failed)");
                return 1;
            }
        }
    }
    // Saved, so the Wi-Fi service rejoins by itself after failures.
    let pass = (!password.is_empty()).then_some(password);
    if let Err(e) = wifi::connect(ssid, pass, true) {
        println!("wifi connect ... FAILED: {}", e);
        println!("FAIL (1 failed)");
        return 1;
    }
    println!("watch: started for {}", ssid);
    let mut last = String::new();
    let mut seq: u16 = 0;
    loop {
        let wifi_part = match wifi::status() {
            Ok(s) if s.state == wifi::ConnState::Connected => format!("connected ap={}", vnet::format_mac(&s.bssid)),
            Ok(s) => format!("{:?}", s.state).to_ascii_lowercase(),
            Err(_) => String::from("unavailable"),
        };
        let st = vnet::status().ok();
        let online = st
            .as_ref()
            .is_some_and(|s| s.connectivity == Connectivity::Routable && (by_route || s.default_interface == "wlan0"));
        let route = match &st {
            Some(s) if s.connectivity == Connectivity::Routable => s.default_interface.clone(),
            _ => String::from("none"),
        };
        let (ping, dns) = match (online, st) {
            (true, Some(s)) => {
                seq = seq.wrapping_add(1);
                let ping = match (s.default_gateway, vnet::Pinger::new()) {
                    (Some(gw), Ok(mut p)) => {
                        if p.ping(gw, seq, 56, 0).is_ok() {
                            "ok"
                        } else {
                            "fail"
                        }
                    }
                    _ => "fail",
                };
                let dns = match s.dns_servers.first() {
                    Some(&server) => dns_probe(server, 0x4000 | seq),
                    None => String::from("none"),
                };
                (ping, dns)
            }
            _ => ("none", String::from("none")),
        };
        let line = if by_route {
            format!("watch: route={route} ping={ping} dns={dns}")
        } else {
            format!("watch: wifi={wifi_part} ping={ping} dns={dns}")
        };
        if line != last {
            println!("{}", line);
            last = line;
        }
        vrt::time::sleep(Duration::from_millis(500));
    }
}

fn run(checks: &[(&str, &dyn Fn() -> Check)]) -> i32 {
    let mut failed = 0;
    for (name, f) in checks {
        match f() {
            Ok(detail) => println!("{} ... ok: {}", name, detail),
            Err(e) => {
                failed += 1;
                println!("{} ... FAILED: {}", name, e);
            }
        }
    }
    if failed == 0 {
        println!("PASS");
        0
    } else {
        println!("FAIL ({} failed)", failed);
        1
    }
}

fn main() -> i32 {
    let args = vrt::env::args();
    let mode = args.get(1).map(String::as_str).unwrap_or("online");
    match mode {
        "online" => {
            let gateway = || -> Check {
                let st = vnet::status().map_err(net)?;
                let gw = st.default_gateway.ok_or("no gateway")?;
                check_ping(gw)
            };
            run(&[
                ("link", &|| wait_online(Duration::from_secs(60))),
                ("dns example.com", &|| check_dns("example.com")),
                ("dns www.wikipedia.org", &|| check_dns("www.wikipedia.org")),
                ("dns failure", &|| match vnet::resolve("no-such-host.invalid") {
                    Err(NetError::NameNotFound) => Ok(String::from("name not found, as expected")),
                    Err(e) => Err(format!("unexpected error {e}")),
                    Ok(a) => Err(format!("resolved to {a:?}")),
                }),
                ("udp dns query", &check_udp),
                ("tcp 1.1.1.1:443", &|| check_tcp("1.1.1.1", 443)),
                ("tcp example.com:80", &|| check_tcp("example.com", 80)),
                ("http example.com", &|| check_http("http://example.com/", "Example Domain")),
                ("ping gateway", &gateway),
            ])
        }
        "lan" => {
            let gateway = || -> Check {
                let st = vnet::status().map_err(net)?;
                let gw = st.default_gateway.ok_or("no gateway")?;
                check_ping(gw)
            };
            run(&[
                ("link", &|| wait_online(Duration::from_secs(60))),
                ("real network", &check_lan),
                ("dns example.com", &|| check_dns("example.com")),
                ("dns www.wikipedia.org", &|| check_dns("www.wikipedia.org")),
                ("dns failure", &|| match vnet::resolve("no-such-host.invalid") {
                    Err(NetError::NameNotFound) => Ok(String::from("name not found, as expected")),
                    Err(e) => Err(format!("unexpected error {e}")),
                    Ok(a) => Err(format!("resolved to {a:?}")),
                }),
                ("udp dns query", &check_udp),
                ("tcp 1.1.1.1:443", &|| check_tcp("1.1.1.1", 443)),
                ("tcp example.com:80", &|| check_tcp("example.com", 80)),
                ("http example.com", &|| check_http("http://example.com/", "Example Domain")),
                ("tcp over ipv6", &check_ipv6_internet),
                ("ping gateway", &gateway),
            ])
        }
        "ipv6" => run(&[("link", &|| wait_online(Duration::from_secs(60))), ("tcp over ipv6", &|| ipv6_tcp(false))]),
        "https" => run(&[
            ("link", &|| wait_online(Duration::from_secs(60))),
            ("https example.com", &|| {
                check_https("example.com", "/", 200, Some("Example Domain"), Connect::Separately)
            }),
            ("https api.deepgram.com", &|| {
                check_https("api.deepgram.com", "/v1/projects", 401, None, Connect::Together)
            }),
        ]),
        "local" => {
            let host = args.get(2).cloned().unwrap_or_else(|| String::from("10.0.2.2"));
            let port: u16 = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(8080);
            let url = format!("http://{host}:{port}/hello");
            run(&[
                ("link", &|| wait_online(Duration::from_secs(60))),
                ("http local", &|| check_http(&url, "hello from the host")),
            ])
        }
        "wifi-watch" => {
            let ssid = args.get(2).map(|s| s.replace('+', " ")).unwrap_or_else(|| String::from("Veda Home"));
            let password = args.get(3).cloned().unwrap_or_else(|| String::from("veda-wifi"));
            watch(&ssid, &password, false)
        }
        "route-watch" => {
            let ssid = args.get(2).map(|s| s.replace('+', " ")).unwrap_or_else(|| String::from("Veda Home"));
            let password = args.get(3).cloned().unwrap_or_else(|| String::from("veda-wifi"));
            watch(&ssid, &password, true)
        }
        "wifi" => {
            let ssid = args.get(2).map(|s| s.replace('+', " ")).unwrap_or_else(|| String::from("Veda Home"));
            let password = args.get(3).cloned().unwrap_or_else(|| String::from("veda-wifi"));
            let gateway = || -> Check {
                let st = vnet::status().map_err(net)?;
                let gw = st.default_gateway.ok_or("no gateway")?;
                check_ping(gw)
            };
            run(&[
                ("wifi adapter", &check_wifi_adapter),
                ("wifi scan", &|| check_wifi_scan(&ssid)),
                ("wifi connect", &|| check_wifi_connect(&ssid, &password)),
                ("link", &|| wait_online_via(Duration::from_secs(60), Some("wlan0"))),
                ("dns example.com", &|| check_dns("example.com")),
                ("udp dns query", &check_udp),
                ("tcp 1.1.1.1:443", &|| check_tcp("1.1.1.1", 443)),
                ("http example.com", &|| check_http("http://example.com/", "Example Domain")),
                ("ping gateway", &gateway),
            ])
        }
        other => {
            println!("unknown mode {}", other);
            2
        }
    }
}
