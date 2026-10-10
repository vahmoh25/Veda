//! `nvme` — the driver for NVM Express disks: the SSDs of most PCs since
//! about 2016, and QEMU's `nvme`.
//!
//! Like `ahci` and `virtio-blk`, every namespace is served as the `block`
//! protocol under `block/<serial>` (the controller's serial number; a
//! namespace other than the first adds `-n<id>`), so the file system finds
//! the home disk by its serial number whatever the disk.
//!
//! The controller is reset and given an admin queue pair and one I/O queue
//! pair, and the driver runs a request at a time (the file system issues
//! one at a time anyway): READ, WRITE and FLUSH, the data in one buffer as
//! large as the largest transfer, which PRP entries describe (a list past
//! two pages); a request larger than the controller takes is split.
//! IDENTIFY gives the serial number and model, the namespaces, and their
//! sizes and block sizes (512 or 4096 bytes; formats with metadata are not
//! used). It waits on an MSI-X (or MSI) interrupt when the controller has
//! one, and polls otherwise.

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
/// The BAR of the controller's registers.
const REGISTERS: u8 = 0;
const PAGE: usize = 4096;

/// Registers (NVM Express 2.0, section 3.1).
mod reg {
    pub const CAP: usize = 0x00;
    pub const CC: usize = 0x14;
    pub const CSTS: usize = 0x1C;
    pub const AQA: usize = 0x24;
    pub const ASQ: usize = 0x28;
    pub const ACQ: usize = 0x30;
    pub const DOORBELLS: usize = 0x1000;
}

const CC_EN: u32 = 1 << 0;
/// 64-byte submission and 16-byte completion queue entries; 4 KiB pages;
/// the NVM command set.
const CC_IO_ENTRIES: u32 = 6 << 16 | 4 << 20;
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;

/// Admin commands.
const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const ADMIN_SET_FEATURES: u8 = 0x09;
const FEATURE_QUEUES: u32 = 0x07;
const IDENTIFY_NAMESPACE: u32 = 0;
const IDENTIFY_CONTROLLER: u32 = 1;
const IDENTIFY_NAMESPACES: u32 = 2;
/// NVM commands.
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

/// Entries of each queue.
const ENTRIES: u16 = 32;
/// Layout of the DMA memory: the admin queues, the I/O queues, the
/// identify buffer, the PRP list, the data buffer.
const ADMIN_SQ: usize = 0;
const ADMIN_CQ: usize = PAGE;
const IO_SQ: usize = 2 * PAGE;
const IO_CQ: usize = 3 * PAGE;
const IDENTIFY: usize = 4 * PAGE;
const PRP_LIST: usize = 5 * PAGE;
const DATA: usize = 6 * PAGE;
const MEMORY: usize = DATA + MAX_TRANSFER as usize;

/// How long a command may take.
const COMMAND_TIMEOUT_NS: u64 = 30_000_000_000;

/// A queue pair: where its entries are, the next submission and the next
/// completion, whose phase tag tells a new one.
struct Queue {
    id: u16,
    sq: usize,
    cq: usize,
    tail: u16,
    head: u16,
    phase: bool,
}

impl Queue {
    fn new(id: u16, sq: usize, cq: usize) -> Queue {
        Queue { id, sq, cq, tail: 0, head: 0, phase: true }
    }
}

/// Why a command failed: the controller's status (its type and code), or
/// no answer.
enum Failure {
    Status(u16),
    Timeout,
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::Status(s) => write!(f, "status type {}, code {:#04x}", s >> 8, s & 0xFF),
            Failure::Timeout => f.write_str("no answer"),
        }
    }
}

struct Controller {
    _bar: Mapping,
    mmio: *mut u8,
    mem: DmaBuffer,
    irq: Option<Interrupt>,
    /// Bytes between doorbells.
    stride: usize,
    /// The largest transfer of one command, in bytes.
    max_transfer: usize,
    admin: Queue,
    io: Queue,
    next_id: u16,
}

/// A namespace being served.
struct Namespace {
    id: u32,
    blocks: u64,
    block_size: usize,
}

