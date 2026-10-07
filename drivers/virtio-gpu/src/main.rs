//! `virtio-gpu` — 3D rendering on the host's GPU.
//!
//! QEMU's virtio-gpu device in 3D mode runs virglrenderer, which takes
//! command streams in virgl's protocol (Gallium state, TGSI shaders, draws)
//! and replays them with the host's OpenGL. This driver owns the device and
//! serves the `gpu` protocol (`vproto::gpu`): every connection gets a virgl
//! context of its own, resources with handles the driver assigns, and a
//! block of memory shared with the device for its commands, transfers and
//! query results. The driver keeps contexts apart (a context sees only the
//! resources attached to it, which are only its own), bounds what each may
//! allocate and what all take together, and frees what a client leaves
//! behind when it goes away.
//!
//! Fences are asynchronous: a fenced, empty submission completes when the
//! host's GPU has done everything before it, and the driver then writes the
//! fence's number into the client's memory and signals its event, so that
//! clients wait for fences without calling.
//!
//! The device's displays are not used: Veda's display stays on the firmware
//! framebuffer (under QEMU the device is often the VGA display as well,
//! `virtio-vga-gl`, whose VGA side shows it), and 3D clients show their
//! frames in windows.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{Rights, signals};
use vipc::{Bytes, WaitSet};
use vproto::gpu::{GpuError, ResourceSpec, Session, gpu};
use vproto::pci::pcidev;
use vrt::object::{Channel, Event, Interrupt, Vmo};
use vrt::println;
use vrt::vm::Mapping;
use vvirtio::{Device, DmaBuffer, Segment, Virtqueue};

vrt::entry!(main);

/// Handle role of the PCI device channel from `devmgr`.
const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;

/// `VIRTIO_GPU_F_VIRGL`: 3D commands.
const F_VIRGL: u64 = 1 << 0;

// Control commands and responses.
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_GET_CAPSET_INFO: u32 = 0x0108;
const CMD_GET_CAPSET: u32 = 0x0109;
const CMD_CTX_CREATE: u32 = 0x0200;
const CMD_CTX_DESTROY: u32 = 0x0201;
const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
const CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const CMD_SUBMIT_3D: u32 = 0x0207;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_CAPSET_INFO: u32 = 0x1102;
const RESP_OK_CAPSET: u32 = 0x1103;
const FLAG_FENCE: u32 = 1;
/// `VIRTIO_GPU_CAPSET_VIRGL2`.
const CAPSET_VIRGL2: u32 = 2;

const QUEUE_SIZE: u16 = 256;
/// Layout of the driver's request buffer: request, then response.
const REQUEST: usize = 0;
const RESPONSE: usize = 2048;
/// Bytes per fence slot: a submission header, then its response.
const FENCE_SLOT: usize = 64;
const FENCE_SLOTS: usize = 64;
/// How long to wait for an interrupt before polling the queue again.
const POLL_NS: u64 = 20_000_000;

/// A session's memory: a page for the fence word, the command area, then
/// the shared area.
const HEADER_BYTES: usize = 4096;
const COMMAND_BYTES: usize = 1024 * 1024;
const MIN_SHARED: usize = 1024 * 1024;
const MAX_SHARED: usize = 64 * 1024 * 1024;

/// Limits per connection: resources, and the memory they may take on the
/// host (estimated generously: 16 bytes a texel, mipmaps included).
const MAX_RESOURCES: usize = 1 << 16;
const MAX_BYTES: u64 = 8 << 30;
/// What all sessions' memory may add up to: it is contiguous memory, and
/// each session is a context on the host too.
const MAX_SESSIONS_BYTES: usize = 512 * 1024 * 1024;

struct Gpu {
    queue: Virtqueue,
    irq: Option<Interrupt>,
    /// The driver's own synchronous requests.
    req: DmaBuffer,
    /// Fenced submissions in flight: their slots, by descriptor head.
    fence_mem: DmaBuffer,
    fence_slots: Vec<usize>,
    in_flight: BTreeMap<u16, Fenced>,
    next_fence: u64,
    caps: Vec<u8>,
    renderer: String,
    used_ids: BTreeSet<u32>,
    next_id: u32,
    next_ctx: u32,
    /// The memory of all sessions.
    sessions_bytes: usize,
    /// Keeps the device (and its mappings) alive.
    _dev: Device,
}

