//! `e1000` — the driver for Intel PRO/1000 network cards: the 82540EM
//! (QEMU's `e1000`, VirtualBox's default card), 82545EM (VMware's) and
//! 82574L (QEMU's `e1000e`).
//!
//! Like `virtio-net`, it offers the card to `netd` through the `netdev`
//! protocol and copies frames between the card's descriptor rings and the
//! shared link. It uses the legacy descriptor format every member of the
//! family understands, one receive and one transmit ring of 2 KiB buffers,
//! no offloads, an MSI interrupt when the card has one (polling otherwise).
//! If the network service restarts, the driver attaches again.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::ptr::{read_volatile, write_volatile};

use vabi::{WaitItem, map_flags, signals};
use vproto::net::{DeviceAttachment, DeviceInfo, InterfaceKind};
use vproto::netring::{Link, SlotMeta, kind};
use vproto::pci::pcidev;
use vrt::object::{Channel, Interrupt};
use vrt::println;
use vrt::vm::Mapping;
use vvirtio::DmaBuffer;

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;

/// Registers (Intel 8254x/82574 software developer's manuals).
mod reg {
    pub const CTRL: usize = 0x0000;
    pub const STATUS: usize = 0x0008;
    pub const ICR: usize = 0x00C0;
    pub const IMS: usize = 0x00D0;
    pub const IMC: usize = 0x00D8;
    pub const RCTL: usize = 0x0100;
    pub const TCTL: usize = 0x0400;
    pub const TIPG: usize = 0x0410;
    pub const RDBAL: usize = 0x2800;
    pub const RDBAH: usize = 0x2804;
    pub const RDLEN: usize = 0x2808;
    pub const RDH: usize = 0x2810;
    pub const RDT: usize = 0x2818;
    pub const TDBAL: usize = 0x3800;
    pub const TDBAH: usize = 0x3804;
    pub const TDLEN: usize = 0x3808;
    pub const TDH: usize = 0x3810;
    pub const TDT: usize = 0x3818;
    pub const MTA: usize = 0x5200;
    pub const RAL0: usize = 0x5400;
    pub const RAH0: usize = 0x5404;
}

const CTRL_LRST: u32 = 1 << 3;
const CTRL_ASDE: u32 = 1 << 5;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_ILOS: u32 = 1 << 7;
const CTRL_RST: u32 = 1 << 26;
const CTRL_VME: u32 = 1 << 30;
const CTRL_PHY_RST: u32 = 1 << 31;
const STATUS_LU: u32 = 1 << 1;
const RCTL_EN: u32 = 1 << 1;
/// Multicast promiscuous (IPv6 neighbour discovery needs multicast).
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const RAH_AV: u32 = 1 << 31;
/// Interrupt causes: transmit written back, link change, receive
/// threshold, receive overrun, receive timer.
const INT_MASK: u32 = (1 << 0) | (1 << 2) | (1 << 4) | (1 << 6) | (1 << 7);

const TX_EOP: u8 = 1 << 0;
const TX_IFCS: u8 = 1 << 1;
const TX_RS: u8 = 1 << 3;
const DESC_DD: u8 = 1 << 0;
const RX_EOP: u8 = 1 << 1;

/// Descriptors per ring (a multiple of 8) and bytes per buffer.
const RING: usize = 128;
const DESC: usize = 16;
const BUF: usize = 2048;
const MIN_FRAME: usize = 14;
const MAX_FRAME: usize = 1518;
/// Slots per direction of the link to the stack.
const LINK_SLOTS: u32 = 256;
const POLL_NS: u64 = 5_000_000;
const IDLE_POLL_NS: u64 = 1_000_000_000;

/// The PCI capability ID of MSI.
const CAP_MSI: u8 = 0x05;

struct Card {
    _bar: Mapping,
    mmio: *mut u8,
    rx_ring: DmaBuffer,
    tx_ring: DmaBuffer,
    rx_buf: DmaBuffer,
    tx_buf: DmaBuffer,
    /// Next receive descriptor to look at.
    rx_next: usize,
    /// Next transmit descriptor to fill, and the oldest one in flight.
    tx_next: usize,
    tx_clean: usize,
    irq: Option<Interrupt>,
    mac: [u8; 6],
    location: String,
    rx_frames: u64,
    tx_frames: u64,
    rx_dropped: u64,
}

impl Card {
    fn r(&self, off: usize) -> u32 {
        // SAFETY: a register inside the mapped BAR 0.
        unsafe { read_volatile(self.mmio.add(off) as *const u32) }
    }

