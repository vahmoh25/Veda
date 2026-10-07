//! Registers of the GPU's engines and of the GT around them (Gen12: Tiger
//! Lake to Raptor Lake), by the names Linux's i915 driver gives them
//! (`intel_gt_regs.h`, `intel_engine_regs.h`, `i915_reg.h`). Offsets are in
//! the GPU's first BAR.

/// A register written with a mask: the upper 16 bits say which of the
/// lower 16 the write changes (i915's `_MASKED_FIELD`).
pub const fn masked(mask: u32, value: u32) -> u32 {
    (mask << 16) | value
}

/// Sets `bits` of a masked register (`_MASKED_BIT_ENABLE`).
pub const fn masked_on(bits: u32) -> u32 {
    masked(bits, bits)
}

/// Clears `bits` of a masked register (`_MASKED_BIT_DISABLE`).
pub const fn masked_off(bits: u32) -> u32 {
    masked(bits, 0)
}

// ---- forcewake -------------------------------------------------------------

/// A power domain the driver keeps awake while it uses the registers in it:
/// where it asks, and where the GPU acknowledges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Domain {
    pub name: &'static str,
    pub request: u32,
    pub ack: u32,
}

/// `FORCEWAKE_GT_GEN9` and its ack: the copy engine, MOCS, PAT, reset, power
/// management.
pub const FORCEWAKE_GT: Domain = Domain { name: "GT", request: 0xA188, ack: 0x130044 };
/// `FORCEWAKE_RENDER_GEN9` and its ack: the render engine and its units.
pub const FORCEWAKE_RENDER: Domain = Domain { name: "render", request: 0xA278, ack: 0x0D84 };
/// The driver's bit of a forcewake request (`FORCEWAKE_KERNEL`), and the
/// one that kicks the GPU when an ack does not come
/// (`FORCEWAKE_KERNEL_FALLBACK`).
pub const FORCEWAKE_KERNEL: u32 = 1 << 0;
pub const FORCEWAKE_KERNEL_FALLBACK: u32 = 1 << 15;

// ---- the GT ------------------------------------------------------------------

/// `RPM_CONFIG0`: the crystal clock and how the timestamp counts it.
pub const RPM_CONFIG0: u32 = 0x0D00;
/// `CTC_MODE`: whether the timestamp counts the crystal (0) or a divider
/// (1, `TIMESTAMP_OVERRIDE`).
pub const CTC_MODE: u32 = 0xA26C;
/// `GEN8_MCR_SELECTOR`: which slice and subslice reads of replicated
/// registers are steered to.
pub const MCR_SELECTOR: u32 = 0x0FDC;
pub const fn mcr_steering(slice: u32, subslice: u32) -> u32 {
    ((slice & 0xF) << 27) | ((subslice & 0x7) << 24)
}
pub const MCR_STEERING_MASK: u32 = (0xF << 27) | (0x7 << 24);
/// `GEN11_EU_DISABLE`: a bit per pair of units disabled in every subslice.
pub const EU_DISABLE: u32 = 0x9134;
/// `GEN11_GT_SLICE_ENABLE`, `GEN12_GT_GEOMETRY_DSS_ENABLE`.
pub const SLICE_ENABLE: u32 = 0x9138;
pub const DSS_ENABLE: u32 = 0x913C;
/// `GEN11_GT_VEBOX_VDBOX_DISABLE`.
pub const VEBOX_VDBOX_DISABLE: u32 = 0x9140;
/// `GEN12_PAT_INDEX(i)`.
pub const fn pat_index(i: u32) -> u32 {
    0x4800 + i * 4
}
/// PAT entries: write-back, write-combining, write-through, uncached
/// (`GEN8_PPAT_*`).
pub const PPAT_WB: u32 = 3;
pub const PPAT_WC: u32 = 1;
pub const PPAT_WT: u32 = 2;
pub const PPAT_UC: u32 = 0;
/// `GEN12_GLOBAL_MOCS(i)` and `GEN9_LNCFCMOCS(i)` (two entries each).
pub const fn global_mocs(i: u32) -> u32 {
    0x4000 + i * 4
}
pub const fn l3cc(i: u32) -> u32 {
    0xB020 + i * 4
}
/// `GEN6_GDRST`: resets the GT (`FULL`) or engines.
pub const GDRST: u32 = 0x941C;
pub const GRDOM_FULL: u32 = 1 << 0;
pub const GRDOM_RENDER: u32 = 1 << 1;
pub const GRDOM_BLT: u32 = 1 << 2;
/// Frequencies: the request (`GEN6_RPNSWREQ`, `GEN9_FREQUENCY`), whether
/// the power unit takes requests (`GEN6_RP_CONTROL`,
/// `GEN9_RPSWCTL_ENABLE`), what the GPU may run at (`GEN6_RP_STATE_CAP`,
/// in 50 MHz), and what it runs at (`GEN12_RPSTAT1`, `GEN12_CAGF`).
pub const RPNSWREQ: u32 = 0xA008;
pub const fn frequency_request(units: u32) -> u32 {
    units << 23
}
pub const RP_CONTROL: u32 = 0xA024;
pub const RPSWCTL_ENABLE: u32 = 2 << 9;
pub const RP_STATE_CAP: u32 = 0x14_5998;
pub const RPSTAT1: u32 = 0x13_81B4;
pub const fn rpstat1_frequency(v: u32) -> u32 {
    (v >> 11) & 0x1FF
}
/// Frequencies are in units of 50/3 MHz from Gen9 on.
pub const FREQ_UNIT_KHZ: u32 = 16_667;
pub const FREQ_SCALER: u32 = 3;

