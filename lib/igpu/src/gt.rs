//! The GT: the part of the GPU that runs commands. Bringing it up and
//! driving its engines as Linux's i915 does on Gen12 with execlists
//! (`intel_uncore.c`, `intel_gt.c`, `intel_workarounds.c`, `intel_mocs.c`,
//! `intel_execlists_submission.c`, `intel_reset.c`, `intel_gt_irq.c`,
//! `intel_rps.c`), for the two engines the driver uses: render and copy.
//!
//! Everything goes through [`Mmio`]; waits are counted in register reads
//! (`spins`), each of which takes a microsecond or so on the hardware, so
//! that host tests run them against a model.

use alloc::vec::Vec;

use crate::Mmio;
use crate::gtregs::*;
use crate::lrc::Kind;

/// How many reads of an acknowledgement before giving up (i915 waits 50
/// ms for forcewake, 700 us for a reset to be ready, 2 ms for a reset).
pub const ACK_SPINS: u32 = 50_000;

/// Waits until `(read(reg) & mask) == value`, at most `spins` reads.
pub fn wait_for(mmio: &impl Mmio, reg: u32, mask: u32, value: u32, spins: u32) -> bool {
    for _ in 0..spins {
        if mmio.read(reg) & mask == value {
            return true;
        }
        core::hint::spin_loop();
    }
    mmio.read(reg) & mask == value
}

// ---- forcewake ---------------------------------------------------------------

/// Keeps `d` awake: asks, and waits for the GPU to say it is
/// (`fw_domains_get_with_fallback`). Whether it did.
pub fn forcewake_get(mmio: &impl Mmio, d: Domain) -> bool {
    // A previous request's ack must have cleared first.
    if !wait_for(mmio, d.ack, FORCEWAKE_KERNEL, 0, ACK_SPINS) {
        kick(mmio, d, 0);
    }
    mmio.write(d.request, masked_on(FORCEWAKE_KERNEL));
    if wait_for(mmio, d.ack, FORCEWAKE_KERNEL, FORCEWAKE_KERNEL, ACK_SPINS) {
        return true;
    }
    kick(mmio, d, FORCEWAKE_KERNEL)
}

/// The fallback toggle, for an ack the GPU did not deliver
/// (`fw_domain_wait_ack_with_fallback`): whether the ack is `value` after.
fn kick(mmio: &impl Mmio, d: Domain, value: u32) -> bool {
    for _ in 0..10 {
        wait_for(mmio, d.ack, FORCEWAKE_KERNEL_FALLBACK, 0, ACK_SPINS);
        mmio.write(d.request, masked_on(FORCEWAKE_KERNEL_FALLBACK));
        wait_for(mmio, d.ack, FORCEWAKE_KERNEL_FALLBACK, FORCEWAKE_KERNEL_FALLBACK, ACK_SPINS);
        let ok = mmio.read(d.ack) & FORCEWAKE_KERNEL == value;
        mmio.write(d.request, masked_off(FORCEWAKE_KERNEL_FALLBACK));
        if ok {
            return true;
        }
    }
    false
}

/// Lets `d` sleep again.
pub fn forcewake_put(mmio: &impl Mmio, d: Domain) {
    mmio.write(d.request, masked_off(FORCEWAKE_KERNEL));
}

/// Clears whatever requests were left (`fw_domain_reset`: Gen12 keeps bit
/// 12).
pub fn forcewake_reset(mmio: &impl Mmio, d: Domain) {
    mmio.write(d.request, masked_off(0xEFFF));
}

// ---- what the GT is ----------------------------------------------------------

/// The execution units present (`gen12_sseu_info_init`): one slice, its
/// dual subslices, and the units of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topology {
    pub slice_mask: u32,
    pub dss_mask: u32,
    /// The units of each dual subslice: a bit each (the fuses disable them
    /// in pairs).
    pub eu_mask: u32,
}

