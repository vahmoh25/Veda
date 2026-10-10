//! `net` — Veda's network driver for Linux: the guest's Ethernet cards
//! (Linux's network interfaces of PCI and USB devices; Wi-Fi radios are
//! `wifi`'s) as Veda's network devices, each attached to Veda's network
//! service (`netdev`) as Veda's own drivers attach theirs.
//!
//! A card's frames go between a raw packet socket on its interface and its
//! link to the service, a copy each way. Linux has no IP stack in the
//! driver VM: it moves frames and answers nothing on the network itself;
//! addresses, DHCP, ARP and everything above are Veda's. Each card has a
//! thread, which waits on the socket and the link's Veda event at once
//! (the bridge's watches); cards that come later (USB) are found as they
//! do.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use guest_sys::{POLLIN, POLLOUT, PollFd, poll_many};
use vabi::signals;
use vproto::net::{DeviceAttachment, DeviceInfo, InterfaceKind};
use vproto::netring::{SlotMeta, kind};

/// Frames each way in a card's link, and their slots (a frame and its
/// header).
const SLOTS: u32 = 256;
const SLOT_SIZE: u32 = 2048;
/// How often a card's carrier (and the guest's new cards) are looked at.
const CHECK: Duration = Duration::from_millis(500);

/// A network interface of Linux's for an Ethernet card.
struct Card {
    interface: String,
    index: i32,
    mac: [u8; 6],
    mtu: u32,
    /// Linux's driver, and where the card is (`pci 00:03.0`).
    driver: String,
    location: String,
}

