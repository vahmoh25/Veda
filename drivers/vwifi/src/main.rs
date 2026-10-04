//! `vwifi` — the driver for Veda's virtual Wi-Fi radio.
//!
//! QEMU has no Wi-Fi adapter. Under QEMU the radio is a virtio-serial port
//! named `org.veda.wlan.0` whose other end is `airsim` on the host (see
//! `tools/airsim`). This driver exchanges raw 802.11 frames with it using
//! the radio link protocol ([`vradiolink`]) and offers the radio to the
//! Wi-Fi service through the `wlanphy` protocol, as a driver for real
//! hardware would. Everything above the radio — scanning, authentication,
//! the key handshakes, encryption — happens in the Wi-Fi service, so this
//! driver never sees keys or passwords.
//!
//! When the host side disconnects the radio is reported unavailable, and
//! when it comes back the driver greets it again and restores the channel
//! and power the service asked for. If the Wi-Fi service restarts, the
//! driver attaches again.

#![no_std]
#![no_main]

extern crate alloc;

mod console;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vproto::netring::{Link, RxInfo, SlotMeta, TxInfo, TxStatus, kind};
use vproto::pci::pcidev;
use vproto::wlan::{
    Band, ChannelInfo, PhyError, PhyInfo, PhyStats, RADIO_STATE_EVENT, RadioState, phy_caps, wlanphy, wlanphy_ctl,
};
use vradiolink::{LinkError, Message, Reader, VERSION, msg};
use vrt::object::Channel;
use vrt::println;

use console::{Console, Notice, PORT_BUF};

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
/// The virtio-serial port that carries the radio link.
const PORT_NAME: &str = "org.veda.wlan.0";
/// Slots per direction of the link to the Wi-Fi service, and their size
/// (an 802.11 frame is at most 2346 bytes without aggregation).
const LINK_SLOTS: u32 = 256;
const SLOT_SIZE: u32 = 4096;
/// Poll interval without interrupts, and a safety net with them.
const POLL_NS: u64 = 5_000_000;
const IDLE_NS: u64 = 500_000_000;
/// How often to greet the host again until it answers.
const HELLO_RETRY_NS: u64 = 1_000_000_000;

fn band_of(channel: u8) -> Band {
    if channel <= 14 { Band::Ghz2 } else { Band::Ghz5 }
}

fn freq_mhz(channel: u8) -> u16 {
    match channel {
        14 => 2484,
        1..=13 => 2407 + 5 * channel as u16,
        _ => 5000 + 5 * channel as u16,
    }
}