impl Topology {
    pub fn read(mmio: &impl Mmio) -> Topology {
        let slice_mask = mmio.read(SLICE_ENABLE) & 0xFF;
        let dss_mask = mmio.read(DSS_ENABLE) & 0x3F;
        let disabled = mmio.read(EU_DISABLE) & 0xFF;
        let mut eu_mask = 0;
        for pair in 0..8 {
            if disabled & (1 << pair) == 0 {
                eu_mask |= 0b11 << (pair * 2);
            }
        }
        Topology { slice_mask: if slice_mask == 0 { 1 } else { slice_mask }, dss_mask, eu_mask }
    }

    pub fn eus(&self) -> u32 {
        self.dss_mask.count_ones() * self.eu_mask.count_ones()
    }
}

/// The command streamers' timestamp clock, in Hz
/// (`gen11_read_clock_frequency`); `None` if it counts a divider this
/// driver does not read.
pub fn timestamp_hz(mmio: &impl Mmio) -> Option<u64> {
    if mmio.read(CTC_MODE) & 1 != 0 {
        return None;
    }
    let c0 = mmio.read(RPM_CONFIG0);
    let crystal: u64 = match (c0 >> 3) & 0x7 {
        0 => 24_000_000,
        1 => 19_200_000,
        2 => 38_400_000,
        3 => 25_000_000,
        _ => return None,
    };
    let shift = (c0 >> 1) & 0x3;
    Some(crystal >> (3 - shift))
}

/// What frequencies the GT may run at, in units of 50/3 MHz
/// (`__gen6_rps_get_freq_caps`): the highest (RP0) and the lowest (RPn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frequencies {
    pub max: u32,
    pub min: u32,
}

impl Frequencies {
    pub fn read(mmio: &impl Mmio) -> Frequencies {
        let caps = mmio.read(RP_STATE_CAP);
        Frequencies { max: (caps & 0xFF) * FREQ_SCALER, min: ((caps >> 16) & 0xFF) * FREQ_SCALER }
    }

    pub fn mhz(units: u32) -> u32 {
        units * FREQ_UNIT_KHZ / 1000
    }
}

/// Lets the power unit take the driver's requests for a frequency, with
/// nothing of its own deciding (`intel_rps_set_manual`).
pub fn manual_frequency(mmio: &impl Mmio) {
    mmio.write(RP_CONTROL, RPSWCTL_ENABLE);
}

/// Asks for the GT to run at `units` (`gen6_rps_set`).
pub fn request_frequency(mmio: &impl Mmio, units: u32) {
    mmio.write(RPNSWREQ, frequency_request(units));
}

/// What the GT runs at now.
pub fn current_frequency(mmio: &impl Mmio) -> u32 {
    rpstat1_frequency(mmio.read(RPSTAT1))
}

// ---- bringing it up ------------------------------------------------------------

/// The MOCS table (`gen12_mocs_table`): entries' control and L3 values;
/// entries it does not name take entry 2's (`unused_entries_index`).
pub mod mocs {
    const LE_UC: u32 = 1;
    const LE_WB: u32 = 3;
    const LE_TC_LLC: u32 = 1 << 2;
    const fn lrum(v: u32) -> u32 {
        v << 4
    }
    const fn aom(v: u32) -> u32 {
        v << 6
    }
    const fn rsc(v: u32) -> u32 {
        v << 7
    }
    const fn scc(v: u32) -> u32 {
        v << 8
    }
    const fn scf(v: u32) -> u32 {
        v << 14
    }
    const fn sse(v: u32) -> u32 {
        v << 17
    }
    const L3_UC: u16 = 1 << 4;
    const L3_WB: u16 = 3 << 4;

    /// The uncached entry (`uc_index`).
    pub const UNCACHED: u32 = 3;

