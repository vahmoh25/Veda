# Networking and Wi-Fi

Veda reaches the Internet over Ethernet and Wi-Fi. As everywhere else
in the system, the work is split into isolated user-space processes that
talk over kernel channels; the kernel knows nothing about networks.

```text
 applications (Settings, the taskbar's network flyout, Terminal, nettest, ...)
        |  vnet: sockets, DNS, status          vnet::wifi: networks, joining
        |  ("net" protocol)                    ("wlan" protocol)
        v                                      v
 +----------------------+    "netdev"    +----------------------------+
 | netd                 |<---------------| wlan  (the Wi-Fi service)  |
 | vnetstack + smoltcp: |  wlan0 frames  | vwlan: choosing networks,  |
 | interfaces, DHCP,    |  (Ethernet)    | SAE, 4-way and group       |
 | SLAAC, routes, DNS,  |                | handshakes, saved networks,|
 | TCP, UDP, ICMP       |                | reconnection, roaming      |
 +----------------------+                +----------------------------+
        ^ "netdev"                              ^ "wlanphy" (managed:
        | eth0 frames                           |  commands, Ethernet frames)
 ===== the driver VM ==========================================================
 +--------------+                        +--------------+
 | net          |  Linux's network       | wifi         |  Linux's Wi-Fi radios
 | (guest/net)  |  cards                 | (guest/wifi) |  (nl80211; airlink
 +--------------+                        +--------------+  makes QEMU's virtual
                                                           radio one)
```

* **Drivers** move frames and nothing else, and they are Linux's, in
  [the driver VM](DRIVERVM.md): `net` offers each of Linux's network
  cards to `netd` as an Ethernet device, and `wifi` each of its Wi-Fi
  radios to `wlan` as a *managed* radio, which joins access points on
  `wlan`'s commands (the cards of PCs keep the MAC in their firmware) and
  moves Ethernet frames (see below). Neither sees passwords or addresses;
  a radio gets the session keys only.
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
  HTTP client, and `vnet::wifi` for Wi-Fi; and `vtls` (`lib/tls`) for TLS
  (HTTPS, secure WebSockets) over a `TcpStream`, inside the application's
  own process.

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
| `guest/net`, `guest/wifi`, `guest/airlink` | in the driver VM: Linux's Ethernet cards as `netdev` devices; Linux's Wi-Fi radios as managed radios; QEMU's virtual radio as one of Linux's (mac80211_hwsim) |
| `lib/radiolink` | `vradiolink`: the message format between `airlink` and `airsim` |
| `lib/net` | `vnet`: the application API, including `vnet::wifi` |
| `lib/tls` | `vtls`: TLS 1.3 and 1.2 client (rustls with a pure-Rust cryptography provider) |
| `lib/proto/src/{net,wlan,netring}.rs` | the service protocols and frame rings |
| `lib/entropy`, `kernel/src/random.rs` | the kernel's random number generator |
| `tools/airsim` | the simulated Wi-Fi environment on the host |
| `tests/nettest` | in-system network checks |
| `tests/ui/wifi-connect.vts`, `drivervm-wifi*.vts` | Wi-Fi GUI and failure-recovery tests |

## Wi-Fi

### Joining a network

1. **Scanning.** The service has the radio scan every channel (probe
   requests where transmitting is allowed, and directed ones for hidden
   networks) and records every beacon and probe response it reports.
   Networks are grouped by name and security for the network list.
2. **Authentication.** Open system authentication for WPA2 and open
   networks; Simultaneous Authentication of Equals (SAE, WPA3), whose
   commit and confirm the service computes, with both the
   hunting-and-pecking and the hash-to-element password element, and the
   radio sends.
3. **Association**, by the radio, with the service's RSN element, matching
   the access point's ciphers (CCMP-128) and management frame protection
   (required for WPA3, used whenever the access point is capable).
4. **The 4-way handshake**, the service's, derives the session keys from
   the PMK (the PBKDF2 pre-shared key for WPA2, the SAE key for WPA3) and
   gives the radio the pairwise and group keys; the group key handshake
   renews group keys.
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

QEMU cannot emulate a Wi-Fi adapter. Under QEMU the radio is therefore a
virtio-serial port named `org.veda.wlan.0` whose other end is **airsim**
(`tools/airsim`), a host program that simulates the radio medium and a set
of access points built on `vwlan`'s access point state machine. The port
goes to the driver VM, where `airlink` makes it a radio of Linux's (see
below). airsim
bridges the access points to a QEMU user-mode network (NAT) through a hub
and a datagram link, so the guest reaches the real Internet over
(simulated) Wi-Fi. It talks to QEMU, and takes the tests' commands, over
Unix sockets in the run's directory (none is a port that another run on
the machine could take); it has nothing to do with the host's own Wi-Fi.

