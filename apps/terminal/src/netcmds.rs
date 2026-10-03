//! Network commands: `wifi`, `ifconfig`, `ping`, `nslookup`, `netstat`,
//! `route` and `curl`. They talk to the network and Wi-Fi services through
//! `vnet`; every wait is bounded, as commands run on the terminal's thread.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vnet::wifi::{self, ConnState, WlanEvent};
use vnet::{Duration, IpAddr, NetError};

use crate::screen::{Style, color};
use crate::shell::{Io, Shell};

fn net_err(io: &mut Io, cmd: &str, e: NetError) -> i32 {
    let status = io.error(cmd, &format!("{e}"));
    if e == NetError::Unavailable {
        io.hint("The network service is not running.");
    }
    status
}

fn wifi_err(io: &mut Io, e: wifi::WifiError) -> i32 {
    io.error("wifi", &format!("{e}"))
}

fn bars(n: u8) -> &'static str {
    [
        "\u{2581}    ",
        "\u{2581}\u{2583}   ",
        "\u{2581}\u{2583}\u{2585}  ",
        "\u{2581}\u{2583}\u{2585}\u{2587} ",
        "\u{2581}\u{2583}\u{2585}\u{2587}\u{2588}",
    ][n.min(4) as usize]
}

fn pad(s: &str, w: usize) -> String {
    let mut out = String::from(s);
    for _ in s.chars().count()..w {
        out.push(' ');
    }
    out
}

fn print_status(io: &mut Io, s: &wifi::WlanStatus) {
    let state = match s.state {
        ConnState::NoAdapter => "no Wi-Fi adapter",
        ConnState::RadioOff => "off",
        ConnState::Disconnected => "disconnected",
        ConnState::Authenticating => "authenticating",
        ConnState::Associating => "associating",
        ConnState::Securing => "exchanging keys",
        ConnState::Connected => "connected",
    };
    io.styled("Wi-Fi: ", Style::BOLD);
    io.println(state);
    if s.state != ConnState::NoAdapter {
        io.println(&format!("  adapter   {} ({})", s.adapter, vnet::format_mac(&s.mac)));
    }
    if !s.ssid.0.is_empty()
        && matches!(
            s.state,
            ConnState::Connected | ConnState::Authenticating | ConnState::Associating | ConnState::Securing
        )
    {
        io.println(&format!("  network   {}", s.name));
        io.println(&format!("  security  {}", s.security.label()));
        io.println(&format!("  access pt {} on channel {}", vnet::format_mac(&s.bssid), s.channel));
        io.println(&format!("  signal    {} dBm {}", s.signal_dbm, bars(wifi::signal_bars(s.signal_dbm))));
        if s.state == ConnState::Connected {
            io.println(&format!("  connected {} s", s.connected_s));
        }
    }
    if let Some(f) = s.last_failure {
        io.styled(&format!("  last failure: {f}\n"), Style::DIM);
    }
}

fn print_networks(io: &mut Io, nets: &[wifi::NetworkInfo]) {
    if nets.is_empty() {
        io.println("No networks found.");
        return;
    }
    let w = nets.iter().map(|n| n.name.chars().count()).max().unwrap_or(4).max(4) + 2;
    io.styled(&format!("  {}SIGNAL         SECURITY\n", pad("NETWORK", w)), Style::DIM);
    for n in nets {
        let mark = if n.connected { "*" } else { " " };
        let line = format!(
            "{mark} {}{:>4} dBm {}  {}{}",
            pad(&n.name, w),
            n.signal_dbm,
            bars(n.bars),
            n.security.label(),
            if n.saved { " (saved)" } else { "" }
        );
        if n.connected {
            io.styled(&format!("{line}\n"), Style::fg(color::BRIGHT_GREEN));
        } else {
            io.println(&line);
        }
    }
}

/// Starts a scan and waits (briefly) for it to finish.
fn scan_and_wait(io: &mut Io) -> Result<(), i32> {
    let watcher = wifi::Watcher::new().map_err(|e| wifi_err(io, e))?;
    wifi::scan().map_err(|e| wifi_err(io, e))?;
    let end = vrt::time::now_ns() + 8_000_000_000;
    while vrt::time::now_ns() < end {
        match watcher.next(Duration::from_millis(500)) {
            Ok(Some(WlanEvent::ScanDone {})) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(wifi_err(io, e)),
        }
    }
    Ok(())
}

