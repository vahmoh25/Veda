//! `virtio-net` — the driver for virtio network cards.
//!
//! The card is offered to `netd` through the `netdev` protocol; frames move
//! between the card's virtqueues and the shared [`Link`] rings by copying
//! (one copy each way, which is cheap next to everything else a packet
//! goes through). If the network service restarts, the driver attaches
//! again with a fresh link.
//!
//! Only the basic feature set is used: the MAC address, link status, one
//! receive and one transmit queue, 2 KiB buffers, no offloads.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vproto::net::{DeviceAttachment, DeviceInfo, InterfaceKind};
use vproto::netring::{Link, SlotMeta, kind};
use vproto::pci::pcidev;
use vrt::object::{Channel, Interrupt};
use vrt::println;
use vvirtio::{Device, DmaBuffer, Segment, Virtqueue};

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;

// Feature bits (virtio 1.2, section 5.1.3).
const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
/// `status` field bit: the link is up.
const S_LINK_UP: u16 = 1;

/// Size of `struct virtio_net_hdr_v1` (with `num_buffers`).
const NET_HDR: usize = 12;
/// Bytes per receive and transmit buffer: header plus the largest frame
/// (1514 bytes, 1518 with a VLAN tag), rounded up.
const BUF: usize = 2048;
const MAX_FRAME: usize = 1518;
const MIN_FRAME: usize = 14;
/// Descriptors per queue (and buffers per direction).
const QUEUE_SIZE: u16 = 128;
/// Slots per direction of the link to the stack.
const LINK_SLOTS: u32 = 256;
/// Poll interval without interrupts, and a safety net with them.
const POLL_NS: u64 = 5_000_000;
const IDLE_POLL_NS: u64 = 1_000_000_000;

struct Nic {
    dev: Device,
    rxq: Virtqueue,
    txq: Virtqueue,
    rx_buf: DmaBuffer,
    tx_buf: DmaBuffer,
    /// Buffer index of each outstanding descriptor chain head.
    rx_slot: Vec<u16>,
    tx_slot: Vec<u16>,
    tx_free: Vec<u16>,
    rx_irq: Option<Interrupt>,
    tx_irq: Option<Interrupt>,
    cfg_irq: Option<Interrupt>,
    has_status: bool,
    mac: [u8; 6],
    location: String,
    rx_frames: u64,
    tx_frames: u64,
    rx_dropped: u64,
}

impl Nic {
    fn link_up(&self) -> bool {
        !self.has_status || self.dev.cfg_read16(6) & S_LINK_UP != 0
    }

    /// Posts receive buffer `i`.
    fn post_rx(&mut self, i: u16) {
        let seg =
            Segment { phys: self.rx_buf.phys() + (i as usize * BUF) as u64, len: BUF as u32, device_writes: true };
        if let Some(head) = self.rxq.push(&[seg]) {
            self.rx_slot[head as usize] = i;
        }
    }

    /// Moves received frames to the stack and recycles their buffers.
    fn receive(&mut self, link: &Link) {
        let mut posted = false;
        while let Some((head, len)) = self.rxq.pop_used() {
            let i = self.rx_slot[head as usize];
            let len = len as usize;
            if (NET_HDR + MIN_FRAME..=NET_HDR + MAX_FRAME).contains(&len) {
                // SAFETY: the device has finished writing this buffer.
                let frame = unsafe { self.rx_buf.bytes(i as usize * BUF + NET_HDR, len - NET_HDR) };
                if link.send(SlotMeta::ethernet(), frame) {
                    self.rx_frames += 1;
                } else {
                    self.rx_dropped += 1;
                }
            }
            self.post_rx(i);
            posted = true;
        }
        if posted {
            self.rxq.notify();
        }
    }

    /// Reclaims transmitted buffers and sends frames the stack queued.
    fn transmit(&mut self, link: &Link, frame: &mut [u8]) {
        while let Some((head, _)) = self.txq.pop_used() {
            self.tx_free.push(self.tx_slot[head as usize]);
        }
        let mut pushed = false;
        while let Some(&i) = self.tx_free.last() {
            let Some((meta, len)) = link.recv(frame) else { break };
            if meta.kind != kind::ETHERNET || !(MIN_FRAME..=MAX_FRAME).contains(&len) {
                continue;
            }
            let off = i as usize * BUF;
            self.tx_buf.write(off, &[0u8; NET_HDR]);
            self.tx_buf.write(off + NET_HDR, &frame[..len]);
            let seg =
                Segment { phys: self.tx_buf.phys() + off as u64, len: (NET_HDR + len) as u32, device_writes: false };
            match self.txq.push(&[seg]) {
                Some(head) => {
                    self.tx_slot[head as usize] = i;
                    self.tx_free.pop();
                    self.tx_frames += 1;
                    pushed = true;
                }
                None => break,
            }
        }
        if pushed {
            self.txq.notify();
        }
    }
}