    /// (index, control, L3).
    pub const TABLE: &[(u32, u32, u16)] = &[
        (2, LE_WB | LE_TC_LLC | lrum(3), L3_WB),
        (3, LE_UC | LE_TC_LLC, L3_UC),
        (4, LE_UC | LE_TC_LLC, L3_WB),
        (5, LE_WB | LE_TC_LLC | lrum(3), L3_UC),
        (6, LE_WB | LE_TC_LLC | lrum(1), L3_UC),
        (7, LE_WB | LE_TC_LLC | lrum(1), L3_WB),
        (8, LE_WB | LE_TC_LLC | lrum(2), L3_UC),
        (9, LE_WB | LE_TC_LLC | lrum(2), L3_WB),
        (10, LE_WB | LE_TC_LLC | lrum(3) | aom(1), L3_UC),
        (11, LE_WB | LE_TC_LLC | lrum(3) | aom(1), L3_WB),
        (12, LE_WB | LE_TC_LLC | lrum(1) | aom(1), L3_UC),
        (13, LE_WB | LE_TC_LLC | lrum(1) | aom(1), L3_WB),
        (14, LE_WB | LE_TC_LLC | lrum(2) | aom(1), L3_UC),
        (15, LE_WB | LE_TC_LLC | lrum(2) | aom(1), L3_WB),
        (16, LE_UC | LE_TC_LLC | scf(1), L3_UC),
        (17, LE_UC | LE_TC_LLC | scf(1), L3_WB),
        (18, LE_WB | LE_TC_LLC | lrum(3) | sse(3), L3_WB),
        (19, LE_WB | LE_TC_LLC | lrum(3) | scc(7), L3_WB),
        (20, LE_WB | LE_TC_LLC | lrum(3) | scc(3), L3_WB),
        (21, LE_WB | LE_TC_LLC | lrum(3) | scc(1), L3_WB),
        (22, LE_WB | LE_TC_LLC | lrum(3) | rsc(1) | scc(3), L3_WB),
        (23, LE_WB | LE_TC_LLC | lrum(3) | rsc(1) | scc(7), L3_WB),
        (48, LE_WB | LE_TC_LLC | lrum(3), L3_WB),
        (49, LE_UC | LE_TC_LLC, L3_WB),
        (50, LE_WB | LE_TC_LLC | lrum(3), L3_UC),
        (51, LE_UC | LE_TC_LLC, L3_UC),
        (60, LE_WB | LE_TC_LLC | lrum(3), L3_UC),
        (61, LE_UC | LE_TC_LLC, L3_WB),
        (62, LE_WB | LE_TC_LLC | lrum(3), L3_UC),
        (63, LE_WB | LE_TC_LLC | lrum(3), L3_UC),
    ];

    /// Entry `i`: (control, L3).
    pub fn entry(i: u32) -> (u32, u16) {
        let unused = TABLE[0];
        let e = TABLE.iter().find(|e| e.0 == i).unwrap_or(&unused);
        (e.1, e.2)
    }
}

/// A register write: plain, or read, changed and written (`clear` the bits
/// changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegWrite {
    pub reg: u32,
    pub clear: u32,
    pub set: u32,
}

impl RegWrite {
    pub const fn write(reg: u32, value: u32) -> RegWrite {
        RegWrite { reg, clear: !0, set: value }
    }

    pub const fn or(reg: u32, bits: u32) -> RegWrite {
        RegWrite { reg, clear: bits, set: bits }
    }

    pub const fn clear(reg: u32, bits: u32) -> RegWrite {
        RegWrite { reg, clear: bits, set: 0 }
    }

    pub fn apply(&self, mmio: &impl Mmio) {
        let old = if self.clear == !0 { 0 } else { mmio.read(self.reg) };
        mmio.write(self.reg, (old & !self.clear) | self.set);
    }
}

/// The GT's workarounds (`gen12_gt_workarounds_init`), steering reads of
/// replicated registers to `dss` (the first enabled dual subslice).
pub fn gt_workarounds(dss: u32) -> Vec<RegWrite> {
    alloc::vec![
        RegWrite { reg: MCR_SELECTOR, clear: MCR_STEERING_MASK, set: mcr_steering(0, dss) },
        RegWrite::or(DFR_RATIO_EN_AND_CHICKEN, DFR_DISABLE),
        RegWrite::clear(MISCCPCTL, DOP_CLOCK_GATE_RENDER_ENABLE),
    ]
}

