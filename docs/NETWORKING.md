# Networking and Wi-Fi

Vindows reaches the Internet over Ethernet and Wi-Fi. As everywhere else
in the system, the work is split into isolated user-space processes that
talk over kernel channels; the kernel knows nothing about networks.

```text
 applications (Settings, the taskbar's network flyout, Terminal, nettest, ...)
        |  vnet: sockets, DNS, status          vnet::wifi: networks, joining
        |  ("net" protocol)                    ("wlan" protocol)
        v                                      v
 +----------------------+    "netdev"    +----------------------------+
 | netd                 |<---------------| wlan  (the Wi-Fi service)  |
 | vnetstack + smoltcp: |  wlan0 frames  | vwlan: scanning, SAE,      |
 | interfaces, DHCP,    |  (Ethernet)    | association, 4-way and     |
 | SLAAC, routes, DNS,  |                | group handshakes, CCMP,    |
 | TCP, UDP, ICMP       |                | management frame           |
 +----------------------+                | protection, saved networks,|
        ^ "netdev"                       | reconnection, roaming      |
        | eth0 frames                    +----------------------------+
 +--------------+                               ^ "wlanphy"
 | virtio-net   |                               | raw 802.11 frames
 | (driver)     |                        +--------------+
 +--------------+                        | vwifi        |  virtio-serial port
                                         | (driver)     |  org.vindows.wlan.0
                                         +--------------+
```

* **Drivers** move frames and nothing else. A network card driver
  (`virtio-net`, `e1000`) offers Ethernet frames to `netd`; a radio driver
  (`vwifi`) offers raw 802.11 frames to `wlan` ("soft MAC"). Neither sees
  keys, passwords or addresses.
* **`wlan`** is the Wi-Fi service. Everything above the radio happens here:
  scanning, authentication, association, the key handshakes, encryption,
  saved networks and the decisions of when and where to connect. Each
  radio appears to `netd` as the Ethernet-like interface `wlan0`, whose
  link goes up when a network is joined.