fn mac_string(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

struct Radio {
    con: Console,
    /// The port carrying the radio link, once named.
    port: Option<u32>,
    /// The host side of the port is connected.
    host_open: bool,
    /// The host answered our hello on the current connection.
    greeted: bool,
    hello_sent_ns: u64,
    reader: Reader,
    mac: [u8; 6],
    channels: Vec<u8>,
    /// What the Wi-Fi service asked for, restored after reconnecting.
    channel: Option<u8>,
    power: bool,
    stats: PhyStats,
    pci_location: String,
    location: String,
}

impl Radio {
    fn usable(&self) -> bool {
        self.port.is_some() && self.host_open && self.greeted
    }

    fn state(&self) -> RadioState {
        if self.usable() { RadioState::Ready } else { RadioState::Unavailable }
    }

    fn send(&mut self, m: &Message) -> bool {
        let Some(port) = self.port else { return false };
        let bytes = m.encode();
        bytes.len() <= PORT_BUF && self.con.send(port, &bytes)
    }

    fn hello(&mut self) {
        self.greeted = false;
        self.reader.resync(msg::HELLO_ACK);
        self.hello_sent_ns = vrt::time::now_ns();
        let mac = self.mac;
        self.send(&Message::Hello { version: VERSION, mac });
    }

    fn channel_infos(&self) -> Vec<ChannelInfo> {
        self.channels
            .iter()
            .map(|&c| ChannelInfo { band: band_of(c), number: c, freq_mhz: freq_mhz(c), flags: 0, max_power_dbm: 20 })
            .collect()
    }

    /// Handles news from the device and the host. Received frames go to
    /// `link` (dropped before the radio is attached).
    fn pump(&mut self, link: Option<&Link>) {
        for n in self.con.poll_control() {
            match n {
                Notice::Named(id, name) if name == PORT_NAME => {
                    println!("radio link on virtio-serial port {}", id);
                    self.port = Some(id);
                    self.location = format!("{}, virtio-serial port {} ({})", self.pci_location, id, PORT_NAME);
                    self.con.set_guest_open(id, true);
                }
                Notice::Named(..) => {}
                Notice::HostOpen(id, open) if Some(id) == self.port => {
                    self.host_open = open;
                    if open {
                        println!("radio host connected");
                        self.hello();
                    } else {
                        println!("radio host disconnected");
                        self.greeted = false;
                        self.reader.reset();
                    }
                }
                Notice::HostOpen(..) => {}
                Notice::Removed(id) if Some(id) == self.port => {
                    println!("radio port removed");
                    self.port = None;
                    self.host_open = false;
                    self.greeted = false;
                }
                Notice::Removed(_) => {}
            }
        }
        if let Some(port) = self.port {
            let mut bytes: Vec<u8> = Vec::new();
            self.con.receive(port, |b| bytes.extend_from_slice(b));
            if !bytes.is_empty() {
                self.reader.push(&bytes);
            }
            loop {
                match self.reader.next_message() {
                    Ok(Some(m)) => self.handle(m, link),
                    Ok(None) => break,
                    Err(LinkError::BadLength | LinkError::BadMessage) => {
                        println!("radio link out of step; greeting the host again");
                        self.hello();
                    }
                }
            }
        }
        if self.host_open && !self.greeted && vrt::time::now_ns() >= self.hello_sent_ns + HELLO_RETRY_NS {
            self.hello();
        }
    }

    fn handle(&mut self, m: Message, link: Option<&Link>) {
        match m {
            Message::HelloAck { version, mac, channels } => {
                if version != VERSION {
                    println!("radio host speaks link version {} (we speak {})", version, VERSION);
                }
                if self.mac == [0; 6] && mac[0] & 1 == 0 && mac != [0; 6] {
                    self.mac = mac;
                }
                let mut ch: Vec<u8> = channels.into_iter().filter(|&c| matches!(c, 1..=14 | 32..=177)).collect();
                ch.sort_unstable();
                ch.dedup();
                ch.truncate(64);
                if self.channels.is_empty() {
                    self.channels = ch;
                }
                self.greeted = true;
                println!("radio ready: MAC {}, channels {:?}", mac_string(&self.mac), self.channels);
                if let Some(c) = self.channel {
                    self.send(&Message::SetChannel { channel: c });
                }
                let on = self.power;
                self.send(&Message::SetPower { on });
            }
            Message::Rx { channel, signal_dbm, frame } => {
                let Some(link) = link else { return };
                if !self.usable() || !self.power {
                    return;
                }
                let info = RxInfo { channel, band: band_of(channel) as u8, signal_dbm, noise_dbm: -95, rate: 0 };
                if link.send(SlotMeta { kind: kind::IEEE80211, flags: 0, meta: info.pack() }, &frame) {
                    self.stats.rx_frames += 1;
                } else {
                    self.stats.rx_dropped += 1;
                }
            }
            Message::TxStatus { id, acked } => {
                if !acked {
                    self.stats.tx_failed += 1;
                }
                if let Some(link) = link {
                    let st = TxStatus { id, acked, attempts: 1 };
                    link.send(SlotMeta { kind: kind::TX_STATUS, flags: 0, meta: st.pack() }, &[]);
                }
            }
            other => println!("unexpected radio link message {:?}", other),
        }
    }

    /// Sends a frame from the Wi-Fi service, or reports it failed.
    fn transmit(&mut self, meta: SlotMeta, frame: &[u8], link: &Link) {
        if meta.kind != kind::IEEE80211 {
            return;
        }
        let info = TxInfo::unpack(meta.meta);
        let no_ack = info.no_ack || info.id == 0;
        let sent = self.usable()
            && self.power
            && frame.len() >= 10
            && self.send(&Message::Tx { id: info.id, no_ack, frame: frame.to_vec() });
        if sent {
            self.stats.tx_frames += 1;
        } else {
            self.stats.tx_failed += 1;
            if !no_ack {
                let st = TxStatus { id: info.id, acked: false, attempts: 0 };
                link.send(SlotMeta { kind: kind::TX_STATUS, flags: 0, meta: st.pack() }, &[]);
            }
        }
    }

    fn can_send(&mut self) -> bool {
        match self.port {
            Some(p) if self.usable() => self.con.can_send(p),
            // Frames are dropped (and reported) while unusable.
            _ => true,
        }
    }

    /// Waits for the device, the host or the given handles.
    fn wait(&self, extra: &mut [WaitItem]) {
        let mut items: Vec<WaitItem> = extra.to_vec();
        for irq in &self.con.irqs {
            items.push(WaitItem { handle: irq.raw(), signals: signals::SIGNALED, observed: 0, _reserved: 0 });
        }
        let mut deadline = vrt::time::now_ns() + if self.con.irqs.is_empty() { POLL_NS } else { IDLE_NS };
        if self.host_open && !self.greeted {
            deadline = deadline.min(self.hello_sent_ns + HELLO_RETRY_NS);
        }
        let _ = vrt::object::wait_many(&mut items, deadline);
        for (dst, src) in extra.iter_mut().zip(items.iter()) {
            dst.observed = src.observed;
        }
        self.con.ack_interrupts();
    }
}

/// Requests from the Wi-Fi service.
struct Control<'a>(&'a mut Radio);

impl wlanphy_ctl::Server for Control<'_> {
    fn set_channel(&mut self, band: Band, number: u8) -> Result<(), PhyError> {
        let r = &mut *self.0;
        if !r.channels.contains(&number) || band_of(number) != band {
            return Err(PhyError::BadChannel);
        }
        r.channel = Some(number);
        if r.usable() {
            r.send(&Message::SetChannel { channel: number });
        }
        Ok(())
    }

    fn set_power(&mut self, on: bool) -> Result<(), PhyError> {
        let r = &mut *self.0;
        r.power = on;
        if r.usable() {
            r.send(&Message::SetPower { on });
        }
        Ok(())
    }

    fn stats(&mut self) -> PhyStats {
        self.0.stats
    }
}

