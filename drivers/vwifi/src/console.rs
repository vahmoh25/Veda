//! virtio-console with multiple ports (virtio 1.2, section 5.3): just
//! enough to find a port by name and move a byte stream over it.
//!
//! Queue layout with `VIRTIO_CONSOLE_F_MULTIPORT`: queues 0/1 are port 0's
//! receive/transmit queues, 2/3 the control queues, and port *n* >= 1 uses
//! queues 2n+2 (receive) and 2n+3 (transmit). Queues are set up before
//! `DRIVER_OK` for ports 1..=[`MAX_PORT`] (port 0 is the console port,
//! which this driver does not use).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vproto::pci::pcidev;
use vrt::object::Interrupt;
use vvirtio::{Device, DmaBuffer, Segment, Virtqueue};

/// Feature bit: several ports and the control queues.
const F_MULTIPORT: u64 = 1 << 1;

/// Control events (5.3.6.2).
mod event {
    pub const DEVICE_READY: u16 = 0;
    pub const DEVICE_ADD: u16 = 1;
    pub const DEVICE_REMOVE: u16 = 2;
    pub const PORT_READY: u16 = 3;
    pub const PORT_OPEN: u16 = 6;
    pub const PORT_NAME: u16 = 7;
}

/// `struct virtio_console_control`: id (u32), event (u16), value (u16).
const CTRL_HDR: usize = 8;
/// Control buffers: the header plus a port name.
const CTRL_BUF: usize = 512;
const CTRL_QUEUE: u16 = 16;
/// Port buffers. Every transmitted message must fit in one.
pub const PORT_BUF: usize = 4096;
const PORT_QUEUE: u16 = 64;
/// Highest port number with queues.
pub const MAX_PORT: u32 = 3;

/// Something the device told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A port has a name.
    Named(u32, String),
    /// The host side of a port connected (`true`) or disconnected.
    HostOpen(u32, bool),
    /// A port was removed.
    Removed(u32),
}

/// One direction's buffers: a virtqueue with one fixed-size DMA buffer per
/// descriptor chain.
struct Queue {
    vq: Virtqueue,
    buf: DmaBuffer,
    size: usize,
    /// Buffer index of each outstanding chain, by head descriptor.
    slot: Vec<u16>,
    /// Transmit buffers not in use.
    free: Vec<u16>,
}

impl Queue {
    fn new(dev: &Device, index: u16, entries: u16, size: usize) -> Result<Queue, String> {
        let vq = dev.setup_queue(index, entries).map_err(|e| format!("queue {index}: {e:?}"))?;
        let n = vq.size() as usize;
        let buf = DmaBuffer::new(dev.dma(), n * size).map_err(|_| String::from("out of DMA memory"))?;
        Ok(Queue { vq, buf, size, slot: alloc::vec![0; n], free: (0..n as u16).collect() })
    }

    /// Hands every buffer to the device for receiving.
    fn post_all(&mut self) {
        while let Some(i) = self.free.pop() {
            self.post(i);
        }
        self.vq.notify();
    }

    fn post(&mut self, i: u16) {
        let seg = Segment {
            phys: self.buf.phys() + (i as usize * self.size) as u64,
            len: self.size as u32,
            device_writes: true,
        };
        if let Some(head) = self.vq.push(&[seg]) {
            self.slot[head as usize] = i;
        }
    }

    /// Passes each filled receive buffer to `f` and posts it again.
    fn drain(&mut self, mut f: impl FnMut(&[u8])) {
        let mut any = false;
        while let Some((head, len)) = self.vq.pop_used() {
            let i = self.slot[head as usize];
            let len = (len as usize).min(self.size);
            // SAFETY: the device has finished writing this buffer.
            let data = unsafe { self.buf.bytes(i as usize * self.size, len) };
            f(data);
            self.post(i);
            any = true;
        }
        if any {
            self.vq.notify();
        }
    }

    /// Queues `data` (at most one buffer) for transmission.
    fn send(&mut self, data: &[u8]) -> bool {
        while let Some((head, _)) = self.vq.pop_used() {
            self.free.push(self.slot[head as usize]);
        }
        if data.len() > self.size {
            return false;
        }
        let Some(i) = self.free.pop() else { return false };
        let off = i as usize * self.size;
        self.buf.write(off, data);
        let seg = Segment { phys: self.buf.phys() + off as u64, len: data.len() as u32, device_writes: false };
        match self.vq.push(&[seg]) {
            Some(head) => {
                self.slot[head as usize] = i;
                self.vq.notify();
                true
            }
            None => {
                self.free.push(i);
                false
            }
        }
    }

    fn has_free(&mut self) -> bool {
        while let Some((head, _)) = self.vq.pop_used() {
            self.free.push(self.slot[head as usize]);
        }
        !self.free.is_empty()
    }
}

struct Port {
    rx: Queue,
    tx: Queue,
}

