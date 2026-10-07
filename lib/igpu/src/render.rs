//! The GPU's engines for the GEM protocol: [`Render`] is the
//! [`Hardware`](crate::gem::Hardware) behind `intel-gpu`'s `gem` service,
//! on the render and copy engines of a Gen12 GPU.
//!
//! * **Bringing up** resets the engines, sets the GT and each engine up as
//!   i915 does, and runs each engine's golden context ([`crate::lrc`]):
//!   the image the engine saves of it is the state every context starts
//!   from. That it runs is the first sign the engine works.
//! * **Address spaces** are four-level page tables ([`crate::ppgtt`]) in
//!   pages from a pool of physically contiguous memory.
//! * **Contexts** are a context image and a ring per engine, in a range of
//!   the global table the driver reserves.
//! * **Submissions** queue per engine and go to the engine one at a time
//!   (execlists, [`crate::gt::submit`]): each ends by writing its number
//!   into the engine's status page and interrupting, and the next goes
//!   when it is done. One at a time keeps the engine's submit queue simple
//!   (no preemption, no context switches to follow); the GPU's work is
//!   whole frames, not many small ones.
//! * **Hangs**: a submission that runs longer than the limit is lost, and
//!   the engine reset. Its context is lost for its client (i915's ban),
//!   which Mesa recovers from by making another.
//!
//! The registers are [`Mmio`]; memory comes from [`Dma`], the clock and
//! delays from the caller, so that host tests run all of it against a
//! model of the hardware.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec;
use alloc::vec::Vec;

use crate::Mmio;
use crate::gem::{EngineId, GemError, Hardware};
use crate::gt::{self, EngineState, Topology};
use crate::gtt::Gtt;
use crate::lrc::{self, Emitter, Kind, Placement, Wa};
use crate::ppgtt::{self, AddressSpace, TableMemory};

const PAGE: u64 = 4096;

/// Physically contiguous memory the GPU reaches, and the driver too.
pub trait Region {
    /// Its physical address.
    fn phys(&self) -> u64;
    /// The driver's view of it.
    fn words(&mut self) -> &mut [u32];
}

/// Where the engines' memory comes from.
pub trait Dma {
    type Region: Region;
    /// What keeps a buffer's pages (the driver's handle of the memory
    /// object the client maps).
    type Buffer;
    /// `pages` pages of zeroed, physically contiguous memory.
    fn alloc(&mut self, pages: u32) -> Option<Self::Region>;
}

/// A buffer's memory: its pages, and what keeps them.
pub struct BufferMemory<K> {
    pub pages: Vec<u64>,
    pub keep: K,
}

/// Time, for bringing up: the clock, and waiting.
pub trait Time {
    /// Nanoseconds, monotonic.
    fn now(&self) -> u64;
    /// Waits about `ns` nanoseconds (perhaps longer).
    fn delay(&self, ns: u64);
}

// ---- page tables ------------------------------------------------------------------

/// Pages for page tables, from chunks of contiguous memory.
pub struct TablePool<D: Dma> {
    dma: D,
    /// Chunks by physical address.
    chunks: BTreeMap<u64, D::Region>,
    free: Vec<u64>,
}

/// Pages a chunk has.
const CHUNK_PAGES: u32 = 64;

impl<D: Dma> TablePool<D> {
    pub fn new(dma: D) -> TablePool<D> {
        TablePool { dma, chunks: BTreeMap::new(), free: Vec::new() }
    }

    pub fn dma(&mut self) -> &mut D {
        &mut self.dma
    }

    /// The words of the page at `phys`.
    fn page(&mut self, phys: u64) -> Option<&mut [u32]> {
        let (&base, chunk) = self.chunks.range_mut(..=phys).next_back()?;
        let at = ((phys - base) / 4) as usize;
        let words = chunk.words();
        words.get_mut(at..at + PAGE as usize / 4)
    }
}

impl<D: Dma> TableMemory for TablePool<D> {
    fn alloc(&mut self) -> Option<u64> {
        if self.free.is_empty() {
            let chunk = self.dma.alloc(CHUNK_PAGES)?;
            let base = chunk.phys();
            self.free.extend((0..u64::from(CHUNK_PAGES)).rev().map(|i| base + i * PAGE));
            self.chunks.insert(base, chunk);
        }
        let page = self.free.pop()?;
        self.page(page)?.fill(0);
        Some(page)
    }

