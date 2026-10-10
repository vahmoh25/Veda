//! `airlink` — the driver VM's radio medium, for the tests of Wi-Fi: QEMU's
//! virtual Wi-Fi radio (a virtio-serial port named `org.veda.wlan.0`, whose
//! other end is `airsim` on the host, which simulates the access points) as
//! a Wi-Fi radio of Linux's own, one of `mac80211_hwsim`'s, which `wifi`
//! drives as it drives a real one.
//!
//! airlink makes the radio, with the address airsim gives, and is its medium
//! (hwsim's netlink protocol, wmediumd's): what the radio transmits goes to
//! airsim on the frame's channel, and its transmit status comes back; what
//! the access points send comes to the radio with its channel, and the radio
//! keeps what is on its own (airsim's radio listens to every channel). The
//! radio goes when airlink does. Without the port (not under QEMU) it ends.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use guest_netlink::{Builder, Family, NETLINK_GENERIC, Socket};
use guest_sys::{POLLERR, POLLHUP, POLLIN, POLLOUT, PollFd, poll_many};
use vradiolink::{LinkError, Message, Reader, VERSION, msg};

/// The port of the radio link, and how long to wait for Linux to find it.
const PORT_NAME: &str = "org.veda.wlan.0";
const PORT_WAIT: Duration = Duration::from_secs(10);
/// How often to greet airsim again until it answers.
const HELLO_EVERY: Duration = Duration::from_secs(1);
/// The most bytes waiting for the port, and frames waiting for their
/// transmit status: beyond, frames are dropped, as a busy medium drops.
const MAX_QUEUED: usize = 1 << 20;
const MAX_IN_FLIGHT: usize = 1024;
const O_NONBLOCK: i32 = 0o4000;
/// The rates a frame's transmit status has (`IEEE80211_TX_MAX_RATES`).
const TX_RATES: usize = 4;

/// hwsim's commands and attributes (`mac80211_hwsim.h`).
mod hwsim {
    pub const CMD_REGISTER: u8 = 1;
    pub const CMD_FRAME: u8 = 2;
    pub const CMD_TX_INFO_FRAME: u8 = 3;
    pub const CMD_NEW_RADIO: u8 = 4;
    pub const ADDR_RECEIVER: u16 = 1;
    pub const ADDR_TRANSMITTER: u16 = 2;
    pub const FRAME: u16 = 3;
    pub const FLAGS: u16 = 4;
    pub const RX_RATE: u16 = 5;
    pub const SIGNAL: u16 = 6;
    pub const TX_INFO: u16 = 7;
    pub const COOKIE: u16 = 8;
    pub const DESTROY_RADIO_ON_CLOSE: u16 = 16;
    pub const FREQ: u16 = 19;
    pub const PERM_ADDR: u16 = 22;
    /// Flags of a frame.
    pub const TX_CTL_NO_ACK: u32 = 2;
    pub const TX_STAT_ACK: u32 = 4;
}

fn channel_of(freq: u32) -> Option<u8> {
    match freq {
        2484 => Some(14),
        2412..=2472 => Some(((freq - 2407) / 5) as u8),
        5160..=5885 => Some(((freq - 5000) / 5) as u8),
        _ => None,
    }
}

fn freq_of(channel: u8) -> u32 {
    match channel {
        14 => 2484,
        1..=13 => 2407 + 5 * channel as u32,
        _ => 5000 + 5 * channel as u32,
    }
}

/// The port's device (`/dev/vport0p1`), once Linux has found it.
fn find_port() -> Option<String> {
    let ports = std::fs::read_dir("/sys/class/virtio-ports").ok()?;
    ports.flatten().find_map(|p| {
        let name = std::fs::read_to_string(p.path().join("name")).ok()?;
        (name.trim() == PORT_NAME).then(|| format!("/dev/{}", p.file_name().to_string_lossy()))
    })
}

/// A frame the radio sent, waiting for its transmit status.
struct InFlight {
    cookie: u64,
    flags: u32,
    tx_info: Vec<u8>,
}

struct Airlink {
    port: File,
    reader: Reader,
    /// Bytes for the port it had no room for yet.
    out: VecDeque<u8>,
    hwsim: Socket,
    family: u16,
    /// The radio's address (airsim's), once made, and airsim's channels.
    mac: Option<[u8; 6]>,
    channels: Vec<u8>,
    greeted: bool,
    hello_at: Instant,
    /// The channel airsim's radio is on.
    tuned: Option<u8>,
    in_flight: BTreeMap<u32, InFlight>,
    next_id: u32,
}

