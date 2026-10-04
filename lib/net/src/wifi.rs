//! Wi-Fi for applications: the networks in range, joining and leaving
//! them, saved networks, the radio switch, status events and diagnostics
//! (through the Wi-Fi service's `wlan` protocol).
//!
//! ```ignore
//! vnet::wifi::scan()?;
//! for n in vnet::wifi::networks()? {
//!     println!("{} {} {} bars", n.name, n.security.label(), n.bars);
//! }
//! vnet::wifi::connect("Veda Home", Some("password"), true)?;
//! ```
//!
//! Joining happens in the background: `connect` returns once the attempt
//! has started, and its progress shows in [`status`] and in the events of a
//! [`Watcher`]. Once joined, the network service configures the `wlan0`
//! interface (DHCP and so on) and the usual sockets work over it.

use alloc::string::String;
use alloc::vec::Vec;

pub use vproto::wlan::{
    Band, BssInfo, ConnState, FailReason, LogEntry, NetworkInfo, SavedNetwork, Security, WlanCounters, WlanDiagnostics,
    WlanError, WlanEvent, WlanStatus, signal_bars, ssid_display,
};

use vipc::{Bytes, IpcError};
use vproto::wlan::{ConnectRequest, WLAN_EVENT, wlan};
use vrt::object::Channel;
use vrt::sync::Mutex;

use crate::Duration;

static CLIENT: Mutex<Option<wlan::Client>> = Mutex::new(None);

/// Errors of the Wi-Fi API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiError {
    /// The Wi-Fi service is not running.
    Unavailable,
    /// The service refused the request.
    Refused(WlanError),
}

impl core::fmt::Display for WifiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WifiError::Unavailable => f.write_str("the Wi-Fi service is not running"),
            WifiError::Refused(e) => write!(f, "{e}"),
        }
    }
}

/// Whether the Wi-Fi service is running.
pub fn available() -> bool {
    vproto::with_registry(|r| r.list().map(|l| l.iter().any(|n| n == wlan::NAME)).unwrap_or(false)).unwrap_or(false)
}

fn with_client<R>(mut f: impl FnMut(&wlan::Client) -> Result<R, IpcError>) -> Result<R, WifiError> {
    let mut slot = CLIENT.lock();
    for _ in 0..2 {
        if slot.is_none() {
            if !available() {
                return Err(WifiError::Unavailable);
            }
            let ch = vproto::connect(wlan::NAME).map_err(|_| WifiError::Unavailable)?;
            let c = wlan::Client::new(ch);
            // The service answers quickly; do not hang if it is stuck.
            c.set_timeout(10_000_000_000);
            *slot = Some(c);
        }
        match f(slot.as_ref().unwrap()) {
            Ok(r) => return Ok(r),
            Err(_) => *slot = None,
        }
    }
    Err(WifiError::Unavailable)
}

fn flat<T>(r: Result<Result<T, WlanError>, WifiError>) -> Result<T, WifiError> {
    r?.map_err(WifiError::Refused)
}

pub fn status() -> Result<WlanStatus, WifiError> {
    with_client(|c| c.status())
}

/// Starts a scan; [`WlanEvent::ScanDone`] announces the result.
pub fn scan() -> Result<(), WifiError> {
    flat(with_client(|c| c.scan()))
}

/// Networks in range, strongest first (the connected one first).
pub fn networks() -> Result<Vec<NetworkInfo>, WifiError> {
    with_client(|c| c.networks())
}

/// Every access point heard recently.
pub fn access_points() -> Result<Vec<BssInfo>, WifiError> {
    with_client(|c| c.access_points())
}

/// Options of [`connect_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Required security (`None`: what the network offers).
    pub security: Option<Security>,
    /// Remember the network and its password.
    pub save: bool,
    /// Join it automatically when in range (saved networks).
    pub auto_connect: bool,
    /// The network does not broadcast its name.
    pub hidden: bool,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions { security: None, save: true, auto_connect: true, hidden: false }
    }
}

