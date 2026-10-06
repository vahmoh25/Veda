//! `vcs35l41` — Cirrus Logic CS35L41 speaker amplifiers.
//!
//! Many laptops since 2021 (ASUS, Lenovo, HP, Dell, Acer) drive their
//! speakers with two or four CS35L41 "smart" amplifiers. The HD Audio codec
//! sends them its speaker output over I2S; the amplifiers are set up over
//! SPI or I2C, and stay silent until they are: reset, booted from their OTP
//! memory, trimmed with the calibration values it holds, patched for their
//! silicon revision, and enabled each time playback starts.
//!
//! This crate holds what does not touch the bus, so that it runs inside
//! Veda and in host tests alike, following Linux's `cs35l41_hda` driver
//! and `cs35l41-lib` (Cirrus Logic's own) value for value:
//!
//! * [`regs`]: the registers and bits used;
//! * [`otp`]: unpacking the OTP trims into their registers;
//! * [`amp`]: the sequences — probe, play, pause — over a register [`Bus`];
//! * [`wmfw`], [`dsp`]: the DSP's firmware files, and loading, starting
//!   and commanding the firmware;
//! * [`group`]: the amplifiers of one firmware device together, over the
//!   [`group::Board`] around them (their bus and GPIOs, and the firmware
//!   files);
//! * [`config`]: the laptops whose firmware leaves the amplifiers' settings
//!   out (which boost circuit, which channel each amplifier plays, which
//!   GPIO resets them), by subsystem id.
//!
//! With the DSP firmware Linux loads (Cirrus's speaker protection and the
//! board maker's tuning for its speakers, from the Linux firmware
//! collection), the codec's audio goes through the DSP to the amplifier at
//! its full gain. Without it ("DSP bypass", as on Linux when the files are
//! missing), it goes straight to the amplifier at a low fixed gain.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod amp;
pub mod config;
pub mod dsp;
pub mod group;
pub mod otp;
pub mod regs;
pub mod wmfw;

#[cfg(test)]
mod tests;

pub use amp::{Amp, Error};

/// Register access to one amplifier, and waiting.
pub trait Bus {
    fn read(&mut self, register: u32) -> Result<u32, Error>;
    fn write(&mut self, register: u32, value: u32) -> Result<(), Error>;
    /// Writes `data`, whole big-endian registers, to `register` and those
    /// after it (as one transfer, where the bus can).
    fn write_block(&mut self, register: u32, data: &[u8]) -> Result<(), Error> {
        let (words, _) = data.as_chunks::<4>();
        for (i, w) in words.iter().enumerate() {
            self.write(register + 4 * i as u32, u32::from_be_bytes(*w))?;
        }
        Ok(())
    }
    fn sleep_us(&mut self, us: u64);
}

/// Which channel an amplifier plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Left,
    Right,
    /// Both, mixed (a single amplifier).
    Center,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Channel::Left => "left",
            Channel::Right => "right",
            Channel::Center => "centre",
        }
    }
}
