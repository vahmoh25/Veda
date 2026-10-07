//! What an engine runs: contexts' images, and the commands of their rings
//! (Gen12, as Linux's i915 writes them: `intel_lrc.c`,
//! `gen8_engine_cs.c`).
//!
//! A context is a run of pages in the GPU's global table: a status page of
//! its own, then the image of its registers, which the engine loads when
//! it switches to the context and saves when it switches away, then two
//! pages of commands the engine runs as it loads the context (i915's
//! workaround batches). Its ring, apart, holds what each submission runs:
//! caches flushed and invalidated, the client's batch, then the
//! submission's number written where the driver reads it, and an
//! interrupt.
//!
//! As in i915, each engine first runs a golden context: its image tells
//! the engine not to load the rest of its state
//! (`CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT`), so it starts from the engine's
//! defaults, and its one submission sets the registers i915 sets in every
//! context (its context workarounds). The image the engine saves of it is
//! the engine's default state, which every other context starts as a copy
//! of, loaded whole.

/// Bytes in a page.
pub const PAGE: u32 = 4096;
/// The pages of a context's image (`GEN11_LR_CONTEXT_RENDER_SIZE`,
/// `GEN8_LR_CONTEXT_OTHER_SIZE`), its status page first; the workaround
/// batches' two pages follow.
pub const RENDER_IMAGE_PAGES: u32 = 14;
pub const OTHER_IMAGE_PAGES: u32 = 2;
/// A ring's bytes.
pub const RING_BYTES: u32 = 16 * 1024;
/// Where the register state starts in the image (`LRC_STATE_OFFSET`).
pub const STATE_OFFSET: u32 = PAGE;

/// Indices of the register state's values (`CTX_*`, `intel_lrc_reg.h`).
pub mod ctx {
    pub const CONTEXT_CONTROL: usize = 0x02 + 1;
    pub const RING_HEAD: usize = 0x04 + 1;
    pub const RING_TAIL: usize = 0x06 + 1;
    pub const RING_START: usize = 0x08 + 1;
    pub const RING_CTL: usize = 0x0A + 1;
    pub const BB_STATE: usize = 0x10 + 1;
    pub const PER_CTX_BB: usize = 0x12 + 1;
    pub const INDIRECT_CTX: usize = 0x14 + 1;
    pub const INDIRECT_CTX_OFFSET: usize = 0x16 + 1;
    pub const TIMESTAMP: usize = 0x22 + 1;
    pub const PDP0_UDW: usize = 0x30 + 1;
    pub const PDP0_LDW: usize = 0x32 + 1;
    /// The render engine's power and clock state (`CTX_R_PWR_CLK_STATE`).
    pub const R_PWR_CLK_STATE: usize = 0x42 + 1;
    /// Gen12's `RING_MI_MODE`, the batch's offset, `GPR0` and the render
    /// engine's `RING_CMD_BUF_CCTL` (`lrc_ring_mi_mode` and others).
    pub const MI_MODE: usize = 0x60 + 1;
    pub const BB_OFFSET: usize = 0x70 + 1;
    pub const GPR0: usize = 0x74 + 1;
    pub const CMD_BUF_CCTL: usize = 0xB6 + 1;
}

/// A step of a register state's layout (i915's `NOP`, `LRI`, `REG`).
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Dwords left as they are.
    Skip(usize),
    /// `MI_LOAD_REGISTER_IMM` of this many registers, posted or not.
    Lri(u32, bool),
    /// A register, by its offset from the engine's registers.
    Reg(u32),
}

use Step::{Lri, Reg, Skip};

/// The registers every engine's image starts with (`gen12_xcs_offsets`).
const COMMON: &[Step] = &[
    Skip(1),
    Lri(13, true),
    Reg(0x244),
    Reg(0x034),
    Reg(0x030),
    Reg(0x038),
    Reg(0x03C),
    Reg(0x168),
    Reg(0x140),
    Reg(0x110),
    Reg(0x1C0),
    Reg(0x1C4),
    Reg(0x1C8),
    Reg(0x180),
    Reg(0x2B4),
    Skip(5),
    Lri(9, true),
    Reg(0x3A8),
    Reg(0x28C),
    Reg(0x288),
    Reg(0x284),
    Reg(0x280),
    Reg(0x27C),
    Reg(0x278),
    Reg(0x274),
    Reg(0x270),
];