    fn free(&mut self, page: u64) {
        self.free.push(page);
    }

    fn write(&mut self, table: u64, index: usize, entry: u64) {
        if let Some(words) = self.page(table) {
            words[2 * index] = entry as u32;
            words[2 * index + 1] = (entry >> 32) as u32;
        }
    }

    fn fill(&mut self, table: u64, entry: u64) {
        if let Some(words) = self.page(table) {
            for pair in words.as_chunks_mut::<2>().0 {
                pair[0] = entry as u32;
                pair[1] = (entry >> 32) as u32;
            }
        }
    }
}

// ---- the global table's room -----------------------------------------------------

/// The driver's range of the global table, handed out in pages.
struct GgttRange {
    /// Free runs: start page -> pages.
    free: BTreeMap<u32, u32>,
}

impl GgttRange {
    fn new(start: u64, bytes: u64) -> GgttRange {
        let mut free = BTreeMap::new();
        free.insert((start / PAGE) as u32, (bytes / PAGE) as u32);
        GgttRange { free }
    }

    fn alloc(&mut self, pages: u32) -> Option<u32> {
        let (&start, &n) = self.free.iter().find(|&(_, &n)| n >= pages)?;
        self.free.remove(&start);
        if n > pages {
            self.free.insert(start + pages, n - pages);
        }
        Some(start * PAGE as u32)
    }

    fn free(&mut self, address: u32, pages: u32) {
        let mut start = address / PAGE as u32;
        let mut n = pages;
        // Merged with the runs on either side.
        if let Some((&before, &m)) = self.free.range(..start).next_back()
            && before + m == start
        {
            self.free.remove(&before);
            start = before;
            n += m;
        }
        if let Some(&m) = self.free.get(&(start + n)) {
            self.free.remove(&(start + n));
            n += m;
        }
        self.free.insert(start, n);
    }
}

// ---- engines and contexts ---------------------------------------------------------

/// A submission for an engine.
#[derive(Debug, Clone, Copy)]
struct Request {
    context: u32,
    batch: u64,
    seqno: u64,
}

/// What an engine runs now.
#[derive(Debug, Clone, Copy)]
struct Running {
    request: Request,
    /// When it went to the engine.
    since: u64,
}

struct Engine<R> {
    kind: Kind,
    id: EngineId,
    /// Its status page, and where the engine sees it.
    status: R,
    status_at: u32,
    queue: VecDeque<Request>,
    running: Option<Running>,
    completed: u64,
    /// Tags for contexts' IDs, in turn.
    tag: u32,
    hangs: u32,
    /// The engine's default state: its golden context's saved image.
    golden: Vec<u32>,
}

/// The status page's words: the submissions' numbers (a quad word), and
/// the golden context's.
const SEQNO_WORD: usize = (gt::hwsp::SEQNO / 4) as usize;
const GOLDEN_WORD: usize = SEQNO_WORD + 2;

impl<R: Region> Engine<R> {
    /// The status page's word `i`, as the engine wrote it.
    fn read(&mut self, i: usize) -> u32 {
        let p = &self.status.words()[i] as *const u32;
        // SAFETY: a word of the status page, which the GPU writes.
        unsafe { core::ptr::read_volatile(p) }
    }
}

/// A context's state on one engine.
struct Context<R> {
    engine: usize,
    /// Its image (and workaround batches), its ring; where they are in the
    /// global table.
    image: R,
    image_at: u32,
    image_pages: u32,
    ring: R,
    ring_at: u32,
    /// Where the next submission's commands go in the ring, in bytes.
    tail: u32,
    /// Not yet submitted: its first is loaded whole
    /// (`CTX_DESC_FORCE_RESTORE`).
    fresh: bool,
    /// Lost to a reset; submissions lost while running, and while waiting.
    lost: bool,
    lost_active: u32,
    lost_pending: u32,
    /// Given back: freed once the engine is surely done with it.
    dropped: Option<u64>,
}

/// An address space.
pub struct Space {
    tables: AddressSpace,
}

