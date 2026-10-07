//! What a server of the GEM protocol (`vproto::gem`) keeps: each client's
//! buffers, address spaces (and where buffers are bound in them) and
//! contexts, and the engines' sequence numbers. Mesa's iris driver uses
//! it through the POSIX layer's DRM device, as it uses Linux's i915.
//!
//! The GPU itself is behind [`Hardware`]: its address spaces' page tables,
//! its contexts' state on each engine, and its engines (`intel-gpu`), or a
//! stand-in that runs nothing (`gemsim`), or a model in host tests.
//!
//! Submissions on an engine complete in order and are numbered from 1,
//! device-wide. A buffer a client closes while the GPU may still use it
//! stays bound until its last submission is done; one bound at an address
//! another holds is refused, so no submission can make the GPU use what
//! it was not given. Contexts and address spaces a client destroys, or
//! leaves behind, go once the GPU is done with them too.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GemError {
    /// An unknown handle, address space or context; a buffer where
    /// another is; a batch outside its buffer.
    Invalid,
    NoMemory,
    /// The GPU failed.
    Lost,
}

/// An engine: its class and instance (i915's numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineId {
    pub class: u16,
    pub instance: u16,
}

/// The GPU, as the model drives it.
pub trait Hardware {
    /// A buffer's memory.
    type Memory;
    /// An address space (its page tables).
    type Space;
    /// A context's state on one engine: what it keeps between submissions
    /// (on Intel's GPUs, the image of its registers and its ring).
    type Context;

    /// The engines, in the order of their sequence numbers.
    fn engines(&self) -> &[EngineId];
    /// The bytes of each address space.
    fn space_size(&self) -> u64;
    /// The bytes of a buffer's memory.
    fn size(&self, memory: &Self::Memory) -> u64;
    fn new_space(&mut self) -> Result<Self::Space, GemError>;
    /// Gives an address space back: nothing runs in it, and nothing is
    /// bound in it any more.
    fn drop_space(&mut self, space: Self::Space);
    /// Maps `memory` at `address` (page-aligned, inside the space, not
    /// overlapping anything mapped).
    fn bind(&mut self, space: &mut Self::Space, address: u64, memory: &Self::Memory) -> Result<(), GemError>;
    fn unbind(&mut self, space: &mut Self::Space, address: u64, size: u64);
    /// A context's state on `engine`, in `space`.
    fn new_context(&mut self, engine: usize, space: &Self::Space) -> Result<Self::Context, GemError>;
    /// Gives a context's state back, once the GPU is done with it.
    fn drop_context(&mut self, context: Self::Context);
    /// Runs the commands at `batch` (`len` bytes) in `context` (on its
    /// engine, in `space`); when they are done, that engine's completed
    /// number becomes `seqno`. It cannot fail: what the GPU loses (a reset,
    /// a context it cannot run) completes all the same.
    fn run(&mut self, context: &mut Self::Context, space: &Self::Space, batch: u64, len: u32, seqno: u64);
    /// Whether a context's state was lost to a reset of the GPU: it takes
    /// no more submissions (i915 bans contexts that do not recover).
    fn lost(&self, _context: &Self::Context) -> bool {
        false
    }
    /// How many of a context's submissions a reset lost: while running,
    /// and while waiting.
    fn reset_stats(&self, _context: &Self::Context) -> (u32, u32) {
        (0, 0)
    }
    /// The last sequence number the engine completed.
    fn completed(&self, engine: usize) -> u64;
}

const PAGE: u64 = 4096;

/// A buffer the GPU uses at an address of a context's space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub handle: u32,
    pub address: u64,
}

/// A submission (the protocol's `Exec`, its buffers' write flags aside).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub context: u32,
    /// The engine, by its index in the context's engines.
    pub engine: u32,
    /// The buffers; the first is the batch.
    pub buffers: Vec<Use>,
    pub batch_start: u32,
    pub batch_len: u32,
    /// Points that must have passed first: (engine, seqno).
    pub waits: Vec<(u32, u64)>,
}

struct Buffer<M> {
    memory: M,
    size: u64,
    /// Where it is bound: (space id, address).
    bound: Vec<(u32, u64)>,
    /// Its last submission on each engine.
    last: Vec<u64>,
}