// ---- interrupts --------------------------------------------------------------

/// `GEN11_GFX_MSTR_IRQ`'s bits for the GT's interrupt banks
/// (`GEN11_GT_DW_IRQ`).
pub const MASTER_IRQ_GT_DW0: u32 = 1 << 0;
pub const MASTER_IRQ_GT_DW1: u32 = 1 << 1;
/// `GEN11_GT_INTR_DW(bank)`: which engines have something; bit 0 is the
/// render engine, 15 the copy engine (`GEN11_RCS0`, `GEN11_BCS`).
pub const fn gt_intr_dw(bank: u32) -> u32 {
    0x19_0018 + bank * 4
}
pub const INTR_BIT_RCS0: u32 = 0;
pub const INTR_BIT_BCS: u32 = 15;
/// `GEN11_IIR_REG_SELECTOR(bank)` and `GEN11_INTR_IDENTITY_REG(bank)`.
pub const fn iir_selector(bank: u32) -> u32 {
    0x19_0070 + bank * 4
}
pub const fn intr_identity(bank: u32) -> u32 {
    0x19_0060 + bank * 4
}
pub const INTR_DATA_VALID: u32 = 1 << 31;
/// `GEN11_RENDER_COPY_INTR_ENABLE`: the render engine's above, the copy
/// engine's below; the engines' masks (`GEN11_RCS0_RSVD_INTR_MASK`,
/// `GEN11_BCS_RSVD_INTR_MASK`, the engine's in the upper half).
pub const RENDER_COPY_INTR_ENABLE: u32 = 0x19_0030;
pub const VCS_VECS_INTR_ENABLE: u32 = 0x19_0034;
pub const RCS0_INTR_MASK: u32 = 0x19_0090;
pub const BCS_INTR_MASK: u32 = 0x19_00A0;
/// An engine's interrupts: a batch's `MI_USER_INTERRUPT`, a command
/// streamer error, a context switch.
pub const GT_RENDER_USER_INTERRUPT: u32 = 1 << 0;
pub const GT_CS_MASTER_ERROR_INTERRUPT: u32 = 1 << 3;
pub const GT_CONTEXT_SWITCH_INTERRUPT: u32 = 1 << 8;

// ---- engines -------------------------------------------------------------------

/// Where an engine's registers start (`RENDER_RING_BASE`, `BLT_RING_BASE`).
pub const RENDER_BASE: u32 = 0x2000;
pub const BLT_BASE: u32 = 0x22000;

