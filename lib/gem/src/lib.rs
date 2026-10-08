//! `vgem` — the `gem` service (`vproto::gem`) around the GEM model
//! (`vigpu::gem`): its clients and their sessions, the page of the
//! engines' completed numbers every client maps, and the events that say
//! when they move.
//!
//! The GPU's driver supplies the engines and what is particular to them
//! ([`Driver`]): `intel-gpu` the GPU's render and copy engines, `gemsim`
//! engines that run nothing. So QEMU's tests, which run `gemsim`, run this
//! loop as the hardware does.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use vabi::{Rights, signals};
use vigpu::gem::{EngineId, Gem, GemError as ModelError, Hardware, Submission, Use};
use vipc::{Bytes, WaitSet};
use vproto::gem::{Engine, Exec, GemDevice, GemError, GemSession, Point, gem};
use vrt::object::{Channel, Event, Vmo};
use vrt::println;
use vrt::vm::Mapping;

/// The largest buffer a client may ask for.
pub const MAX_BUFFER: u64 = 1 << 32;

/// What a GPU's driver does beside the GEM model.
pub trait Driver {
    type Hardware: Hardware;

    /// A new buffer of `size` bytes, a multiple of the page size: its
    /// memory, which the model keeps until the GPU is done with it, and
    /// the memory object its client maps.
    fn buffer(&mut self, size: u64) -> Result<(<Self::Hardware as Hardware>::Memory, Vmo), GemError>;

    /// A buffer of `memory` the client holds (see `gem::import`): its
    /// memory as the model keeps it, and its size.
    fn import(&mut self, memory: Vmo) -> Result<(<Self::Hardware as Hardware>::Memory, u64), GemError> {
        let _ = memory;
        Err(GemError::Unsupported)
    }

    /// The render engine's timestamp counter.
    fn timestamp(&mut self) -> u64;

    /// A submission is about to go to the engines.
    fn submitting(&mut self) {}

    /// Looks at the engines (at `now`), before the model starts what may
    /// run now and frees what they are done with.
    fn look(&mut self, gem: &mut Gem<Self::Hardware>, now: u64);

    /// When to look again without being asked (a kernel deadline).
    fn deadline(&self, gem: &Gem<Self::Hardware>, now: u64) -> u64;
}

/// The rights a client gets to a buffer's memory, and to its session's
/// fence page and event.
pub const BUFFER_RIGHTS: Rights =
    Rights(Rights::TRANSFER.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::GET_INFO.0);
const FENCE_RIGHTS: Rights = Rights(Rights::TRANSFER.0 | Rights::READ.0 | Rights::MAP.0 | Rights::GET_INFO.0);
const PROGRESS_RIGHTS: Rights = Rights(Rights::TRANSFER.0 | Rights::WAIT.0 | Rights::SIGNAL.0);

fn error(e: ModelError) -> GemError {
    match e {
        ModelError::Invalid => GemError::Invalid,
        ModelError::NoMemory => GemError::NoMemory,
        ModelError::Lost => GemError::Lost,
    }
}

/// The client's handle of `vmo`, with [`BUFFER_RIGHTS`].
pub fn client_handle(vmo: &Vmo) -> Result<Vmo, GemError> {
    vmo.0.duplicate(Some(BUFFER_RIGHTS)).map(Vmo::from_handle).map_err(|_| GemError::NoMemory)
}

struct Client {
    channel: Channel,
    /// Signaled as the engines' numbers move (theirs to wait on).
    progress: Option<Event>,
}

struct Server<D: Driver> {
    gem: Gem<D::Hardware>,
    driver: D,
    device: GemDevice,
    clients: BTreeMap<u64, Client>,
    /// The engines' completed numbers, which every client maps.
    fences: Mapping,
    published: Vec<u64>,
}

impl<D: Driver> Server<D> {
    /// Looks at the engines, starts what may run, frees what the GPU is
    /// done with, writes the numbers that moved and tells the clients.
    fn look(&mut self) {
        let now = vrt::time::now_ns();
        self.driver.look(&mut self.gem, now);
        self.gem.poll();
        let mut moved = false;
        for e in 0..self.published.len() {
            let n = self.gem.completed(e);
            if n != self.published[e] {
                self.published[e] = n;
                // SAFETY: the fence page, a u64 per engine.
                unsafe { core::ptr::write_volatile((self.fences.as_ptr() as *mut u64).add(e), n) };
                moved = true;
            }
        }
        if moved {
            for c in self.clients.values() {
                if let Some(p) = &c.progress {
                    let _ = p.signal();
                }
            }
        }
    }
}

/// One client's request.
struct Request<'a, D: Driver> {
    server: &'a mut Server<D>,
    key: u64,
}