| Name | SSID | Security | Channel | Signal |
|------|------|----------|---------|--------|
| home | Veda Home | WPA2-Personal | 1 | -48 dBm |
| home2 | Veda Home | WPA2-Personal | 11 | -66 dBm |
| wpa3 | Veda WPA3 | WPA3-Personal (H2E, PMF required) | 6 | -55 dBm |
| mixed | Veda Mixed | WPA2/WPA3 transition | 44 | -60 dBm |
| guest | Veda Guest | open | 36 | -71 dBm |
| hidden | (Veda Hidden) | WPA2-Personal, hidden | 6 | -63 dBm |
| corp | Veda Corp | Enterprise (not supported) | 11 | -78 dBm |

The password of every secured network is `veda-wifi`. The Wi-Fi NAT is
10.0.3.0/24 (gateway 10.0.3.2, DNS 10.0.3.3), distinct from the wired
NAT's 10.0.2.0/24 so both can be used at once.

```bash
cargo xtask run --net wifi
```

boots with the Wi-Fi radio only (`--net both` adds the wired card). airsim
logs to `target/veda/airsim.log` and accepts commands on the control
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

The service can also drive a radio that moves raw 802.11 frames and does
nothing else (a *soft MAC* radio, `wlanphy_ctl`), being its MLME itself:
scanning channel by channel, sending the authentication and association
frames, encrypting with CCMP. Veda's own driver of the virtual radio was
one; it went with Veda's other network drivers, and no radio of that kind
is left.

### Managed radios: Linux's Wi-Fi

The cards of PCs (Intel's, MediaTek's, Realtek's, Qualcomm's) keep their
MAC in firmware: their drivers do not move raw frames, and Linux's 802.11
stack (cfg80211 and mac80211) builds on that. Veda uses them through
[the driver VM](DRIVERVM.md), where `wifi` (`guest/wifi`) offers each of
Linux's radios to `wlan` as a *managed* radio (`phy_caps::MANAGED`): its
control channel speaks `wlanmlme_ctl` instead of `wlanphy_ctl`, and the
station of `vwlan` drives it with commands rather than frames:

| The station | `wlanmlme_ctl` | Linux (nl80211) |
|-------------|----------------|-----------------|
| scans | `scan(ssids)`; results as beacons and probe responses on the link, then `ScanDone` | `TRIGGER_SCAN`, `GET_SCAN` |
| authenticates (open; SAE's commit and confirm, computed by the station) | `authenticate(bssid, channel, ssid, body)` | `AUTHENTICATE` with `AUTH_DATA`; the answer as a frame |
| associates (its RSN and RSNX elements, PMF) | `associate(...)` | `ASSOCIATE`, the control port over nl80211, owned by `wifi`'s socket |
| runs the 4-way and group handshakes | `send_eapol(peer, frame, encrypt)`; EAPOL from the AP as Ethernet frames | `CONTROL_PORT_FRAME` (message 4 in clear, whatever key follows it) |
| installs the session keys only | `install_key(kind, index, key, rsc, peer)` | `NEW_KEY` (CCMP-128, BIP-CMAC-128) |
| opens the port | `authorize(peer)` | `SET_STATION` (authorized) |
| checks an unprotected deauthentication (SA Query) | `send_management(frame)`; `UnprotectedDeauth` | `FRAME`; `UNPROT_DEAUTHENTICATE` |
| leaves | `deauthenticate(bssid, reason)` | `DEAUTHENTICATE` |

Linux retries, times out (`Timeout`), encrypts and decrypts, answers the
access point's SA Queries, and watches the link (`LinkLost` when it gives
up on the access point; `Signal` as it changes); the station decides, and
authenticates, as with a soft-MAC radio. Linux joins only access points it
heard lately: `wifi` listens on the access point's channel first when it
must. The Wi-Fi service is the same for both kinds of radio, and so are
the user's networks and the connection policy.

Under QEMU, `airlink` (`guest/airlink`) makes the virtual radio's port one
of Linux's simulated radios (`mac80211_hwsim`), whose medium it is, so the
whole path runs against airsim's networks: `tests/ui/drivervm-wifi.vts`
joins every kind of network through it, and
`tests/ui/drivervm-wifi-recovery.vts` breaks them. airsim's radio then hears every channel (radio link version 2's
`LISTEN`), each frame with its own, and Linux keeps what is on its
channel.

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
vnet::wifi::connect("Veda Home", Some("veda-wifi"), true)?;
let events = vnet::wifi::Watcher::new()?; // status changes, scans, failures