/// An engine's workarounds (`rcs_engine_wa_init`,
/// `general_render_compute_wa_init`, `engine_fake_wa_init`).
pub fn engine_workarounds(kind: Kind) -> Vec<RegWrite> {
    let base = kind.base();
    let cctl = RegWrite::write(ring_cmd_cctl(base), masked(CMD_CCTL_MOCS_MASK, cmd_cctl_mocs(mocs::UNCACHED)));
    match kind {
        Kind::Copy => alloc::vec![cctl],
        Kind::Render => alloc::vec![
            cctl,
            RegWrite::clear(GARBCNTL, BUS_HASH_CTL_BIT_EXC),
            RegWrite::write(SAMPLER_MODE, masked_on(INDIRECT_STATE_BASE_ADDR_OVERRIDE | ENABLE_SMALLPL)),
            RegWrite::write(CS_DEBUG_MODE1, masked_on(FF_DOP_CLOCK_GATE_DISABLE)),
            RegWrite::write(ROW_CHICKEN2, masked_on(DISABLE_EARLY_READ | PUSH_CONST_DEREF_HOLD_DIS)),
            RegWrite::or(FF_THREAD_MODE, FF_TESSELATION_DOP_GATE_DISABLE),
            RegWrite::write(ROW_CHICKEN4, masked_on(DISABLE_TDL_PUSH)),
            RegWrite::write(
                ring_psmi_ctl(base),
                masked_on(WAIT_FOR_EVENT_POWER_DOWN_DISABLE | RC_SEMA_IDLE_MSG_DISABLE),
            ),
            RegWrite::write(FF_SLICE_CS_CHICKEN1, masked_on(FFSC_PERCTX_PREEMPT_CTRL)),
        ],
    }
}

/// The registers a batch, which runs unprivileged, may still use
/// (`tgl_whitelist_build`, with `allow_read_ctx_timestamp`): their
/// `RING_FORCE_TO_NONPRIV` values.
pub fn whitelist(kind: Kind) -> Vec<u32> {
    let base = kind.base();
    let mut w = alloc::vec![ring_ctx_timestamp(base) | NONPRIV_ACCESS_RD];
    if kind == Kind::Render {
        w.extend([
            PS_INVOCATION_COUNT | NONPRIV_ACCESS_RD | NONPRIV_RANGE_4,
            COMMON_SLICE_CHICKEN1,
            HIZ_CHICKEN,
            COMMON_SLICE_CHICKEN3,
        ]);
    }
    w.sort_unstable();
    w
}

/// Sets the GT up (`intel_gt_init_hw`): its workarounds, the PAT entries
/// page tables select, the MOCS table surfaces select.
pub fn init_gt(mmio: &impl Mmio, topology: &Topology) {
    let dss = topology.dss_mask.trailing_zeros().min(5);
    for w in gt_workarounds(dss) {
        w.apply(mmio);
    }
    // `tgl_setup_private_ppat`: index 0 write-back, 1 write-combining, 2
    // write-through, 3 uncached (`render::UNCACHED_PAT`), the rest
    // write-back.
    for (i, v) in [PPAT_WB, PPAT_WC, PPAT_WT, PPAT_UC, PPAT_WB, PPAT_WB, PPAT_WB, PPAT_WB].iter().enumerate() {
        mmio.write(pat_index(i as u32), *v);
    }
    for i in 0..64 {
        mmio.write(global_mocs(i), mocs::entry(i).0);
    }
    for i in 0..32 {
        let (low, high) = (mocs::entry(2 * i).1, mocs::entry(2 * i + 1).1);
        mmio.write(l3cc(i), u32::from(low) | u32::from(high) << 16);
    }
}

/// The GT's interrupts for the render and copy engines
/// (`gen11_gt_irq_postinstall`): a batch's end, errors, context switches.
pub fn enable_gt_interrupts(mmio: &impl Mmio) {
    let irqs = GT_RENDER_USER_INTERRUPT | GT_CS_MASTER_ERROR_INTERRUPT | GT_CONTEXT_SWITCH_INTERRUPT;
    mmio.write(RENDER_COPY_INTR_ENABLE, irqs << 16 | irqs);
    mmio.write(RCS0_INTR_MASK, !(irqs << 16));
    mmio.write(BCS_INTR_MASK, !(irqs << 16));
}