pub const fn ring_tail(base: u32) -> u32 {
    base + 0x30
}
pub const fn ring_head(base: u32) -> u32 {
    base + 0x34
}
pub const fn ring_start(base: u32) -> u32 {
    base + 0x38
}
pub const fn ring_ctl(base: u32) -> u32 {
    base + 0x3C
}
pub const fn ring_psmi_ctl(base: u32) -> u32 {
    base + 0x50
}
pub const fn ring_acthd(base: u32) -> u32 {
    base + 0x74
}
pub const fn ring_hws_pga(base: u32) -> u32 {
    base + 0x80
}
pub const fn ring_hwstam(base: u32) -> u32 {
    base + 0x98
}
pub const fn ring_mi_mode(base: u32) -> u32 {
    base + 0x9C
}
pub const fn ring_imr(base: u32) -> u32 {
    base + 0xA8
}
pub const fn ring_eir(base: u32) -> u32 {
    base + 0xB0
}
pub const fn ring_emr(base: u32) -> u32 {
    base + 0xB4
}
pub const fn ring_esr(base: u32) -> u32 {
    base + 0xB8
}
pub const fn ring_cmd_cctl(base: u32) -> u32 {
    base + 0xC4
}
pub const fn ring_reset_ctl(base: u32) -> u32 {
    base + 0xD0
}
pub const fn ring_bbaddr(base: u32) -> u32 {
    base + 0x140
}
pub const fn ring_execlist_status(base: u32) -> u32 {
    base + 0x234
}
pub const fn ring_context_control(base: u32) -> u32 {
    base + 0x244
}
pub const fn ring_mode(base: u32) -> u32 {
    base + 0x29C
}
pub const fn ring_timestamp(base: u32) -> u32 {
    base + 0x358
}
pub const fn ring_context_status_ptr(base: u32) -> u32 {
    base + 0x3A0
}
pub const fn ring_ctx_timestamp(base: u32) -> u32 {
    base + 0x3A8
}
pub const fn ring_nopid(base: u32) -> u32 {
    base + 0x94
}
/// `RING_FORCE_TO_NONPRIV(base, i)`: registers a batch (which runs
/// unprivileged) may write after all; twelve slots.
pub const fn ring_force_to_nonpriv(base: u32, i: u32) -> u32 {
    base + 0x4D0 + i * 4
}
pub const NONPRIV_SLOTS: u32 = 12;
pub const NONPRIV_ACCESS_RD: u32 = 1 << 28;
pub const NONPRIV_RANGE_4: u32 = 1;
/// `RING_EXECLIST_SQ_CONTENTS` (two ports of two dwords) and
/// `RING_EXECLIST_CONTROL` (`EL_CTRL_LOAD`).
pub const fn ring_execlist_sq(base: u32) -> u32 {
    base + 0x510
}
pub const fn ring_execlist_control(base: u32) -> u32 {
    base + 0x550
}
pub const EL_CTRL_LOAD: u32 = 1 << 0;

/// `RING_MI_MODE` bits.
pub const MODE_IDLE: u32 = 1 << 9;
pub const STOP_RING: u32 = 1 << 8;
/// `RING_MODE_GEN7` bit: execlists, not the legacy ring
/// (`GEN11_GFX_DISABLE_LEGACY_MODE`).
pub const GFX_DISABLE_LEGACY_MODE: u32 = 1 << 3;
/// `RING_RESET_CTL` bits.
pub const RESET_CTL_CAT_ERROR: u32 = 1 << 2;
pub const RESET_CTL_READY_TO_RESET: u32 = 1 << 1;
pub const RESET_CTL_REQUEST_RESET: u32 = 1 << 0;
/// `RING_CTL`: the ring's size (`RING_CTL_SIZE`, in pages - 1) and valid.
pub const fn ring_ctl_size(bytes: u32) -> u32 {
    bytes - 4096
}
pub const RING_VALID: u32 = 1;
/// `RING_BBSTATE`: batches are in the per-process space.
pub const RING_BB_PPGTT: u32 = 1 << 5;
/// `RING_CONTEXT_CONTROL` bits.
pub const CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT: u32 = 1 << 0;
pub const CTX_CTRL_INHIBIT_SYN_CTX_SWITCH: u32 = 1 << 3;
/// `I915_ERROR_INSTRUCTION`: what `RING_EMR` lets interrupt.
pub const ERROR_INSTRUCTION: u32 = 1 << 0;