/// And the render engine's then (`gen12_rcs_offsets`).
const RENDER: &[Step] = &[
    Lri(3, true),
    Reg(0x1B0),
    Reg(0x5A8),
    Reg(0x5AC),
    Skip(6),
    Lri(1, false),
    Reg(0x0C8),
    Skip(3 + 9 + 1),
    Lri(51, true),
    Reg(0x588),
    Reg(0x588),
    Reg(0x588),
    Reg(0x588),
    Reg(0x588),
    Reg(0x588),
    Reg(0x028),
    Reg(0x09C),
    Reg(0x0C0),
    Reg(0x178),
    Reg(0x17C),
    Reg(0x358),
    Reg(0x170),
    Reg(0x150),
    Reg(0x154),
    Reg(0x158),
    Reg(0x41C),
    // GPR0..15, two dwords each: 0x600..0x67C.
    Reg(0x600),
    Reg(0x604),
    Reg(0x608),
    Reg(0x60C),
    Reg(0x610),
    Reg(0x614),
    Reg(0x618),
    Reg(0x61C),
    Reg(0x620),
    Reg(0x624),
    Reg(0x628),
    Reg(0x62C),
    Reg(0x630),
    Reg(0x634),
    Reg(0x638),
    Reg(0x63C),
    Reg(0x640),
    Reg(0x644),
    Reg(0x648),
    Reg(0x64C),
    Reg(0x650),
    Reg(0x654),
    Reg(0x658),
    Reg(0x65C),
    Reg(0x660),
    Reg(0x664),
    Reg(0x668),
    Reg(0x66C),
    Reg(0x670),
    Reg(0x674),
    Reg(0x678),
    Reg(0x67C),
    Reg(0x068),
    Reg(0x084),
    Skip(1),
];

// ---- commands ------------------------------------------------------------------

/// An `MI_*` command: its opcode and length (dwords - 2).
pub const fn mi(opcode: u32, len: u32) -> u32 {
    (opcode << 23) | len
}

pub mod cmd {
    use super::mi;

    pub const MI_NOOP: u32 = 0;
    pub const MI_USER_INTERRUPT: u32 = mi(0x02, 0);
    pub const MI_ARB_CHECK: u32 = mi(0x05, 0);
    pub const MI_ARB_ON_OFF: u32 = mi(0x08, 0);
    pub const MI_ARB_ENABLE: u32 = 1;
    pub const MI_ARB_DISABLE: u32 = 0;
    pub const MI_BATCH_BUFFER_END: u32 = mi(0x0A, 0);
    /// Gen12's semaphore wait with a token dword.
    pub const MI_SEMAPHORE_WAIT_TOKEN: u32 = mi(0x1C, 3);
    pub const MI_SEMAPHORE_REGISTER_POLL: u32 = 1 << 16;
    pub const MI_SEMAPHORE_POLL: u32 = 1 << 15;
    pub const MI_SEMAPHORE_SAD_EQ_SDD: u32 = 4 << 12;
    pub const fn mi_load_register_imm(n: u32) -> u32 {
        mi(0x22, 2 * n - 1)
    }
    pub const MI_LRI_FORCE_POSTED: u32 = 1 << 12;
    pub const MI_LRI_MMIO_REMAP_EN: u32 = 1 << 17;
    /// The register offsets are the engine's own (from Gen11).
    pub const MI_LRI_LRM_CS_MMIO: u32 = 1 << 19;
    pub const MI_FLUSH_DW: u32 = mi(0x26, 1);
    pub const MI_FLUSH_DW_STORE_INDEX: u32 = 1 << 21;
    pub const MI_INVALIDATE_TLB: u32 = 1 << 18;
    pub const MI_FLUSH_DW_CCS: u32 = 1 << 16;
    pub const MI_FLUSH_DW_OP_STOREDW: u32 = 1 << 14;
    pub const MI_FLUSH_DW_USE_GTT: u32 = 1 << 2;
    pub const MI_LOAD_REGISTER_MEM: u32 = mi(0x29, 2);
    pub const MI_SRM_LRM_GLOBAL_GTT: u32 = 1 << 22;
    pub const MI_LOAD_REGISTER_REG: u32 = mi(0x2A, 1);
    pub const MI_LRR_SOURCE_CS_MMIO: u32 = 1 << 18;
    pub const MI_BATCH_BUFFER_START: u32 = mi(0x31, 1);
    /// The batch is in the context's per-process space (and so runs
    /// unprivileged).
    pub const MI_BATCH_PPGTT: u32 = 1 << 8;
    /// `GFX_OP_PIPE_CONTROL(6)`.
    pub const PIPE_CONTROL: u32 = (3 << 29) | (3 << 27) | (2 << 24) | 4;
    pub const fn preparser_disable(on: bool) -> u32 {
        MI_ARB_CHECK | (1 << 8) | on as u32
    }
}