/// A fenced submission in flight.
struct Fenced {
    slot: usize,
    fence: u64,
    client: u64,
}

/// A connection.
struct Client {
    channel: Channel,
    ctx: Option<Context>,
}

/// A connection's virgl context.
struct Context {
    id: u32,
    /// The shared memory: the device sees it physically, both the driver
    /// and the client map it.
    _memory: Vmo,
    map: Mapping,
    phys: u64,
    shared_size: usize,
    fences: Event,
    /// Its resources, with their estimated size.
    resources: BTreeMap<u32, u64>,
    bytes: u64,
    /// The last fence of this context that signaled.
    signaled: u64,
}

fn hdr(ty: u32, flags: u32, fence: u64, ctx: u32) -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0..4].copy_from_slice(&ty.to_le_bytes());
    h[4..8].copy_from_slice(&flags.to_le_bytes());
    h[8..16].copy_from_slice(&fence.to_le_bytes());
    h[16..20].copy_from_slice(&ctx.to_le_bytes());
    h
}

/// A request: a header and little-endian words.
fn request(ty: u32, ctx: u32, words: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(24 + 4 * words.len());
    v.extend_from_slice(&hdr(ty, 0, 0, ctx));
    for w in words {
        v.extend_from_slice(&w.to_le_bytes());
    }
    v
}

impl Gpu {
    /// Sends a request and waits for its response; returns the response
    /// type. `data`, if any, follows the request (read by the device).
    fn call(&mut self, req: &[u8], data: Option<Segment>, response: usize, clients: &mut BTreeMap<u64, Client>) -> u32 {
        self.req.write(REQUEST, req);
        self.req.write(RESPONSE, &[0; 24]);
        let base = self.req.phys();
        let mut chain =
            alloc::vec![Segment { phys: base + REQUEST as u64, len: req.len() as u32, device_writes: false }];
        chain.extend(data);
        chain.push(Segment { phys: base + RESPONSE as u64, len: response as u32, device_writes: true });
        // A full queue drains as fences signal.
        let head = loop {
            if let Some(h) = self.queue.push(&chain) {
                break h;
            }
            self.wait_device();
        };
        self.queue.notify();
        loop {
            if self.reap(clients, Some(head)) {
                break;
            }
            self.wait_device();
        }
        // SAFETY: the device has written the response.
        let r = unsafe { self.req.bytes(RESPONSE, 4) };
        u32::from_le_bytes([r[0], r[1], r[2], r[3]])
    }

    /// Waits for the device's interrupt (or a while).
    fn wait_device(&mut self) {
        match &self.irq {
            Some(irq) => {
                let _ = irq.wait_irq(vrt::time::now_ns() + POLL_NS);
                let _ = irq.ack();
            }
            None => vrt::time::sleep(vrt::time::Duration::from_millis(1)),
        }
    }

    /// Takes completed requests: retires fences; returns whether `head`
    /// was among them.
    fn reap(&mut self, clients: &mut BTreeMap<u64, Client>, head: Option<u16>) -> bool {
        let mut found = false;
        while let Some((h, _)) = self.queue.pop_used() {
            if Some(h) == head {
                found = true;
            } else if let Some(f) = self.in_flight.remove(&h) {
                self.fence_slots.push(f.slot);
                if let Some(ctx) = clients.get_mut(&f.client).and_then(|c| c.ctx.as_mut()) {
                    ctx.signaled = ctx.signaled.max(f.fence);
                    // SAFETY: the fence word, in the mapping.
                    unsafe { core::ptr::write_volatile(ctx.map.as_ptr() as *mut u64, ctx.signaled) };
                    let _ = ctx.fences.signal();
                }
            }
        }
        found
    }

    fn ok(&mut self, req: &[u8], clients: &mut BTreeMap<u64, Client>) -> bool {
        self.call(req, None, 24, clients) == RESP_OK_NODATA
    }

