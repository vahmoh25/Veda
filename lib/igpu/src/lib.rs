//! `vigpu` — Intel integrated graphics for Veda's driver
//! (`drivers/intel-gpu`): the GPUs of Tiger Lake to Raptor Lake (Iris Xe
//! and UHD, display versions 12 and 13, graphics version 12), whose
//! display engines and GTs work alike.
//!
//! The driver keeps the mode the firmware set and takes over the picture
//! it shows ([`display`]), maps its own pictures into the GPU's address
//! table ([`gtt`]), and flips between them at the start of each vertical
//! blank ([`flip`]), knowing when one began from the display engine's
//! interrupts ([`irq`]). So frames reach the screen whole and evenly
//! paced, without tearing, as on the virtual machines' displays. The flip
//! loop itself is here too ([`scanout`]): the driver only waits for what
//! wakes it and calls in.
//!
//! Beside the display, the GT runs what the renderer draws with: its
//! render and copy engines ([`render`]) behind the GEM model ([`gem`]),
//! brought up and driven as i915 does ([`gt`], with the registers in
//! [`gtregs`]): contexts ([`lrc`]) in address spaces of their own
//! ([`ppgtt`]), submitted to the engines with execlists.
//!
//! Everything here reaches the hardware through [`Mmio`], and time through
//! [`scanout::Clock`] and [`render::Time`]: the driver's mapping of the
//! GPU's first BAR and the system's clock, or a simulated GPU and time in
//! host tests (`vboardsim`). Register names and fields follow Linux's i915
//! driver ([`regs`]); the devices are those it lists ([`device`]).

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod device;
pub mod display;
pub mod flip;
pub mod gem;
pub mod gt;
pub mod gtregs;
pub mod gtt;
pub mod irq;
pub mod lrc;
pub mod ppgtt;
pub mod regs;
pub mod render;
pub mod scanout;

#[cfg(test)]
mod tests;

/// The GPU's first BAR: its registers, and the global address table in
/// the upper half. Offsets are in bytes.
pub trait Mmio {
    fn read(&self, offset: u32) -> u32;
    fn write(&self, offset: u32, value: u32);
    fn read64(&self, offset: u32) -> u64;
    fn write64(&self, offset: u32, value: u64);
}

/// A reference reaches the same registers.
impl<T: Mmio + ?Sized> Mmio for &T {
    fn read(&self, offset: u32) -> u32 {
        (**self).read(offset)
    }

    fn write(&self, offset: u32, value: u32) {
        (**self).write(offset, value)
    }

    fn read64(&self, offset: u32) -> u64 {
        (**self).read64(offset)
    }

    fn write64(&self, offset: u32, value: u64) {
        (**self).write64(offset, value)
    }
}
