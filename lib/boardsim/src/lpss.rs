//! An Intel LPSS SPI controller, as its registers behave: a PXA2xx-style
//! port that shifts a byte out and one in for each byte written to its
//! data register while it is on, a receive FIFO, and Intel's private
//! registers (reset, the clock and its divider, the chip select lines,
//! which chip selects the controller has).

use std::collections::{BTreeMap, VecDeque};

use vspi::regs::*;

/// Bytes the receive FIFO holds.
const FIFO: usize = 64;

/// A byte the port shifts, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift {
    pub byte: u8,
    /// The bit clock it goes at.
    pub rate_hz: u32,
    pub bits: u32,
    pub cpol: bool,
    pub cpha: bool,
}

pub struct SspModel {
    regs: BTreeMap<usize, u32>,
    rx: VecDeque<u8>,
    overrun: bool,
    /// The clock into Intel's divider.
    pub input_hz: u32,
}

impl SspModel {
    /// A controller with `chip_selects` chip selects of its own.
    pub fn new(chip_selects: u32, input_hz: u32) -> SspModel {
        let present = (1u32 << chip_selects) - 1;
        SspModel {
            regs: BTreeMap::from([(CAPABILITIES, present << CAPS_CS_SHIFT)]),
            rx: VecDeque::new(),
            overrun: false,
            input_hz,
        }
    }

    pub fn reg(&self, offset: usize) -> u32 {
        self.regs.get(&offset).copied().unwrap_or(0)
    }

    fn out_of_reset(&self) -> bool {
        self.reg(RESETS) & RESETS_RELEASED == RESETS_RELEASED
    }

    /// The native chip select asserted (lines are active low), if any.
    pub fn asserted(&self) -> Option<u8> {
        let v = self.reg(CS_CONTROL);
        (self.out_of_reset() && v & CS_SOFTWARE != 0 && v & CS_HIGH == 0).then_some((v >> CS_SELECT_SHIFT & 3) as u8)
    }

    /// The port's clock: the input through Intel's M/N divider.
    pub fn port_hz(&self) -> u32 {
        let clock = self.reg(CLOCK);
        let (m, n) = ((clock >> 1) & 0x7FFF, (clock >> 16) & 0x7FFF);
        if m != 0 && n != 0 { (self.input_hz as u64 * m as u64 / n as u64) as u32 } else { self.input_hz }
    }

    pub fn read(&mut self, offset: usize) -> u32 {
        match offset {
            SSSR => {
                let mut v = SSSR_TNF;
                if !self.rx.is_empty() {
                    v |= SSSR_RNE;
                }
                if self.overrun {
                    v |= SSSR_ROR;
                }
                v
            }
            SSDR => self.rx.pop_front().unwrap_or(0) as u32,
            o => self.reg(o),
        }
    }

    /// A register write. A byte written to the data register while the port
    /// is on is shifted: returned, for the board to put on its wires.
    pub fn write(&mut self, offset: usize, value: u32) -> Option<Shift> {
        match offset {
            SSSR => {
                if value & SSSR_ROR != 0 {
                    self.overrun = false;
                }
                None
            }
            SSDR => {
                let cr0 = self.reg(SSCR0);
                if !self.out_of_reset() || cr0 & SSCR0_SSE == 0 {
                    return None;
                }
                let cr1 = self.reg(SSCR1);
                let scr = (cr0 & SSCR0_SCR_MASK) >> SSCR0_SCR_SHIFT;
                Some(Shift {
                    byte: value as u8,
                    rate_hz: self.port_hz() / (scr + 1),
                    bits: (cr0 & SSCR0_DSS) + 1,
                    cpol: cr1 & SSCR1_SPO != 0,
                    cpha: cr1 & SSCR1_SPH != 0,
                })
            }
            RESETS if value & RESETS_RELEASED == 0 => {
                // In reset: the port, its FIFO and chip select control go.
                self.regs.retain(|&o, _| o == CAPABILITIES || o == CLOCK);
                self.regs.insert(RESETS, value);
                self.rx.clear();
                self.overrun = false;
                None
            }
            o => {
                self.regs.insert(o, value);
                None
            }
        }
    }

    /// The byte shifted in.
    pub fn receive(&mut self, byte: u8) {
        if self.rx.len() == FIFO {
            self.overrun = true;
        } else {
            self.rx.push_back(byte);
        }
    }
}