impl<D: Driver> gem::Server for Request<'_, D> {
    fn open(&mut self) -> Result<GemSession, GemError> {
        let s = &mut *self.server;
        let c = s.clients.get_mut(&self.key).ok_or(GemError::Invalid)?;
        if c.progress.is_some() {
            return Err(GemError::Invalid);
        }
        let progress = Event::create().map_err(|_| GemError::NoMemory)?;
        let theirs = progress.0.duplicate(Some(PROGRESS_RIGHTS)).map_err(|_| GemError::NoMemory)?;
        let fences = s.fences.vmo().0.duplicate(Some(FENCE_RIGHTS)).map_err(|_| GemError::NoMemory)?;
        c.progress = Some(progress);
        s.gem.connect(self.key);
        Ok(GemSession {
            device: s.device.clone(),
            fences: Vmo::from_handle(fences),
            progress: Event::from_handle(theirs),
        })
    }

    fn create(&mut self, size: u64, _flags: u32) -> Result<(u32, Vmo), GemError> {
        if size == 0 || size > MAX_BUFFER {
            return Err(GemError::Invalid);
        }
        let (memory, theirs) = self.server.driver.buffer(size.next_multiple_of(4096))?;
        let handle = self.server.gem.create(self.key, memory).map_err(error)?;
        Ok((handle, theirs))
    }

    fn close(&mut self, handle: u32) {
        self.server.gem.close(self.key, handle);
    }

    fn vm_create(&mut self) -> Result<u32, GemError> {
        self.server.gem.space_create(self.key).map_err(error)
    }

    fn vm_destroy(&mut self, vm: u32) {
        let _ = self.server.gem.space_destroy(self.key, vm);
    }

    fn context_create(&mut self, vm: u32, engines: Vec<Engine>) -> Result<u32, GemError> {
        let engines: Vec<EngineId> =
            engines.iter().map(|e| EngineId { class: e.class, instance: e.instance }).collect();
        self.server.gem.context_create(self.key, vm, &engines).map_err(error)
    }

    fn context_destroy(&mut self, context: u32) {
        self.server.gem.context_destroy(self.key, context);
    }

    fn execute(&mut self, exec: Exec) -> Result<Point, GemError> {
        let s = Submission {
            context: exec.context,
            engine: exec.engine,
            buffers: exec.buffers.iter().map(|b| Use { handle: b.handle, address: b.address }).collect(),
            batch_start: exec.batch_start,
            batch_len: exec.batch_len,
            waits: exec.waits.iter().map(|p| (p.engine, p.seqno)).collect(),
        };
        self.server.driver.submitting();
        let (engine, seqno) = self.server.gem.execute(self.key, &s).map_err(error)?;
        Ok(Point { engine, seqno })
    }

    fn timestamp(&mut self) -> Result<u64, GemError> {
        Ok(self.server.driver.timestamp())
    }

    fn reset_stats(&mut self, context: u32) -> Result<(u32, u32), GemError> {
        self.server.gem.reset_stats(self.key, context).map_err(error)
    }

    fn hwconfig(&mut self) -> Result<Bytes, GemError> {
        // Mesa knows Gfx12's GPUs from its own tables.
        Err(GemError::Unsupported)
    }

    fn import(&mut self, memory: Vmo) -> Result<(u32, u64), GemError> {
        let size = memory.size().map_err(|_| GemError::Invalid)? as u64;
        if size == 0 || size > MAX_BUFFER || !size.is_multiple_of(4096) {
            return Err(GemError::Invalid);
        }
        let (memory, size) = self.server.driver.import(memory)?;
        let handle = self.server.gem.create(self.key, memory).map_err(error)?;
        Ok((handle, size))
    }
}

/// Registers `gem` and serves it as `device`, with the model on
/// `hardware`; returns only if it cannot start.
pub fn serve<D: Driver>(device: GemDevice, hardware: D::Hardware, driver: D) {
    let Ok(fences) = Mapping::anonymous(4096) else {
        println!("no memory for the fence page");
        return;
    };
    let listener = match vproto::register(gem::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("cannot register {}: {:?}", gem::NAME, e);
            return;
        }
    };
    println!("serving {} as {}", gem::NAME, device.name);
    let published = vec![0; hardware.engines().len()];
    let mut s = Server { gem: Gem::new(hardware), driver, device, clients: BTreeMap::new(), fences, published };
    let mut next = 1u64;
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE, 0);
        for (&k, c) in &s.clients {
            ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        let deadline = s.driver.deadline(&s.gem, vrt::time::now_ns());
        let ready = ws.wait(deadline).unwrap_or_default();
        for (key, observed) in ready {
            if key == 0 {
                while let Some(ch) = vproto::accept(&listener) {
                    s.clients.insert(next, Client { channel: ch, progress: None });
                    next += 1;
                }
                continue;
            }
            if observed & signals::READABLE != 0 {
                loop {
                    let Some(Ok(msg)) = s.clients.get(&key).map(|c| c.channel.read()) else { break };
                    let reply = gem::dispatch(&mut Request { server: &mut s, key }, msg);
                    // What completed shows before the reply: a client that
                    // waits at once finds it done.
                    s.look();
                    match reply {
                        Ok(reply) => {
                            if let Some(c) = s.clients.get(&key) {
                                let _ = reply.send(&c.channel);
                            }
                        }
                        Err(e) => println!("bad request: {}", e),
                    }
                }
            }
            if observed & signals::PEER_CLOSED != 0 {
                s.gem.disconnect(key);
                s.clients.remove(&key);
            }
        }
        s.look();
    }
}
