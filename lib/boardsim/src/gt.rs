//! The GT of Intel's Gen12 GPUs, as `vigpu::render` drives it: forcewake
//! and reset handshakes, the global address table, and two engines (render
//! and copy) that take contexts from their execlist submit queues and run
//! their rings: cache flushes, register loads and semaphore waits taken as
//! done, batches run from the context's address space (walking its page
//! tables), numbers written where the commands say, interrupts raised.
//!
//! Batches here hold `MI_STORE_DATA_IMM` and `MI_BATCH_BUFFER_END` only: a
//! test's batch writes a value into a buffer, which shows the batch ran
//! where the driver mapped it. A test can make an engine hang.
//!
//! Memory is [`Memory`]: pages the driver allocates through
//! `vigpu::render::Dma` and the model reads by physical address.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use vigpu::gtregs::*;
use vigpu::lrc::{self, ctx};
use vigpu::render::{Dma, Region, Time};

const PAGE: u64 = 4096;

#[derive(Default)]
struct MemState {
    /// Regions by physical address: their words.
    regions: BTreeMap<u64, (*mut u32, usize)>,
    next: u64,
    allocated: usize,
}

/// Physical memory the driver and the model share.
#[derive(Clone, Default)]
pub struct Memory(Rc<RefCell<MemState>>);

/// A region of [`Memory`].
pub struct SimRegion {
    phys: u64,
    words: Box<[u32]>,
    mem: Memory,
}

impl Region for SimRegion {
    fn phys(&self) -> u64 {
        self.phys
    }

    fn words(&mut self) -> &mut [u32] {
        &mut self.words
    }
}

impl Drop for SimRegion {
    fn drop(&mut self) {
        let mut m = self.mem.0.borrow_mut();
        m.regions.remove(&self.phys);
        m.allocated -= self.words.len() / 1024;
    }
}

impl Memory {
    /// `pages` of zeroed memory, physically contiguous (above 4 GiB, as
    /// memory often is).
    pub fn region(&self, pages: u32) -> SimRegion {
        let mut words = vec![0u32; pages as usize * 1024].into_boxed_slice();
        let mut m = self.0.borrow_mut();
        if m.next == 0 {
            m.next = 0x1_0000_0000;
        }
        let phys = m.next;
        // A page apart, so that a run past a region's end shows.
        m.next += (u64::from(pages) + 1) * PAGE;
        m.regions.insert(phys, (words.as_mut_ptr(), words.len()));
        m.allocated += pages as usize;
        SimRegion { phys, words, mem: self.clone() }
    }

    /// The pages allocated now.
    pub fn pages(&self) -> usize {
        self.0.borrow().allocated
    }

    fn word(&self, phys: u64) -> Option<*mut u32> {
        let m = self.0.borrow();
        let (&base, &(ptr, len)) = m.regions.range(..=phys).next_back()?;
        let i = ((phys - base) / 4) as usize;
        // SAFETY: inside the region, which lives while it is registered.
        (i < len).then(|| unsafe { ptr.add(i) })
    }

    pub fn read(&self, phys: u64) -> Option<u32> {
        // SAFETY: a word of a live region; the model is single-threaded.
        self.word(phys).map(|p| unsafe { p.read_volatile() })
    }

    pub fn write(&self, phys: u64, value: u32) -> bool {
        // SAFETY: as above.
        self.word(phys).map(|p| unsafe { p.write_volatile(value) }).is_some()
    }
}

/// [`Memory`] as the driver allocates it.
#[derive(Clone)]
pub struct SimDma(pub Memory);

impl Dma for SimDma {
    type Region = SimRegion;
    type Buffer = SimRegion;

    fn alloc(&mut self, pages: u32) -> Option<SimRegion> {
        Some(self.0.region(pages))
    }
}

/// Time for bringing up: a clock that moves only as the driver waits.
#[derive(Default)]
pub struct SimTime(pub Cell<u64>);

impl Time for SimTime {
    fn now(&self) -> u64 {
        self.0.get()
    }

    fn delay(&self, ns: u64) {
        self.0.set(self.0.get() + ns);
    }
}