/// No interrupt from the render and copy engines (`gen11_gt_irq_reset`):
/// nothing is latched for them, so nothing reaches the master control.
pub fn disable_gt_interrupts(mmio: &impl Mmio) {
    mmio.write(RENDER_COPY_INTR_ENABLE, 0);
    mmio.write(RCS0_INTR_MASK, !0);
    mmio.write(BCS_INTR_MASK, !0);
}

/// What the GT's interrupt banks brought: the engines that interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GtCause {
    pub render: u16,
    pub copy: u16,
    pub other: bool,
}

/// Handles the GT's part of an interrupt (`gen11_gt_irq_handler`): each
/// pending engine's identity read and acknowledged, then the banks
/// cleared. The caller has masked the master interrupt.
pub fn handle_gt_interrupt(mmio: &impl Mmio, master: u32) -> GtCause {
    let mut cause = GtCause::default();
    for bank in 0..2 {
        if master & (1 << bank) == 0 {
            continue;
        }
        let dw = mmio.read(gt_intr_dw(bank));
        for bit in 0..32 {
            if dw & (1 << bit) == 0 {
                continue;
            }
            mmio.write(iir_selector(bank), 1 << bit);
            let ok = wait_for(mmio, intr_identity(bank), INTR_DATA_VALID, INTR_DATA_VALID, 100);
            let ident = mmio.read(intr_identity(bank));
            mmio.write(intr_identity(bank), INTR_DATA_VALID);
            let intr = (ident & 0xFFFF) as u16;
            match (bank, bit, ok) {
                (0, INTR_BIT_RCS0, true) => cause.render |= intr,
                (0, INTR_BIT_BCS, true) => cause.copy |= intr,
                _ => cause.other = true,
            }
        }
        mmio.write(gt_intr_dw(bank), dw);
    }
    cause
}

// ---- engines -----------------------------------------------------------------

/// Where an engine's status page has what: the context status buffer
/// (12 entries of two dwords from dword 0x10), its write pointer (dword
/// 0x2F), and the number the engine's submissions write (the driver's).
pub mod hwsp {
    pub const CSB: usize = 0x10;
    pub const CSB_ENTRIES: usize = 12;
    pub const CSB_WRITE: usize = 0x2F;
    /// A quad word, in bytes.
    pub const SEQNO: u32 = 0x100;
}

/// Sets an engine up for execlists, its status page at `hwsp` in the
/// global table (`execlists_resume`, `enable_execlists`, the engine's
/// workarounds and whitelist, `reset_csb_pointers`); `status` is that
/// page, as the driver writes it.
pub fn init_engine(mmio: &impl Mmio, kind: Kind, hwsp: u32, status: &mut [u32]) {
    let base = kind.base();
    for w in engine_workarounds(kind) {
        w.apply(mmio);
    }
    let w = whitelist(kind);
    for i in 0..NONPRIV_SLOTS {
        let reg = w.get(i as usize).copied().unwrap_or(ring_nopid(base));
        mmio.write(ring_force_to_nonpriv(base, i), reg);
    }
    // No interrupt written to the status page; execlists, not the legacy
    // ring; running; the status page.
    mmio.write(ring_hwstam(base), !0);
    mmio.write(ring_mode(base), masked_on(GFX_DISABLE_LEGACY_MODE));
    mmio.write(ring_mi_mode(base), masked_off(STOP_RING));
    mmio.write(ring_hws_pga(base), hwsp);
    let _ = mmio.read(ring_hws_pga(base));
    mmio.write(ring_emr(base), !ERROR_INSTRUCTION);
    reset_csb(mmio, kind, status);
    let irqs = GT_RENDER_USER_INTERRUPT | GT_CS_MASTER_ERROR_INTERRUPT | GT_CONTEXT_SWITCH_INTERRUPT;
    mmio.write(ring_imr(base), !irqs);
}