impl Controller {
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: a register inside the mapped BAR.
        unsafe { read_volatile(self.mmio.add(off) as *const u32) }
    }

    fn w32(&self, off: usize, v: u32) {
        // SAFETY: a register inside the mapped BAR.
        unsafe { write_volatile(self.mmio.add(off) as *mut u32, v) }
    }

    fn r64(&self, off: usize) -> u64 {
        self.r32(off) as u64 | (self.r32(off + 4) as u64) << 32
    }

    fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }

    /// Waits until `done` holds, up to `timeout_ns`.
    fn wait(&self, timeout_ns: u64, mut done: impl FnMut(&Controller) -> bool) -> bool {
        let end = vrt::time::now_ns() + timeout_ns;
        while !done(self) {
            if vrt::time::now_ns() > end {
                return false;
            }
            vrt::time::sleep(vrt::time::Duration::from_millis(1));
        }
        true
    }

    /// Runs command `cmd` (its sixteen dwords; the identifier is filled in)
    /// on queue pair `io` (else the admin one) and waits for it: dword 0 of
    /// its completion.
    fn run(&mut self, io: bool, mut cmd: [u32; 16]) -> Result<u32, Failure> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        cmd[0] |= (id as u32) << 16;
        let stride = self.stride;
        let q = if io { &mut self.io } else { &mut self.admin };
        let bytes: Vec<u8> = cmd.iter().flat_map(|d| d.to_le_bytes()).collect();
        self.mem.write(q.sq + q.tail as usize * 64, &bytes);
        q.tail = (q.tail + 1) % ENTRIES;
        let (qid, tail) = (q.id as usize, q.tail);
        fence(Ordering::SeqCst);
        self.w32(reg::DOORBELLS + 2 * qid * stride, tail as u32);
        let end = vrt::time::now_ns() + COMMAND_TIMEOUT_NS;
        let mut spins = 0u32;
        loop {
            let q = if io { &mut self.io } else { &mut self.admin };
            let at = q.cq + q.head as usize * 16;
            // SAFETY: the completion queue's entry, which the device writes
            // whole before it flips the phase tag.
            let entry = unsafe { self.mem.bytes(at, 16) };
            let dword =
                |i: usize| u32::from_le_bytes([entry[4 * i], entry[4 * i + 1], entry[4 * i + 2], entry[4 * i + 3]]);
            let status = dword(3) >> 16;
            if (status & 1 == 1) == q.phase {
                fence(Ordering::SeqCst);
                let (result, done_id) = (dword(0), dword(3) as u16);
                q.head = (q.head + 1) % ENTRIES;
                if q.head == 0 {
                    q.phase = !q.phase;
                }
                let head = q.head;
                self.w32(reg::DOORBELLS + (2 * qid + 1) * stride, head as u32);
                if done_id != id {
                    // An answer to a command that timed out before.
                    continue;
                }
                return match (status >> 1) & 0x7FF {
                    0 => Ok(result),
                    code => Err(Failure::Status(code as u16)),
                };
            }
            if vrt::time::now_ns() > end || self.r32(reg::CSTS) & CSTS_CFS != 0 {
                return Err(Failure::Timeout);
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
    }

    /// An admin or NVM command: its opcode, namespace, data pointers and
    /// command dwords 10 to 15.
    fn command(opcode: u8, namespace: u32, prp: [u64; 2], cdw: [u32; 6]) -> [u32; 16] {
        let mut c = [0u32; 16];
        c[0] = opcode as u32;
        c[1] = namespace;
        c[6] = prp[0] as u32;
        c[7] = (prp[0] >> 32) as u32;
        c[8] = prp[1] as u32;
        c[9] = (prp[1] >> 32) as u32;
        c[10..16].copy_from_slice(&cdw);
        c
    }

    /// IDENTIFY: the 4 KiB structure `cns` names (of `namespace`).
    fn identify(&mut self, cns: u32, namespace: u32) -> Result<Vec<u8>, Failure> {
        let buffer = self.mem.phys() + IDENTIFY as u64;
        self.run(false, Self::command(ADMIN_IDENTIFY, namespace, [buffer, 0], [cns, 0, 0, 0, 0, 0]))?;
        // SAFETY: the device has written the structure.
        Ok(unsafe { self.mem.bytes(IDENTIFY, PAGE) }.to_vec())
    }

    /// The data buffer's `len` bytes from page `offset` on as PRP entries:
    /// the first page, and the second or the list of the others (the
    /// buffer's list from that page on).
    fn prp(&self, offset: usize, len: usize) -> [u64; 2] {
        let first = self.mem.phys() + (DATA + offset) as u64;
        match len {
            0..=PAGE => [first, 0],
            _ if len <= 2 * PAGE => [first, first + PAGE as u64],
            _ => [first, self.mem.phys() + (PRP_LIST + offset / PAGE * 8) as u64],
        }
    }

    /// Reads (or writes) `count` blocks of `ns` at `lba`, the data in the
    /// data buffer, in as many commands as the controller needs (each a
    /// whole number of pages of the buffer).
    fn transfer(&mut self, ns: &Namespace, lba: u64, count: usize, write: bool) -> Result<(), BlockError> {
        let per_command = (self.max_transfer / ns.block_size).max(1);
        let mut done = 0;
        while done < count {
            let n = (count - done).min(per_command);
            let offset = done * ns.block_size;
            let opcode = if write { IO_WRITE } else { IO_READ };
            let start = lba + done as u64;
            let prp = self.prp(offset, n * ns.block_size);
            let cmd = Self::command(opcode, ns.id, prp, [start as u32, (start >> 32) as u32, n as u32 - 1, 0, 0, 0]);
            if let Err(e) = self.run(true, cmd) {
                let what = if write { "write" } else { "read" };
                println!("namespace {}: {} of {} blocks at {} failed: {}", ns.id, what, n, start, e);
                return Err(BlockError::Io);
            }
            done += n;
        }
        Ok(())
    }
}