/// An engine of the model.
#[derive(Default)]
struct Engine {
    /// The submit queue's port 0.
    sq: [u32; 4],
    /// The interrupts raised and not yet taken (`GT_RENDER_USER_INTERRUPT`
    /// and others).
    pending: u32,
    /// Hung: runs nothing more until reset.
    hung: bool,
    /// Runs nothing at all, reset or not (an engine that never comes up).
    dead: bool,
    /// What ran: batches' addresses in their space.
    batches: Vec<u64>,
    /// Contexts it loaded whole (restore inhibited).
    fresh_loads: u32,
    /// The root table of the address space it last ran in.
    root: u64,
}

/// The GT.
pub struct Gt {
    pub memory: Memory,
    regs: RefCell<BTreeMap<u32, u32>>,
    gtt: RefCell<BTreeMap<u64, u64>>,
    /// Where the global table's entries start in the BAR.
    gtt_base: u32,
    engines: RefCell<[Engine; 2]>,
    /// Engines told to hang at their next batch.
    hang_next: RefCell<[bool; 2]>,
    /// What went wrong, as the hardware would show it (or not).
    pub faults: RefCell<Vec<String>>,
    resets: RefCell<u32>,
}

impl Gt {
    pub fn new(memory: Memory, gtt_base: u32) -> Gt {
        let mut regs = BTreeMap::new();
        // One slice of six dual subslices, sixteen units each; a 19.2 MHz
        // crystal counted whole; frequencies 100 MHz to 1.4 GHz.
        regs.insert(SLICE_ENABLE, 1);
        regs.insert(DSS_ENABLE, 0x3F);
        regs.insert(EU_DISABLE, 0);
        regs.insert(RPM_CONFIG0, (1 << 3) | (3 << 1));
        regs.insert(RP_STATE_CAP, (2 << 16) | 28);
        Gt {
            memory,
            regs: RefCell::new(regs),
            gtt: RefCell::new(BTreeMap::new()),
            gtt_base,
            engines: RefCell::new([Engine::default(), Engine::default()]),
            hang_next: RefCell::new([false; 2]),
            faults: RefCell::new(Vec::new()),
            resets: RefCell::new(0),
        }
    }

    pub fn reg(&self, reg: u32) -> u32 {
        self.regs.borrow().get(&reg).copied().unwrap_or(0)
    }

    /// The batches engine `e` (0 render, 1 copy) ran.
    pub fn batches(&self, e: usize) -> Vec<u64> {
        self.engines.borrow()[e].batches.clone()
    }

    pub fn fresh_loads(&self, e: usize) -> u32 {
        self.engines.borrow()[e].fresh_loads
    }

    /// Makes engine `e` run nothing, ever.
    pub fn kill(&self, e: usize) {
        self.engines.borrow_mut()[e].dead = true;
    }

    /// Makes engine `e` hang at its next batch.
    pub fn hang_next(&self, e: usize) {
        self.hang_next.borrow_mut()[e] = true;
    }

    pub fn resets(&self) -> u32 {
        *self.resets.borrow()
    }

    /// The page table entry that maps `address` in the address space
    /// engine `e` last ran in (its PAT bits say how the GPU caches the
    /// page).
    pub fn page_entry(&self, e: usize, address: u64) -> Option<u64> {
        let mut table = self.engines.borrow()[e].root;
        for level in (1..=4).rev() {
            let i = (address >> (12 + 9 * (level - 1))) & 511;
            let lo = self.memory.read(table + i * 8)?;
            let hi = self.memory.read(table + i * 8 + 4)?;
            let entry = u64::from(lo) | u64::from(hi) << 32;
            if entry & 1 == 0 {
                return None;
            }
            if level == 1 {
                return Some(entry);
            }
            table = entry & 0x0000_FFFF_FFFF_F000;
        }
        None
    }

    fn fault(&self, what: String) {
        self.faults.borrow_mut().push(what);
    }

    /// A global table address's physical address.
    fn ggtt(&self, address: u64) -> Option<u64> {
        let pte = *self.gtt.borrow().get(&(address / PAGE))?;
        (pte & 1 != 0).then_some((pte & 0x0000_FFFF_FFFF_F000) + address % PAGE)
    }

    fn ggtt_read(&self, address: u64) -> Option<u32> {
        self.memory.read(self.ggtt(address)?)
    }