/// `PIPE_CONTROL` flags: the first dword's, then the second's.
pub mod pc {
    pub const HDC_PIPELINE_FLUSH: u32 = 1 << 9;

    pub const COMMAND_CACHE_INVALIDATE: u32 = 1 << 29;
    pub const TILE_CACHE_FLUSH: u32 = 1 << 28;
    pub const FLUSH_L3: u32 = 1 << 27;
    pub const GLOBAL_GTT: u32 = 1 << 24;
    pub const STORE_DATA_INDEX: u32 = 1 << 21;
    pub const CS_STALL: u32 = 1 << 20;
    pub const TLB_INVALIDATE: u32 = 1 << 18;
    pub const QW_WRITE: u32 = 1 << 14;
    pub const DEPTH_STALL: u32 = 1 << 13;
    pub const RENDER_TARGET_CACHE_FLUSH: u32 = 1 << 12;
    pub const INSTRUCTION_CACHE_INVALIDATE: u32 = 1 << 11;
    pub const TEXTURE_CACHE_INVALIDATE: u32 = 1 << 10;
    pub const FLUSH_ENABLE: u32 = 1 << 7;
    pub const DC_FLUSH_ENABLE: u32 = 1 << 5;
    pub const VF_CACHE_INVALIDATE: u32 = 1 << 4;
    pub const CONST_CACHE_INVALIDATE: u32 = 1 << 3;
    pub const STATE_CACHE_INVALIDATE: u32 = 1 << 2;
    pub const DEPTH_CACHE_FLUSH: u32 = 1 << 0;
}

use cmd::*;

/// Where a context's status page has room for writes nobody reads
/// (`LRC_PPHWSP_SCRATCH_ADDR`).
const SCRATCH: u32 = 0x34 * 4;

/// Commands written one after another.
pub struct Emitter<'a> {
    out: &'a mut [u32],
    at: usize,
}

impl<'a> Emitter<'a> {
    pub fn new(out: &'a mut [u32]) -> Emitter<'a> {
        Emitter { out, at: 0 }
    }

    pub fn len(&self) -> usize {
        self.at
    }

    pub fn is_empty(&self) -> bool {
        self.at == 0
    }

    pub fn emit(&mut self, words: &[u32]) {
        self.out[self.at..self.at + words.len()].copy_from_slice(words);
        self.at += words.len();
    }

    fn pipe_control(&mut self, dw0: u32, dw1: u32, address: u32) {
        self.emit(&[PIPE_CONTROL | dw0, dw1, address, 0, 0, 0]);
    }

    /// Pads with `MI_NOOP` to a multiple of `words`.
    pub fn align(&mut self, words: usize) {
        while !self.at.is_multiple_of(words) {
            self.emit(&[MI_NOOP]);
        }
    }
}

/// An engine of the two the driver uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Render,
    Copy,
}

impl Kind {
    /// Where its registers start.
    pub fn base(self) -> u32 {
        match self {
            Kind::Render => crate::gtregs::RENDER_BASE,
            Kind::Copy => crate::gtregs::BLT_BASE,
        }
    }