/// An identify string: ASCII, padded with spaces.
fn ascii(bytes: &[u8]) -> String {
    String::from(String::from_utf8_lossy(bytes).trim_matches(|c: char| c == ' ' || c == '\0'))
}

/// Resets the controller and starts it with its admin queues, then its
/// I/O queue pair.
fn start(c: &mut Controller, cap: u64) -> Result<(), String> {
    // CAP.TO: how long the controller may take to get ready, in 500 ms.
    let timeout = ((cap >> 24) & 0xFF).max(1) * 500_000_000;
    if c.r32(reg::CC) & CC_EN != 0 {
        c.w32(reg::CC, c.r32(reg::CC) & !CC_EN);
    }
    if !c.wait(timeout, |c| c.r32(reg::CSTS) & CSTS_RDY == 0) {
        return Err(String::from("does not reset"));
    }
    let base = c.mem.phys();
    let entries = (ENTRIES - 1) as u32;
    c.w32(reg::AQA, entries << 16 | entries);
    c.w64(reg::ASQ, base + ADMIN_SQ as u64);
    c.w64(reg::ACQ, base + ADMIN_CQ as u64);
    c.w32(reg::CC, CC_IO_ENTRIES | CC_EN);
    if !c.wait(timeout, |c| c.r32(reg::CSTS) & (CSTS_RDY | CSTS_CFS) != 0) || c.r32(reg::CSTS) & CSTS_CFS != 0 {
        return Err(String::from("does not start"));
    }
    // One I/O queue pair, its completions on interrupt entry 0.
    let fail = |what: &str, e: Failure| format!("{what}: {e}");
    c.run(false, Controller::command(ADMIN_SET_FEATURES, 0, [0, 0], [FEATURE_QUEUES, 0, 0, 0, 0, 0]))
        .map_err(|e| fail("cannot have an I/O queue", e))?;
    let interrupts = if c.irq.is_some() { 1 << 1 } else { 0 };
    let cq = Controller::command(
        ADMIN_CREATE_CQ,
        0,
        [base + IO_CQ as u64, 0],
        [entries << 16 | 1, interrupts | 1, 0, 0, 0, 0],
    );
    c.run(false, cq).map_err(|e| fail("cannot make the I/O completion queue", e))?;
    let sq =
        Controller::command(ADMIN_CREATE_SQ, 0, [base + IO_SQ as u64, 0], [entries << 16 | 1, 1 << 16 | 1, 0, 0, 0, 0]);
    c.run(false, sq).map_err(|e| fail("cannot make the I/O submission queue", e))?;
    Ok(())
}