    fn ggtt_write(&self, address: u64, value: u32) {
        match self.ggtt(address) {
            Some(p) if self.memory.write(p, value) => {}
            _ => self.fault(format!("a write to {address:#x} of the global table, which maps no memory")),
        }
    }

    /// An address of a space whose root table is at `root`: its physical
    /// address (scratch pages included), walking four levels.
    fn ppgtt(&self, root: u64, address: u64) -> Option<u64> {
        let mut table = root;
        for level in (1..=4).rev() {
            let i = (address >> (12 + 9 * (level - 1))) & 511;
            let lo = self.memory.read(table + i * 8)?;
            let hi = self.memory.read(table + i * 8 + 4)?;
            let entry = u64::from(lo) | u64::from(hi) << 32;
            if entry & 1 == 0 {
                return None;
            }
            table = entry & 0x0000_FFFF_FFFF_F000;
        }
        Some(table + address % PAGE)
    }

    fn engine_of(&self, offset: u32) -> Option<(usize, u32)> {
        if (RENDER_BASE..RENDER_BASE + 0x1000).contains(&offset) {
            Some((0, offset - RENDER_BASE))
        } else if (BLT_BASE..BLT_BASE + 0x1000).contains(&offset) {
            Some((1, offset - BLT_BASE))
        } else {
            None
        }
    }

    /// The submit queue was loaded: the engine runs port 0's context.
    fn load(&self, e: usize) {
        let sq = self.engines.borrow()[e].sq;
        let desc = u64::from(sq[0]) | u64::from(sq[1]) << 32;
        if desc & 1 == 0 {
            return self.fault(format!("engine {e} loaded an invalid descriptor {desc:#x}"));
        }
        if desc & (3 << 3) != 3 << 3 || desc & (1 << 8) == 0 {
            return self.fault(format!("engine {e}: a descriptor not 64-bit and privileged: {desc:#x}"));
        }
        if self.engines.borrow()[e].hung || self.engines.borrow()[e].dead {
            return;
        }
        let image = desc & 0xFFFF_F000;
        let state = image + u64::from(lrc::STATE_OFFSET);
        let reg = |i: usize| self.ggtt_read(state + i as u64 * 4).unwrap_or(0);
        let base = if e == 0 { RENDER_BASE } else { BLT_BASE };
        if reg(ctx::RING_TAIL - 1) != base + 0x30 {
            return self.fault(format!("engine {e}: the context's image has no ring tail where it should"));
        }
        if reg(ctx::CONTEXT_CONTROL) & CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT != 0 {
            self.engines.borrow_mut()[e].fresh_loads += 1;
        }
        let ring = u64::from(reg(ctx::RING_START));
        let size = u64::from((reg(ctx::RING_CTL) & 0x1F_F000) + 4096);
        let root = u64::from(reg(ctx::PDP0_LDW)) | u64::from(reg(ctx::PDP0_UDW)) << 32;
        self.engines.borrow_mut()[e].root = root;
        let mut head = u64::from(reg(ctx::RING_HEAD));
        let tail = u64::from(reg(ctx::RING_TAIL));
        let mut guard = 0;
        while head != tail {
            guard += 1;
            if guard > 4096 {
                return self.fault(format!("engine {e}: a ring that never ends"));
            }
            let w = |i: u64| self.ggtt_read(ring + (head + i * 4) % size).unwrap_or(0);
            let op = w(0);
            let n = match op >> 29 {
                0 => self.mi(e, op, &w, image, root),
                3 if op >> 16 == 0x7A00 => self.pipe_control(e, &w, image),
                _ => {
                    self.fault(format!("engine {e}: an unknown command {op:#x}"));
                    return;
                }
            };
            let Some(n) = n else { return };
            head = (head + n * 4) % size;
        }
        // Done: the engine saves where it stopped.
        self.ggtt_write(state + ctx::RING_HEAD as u64 * 4, head as u32);
    }