    /// The pages of a context's image on it, the workaround batches'
    /// excluded.
    pub fn image_pages(self) -> u32 {
        match self {
            Kind::Render => RENDER_IMAGE_PAGES,
            Kind::Copy => OTHER_IMAGE_PAGES,
        }
    }

    /// Its table of auxiliary data's invalidation register.
    fn aux_inv(self) -> u32 {
        match self {
            Kind::Render => crate::gtregs::CCS_AUX_INV,
            Kind::Copy => crate::gtregs::BCS0_AUX_INV,
        }
    }
}

/// Fills a register state (`regs`, a page, zeroed) with its layout: the
/// `MI_LOAD_REGISTER_IMM` headers and the registers' offsets
/// (`set_offsets`).
fn set_offsets(regs: &mut [u32], kind: Kind, close: bool) {
    let base = kind.base();
    let mut at = 0;
    let render: &[Step] = if kind == Kind::Render { RENDER } else { &[] };
    for step in COMMON.iter().chain(render) {
        match *step {
            Skip(n) => at += n,
            Lri(count, posted) => {
                regs[at] =
                    mi_load_register_imm(count) | MI_LRI_LRM_CS_MMIO | if posted { MI_LRI_FORCE_POSTED } else { 0 };
                at += 1;
            }
            Reg(offset) => {
                regs[at] = base + offset;
                at += 2;
            }
        }
    }
    if close {
        // The end of what the engine loads (with Gen11's end-of-context
        // bit).
        regs[at] = MI_BATCH_BUFFER_END | 1;
    }
}

/// The value of `R_PWR_CLK_STATE` that keeps `slices` slices powered
/// (`intel_sseu_make_rpcs`: Gen12 gates whole slices only).
pub fn rpcs(slices: u32) -> u32 {
    const RPCS_ENABLE: u32 = 1 << 31;
    const RPCS_S_CNT_ENABLE: u32 = 1 << 18;
    RPCS_ENABLE | RPCS_S_CNT_ENABLE | ((slices & 0x3F) << 12)
}

/// Where a context is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Its image's address in the global table (its status page's).
    pub image: u32,
    /// Its ring's.
    pub ring: u32,
    /// The physical address of its address space's root table.
    pub root: u64,
}

/// A new context's image (`image`: all of it, its workaround batches'
/// pages included) for an engine of `kind`, at `at`, `slices` of render
/// slices powered (`lrc_init_state`, `__lrc_init_regs`).
///
/// From `golden`, the engine's default state (an image the engine saved),
/// which it loads whole; without one, empty and with nothing to load but
/// its registers' list (the golden context itself).
pub fn init_image(image: &mut [u32], kind: Kind, at: Placement, slices: u32, golden: Option<&[u32]>) {
    let page = PAGE as usize / 4;
    let image_words = kind.image_pages() as usize * page;
    match golden {
        Some(g) => image[..image_words].copy_from_slice(&g[..image_words]),
        None => image[..image_words].fill(0),
    }
    // The context's own status page, clean.
    image[..page].fill(0);
    let inhibit = golden.is_none();
    let regs = &mut image[page..2 * page];
    set_offsets(regs, kind, inhibit);
    let control = crate::gtregs::CTX_CTRL_INHIBIT_SYN_CTX_SWITCH;
    let restore = crate::gtregs::CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT;
    regs[ctx::CONTEXT_CONTROL] = crate::gtregs::masked(control | restore, control | if inhibit { restore } else { 0 });
    regs[ctx::TIMESTAMP] = 0;
    regs[ctx::BB_OFFSET] = 0;
    regs[ctx::RING_START] = at.ring;
    regs[ctx::RING_HEAD] = 0;
    regs[ctx::RING_TAIL] = 0;
    regs[ctx::RING_CTL] = crate::gtregs::ring_ctl_size(RING_BYTES) | crate::gtregs::RING_VALID;
    regs[ctx::BB_STATE] = crate::gtregs::RING_BB_PPGTT;
    regs[ctx::PDP0_UDW] = (at.root >> 32) as u32;
    regs[ctx::PDP0_LDW] = at.root as u32;
    // Not stopped, the rest as saved (`__reset_stop_ring`).
    let stop = crate::gtregs::STOP_RING;
    regs[ctx::MI_MODE] = (regs[ctx::MI_MODE] & !stop) | (stop << 16);
    if kind == Kind::Render {
        regs[ctx::R_PWR_CLK_STATE] = rpcs(slices);
    }
    // The workaround batches, after the image: commands the engine runs as
    // it loads the context, then one it runs before each batch (empty).
    let wa = kind.image_pages() as usize * page;
    let wa_address = at.image + kind.image_pages() * PAGE;
    let mut e = Emitter::new(&mut image[wa..wa + page]);
    indirect_context(&mut e, kind, at.image);
    e.align(16);
    let words = e.len() as u32;
    image[wa + page] = MI_BATCH_BUFFER_END;
    let regs = &mut image[page..2 * page];
    // Indirect context: address | size in cache lines, at its default
    // offset in the image's restore.
    regs[ctx::INDIRECT_CTX] = wa_address | (words * 4 / 64);
    regs[ctx::INDIRECT_CTX_OFFSET] = 0xD << 6;
    const PER_CTX_BB_FORCE: u32 = 1 << 2;
    const PER_CTX_BB_VALID: u32 = 1 << 0;
    regs[ctx::PER_CTX_BB] = (wa_address + PAGE) | PER_CTX_BB_FORCE | PER_CTX_BB_VALID;
}

