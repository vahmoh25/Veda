//! `nettest` — network checks that run inside Vindows.
//!
//! * `nettest` / `nettest online`: waits for a network connection, then
//!   checks Internet access end to end — DNS, TCP and HTTP to public
//!   servers, UDP (a DNS query sent by hand) and ping. Needs a real
//!   Internet connection on the host.
//! * `nettest local HOST PORT`: hermetic checks against the test servers
//!   that `cargo xtask` runs on the host (an HTTP server and a TCP echo
//!   server reachable at the gateway address).
//!
//! Every check prints `nettest: <name> ... ok` or `... FAILED: <why>`, and
//! the run ends with `nettest: PASS` or `nettest: FAIL (n failed)`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vnet::{Connectivity, Duration, IpAddr, NetError, SocketAddr, TcpStream, UdpSocket};
use vrt::println;

vrt::entry!(main);

type Check = Result<String, String>;

fn net(e: NetError) -> String {
    format!("{e}")
}

/// Waits until the network has a default route.
fn wait_online(timeout: Duration) -> Check {
    let end = vrt::time::now_ns() + timeout.as_nanos() as u64;
    loop {
        if let Ok(st) = vnet::status()
            && st.connectivity == Connectivity::Routable
        {
            return Ok(format!("via {} (gateway {:?})", st.default_interface, st.default_gateway));
        }
        if vrt::time::now_ns() > end {
            return Err(String::from("no network connection"));
        }
        vrt::time::sleep(Duration::from_millis(250));
    }
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
        "local" => {
            let host = args.get(2).cloned().unwrap_or_else(|| String::from("10.0.2.2"));
            let port: u16 = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(8080);
            let url = format!("http://{host}:{port}/hello");
            run(&[
                ("link", &|| wait_online(Duration::from_secs(60))),
                ("http local", &|| check_http(&url, "hello from the host")),
            ])
        }
        other => {
            println!("unknown mode {}", other);
            2
        }
    }
}