    /// An `MI_*` command: how many dwords it took, or `None` to stop.
    fn mi(&self, e: usize, op: u32, w: &dyn Fn(u64) -> u32, image: u64, root: u64) -> Option<u64> {
        let opcode = (op >> 23) & 0x3F;
        let len = u64::from(op & 0xFF) + 2;
        Some(match opcode {
            0x00 | 0x05 | 0x08 => 1,
            // Latched only if enabled (render's in the upper half, copy's
            // in the lower).
            0x02 => {
                let enabled = self.reg(RENDER_COPY_INTR_ENABLE) >> if e == 0 { 16 } else { 0 };
                self.engines.borrow_mut()[e].pending |= GT_RENDER_USER_INTERRUPT & enabled;
                1
            }
            // A register load: the auxiliary table's invalidation is done
            // at once.
            0x22 => {
                for i in 0..(len - 1) / 2 {
                    let (reg, value) = (w(1 + 2 * i), w(2 + 2 * i));
                    let value = if reg == CCS_AUX_INV || reg == BCS0_AUX_INV { 0 } else { value };
                    self.regs.borrow_mut().insert(reg, value);
                }
                len
            }
            // A semaphore wait polling a register: met.
            0x1C => len,
            // MI_FLUSH_DW: a number stored, if it says so.
            0x26 => {
                if op & (1 << 14) != 0 {
                    let address = u64::from(w(1) & !7);
                    if op & (1 << 21) != 0 {
                        self.ggtt_write(image + address, w(3));
                    } else if w(1) & (1 << 2) != 0 {
                        self.ggtt_write(address, w(3));
                    } else {
                        self.fault(format!("engine {e}: a flush storing into a space"));
                    }
                }
                len
            }
            0x31 => {
                let batch = u64::from(w(1)) | u64::from(w(2)) << 32;
                if op & (1 << 8) == 0 {
                    self.fault(format!("engine {e}: a privileged batch"));
                    return None;
                }
                self.engines.borrow_mut()[e].batches.push(batch);
                if std::mem::take(&mut self.hang_next.borrow_mut()[e]) {
                    self.engines.borrow_mut()[e].hung = true;
                    return None;
                }
                self.batch(e, root, batch)?;
                len
            }
            _ => {
                self.fault(format!("engine {e}: an unknown command {op:#x}"));
                return None;
            }
        })
    }

    /// A `PIPE_CONTROL`: its quad word written, if it says so.
    fn pipe_control(&self, e: usize, w: &dyn Fn(u64) -> u32, image: u64) -> Option<u64> {
        let flags = w(1);
        if flags & (1 << 14) != 0 {
            let address = u64::from(w(2));
            if flags & (1 << 21) != 0 {
                self.ggtt_write(image + address, w(4));
                self.ggtt_write(image + address + 4, w(5));
            } else if flags & (1 << 24) != 0 {
                self.ggtt_write(address, w(4));
                self.ggtt_write(address + 4, w(5));
            } else {
                self.fault(format!("engine {e}: a pipe control writing into a space"));
            }
        }
        Some(6)
    }

    /// Runs a batch at `address` of the space at `root`.
    fn batch(&self, e: usize, root: u64, mut address: u64) -> Option<()> {
        for _ in 0..1024 {
            let w = |i: u64| self.ppgtt(root, address + i * 4).and_then(|p| self.memory.read(p));
            let Some(op) = w(0) else {
                self.fault(format!("engine {e}: a batch at {address:#x}, which its space does not map"));
                return None;
            };
            match op {
                // MI_BATCH_BUFFER_END
                0x0500_0000 => return Some(()),
                0 => address += 4,
                // MI_STORE_DATA_IMM, one dword, into the space.
                0x1000_0002 => {
                    let at = u64::from(w(1)?) | u64::from(w(2)?) << 32;
                    match self.ppgtt(root, at) {
                        Some(p) => {
                            self.memory.write(p, w(3)?);
                        }
                        None => self.fault(format!("engine {e}: a batch writing to unmapped {at:#x}")),
                    }
                    address += 16;
                }
                op => {
                    self.fault(format!("engine {e}: a batch command {op:#x}"));
                    return None;
                }
            }
        }
        self.fault(format!("engine {e}: a batch that never ends"));
        None
    }

    /// The GT's interrupt banks: which engines have something (bank 0).
    fn intr_dw0(&self) -> u32 {
        let eng = self.engines.borrow();
        let mut dw = 0;
        if eng[0].pending != 0 {
            dw |= 1 << INTR_BIT_RCS0;
        }
        if eng[1].pending != 0 {
            dw |= 1 << INTR_BIT_BCS;
        }
        dw
    }