/// Starts joining `ssid`. `password` may be `None` for open networks and
/// for saved networks (the saved password is used).
pub fn connect(ssid: &str, password: Option<&str>, save: bool) -> Result<(), WifiError> {
    connect_with(ssid.as_bytes(), password, &ConnectOptions { save, auto_connect: save, ..Default::default() })
}

pub fn connect_with(ssid: &[u8], password: Option<&str>, o: &ConnectOptions) -> Result<(), WifiError> {
    let r = ConnectRequest {
        ssid: Bytes(ssid.to_vec()),
        security: o.security,
        passphrase: password.map(String::from),
        save: o.save,
        auto_connect: o.auto_connect,
        hidden: o.hidden,
    };
    flat(with_client(|c| c.connect(r.clone())))
}

/// Leaves the network; nothing is joined automatically until the next
/// [`connect`] (or a restart).
pub fn disconnect() -> Result<(), WifiError> {
    flat(with_client(|c| c.disconnect()))
}

pub fn saved() -> Result<Vec<SavedNetwork>, WifiError> {
    with_client(|c| c.saved())
}

pub fn forget(ssid: &[u8]) -> Result<(), WifiError> {
    flat(with_client(|c| c.forget(Bytes(ssid.to_vec()))))
}

pub fn set_auto_connect(ssid: &[u8], enabled: bool) -> Result<(), WifiError> {
    flat(with_client(|c| c.set_auto_connect(Bytes(ssid.to_vec()), enabled)))
}

/// Turns the Wi-Fi radio on or off.
pub fn set_radio(on: bool) -> Result<(), WifiError> {
    flat(with_client(|c| c.set_radio(on)))
}

pub fn diagnostics() -> Result<WlanDiagnostics, WifiError> {
    with_client(|c| c.diagnostics())
}

/// Waits until [`status`] satisfies `done` or `timeout` passes; returns
/// the last status.
pub fn wait_for(timeout: Duration, mut done: impl FnMut(&WlanStatus) -> bool) -> Result<WlanStatus, WifiError> {
    let end = vrt::time::deadline_after(timeout);
    loop {
        let s = status()?;
        if done(&s) || vrt::time::now_ns() >= end {
            return Ok(s);
        }
        vrt::time::sleep(Duration::from_millis(100));
    }
}

/// A channel of Wi-Fi events (status changes, finished scans, failed
/// connection attempts). The current status arrives first.
pub struct Watcher {
    channel: Channel,
}

impl Watcher {
    pub fn new() -> Result<Watcher, WifiError> {
        let (mine, theirs) = Channel::create().map_err(|_| WifiError::Unavailable)?;
        let mut theirs = Some(theirs);
        flat(with_client(|c| match theirs.take() {
            Some(ch) => c.watch(ch),
            None => Ok(Err(WlanError::Busy)),
        }))?;
        Ok(Watcher { channel: mine })
    }

    /// The channel to wait on (readable when an event is pending).
    pub fn handle(&self) -> &Channel {
        &self.channel
    }

    /// The next pending event, without blocking. `Err` means the service
    /// went away (create a new watcher once it is back).
    pub fn try_next(&self) -> Result<Option<WlanEvent>, WifiError> {
        match self.channel.read() {
            Ok(msg) => match vipc::decode_event::<WlanEvent>(msg) {
                Ok((WLAN_EVENT, ev)) => Ok(Some(ev)),
                _ => Ok(None),
            },
            Err(vabi::Error::ShouldWait) => Ok(None),
            Err(_) => Err(WifiError::Unavailable),
        }
    }

    /// Waits up to `timeout` for the next event.
    pub fn next(&self, timeout: Duration) -> Result<Option<WlanEvent>, WifiError> {
        let deadline = vrt::time::deadline_after(timeout);
        loop {
            if let Some(ev) = self.try_next()? {
                return Ok(Some(ev));
            }
            match self.channel.wait(vabi::signals::READABLE | vabi::signals::PEER_CLOSED, deadline) {
                Ok(s) if s & vabi::signals::READABLE != 0 => continue,
                Ok(_) => return Err(WifiError::Unavailable),
                Err(_) => return Ok(None),
            }
        }
    }
}