    fn w(&self, off: usize, v: u32) {
        // SAFETY: a register inside the mapped BAR 0.
        unsafe { write_volatile(self.mmio.add(off) as *mut u32, v) }
    }

    fn link_up(&self) -> bool {
        self.r(reg::STATUS) & STATUS_LU != 0
    }

    /// Byte `off` of descriptor `i` in `ring`.
    fn desc_u8(ring: &DmaBuffer, i: usize, off: usize) -> u8 {
        // SAFETY: inside the ring buffer; the device may write it, hence volatile.
        unsafe { read_volatile(ring.ptr().add(i * DESC + off)) }
    }

    fn set_desc(ring: &DmaBuffer, i: usize, bytes: &[u8; DESC]) {
        ring.write(i * DESC, bytes);
    }

    fn rx_desc(&self, i: usize) -> [u8; DESC] {
        let mut d = [0u8; DESC];
        d[0..8].copy_from_slice(&(self.rx_buf.phys() + (i * BUF) as u64).to_le_bytes());
        d
    }

    /// Moves received frames to the stack and gives the buffers back.
    fn receive(&mut self, link: &Link) {
        let mut returned = None;
        for _ in 0..RING {
            let i = self.rx_next;
            let status = Self::desc_u8(&self.rx_ring, i, 12);
            if status & DESC_DD == 0 {
                break;
            }
            let len =
                u16::from_le_bytes([Self::desc_u8(&self.rx_ring, i, 8), Self::desc_u8(&self.rx_ring, i, 9)]) as usize;
            let errors = Self::desc_u8(&self.rx_ring, i, 13);
            // Frames larger than one buffer (never sent with these settings)
            // and frames with errors are dropped.
            if status & RX_EOP != 0 && errors == 0 && (MIN_FRAME..=MAX_FRAME).contains(&len) {
                // SAFETY: the device has finished writing this buffer.
                let frame = unsafe { self.rx_buf.bytes(i * BUF, len) };
                if link.send(SlotMeta::ethernet(), frame) {
                    self.rx_frames += 1;
                } else {
                    self.rx_dropped += 1;
                }
            } else {
                self.rx_dropped += 1;
            }
            let d = self.rx_desc(i);
            Self::set_desc(&self.rx_ring, i, &d);
            returned = Some(i);
            self.rx_next = (i + 1) % RING;
        }
        if let Some(i) = returned {
            // The tail is the last descriptor the card may fill.
            self.w(reg::RDT, i as u32);
        }
    }

    /// Reclaims sent buffers and queues the frames the stack has.
    fn transmit(&mut self, link: &Link, frame: &mut [u8]) {
        while self.tx_clean != self.tx_next && Self::desc_u8(&self.tx_ring, self.tx_clean, 12) & DESC_DD != 0 {
            self.tx_clean = (self.tx_clean + 1) % RING;
        }
        let mut queued = false;
        loop {
            // Keep one descriptor free so a full ring differs from an empty one.
            if (self.tx_next + 1) % RING == self.tx_clean {
                break;
            }
            let Some((meta, len)) = link.recv(frame) else { break };
            if meta.kind != kind::ETHERNET || !(MIN_FRAME..=MAX_FRAME).contains(&len) {
                continue;
            }
            let i = self.tx_next;
            self.tx_buf.write(i * BUF, &frame[..len]);
            let mut d = [0u8; DESC];
            d[0..8].copy_from_slice(&(self.tx_buf.phys() + (i * BUF) as u64).to_le_bytes());
            d[8..10].copy_from_slice(&(len as u16).to_le_bytes());
            d[11] = TX_EOP | TX_IFCS | TX_RS;
            Self::set_desc(&self.tx_ring, i, &d);
            self.tx_next = (i + 1) % RING;
            self.tx_frames += 1;
            queued = true;
        }
        if queued {
            self.w(reg::TDT, self.tx_next as u32);
        }
    }

    fn tx_has_room(&self) -> bool {
        (self.tx_next + 1) % RING != self.tx_clean
    }
}

fn config_read(pci: &pcidev::Client, off: u16, width: u8) -> Option<u32> {
    pci.config_read(off, width).ok()?.ok()
}

