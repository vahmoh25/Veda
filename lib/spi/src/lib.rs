//! `vspi` — SPI controllers for Veda's drivers.
//!
//! Intel's LPSS SPI controllers (Cannon Lake and later, `LPSS_CNL_SSP` in
//! Linux): a PXA2xx-style synchronous serial port, plus Intel's private
//! registers for reset, the clock and the chip selects. They are set up as
//! Linux's `intel-lpss` and `spi-pxa2xx` do, and transfers are polled, a
//! byte in for each byte out (the devices here need short ones). The
//! registers are reached through [`Hardware`], so that this runs in Veda
//! (over the mapped BAR) and in host tests (over a model) alike.

#![no_std]

/// The controller's registers, and a way to wait.
pub trait Hardware {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    /// Waits at least `ns` nanoseconds.
    fn delay_ns(&self, ns: u64);
}

/// Register offsets and bits.
pub mod regs {
    // The serial port.
    pub const SSCR0: usize = 0x00;
    pub const SSCR1: usize = 0x04;
    pub const SSSR: usize = 0x08;
    pub const SSDR: usize = 0x10;
    /// Data size select: the bits per word, minus one.
    pub const SSCR0_DSS: u32 = 0xF;
    pub const SSCR0_SSE: u32 = 1 << 7;
    /// Serial clock rate: the bit clock is the port's clock / (SCR + 1).
    pub const SSCR0_SCR_SHIFT: u32 = 8;
    pub const SSCR0_SCR_MASK: u32 = 0xFFF << SSCR0_SCR_SHIFT;
    pub const SSCR1_SPO: u32 = 1 << 3;
    pub const SSCR1_SPH: u32 = 1 << 4;
    pub const SSSR_TNF: u32 = 1 << 2;
    pub const SSSR_RNE: u32 = 1 << 3;
    pub const SSSR_BSY: u32 = 1 << 4;
    pub const SSSR_ROR: u32 = 1 << 7;

    // Intel's private registers.
    pub const PRIVATE: usize = 0x200;
    /// Clock: enable (bit 0), divider M (1-15) and N (16-30), update (31).
    pub const CLOCK: usize = PRIVATE;
    pub const RESETS: usize = PRIVATE + 0x04;
    /// Out of reset: the function and its DMA engine.
    pub const RESETS_RELEASED: u32 = 0x7;
    pub const CS_CONTROL: usize = PRIVATE + 0x24;
    pub const CS_SOFTWARE: u32 = 1 << 0;
    pub const CS_HIGH: u32 = 1 << 1;
    pub const CS_SELECT_SHIFT: u32 = 8;
    pub const CS_SELECT_MASK: u32 = 3 << CS_SELECT_SHIFT;
    pub const CLOCK_GATE: usize = PRIVATE + 0x38;
    pub const CLOCK_GATE_FORCE_ON: u32 = 0x3;
    pub const REMAP_ADDRESS: usize = PRIVATE + 0x40;
    pub const CAPABILITIES: usize = PRIVATE + 0xFC;
    /// Which chip selects the controller has (bits 9-12).
    pub const CAPS_CS_SHIFT: u32 = 9;
}

use regs::*;

/// Status polls before a transfer is given up (each a register read).
const MAX_POLLS: u32 = 200_000;
/// Bytes left in the receive FIFO that are drained at most.
const MAX_LEFTOVER: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiError {
    /// The port did not take or give a byte.
    Stalled,
    /// Data was lost (receive overrun).
    Overrun,
}

/// A device's clock polarity and phase (SPI mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub cpol: bool,
    pub cpha: bool,
}

pub struct Controller<H> {
    hw: H,
    /// The port's input clock, after Intel's divider.
    pub clock_hz: u32,
    /// Native chip selects.
    pub chip_selects: u32,
    /// Serial clock divider (SCR): the bit clock is `clock_hz / (scr + 1)`.
    scr: u32,
}

impl<H: Hardware> Controller<H> {
    /// Takes the controller out of reset and sets up its clock (whose
    /// input runs at `input_hz`) and chip selects. `bar` is the address
    /// of its registers, for its DMA engine's view of them.
    pub fn start(hw: H, input_hz: u32, bar: u64) -> Result<Controller<H>, &'static str> {
        let mut c = Controller { hw, clock_hz: input_hz, chip_selects: 1, scr: 0 };
        if c.hw.read(CAPABILITIES) == u32::MAX {
            return Err("the controller does not answer");
        }
        c.hw.write(RESETS, 0);
        c.hw.write(RESETS, RESETS_RELEASED);
        c.hw.write(REMAP_ADDRESS, bar as u32);
        c.hw.write(REMAP_ADDRESS + 4, (bar >> 32) as u32);