    /// Whether an interrupt is pending (and the master enabled).
    pub fn interrupting(&self) -> bool {
        self.intr_dw0() != 0
    }
}

impl vigpu::Mmio for Gt {
    fn read(&self, offset: u32) -> u32 {
        if offset == vigpu::regs::MASTER_IRQ {
            let gt = if self.intr_dw0() != 0 { MASTER_IRQ_GT_DW0 } else { 0 };
            return self.reg(offset) | gt;
        }
        if offset == gt_intr_dw(0) {
            return self.intr_dw0();
        }
        if let Some((e, at)) = self.engine_of(offset) {
            // Ready to reset as soon as asked.
            if at == 0xD0 {
                let v = self.reg(offset);
                return if v & RESET_CTL_REQUEST_RESET != 0 { v | RESET_CTL_READY_TO_RESET } else { v };
            }
            let _ = e;
        }
        self.reg(offset)
    }

    fn write(&self, offset: u32, value: u32) {
        // Masked registers keep the bits the mask names.
        let masked = |old: u32| (old & !(value >> 16)) | (value & (value >> 16));
        match offset {
            o if o == FORCEWAKE_GT.request || o == FORCEWAKE_RENDER.request => {
                let new = masked(self.reg(o));
                self.regs.borrow_mut().insert(o, new);
                let ack = if o == FORCEWAKE_GT.request { FORCEWAKE_GT.ack } else { FORCEWAKE_RENDER.ack };
                self.regs.borrow_mut().insert(ack, new);
            }
            GDRST => {
                *self.resets.borrow_mut() += 1;
                let mut eng = self.engines.borrow_mut();
                for (e, bit) in [(0, GRDOM_RENDER), (1, GRDOM_BLT)] {
                    if value & (bit | GRDOM_FULL) != 0 {
                        eng[e].hung = false;
                        eng[e].pending = 0;
                    }
                }
                // Done at once.
                self.regs.borrow_mut().insert(GDRST, 0);
            }
            o if o == intr_identity(0) => {}
            o if o == iir_selector(0) => {
                let mut eng = self.engines.borrow_mut();
                let (e, class) = if value & (1 << INTR_BIT_RCS0) != 0 { (0, 0) } else { (1, 1) };
                let intr = std::mem::take(&mut eng[e].pending);
                self.regs.borrow_mut().insert(intr_identity(0), INTR_DATA_VALID | (class << 16) | intr);
            }
            o if o >= self.gtt_base => {
                let index = u64::from(o - self.gtt_base) / 8;
                let mut gtt = self.gtt.borrow_mut();
                let old = gtt.get(&index).copied().unwrap_or(0);
                gtt.insert(index, (old & !0xFFFF_FFFF) | u64::from(value));
            }
            o => {
                if let Some((e, at)) = self.engine_of(o) {
                    match at {
                        0x510..=0x51C => {
                            if at < 0x518 {
                                self.engines.borrow_mut()[e].sq[((at - 0x510) / 4) as usize] = value;
                            }
                            return;
                        }
                        0x550 => {
                            if value & EL_CTRL_LOAD != 0 {
                                self.load(e);
                            }
                            return;
                        }
                        0xD0 | 0x29C | 0x9C | 0x50 | 0xC4 | 0x244 => {
                            let new = masked(self.reg(o));
                            self.regs.borrow_mut().insert(o, new);
                            return;
                        }
                        _ => {}
                    }
                }
                self.regs.borrow_mut().insert(o, value);
            }
        }
    }

    fn read64(&self, offset: u32) -> u64 {
        if offset >= self.gtt_base {
            let index = u64::from(offset - self.gtt_base) / 8;
            return self.gtt.borrow().get(&index).copied().unwrap_or(0);
        }
        u64::from(self.read(offset)) | u64::from(self.read(offset + 4)) << 32
    }

    fn write64(&self, offset: u32, value: u64) {
        if offset >= self.gtt_base {
            let index = u64::from(offset - self.gtt_base) / 8;
            self.gtt.borrow_mut().insert(index, value);
            return;
        }
        self.write(offset, value as u32);
        self.write(offset + 4, (value >> 32) as u32);
    }
}