/// How long a submission may run before the engine is reset.
pub const HANG_NS: u64 = 4_000_000_000;
/// How long after its last submission a context given back is freed (the
/// engine saves the context once its ring is done).
const SETTLE_NS: u64 = 50_000_000;
/// How long the golden context may take, and how long the engine is given
/// to save it after.
const GOLDEN_NS: u64 = 500_000_000;
const SAVE_NS: u64 = 10_000_000;
/// How often bringing up looks.
const STEP_NS: u64 = 10_000;

/// What happened since the last look.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Events {
    /// The engines whose numbers moved.
    pub moved: bool,
    /// Engines reset after a hang: the engine, and its state before.
    pub hangs: Vec<(usize, EngineState)>,
}

/// Why bringing up failed, and the engine's state if one did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitError {
    pub what: &'static str,
    pub engine: Option<EngineState>,
}

impl core::fmt::Display for InitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.what)?;
        if let Some(s) = &self.engine {
            write!(f, " ({s})")?;
        }
        Ok(())
    }
}

fn failed(what: &'static str) -> InitError {
    InitError { what, engine: None }
}

/// The render and copy engines, as the GEM model drives them.
pub struct Render<M: Mmio, D: Dma> {
    mmio: M,
    tables: TablePool<D>,
    gtt: Gtt,
    room: GgttRange,
    engines: Vec<Engine<D::Region>>,
    ids: Vec<EngineId>,
    contexts: BTreeMap<u32, Context<D::Region>>,
    next_context: u32,
    /// Slices powered in render contexts.
    slices: u32,
    /// The clock, as the driver last told it.
    now: u64,
}

/// What the driver tells [`Render::new`] about the GPU.
#[derive(Debug, Clone, Copy)]
pub struct Setup {
    pub gtt: Gtt,
    /// The driver's range of the global table: start and bytes.
    pub ggtt_start: u64,
    pub ggtt_bytes: u64,
    pub topology: Topology,
    /// Whether the engines interrupt as submissions end (the caller takes
    /// the GT's interrupts), or only write their numbers, which the
    /// caller looks for ([`Render::poll`]).
    pub interrupts: bool,
}

impl<M: Mmio, D: Dma> Render<M, D> {
    /// Brings the GT up: the engines reset, the GT and each engine set up,
    /// their interrupts on or off, their golden contexts run. The caller
    /// holds forcewake for the GT and the render engine.
    ///
    /// Should an engine not run its golden context, it is reset, and the
    /// memory it was given is never freed: an engine that wakes up late
    /// finds it still there.
    pub fn new(mmio: M, dma: D, setup: Setup, time: &dyn Time) -> Result<Render<M, D>, InitError> {
        let mut tables = TablePool::new(dma);
        let mut room = GgttRange::new(setup.ggtt_start, setup.ggtt_bytes);
        if !gt::reset_engines(&mmio, &[Kind::Render, Kind::Copy]) {
            return Err(failed("the engines did not reset"));
        }
        time.delay(50_000);
        gt::init_gt(&mmio, &setup.topology);
        let mut engines = Vec::new();
        for (kind, class) in [(Kind::Render, 0u16), (Kind::Copy, 1u16)] {
            let mut status = tables.dma().alloc(1).ok_or(failed("no memory for a status page"))?;
            let status_at = room.alloc(1).ok_or(failed("no room in the global table"))?;
            setup.gtt.map(&mmio, u64::from(status_at), status.phys(), PAGE);
            gt::init_engine(&mmio, kind, status_at, status.words());
            engines.push(Engine {
                kind,
                id: EngineId { class, instance: 0 },
                status,
                status_at,
                queue: VecDeque::new(),
                running: None,
                completed: 0,
                tag: 0,
                hangs: 0,
                golden: Vec::new(),
            });
        }
        if setup.interrupts {
            gt::enable_gt_interrupts(&mmio);
        } else {
            gt::disable_gt_interrupts(&mmio);
        }
        let ids = engines.iter().map(|e| e.id).collect();
        let mut r = Render {
            mmio,
            tables,
            gtt: setup.gtt,
            room,
            engines,
            ids,
            contexts: BTreeMap::new(),
            next_context: 1,
            slices: setup.topology.slice_mask.count_ones().max(1),
            now: time.now(),
        };
        for e in 0..r.engines.len() {
            if let Err(why) = r.record_golden(e, time) {
                gt::reset_engines(&r.mmio, &[Kind::Render, Kind::Copy]);
                core::mem::forget(r);
                return Err(why);
            }
        }
        r.now = time.now();
        Ok(r)
    }