struct Space<S> {
    space: S,
    /// What is bound: address -> (handle, size).
    bindings: BTreeMap<u64, (u32, u64)>,
    /// Destroyed by its client: forgotten once the GPU is done in it.
    dying: bool,
}

struct Context<C> {
    space: u32,
    /// Its engines: the device's engine of each, and the context's state
    /// there.
    slots: Vec<(usize, C)>,
    /// The last submission in each slot.
    last: Vec<u64>,
    /// Destroyed by its client: forgotten once the GPU is done with it.
    dying: bool,
}

/// A buffer closed while the GPU may use it: freed once these numbers
/// have passed.
struct Closing<M> {
    client: u64,
    memory: M,
    bound: Vec<(u32, u64, u64)>,
    last: Vec<u64>,
}

struct Client<H: Hardware> {
    buffers: BTreeMap<u32, Buffer<H::Memory>>,
    spaces: BTreeMap<u32, Space<H::Space>>,
    contexts: BTreeMap<u32, Context<H::Context>>,
    next: u32,
    /// Disconnected: forgotten once the GPU is done with what it left.
    gone: bool,
}

/// A submission waiting for others first.
struct Waiting {
    client: u64,
    context: u32,
    slot: usize,
    engine: usize,
    batch: u64,
    len: u32,
    seqno: u64,
    waits: Vec<(usize, u64)>,
}

/// The server's state.
pub struct Gem<H: Hardware> {
    hw: H,
    clients: BTreeMap<u64, Client<H>>,
    /// The last number given out on each engine.
    issued: Vec<u64>,
    waiting: Vec<Waiting>,
    closing: Vec<Closing<H::Memory>>,
}

impl<H: Hardware> Gem<H> {
    pub fn new(hw: H) -> Gem<H> {
        let n = hw.engines().len();
        Gem { hw, clients: BTreeMap::new(), issued: alloc::vec![0; n], waiting: Vec::new(), closing: Vec::new() }
    }

    pub fn hardware(&self) -> &H {
        &self.hw
    }

    pub fn hardware_mut(&mut self) -> &mut H {
        &mut self.hw
    }

    pub fn connect(&mut self, client: u64) {
        self.clients.insert(
            client,
            Client {
                buffers: BTreeMap::new(),
                spaces: BTreeMap::new(),
                contexts: BTreeMap::new(),
                next: 1,
                gone: false,
            },
        );
    }

    /// Forgets a client: what the GPU may still use stays until done.
    pub fn disconnect(&mut self, client: u64) {
        let Some(mut c) = self.clients.remove(&client) else { return };
        let handles: Vec<u32> = c.buffers.keys().copied().collect();
        for h in handles {
            if let Some(b) = c.buffers.remove(&h) {
                self.retire_buffer(client, &mut c, b);
            }
        }
        for x in c.contexts.values_mut() {
            x.dying = true;
        }
        for s in c.spaces.values_mut() {
            s.dying = true;
        }
        c.gone = true;
        self.clients.insert(client, c);
        self.poll();
    }

    /// The clients connected (not those only waiting for the GPU).
    pub fn clients(&self) -> usize {
        self.clients.values().filter(|c| !c.gone).count()
    }

    fn client(&mut self, client: u64) -> Result<&mut Client<H>, GemError> {
        self.clients.get_mut(&client).filter(|c| !c.gone).ok_or(GemError::Invalid)
    }

    /// A new buffer of `memory`; its handle.
    pub fn create(&mut self, client: u64, memory: H::Memory) -> Result<u32, GemError> {
        let size = self.hw.size(&memory);
        let engines = self.hw.engines().len();
        let c = self.client(client)?;
        let h = c.next;
        c.next = c.next.checked_add(1).ok_or(GemError::NoMemory)?;
        c.buffers.insert(h, Buffer { memory, size, bound: Vec::new(), last: alloc::vec![0; engines] });
        Ok(h)
    }

    pub fn close(&mut self, client: u64, handle: u32) {
        let Some(mut c) = self.clients.remove(&client) else { return };
        if let Some(b) = c.buffers.remove(&handle) {
            self.retire_buffer(client, &mut c, b);
        }
        self.clients.insert(client, c);
    }

