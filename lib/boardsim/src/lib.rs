//! `vboardsim` — simulated machines, for host tests of what Veda's
//! drivers do to hardware that no emulator has.
//!
//! A simulated board is its firmware's description (AML shaped like the
//! real firmware's) and models of its chips, wired together as on the real
//! board. Veda's own code runs against it unchanged, through the traits it
//! uses on the hardware: the ACPI interpreter devmgr uses (`vacpi`), the
//! GPIO pads (`vgpio`), the SPI controller (`vspi`) and the amplifiers'
//! sequences (`vcs35l41`). Only the MMIO and IPC plumbing of devmgr and the
//! drivers is left out.
//!
//! What a simulation shows: that the drivers find the devices where the
//! firmware puts them, and drive them as they mean to, in the right order,
//! on the right wires (never two devices selected at once, no protected
//! register written while locked, no amplifier powered up without its
//! clock). What it cannot: that the real chips behave as their models do,
//! since the models come from the same documents as the drivers; or how
//! anything sounds. The real machine stays the final check.
//!
//! * [`zenbook`]: the ASUS Zenbook Pro 16X (UX7602ZM) and its speaker
//!   amplifiers, with the DSP firmware Veda ships for them;
//! * [`cs35l41`], [`halo`], [`lpss`], [`pads`]: the chips' models (the
//!   amplifiers, their DSPs, the SPI controller, the GPIO pads).

pub mod cs35l41;
pub mod halo;
pub mod lpss;
pub mod pads;
pub mod zenbook;

#[cfg(test)]
mod tests;