/// The commands the engine runs as it loads a context
/// (`gen12_emit_indirect_ctx_rcs` and `_xcs`): workarounds for registers
/// the engine restores wrong, and the auxiliary table invalidated.
fn indirect_context(e: &mut Emitter, kind: Kind, image: u32) {
    let state = image + STATE_OFFSET;
    const GPR0: u32 = 0x600;
    const CTX_TIMESTAMP: u32 = 0x3A8;
    const CMD_BUF_CCTL: u32 = 0x84;
    // The context's timestamp, loaded twice through GPR0.
    e.emit(&[
        MI_LOAD_REGISTER_MEM | MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO,
        GPR0,
        state + ctx::TIMESTAMP as u32 * 4,
        0,
    ]);
    for _ in 0..2 {
        e.emit(&[MI_LOAD_REGISTER_REG | MI_LRR_SOURCE_CS_MMIO | MI_LRI_LRM_CS_MMIO, GPR0, CTX_TIMESTAMP]);
    }
    if kind == Kind::Render {
        e.emit(&[
            MI_LOAD_REGISTER_MEM | MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO,
            GPR0,
            state + ctx::CMD_BUF_CCTL as u32 * 4,
            0,
        ]);
        e.emit(&[MI_LOAD_REGISTER_REG | MI_LRR_SOURCE_CS_MMIO | MI_LRI_LRM_CS_MMIO, GPR0, CMD_BUF_CCTL]);
    }
    // GPR0 back.
    e.emit(&[MI_LOAD_REGISTER_MEM | MI_SRM_LRM_GLOBAL_GTT | MI_LRI_LRM_CS_MMIO, GPR0, state + ctx::GPR0 as u32 * 4, 0]);
    aux_table_invalidate(e, kind);
    if kind == Kind::Render {
        // Wa_18022495364: the state cache invalidated.
        const CS_DEBUG_MODE2: u32 = 0x20D8;
        const INSTRUCTION_STATE_CACHE_INVALIDATE: u32 = 1 << 6;
        e.emit(&[
            mi_load_register_imm(1),
            CS_DEBUG_MODE2,
            crate::gtregs::masked_on(INSTRUCTION_STATE_CACHE_INVALIDATE),
        ]);
    }
}

/// Invalidates the auxiliary table of compressed surfaces, and waits until
/// the GPU has (`gen12_emit_aux_table_inv`).
fn aux_table_invalidate(e: &mut Emitter, kind: Kind) {
    let reg = kind.aux_inv();
    e.emit(&[mi_load_register_imm(1) | MI_LRI_MMIO_REMAP_EN, reg, crate::gtregs::AUX_INV]);
    e.emit(&[
        MI_SEMAPHORE_WAIT_TOKEN | MI_SEMAPHORE_REGISTER_POLL | MI_SEMAPHORE_POLL | MI_SEMAPHORE_SAD_EQ_SDD,
        0,
        reg,
        0,
        0,
    ]);
}

