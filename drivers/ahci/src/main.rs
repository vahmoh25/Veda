//! `ahci` — the driver for SATA disks behind an AHCI controller: the disk
//! controller of QEMU's q35 machine and of most PCs.
//!
//! Like `virtio-blk`, every disk is served as the `block` protocol under the
//! name `block/<serial>` (`vproto::block::service_name`), so the file system
//! finds the home disk by its serial number whatever the controller.
//!
//! The driver uses command slot 0 of each port and processes one request at
//! a time (the file system issues one at a time anyway): READ/WRITE DMA EXT
//! with one physical region per request, FLUSH CACHE (EXT), and IDENTIFY
//! DEVICE for the size and serial number. It waits on an MSI interrupt when
//! the controller has one, and polls otherwise. ATAPI drives (CD/DVD) and
//! port multipliers are ignored.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use vabi::{map_flags, signals};
use vipc::{Bytes, WaitSet};
use vproto::block::{BlockError, BlockInfo, MAX_TRANSFER, block, service_name};
use vproto::pci::pcidev;
use vrt::object::{Channel, Interrupt};
use vrt::println;
use vrt::vm::Mapping;
use vvirtio::DmaBuffer;

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
const SECTOR: usize = 512;
/// The BAR holding the controller's registers (ABAR).
const ABAR: u8 = 5;

/// Registers (AHCI 1.3.1, section 3).
mod reg {
    pub const CAP: usize = 0x00;
    pub const GHC: usize = 0x04;
    pub const IS: usize = 0x08;
    pub const PI: usize = 0x0C;
    pub const CAP2: usize = 0x24;
    pub const BOHC: usize = 0x28;
    /// Port registers, relative to `0x100 + 0x80 * port`.
    pub const P_CLB: usize = 0x00;
    pub const P_CLBU: usize = 0x04;
    pub const P_FB: usize = 0x08;
    pub const P_FBU: usize = 0x0C;
    pub const P_IS: usize = 0x10;
    pub const P_IE: usize = 0x14;
    pub const P_CMD: usize = 0x18;
    pub const P_TFD: usize = 0x20;
    pub const P_SIG: usize = 0x24;
    pub const P_SSTS: usize = 0x28;
    pub const P_SERR: usize = 0x30;
    pub const P_CI: usize = 0x38;
}

const CAP_S64A: u32 = 1 << 31;
const CAP_SSS: u32 = 1 << 27;
const CAP2_BOH: u32 = 1 << 0;
const BOHC_BOS: u32 = 1 << 0;
const BOHC_OOS: u32 = 1 << 1;
const GHC_AE: u32 = 1 << 31;
const GHC_IE: u32 = 1 << 1;
const CMD_ST: u32 = 1 << 0;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 0x01;
const TFD_DRQ: u32 = 0x08;
const TFD_BSY: u32 = 0x80;
/// Port interrupts: device-to-host register FIS, PIO setup FIS, task file
/// error.
const PIS_DHRS: u32 = 1 << 0;
const PIS_PSS: u32 = 1 << 1;
const PIS_TFES: u32 = 1 << 30;
/// Signature of an ATA disk.
const SIG_ATA: u32 = 0x0000_0101;

const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH: u8 = 0xE7;
const ATA_FLUSH_EXT: u8 = 0xEA;

/// Layout of each port's DMA memory: the command list (slot 0 used), the
/// received-FIS area, the command table with one PRD entry, and the data
/// buffer.
const CL: usize = 0;
const FB: usize = 1024;
const CT: usize = 2048;
const DATA: usize = 4096;
const PORT_MEM: usize = DATA + MAX_TRANSFER as usize;

/// How long a command may take.
const COMMAND_TIMEOUT_NS: u64 = 10_000_000_000;

struct Port {
    index: usize,
    mem: DmaBuffer,
    sectors: u64,
    lba48: bool,
    flush_ext: bool,
    serial: String,
    model: String,
}

