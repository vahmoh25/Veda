//! The Wi-Fi protocols served by the `wlan` service.
//!
//! * [`wlan`] (service `"wlan"`): Wi-Fi management for the desktop, Settings
//!   and the Terminal — scanning, the list of networks, connecting and
//!   disconnecting, saved networks, the radio switch and diagnostics.
//! * [`wlanphy`] (service `"wlanphy"`): a radio driver offers its radio to
//!   the service. The driver only moves raw 802.11 frames (through a
//!   [`crate::netring::Link`], slot kind
//!   [`crate::netring::kind::IEEE80211`]) and tunes the radio; scanning,
//!   authentication (SAE), association, key handshakes, encryption and
//!   roaming all happen in the service ("soft MAC"), so a new radio needs
//!   only a small driver.
//!
//!   A *managed* radio ([`phy_caps::MANAGED`]) joins access points itself,
//!   as Linux's mac80211 drivers do (its firmware keeps the MAC: what it
//!   receives, acknowledges and encrypts): the service gives it commands
//!   instead of frames — scan, authenticate, associate, leave — while
//!   still deciding what to join and authenticating itself: SAE's commit
//!   and confirm, the key handshakes and the keys' derivation stay in the
//!   service, and the radio gets only the session keys. Its link carries
//!   Ethernet frames, and the frames of the access point's the service
//!   needs (authentication and association responses, deauthentications
//!   and disassociations, scan results as probe responses, SA Query
//!   actions); it reports scans' ends, timeouts and a lost link as
//!   [`MlmeEvent`]s.
//!
//! The service presents each connected radio to `netd` as an Ethernet-like
//! interface (`wlan0`) through the `netdev` protocol.

use alloc::string::String;
use alloc::vec::Vec;

use vipc::{Bytes, enumeration, message, protocol, union};
use vrt::object::Channel;

use crate::netring::LinkEndpoints;

/// Longest SSID (IEEE 802.11-2020, 9.4.2.2).
pub const MAX_SSID_LEN: usize = 32;
/// Shortest and longest WPA passphrase (8..63 characters; 64 hex digits
/// give the PSK directly).
pub const MIN_PASSPHRASE_LEN: usize = 8;
pub const MAX_PASSPHRASE_LEN: usize = 64;

enumeration! {
    /// Frequency bands.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum Band {
        Ghz2 = 1,
        Ghz5 = 2,
        Ghz6 = 3,
    }
}

/// Bits of [`ChannelInfo::flags`].
pub mod channel_flags {
    /// Passive scanning only: the radio may not transmit until it hears an
    /// access point (radar channels, regulatory "no initiating radiation").
    pub const NO_IR: u32 = 1;
    /// Radar detection required.
    pub const DFS: u32 = 2;
    /// Not usable in the current regulatory domain.
    pub const DISABLED: u32 = 4;
}

message! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChannelInfo {
        pub band: Band,
        pub number: u8,
        pub freq_mhz: u16,
        /// [`channel_flags`].
        pub flags: u32,
        pub max_power_dbm: i8,
    }
}

/// Bits of [`PhyInfo::caps`].
pub mod phy_caps {
    /// The radio encrypts and decrypts CCMP itself (keys can be installed).
    pub const HW_CRYPTO: u32 = 1;
    /// The radio reports transmit status (acknowledged or not).
    pub const TX_STATUS: u32 = 2;
    /// The radio supports 802.11n (HT).
    pub const HT: u32 = 4;
    /// The radio joins access points itself, on the service's commands: its
    /// control channel speaks [`super::wlanmlme_ctl`].
    pub const MANAGED: u32 = 8;
}

message! {
    /// A radio offered to the Wi-Fi service.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PhyInfo {
        /// Driver name, e.g. `iwlwifi (Linux)`.
        pub driver: String,
        /// Where the radio is, e.g. `pci 00:14.3`.
        pub location: String,
        /// The radio's permanent MAC address.
        pub mac: [u8; 6],
        pub channels: Vec<ChannelInfo>,
        /// [`phy_caps`].
        pub caps: u32,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PhyError {
        /// The radio is not ready (powered off, hardware gone).
        NotReady = 1,
        /// The channel is not supported.
        BadChannel = 2,
        NotSupported = 3,
        Io = 4,
    }
}

enumeration! {
    /// What a radio driver reports about its hardware.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RadioState {
        /// Working.
        Ready = 1,
        /// Temporarily unusable (for example the virtual radio's host side
        /// disconnected); frames are dropped until it is `Ready` again.
        Unavailable = 2,
    }
}