/// Sets up MSI if the card has it: one vector for every cause.
fn enable_msi(pci: &pcidev::Client) -> Option<Interrupt> {
    let mut cap = (config_read(pci, 0x34, 1)? & 0xFC) as u16;
    for _ in 0..48 {
        if cap == 0 {
            return None;
        }
        if config_read(pci, cap, 1)? as u8 == CAP_MSI {
            break;
        }
        cap = (config_read(pci, cap + 1, 1)? & 0xFC) as u16;
    }
    if cap == 0 || config_read(pci, cap, 1)? as u8 != CAP_MSI {
        return None;
    }
    let control = config_read(pci, cap + 2, 2)? as u16;
    let (irq, msi) = pci.alloc_msi().ok()?.ok()?;
    let is64 = control & (1 << 7) != 0;
    let ok = |r: Result<Result<(), vproto::pci::PciError>, vipc::IpcError>| matches!(r, Ok(Ok(())));
    let mut good = ok(pci.config_write(cap + 4, 4, msi.address as u32));
    let data_at = if is64 {
        good &= ok(pci.config_write(cap + 8, 4, (msi.address >> 32) as u32));
        cap + 12
    } else {
        cap + 8
    };
    good &= ok(pci.config_write(data_at, 2, msi.data & 0xFFFF));
    // Enable, with a single message.
    good &= ok(pci.config_write(cap + 2, 2, ((control & !(0x7 << 4)) | 1) as u32));
    good.then_some(irq)
}