    /// Runs engine `e`'s golden context: a new context, nothing loaded, its
    /// one submission setting the context workarounds. What the engine
    /// saves of it is the default state of every context after.
    fn record_golden(&mut self, e: usize, time: &dyn Time) -> Result<(), InitError> {
        let kind = self.engines[e].kind;
        let space = AddressSpace::new(&mut self.tables).map_err(|_| failed("no memory for the golden context"))?;
        let id = self.make_context(e, space.root(), None).map_err(|_| failed("no memory for the golden context"))?;
        let was = lrc::context_workarounds(kind, gt::mocs::UNCACHED);
        let at = self.engines[e].status_at + (GOLDEN_WORD * 4) as u32;
        self.write_request(id, None, 1, at, Some(&was));
        let start = time.now();
        while self.engines[e].read(GOLDEN_WORD) != 1 {
            if time.now().saturating_sub(start) > GOLDEN_NS {
                let engine = Some(EngineState::read(&self.mmio, kind));
                return Err(InitError { what: "an engine did not run its golden context", engine });
            }
            time.delay(STEP_NS);
        }
        // Saved once its ring is done.
        time.delay(SAVE_NS);
        let words = (kind.image_pages() * lrc::PAGE / 4) as usize;
        let c = self.contexts.get_mut(&id).ok_or(failed("the golden context went"))?;
        self.engines[e].golden = c.image.words()[..words].to_vec();
        c.dropped = Some(0);
        space.destroy(&mut self.tables);
        Ok(())
    }

    pub fn mmio(&self) -> &M {
        &self.mmio
    }

    /// Whether any engine has work.
    pub fn busy(&self) -> bool {
        self.engines.iter().any(|e| e.running.is_some() || !e.queue.is_empty())
    }

    /// Whether [`poll`](Self::poll) has more to do: engines with work, or
    /// contexts given back and not yet freed.
    pub fn pending(&self) -> bool {
        self.busy() || self.contexts.values().any(|c| c.dropped.is_some())
    }

    /// Looks at the engines (after an interrupt, or now and then): what
    /// finished, and what hung; starts what is queued, and frees contexts
    /// given back that the engines are done with.
    pub fn poll(&mut self, now: u64) -> Events {
        self.now = now;
        let mut events = Events::default();
        for e in 0..self.engines.len() {
            if let Some(r) = self.engines[e].running {
                let written = self.engines[e].read(SEQNO_WORD);
                if written.wrapping_sub(r.request.seqno as u32) as i32 >= 0 {
                    self.engines[e].completed = r.request.seqno;
                    self.engines[e].running = None;
                    events.moved = true;
                } else if now.saturating_sub(r.since) > HANG_NS {
                    let state = EngineState::read(&self.mmio, self.engines[e].kind);
                    self.recover(e, r.request);
                    events.hangs.push((e, state));
                    events.moved = true;
                }
            }
            if self.engines[e].running.is_none() {
                self.start(e);
            }
        }
        // Contexts given back, once their engine has long been done.
        let settled: Vec<u32> = self
            .contexts
            .iter()
            .filter(|(_, c)| c.dropped.is_some_and(|at| now.saturating_sub(at) > SETTLE_NS))
            .filter(|&(&id, c)| self.engines[c.engine].running.is_none_or(|r| r.request.context != id))
            .map(|(&id, _)| id)
            .collect();
        for id in settled {
            if let Some(c) = self.contexts.remove(&id) {
                self.gtt.unmap(&self.mmio, u64::from(c.image_at), u64::from(c.image_pages) * PAGE);
                self.gtt.unmap(&self.mmio, u64::from(c.ring_at), u64::from(lrc::RING_BYTES));
                self.room.free(c.image_at, c.image_pages);
                self.room.free(c.ring_at, lrc::RING_BYTES / lrc::PAGE);
            }
        }
        events
    }