message! {
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct PhyStats {
        pub tx_frames: u64,
        pub tx_failed: u64,
        pub rx_frames: u64,
        pub rx_dropped: u64,
    }
}

/// Event ordinal on a [`wlanphy`] channel (driver to service, after
/// `attach`): the hardware became usable or unusable; the payload is a
/// [`RadioState`]. It is a one-way event rather than a call so that a driver
/// reporting its state can never deadlock against the service calling it
/// through [`wlanphy_ctl`].
pub const RADIO_STATE_EVENT: u32 = 1;
/// Event ordinal on a [`wlanphy`] channel: what a managed radio reports
/// ([`MlmeEvent`]).
pub const MLME_EVENT: u32 = 2;

union! {
    /// What a managed radio reports, besides the frames on its link.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum MlmeEvent {
        /// A scan ended (its results came before, as probe responses).
        1 => ScanDone {},
        /// The access point did not answer an authentication or an
        /// association.
        2 => Timeout {},
        /// The access point stopped being heard: the radio left it.
        3 => LinkLost {},
        /// The signal of the access point joined (dBm).
        4 => Signal { dbm: i8 },
        /// An unprotected deauthentication or disassociation came while
        /// management frames are protected: maybe a forgery (an SA Query
        /// tells), so the radio stays.
        5 => UnprotectedDeauth { reason: u16 },
    }
}

enumeration! {
    /// The keys a managed radio is given.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum KeyKind {
        /// The pairwise key (CCMP-128), index 0.
        Pairwise = 1,
        /// A group key (CCMP-128), index 1 to 3.
        Group = 2,
        /// The integrity group key (BIP-CMAC-128), index 4 or 5.
        Integrity = 3,
    }
}

protocol! {
    /// Radio drivers offering a radio to the Wi-Fi service. One radio per
    /// channel; closing it removes the radio.
    pub mod wlanphy = "wlanphy" {
        /// Offers a radio. `control` is served by the driver with the
        /// [`wlanphy_ctl`] protocol.
        1 => fn attach(info: PhyInfo, link: LinkEndpoints, control: Channel, state: RadioState) -> Result<(), PhyError>;
    }
}

protocol! {
    /// Commands from the Wi-Fi service to a radio driver.
    pub mod wlanphy_ctl = "wlanphy-ctl" {
        /// Tunes the radio. Frames arriving on other channels are no longer
        /// delivered.
        1 => fn set_channel(band: Band, number: u8) -> Result<(), PhyError>;
        /// Switches the transmitter and receiver on or off.
        2 => fn set_power(on: bool) -> Result<(), PhyError>;
        3 => fn stats() -> PhyStats;
    }
}

protocol! {
    /// Commands from the Wi-Fi service to a managed radio
    /// ([`phy_caps::MANAGED`]), which serves this protocol on its control
    /// channel instead of [`wlanphy_ctl`].
    pub mod wlanmlme_ctl = "wlanmlme-ctl" {
        /// Switches the transmitter and receiver on or off.
        1 => fn set_power(on: bool) -> Result<(), PhyError>;
        2 => fn stats() -> PhyStats;
        /// Scans every channel, probing for `ssids` (and for every network)
        /// where it may. The access points come as probe responses on the
        /// link, then [`MlmeEvent::ScanDone`].
        3 => fn scan(ssids: Vec<Bytes>) -> Result<(), PhyError>;
        /// Sends `bssid` (`ssid`, on `channel`) an Authentication frame
        /// with `body` (the algorithm, the transaction, the status, then
        /// the algorithm's data: SAE's commit or confirm). The answer comes
        /// as an Authentication frame, or [`MlmeEvent::Timeout`].
        4 => fn authenticate(bssid: [u8; 6], band: Band, channel: u8, ssid: Bytes, body: Bytes) -> Result<(), PhyError>;
        /// Associates with `bssid`, authenticated, with `ies` (a protected
        /// network's RSN and RSNX elements) and management frame
        /// protection if `pmf`. The answer comes as an Association Response
        /// frame, or [`MlmeEvent::Timeout`].
        5 => fn associate(bssid: [u8; 6], band: Band, channel: u8, ssid: Bytes, ies: Bytes, pmf: bool) -> Result<(), PhyError>;
        /// Leaves `bssid` (a deauthentication, `reason`).
        6 => fn deauthenticate(bssid: [u8; 6], reason: u16) -> Result<(), PhyError>;
        /// Sends `peer` an EAPOL frame, encrypted with the pairwise key if
        /// `encrypt` (in clear before that is in place). A call, so that
        /// it goes before the key that follows it.
        7 => fn send_eapol(peer: [u8; 6], frame: Bytes, encrypt: bool) -> Result<(), PhyError>;
        /// Installs a key of `kind` at `index` for the connection to
        /// `peer`; frames are accepted with packet numbers above `rsc`.
        8 => fn install_key(kind: KeyKind, index: u8, key: Bytes, rsc: u64, peer: [u8; 6]) -> Result<(), PhyError>;
        /// Lets data other than EAPOL through to and from `peer` (the
        /// handshake succeeded).
        9 => fn authorize(peer: [u8; 6]) -> Result<(), PhyError>;
        /// Sends a management frame (an SA Query action) as built; the
        /// radio protects it when it must.
        10 => fn send_management(frame: Bytes) -> Result<(), PhyError>;
    }
}

