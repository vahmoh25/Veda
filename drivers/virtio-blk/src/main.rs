//! `virtio-blk` — the driver for virtio block devices (disks).
//!
//! Each disk is served as the `block` protocol under the name
//! `block/<serial>` (`vproto::block::service_name`), so that clients find a
//! particular disk, such as the user's home disk, by its serial number.
//! Requests are processed one at a time: the only client, the file system,
//! issues one request at a time anyway.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;

use vabi::signals;
use vipc::{Bytes, WaitSet};
use vproto::block::{BlockError, BlockInfo, MAX_TRANSFER, block, service_name};
use vproto::pci::pcidev;
use vrt::object::{Channel, Interrupt};
use vrt::println;
use vvirtio::{Device, DmaBuffer, Segment, Virtqueue};

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
const SECTOR: usize = 512;
const QUEUE_SIZE: u16 = 16;

// Feature bits (virtio 1.2, section 5.2.3).
const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;

// Request types.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const S_OK: u8 = 0;

/// Layout of the DMA buffer: request header, status byte, data.
const HEADER: usize = 0;
const STATUS: usize = 16;
const DATA: usize = 64;

/// How long to wait for an interrupt before polling the queue again.
const POLL_NS: u64 = 50_000_000;

struct Disk {
    queue: Virtqueue,
    irq: Option<Interrupt>,
    buf: DmaBuffer,
    sectors: u64,
    read_only: bool,
    can_flush: bool,
    serial: String,
    /// Keeps the device (and its mappings) alive.
    _dev: Device,
}

impl Disk {
    /// Submits one request and waits for it. `len` bytes at `DATA` are sent
    /// to the device, or received if `device_writes`.
    fn request(&mut self, kind: u32, sector: u64, len: usize, device_writes: bool) -> Result<(), BlockError> {
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&kind.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        self.buf.write(HEADER, &header);
        self.buf.write(STATUS, &[0xFF]);
        let base = self.buf.phys();
        let head = Segment { phys: base + HEADER as u64, len: 16, device_writes: false };
        let data = Segment { phys: base + DATA as u64, len: len as u32, device_writes };
        let status = Segment { phys: base + STATUS as u64, len: 1, device_writes: true };
        let pushed = if len > 0 { self.queue.push(&[head, data, status]) } else { self.queue.push(&[head, status]) };
        pushed.ok_or(BlockError::Io)?;
        self.queue.notify();
        while self.queue.pop_used().is_none() {
            match &self.irq {
                // Interrupts are edge-like: re-arm, then look at the queue.
                Some(irq) => {
                    let _ = irq.wait_irq(vrt::time::now_ns() + POLL_NS);
                    let _ = irq.ack();
                }
                None => core::hint::spin_loop(),
            }
        }
        // SAFETY: the device has completed the request.
        let st = unsafe { self.buf.bytes(STATUS, 1)[0] };
        if st == S_OK { Ok(()) } else { Err(BlockError::Io) }
    }

    fn check_range(&self, lba: u64, sectors: u64) -> Result<(), BlockError> {
        match lba.checked_add(sectors) {
            Some(end) if end <= self.sectors => Ok(()),
            _ => Err(BlockError::OutOfRange),
        }
    }
}

impl block::Server for Disk {
    fn info(&mut self) -> BlockInfo {
        BlockInfo {
            sectors: self.sectors,
            sector_size: SECTOR as u32,
            read_only: self.read_only,
            serial: self.serial.clone(),
        }
    }

    fn read(&mut self, lba: u64, count: u32) -> Result<Bytes, BlockError> {
        let len = count as usize * SECTOR;
        if count == 0 || len > MAX_TRANSFER as usize {
            return Err(BlockError::Invalid);
        }
        self.check_range(lba, count as u64)?;
        self.request(T_IN, lba, len, true)?;
        // SAFETY: the device has finished writing the data.
        Ok(Bytes(unsafe { self.buf.bytes(DATA, len) }.to_vec()))
    }

    fn write(&mut self, lba: u64, data: Bytes) -> Result<(), BlockError> {
        let len = data.0.len();
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        if len == 0 || !len.is_multiple_of(SECTOR) || len > MAX_TRANSFER as usize {
            return Err(BlockError::Invalid);
        }
        self.check_range(lba, (len / SECTOR) as u64)?;
        self.buf.write(DATA, &data.0);
        self.request(T_OUT, lba, len, false)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        if self.can_flush { self.request(T_FLUSH, 0, 0, false) } else { Ok(()) }
    }
}

/// Reads the disk's serial number (up to 20 bytes, NUL padded).
fn serial_number(disk: &mut Disk) -> String {
    if disk.request(T_GET_ID, 0, 20, true).is_err() {
        return String::new();
    }
    // SAFETY: the device has finished writing the ID.
    let id = unsafe { disk.buf.bytes(DATA, 20) };
    let n = id.iter().position(|&b| b == 0).unwrap_or(id.len());
    String::from_utf8_lossy(&id[..n]).trim().into()
}

fn main() -> i32 {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        println!("no pcidev channel");
        return 1;
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let location = pci.info().map(|i| format!("{:02x}:{:02x}.{}", i.bus, i.slot, i.function)).unwrap_or_default();
    let mut dev = match Device::new(pci) {
        Ok(d) => d,
        Err(e) => {
            println!("device setup failed: {:?}", e);
            return 1;
        }
    };
    let features = match dev.initialize(F_RO | F_FLUSH) {
        Ok(f) => f,
        Err(e) => {
            println!("feature negotiation failed: {:?}", e);
            return 1;
        }
    };
    let sectors = dev.cfg_read64(0);
    let Ok(queue) = dev.setup_queue(0, QUEUE_SIZE) else {
        println!("no request queue");
        return 1;
    };
    let Ok(buf) = DmaBuffer::new(dev.dma(), DATA + MAX_TRANSFER as usize) else {
        println!("out of DMA memory");
        return 1;
    };
    let irq = dev.msix_vector(Some(0)).ok();
    dev.driver_ok();
    let mut disk = Disk {
        queue,
        irq,
        buf,
        sectors,
        read_only: features & F_RO != 0,
        can_flush: features & F_FLUSH != 0,
        serial: String::new(),
        _dev: dev,
    };
    disk.serial = serial_number(&mut disk);
    let id = if disk.serial.is_empty() { format!("pci-{location}") } else { disk.serial.clone() };
    let listener = match vproto::register(&service_name(&id)) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register block/{}: {:?}", id, e);
            return 1;
        }
    };
    println!(
        "disk {} at {}: {} MiB{}",
        id,
        location,
        (sectors * SECTOR as u64) >> 20,
        if disk.read_only { ", read-only" } else { "" }
    );

    let mut clients: BTreeMap<u64, Channel> = BTreeMap::new();
    let mut next = 1u64;
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE | signals::PEER_CLOSED, 0);
        for (&k, c) in &clients {
            ws.add(c.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { continue };
        for (key, observed) in ready {
            if key == 0 {
                while let Some(ch) = vproto::accept(&listener) {
                    clients.insert(next, ch);
                    next += 1;
                }
                continue;
            }
            if observed & signals::READABLE != 0 {
                while let Some(Ok(msg)) = clients.get(&key).map(|c| c.read()) {
                    match block::dispatch(&mut disk, msg) {
                        Ok(reply) => {
                            if let Some(c) = clients.get(&key) {
                                let _ = reply.send(c);
                            }
                        }
                        Err(e) => println!("bad request: {}", e),
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                clients.remove(&key);
            }
        }
    }
}