    /// A fresh resource id.
    fn new_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if self.used_ids.insert(id) {
                return id;
            }
        }
    }

    /// Reads capability set 2 (virgl's).
    fn read_caps(&mut self, count: u32, clients: &mut BTreeMap<u64, Client>) -> Option<Vec<u8>> {
        for i in 0..count {
            let r = self.call(&request(CMD_GET_CAPSET_INFO, 0, &[i, 0]), None, 40, clients);
            if r != RESP_OK_CAPSET_INFO {
                continue;
            }
            // SAFETY: the device wrote the response.
            let info = unsafe { self.req.bytes(RESPONSE + 24, 12) }.to_vec();
            let w = |k: usize| u32::from_le_bytes([info[k], info[k + 1], info[k + 2], info[k + 3]]);
            let (id, version, size) = (w(0), w(4), w(8) as usize);
            if id != CAPSET_VIRGL2 || size == 0 || RESPONSE + 24 + size > self.req.len() {
                continue;
            }
            let r = self.call(&request(CMD_GET_CAPSET, 0, &[id, version]), None, 24 + size, clients);
            if r != RESP_OK_CAPSET {
                return None;
            }
            // SAFETY: the device wrote the response.
            return Some(unsafe { self.req.bytes(RESPONSE + 24, size) }.to_vec());
        }
        None
    }

    // ---- Requests from clients ------------------------------------------------

    fn open(
        &mut self,
        key: u64,
        shared: u64,
        dma: &vrt::object::Resource,
        clients: &mut BTreeMap<u64, Client>,
    ) -> Result<Session, GpuError> {
        if clients.get(&key).is_none_or(|c| c.ctx.is_some()) {
            return Err(GpuError::State);
        }
        // The largest memory that can be had, within what all sessions may
        // take, halving down to the minimum.
        let room = MAX_SESSIONS_BYTES.saturating_sub(self.sessions_bytes + HEADER_BYTES + COMMAND_BYTES) & !4095;
        if room < MIN_SHARED {
            return Err(GpuError::NoMemory);
        }
        let mut shared_size = (shared as usize).clamp(MIN_SHARED, MAX_SHARED).next_multiple_of(4096).min(room);
        let memory = loop {
            match Vmo::create_contiguous(dma, HEADER_BYTES + COMMAND_BYTES + shared_size) {
                Ok(v) => break v,
                Err(_) if shared_size > MIN_SHARED => shared_size = (shared_size / 2).max(MIN_SHARED),
                Err(_) => return Err(GpuError::NoMemory),
            }
        };
        let size = HEADER_BYTES + COMMAND_BYTES + shared_size;
        let phys = memory.phys_addr(0).map_err(|_| GpuError::NoMemory)?;
        let mine = Vmo::from_handle(memory.0.duplicate(None).map_err(|_| GpuError::NoMemory)?);
        let map =
            Mapping::new(mine, size, vabi::map_flags::READ | vabi::map_flags::WRITE).map_err(|_| GpuError::NoMemory)?;
        let fences = Event::create().map_err(|_| GpuError::NoMemory)?;
        let theirs_fences = Event::from_handle(
            fences
                .0
                .duplicate(Some(Rights(Rights::TRANSFER.0 | Rights::WAIT.0 | Rights::SIGNAL.0)))
                .map_err(|_| GpuError::NoMemory)?,
        );
        let rights = Rights(Rights::TRANSFER.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::GET_INFO.0);
        let theirs = Vmo::from_handle(memory.0.duplicate(Some(rights)).map_err(|_| GpuError::NoMemory)?);
        let id = self.next_ctx;
        self.next_ctx += 1;
        let name = format!("veda-{key}");
        let mut req = request(CMD_CTX_CREATE, id, &[name.len() as u32, 0]);
        let mut debug = [0u8; 64];
        debug[..name.len()].copy_from_slice(name.as_bytes());
        req.extend_from_slice(&debug);
        if !self.ok(&req, clients) {
            return Err(GpuError::Unavailable);
        }
        self.sessions_bytes += size;
        if let Some(c) = clients.get_mut(&key) {
            c.ctx = Some(Context {
                id,
                _memory: memory,
                map,
                phys,
                shared_size,
                fences,
                resources: BTreeMap::new(),
                bytes: 0,
                signaled: 0,
            });
        }
        Ok(Session {
            caps: Bytes(self.caps.clone()),
            memory: theirs,
            command_offset: HEADER_BYTES as u64,
            command_size: COMMAND_BYTES as u64,
            shared_offset: (HEADER_BYTES + COMMAND_BYTES) as u64,
            shared_size: shared_size as u64,
            fence_offset: 0,
            fences: theirs_fences,
            renderer: self.renderer.clone(),
        })
    }

    fn create(
        &mut self,
        key: u64,
        spec: ResourceSpec,
        backing: (u64, u64),
        clients: &mut BTreeMap<u64, Client>,
    ) -> Result<u32, GpuError> {
        let Some(ctx) = clients.get(&key).and_then(|c| c.ctx.as_ref()) else { return Err(GpuError::State) };
        if !plausible(&spec) {
            return Err(GpuError::Invalid);
        }
        let size = estimate(&spec);
        if ctx.resources.len() >= MAX_RESOURCES || ctx.bytes.saturating_add(size) > MAX_BYTES {
            return Err(GpuError::NoMemory);
        }
        let (off, len) = backing;
        let backing = match (off, len) {
            (_, 0) => None,
            (o, l) if o.checked_add(l).is_some_and(|e| e <= ctx.shared_size as u64) => {
                Some((ctx.phys + (HEADER_BYTES + COMMAND_BYTES) as u64 + o, l as u32))
            }
            _ => return Err(GpuError::Invalid),
        };
        let ctx_id = ctx.id;
        let id = self.new_id();
        let s = spec;
        let words = [
            id,
            s.target,
            s.format,
            s.bind,
            s.width,
            s.height,
            s.depth,
            s.array_size,
            s.last_level,
            s.nr_samples,
            s.flags,
            0,
        ];
        if !self.ok(&request(CMD_RESOURCE_CREATE_3D, 0, &words), clients) {
            self.used_ids.remove(&id);
            return Err(GpuError::Invalid);
        }
        if let Some((addr, len)) = backing {
            let mut req = request(CMD_RESOURCE_ATTACH_BACKING, 0, &[id, 1]);
            req.extend_from_slice(&addr.to_le_bytes());
            req.extend_from_slice(&len.to_le_bytes());
            req.extend_from_slice(&0u32.to_le_bytes());
            if !self.ok(&req, clients) {
                self.unref(id, clients);
                return Err(GpuError::NoMemory);
            }
        }
        if !self.ok(&request(CMD_CTX_ATTACH_RESOURCE, ctx_id, &[id, 0]), clients) {
            self.unref(id, clients);
            return Err(GpuError::Invalid);
        }
        if let Some(ctx) = clients.get_mut(&key).and_then(|c| c.ctx.as_mut()) {
            ctx.resources.insert(id, size);
            ctx.bytes += size;
        }
        Ok(id)
    }

    fn unref(&mut self, id: u32, clients: &mut BTreeMap<u64, Client>) {
        self.ok(&request(CMD_RESOURCE_UNREF, 0, &[id, 0]), clients);
        self.used_ids.remove(&id);
    }

    fn destroy(&mut self, key: u64, id: u32, clients: &mut BTreeMap<u64, Client>) {
        let Some(ctx) = clients.get_mut(&key).and_then(|c| c.ctx.as_mut()) else { return };
        let Some(size) = ctx.resources.remove(&id) else { return };
        ctx.bytes -= size;
        let ctx_id = ctx.id;
        self.ok(&request(CMD_CTX_DETACH_RESOURCE, ctx_id, &[id, 0]), clients);
        self.unref(id, clients);
    }

    fn submit(&mut self, key: u64, words: u32, clients: &mut BTreeMap<u64, Client>) -> Result<(), GpuError> {
        let Some(ctx) = clients.get(&key).and_then(|c| c.ctx.as_ref()) else { return Err(GpuError::State) };
        let bytes = words as usize * 4;
        if bytes > COMMAND_BYTES {
            return Err(GpuError::Invalid);
        }
        if bytes == 0 {
            return Ok(());
        }
        let data = Segment { phys: ctx.phys + HEADER_BYTES as u64, len: bytes as u32, device_writes: false };
        let req = request(CMD_SUBMIT_3D, ctx.id, &[bytes as u32, 0]);
        match self.call(&req, Some(data), 24, clients) {
            RESP_OK_NODATA => Ok(()),
            _ => Err(GpuError::Invalid),
        }
    }

    /// Queues a fence (an empty fenced submission) for a context.
    fn fence(&mut self, key: u64, clients: &mut BTreeMap<u64, Client>) -> Result<u64, GpuError> {
        let Some(ctx_id) = clients.get(&key).and_then(|c| c.ctx.as_ref()).map(|c| c.id) else {
            return Err(GpuError::State);
        };
        let slot = loop {
            if let Some(s) = self.fence_slots.pop() {
                break s;
            }
            // Every slot is in flight: wait for one to come back.
            self.wait_device();
            self.reap(clients, None);
        };
        self.next_fence += 1;
        let fence = self.next_fence;
        let at = slot * FENCE_SLOT;
        let mut req = Vec::from(hdr(CMD_SUBMIT_3D, FLAG_FENCE, fence, ctx_id));
        req.extend_from_slice(&[0u8; 8]);
        self.fence_mem.write(at, &req);
        self.fence_mem.write(at + 32, &[0; 24]);
        let base = self.fence_mem.phys() + at as u64;
        let chain = [
            Segment { phys: base, len: 32, device_writes: false },
            Segment { phys: base + 32, len: 24, device_writes: true },
        ];
        let head = loop {
            if let Some(h) = self.queue.push(&chain) {
                break h;
            }
            self.wait_device();
            self.reap(clients, None);
        };
        self.queue.notify();
        self.in_flight.insert(head, Fenced { slot, fence, client: key });
        Ok(fence)
    }

    /// Frees everything a client had.
    fn close(&mut self, key: u64, clients: &mut BTreeMap<u64, Client>) {
        let Some(ctx) = clients.get_mut(&key).and_then(|c| c.ctx.take()) else { return };
        for &id in ctx.resources.keys() {
            self.ok(&request(CMD_CTX_DETACH_RESOURCE, ctx.id, &[id, 0]), clients);
            self.unref(id, clients);
        }
        self.ok(&request(CMD_CTX_DESTROY, ctx.id, &[]), clients);
        // Fences still in flight finish without it; its memory goes now
        // (the device has finished every command naming it).
        self.sessions_bytes -= HEADER_BYTES + COMMAND_BYTES + ctx.shared_size;
    }
}