/// Puts the context status buffer's pointers back, as after a reset.
pub fn reset_csb(mmio: &impl Mmio, kind: Kind, status: &mut [u32]) {
    let last = (hwsp::CSB_ENTRIES - 1) as u32;
    let reg = ring_context_status_ptr(kind.base());
    mmio.write(reg, 0xFFFF << 16 | last << 8 | last);
    let _ = mmio.read(reg);
    status[hwsp::CSB_WRITE] = last;
    for v in &mut status[hwsp::CSB..hwsp::CSB + 2 * hwsp::CSB_ENTRIES] {
        *v = !0;
    }
    mmio.write(reg, 0xFFFF << 16 | last << 8 | last);
    let _ = mmio.read(reg);
}

/// Hands the engine a context to run (`execlists_submit_ports` with the
/// submit queue): port 1 empty, port 0 the context, then load.
pub fn submit(mmio: &impl Mmio, kind: Kind, descriptor: u64) {
    let sq = ring_execlist_sq(kind.base());
    mmio.write(sq + 8, 0);
    mmio.write(sq + 12, 0);
    mmio.write(sq, descriptor as u32);
    mmio.write(sq + 4, (descriptor >> 32) as u32);
    mmio.write(ring_execlist_control(kind.base()), EL_CTRL_LOAD);
}

/// Resets engines (`gen8_reset_engines`: each asked to get ready, the
/// reset, the requests withdrawn); whether the GPU acknowledged it.
pub fn reset_engines(mmio: &impl Mmio, kinds: &[Kind]) -> bool {
    let mut domains = 0;
    for &k in kinds {
        let reg = ring_reset_ctl(k.base());
        let ctl = mmio.read(reg);
        if ctl & RESET_CTL_CAT_ERROR != 0 {
            mmio.write(reg, masked_on(RESET_CTL_CAT_ERROR));
            wait_for(mmio, reg, RESET_CTL_CAT_ERROR, 0, ACK_SPINS);
        } else if ctl & RESET_CTL_READY_TO_RESET == 0 {
            mmio.write(reg, masked_on(RESET_CTL_REQUEST_RESET));
            wait_for(mmio, reg, RESET_CTL_READY_TO_RESET, RESET_CTL_READY_TO_RESET, ACK_SPINS);
        }
        domains |= match k {
            Kind::Render => GRDOM_RENDER,
            Kind::Copy => GRDOM_BLT,
        };
    }
    // Twice: the engines' registers settle only after the second
    // (`gen6_hw_domain_reset`).
    let mut ok = true;
    for _ in 0..2 {
        mmio.write(GDRST, domains);
        ok &= wait_for(mmio, GDRST, domains, 0, ACK_SPINS);
    }
    for &k in kinds {
        mmio.write(ring_reset_ctl(k.base()), masked_off(RESET_CTL_REQUEST_RESET));
    }
    ok
}

/// An engine's state, for the log when it hangs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineState {
    pub head: u32,
    pub tail: u32,
    pub acthd: u32,
    pub bbaddr: u32,
    pub execlist_status: u32,
    pub eir: u32,
    pub esr: u32,
    pub mi_mode: u32,
}

impl EngineState {
    pub fn read(mmio: &impl Mmio, kind: Kind) -> EngineState {
        let base = kind.base();
        EngineState {
            head: mmio.read(ring_head(base)),
            tail: mmio.read(ring_tail(base)),
            acthd: mmio.read(ring_acthd(base)),
            bbaddr: mmio.read(ring_bbaddr(base)),
            execlist_status: mmio.read(ring_execlist_status(base)),
            eir: mmio.read(ring_eir(base)),
            esr: mmio.read(ring_esr(base)),
            mi_mode: mmio.read(ring_mi_mode(base)),
        }
    }
}

impl core::fmt::Display for EngineState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "head {:#x} tail {:#x} acthd {:#x} batch {:#x} execlists {:#x} eir {:#x} esr {:#x} mi_mode {:#x}",
            self.head, self.tail, self.acthd, self.bbaddr, self.execlist_status, self.eir, self.esr, self.mi_mode
        )
    }
}
