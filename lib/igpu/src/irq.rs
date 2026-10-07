//! Vertical blank interrupts, the way Linux's i915 driver handles them on
//! display versions 11 and later: a master control for the whole GPU
//! (`GEN11_GFX_MSTR_IRQ`), one for the display engine
//! (`GEN11_DISPLAY_INT_CTL`) and each pipe's mask, enable and identity
//! registers (`GEN8_DE_PIPE_IMR`, `IER`, `IIR`). An event the mask lets
//! through is latched in the identity register; one that is also enabled
//! interrupts.
//!
//! The pipe's faults ([`FAULTS`]) are latched without interrupting: the
//! driver finds them with the next vertical blank, or when it looks
//! ([`faults`]).

use crate::Mmio;
use crate::regs::{self, Pipe};

/// What the driver watches for without being interrupted: plane 1 reading
/// unmapped memory, the pipe running short of pixels.
pub const FAULTS: u32 = regs::PIPE_PLANE1_FAULT | regs::PIPE_FIFO_UNDERRUN;

/// No interrupt from the GPU, pipe `pipe`'s events cleared: a known state
/// to start from (the firmware polls). Only the pipe's faults are latched
/// from now on.
pub fn reset(mmio: &impl Mmio, pipe: Pipe) {
    mmio.write(regs::MASTER_IRQ, 0);
    mmio.write(regs::pipe_ier(pipe), 0);
    mmio.write(regs::pipe_imr(pipe), !FAULTS);
    clear(mmio, pipe, !0);
}

/// Clears `events` that pipe `pipe` has pending (its identity register
/// holds one more event behind the one it shows, so twice).
fn clear(mmio: &impl Mmio, pipe: Pipe, events: u32) {
    for _ in 0..2 {
        let pending = mmio.read(regs::pipe_iir(pipe)) & events;
        if pending == 0 {
            break;
        }
        mmio.write(regs::pipe_iir(pipe), pending);
    }
}

/// Interrupts at every vertical blank of `pipe`, from now on.
pub fn enable_vblank(mmio: &impl Mmio, pipe: Pipe) {
    clear(mmio, pipe, regs::PIPE_VBLANK);
    mmio.write(regs::pipe_ier(pipe), regs::PIPE_VBLANK);
    mmio.write(regs::pipe_imr(pipe), !(regs::PIPE_VBLANK | FAULTS));
    let ctl = mmio.read(regs::DISPLAY_INT_CTL);
    mmio.write(regs::DISPLAY_INT_CTL, ctl | regs::DISPLAY_IRQ_ENABLE);
    mmio.write(regs::MASTER_IRQ, regs::MASTER_IRQ_ENABLE);
}

/// No more vertical blank interrupts of `pipe` (its faults are still
/// latched).
pub fn disable_vblank(mmio: &impl Mmio, pipe: Pipe) {
    mmio.write(regs::pipe_imr(pipe), !FAULTS);
    mmio.write(regs::pipe_ier(pipe), 0);
    clear(mmio, pipe, regs::PIPE_VBLANK);
}

/// What an interrupt brought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cause {
    /// Pipe `pipe`'s vertical blank began.
    pub vblank: bool,
    /// The pipe's faults latched since they were last looked at
    /// ([`FAULTS`] bits).
    pub faults: u32,
    /// Something this driver did not ask for (acknowledged and ignored).
    pub other: bool,
}

/// Handles the GPU's interrupt: masks it at the master control, finds and
/// acknowledges what `pipe` raised, and enables it again.
pub fn handle(mmio: &impl Mmio, pipe: Pipe) -> Cause {
    let master = mmio.read(regs::MASTER_IRQ);
    mmio.write(regs::MASTER_IRQ, 0);
    let mut cause = Cause::default();
    if master & regs::MASTER_IRQ_DISPLAY != 0 {
        let display = mmio.read(regs::DISPLAY_INT_CTL);
        if display & regs::display_int_pipe(pipe) != 0 {
            let pending = mmio.read(regs::pipe_iir(pipe));
            mmio.write(regs::pipe_iir(pipe), pending);
            cause.vblank = pending & regs::PIPE_VBLANK != 0;
            cause.faults = pending & FAULTS;
            cause.other |= pending & !(regs::PIPE_VBLANK | FAULTS) != 0;
        }
        // The engine's other sources (other pipes, ports, hot plugging).
        cause.other |= display & regs::DISPLAY_SOURCES & !regs::display_int_pipe(pipe) != 0;
    }
    cause.other |= master & !(regs::MASTER_IRQ_DISPLAY | regs::MASTER_IRQ_ENABLE) != 0;
    mmio.write(regs::MASTER_IRQ, regs::MASTER_IRQ_ENABLE);
    cause
}

/// The faults pipe `pipe` latched since they were last looked at
/// ([`FAULTS`] bits), acknowledged.
pub fn faults(mmio: &impl Mmio, pipe: Pipe) -> u32 {
    let pending = mmio.read(regs::pipe_iir(pipe)) & FAULTS;
    if pending != 0 {
        mmio.write(regs::pipe_iir(pipe), pending);
    }
    pending
}

/// No interrupt from the GPU at all (when something keeps raising
/// interrupts this driver does not understand, it polls instead).
pub fn shut(mmio: &impl Mmio, pipe: Pipe) {
    mmio.write(regs::MASTER_IRQ, 0);
    let ctl = mmio.read(regs::DISPLAY_INT_CTL);
    mmio.write(regs::DISPLAY_INT_CTL, ctl & !regs::DISPLAY_IRQ_ENABLE);
    disable_vblank(mmio, pipe);
}