enumeration! {
    /// The security of a network.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum Security {
        /// No encryption.
        Open = 1,
        /// WEP: obsolete and broken; listed but never connected to.
        Wep = 2,
        /// WPA2-Personal (pre-shared key, CCMP).
        Wpa2Personal = 3,
        /// WPA3-Personal (SAE, CCMP, management frame protection).
        Wpa3Personal = 4,
        /// WPA2/WPA3 transition mode: WPA3 is used.
        Wpa2Wpa3Personal = 5,
        /// WPA/WPA2/WPA3-Enterprise (802.1X): not supported yet.
        Enterprise = 6,
        /// Enhanced Open (OWE): not supported yet.
        Owe = 7,
        /// WPA (TKIP) only: obsolete; never connected to.
        WpaTkip = 8,
    }
}

impl Security {
    /// Whether Veda can connect to networks with this security.
    pub const fn supported(self) -> bool {
        matches!(self, Security::Open | Security::Wpa2Personal | Security::Wpa3Personal | Security::Wpa2Wpa3Personal)
    }

    /// Whether a password is needed.
    pub const fn needs_password(self) -> bool {
        !matches!(self, Security::Open | Security::Owe)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Security::Open => "Open",
            Security::Wep => "WEP (unsupported)",
            Security::Wpa2Personal => "WPA2-Personal",
            Security::Wpa3Personal => "WPA3-Personal",
            Security::Wpa2Wpa3Personal => "WPA2/WPA3-Personal",
            Security::Enterprise => "Enterprise (unsupported)",
            Security::Owe => "Enhanced Open (unsupported)",
            Security::WpaTkip => "WPA-TKIP (unsupported)",
        }
    }
}

message! {
    /// One access point heard during scanning.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BssInfo {
        /// The SSID as sent (up to 32 arbitrary bytes; empty if hidden).
        pub ssid: Bytes,
        pub bssid: [u8; 6],
        pub band: Band,
        pub channel: u8,
        pub signal_dbm: i8,
        pub security: Security,
        /// Management frame protection: 0 = no, 1 = capable, 2 = required.
        pub pmf: u8,
        /// Milliseconds since it was last heard.
        pub age_ms: u32,
    }
}

message! {
    /// A network (all access points with one SSID and security), as listed
    /// in the desktop's Wi-Fi menu.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct NetworkInfo {
        pub ssid: Bytes,
        /// The SSID for display (invalid UTF-8 escaped).
        pub name: String,
        pub security: Security,
        /// Strongest signal among its access points.
        pub signal_dbm: i8,
        /// Signal as 0-4 bars.
        pub bars: u8,
        pub saved: bool,
        pub connected: bool,
        pub access_points: u32,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ConnState {
        /// No Wi-Fi radio.
        NoAdapter = 1,
        /// The radio is switched off.
        RadioOff = 2,
        Disconnected = 3,
        /// Authenticating (SAE or open system).
        Authenticating = 4,
        Associating = 5,
        /// Exchanging keys (the 4-way handshake).
        Securing = 6,
        Connected = 7,
    }
}

enumeration! {
    /// Why a connection attempt failed or a connection ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FailReason {
        /// The password is wrong (handshake failed).
        WrongPassword = 1,
        /// The network is out of range.
        NotFound = 2,
        /// The access point refused authentication.
        AuthRejected = 3,
        /// The access point refused association (full, incompatible).
        AssocRejected = 4,
        /// The access point stopped answering.
        Timeout = 5,
        /// The network uses unsupported security.
        Unsupported = 6,
        /// The access point disconnected us.
        Disconnected = 7,
        /// The signal was lost.
        SignalLost = 8,
        /// The security handshake failed for another reason (or the access
        /// point's security information changed: a possible attack).
        HandshakeFailed = 9,
        /// The radio stopped working.
        RadioFailure = 10,
        /// A password is required.
        PasswordRequired = 11,
    }
}