    /// Resets engine `e` after `lost` hung: its context is lost for its
    /// client, and the submission completes.
    fn recover(&mut self, e: usize, lost: Request) {
        let kind = self.engines[e].kind;
        gt::reset_engines(&self.mmio, &[kind]);
        let status_at = self.engines[e].status_at;
        gt::init_engine(&self.mmio, kind, status_at, self.engines[e].status.words());
        self.engines[e].hangs += 1;
        self.engines[e].completed = lost.seqno;
        self.engines[e].running = None;
        if let Some(c) = self.contexts.get_mut(&lost.context) {
            c.lost = true;
            c.lost_active += 1;
        }
    }

    /// Hands engine `e` its next submission, if it has one.
    fn start(&mut self, e: usize) {
        while let Some(r) = self.engines[e].queue.pop_front() {
            let lost = self.contexts.get_mut(&r.context).map(|c| {
                if c.lost {
                    c.lost_pending += 1;
                }
                c.lost
            });
            if lost != Some(false) {
                // Gone, or its state lost: nothing of it runs any more.
                self.engines[e].completed = r.seqno;
                continue;
            }
            let at = self.engines[e].status_at + gt::hwsp::SEQNO;
            self.write_request(r.context, Some(r.batch), r.seqno as u32, at, None);
            self.engines[e].running = Some(Running { request: r, since: self.now });
            return;
        }
    }

    /// Writes a submission's commands into its context's ring, and hands
    /// the context to its engine.
    fn write_request(&mut self, id: u32, batch: Option<u64>, seqno: u32, at: u32, workarounds: Option<&[Wa]>) {
        let Some(c) = self.contexts.get_mut(&id) else { return };
        let engine = &mut self.engines[c.engine];
        let kind = engine.kind;
        let mut words = [0u32; lrc::MAX_REQUEST_WORDS];
        let mut em = Emitter::new(&mut words);
        lrc::request(&mut em, kind, batch, seqno, at, workarounds);
        let n = em.len();
        // Into the ring, around its end if it must.
        let ring_words = (lrc::RING_BYTES / 4) as usize;
        let start = (c.tail / 4) as usize;
        let ring = c.ring.words();
        for (i, w) in words[..n].iter().enumerate() {
            ring[(start + i) % ring_words] = *w;
        }
        let tail = (((start + n) % ring_words) * 4) as u32;
        c.tail = tail;
        let regs_at = (lrc::STATE_OFFSET / 4) as usize;
        c.image.words()[regs_at + lrc::ctx::RING_TAIL] = tail;
        // Loaded whole the first time; after, should the engine still be
        // on the context, it takes the new tail (a "lite restore", which a
        // forced restore would undo by loading an image not yet saved).
        // The tail always moves on: around the ring's end is forward too.
        let force = c.fresh;
        c.fresh = false;
        engine.tag = (engine.tag + 1) % 62;
        let desc =
            lrc::descriptor(c.image_at, engine.tag, engine.id.class, engine.id.instance, kind == Kind::Render, force);
        // What the engine reads is written before it is told.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        gt::submit(&self.mmio, kind, desc);
    }

    /// A context on engine `e` in the space whose root table is at `root`,
    /// from the engine's golden image (or, without one, the golden context
    /// itself).
    fn make_context(&mut self, e: usize, root: u64, golden: Option<&[u32]>) -> Result<u32, GemError> {
        let kind = self.engines[e].kind;
        let image_pages = kind.image_pages() + 2;
        let ring_pages = lrc::RING_BYTES / lrc::PAGE;
        let mut image = self.tables.dma().alloc(image_pages).ok_or(GemError::NoMemory)?;
        let ring = self.tables.dma().alloc(ring_pages).ok_or(GemError::NoMemory)?;
        let image_at = self.room.alloc(image_pages).ok_or(GemError::NoMemory)?;
        let Some(ring_at) = self.room.alloc(ring_pages) else {
            self.room.free(image_at, image_pages);
            return Err(GemError::NoMemory);
        };
        self.gtt.map(&self.mmio, u64::from(image_at), image.phys(), u64::from(image_pages) * PAGE);
        self.gtt.map(&self.mmio, u64::from(ring_at), ring.phys(), u64::from(lrc::RING_BYTES));
        let at = Placement { image: image_at, ring: ring_at, root };
        lrc::init_image(image.words(), kind, at, self.slices, golden);
        let id = self.next_context;
        self.next_context = self.next_context.checked_add(1).ok_or(GemError::NoMemory)?;
        let c = Context {
            engine: e,
            image,
            image_at,
            image_pages,
            ring,
            ring_at,
            tail: 0,
            fresh: true,
            lost: false,
            lost_active: 0,
            lost_pending: 0,
            dropped: None,
        };
        self.contexts.insert(id, c);
        Ok(id)
    }