/// The namespace `id`, if it can be served: its size and block size.
fn namespace(c: &mut Controller, id: u32) -> Option<Namespace> {
    let ns = c.identify(IDENTIFY_NAMESPACE, id).ok()?;
    let qword = |at: usize| u64::from_le_bytes(ns[at..at + 8].try_into().unwrap_or_default());
    let blocks = qword(0);
    let format = (ns[26] & 0xF) as usize | ((ns[26] >> 5) as usize & 3) << 4;
    let lbaf = u32::from_le_bytes(ns[128 + 4 * format..132 + 4 * format].try_into().unwrap_or_default());
    let (metadata, shift) = (lbaf & 0xFFFF, (lbaf >> 16) & 0xFF);
    if blocks == 0 {
        return None;
    }
    if metadata != 0 || !matches!(shift, 9 | 12) {
        println!("namespace {}: blocks of {} bytes with {} of metadata are not supported", id, 1u64 << shift, metadata);
        return None;
    }
    Some(Namespace { id, blocks, block_size: 1 << shift })
}

/// A namespace being served on a client's connection.
struct Disk<'a> {
    c: &'a mut Controller,
    ns: &'a Namespace,
    serial: &'a str,
}

impl Disk<'_> {
    /// The blocks of `count` blocks at `lba`, if they are on the disk and
    /// fit one transfer.
    fn check(&self, lba: u64, count: u64) -> Result<usize, BlockError> {
        let len = count as usize * self.ns.block_size;
        if count == 0 || len > MAX_TRANSFER as usize {
            return Err(BlockError::Invalid);
        }
        match lba.checked_add(count) {
            Some(end) if end <= self.ns.blocks => Ok(len),
            _ => Err(BlockError::OutOfRange),
        }
    }
}