impl FailReason {
    pub const fn description(self) -> &'static str {
        match self {
            FailReason::WrongPassword => "the password is incorrect",
            FailReason::NotFound => "the network is out of range",
            FailReason::AuthRejected => "the access point refused the connection",
            FailReason::AssocRejected => "the access point is not accepting connections",
            FailReason::Timeout => "the access point stopped responding",
            FailReason::Unsupported => "the network's security is not supported",
            FailReason::Disconnected => "the access point disconnected",
            FailReason::SignalLost => "the signal was lost",
            FailReason::HandshakeFailed => "the security handshake failed",
            FailReason::RadioFailure => "the Wi-Fi adapter stopped working",
            FailReason::PasswordRequired => "a password is required",
        }
    }
}

impl core::fmt::Display for FailReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.description())
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WlanStatus {
        pub state: ConnState,
        /// The radio's driver and MAC address ("" / zeros without one).
        pub adapter: String,
        pub mac: [u8; 6],
        /// The network being joined or joined.
        pub ssid: Bytes,
        pub name: String,
        pub bssid: [u8; 6],
        pub band: Band,
        pub channel: u8,
        pub signal_dbm: i8,
        pub security: Security,
        /// The last connection failure (cleared on success).
        pub last_failure: Option<FailReason>,
        /// Seconds connected.
        pub connected_s: u32,
        /// A scan is in progress.
        pub scanning: bool,
    }
}

message! {
    /// A connection request from the user.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConnectRequest {
        pub ssid: Bytes,
        /// The security to use; `None` picks it from the scan results (or the
        /// saved network).
        pub security: Option<Security>,
        /// The password (`None` for open networks, or to use the saved one).
        pub passphrase: Option<String>,
        /// Remember the network (and the password) for next time.
        pub save: bool,
        /// Join it automatically when in range.
        pub auto_connect: bool,
        /// The network does not broadcast its SSID: probe for it by name.
        pub hidden: bool,
    }
}

message! {
    /// A remembered network (the password is never returned).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SavedNetwork {
        pub ssid: Bytes,
        pub name: String,
        pub security: Security,
        pub auto_connect: bool,
        pub hidden: bool,
        /// Unix time of the last successful connection (0 = never).
        pub last_connected: u64,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WlanError {
        NoAdapter = 1,
        RadioOff = 2,
        /// The SSID is empty or longer than 32 bytes.
        BadSsid = 3,
        /// The password does not meet the rules (8-63 characters or 64 hex
        /// digits).
        BadPassphrase = 4,
        PasswordRequired = 5,
        /// The network's security is not supported.
        Unsupported = 6,
        NotFound = 7,
        Busy = 8,
        Storage = 9,
        LimitReached = 10,
    }
}

impl core::fmt::Display for WlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            WlanError::NoAdapter => "no Wi-Fi adapter",
            WlanError::RadioOff => "Wi-Fi is turned off",
            WlanError::BadSsid => "invalid network name",
            WlanError::BadPassphrase => "the password must be 8 to 63 characters (or 64 hex digits)",
            WlanError::PasswordRequired => "a password is required",
            WlanError::Unsupported => "the network's security is not supported",
            WlanError::NotFound => "no such network",
            WlanError::Busy => "busy",
            WlanError::Storage => "could not save the network",
            WlanError::LimitReached => "too many saved networks",
        })
    }
}

message! {
    /// One line of the connection log shown by diagnostics.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LogEntry {
        /// Milliseconds since boot.
        pub time_ms: u64,
        pub message: String,
    }
}

message! {
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct WlanCounters {
        pub scans: u64,
        pub connect_attempts: u64,
        pub connections: u64,
        pub disconnections: u64,
        pub auth_failures: u64,
        pub beacons_rx: u64,
        pub beacon_losses: u64,
        pub data_tx: u64,
        pub data_rx: u64,
        /// Frames that failed decryption or integrity checks.
        pub decrypt_errors: u64,
        /// Replayed frames dropped.
        pub replays: u64,
        /// Unprotected management frames ignored because the connection uses
        /// management frame protection.
        pub unprotected_dropped: u64,
        pub group_rekeys: u64,
        pub roams: u64,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WlanDiagnostics {
        pub status: WlanStatus,
        pub counters: WlanCounters,
        pub log: Vec<LogEntry>,
    }
}

union! {
    /// Notifications on a [`wlan::Client::watch`] channel.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum WlanEvent {
        1 => StatusChanged { status: WlanStatus },
        /// A scan finished; fetch the list with [`wlan::Client::networks`].
        2 => ScanDone {},
        /// A connection attempt failed (also reflected in the status).
        3 => ConnectFailed { ssid: Bytes, name: String, reason: FailReason },
    }
}