* **`netd`** is the network service: IPv4 and IPv6, DHCP, IPv6 address
  autoconfiguration, routing between interfaces, a caching DNS resolver,
  and the sockets applications use. The protocol engine is
  [smoltcp](https://github.com/smoltcp-rs/smoltcp), vendored in
  `third_party/smoltcp` with one documented patch (see below).
* **Applications** use `vnet` (`lib/net`): blocking, `std::net`-like
  `TcpStream`, `TcpListener`, `UdpSocket`, name resolution, ping, a small
  HTTP client, and `vnet::wifi` for Wi-Fi.

Frames travel between processes through shared-memory rings
(`vproto::netring`): a pair of single-producer/single-consumer rings per
link with wake-up events, so a frame costs one copy into the ring and one
out, and a system call only when the other side is asleep.

## Components

| Path | What it is |
|------|------------|
| `services/netd` | the network service (`net` and `netdev` protocols) |
| `lib/netstack` | `vnetstack`: interfaces, DHCP, SLAAC, routes, DNS, sockets on smoltcp |
| `services/wlan` | the Wi-Fi service (`wlan` and `wlanphy` protocols) |
| `lib/wlan` | `vwlan`: IEEE 802.11 frames, RSN, CCMP/BIP, EAPOL, handshakes, SAE, station and access point state machines, saved-network format, connection policy |
| `drivers/virtio-net` | virtio network card driver |
| `drivers/e1000` | Intel PRO/1000 driver: 82540EM (QEMU `e1000`, VirtualBox), 82545EM (VMware), 82574L (QEMU `e1000e`) |
| `drivers/vwifi` | the virtual Wi-Fi radio (virtio-console port) |
| `lib/radiolink` | `vradiolink`: the message format between `vwifi` and `airsim` |
| `lib/net` | `vnet`: the application API, including `vnet::wifi` |
| `lib/proto/src/{net,wlan,netring}.rs` | the service protocols and frame rings |
| `lib/entropy`, `kernel/src/random.rs` | the kernel's random number generator |
| `tools/airsim` | the simulated Wi-Fi environment on the host |
| `tests/nettest` | in-system network checks |
| `tests/ui/wifi-*.vts` | Wi-Fi GUI and failure-recovery tests |

## Wi-Fi

### Joining a network

1. **Scanning.** The service tunes the radio to each channel in turn for
   130 ms, sends a probe request where transmitting is allowed (and
   directed probes for hidden networks), and records every beacon and
   probe response it hears. Networks are grouped by name and security
   for the network list.
2. **Authentication.** Open system authentication for WPA2 and open
   networks; Simultaneous Authentication of Equals (SAE, WPA3) with both
   the hunting-and-pecking and the hash-to-element password element.
3. **Association**, with an RSN element matching the access point's
   ciphers (CCMP-128) and management frame protection (required for WPA3,
   used whenever the access point is capable).
4. **The 4-way handshake** derives the session keys from the PMK (the
   PBKDF2 pre-shared key for WPA2, the SAE key for WPA3) and installs the
   pairwise and group keys; the group key handshake renews group keys.
5. The link to `netd` goes up; `netd` runs DHCP (and SLAAC) on `wlan0`, and
   the default route moves to it unless a wired connection is up (Ethernet
   has the lower route metric).

### Staying connected

* **Saved networks** (`/home/user/.config/wlan/networks`) are joined
  automatically, preferring the most recently used, then the strongest
  access point (5 GHz gets a small bonus).
* **Lost connections** — the access point stops answering, disconnects
  the station, or the signal fades — are re-established after a fresh scan,
  on any access point of the network. An access point that stopped
  answering is not tried again until it is heard again.
* **Failed attempts** are retried with growing delays (2, 4, 8, 16, 32,
  then every 60 seconds).
* **Weak connections** (below -72 dBm) look for an access point of the
  same network that is at least 10 dB stronger and move to it, keeping the
  IP configuration.
* **A refused password** is not retried until the user connects to the
  network again (repeated attempts could get the device blocked).
* **The radio** may go away and come back (the driver reports it, the
  service rejoins); if the driver stops responding to control calls for two
  seconds, the service drops the radio rather than hanging. If the Wi-Fi
  service exits, `init` restarts it and the driver offers the radio again.

### Security

* All key material comes from the kernel's ChaCha20 random number
  generator, seeded from the firmware's `EFI_RNG_PROTOCOL`, RDSEED, RDRAND
  and timing jitter.
* Every received frame is untrusted: parsers are bounds-checked and never
  panic, MICs and authentication tags are compared in constant time, and
  received key material is used only after the standard's checks pass.
  CCMP and BIP enforce replay counters; duplicate frames are dropped.
* Unprotected deauthentication and disassociation frames are ignored once
  management frame protection is in use; an unprotected one starts an SA
  Query to check whether the access point really lost the association.
* **No downgrades:** a network saved with security is never joined through
  an open access point; a network saved as WPA3, or one where WPA3 was used
  once, is never joined through an access point offering only WPA2; WEP,
  TKIP-only WPA, Enterprise and OWE networks are listed but never joined.
* The RSN element the access point sends in the handshake must match the
  one it advertised.
* Passphrases are checked (8-63 printable characters or 64 hex digits),
  never logged and never returned by the API. The Terminal does not keep
  them in its history. Session keys are zeroed when a connection ends.
* The saved-networks file holds the passphrases (needed to rejoin); the
  file system has no access control yet, so any program can read it.

### The virtual radio and `airsim`

QEMU cannot emulate a Wi-Fi adapter, and a Windows host cannot pass its own
adapter through. Under QEMU the radio is therefore a virtio-serial port
named `org.vindows.wlan.0` whose other end is **airsim**
(`tools/airsim`), a host program that simulates the radio medium and a set
of access points built on `vwlan`'s access point state machine. airsim
bridges the access points to a QEMU user-mode network (NAT) through a hub
and a UDP link, so the guest reaches the real Internet over (simulated)
Wi-Fi. It talks only to QEMU over the loopback interface; it has nothing to
do with the host's own Wi-Fi.

| Name | SSID | Security | Channel | Signal |
|------|------|----------|---------|--------|
| home | Vindows Home | WPA2-Personal | 1 | -48 dBm |
| home2 | Vindows Home | WPA2-Personal | 11 | -66 dBm |
| wpa3 | Vindows WPA3 | WPA3-Personal (H2E, PMF required) | 6 | -55 dBm |
| mixed | Vindows Mixed | WPA2/WPA3 transition | 44 | -60 dBm |
| guest | Vindows Guest | open | 36 | -71 dBm |
| hidden | (Vindows Hidden) | WPA2-Personal, hidden | 6 | -63 dBm |
| corp | Vindows Corp | Enterprise (not supported) | 11 | -78 dBm |

The password of every secured network is `vindows-wifi`. The Wi-Fi NAT is
10.0.3.0/24 (gateway 10.0.3.2, DNS 10.0.3.3), distinct from the wired
NAT's 10.0.2.0/24 so both can be used at once.

```bash
cargo xtask run --net wifi
```

boots with the Wi-Fi radio only (`--net both` adds the wired card). airsim
logs to `target/vindows/airsim.log` and accepts commands on the control
port printed at startup — one per line, answered by `ok` or `error: ...`:

```text
list | status                       access points; radio, wired side and counters
ap NAME on|off                      switch an access point (on = restarted)
ap NAME signal DBM                  signal at the guest (-92 and below: out of range)
ap NAME loss PERCENT                frame loss in each direction
ap NAME deauth [REASON]             disconnect its stations
ap NAME rekey                       send new group keys
ap NAME password PASSWORD           change the password (restarts it)
wired up|down                       the network behind the access points
dhcp on|off                         let DHCP through or not
dns normal|unanswered|servfail      how DNS queries are treated
radio drop [SECONDS]                cut the guest's radio link for a while
```

A driver for real hardware would implement the same `wlanphy` protocol as
`vwifi`: report its channels, move frames through the link and tune the
radio. The service needs no change.

## Using the network

* **The taskbar's network icon** shows the Wi-Fi signal (or the wired
  connection) and opens the network flyout: Wi-Fi on and off, networks in
  range, joining one (with its password, optionally automatically in
  future), hidden networks, disconnecting, and a link to Settings.