fn sysfs(interface: &str, file: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/net/{interface}/{file}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn link_name(path: String) -> Option<String> {
    Some(std::fs::read_link(path).ok()?.file_name()?.to_string_lossy().into_owned())
}

impl Card {
    /// The interface as a card: Ethernet, a device's (not a virtual
    /// interface), not a radio.
    fn of(interface: &str) -> Option<Card> {
        let dir = format!("/sys/class/net/{interface}");
        let wireless = ["wireless", "phy80211"].iter().any(|d| std::fs::metadata(format!("{dir}/{d}")).is_ok());
        if sysfs(interface, "type") != "1" || wireless {
            return None;
        }
        let device = link_name(format!("{dir}/device"))?;
        let bus = link_name(format!("{dir}/device/subsystem")).unwrap_or_default();
        let location = match bus.as_str() {
            "pci" => format!("pci {}", device.trim_start_matches("0000:")),
            _ => format!("{bus} {device}"),
        };
        let driver = link_name(format!("{dir}/device/driver")).unwrap_or_else(|| "linux".into());
        let mut mac = [0u8; 6];
        let digits: Vec<u8> =
            sysfs(interface, "address").split(':').filter_map(|d| u8::from_str_radix(d, 16).ok()).collect();
        if digits.len() != 6 {
            return None;
        }
        mac.copy_from_slice(&digits);
        Some(Card {
            interface: interface.to_string(),
            index: sysfs(interface, "ifindex").parse().ok()?,
            mac,
            mtu: sysfs(interface, "mtu").parse().unwrap_or(1500),
            driver,
            location,
        })
    }

    fn carrier(&self) -> bool {
        sysfs(&self.interface, "carrier") == "1"
    }
}

/// The card's interface up, and a raw packet socket on it.
fn open(card: &Card) -> io::Result<File> {
    guest_sys::netif::set_up(&card.interface, true)?;
    guest_sys::netif::packet_socket(card.index)
}

/// Serves `card` while it lasts: attached to the network service, again
/// whenever the service comes back.
fn serve(card: Card) {
    let socket = match open(&card) {
        Ok(s) => s,
        Err(e) => {
            println!("net: {}: cannot use the card: {e}", card.interface);
            return;
        }
    };
    let mac = card.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
    println!(
        "net: {} ({}, {}): MAC {mac}, link {}",
        card.interface,
        card.driver,
        card.location,
        if card.carrier() { "up" } else { "down" }
    );
    loop {
        let info = DeviceInfo {
            kind: InterfaceKind::Ethernet,
            mac: card.mac,
            mtu: card.mtu,
            driver: format!("{} (Linux)", card.driver),
            location: card.location.clone(),
        };
        match DeviceAttachment::attach(info, SLOTS, SLOT_SIZE, card.carrier()) {
            Ok(att) => {
                println!("net: {}: attached as {}", card.interface, att.name);
                match run(&card, &socket, &att) {
                    Ok(()) => println!("net: {}: the network service went away; attaching again", card.interface),
                    Err(e) => {
                        println!("net: {}: the card is gone: {e}", card.interface);
                        return;
                    }
                }
            }
            Err(e) => {
                println!("net: {}: cannot attach: {e}", card.interface);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Moves frames until the network service goes away (`Ok`) or the card
/// does (`Err`).
fn run(card: &Card, socket: &File, att: &DeviceAttachment) -> io::Result<()> {
    let watch = |handle, signals| vrt::guest::watch(handle, signals).map_err(|e| io::Error::other(format!("{e}")));
    let mut wake = File::from(watch(att.link.wake_event().raw(), signals::SIGNALED)?);
    let closed = watch(att.channel().raw(), signals::PEER_CLOSED)?;
    let (mut to_card, mut from_card) = (vec![0u8; SLOT_SIZE as usize], vec![0u8; SLOT_SIZE as usize]);
    // A frame of Veda's the card had no room for yet.
    let mut held: Option<usize> = None;
    let mut link_up = card.carrier();
    let mut checked = Instant::now();
    loop {
        // Veda's frames to the card, while it takes them.
        loop {
            if let Some(len) = held {
                match (&*socket).write(&to_card[..len]) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    // Dropped, as a card drops what it cannot send.
                    _ => held = None,
                }
            }
            match att.link.recv(&mut to_card) {
                Some((meta, len)) if meta.kind == kind::ETHERNET => held = Some(len),
                Some(_) => {}
                None => break,
            }
        }
        // The card's frames to Veda (dropped if its ring is full).
        loop {
            match (&*socket).read(&mut from_card) {
                Ok(n) => {
                    att.link.send(SlotMeta::ethernet(), &from_card[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if checked.elapsed() >= CHECK {
            checked = Instant::now();
            let up = card.carrier();
            if up != link_up {
                link_up = up;
                println!("net: {}: link {}", card.interface, if up { "up" } else { "down" });
                if !att.set_link(up) {
                    return Ok(());
                }
            }
        }
        // Sleep until the card, the service or the link has something:
        // with a frame held, the service's frames wait for the card.
        let from_veda = held.is_none();
        if from_veda && !att.link.prepare_wait(false) {
            continue;
        }
        let mut fds = [
            PollFd::new(socket.as_raw_fd(), POLLIN | if from_veda { 0 } else { POLLOUT }),
            PollFd::new(closed.as_fd().as_raw_fd(), POLLIN),
            PollFd::new(if from_veda { wake.as_raw_fd() } else { -1 }, POLLIN),
        ];
        let _ = poll_many(&mut fds, CHECK.as_millis() as i32);
        att.link.finish_wait();
        if fds[1].revents != 0 {
            return Ok(());
        }
        if fds[2].revents & POLLIN != 0 {
            // Taken: the next poll waits for the event again.
            let _ = wake.read(&mut [0u8; 4]);
        }
    }
}

fn main() {
    let mut serving = BTreeSet::new();
    loop {
        let interfaces: Vec<String> = std::fs::read_dir("/sys/class/net")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        for interface in interfaces {
            if serving.contains(&interface) {
                continue;
            }
            if let Some(card) = Card::of(&interface) {
                serving.insert(interface);
                std::thread::spawn(move || serve(card));
            }
        }
        std::thread::sleep(CHECK);
    }
}