/// Event ordinal of [`WlanEvent`]s on a watch channel.
pub const WLAN_EVENT: u32 = 1;

protocol! {
    /// Wi-Fi management.
    pub mod wlan = "wlan" {
        1 => fn status() -> WlanStatus;
        /// Starts a scan (the result is announced with
        /// [`WlanEvent::ScanDone`]).
        2 => fn scan() -> Result<(), WlanError>;
        /// Networks in range, strongest first.
        3 => fn networks() -> Vec<NetworkInfo>;
        /// Every access point heard recently.
        4 => fn access_points() -> Vec<BssInfo>;
        /// Starts connecting; progress and the outcome are reported through
        /// the status and [`WlanEvent`]s.
        5 => fn connect(request: ConnectRequest) -> Result<(), WlanError>;
        /// Disconnects and stops joining networks automatically until the
        /// next `connect` (or restart).
        6 => fn disconnect() -> Result<(), WlanError>;
        7 => fn saved() -> Vec<SavedNetwork>;
        8 => fn forget(ssid: Bytes) -> Result<(), WlanError>;
        9 => fn set_auto_connect(ssid: Bytes, enabled: bool) -> Result<(), WlanError>;
        10 => fn set_radio(on: bool) -> Result<(), WlanError>;
        /// Sends [`WlanEvent`]s (ordinal [`WLAN_EVENT`]) on `events` until it
        /// is closed.
        11 => fn watch(events: Channel) -> Result<(), WlanError>;
        12 => fn diagnostics() -> WlanDiagnostics;
    }
}

/// Formats an SSID for display: valid UTF-8 as is (control characters
/// escaped), other bytes as `\xNN`.
pub fn ssid_display(ssid: &[u8]) -> String {
    use core::fmt::Write;
    let mut out = String::new();
    let mut rest = ssid;
    while !rest.is_empty() {
        match core::str::from_utf8(rest) {
            Ok(s) => {
                push_escaped(&mut out, s);
                break;
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                // SAFETY-free: `good` is valid UTF-8 by construction.
                push_escaped(&mut out, core::str::from_utf8(good).unwrap_or(""));
                let n = e.error_len().unwrap_or(bad.len()).max(1);
                for b in &bad[..n] {
                    let _ = write!(out, "\\x{b:02x}");
                }
                rest = &bad[n..];
            }
        }
    }
    out
}

fn push_escaped(out: &mut String, s: &str) {
    use core::fmt::Write;
    for c in s.chars() {
        if c.is_control() {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
    }
}

/// Signal strength as 0-4 bars.
pub fn signal_bars(dbm: i8) -> u8 {
    match dbm {
        d if d >= -55 => 4,
        d if d >= -67 => 3,
        d if d >= -75 => 2,
        d if d >= -85 => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use vipc::{Decode, Decoder, Encode, Encoder};

    #[test]
    fn ssids_display_safely() {
        assert_eq!(ssid_display(b"Cafe WiFi"), "Cafe WiFi");
        assert_eq!(ssid_display("Café ☕".as_bytes()), "Café ☕");
        assert_eq!(ssid_display(b"bad\xff\xfename"), "bad\\xff\\xfename");
        assert_eq!(ssid_display(b"tab\there"), "tab\\u{9}here");
        assert_eq!(ssid_display(b""), "");
    }

    #[test]
    fn bars() {
        assert_eq!(signal_bars(-40), 4);
        assert_eq!(signal_bars(-60), 3);
        assert_eq!(signal_bars(-70), 2);
        assert_eq!(signal_bars(-80), 1);
        assert_eq!(signal_bars(-95), 0);
    }

    #[test]
    fn requests_round_trip() {
        let r = ConnectRequest {
            ssid: Bytes(b"VedaNet".to_vec()),
            security: Some(Security::Wpa3Personal),
            passphrase: Some("correct horse".into()),
            save: true,
            auto_connect: true,
            hidden: false,
        };
        let mut e = Encoder::new();
        r.clone().encode(&mut e);
        let mut d = Decoder::new(&e.bytes, vec![]);
        assert_eq!(ConnectRequest::decode(&mut d).unwrap(), r);
        assert!(Security::Wpa2Wpa3Personal.supported());
        assert!(!Security::Wep.supported());
    }
}