/// An upper bound on the host memory a resource takes.
fn estimate(s: &ResourceSpec) -> u64 {
    if s.target == 0 {
        return u64::from(s.width);
    }
    let texels = u64::from(s.width) * u64::from(s.height) * u64::from(s.depth) * u64::from(s.array_size);
    let bytes = texels * 16 * u64::from(s.nr_samples.max(1));
    if s.last_level > 0 { bytes + bytes / 3 } else { bytes }
}

/// Whether a resource description is within what any host accepts (the
/// host checks the rest).
fn plausible(s: &ResourceSpec) -> bool {
    let buffer = s.target == 0;
    s.target <= 8
        && s.width >= 1
        && (if buffer { s.width <= 1 << 30 } else { s.width <= 16384 })
        && (1..=16384).contains(&s.height)
        && (1..=2048).contains(&s.depth)
        && (1..=2048).contains(&s.array_size)
        && s.last_level < 16
        && s.nr_samples <= 16
}

/// One client request, with the client's key for the server.
struct Request<'a> {
    gpu: &'a mut Gpu,
    key: u64,
    clients: &'a mut BTreeMap<u64, Client>,
    dma: &'a vrt::object::Resource,
}

impl gpu::Server for Request<'_> {
    fn open(&mut self, shared: u64) -> Result<Session, GpuError> {
        self.gpu.open(self.key, shared, self.dma, self.clients)
    }

    fn create(&mut self, spec: ResourceSpec, backing_offset: u64, backing_len: u64) -> Result<u32, GpuError> {
        self.gpu.create(self.key, spec, (backing_offset, backing_len), self.clients)
    }

    fn destroy(&mut self, resource: u32) {
        self.gpu.destroy(self.key, resource, self.clients);
    }

    fn submit(&mut self, words: u32) -> Result<(), GpuError> {
        self.gpu.submit(self.key, words, self.clients)
    }

    fn fence(&mut self) -> Result<u64, GpuError> {
        self.gpu.fence(self.key, self.clients)
    }
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
    let features = match dev.initialize(F_VIRGL) {
        Ok(f) => f,
        Err(e) => {
            println!("feature negotiation failed: {:?}", e);
            return 1;
        }
    };
    if features & F_VIRGL == 0 {
        println!(
            "device at {location} has no 3D support (QEMU: use virtio-vga-gl or virtio-gpu-gl-pci, with an OpenGL display)"
        );
        return 0;
    }
    let num_capsets = dev.cfg_read32(12);
    let Ok(queue) = dev.setup_queue(0, QUEUE_SIZE) else {
        println!("no control queue");
        return 1;
    };
    let (Ok(req), Ok(fence_mem)) =
        (DmaBuffer::new(dev.dma(), 64 * 1024), DmaBuffer::new(dev.dma(), FENCE_SLOT * FENCE_SLOTS))
    else {
        println!("out of DMA memory");
        return 1;
    };
    let irq = dev.msix_vector(Some(0)).ok();
    dev.driver_ok();
    let Ok(dma) = dev.dma().duplicate() else {
        println!("no DMA resource");
        return 1;
    };
    let mut gpu = Gpu {
        queue,
        irq,
        req,
        fence_mem,
        fence_slots: (0..FENCE_SLOTS).rev().collect(),
        in_flight: BTreeMap::new(),
        next_fence: 0,
        caps: Vec::new(),
        renderer: String::new(),
        used_ids: BTreeSet::new(),
        next_id: 1,
        next_ctx: 1,
        sessions_bytes: 0,
        _dev: dev,
    };
    let mut clients: BTreeMap<u64, Client> = BTreeMap::new();
    let Some(caps) = gpu.read_caps(num_capsets, &mut clients) else {
        println!("device at {location} reports no virgl capabilities");
        return 1;
    };
    // The host's renderer name (`virgl_caps_v2.renderer`).
    if caps.len() >= 760 {
        let name = &caps[696..760];
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        gpu.renderer = String::from_utf8_lossy(&name[..n]).into();
    }
    gpu.caps = caps;
    let listener = match vproto::register(gpu::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register {}: {:?}", gpu::NAME, e);
            return 1;
        }
    };
    println!("3D at {}: {}", location, gpu.renderer);

    let mut next = 1u64;
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE | signals::PEER_CLOSED, 0);
        if let Some(irq) = &gpu.irq {
            ws.add(irq.raw(), signals::SIGNALED, u64::MAX);
        }
        for (&k, c) in &clients {
            ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        // Fences in flight are looked for now and then even without an
        // interrupt.
        let deadline = if gpu.in_flight.is_empty() { vabi::DEADLINE_INFINITE } else { vrt::time::now_ns() + POLL_NS };
        let ready = ws.wait(deadline).unwrap_or_default();
        if let Some(irq) = &gpu.irq {
            let _ = irq.ack();
        }
        gpu.reap(&mut clients, None);
        for (key, observed) in ready {
            if key == u64::MAX {
                continue;
            }
            if key == 0 {
                while let Some(ch) = vproto::accept(&listener) {
                    clients.insert(next, Client { channel: ch, ctx: None });
                    next += 1;
                }
                continue;
            }
            if observed & signals::READABLE != 0 {
                loop {
                    let Some(Ok(msg)) = clients.get(&key).map(|c| c.channel.read()) else { break };
                    let mut r = Request { gpu: &mut gpu, key, clients: &mut clients, dma: &dma };
                    match gpu::dispatch(&mut r, msg) {
                        Ok(reply) => {
                            if let Some(c) = clients.get(&key) {
                                let _ = reply.send(&c.channel);
                            }
                        }
                        Err(e) => println!("bad request: {}", e),
                    }
                }
            }
            if observed & signals::PEER_CLOSED != 0 {
                gpu.close(key, &mut clients);
                clients.remove(&key);
            }
        }
    }
}