struct Attachment {
    client: wlanphy::Client,
    link: Link,
    control: Channel,
}

fn attach(r: &Radio) -> Result<Attachment, String> {
    let ch = vproto::connect(wlanphy::NAME).map_err(|e| format!("cannot reach the Wi-Fi service: {:?}", e))?;
    let client = wlanphy::Client::new(ch);
    let (link, ends) = Link::create(LINK_SLOTS, SLOT_SIZE).map_err(|e| format!("cannot create the link: {}", e))?;
    let (control, theirs) = Channel::create().map_err(|_| String::from("cannot create a channel"))?;
    let info = PhyInfo {
        driver: "vwifi".into(),
        location: r.location.clone(),
        mac: r.mac,
        channels: r.channel_infos(),
        caps: phy_caps::TX_STATUS,
    };
    match client.attach(info, ends, theirs, r.state()) {
        Ok(Ok(())) => Ok(Attachment { client, link, control }),
        Ok(Err(e)) => Err(format!("the Wi-Fi service refused the radio: {:?}", e)),
        Err(_) => Err("the Wi-Fi service went away".into()),
    }
}

/// Serves the Wi-Fi service until it goes away.
fn serve(r: &mut Radio, att: &Attachment) {
    let mut frame = alloc::vec![0u8; SLOT_SIZE as usize];
    let mut reported = r.state();
    loop {
        r.pump(Some(&att.link));
        if r.state() != reported {
            reported = r.state();
            if vipc::send_event(att.client.channel(), RADIO_STATE_EVENT, reported).is_err() {
                return;
            }
        }
        for _ in 0..LINK_SLOTS {
            if !r.can_send() {
                break;
            }
            let Some((meta, len)) = att.link.recv(&mut frame) else { break };
            r.transmit(meta, &frame[..len], &att.link);
        }
        loop {
            match att.control.read() {
                Ok(m) => {
                    let mut c = Control(r);
                    match wlanphy_ctl::dispatch(&mut c, m) {
                        Ok(reply) => {
                            if reply.send(&att.control).is_err() {
                                return;
                            }
                        }
                        Err(e) => println!("bad request from the Wi-Fi service: {}", e),
                    }
                }
                Err(vabi::Error::ShouldWait) => break,
                Err(_) => return,
            }
        }

        let can_send = r.can_send();
        if can_send && !att.link.prepare_wait(false) {
            continue;
        }
        let mut items = [
            WaitItem { handle: att.client.channel().raw(), signals: signals::PEER_CLOSED, observed: 0, _reserved: 0 },
            WaitItem {
                handle: att.control.raw(),
                signals: signals::READABLE | signals::PEER_CLOSED,
                observed: 0,
                _reserved: 0,
            },
            WaitItem { handle: att.link.wake_event().raw(), signals: signals::SIGNALED, observed: 0, _reserved: 0 },
        ];
        let n = if can_send { 3 } else { 2 };
        r.wait(&mut items[..n]);
        att.link.finish_wait();
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return;
        }
    }
}

fn main() -> i32 {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        println!("no pcidev channel");
        return 1;
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let location = pci.info().map(|i| format!("pci {:02x}:{:02x}.{}", i.bus, i.slot, i.function)).unwrap_or_default();
    let con = match Console::new(pci) {
        Ok(c) => c,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    let mut r = Radio {
        con,
        port: None,
        host_open: false,
        greeted: false,
        hello_sent_ns: 0,
        reader: Reader::new(),
        mac: [0; 6],
        channels: Vec::new(),
        channel: None,
        power: false,
        stats: PhyStats::default(),
        pci_location: location.clone(),
        location,
    };
    // Find the radio port and greet the host before offering the radio:
    // its address and channels come from the host.
    let mut waited_ns = 0u64;
    while !r.usable() {
        r.pump(None);
        if r.usable() {
            break;
        }
        let before = vrt::time::now_ns();
        r.wait(&mut []);
        waited_ns += vrt::time::now_ns() - before;
        if r.port.is_none() && waited_ns > 10_000_000_000 {
            println!("no port named {}; this virtio-serial device is not a radio", PORT_NAME);
            return 0;
        }
    }
    loop {
        match attach(&r) {
            Ok(att) => {
                println!("radio offered to the Wi-Fi service");
                serve(&mut r, &att);
                println!(
                    "Wi-Fi service went away (rx {} frames, tx {} frames, {} failed); offering the radio again",
                    r.stats.rx_frames, r.stats.tx_frames, r.stats.tx_failed
                );
                r.power = false;
                if r.usable() {
                    r.send(&Message::SetPower { on: false });
                }
            }
            Err(e) => {
                println!("{}", e);
                vrt::time::sleep(vrt::time::Duration::from_secs(1));
                r.pump(None);
            }
        }
    }
}