// ---- workarounds (`intel_workarounds.c`) ----------------------------------------

pub const CS_DEBUG_MODE1: u32 = 0x20EC;
pub const FF_DOP_CLOCK_GATE_DISABLE: u32 = 1 << 1;
pub const FF_SLICE_CS_CHICKEN1: u32 = 0x20E0;
pub const FFSC_PERCTX_PREEMPT_CTRL: u32 = 1 << 14;
pub const FF_THREAD_MODE: u32 = 0x20A0;
pub const FF_TESSELATION_DOP_GATE_DISABLE: u32 = 1 << 19;
pub const CS_CHICKEN1: u32 = 0x2580;
pub const PREEMPT_GPGPU_LEVEL_MASK: u32 = 0b110;
pub const PREEMPT_GPGPU_THREAD_GROUP_LEVEL: u32 = 0b010;
pub const PS_INVOCATION_COUNT: u32 = 0x2348;
pub const FF_MODE2: u32 = 0x6604;
pub const FF_MODE2_GS_TIMER_224: u32 = 224 << 24;
pub const FF_MODE2_TDS_TIMER_128: u32 = 4 << 16;
pub const COMMON_SLICE_CHICKEN1: u32 = 0x7010;
pub const HIZ_CHICKEN: u32 = 0x7018;
pub const HZ_DEPTH_TEST_LE_GE_OPT_DISABLE: u32 = 1 << 13;
pub const COMMON_SLICE_CHICKEN4: u32 = 0x7300;
pub const DISABLE_TDC_LOAD_BALANCING_CALC: u32 = 1 << 6;
pub const COMMON_SLICE_CHICKEN3: u32 = 0x7304;
pub const DISABLE_CPS_AWARE_COLOR_PIPE: u32 = 1 << 9;
pub const MISCCPCTL: u32 = 0x9424;
pub const DOP_CLOCK_GATE_RENDER_ENABLE: u32 = 1 << 1;
pub const DFR_RATIO_EN_AND_CHICKEN: u32 = 0x9550;
pub const DFR_DISABLE: u32 = 1 << 9;
pub const GARBCNTL: u32 = 0xB004;
pub const BUS_HASH_CTL_BIT_EXC: u32 = 1 << 7;
pub const SAMPLER_MODE: u32 = 0xE18C;
pub const ENABLE_SMALLPL: u32 = 1 << 15;
pub const INDIRECT_STATE_BASE_ADDR_OVERRIDE: u32 = 1 << 0;
pub const ROW_CHICKEN4: u32 = 0xE48C;
pub const DISABLE_TDL_PUSH: u32 = 1 << 9;
pub const ROW_CHICKEN2: u32 = 0xE4F4;
pub const DISABLE_EARLY_READ: u32 = 1 << 14;
pub const PUSH_CONST_DEREF_HOLD_DIS: u32 = 1 << 8;
pub const WAIT_FOR_EVENT_POWER_DOWN_DISABLE: u32 = 1 << 7;
pub const RC_SEMA_IDLE_MSG_DISABLE: u32 = 1 << 12;
/// `BLIT_CCTL`: the MOCS of copy commands that have no field for it.
pub const fn blit_cctl(base: u32) -> u32 {
    base + 0x204
}
pub const fn blit_cctl_mocs(index: u32) -> u32 {
    ((index << 1) << 8) | (index << 1)
}
pub const BLIT_CCTL_MASK: u32 = (0x7F << 8) | 0x7F;
/// `RING_CMD_CCTL`: the MOCS of what the command streamer reads and
/// writes without a field for it (`CMD_CCTL_MOCS_OVERRIDE`).
pub const fn cmd_cctl_mocs(index: u32) -> u32 {
    ((index << 1) << 7) | (index << 1)
}
pub const CMD_CCTL_MOCS_MASK: u32 = 0x3FFF;

/// `GEN12_CCS_AUX_INV` and `GEN12_BCS0_AUX_INV`: invalidating the table of
/// compressed surfaces' auxiliary data (`AUX_INV`).
pub const CCS_AUX_INV: u32 = 0x4208;
pub const BCS0_AUX_INV: u32 = 0x4248;
pub const AUX_INV: u32 = 1 << 0;