let config = vtls::ClientConfig::with_alpn(&[b"http/1.1"]); // share it
let mut tls = vtls::connect("example.com", 443, Duration::from_secs(20), &config)?;
tls.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")?;
let n = tls.read(&mut buf)?; // Ok(0): the server closed the connection
```

## TLS

`vtls` (`lib/tls`) is the TLS client of Veda applications: HTTPS,
secure WebSockets, anything else over TLS 1.3 or TLS 1.2. It runs in the
application's process, on top of a `vnet::TcpStream`.

* **Protocol.** [rustls](https://github.com/rustls/rustls) 0.23, through
  its `no_std` "unbuffered" API: `TlsStream` keeps the received TLS bytes,
  the decrypted data and the records to send, lets rustls process what
  arrived, sends what rustls produces and reads from the socket only when
  rustls needs more. Partial records, several records per read, key
  updates, session tickets, alerts and close_notify are handled inside.
* **Cryptography** is vtls' own rustls provider on pure-Rust crates (no C,
  no assembly): AES-128-GCM, AES-256-GCM and ChaCha20-Poly1305 records;
  SHA-256/384 and HMAC, with rustls' HKDF and TLS 1.2 PRF over them; X25519,
  secp256r1 and secp384r1 key exchange; ECDSA (P-256, P-384), Ed25519, RSA
  PKCS#1 v1.5 and RSA-PSS signatures. The cipher suites are the three TLS
  1.3 ones and ECDHE-ECDSA/ECDHE-RSA with AES-GCM or ChaCha20-Poly1305 for
  TLS 1.2. RSA verification is implemented in vtls on `crypto-bigint`,
  with *ring*'s rules (2048 to 8192-bit moduli, PSS salts as long as the
  hash). Ephemeral secrets come from the kernel's ChaCha20 generator and
  are zeroed after use.
* **Certificates** are verified by rustls-webpki against the Mozilla root
  program (webpki-roots, 121 roots compiled in), at the time of the system
  clock (the firmware's clock, advanced by the kernel). A clock that reads
  earlier than 2025 is treated as unset, and the handshake fails with a
  message saying so rather than with misleading expiry errors.
* **Errors** are `TlsError`s with messages for users: "the server's
  certificate is not trusted: it expired on 2025-01-01 (the system clock
  reads 2026-06-01)", "secure connection failed: the server refused the
  connection: no security parameters in common". After a protocol error
  the alert rustls produces is sent to the server; every later call
  reports the same error. Nothing panics on what the network sends.
* **Event loops** wait for `tls.transport().handle()` to become readable
  and call `try_read` until it returns `Ok(None)`: it decrypts whatever
  the socket holds without waiting.

Under QEMU's TCG emulation a full TLS 1.3 handshake with example.com
(X25519, an ECDSA P-256/P-384 chain) took 115 ms, the network round trip
included. Every connection is a full handshake (there is no session cache
yet).

`run=nettest:https` checks it from inside Veda (see Testing).

## Dependencies

The protocols are implemented with mature Rust crates where they exist,
built `no_std`:

* **smoltcp 0.14** (TCP/IP), vendored with one patch: its random numbers
  (TCP initial sequence numbers, DHCP and DNS ids, source ports) come from
  a ChaCha20 stream keyed from the kernel's generator instead of the
  predictable upstream sPCG32 (`third_party/smoltcp/VEDA-PATCHES.md`).
* **RustCrypto**: `sha1`, `sha2`, `hmac`, `pbkdf2`, `aes`, `ccm`, `cmac`,
  `aes-kw`, `p256` (SAE), plus `subtle` (constant-time comparisons) and
  `zeroize`.
* **httparse** for the HTTP client.
* **TLS** (`vtls`):
  * **rustls 0.23.45** (`tls12`, `custom-provider`; no default features):
    the most widely used Rust TLS implementation, memory safe and
    `no_std`-capable through its unbuffered API. It brings
    **rustls-webpki 0.103** (certificate path validation) and
    **rustls-pki-types 1**. The 0.24 line is still a pre-release.
  * **webpki-roots 1.0.9**: the Mozilla root certificates, compiled in.
  * **aes-gcm 0.11**, **chacha20poly1305 0.11**, **p384 0.14** and the
    `ecdsa`/`ecdh` features of **p256 0.14** (pulling **ecdsa 0.17**),
    **crypto-bigint 0.7**: the RustCrypto generation the Wi-Fi code
    already uses (`aes` 0.9, `sha2` 0.11, `hmac` 0.13, `p256` 0.14), so no
    crate family is built twice.
  * **x25519-dalek 3** and **ed25519-dalek 3** (on curve25519-dalek 5):
    the standard X25519 and Ed25519 implementations, same generation
    (`sha2` 0.11, `rand_core` 0.10, unused here: secrets are made from
    the kernel's random bytes).
  * Not used: *ring* and aws-lc-rs (rustls' usual providers) contain C and
    assembly; the `rsa` crate's current line (0.10) is only a release
    candidate, and verification, all a TLS client needs, is short and
    involves no secrets, so vtls implements it on `crypto-bigint`. The
    lock file lists `ring` because of a weak optional feature of
    rustls-webpki; it is never built.

  LLVM cannot build the SIMD code (AES-NI, SSE2, AVX2) of the RustCrypto
  and dalek crates for the soft-float UEFI target, which only the `no_std`
  check of the libraries uses: `.cargo/config.toml` selects their portable
  code for that target (Veda's user space keeps the fast code, chosen at
  run time from the processor's features).

Everything else — the 802.11 protocol, the handshakes, SAE's protocol
logic, the TLS stream and provider, the services and drivers — is in this
repository. The cryptography is checked against published test vectors:
RFC 8439 (ChaCha20), RFC 7693 (BLAKE2s), RFC 3394 (AES key wrap), RFC 4493
(AES-CMAC), the IEEE 802.11 PSK and PRF vectors, the IEEE 802.11i CCMP
test MPDUs, the IEEE 802.11-2020 Annex J.10 SAE vectors, and for TLS the
RFC 8448 handshake trace (X25519, protected records, RSA-PSS and PKCS#1
signatures), RFC 4231 (HMAC), RFC 8032 (Ed25519), the TLS 1.2 PRF vector
and RSA signatures made by OpenSSL.

## Testing

* **Host tests** (`cargo xtask test` runs them all): `vwlan` (frames, RSN,
  crypto vectors, handshakes, SAE, and station-against-access-point runs:
  every security type, wrong passwords, hidden networks, replays,
  deauthentication, silent access points, reconnection), `vnetstack`
  (two stacks over a simulated cable: DHCP including lease renewal and
  server outages, TCP, UDP, DNS, ICMP, IPv6), `vradiolink`, `airsim`
  (the simulator driven by the station: every network, failover, range,
  rekeying, password changes, lossy links, DHCP and DNS conditions,
  malformed frames), the profile and policy modules, and `vtls`: the
  provider against the vectors above, handshakes with a rustls server in
  memory (every cipher suite, key exchange group and certificate type, data
  both ways, closing from either side, key updates, records split down to
  single bytes, expired, future, misnamed, untrusted and client-only
  certificates, broken servers, every bit of the server's first flight
  damaged in turn), and the certificate chains of api.deepgram.com,
  example.com and www.google.com captured on 2026-10-03, verified against
  the built-in roots. `cargo test -p vtls -- --ignored --nocapture`
  connects to those servers for real with each cipher suite and group.
* **In-system**: `nettest` (`run=nettest`, `run=nettest:wifi`): DNS, UDP,
  TCP, HTTP and ping over Ethernet or Wi-Fi; `run=nettest:https`: HTTPS
  requests to example.com (200 and the page) and api.deepgram.com (401
  without a key) with the TLS version, cipher suite and handshake time.
  These checks, and the scripts below, reach real Internet hosts through
  QEMU's NAT, so the host must be online.
* **GUI and recovery** (`cargo xtask test --ui`): `tests/ui/wifi-connect.vts`
  joins networks through the flyout (a wrong password first), Settings and
  the Terminal; `tests/ui/drivervm-wifi-recovery.vts` breaks the network
  through airsim — access point gone, disconnection, wired side down, DNS
  failure, out of range, the medium going silent, rekeying and loss, the
  Wi-Fi service killed — and checks that Veda recovers by itself each
  time; `tests/ui/network-failover.vts` unplugs and replugs the wired
  card's cable (QMP `set_link`) with Wi-Fi connected and checks that
  traffic moves between the interfaces; `tests/ui/drivervm-net.vts` and
  `e1000e.vts` run Linux's drivers of QEMU's Intel 82576 and 82574L, and
  `drivervm-usb.vts` of its USB network adapter.

## Limitations and next steps

* Wi-Fi hardware is Linux's, through the driver VM: Intel's AX211 has its
  firmware so far (`ports/linux/firmware.txt`); other cards need their
  driver and firmware added. 6 GHz channels, and telling Linux the
  country (it keeps to the world's rules, listening first on channels
  that need it), are next.
* WPA2/WPA3-Enterprise (802.1X/EAP), Enhanced Open (OWE), fast roaming
  (802.11r), power saving, 802.11n/ac rate control.
* TLS: no session resumption (each connection is a full handshake), no
  client certificates, no revocation checking (OCSP, CRLs), no Encrypted
  Client Hello, and no post-quantum key exchange yet (X25519MLKEM768, which
  RustCrypto's `ml-kem` 0.3 could provide as a hybrid group). The trusted
  roots are compiled in: updating them needs a new build, and there is no
  system certificate store to add one to (`ClientConfigBuilder` can, per
  application). Certificate checks depend on the firmware clock being
  right.
* An HTTPS client in `vnet::http` (applications speak HTTP over a
  `vtls::TlsStream` themselves for now).
* Encrypting the saved passphrases at rest.