    /// The engines' hangs so far.
    pub fn hangs(&self) -> Vec<u32> {
        self.engines.iter().map(|e| e.hangs).collect()
    }
}

impl<M: Mmio, D: Dma> Hardware for Render<M, D> {
    type Memory = BufferMemory<<D as Dma>::Buffer>;
    type Space = Space;
    type Context = u32;

    fn engines(&self) -> &[EngineId] {
        &self.ids
    }

    fn space_size(&self) -> u64 {
        ppgtt::SIZE
    }

    fn size(&self, memory: &Self::Memory) -> u64 {
        memory.pages.len() as u64 * PAGE
    }

    fn new_space(&mut self) -> Result<Space, GemError> {
        let tables = AddressSpace::new(&mut self.tables).map_err(|_| GemError::NoMemory)?;
        Ok(Space { tables })
    }

    fn drop_space(&mut self, space: Space) {
        space.tables.destroy(&mut self.tables);
    }

    fn bind(&mut self, space: &mut Space, address: u64, memory: &Self::Memory) -> Result<(), GemError> {
        // Write-back (PAT 0), coherent with the processor's caches.
        let r = space.tables.map(&mut self.tables, address, &memory.pages, ppgtt::pat_bits(0));
        if r.is_err() {
            space.tables.unmap(&mut self.tables, address, memory.pages.len() as u64);
        }
        r.map_err(|e| match e {
            ppgtt::Error::NoMemory => GemError::NoMemory,
            ppgtt::Error::Invalid => GemError::Invalid,
        })
    }

    fn unbind(&mut self, space: &mut Space, address: u64, size: u64) {
        space.tables.unmap(&mut self.tables, address, size.div_ceil(PAGE));
    }

    fn new_context(&mut self, engine: usize, space: &Space) -> Result<u32, GemError> {
        if engine >= self.engines.len() {
            return Err(GemError::Invalid);
        }
        let golden = core::mem::take(&mut self.engines[engine].golden);
        let r = self.make_context(engine, space.tables.root(), Some(&golden));
        self.engines[engine].golden = golden;
        r
    }

    fn drop_context(&mut self, context: u32) {
        let now = self.now;
        if let Some(c) = self.contexts.get_mut(&context) {
            c.dropped = Some(now);
        }
    }

    fn run(&mut self, context: &mut u32, _space: &Space, batch: u64, _len: u32, seqno: u64) {
        let Some(e) = self.contexts.get(context).map(|c| c.engine) else { return };
        self.engines[e].queue.push_back(Request { context: *context, batch, seqno });
        if self.engines[e].running.is_none() {
            self.start(e);
        }
    }

    fn completed(&self, engine: usize) -> u64 {
        self.engines.get(engine).map_or(u64::MAX, |e| e.completed)
    }

    fn lost(&self, context: &u32) -> bool {
        self.contexts.get(context).is_some_and(|c| c.lost)
    }

    fn reset_stats(&self, context: &u32) -> (u32, u32) {
        self.contexts.get(context).map_or((0, 0), |c| (c.lost_active, c.lost_pending))
    }
}

/// The words of the golden context's submission on an engine of `kind`
/// (its context workarounds, and no batch), for tests and logs.
pub fn golden_request(kind: Kind, seqno_at: u32) -> Vec<u32> {
    let mut words = vec![0u32; lrc::MAX_REQUEST_WORDS];
    let was: Vec<Wa> = lrc::context_workarounds(kind, gt::mocs::UNCACHED);
    let mut em = Emitter::new(&mut words);
    lrc::request(&mut em, kind, None, 1, seqno_at, Some(&was));
    let n = em.len();
    words.truncate(n);
    words
}