impl Airlink {
    fn send(&mut self, m: &Message) {
        let bytes = m.encode();
        if self.out.len() + bytes.len() <= MAX_QUEUED {
            self.out.extend(bytes);
        }
    }

    /// Writes what waits for the port, while it takes it.
    fn write_out(&mut self) {
        while !self.out.is_empty() {
            let (chunk, _) = self.out.as_slices();
            match self.port.write(chunk) {
                Ok(0) => break,
                Ok(n) => drop(self.out.drain(..n)),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    fn hello(&mut self) {
        self.greeted = false;
        self.reader.resync(msg::HELLO_ACK);
        self.hello_at = Instant::now();
        let mac = self.mac.unwrap_or([0; 6]);
        self.send(&Message::Hello { version: VERSION, mac });
    }

    /// airsim lost: frames on their way are lost too.
    fn disconnected(&mut self) {
        if self.greeted {
            println!("airlink: airsim went away");
        }
        self.greeted = false;
        self.tuned = None;
        self.out.clear();
        self.reader.reset();
        for (_, f) in std::mem::take(&mut self.in_flight) {
            self.tx_status(f, false);
        }
    }

    /// Makes the radio, with airsim's address, and becomes its medium.
    fn make_radio(&mut self, mac: [u8; 6]) -> io::Result<()> {
        let radio = Builder::genl(self.family, 0, hwsim::CMD_NEW_RADIO, 1)
            .attr(hwsim::PERM_ADDR, &mac)
            .flag(hwsim::DESTROY_RADIO_ON_CLOSE);
        self.hwsim.request(radio)?;
        self.hwsim.request(Builder::genl(self.family, 0, hwsim::CMD_REGISTER, 1))?;
        self.mac = Some(mac);
        Ok(())
    }

    fn port_message(&mut self, m: Message) {
        match m {
            Message::HelloAck { version, mac, channels } => {
                if version != VERSION {
                    println!("airlink: airsim speaks radio link version {version}, we speak {VERSION}");
                }
                if self.mac.is_none() {
                    if let Err(e) = self.make_radio(mac) {
                        println!("airlink: cannot make the radio: {e}");
                        return;
                    }
                    let m = mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
                    println!("airlink: radio {m} on airsim's channels {channels:?}");
                }
                self.channels = channels;
                self.greeted = true;
                self.tuned = None;
                // On, hearing every channel: the radio keeps its own.
                self.send(&Message::SetPower { on: true });
                self.send(&Message::Listen { all: true });
            }
            Message::Rx { channel, signal_dbm, frame } => {
                let Some(mac) = self.mac else { return };
                let to_radio = Builder::genl(self.family, 0, hwsim::CMD_FRAME, 1)
                    .attr(hwsim::ADDR_RECEIVER, &mac)
                    .attr(hwsim::FRAME, &frame)
                    .u32(hwsim::RX_RATE, 0)
                    .u32(hwsim::SIGNAL, signal_dbm as i32 as u32)
                    .u32(hwsim::FREQ, freq_of(channel));
                // A frame off the radio's channel is refused: not heard.
                let _ = self.hwsim.send(to_radio);
            }
            Message::TxStatus { id, acked } => {
                if let Some(f) = self.in_flight.remove(&id) {
                    self.tx_status(f, acked);
                }
            }
            other => println!("airlink: unexpected message from airsim: {other:?}"),
        }
    }

    /// Tells the radio how a frame went: acknowledged at the first try, or
    /// not at all.
    fn tx_status(&mut self, f: InFlight, acked: bool) {
        let Some(mac) = self.mac else { return };
        // The `hwsim_tx_rate`s the radio tried (a rate's index and its
        // tries; -1 ends them), all four of them.
        let mut tx_info = f.tx_info;
        tx_info.truncate(TX_RATES * 2);
        while tx_info.len() < TX_RATES * 2 {
            tx_info.extend_from_slice(&[0xFF, 0]);
        }
        if acked {
            tx_info[1] = 1;
            for rate in tx_info[2..].chunks_mut(2) {
                rate[0] = 0xFF;
                rate[1] = 0;
            }
        }
        let flags = f.flags | if acked { hwsim::TX_STAT_ACK } else { 0 };
        let status = Builder::genl(self.family, 0, hwsim::CMD_TX_INFO_FRAME, 1)
            .attr(hwsim::ADDR_TRANSMITTER, &mac)
            .u32(hwsim::FLAGS, flags)
            .u64(hwsim::COOKIE, f.cookie)
            .u32(hwsim::SIGNAL, -50i32 as u32)
            .attr(hwsim::TX_INFO, &tx_info);
        let _ = self.hwsim.send(status);
    }

    /// A frame the radio transmits: to airsim on its channel.
    fn radio_frame(&mut self, a: guest_netlink::Attrs<'_>) {
        let (Some(frame), Some(cookie)) = (a.get(hwsim::FRAME), a.u64(hwsim::COOKIE)) else { return };
        let flags = a.u32(hwsim::FLAGS).unwrap_or(0);
        let f = InFlight { cookie, flags, tx_info: a.get(hwsim::TX_INFO).unwrap_or(&[]).to_vec() };
        let channel = a.u32(hwsim::FREQ).and_then(channel_of).filter(|c| self.channels.contains(c));
        // Not on a channel of airsim's (or airsim is away): nobody hears it.
        let (Some(channel), true) = (channel, self.greeted) else { return self.tx_status(f, false) };
        if self.tuned != Some(channel) {
            self.tuned = Some(channel);
            self.send(&Message::SetChannel { channel });
        }
        let frame = frame.to_vec();
        if flags & hwsim::TX_CTL_NO_ACK != 0 {
            self.send(&Message::Tx { id: 0, no_ack: true, frame });
            return self.tx_status(f, false);
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return self.tx_status(f, false);
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.send(&Message::Tx { id, no_ack: false, frame });
        self.in_flight.insert(id, f);
    }

    /// What came from the port (`false`: airsim is away).
    fn read_port(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.port.read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        loop {
            match self.reader.next_message() {
                Ok(Some(m)) => self.port_message(m),
                Ok(None) => return true,
                Err(LinkError::BadLength | LinkError::BadMessage) => {
                    println!("airlink: radio link out of step; greeting airsim again");
                    self.hello();
                    return true;
                }
            }
        }
    }

    fn run(&mut self) -> io::Result<()> {
        self.hello();
        loop {
            let mut away = false;
            for m in self.hwsim.events()? {
                if m.kind == self.family && m.cmd == hwsim::CMD_FRAME {
                    self.radio_frame(m.attrs());
                }
            }
            if !self.greeted && self.hello_at.elapsed() >= HELLO_EVERY {
                self.hello();
            }
            self.write_out();
            let mut fds = [
                PollFd::new(self.port.as_raw_fd(), POLLIN | if self.out.is_empty() { 0 } else { POLLOUT }),
                PollFd::new(self.hwsim.fd(), POLLIN),
            ];
            let timeout = if self.hwsim.has_events() { 0 } else { HELLO_EVERY.as_millis() as i32 };
            poll_many(&mut fds, timeout)?;
            if fds[0].revents & POLLIN != 0 && !self.read_port() {
                away = true;
            }
            // While airsim is away the port hangs up.
            if fds[0].revents & (POLLHUP | POLLERR) != 0 {
                away = true;
            }
            if away {
                self.disconnected();
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn main() {
    let start = Instant::now();
    let path = loop {
        if let Some(p) = find_port() {
            break p;
        }
        if start.elapsed() >= PORT_WAIT {
            // Stays (init would start it again were it to end).
            println!("airlink: no port named {PORT_NAME}: no virtual radio here");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let port = match OpenOptions::new().read(true).write(true).custom_flags(O_NONBLOCK).open(&path) {
        Ok(p) => p,
        Err(e) => {
            println!("airlink: cannot open {path}: {e}");
            return;
        }
    };
    let opened = Socket::open(NETLINK_GENERIC).and_then(|mut s| {
        let family = Family::find(&mut s, "MAC80211_HWSIM")?;
        Ok((s, family.id))
    });
    let (hwsim, family) = match opened {
        Ok(s) => s,
        Err(e) => {
            println!("airlink: no mac80211_hwsim: {e}");
            return;
        }
    };
    println!("airlink: radio link on {path}");
    let mut a = Airlink {
        port,
        reader: Reader::new(),
        out: VecDeque::new(),
        hwsim,
        family,
        mac: None,
        channels: Vec::new(),
        greeted: false,
        hello_at: Instant::now(),
        tuned: None,
        in_flight: BTreeMap::new(),
        next_id: 0,
    };
    if let Err(e) = a.run() {
        println!("airlink: {e}");
    }
}