impl block::Server for Disk<'_> {
    fn info(&mut self) -> BlockInfo {
        BlockInfo {
            sectors: self.ns.blocks,
            sector_size: self.ns.block_size as u32,
            read_only: false,
            serial: String::from(self.serial),
        }
    }

    fn read(&mut self, lba: u64, count: u32) -> Result<Bytes, BlockError> {
        let len = self.check(lba, count as u64)?;
        self.c.transfer(self.ns, lba, count as usize, false)?;
        // SAFETY: the device has finished writing the data.
        Ok(Bytes(unsafe { self.c.mem.bytes(DATA, len) }.to_vec()))
    }

    fn write(&mut self, lba: u64, data: Bytes) -> Result<(), BlockError> {
        let len = data.0.len();
        if !len.is_multiple_of(self.ns.block_size) {
            return Err(BlockError::Invalid);
        }
        let count = (len / self.ns.block_size) as u64;
        self.check(lba, count)?;
        self.c.mem.write(DATA, &data.0);
        self.c.transfer(self.ns, lba, count as usize, true)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        let cmd = Controller::command(IO_FLUSH, self.ns.id, [0, 0], [0; 6]);
        self.c.run(true, cmd).map(|_| ()).map_err(|e| {
            println!("namespace {}: flush failed: {}", self.ns.id, e);
            BlockError::Io
        })
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
    let Ok(Ok(vmo)) = pci.map_bar(REGISTERS) else {
        println!("cannot map the registers (BAR {})", REGISTERS);
        return 1;
    };
    let Ok(size) = vmo.size() else { return 1 };
    let Ok(bar) = Mapping::new(vmo, size, map_flags::READ | map_flags::WRITE) else {
        println!("cannot map the registers");
        return 1;
    };
    let Ok(mem) = DmaBuffer::new(&dma, MEMORY) else {
        println!("no DMA memory");
        return 1;
    };
    let mut c = Controller {
        mmio: bar.as_ptr(),
        _bar: bar,
        mem,
        irq: None,
        stride: 4,
        max_transfer: MAX_TRANSFER as usize,
        admin: Queue::new(0, ADMIN_SQ, ADMIN_CQ),
        io: Queue::new(1, IO_SQ, IO_CQ),
        next_id: 0,
    };
    if size < reg::DOORBELLS + 16 {
        println!("register window too small ({} bytes)", size);
        return 1;
    }
    let cap = c.r64(reg::CAP);
    c.stride = 4 << ((cap >> 32) & 0xF);
    // Queues of `ENTRIES` (CAP.MQES is the most, less one), the NVM
    // command set (CAP.CSS), 4 KiB pages (CAP.MPSMIN).
    let mpsmin = (cap >> 48) & 0xF;
    if (cap & 0xFFFF) + 1 < ENTRIES as u64 || cap & (1 << 37) == 0 || mpsmin != 0 {
        println!("controller at {}: not one this driver takes (capabilities {:#x})", location, cap);
        return 1;
    }
    // The PRP list: the data buffer's pages after the first.
    let data = c.mem.phys() + DATA as u64;
    let list: Vec<u8> =
        (1..MAX_TRANSFER as usize / PAGE).flat_map(|i| (data + (i * PAGE) as u64).to_le_bytes()).collect();
    c.mem.write(PRP_LIST, &list);
    c.irq = vproto::pci::enable_msix(&pci).or_else(|| vproto::pci::enable_msi(&pci));
    if let Err(why) = start(&mut c, cap) {
        println!("controller at {}: {}", location, why);
        return 1;
    }
    let Ok(controller) = c.identify(IDENTIFY_CONTROLLER, 0) else {
        println!("controller at {}: IDENTIFY CONTROLLER failed", location);
        return 1;
    };
    let serial = ascii(&controller[4..24]);
    let model = ascii(&controller[24..64]);
    // MDTS: the largest transfer, in pages, as a power of two (0: any).
    if controller[77] != 0 {
        c.max_transfer = c.max_transfer.min(PAGE << controller[77]);
    }
    let Ok(list) = c.identify(IDENTIFY_NAMESPACES, 0) else {
        println!("controller at {}: no namespace list", location);
        return 1;
    };
    let ids: Vec<u32> =
        list.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).take_while(|&id| id != 0).collect();
    let namespaces: Vec<Namespace> = ids.iter().filter_map(|&id| namespace(&mut c, id)).collect();
    if namespaces.is_empty() {
        println!("controller at {} ({}): no namespaces", location, model);
        return 0;
    }

    // One service per namespace.
    let mut listeners: Vec<(Channel, usize, String)> = Vec::new();
    for (i, ns) in namespaces.iter().enumerate() {
        let base = if serial.is_empty() { format!("nvme-{}", location) } else { serial.clone() };
        let id = if i == 0 { base } else { format!("{}-n{}", base, ns.id) };
        match vproto::register(&service_name(&id)) {
            Ok(l) => {
                println!(
                    "disk {} at {} namespace {}: {} MiB in {}-byte blocks ({}){}",
                    id,
                    location,
                    ns.id,
                    (ns.blocks * ns.block_size as u64) >> 20,
                    ns.block_size,
                    model,
                    if c.irq.is_some() { "" } else { ", polling" }
                );
                listeners.push((l, i, id));
            }
            Err(e) => println!("cannot register block/{}: {:?}", id, e),
        }
    }

    // Keys: listeners are 1..=n, clients start above.
    let mut clients: BTreeMap<u64, (Channel, usize)> = BTreeMap::new();
    let mut next = 1000u64;
    loop {
        let mut ws = WaitSet::new();
        for (k, (l, _, _)) in listeners.iter().enumerate() {
            ws.add(l.raw(), signals::READABLE | signals::PEER_CLOSED, k as u64 + 1);
        }
        for (&k, (ch, _)) in &clients {
            ws.add(ch.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { continue };
        for (key, observed) in ready {
            if key >= 1 && key as usize <= listeners.len() {
                let (l, ns, _) = &listeners[key as usize - 1];
                while let Some(ch) = vproto::accept(l) {
                    clients.insert(next, (ch, *ns));
                    next += 1;
                }
                continue;
            }
            if observed & signals::READABLE != 0 {
                while let Some((Ok(msg), ns)) = clients.get(&key).map(|(ch, n)| (ch.read(), *n)) {
                    let serial = listeners.iter().find(|(_, n, _)| *n == ns).map(|(_, _, s)| s.as_str()).unwrap_or("");
                    let mut server = Disk { c: &mut c, ns: &namespaces[ns], serial };
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