const WIFI_USAGE: &str = "\
usage: wifi [status]                 show the connection
       wifi scan | list              look for networks / list them
       wifi connect SSID [PASSWORD]  join a network (saved for next time)
       wifi disconnect
       wifi saved | forget SSID | auto SSID on|off
       wifi on | off                 switch the radio
       wifi aps | log                access points / diagnostics";

pub fn cmd_wifi(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let sub = args.get(1).map(String::as_str).unwrap_or("status");
    match sub {
        "status" => match wifi::status() {
            Ok(s) => {
                print_status(io, &s);
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "scan" => {
            if let Err(code) = scan_and_wait(io) {
                return code;
            }
            match wifi::networks() {
                Ok(n) => {
                    print_networks(io, &n);
                    0
                }
                Err(e) => wifi_err(io, e),
            }
        }
        "list" | "networks" => match wifi::networks() {
            Ok(n) => {
                print_networks(io, &n);
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "connect" => {
            let Some(ssid) = args.get(2) else {
                return io.error("wifi", "connect: which network? (wifi connect SSID [PASSWORD])");
            };
            let password = args.get(3).map(String::as_str);
            let watcher = wifi::Watcher::new().ok();
            if let Err(e) = wifi::connect(ssid, password, true) {
                return wifi_err(io, e);
            }
            io.styled(&format!("Joining {ssid}\u{2026}\n"), Style::DIM);
            // Wait for the outcome.
            let end = vrt::time::now_ns() + 30_000_000_000;
            loop {
                if let Some(w) = &watcher
                    && let Ok(Some(WlanEvent::ConnectFailed { reason, .. })) = w.next(Duration::from_millis(200))
                {
                    return io.error("wifi", &format!("could not join {ssid}: {reason}"));
                }
                match wifi::status() {
                    Ok(s) if s.state == ConnState::Connected && s.name == *ssid => {
                        io.styled(&format!("Connected to {ssid}",), Style::fg(color::BRIGHT_GREEN));
                        io.println(&format!(" ({}, channel {}, {} dBm)", s.security.label(), s.channel, s.signal_dbm));
                        return 0;
                    }
                    Ok(_) => {}
                    Err(e) => return wifi_err(io, e),
                }
                if vrt::time::now_ns() > end {
                    return io.error("wifi", "no answer yet; 'wifi status' shows the progress");
                }
                if watcher.is_none() {
                    vrt::time::sleep(Duration::from_millis(200));
                }
            }
        }
        "disconnect" => match wifi::disconnect() {
            Ok(()) => {
                io.println("Disconnected.");
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "saved" => match wifi::saved() {
            Ok(list) if list.is_empty() => {
                io.println("No saved networks.");
                0
            }
            Ok(list) => {
                for s in list {
                    io.println(&format!(
                        "{}  {}{}{}",
                        s.name,
                        s.security.label(),
                        if s.auto_connect { ", connects automatically" } else { "" },
                        if s.hidden { ", hidden" } else { "" }
                    ));
                }
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "forget" => {
            let Some(ssid) = args.get(2) else { return io.error("wifi", "forget: which network?") };
            match wifi::forget(ssid.as_bytes()) {
                Ok(()) => {
                    io.println(&format!("Forgot {ssid}."));
                    0
                }
                Err(e) => wifi_err(io, e),
            }
        }
        "auto" => {
            let (Some(ssid), Some(v)) = (args.get(2), args.get(3)) else {
                return io.error("wifi", "usage: wifi auto SSID on|off");
            };
            let on = match v.as_str() {
                "on" | "yes" | "1" => true,
                "off" | "no" | "0" => false,
                _ => return io.error("wifi", "usage: wifi auto SSID on|off"),
            };
            match wifi::set_auto_connect(ssid.as_bytes(), on) {
                Ok(()) => 0,
                Err(e) => wifi_err(io, e),
            }
        }
        "on" | "off" => match wifi::set_radio(sub == "on") {
            Ok(()) => {
                io.println(if sub == "on" { "Wi-Fi is on." } else { "Wi-Fi is off." });
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "aps" => match wifi::access_points() {
            Ok(list) => {
                io.styled("BSSID              CH  SIGNAL  AGE   SECURITY          SSID\n", Style::DIM);
                for b in list {
                    io.println(&format!(
                        "{}  {:>3}  {:>4} dBm {:>4}s  {}{}",
                        vnet::format_mac(&b.bssid),
                        b.channel,
                        b.signal_dbm,
                        b.age_ms / 1000,
                        pad(b.security.label(), 18),
                        if b.ssid.0.is_empty() { String::from("(hidden)") } else { wifi::ssid_display(&b.ssid.0) }
                    ));
                }
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "log" | "diag" | "diagnostics" => match wifi::diagnostics() {
            Ok(d) => {
                print_status(io, &d.status);
                let c = d.counters;
                io.println(&format!(
                    "  scans {}, attempts {}, connections {}, disconnections {}, authentication failures {}",
                    c.scans, c.connect_attempts, c.connections, c.disconnections, c.auth_failures
                ));
                io.println(&format!(
                    "  frames sent {}, received {}, decryption errors {}, replays {}, unprotected dropped {}, rekeys {}, roams {}",
                    c.data_tx, c.data_rx, c.decrypt_errors, c.replays, c.unprotected_dropped, c.group_rekeys, c.roams
                ));
                for e in d.log {
                    io.styled(&format!("[{:>6}.{:03}] ", e.time_ms / 1000, e.time_ms % 1000), Style::DIM);
                    io.println(&e.message);
                }
                0
            }
            Err(e) => wifi_err(io, e),
        },
        "help" | "-h" | "--help" => {
            io.println(WIFI_USAGE);
            0
        }
        other => {
            io.error("wifi", &format!("unknown command '{other}'"));
            io.hint(WIFI_USAGE);
            2
        }
    }
}

pub fn cmd_ifconfig(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let all = args.iter().any(|a| a == "-a");
    let list = match vnet::interfaces() {
        Ok(l) => l,
        Err(e) => return net_err(io, "ifconfig", e),
    };
    for i in list.iter().filter(|i| all || i.kind != vnet::InterfaceKind::Loopback) {
        let kind = match i.kind {
            vnet::InterfaceKind::Loopback => "loopback",
            vnet::InterfaceKind::Wireless => "wireless",
            _ => "ethernet",
        };
        io.styled(&i.name, Style::BOLD);
        io.println(&format!(
            ": {kind}, link {}, mtu {}, {} ({})",
            if i.link_up { "up" } else { "down" },
            i.mtu,
            vnet::format_mac(&i.mac),
            i.driver
        ));
        for a in &i.addresses {
            let fam = if a.address.is_ipv4() { "inet " } else { "inet6" };
            io.println(&format!("    {fam} {}/{} ({:?})", a.address, a.prefix_len, a.origin));
        }
        if !i.gateways.is_empty() {
            let g: Vec<String> = i.gateways.iter().map(|g| g.to_string()).collect();
            io.println(&format!("    gateway {}", g.join(", ")));
        }
        if !i.dns.is_empty() {
            let d: Vec<String> = i.dns.iter().map(|d| d.to_string()).collect();
            io.println(&format!("    dns     {}", d.join(", ")));
        }
        io.println(&format!("    dhcp    {:?}, lease {} s", i.dhcp.state, i.dhcp.lease_s));
        let s = i.stats;
        io.styled(
            &format!(
                "    rx {} packets ({} bytes, {} dropped, {} errors), tx {} packets ({} bytes, {} dropped)\n",
                s.rx_packets, s.rx_bytes, s.rx_dropped, s.rx_errors, s.tx_packets, s.tx_bytes, s.tx_dropped
            ),
            Style::DIM,
        );
    }
    match vnet::status() {
        Ok(s) => {
            io.println(&format!(
                "status: {:?}{}",
                s.connectivity,
                if s.default_interface.is_empty() { String::new() } else { format!(" via {}", s.default_interface) }
            ));
            0
        }
        Err(e) => net_err(io, "ifconfig", e),
    }
}

pub fn cmd_ping(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let mut count = 4u16;
    let mut host = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "-n" => count = it.next().and_then(|v| v.parse().ok()).unwrap_or(4).clamp(1, 20),
            h => host = Some(h.to_string()),
        }
    }
    let Some(host) = host else { return io.error("ping", "usage: ping HOST [-c COUNT]") };
    let target: IpAddr = match host.parse() {
        Ok(a) => a,
        Err(_) => match vnet::resolve(&host) {
            Ok(list) if !list.is_empty() => list.iter().find(|a| a.is_ipv4()).copied().unwrap_or(list[0]),
            Ok(_) => return io.error("ping", &format!("{host}: no addresses")),
            Err(e) => return net_err(io, "ping", e),
        },
    };
    let mut p = match vnet::Pinger::new() {
        Ok(p) => p,
        Err(e) => return net_err(io, "ping", e),
    };
    io.println(&format!("PING {host} ({target}): 56 data bytes"));
    let mut rtts = Vec::new();
    for seq in 1..=count {
        match p.ping(target, seq, 56, 0) {
            Ok(r) => {
                rtts.push(r.rtt_us);
                io.println(&format!(
                    "{} bytes from {}: seq={} time={}.{:03} ms",
                    r.size + 8,
                    r.from,
                    r.seq,
                    r.rtt_us / 1000,
                    r.rtt_us % 1000
                ));
            }
            Err(e) => io.styled(&format!("seq={seq}: {e}\n"), Style::fg(color::YELLOW)),
        }
    }
    let lost = count as usize - rtts.len();
    io.println(&format!("--- {host}: {count} sent, {} received, {}% loss", rtts.len(), lost * 100 / count as usize));
    if let (Some(min), Some(max)) = (rtts.iter().min(), rtts.iter().max()) {
        let avg = rtts.iter().map(|&x| x as u64).sum::<u64>() / rtts.len() as u64;
        io.println(&format!(
            "round trip min/avg/max = {}.{:03}/{}.{:03}/{}.{:03} ms",
            min / 1000,
            min % 1000,
            avg / 1000,
            avg % 1000,
            max / 1000,
            max % 1000
        ));
    }
    if rtts.is_empty() { 1 } else { 0 }
}

pub fn cmd_nslookup(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let Some(name) = args.get(1) else { return io.error("nslookup", "usage: nslookup NAME") };
    match vnet::lookup(name, vnet::AddrFamily::Any, vnet::RESOLVE_TIMEOUT) {
        Ok(l) => {
            if l.canonical != *name && !l.canonical.is_empty() {
                io.println(&format!("{name} is an alias for {}", l.canonical));
            }
            for a in &l.addresses {
                io.println(&format!("{name} has address {a}"));
            }
            io.styled(&format!("(cached for {} s)\n", l.ttl_s), Style::DIM);
            0
        }
        Err(e) => net_err(io, "nslookup", e),
    }
}

pub fn cmd_netstat(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    match vnet::sockets() {
        Ok(list) => {
            io.styled("PROTO  LOCAL                    REMOTE                   STATE         PID\n", Style::DIM);
            for s in list {
                let a = |x: Option<vnet::SocketAddr>| x.map(|a| a.to_string()).unwrap_or_else(|| String::from("*"));
                io.println(&format!(
                    "{}  {}{}{}{}",
                    pad(&format!("{:?}", s.kind).to_ascii_lowercase(), 5),
                    pad(&a(s.local), 25),
                    pad(&a(s.remote), 25),
                    pad(&s.state, 14),
                    s.owner
                ));
            }
            0
        }
        Err(e) => net_err(io, "netstat", e),
    }
}

pub fn cmd_route(_sh: &mut Shell, io: &mut Io, _args: &[String]) -> i32 {
    match vnet::routes() {
        Ok(list) => {
            io.styled("DESTINATION                GATEWAY                  INTERFACE  METRIC\n", Style::DIM);
            for r in list {
                let dest = format!("{}/{}", r.destination, r.prefix_len);
                let gw = r.gateway.map(|g| g.to_string()).unwrap_or_else(|| String::from("(direct)"));
                io.println(&format!("{}{}{}{}", pad(&dest, 27), pad(&gw, 25), pad(&r.interface, 11), r.metric));
            }
            0
        }
        Err(e) => net_err(io, "route", e),
    }
}

pub fn cmd_curl(_sh: &mut Shell, io: &mut Io, args: &[String]) -> i32 {
    let head_only = args.iter().any(|a| a == "-I");
    let Some(url) = args.iter().skip(1).find(|a| !a.starts_with('-')) else {
        return io.error("curl", "usage: curl [-I] http://HOST[:PORT]/PATH");
    };
    let url = if url.contains("://") { url.clone() } else { format!("http://{url}") };
    match vnet::http::get(&url, Duration::from_secs(20), 1 << 20) {
        Ok(r) => {
            if head_only {
                io.println(&format!("HTTP/1.1 {} {}", r.status, r.reason));
                for (k, v) in &r.headers {
                    io.println(&format!("{k}: {v}"));
                }
            } else {
                let body = String::from_utf8_lossy(&r.body);
                io.print(&body);
                if !body.ends_with('\n') {
                    io.print("\n");
                }
            }
            if r.status >= 400 { 22 } else { 0 }
        }
        Err(e) => {
            let status = io.error("curl", &format!("{e}"));
            if url.starts_with("https://") {
                io.hint("HTTPS is not supported yet (TLS arrives in a later version).");
            }
            status
        }
    }
}