        // The clock: its enable and update gates on, Intel's fractional
        // divider (M/N) as the firmware set it.
        let clock = c.hw.read(CLOCK);
        let (m, n) = ((clock >> 1) & 0x7FFF, (clock >> 16) & 0x7FFF);
        if m != 0 && n != 0 {
            c.clock_hz = (input_hz as u64 * m as u64 / n as u64) as u32;
        }
        c.hw.write(CLOCK, clock | 1 | 1 << 31);

        // Chip selects: the ones present (consecutive from 0), driven by
        // software, all deasserted.
        let present = c.hw.read(CAPABILITIES) >> CAPS_CS_SHIFT & 0xF;
        c.chip_selects = (!present).trailing_zeros().clamp(1, 4);
        c.hw.write(CS_CONTROL, c.hw.read(CS_CONTROL) | CS_SOFTWARE | CS_HIGH);
        c.hw.write(SSCR0, 0);
        Ok(c)
    }

    pub fn hardware(&self) -> &H {
        &self.hw
    }

    /// Sets the bit clock to at most `hz`; returns it.
    pub fn set_speed(&mut self, hz: u32) -> u32 {
        let hz = hz.clamp(1, self.clock_hz);
        self.scr = (self.clock_hz.div_ceil(hz) - 1).min(0xFFF);
        self.clock_hz / (self.scr + 1)
    }

    fn select(&self, cs: u8) {
        let v = self.hw.read(CS_CONTROL);
        let selected = (v & !CS_SELECT_MASK) | (cs as u32) << CS_SELECT_SHIFT;
        if selected != v {
            self.hw.write(CS_CONTROL, selected);
            // The selection is latched before the line is driven: two port
            // clocks, or the previous line glitches.
            self.hw.delay_ns(2 * 1_000_000_000 / self.clock_hz as u64 + 1);
        }
    }

    /// Drives the selected native chip select (active low).
    fn chip_select(&self, active: bool) {
        let v = self.hw.read(CS_CONTROL);
        self.hw.write(CS_CONTROL, if active { v & !CS_HIGH } else { v | CS_HIGH });
        // With the clock gated, the line only moves with the clock: force
        // it on and back, as Linux does.
        let gate = self.hw.read(CLOCK_GATE);
        let on = gate | CLOCK_GATE_FORCE_ON;
        if on != gate {
            self.hw.write(CLOCK_GATE, on);
            self.hw.write(CLOCK_GATE, gate & !CLOCK_GATE_FORCE_ON);
        }
    }

    fn poll(&self, bit: u32) -> Result<(), SpiError> {
        for _ in 0..MAX_POLLS {
            if self.hw.read(SSSR) & bit != 0 {
                return Ok(());
            }
        }
        Err(SpiError::Stalled)
    }

    /// A full-duplex transfer: `data` goes out and is replaced by what
    /// comes in. `native_cs` is the chip select the controller drives
    /// (`None` when the caller drives one through a GPIO).
    pub fn transfer(&self, native_cs: Option<u8>, mode: Mode, data: &mut [u8]) -> Result<(), SpiError> {
        let mode_bits = if mode.cpol { SSCR1_SPO } else { 0 } | if mode.cpha { SSCR1_SPH } else { 0 };
        let cr0 = self.scr << SSCR0_SCR_SHIFT | (8 - 1);
        self.hw.write(SSCR0, cr0);
        self.hw.write(SSCR1, mode_bits);
        // Nothing left over from before.
        for _ in 0..MAX_LEFTOVER {
            if self.hw.read(SSSR) & SSSR_RNE == 0 {
                break;
            }
            self.hw.read(SSDR);
        }
        self.hw.write(SSSR, SSSR_ROR);
        self.hw.write(SSCR0, cr0 | SSCR0_SSE);
        if let Some(cs) = native_cs {
            self.select(cs);
            self.chip_select(true);
        }
        let mut result = Ok(());
        for byte in data.iter_mut() {
            if let Err(e) = self.poll(SSSR_TNF) {
                result = Err(e);
                break;
            }
            self.hw.write(SSDR, *byte as u32);
            if let Err(e) = self.poll(SSSR_RNE) {
                result = Err(e);
                break;
            }
            *byte = self.hw.read(SSDR) as u8;
        }
        // Done shifting before the chip select goes.
        for _ in 0..MAX_POLLS {
            if self.hw.read(SSSR) & SSSR_BSY == 0 {
                break;
            }
        }
        if result.is_ok() && self.hw.read(SSSR) & SSSR_ROR != 0 {
            result = Err(SpiError::Overrun);
        }
        if native_cs.is_some() {
            self.chip_select(false);
        }
        self.hw.write(SSCR0, cr0);
        result
    }
}