fn setup() -> Result<Card, String> {
    let h = vrt::env::take_handle(PCIDEV_ROLE).ok_or("no pcidev channel")?;
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let info = pci.info().map_err(|_| String::from("devmgr unavailable"))?;
    let location = format!("pci {:02x}:{:02x}.{}", info.bus, info.slot, info.function);
    if !matches!(pci.enable(true), Ok(Ok(()))) {
        return Err("cannot enable the device".into());
    }
    let dma = match pci.dma_resource() {
        Ok(Ok(r)) => r,
        _ => return Err("no DMA resource".into()),
    };
    let vmo = match pci.map_bar(0) {
        Ok(Ok(v)) => v,
        _ => return Err("cannot map the registers (BAR 0)".into()),
    };
    let size = vmo.size().map_err(|_| String::from("bad BAR 0"))?;
    if size < 0x6000 {
        return Err(format!("BAR 0 is too small ({size} bytes)"));
    }
    let bar =
        Mapping::new(vmo, size, map_flags::READ | map_flags::WRITE).map_err(|_| String::from("cannot map BAR 0"))?;
    let alloc = |len: usize| DmaBuffer::new(&dma, len).map_err(|_| String::from("out of DMA memory"));
    let mut card = Card {
        mmio: bar.as_ptr(),
        _bar: bar,
        rx_ring: alloc(RING * DESC)?,
        tx_ring: alloc(RING * DESC)?,
        rx_buf: alloc(RING * BUF)?,
        tx_buf: alloc(RING * BUF)?,
        rx_next: 0,
        tx_next: 0,
        tx_clean: 0,
        irq: None,
        mac: [0; 6],
        location,
        rx_frames: 0,
        tx_frames: 0,
        rx_dropped: 0,
    };

    // Reset, with every interrupt masked.
    card.w(reg::IMC, u32::MAX);
    card.w(reg::CTRL, card.r(reg::CTRL) | CTRL_RST);
    let start = vrt::time::now_ns();
    while card.r(reg::CTRL) & CTRL_RST != 0 {
        if vrt::time::now_ns() - start > 100_000_000 {
            return Err("the card does not come out of reset".into());
        }
        core::hint::spin_loop();
    }
    vrt::time::sleep(vrt::time::Duration::from_millis(10));
    card.w(reg::IMC, u32::MAX);
    let _ = card.r(reg::ICR);
    let ctrl = card.r(reg::CTRL) & !(CTRL_LRST | CTRL_PHY_RST | CTRL_ILOS | CTRL_VME);
    card.w(reg::CTRL, ctrl | CTRL_SLU | CTRL_ASDE);

    // The address the firmware loaded from the EEPROM, or a random
    // locally administered one.
    let (ral, rah) = (card.r(reg::RAL0), card.r(reg::RAH0));
    if rah & RAH_AV != 0 && (ral != 0 || rah & 0xFFFF != 0) {
        card.mac[..4].copy_from_slice(&ral.to_le_bytes());
        card.mac[4..].copy_from_slice(&(rah as u16).to_le_bytes());
    } else {
        vrt::object::random_bytes(&mut card.mac);
        card.mac[0] = (card.mac[0] & 0xFE) | 0x02;
        let m = card.mac;
        card.w(reg::RAL0, u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
        card.w(reg::RAH0, u16::from_le_bytes([m[4], m[5]]) as u32 | RAH_AV);
    }
    for i in 0..128 {
        card.w(reg::MTA + i * 4, 0);
    }

    // Receive ring: every buffer handed to the card.
    for i in 0..RING {
        let d = card.rx_desc(i);
        Card::set_desc(&card.rx_ring, i, &d);
    }
    let rx = card.rx_ring.phys();
    card.w(reg::RDBAL, rx as u32);
    card.w(reg::RDBAH, (rx >> 32) as u32);
    card.w(reg::RDLEN, (RING * DESC) as u32);
    card.w(reg::RDH, 0);
    card.w(reg::RDT, (RING - 1) as u32);
    // 2 KiB buffers (size bits 0), CRC stripped, broadcasts and multicasts.
    card.w(reg::RCTL, RCTL_EN | RCTL_BAM | RCTL_MPE | RCTL_SECRC);

    // Transmit ring, empty.
    let tx = card.tx_ring.phys();
    card.w(reg::TDBAL, tx as u32);
    card.w(reg::TDBAH, (tx >> 32) as u32);
    card.w(reg::TDLEN, (RING * DESC) as u32);
    card.w(reg::TDH, 0);
    card.w(reg::TDT, 0);
    // Pad short frames, collision threshold 15, distance 64 (full duplex).
    card.w(reg::TCTL, TCTL_EN | TCTL_PSP | (0x0F << 4) | (0x40 << 12));
    card.w(reg::TIPG, 10 | (8 << 10) | (6 << 20));

    card.irq = enable_msi(&pci);
    card.w(reg::IMS, INT_MASK);
    Ok(card)
}

fn mac_string(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// Serves one attachment until the network service goes away.
fn run(card: &mut Card, att: &DeviceAttachment) {
    let mut frame = alloc::vec![0u8; BUF];
    let mut link_up = card.link_up();
    loop {
        // Reading ICR acknowledges the interrupt causes.
        let _ = card.r(reg::ICR);
        card.receive(&att.link);
        card.transmit(&att.link, &mut frame);
        let up = card.link_up();
        if up != link_up {
            link_up = up;
            println!("{}: link {}", att.name, if up { "up" } else { "down" });
            if !att.set_link(up) {
                return;
            }
        }
        let can_send = card.tx_has_room();
        if can_send && !att.link.prepare_wait(false) {
            continue;
        }
        let mut items: [WaitItem; 3] = Default::default();
        let mut n = 0;
        let mut add = |h: vabi::RawHandle, s: u32| {
            items[n] = WaitItem { handle: h, signals: s, observed: 0, _reserved: 0 };
            n += 1;
        };
        add(att.channel().raw(), signals::PEER_CLOSED);
        if can_send {
            add(att.link.wake_event().raw(), signals::SIGNALED);
        }
        if let Some(irq) = &card.irq {
            add(irq.raw(), signals::SIGNALED);
        }
        // Without an interrupt, poll; with one, the timeout is a safety net
        // (and catches transmit completions while the ring is full).
        let timeout = if card.irq.is_some() && can_send { IDLE_POLL_NS } else { POLL_NS };
        let _ = vrt::object::wait_many(&mut items[..n], vrt::time::now_ns() + timeout);
        att.link.finish_wait();
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return;
        }
        if let Some(irq) = &card.irq {
            let _ = irq.ack();
        }
    }
}

fn main() -> i32 {
    let mut card = match setup() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    println!(
        "card at {}: MAC {}, link {}{}",
        card.location,
        mac_string(&card.mac),
        if card.link_up() { "up" } else { "down" },
        if card.irq.is_some() { ", MSI" } else { " (polling, no MSI)" }
    );
    loop {
        let info = DeviceInfo {
            kind: InterfaceKind::Ethernet,
            mac: card.mac,
            mtu: 1500,
            driver: "e1000".into(),
            location: card.location.clone(),
        };
        match DeviceAttachment::attach(info, LINK_SLOTS, BUF as u32, card.link_up()) {
            Ok(att) => {
                println!("attached as {}", att.name);
                run(&mut card, &att);
                println!(
                    "network service went away (rx {} frames, tx {} frames, {} dropped); attaching again",
                    card.rx_frames, card.tx_frames, card.rx_dropped
                );
            }
            Err(e) => {
                println!("cannot attach: {}", e);
                vrt::time::sleep(vrt::time::Duration::from_secs(1));
            }
        }
    }
}