/// A register set by a context's first submission: written whole, or
/// (masked registers) the upper half saying which bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wa {
    pub reg: u32,
    pub value: u32,
}

/// The registers i915 sets in every context (`gen12_ctx_workarounds_init`
/// and `gen12_ctx_gt_mocs_init`); `uc_mocs` is the uncached MOCS entry.
pub fn context_workarounds(kind: Kind, uc_mocs: u32) -> alloc::vec::Vec<Wa> {
    use crate::gtregs::*;
    match kind {
        Kind::Copy => alloc::vec![Wa { reg: blit_cctl(BLT_BASE), value: blit_cctl_mocs(uc_mocs) }],
        Kind::Render => alloc::vec![
            Wa { reg: CS_CHICKEN1, value: masked(PREEMPT_GPGPU_LEVEL_MASK, PREEMPT_GPGPU_THREAD_GROUP_LEVEL) },
            Wa { reg: FF_MODE2, value: FF_MODE2_TDS_TIMER_128 | FF_MODE2_GS_TIMER_224 },
            Wa { reg: HIZ_CHICKEN, value: masked_on(HZ_DEPTH_TEST_LE_GE_OPT_DISABLE) },
            Wa { reg: COMMON_SLICE_CHICKEN4, value: masked_on(DISABLE_TDC_LOAD_BALANCING_CALC) },
            Wa { reg: COMMON_SLICE_CHICKEN3, value: masked_on(DISABLE_CPS_AWARE_COLOR_PIPE) },
        ],
    }
}

/// Flushes the engine's caches (`EMIT_FLUSH`) and, with `invalidate`,
/// invalidates them and its address translations (`EMIT_INVALIDATE`): the
/// auxiliary table goes with either (`gen12_emit_flush_rcs`, `_xcs`).
pub fn flush(e: &mut Emitter, kind: Kind, flush: bool, invalidate: bool) {
    match kind {
        Kind::Render => {
            // On these GPUs the auxiliary table is invalidated always, so
            // memory traffic stops first, flushing or not.
            let mut dw1 = pc::TILE_CACHE_FLUSH
                | pc::RENDER_TARGET_CACHE_FLUSH
                | pc::DEPTH_CACHE_FLUSH
                | pc::DEPTH_STALL
                | pc::DC_FLUSH_ENABLE
                | pc::FLUSH_ENABLE
                | pc::STORE_DATA_INDEX
                | pc::QW_WRITE
                | pc::CS_STALL;
            if flush {
                dw1 |= pc::FLUSH_L3;
            }
            e.pipe_control(pc::HDC_PIPELINE_FLUSH, dw1, SCRATCH);
            if invalidate {
                e.emit(&[preparser_disable(true)]);
                e.pipe_control(
                    0,
                    pc::COMMAND_CACHE_INVALIDATE
                        | pc::TLB_INVALIDATE
                        | pc::INSTRUCTION_CACHE_INVALIDATE
                        | pc::TEXTURE_CACHE_INVALIDATE
                        | pc::VF_CACHE_INVALIDATE
                        | pc::CONST_CACHE_INVALIDATE
                        | pc::STATE_CACHE_INVALIDATE
                        | pc::STORE_DATA_INDEX
                        | pc::QW_WRITE
                        | pc::CS_STALL,
                    SCRATCH,
                );
                aux_table_invalidate(e, kind);
                e.emit(&[preparser_disable(false)]);
            }
        }
        Kind::Copy => {
            if invalidate {
                e.emit(&[preparser_disable(true)]);
            }
            let mut c = (MI_FLUSH_DW + 1) | MI_FLUSH_DW_STORE_INDEX | MI_FLUSH_DW_OP_STOREDW;
            if invalidate {
                c |= MI_INVALIDATE_TLB | MI_FLUSH_DW_CCS;
            }
            e.emit(&[c, SCRATCH, 0, 0]);
            aux_table_invalidate(e, kind);
            if invalidate {
                e.emit(&[preparser_disable(false)]);
            }
        }
    }
}