    /// Unbinds a buffer now if the GPU is done with it, or once it is.
    fn retire_buffer(&mut self, client: u64, c: &mut Client<H>, b: Buffer<H::Memory>) {
        let done = b.last.iter().enumerate().all(|(e, &n)| self.hw.completed(e) >= n);
        let mut bound = Vec::new();
        for (sid, address) in b.bound {
            if done {
                if let Some(s) = c.spaces.get_mut(&sid) {
                    s.bindings.remove(&address);
                    self.hw.unbind(&mut s.space, address, b.size);
                }
            } else {
                bound.push((sid, address, b.size));
            }
        }
        if !done {
            self.closing.push(Closing { client, memory: b.memory, bound, last: b.last });
        }
    }

    pub fn space_create(&mut self, client: u64) -> Result<u32, GemError> {
        let space = self.hw.new_space()?;
        let c = match self.client(client) {
            Ok(c) => c,
            Err(e) => {
                self.hw.drop_space(space);
                return Err(e);
            }
        };
        let id = c.next;
        c.next = c.next.checked_add(1).ok_or(GemError::NoMemory)?;
        c.spaces.insert(id, Space { space, bindings: BTreeMap::new(), dying: false });
        Ok(id)
    }

    /// Forgets an address space no context uses: now, or once the GPU is
    /// done in it. Its buffers stay, unbound from it.
    pub fn space_destroy(&mut self, client: u64, id: u32) -> Result<(), GemError> {
        let c = self.client(client)?;
        if c.contexts.values().any(|x| x.space == id && !x.dying) {
            return Err(GemError::Invalid);
        }
        c.spaces.get_mut(&id).filter(|s| !s.dying).ok_or(GemError::Invalid)?.dying = true;
        self.reap();
        Ok(())
    }

    /// A context in space `space`, with engines by class and instance.
    pub fn context_create(&mut self, client: u64, space: u32, engines: &[EngineId]) -> Result<u32, GemError> {
        let device: Vec<EngineId> = self.hw.engines().to_vec();
        let mut map = Vec::with_capacity(engines.len());
        for e in engines {
            map.push(device.iter().position(|d| d == e).ok_or(GemError::Invalid)?);
        }
        let Gem { hw, clients, .. } = self;
        let c = clients.get_mut(&client).filter(|c| !c.gone).ok_or(GemError::Invalid)?;
        let s = c.spaces.get(&space).filter(|s| !s.dying).ok_or(GemError::Invalid)?;
        if map.is_empty() {
            return Err(GemError::Invalid);
        }
        let mut slots = Vec::with_capacity(map.len());
        for &engine in &map {
            match hw.new_context(engine, &s.space) {
                Ok(state) => slots.push((engine, state)),
                Err(e) => {
                    for (_, state) in slots {
                        hw.drop_context(state);
                    }
                    return Err(e);
                }
            }
        }
        let id = c.next;
        c.next = c.next.checked_add(1).ok_or(GemError::NoMemory)?;
        let last = alloc::vec![0; slots.len()];
        c.contexts.insert(id, Context { space, slots, last, dying: false });
        Ok(id)
    }

    /// Forgets a context: now, or once the GPU is done with it.
    pub fn context_destroy(&mut self, client: u64, id: u32) {
        if let Ok(c) = self.client(client)
            && let Some(x) = c.contexts.get_mut(&id)
        {
            x.dying = true;
            self.reap();
        }
    }

    /// Queues a submission: its engine (the device's index) and number.
    pub fn execute(&mut self, client: u64, s: &Submission) -> Result<(u32, u64), GemError> {
        let space_size = self.hw.space_size();
        let n_engines = self.hw.engines().len();
        let mut c = self.clients.remove(&client).ok_or(GemError::Invalid)?;
        let r =
            if c.gone { Err(GemError::Invalid) } else { self.execute_for(client, &mut c, s, space_size, n_engines) };
        self.clients.insert(client, c);
        r
    }