* **Settings → Network & Internet**: the Wi-Fi connection in detail,
  networks in range, saved networks (automatic connection, forget), every
  interface with its addresses, gateway, DNS servers, DHCP lease and
  traffic, and the Wi-Fi diagnostics log.
* **Terminal**: `wifi` (`status`, `scan`, `list`, `connect SSID
  [PASSWORD]`, `disconnect`, `saved`, `forget`, `auto`, `on`, `off`,
  `aps`, `log`), `ifconfig`, `ping`, `nslookup`, `netstat`, `route`,
  `curl`.

### For applications

```rust
use vnet::{Duration, TcpStream};

let mut s = TcpStream::connect_host("example.com", 80, Duration::from_secs(20))?;
let page = vnet::http::get("http://example.com/", Duration::from_secs(20), 1 << 20)?;

vnet::wifi::scan()?;
for n in vnet::wifi::networks()? {
    // n.name, n.security, n.signal_dbm, n.bars, n.saved, n.connected
}
vnet::wifi::connect("Vindows Home", Some("vindows-wifi"), true)?;
let events = vnet::wifi::Watcher::new()?; // status changes, scans, failures
```

HTTPS and other TLS protocols will be layered on `TcpStream`; the random
number generator, the hash and cipher primitives (RustCrypto) and the
blocking socket API they need are in place.

## Dependencies

The protocols are implemented with mature Rust crates where they exist,
built `no_std`:

* **smoltcp 0.14** (TCP/IP), vendored with one patch: its random numbers
  (TCP initial sequence numbers, DHCP and DNS ids, source ports) come from
  a ChaCha20 stream keyed from the kernel's generator instead of the
  predictable upstream sPCG32 (`third_party/smoltcp/VINDOWS-PATCHES.md`).
* **RustCrypto**: `sha1`, `sha2`, `hmac`, `pbkdf2`, `aes`, `ccm`, `cmac`,
  `aes-kw`, `p256` (SAE), plus `subtle` (constant-time comparisons) and
  `zeroize`.
* **httparse** for the HTTP client.

Everything else — the 802.11 protocol, the handshakes, SAE's protocol
logic, the services and drivers — is in this repository. The cryptography
is checked against published test vectors: RFC 8439 (ChaCha20), RFC 7693
(BLAKE2s), RFC 3394 (AES key wrap), RFC 4493 (AES-CMAC), the IEEE 802.11
PSK and PRF vectors, the IEEE 802.11i CCMP test MPDUs and the
IEEE 802.11-2020 Annex J.10 SAE vectors.

## Testing

* **Host tests** (`cargo xtask test` runs them all): `vwlan` (frames, RSN,
  crypto vectors, handshakes, SAE, and station-against-access-point runs:
  every security type, wrong passwords, hidden networks, replays,
  deauthentication, silent access points, reconnection), `vnetstack`
  (two stacks over a simulated cable: DHCP including lease renewal and
  server outages, TCP, UDP, DNS, ICMP, IPv6), `vradiolink`, `airsim`
  (the simulator driven by the station: every network, failover, range,
  rekeying, password changes, lossy links, DHCP and DNS conditions,
  malformed frames), and the profile and policy modules.
* **In-system**: `nettest` (`run=nettest`, `run=nettest:wifi`): DNS, UDP,
  TCP, HTTP and ping over Ethernet or Wi-Fi. These checks, and the scripts
  below, reach real Internet hosts through QEMU's NAT, so the host must be
  online.
* **GUI and recovery** (`cargo xtask test --ui`): `tests/ui/wifi-connect.vts`
  joins networks through the flyout (a wrong password first), Settings and
  the Terminal; `tests/ui/wifi-recovery.vts` breaks the network through
  airsim — access point gone, disconnection, wired side down, DNS failure,
  out of range, the radio vanishing, rekeying and loss, the Wi-Fi service
  killed — and checks that Vindows recovers by itself each time;
  `tests/ui/network-failover.vts` unplugs and replugs the wired card's
  cable (QMP `set_link`) with Wi-Fi connected and checks that traffic moves
  between the interfaces; `tests/ui/e1000.vts` and `e1000e.vts` run the
  Intel PRO/1000 driver on QEMU's two models of that card.

## Limitations and next steps

* Drivers for real Wi-Fi hardware (the `wlanphy` protocol is ready for
  soft-MAC drivers; full-MAC adapters would need a variant of it).
* WPA2/WPA3-Enterprise (802.1X/EAP), Enhanced Open (OWE), fast roaming
  (802.11r), power saving, 802.11n/ac rate control.
* TLS (HTTPS) and a certificate store, for the web browser and cloud
  services of the next phase.
* Encrypting the saved passphrases at rest.