/// What a submission runs: caches invalidated (and, with `workarounds`,
/// those registers set: the golden context's), the batch at `batch` in the
/// context's space if there is one, then caches flushed, `seqno`'s low
/// half written at `seqno_at` (the global table: 8-aligned) and an
/// interrupt raised (`gen12_emit_fini_breadcrumb_*`). The words, a
/// multiple of two (the ring's tail moves by 8 bytes).
pub fn request(e: &mut Emitter, kind: Kind, batch: Option<u64>, seqno: u32, seqno_at: u32, workarounds: Option<&[Wa]>) {
    flush(e, kind, false, true);
    if let Some(was) = workarounds.filter(|w| !w.is_empty()) {
        flush(e, kind, true, true);
        e.emit(&[mi_load_register_imm(was.len() as u32)]);
        for w in was {
            e.emit(&[w.reg, w.value]);
        }
        e.emit(&[MI_NOOP]);
        flush(e, kind, true, true);
    }
    // The batch, arbitration on around it (`gen8_emit_bb_start`).
    if let Some(batch) = batch {
        e.emit(&[
            MI_ARB_ON_OFF | MI_ARB_ENABLE,
            MI_BATCH_BUFFER_START | MI_BATCH_PPGTT,
            batch as u32,
            (batch >> 32) as u32,
            MI_ARB_ON_OFF | MI_ARB_DISABLE,
            MI_NOOP,
        ]);
    }
    match kind {
        Kind::Render => {
            e.pipe_control(
                pc::HDC_PIPELINE_FLUSH,
                pc::CS_STALL
                    | pc::TLB_INVALIDATE
                    | pc::TILE_CACHE_FLUSH
                    | pc::RENDER_TARGET_CACHE_FLUSH
                    | pc::DEPTH_CACHE_FLUSH
                    | pc::DC_FLUSH_ENABLE
                    | pc::FLUSH_ENABLE
                    | pc::FLUSH_L3
                    | pc::DEPTH_STALL,
                0,
            );
            // The number, as a quad word (its upper half 0).
            e.emit(&[
                PIPE_CONTROL,
                pc::FLUSH_ENABLE | pc::CS_STALL | pc::GLOBAL_GTT | pc::QW_WRITE,
                seqno_at,
                0,
                seqno,
                0,
            ]);
        }
        Kind::Copy => {
            e.emit(&[MI_FLUSH_DW + 1, 0, 0, 0]);
            e.emit(&[(MI_FLUSH_DW + 1) | MI_FLUSH_DW_OP_STOREDW, seqno_at | MI_FLUSH_DW_USE_GTT, 0, seqno]);
        }
    }
    e.emit(&[MI_USER_INTERRUPT, MI_ARB_ON_OFF | MI_ARB_ENABLE]);
    // `gen8_emit_wa_tail`: room for the engine to restore lightly.
    e.emit(&[MI_ARB_CHECK, MI_NOOP]);
    e.align(2);
}

/// The most words [`request`] writes.
pub const MAX_REQUEST_WORDS: usize = 256;

/// A context's descriptor, for the engine's submit queue
/// (`lrc_descriptor` and the context ID of `__execlists_schedule_in`):
/// its image, flags, its tag and the engine's class and instance.
pub fn descriptor(image: u32, tag: u32, class: u16, instance: u16, render: bool, force_restore: bool) -> u64 {
    const VALID: u64 = 1 << 0;
    const FORCE_RESTORE: u64 = 1 << 2;
    const LEGACY_64B_CONTEXT: u64 = 3 << 3;
    const PRIVILEGE: u64 = 1 << 8;
    const PRIORITY_NORMAL: u64 = 1 << 9;
    let mut d = u64::from(image) | VALID | LEGACY_64B_CONTEXT | PRIVILEGE;
    if render {
        d |= PRIORITY_NORMAL;
    }
    if force_restore {
        d |= FORCE_RESTORE;
    }
    let ccid = ((tag + 1) << 5) | (u32::from(instance) << 16) | (u32::from(class) << 29);
    d | u64::from(ccid) << 32
}