    fn execute_for(
        &mut self,
        client: u64,
        c: &mut Client<H>,
        s: &Submission,
        space_size: u64,
        n_engines: usize,
    ) -> Result<(u32, u64), GemError> {
        let ctx = c.contexts.get(&s.context).filter(|x| !x.dying).ok_or(GemError::Invalid)?;
        let slot = s.engine as usize;
        let (engine, state) = ctx.slots.get(slot).ok_or(GemError::Invalid)?;
        let engine = *engine;
        if self.hw.lost(state) {
            return Err(GemError::Lost);
        }
        let sid = ctx.space;
        let first = s.buffers.first().ok_or(GemError::Invalid)?;
        // Every buffer known, every address page-aligned and inside the
        // space, and no two of them overlapping.
        let mut ranges: Vec<(u64, u64, u32)> = Vec::with_capacity(s.buffers.len());
        for u in &s.buffers {
            let b = c.buffers.get(&u.handle).ok_or(GemError::Invalid)?;
            if !u.address.is_multiple_of(PAGE) || u.address.checked_add(b.size).is_none_or(|end| end > space_size) {
                return Err(GemError::Invalid);
            }
            ranges.push((u.address, u.address + b.size, u.handle));
        }
        ranges.sort_unstable();
        ranges.dedup();
        if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
            return Err(GemError::Invalid);
        }
        let batch = c.buffers.get(&first.handle).map(|b| b.size).unwrap_or(0);
        if u64::from(s.batch_start) + u64::from(s.batch_len) > batch
            || s.batch_len == 0
            || !s.batch_len.is_multiple_of(8)
        {
            return Err(GemError::Invalid);
        }
        let mut waits = Vec::with_capacity(s.waits.len());
        for &(e, n) in &s.waits {
            let e = e as usize;
            if e >= n_engines || n > self.issued[e] {
                return Err(GemError::Invalid);
            }
            waits.push((e, n));
        }
        // Bind what is not bound where the submission says; nothing else
        // of the space may be there (a closing buffer still is).
        let space = c.spaces.get_mut(&sid).ok_or(GemError::Invalid)?;
        for &(start, end, handle) in &ranges {
            if space.bindings.get(&start) == Some(&(handle, end - start)) {
                continue;
            }
            let clash = space.bindings.range(..end).next_back().is_some_and(|(&a, &(_, size))| a + size > start);
            if clash {
                return Err(GemError::Invalid);
            }
        }
        for &(start, end, handle) in &ranges {
            if space.bindings.get(&start) == Some(&(handle, end - start)) {
                continue;
            }
            let b = c.buffers.get_mut(&handle).ok_or(GemError::Invalid)?;
            // A buffer moved: unbind it where it was.
            if let Some(i) = b.bound.iter().position(|&(s2, _)| s2 == sid) {
                let (_, old) = b.bound.swap_remove(i);
                space.bindings.remove(&old);
                self.hw.unbind(&mut space.space, old, b.size);
            }
            self.hw.bind(&mut space.space, start, &b.memory)?;
            space.bindings.insert(start, (handle, b.size));
            b.bound.push((sid, start));
        }
        let seqno = self.issued[engine] + 1;
        let batch_address = first.address + u64::from(s.batch_start);
        let ready =
            waits.iter().all(|&(e, n)| self.hw.completed(e) >= n) && self.waiting.iter().all(|w| w.engine != engine);
        if ready {
            let space = &c.spaces.get(&sid).ok_or(GemError::Invalid)?.space;
            let ctx = c.contexts.get_mut(&s.context).ok_or(GemError::Invalid)?;
            self.hw.run(&mut ctx.slots[slot].1, space, batch_address, s.batch_len, seqno);
        } else {
            self.waiting.push(Waiting {
                client,
                context: s.context,
                slot,
                engine,
                batch: batch_address,
                len: s.batch_len,
                seqno,
                waits,
            });
        }
        // Numbered once it is the GPU's.
        self.issued[engine] = seqno;
        if let Some(ctx) = c.contexts.get_mut(&s.context) {
            ctx.last[slot] = seqno;
        }
        for u in &s.buffers {
            if let Some(b) = c.buffers.get_mut(&u.handle) {
                b.last[engine] = seqno;
            }
        }
        Ok((engine as u32, seqno))
    }

    /// Starts what was waiting and may now run, and frees what closed
    /// buffers, destroyed contexts and spaces, and clients that went away
    /// the GPU is done with. Call after numbers move.
    pub fn poll(&mut self) {
        loop {
            let ready = self.waiting.iter().position(|w| {
                w.waits.iter().all(|&(e, n)| self.hw.completed(e) >= n)
                    && !self.waiting.iter().any(|o| o.engine == w.engine && o.seqno < w.seqno)
            });
            let Some(i) = ready else { break };
            let w = self.waiting.remove(i);
            let Some(c) = self.clients.get_mut(&w.client) else { continue };
            let (Some(ctx), spaces) = (c.contexts.get_mut(&w.context), &c.spaces) else { continue };
            let Some(space) = spaces.get(&ctx.space) else { continue };
            self.hw.run(&mut ctx.slots[w.slot].1, &space.space, w.batch, w.len, w.seqno);
        }
        let mut i = 0;
        while i < self.closing.len() {
            let done = self.closing[i].last.iter().enumerate().all(|(e, &n)| self.hw.completed(e) >= n);
            if !done {
                i += 1;
                continue;
            }
            let x = self.closing.swap_remove(i);
            if let Some(c) = self.clients.get_mut(&x.client) {
                for (sid, address, size) in x.bound {
                    if let Some(s) = c.spaces.get_mut(&sid) {
                        s.bindings.remove(&address);
                        self.hw.unbind(&mut s.space, address, size);
                    }
                }
            }
            drop(x.memory);
        }
        self.reap();
    }

    /// Forgets the destroyed contexts and spaces the GPU is done with, and
    /// the clients that went away once nothing of theirs is left.
    fn reap(&mut self) {
        let Gem { hw, clients, closing, waiting, .. } = self;
        for (&client, c) in clients.iter_mut() {
            // Contexts first: a space goes once no context is in it.
            let done: Vec<u32> = c
                .contexts
                .iter()
                .filter(|&(&id, x)| {
                    x.dying
                        && !waiting.iter().any(|w| w.client == client && w.context == id)
                        && x.slots.iter().zip(&x.last).all(|(&(e, _), &n)| hw.completed(e) >= n)
                })
                .map(|(&id, _)| id)
                .collect();
            for id in done {
                if let Some(x) = c.contexts.remove(&id) {
                    for (_, state) in x.slots {
                        hw.drop_context(state);
                    }
                }
            }
            let done: Vec<u32> = c
                .spaces
                .iter()
                .filter(|&(&id, s)| {
                    s.dying
                        && !c.contexts.values().any(|x| x.space == id)
                        && !closing.iter().any(|x| x.client == client && x.bound.iter().any(|&(s, _, _)| s == id))
                        && !c.buffers.values().any(|b| {
                            b.bound.iter().any(|&(s, _)| s == id)
                                && b.last.iter().enumerate().any(|(e, &n)| hw.completed(e) < n)
                        })
                })
                .map(|(&id, _)| id)
                .collect();
            for id in done {
                let Some(mut space) = c.spaces.remove(&id) else { continue };
                for b in c.buffers.values_mut() {
                    b.bound.retain(|&(s, _)| s != id);
                }
                for (address, (_, size)) in core::mem::take(&mut space.bindings) {
                    hw.unbind(&mut space.space, address, size);
                }
                hw.drop_space(space.space);
            }
        }
        // Clients that went away, once nothing of theirs is left.
        clients.retain(|&id, c| {
            !c.gone || !c.contexts.is_empty() || !c.spaces.is_empty() || closing.iter().any(|x| x.client == id)
        });
    }

    /// The last number the engine completed.
    pub fn completed(&self, engine: usize) -> u64 {
        self.hw.completed(engine)
    }

    /// How many of a context's submissions resets lost: while running, and
    /// while waiting.
    pub fn reset_stats(&mut self, client: u64, id: u32) -> Result<(u32, u32), GemError> {
        let c = self.clients.get(&client).filter(|c| !c.gone).ok_or(GemError::Invalid)?;
        let x = c.contexts.get(&id).ok_or(GemError::Invalid)?;
        Ok(x.slots.iter().fold((0, 0), |(a, p), (_, state)| {
            let (a2, p2) = self.hw.reset_stats(state);
            (a + a2, p + p2)
        }))
    }
}