struct Controller {
    _bar: Mapping,
    mmio: *mut u8,
    irq: Option<Interrupt>,
    s64a: bool,
}

impl Controller {
    fn r(&self, off: usize) -> u32 {
        // SAFETY: a register inside the mapped ABAR.
        unsafe { read_volatile(self.mmio.add(off) as *const u32) }
    }

    fn w(&self, off: usize, v: u32) {
        // SAFETY: a register inside the mapped ABAR.
        unsafe { write_volatile(self.mmio.add(off) as *mut u32, v) }
    }

    fn pr(&self, port: usize, off: usize) -> u32 {
        self.r(0x100 + 0x80 * port + off)
    }

    fn pw(&self, port: usize, off: usize, v: u32) {
        self.w(0x100 + 0x80 * port + off, v)
    }

    /// Waits until `done` holds, up to `timeout_ns`.
    fn wait(&self, timeout_ns: u64, mut done: impl FnMut(&Controller) -> bool) -> bool {
        let end = vrt::time::now_ns() + timeout_ns;
        let mut spins = 0u32;
        loop {
            if done(self) {
                return true;
            }
            if vrt::time::now_ns() > end {
                return false;
            }
            spins += 1;
            if spins > 200 {
                vrt::time::sleep(vrt::time::Duration::from_millis(1));
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// Stops a port's command processing and FIS reception.
    fn stop_port(&self, p: usize) -> bool {
        let cmd = self.pr(p, reg::P_CMD);
        self.pw(p, reg::P_CMD, cmd & !CMD_ST);
        if !self.wait(500_000_000, |c| c.pr(p, reg::P_CMD) & CMD_CR == 0) {
            return false;
        }
        let cmd = self.pr(p, reg::P_CMD);
        self.pw(p, reg::P_CMD, cmd & !CMD_FRE);
        self.wait(500_000_000, |c| c.pr(p, reg::P_CMD) & CMD_FR == 0)
    }

    /// Runs one command on slot 0 of `port` and waits for it. `len` bytes
    /// at `DATA` are sent (`write`) or received.
    fn exec(&self, port: &Port, cmd: u8, lba: u64, count: u16, len: usize, write: bool) -> Result<(), BlockError> {
        let p = port.index;
        if !self.wait(COMMAND_TIMEOUT_NS, |c| c.pr(p, reg::P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
            return Err(BlockError::Io);
        }
        let base = port.mem.phys();
        // Command header 0: a 5-dword FIS, one PRD entry if there is data.
        let mut header = [0u8; 32];
        let prdtl: u32 = if len > 0 { 1 } else { 0 };
        let dw0 = 5 | if write { 1 << 6 } else { 0 } | (prdtl << 16);
        header[0..4].copy_from_slice(&dw0.to_le_bytes());
        header[8..16].copy_from_slice(&(base + CT as u64).to_le_bytes());
        port.mem.write(CL, &header);
        // Command table: a host-to-device register FIS, then the PRD entry.
        let mut table = [0u8; 0x90];
        table[0] = 0x27;
        table[1] = 0x80;
        table[2] = cmd;
        let l = lba.to_le_bytes();
        table[4..7].copy_from_slice(&l[0..3]);
        table[7] = if cmd == ATA_IDENTIFY { 0 } else { 0x40 };
        table[8..11].copy_from_slice(&l[3..6]);
        table[12..14].copy_from_slice(&count.to_le_bytes());
        if len > 0 {
            table[0x80..0x88].copy_from_slice(&(base + DATA as u64).to_le_bytes());
            let dbc = (len as u32 - 1) | (1 << 31);
            table[0x8C..0x90].copy_from_slice(&dbc.to_le_bytes());
        }
        port.mem.write(CT, &table);
        self.pw(p, reg::P_IS, u32::MAX);
        fence(Ordering::SeqCst);
        self.pw(p, reg::P_CI, 1);
        let end = vrt::time::now_ns() + COMMAND_TIMEOUT_NS;
        let mut spins = 0u32;
        loop {
            let is = self.pr(p, reg::P_IS);
            if is & PIS_TFES != 0 {
                self.recover(p);
                return Err(BlockError::Io);
            }
            if self.pr(p, reg::P_CI) & 1 == 0 {
                break;
            }
            if vrt::time::now_ns() > end {
                self.recover(p);
                return Err(BlockError::Io);
            }
            spins += 1;
            match &self.irq {
                Some(irq) if spins > 50 => {
                    let _ = irq.wait_irq(vrt::time::now_ns() + 1_000_000);
                    let _ = irq.ack();
                }
                _ if spins > 500 => vrt::time::sleep(vrt::time::Duration::from_millis(1)),
                _ => core::hint::spin_loop(),
            }
        }
        self.pw(p, reg::P_IS, u32::MAX);
        self.w(reg::IS, 1 << p);
        fence(Ordering::SeqCst);
        if self.pr(p, reg::P_TFD) & TFD_ERR != 0 { Err(BlockError::Io) } else { Ok(()) }
    }

    /// Restarts a port after an error, so later commands can run.
    fn recover(&self, p: usize) {
        let _ = self.stop_port(p);
        self.pw(p, reg::P_SERR, u32::MAX);
        self.pw(p, reg::P_IS, u32::MAX);
        let cmd = self.pr(p, reg::P_CMD);
        self.pw(p, reg::P_CMD, cmd | CMD_FRE);
        let cmd = self.pr(p, reg::P_CMD);
        self.pw(p, reg::P_CMD, cmd | CMD_ST);
    }
}

/// An ATA string: 2 characters per word, high byte first.
fn ata_string(id: &[u8], from_word: usize, to_word: usize) -> String {
    let mut s = String::new();
    for w in from_word..to_word {
        for b in [id[2 * w + 1], id[2 * w]] {
            if b.is_ascii_graphic() || b == b' ' {
                s.push(b as char);
            }
        }
    }
    String::from(s.trim())
}

/// Sets up one port; returns the disk on it, if any.
fn setup_port(c: &Controller, p: usize, dma: &vrt::object::Resource, staggered: bool) -> Option<Port> {
    // A device is present and the link is up (DET = 3).
    if c.pr(p, reg::P_SSTS) & 0xF != 3 {
        return None;
    }
    if !c.stop_port(p) {
        println!("port {}: does not stop; ignored", p);
        return None;
    }
    let mem = DmaBuffer::new(dma, PORT_MEM).ok()?;
    if !c.s64a && mem.phys() + PORT_MEM as u64 > 1 << 32 {
        println!("port {}: no 64-bit DMA and no memory below 4 GiB; ignored", p);
        return None;
    }
    let base = mem.phys();
    c.pw(p, reg::P_CLB, (base + CL as u64) as u32);
    c.pw(p, reg::P_CLBU, ((base + CL as u64) >> 32) as u32);
    c.pw(p, reg::P_FB, (base + FB as u64) as u32);
    c.pw(p, reg::P_FBU, ((base + FB as u64) >> 32) as u32);
    c.pw(p, reg::P_SERR, u32::MAX);
    c.pw(p, reg::P_IS, u32::MAX);
    let mut cmd = c.pr(p, reg::P_CMD) | CMD_FRE | CMD_POD;
    if staggered {
        cmd |= CMD_SUD;
    }
    c.pw(p, reg::P_CMD, cmd);
    if !c.wait(2_000_000_000, |c| c.pr(p, reg::P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
        println!("port {}: the device stays busy; ignored", p);
        return None;
    }
    let sig = c.pr(p, reg::P_SIG);
    if sig != SIG_ATA {
        // ATAPI (0xEB140101), port multipliers and bridges are not handled.
        println!("port {}: not a disk (signature {:#010x}); ignored", p, sig);
        return None;
    }
    c.pw(p, reg::P_IE, if c.irq.is_some() { PIS_DHRS | PIS_PSS | PIS_TFES } else { 0 });
    let cmd = c.pr(p, reg::P_CMD);
    c.pw(p, reg::P_CMD, cmd | CMD_ST);

    let mut port =
        Port { index: p, mem, sectors: 0, lba48: false, flush_ext: false, serial: String::new(), model: String::new() };
    if c.exec(&port, ATA_IDENTIFY, 0, 0, 512, false).is_err() {
        println!("port {}: IDENTIFY DEVICE failed; ignored", p);
        return None;
    }
    // SAFETY: the device has finished writing the identify data.
    let id = unsafe { port.mem.bytes(DATA, 512) }.to_vec();
    let word = |i: usize| u16::from_le_bytes([id[2 * i], id[2 * i + 1]]) as u64;
    // Logical sectors larger than 512 bytes are not supported.
    let w106 = word(106);
    if w106 & 0xC000 == 0x4000 && w106 & (1 << 12) != 0 {
        let words = word(117) | word(118) << 16;
        if words * 2 != SECTOR as u64 {
            println!("port {}: {}-byte sectors are not supported; ignored", p, words * 2);
            return None;
        }
    }
    port.lba48 = word(83) & (1 << 10) != 0;
    port.flush_ext = port.lba48 && word(83) & (1 << 13) != 0;
    port.sectors = if port.lba48 {
        word(100) | word(101) << 16 | word(102) << 32 | word(103) << 48
    } else {
        word(60) | word(61) << 16
    };
    port.serial = ata_string(&id, 10, 20);
    port.model = ata_string(&id, 27, 47);
    if port.sectors == 0 || !port.lba48 {
        // Disks without 48-bit addressing are older than this driver cares for.
        println!("port {}: no 48-bit addressing; ignored", p);
        return None;
    }
    Some(port)
}

/// A disk being served.
struct Disk<'a> {
    c: &'a Controller,
    port: &'a Port,
}

impl Disk<'_> {
    fn check_range(&self, lba: u64, sectors: u64) -> Result<(), BlockError> {
        match lba.checked_add(sectors) {
            Some(end) if end <= self.port.sectors => Ok(()),
            _ => Err(BlockError::OutOfRange),
        }
    }
}

impl block::Server for Disk<'_> {
    fn info(&mut self) -> BlockInfo {
        BlockInfo {
            sectors: self.port.sectors,
            sector_size: SECTOR as u32,
            read_only: false,
            serial: self.port.serial.clone(),
        }
    }

    fn read(&mut self, lba: u64, count: u32) -> Result<Bytes, BlockError> {
        let len = count as usize * SECTOR;
        if count == 0 || len > MAX_TRANSFER as usize {
            return Err(BlockError::Invalid);
        }
        self.check_range(lba, count as u64)?;
        self.c.exec(self.port, ATA_READ_DMA_EXT, lba, count as u16, len, false)?;
        // SAFETY: the device has finished writing the data.
        Ok(Bytes(unsafe { self.port.mem.bytes(DATA, len) }.to_vec()))
    }

    fn write(&mut self, lba: u64, data: Bytes) -> Result<(), BlockError> {
        let len = data.0.len();
        if len == 0 || !len.is_multiple_of(SECTOR) || len > MAX_TRANSFER as usize {
            return Err(BlockError::Invalid);
        }
        self.check_range(lba, (len / SECTOR) as u64)?;
        self.port.mem.write(DATA, &data.0);
        self.c.exec(self.port, ATA_WRITE_DMA_EXT, lba, (len / SECTOR) as u16, len, true)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        let cmd = if self.port.flush_ext { ATA_FLUSH_EXT } else { ATA_FLUSH };
        self.c.exec(self.port, cmd, 0, 0, 0, false)
    }
}

fn main() -> i32 {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        println!("no pcidev channel");
        return 1;
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let location = pci.info().map(|i| format!("{:02x}:{:02x}.{}", i.bus, i.slot, i.function)).unwrap_or_default();
    if !matches!(pci.enable(true), Ok(Ok(()))) {
        println!("cannot enable the controller");
        return 1;
    }
    let Ok(Ok(dma)) = pci.dma_resource() else {
        println!("no DMA resource");
        return 1;
    };
    let Ok(Ok(vmo)) = pci.map_bar(ABAR) else {
        println!("cannot map the registers (BAR {})", ABAR);
        return 1;
    };
    let Ok(size) = vmo.size() else { return 1 };
    let Ok(bar) = Mapping::new(vmo, size, map_flags::READ | map_flags::WRITE) else {
        println!("cannot map the registers");
        return 1;
    };
    let mut c = Controller { mmio: bar.as_ptr(), _bar: bar, irq: None, s64a: false };
    if size < 0x180 {
        println!("register window too small ({} bytes)", size);
        return 1;
    }
    let cap = c.r(reg::CAP);
    c.s64a = cap & CAP_S64A != 0;
    // Take the controller from the firmware if it asks for a handoff.
    if c.r(reg::CAP2) & CAP2_BOH != 0 {
        c.w(reg::BOHC, c.r(reg::BOHC) | BOHC_OOS);
        let _ = c.wait(1_000_000_000, |c| c.r(reg::BOHC) & BOHC_BOS == 0);
    }
    c.w(reg::GHC, c.r(reg::GHC) | GHC_AE);
    c.irq = vproto::pci::enable_msi(&pci);
    if c.irq.is_some() {
        c.w(reg::IS, u32::MAX);
        c.w(reg::GHC, c.r(reg::GHC) | GHC_IE);
    }
    let implemented = c.r(reg::PI);
    let max_ports = (size.saturating_sub(0x100) / 0x80).min(32);
    let mut ports: Vec<Port> = Vec::new();
    for p in 0..max_ports {
        if implemented & (1 << p) != 0
            && let Some(port) = setup_port(&c, p, &dma, cap & CAP_SSS != 0)
        {
            ports.push(port);
        }
    }
    if ports.is_empty() {
        println!("controller at {}: no disks", location);
        return 0;
    }

    // One service per disk.
    let mut listeners: Vec<(Channel, usize)> = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        let id = if port.serial.is_empty() { format!("ahci-{}-{}", location, port.index) } else { port.serial.clone() };
        match vproto::register(&service_name(&id)) {
            Ok(l) => {
                println!(
                    "disk {} at {} port {}: {} MiB ({}){}",
                    id,
                    location,
                    port.index,
                    (port.sectors * SECTOR as u64) >> 20,
                    port.model,
                    if c.irq.is_some() { "" } else { ", polling" }
                );
                listeners.push((l, i));
            }
            Err(e) => println!("cannot register block/{}: {:?}", id, e),
        }
    }

    // Keys: listeners are 1..=n, clients start above.
    let mut clients: BTreeMap<u64, (Channel, usize)> = BTreeMap::new();
    let mut next = 1000u64;
    loop {
        let mut ws = WaitSet::new();
        for (k, (l, _)) in listeners.iter().enumerate() {
            ws.add(l.raw(), signals::READABLE | signals::PEER_CLOSED, k as u64 + 1);
        }
        for (&k, (ch, _)) in &clients {
            ws.add(ch.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { continue };
        for (key, observed) in ready {
            if key >= 1 && key as usize <= listeners.len() {
                let (l, disk) = &listeners[key as usize - 1];
                while let Some(ch) = vproto::accept(l) {
                    clients.insert(next, (ch, *disk));
                    next += 1;
                }
                continue;
            }
            if observed & signals::READABLE != 0 {
                while let Some((Ok(msg), disk)) = clients.get(&key).map(|(ch, d)| (ch.read(), *d)) {
                    let mut server = Disk { c: &c, port: &ports[disk] };
                    match block::dispatch(&mut server, msg) {
                        Ok(reply) => {
                            if let Some((ch, _)) = clients.get(&key) {
                                let _ = reply.send(ch);
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