pub struct Console {
    dev: Device,
    ctrl_rx: Queue,
    ctrl_tx: Queue,
    /// Index = port number; `None` for port 0 and ports without queues.
    ports: Vec<Option<Port>>,
    /// Interrupts to wait on (none: poll).
    pub irqs: Vec<Interrupt>,
}

impl Console {
    pub fn new(pci: pcidev::Client) -> Result<Console, String> {
        let mut dev = Device::new(pci).map_err(|e| format!("device setup failed: {e:?}"))?;
        let features = dev.initialize(F_MULTIPORT).map_err(|e| format!("feature negotiation failed: {e:?}"))?;
        if features & F_MULTIPORT == 0 {
            return Err("the device has no named ports (no multiport support)".into());
        }
        let max_ports = dev.cfg_read32(4);
        let last = max_ports.saturating_sub(1).min(MAX_PORT);
        let mut ctrl_rx = Queue::new(&dev, 2, CTRL_QUEUE, CTRL_BUF)?;
        let ctrl_tx = Queue::new(&dev, 3, CTRL_QUEUE, CTRL_BUF)?;
        let mut ports: Vec<Option<Port>> = Vec::new();
        ports.push(None);
        for p in 1..=last {
            let q = (2 * p + 2) as u16;
            ports.push(Some(Port {
                rx: Queue::new(&dev, q, PORT_QUEUE, PORT_BUF)?,
                tx: Queue::new(&dev, q + 1, PORT_QUEUE, PORT_BUF)?,
            }));
        }
        // Interrupts for everything that brings news; transmit completions
        // are collected when sending. Without enough vectors we poll.
        let mut irqs = Vec::new();
        let mut wanted: Vec<Option<u16>> = alloc::vec![None, Some(2)];
        wanted.extend((1..=last).map(|p| Some((2 * p + 2) as u16)));
        for q in wanted {
            match dev.msix_vector(q) {
                Ok(irq) => irqs.push(irq),
                Err(_) => {
                    irqs.clear();
                    break;
                }
            }
        }
        dev.driver_ok();
        ctrl_rx.post_all();
        for port in ports.iter_mut().flatten() {
            port.rx.post_all();
        }
        let mut c = Console { dev, ctrl_rx, ctrl_tx, ports, irqs };
        c.control(0, event::DEVICE_READY, 1);
        Ok(c)
    }

    fn control(&mut self, id: u32, ev: u16, value: u16) {
        let mut m = [0u8; CTRL_HDR];
        m[0..4].copy_from_slice(&id.to_le_bytes());
        m[4..6].copy_from_slice(&ev.to_le_bytes());
        m[6..8].copy_from_slice(&value.to_le_bytes());
        let _ = self.ctrl_tx.send(&m);
    }

    /// Handles control messages from the device.
    pub fn poll_control(&mut self) -> Vec<Notice> {
        let mut msgs: Vec<(u32, u16, u16, Vec<u8>)> = Vec::new();
        self.ctrl_rx.drain(|b| {
            if b.len() >= CTRL_HDR {
                let id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let ev = u16::from_le_bytes([b[4], b[5]]);
                let value = u16::from_le_bytes([b[6], b[7]]);
                msgs.push((id, ev, value, b[CTRL_HDR..].to_vec()));
            }
        });
        let mut out = Vec::new();
        for (id, ev, value, rest) in msgs {
            let usable = self.ports.get(id as usize).is_some_and(|p| p.is_some());
            match ev {
                event::DEVICE_ADD => self.control(id, event::PORT_READY, usable as u16),
                event::DEVICE_REMOVE => out.push(Notice::Removed(id)),
                event::PORT_NAME if usable => {
                    let name = rest.split(|&b| b == 0).next().unwrap_or(&[]);
                    out.push(Notice::Named(id, String::from_utf8_lossy(name).into_owned()));
                }
                event::PORT_OPEN if usable => out.push(Notice::HostOpen(id, value != 0)),
                _ => {}
            }
        }
        out
    }

    /// Tells the device the guest opened (or closed) a port.
    pub fn set_guest_open(&mut self, port: u32, open: bool) {
        self.control(port, event::PORT_OPEN, open as u16);
    }

    /// Passes the bytes received on `port` to `f`.
    pub fn receive(&mut self, port: u32, f: impl FnMut(&[u8])) {
        if let Some(Some(p)) = self.ports.get_mut(port as usize) {
            p.rx.drain(f);
        }
    }

    /// Queues `data` (at most [`PORT_BUF`] bytes) on `port`.
    pub fn send(&mut self, port: u32, data: &[u8]) -> bool {
        match self.ports.get_mut(port as usize) {
            Some(Some(p)) => p.tx.send(data),
            _ => false,
        }
    }

    /// Whether `port` can take another message now.
    pub fn can_send(&mut self, port: u32) -> bool {
        match self.ports.get_mut(port as usize) {
            Some(Some(p)) => p.tx.has_free(),
            _ => false,
        }
    }

    /// Acknowledges the interrupts after a wait.
    pub fn ack_interrupts(&self) {
        for irq in &self.irqs {
            let _ = irq.ack();
        }
        let _ = self.dev.isr();
    }
}