fn setup() -> Result<Nic, String> {
    let h = vrt::env::take_handle(PCIDEV_ROLE).ok_or("no pcidev channel")?;
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let location = pci.info().map(|i| format!("pci {:02x}:{:02x}.{}", i.bus, i.slot, i.function)).unwrap_or_default();
    let mut dev = Device::new(pci).map_err(|e| format!("device setup failed: {e:?}"))?;
    let features = dev.initialize(F_MAC | F_STATUS).map_err(|e| format!("feature negotiation failed: {e:?}"))?;
    let mut mac = [0u8; 6];
    if features & F_MAC != 0 {
        for (i, b) in mac.iter_mut().enumerate() {
            *b = dev.cfg_read8(i as u32);
        }
    } else {
        // No address from the device: a random locally administered one.
        vrt::object::random_bytes(&mut mac);
        mac[0] = (mac[0] & 0xFE) | 0x02;
    }
    let rxq = dev.setup_queue(0, QUEUE_SIZE).map_err(|e| format!("no receive queue: {e:?}"))?;
    let txq = dev.setup_queue(1, QUEUE_SIZE).map_err(|e| format!("no transmit queue: {e:?}"))?;
    let (rx_n, tx_n) = (rxq.size() as usize, txq.size() as usize);
    let rx_buf = DmaBuffer::new(dev.dma(), rx_n * BUF).map_err(|_| "out of DMA memory")?;
    let tx_buf = DmaBuffer::new(dev.dma(), tx_n * BUF).map_err(|_| "out of DMA memory")?;
    let rx_irq = dev.msix_vector(Some(0)).ok();
    let tx_irq = dev.msix_vector(Some(1)).ok();
    let cfg_irq = dev.msix_vector(None).ok();
    dev.driver_ok();
    let mut nic = Nic {
        dev,
        rxq,
        txq,
        rx_buf,
        tx_buf,
        rx_slot: alloc::vec![0; rx_n],
        tx_slot: alloc::vec![0; tx_n],
        tx_free: (0..tx_n as u16).collect(),
        rx_irq,
        tx_irq,
        cfg_irq,
        has_status: features & F_STATUS != 0,
        mac,
        location,
        rx_frames: 0,
        tx_frames: 0,
        rx_dropped: 0,
    };
    for i in 0..rx_n as u16 {
        nic.post_rx(i);
    }
    nic.rxq.notify();
    Ok(nic)
}

fn mac_string(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// Serves one attachment until the network service goes away.
fn run(nic: &mut Nic, att: &DeviceAttachment) {
    let mut frame = alloc::vec![0u8; BUF];
    let mut link_up = nic.link_up();
    let interrupts = nic.rx_irq.is_some() && nic.tx_irq.is_some();
    loop {
        nic.receive(&att.link);
        nic.transmit(&att.link, &mut frame);
        let up = nic.link_up();
        if up != link_up {
            link_up = up;
            println!("{}: link {}", att.name, if up { "up" } else { "down" });
            if !att.set_link(up) {
                return;
            }
        }

        // Sleep until the card or the stack has something for us. With no
        // free transmit buffers, frames from the stack must wait for a
        // transmit interrupt, so they do not count as work.
        let can_send = !nic.tx_free.is_empty();
        if can_send && !att.link.prepare_wait(false) {
            continue;
        }
        let mut items: [WaitItem; 5] = Default::default();
        let mut n = 0;
        let mut add = |h: vabi::RawHandle, s: u32| {
            items[n] = WaitItem { handle: h, signals: s, observed: 0, _reserved: 0 };
            n += 1;
        };
        add(att.channel().raw(), signals::PEER_CLOSED);
        if can_send {
            add(att.link.wake_event().raw(), signals::SIGNALED);
        }
        for irq in [&nic.rx_irq, &nic.tx_irq, &nic.cfg_irq].into_iter().flatten() {
            add(irq.raw(), signals::SIGNALED);
        }
        let timeout = if interrupts { IDLE_POLL_NS } else { POLL_NS };
        let _ = vrt::object::wait_many(&mut items[..n], vrt::time::now_ns() + timeout);
        att.link.finish_wait();
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return;
        }
        for irq in [&nic.rx_irq, &nic.tx_irq, &nic.cfg_irq].into_iter().flatten() {
            let _ = irq.ack();
        }
    }
}

fn main() -> i32 {
    let mut nic = match setup() {
        Ok(n) => n,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    println!(
        "card at {}: MAC {}, link {}{}",
        nic.location,
        mac_string(&nic.mac),
        if nic.link_up() { "up" } else { "down" },
        if nic.rx_irq.is_some() { "" } else { " (polling, no MSI-X)" }
    );
    loop {
        let info = DeviceInfo {
            kind: InterfaceKind::Ethernet,
            mac: nic.mac,
            mtu: 1500,
            driver: "virtio-net".into(),
            location: nic.location.clone(),
        };
        match DeviceAttachment::attach(info, LINK_SLOTS, BUF as u32, nic.link_up()) {
            Ok(att) => {
                println!("attached as {}", att.name);
                run(&mut nic, &att);
                println!(
                    "network service went away (rx {} frames, tx {} frames, {} dropped); attaching again",
                    nic.rx_frames, nic.tx_frames, nic.rx_dropped
                );
            }
            Err(e) => {
                println!("cannot attach: {}", e);
                vrt::time::sleep(vrt::time::Duration::from_secs(1));
            }
        }
    }
}
