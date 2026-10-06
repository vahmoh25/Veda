//! A Cirrus Logic CS35L41 amplifier, as its SPI port and registers behave:
//! it boots from its OTP memory a moment after its reset is released (or a
//! software reset), answers frames of a 32-bit address (the top bit set to
//! read), 16 bits of padding and 32-bit values, all big-endian, stepping to
//! the next register every four bytes; keeps its test and trim registers
//! behind the test key; latches power-up and power-down (which need the
//! codec's I2S clock, its PLL's reference) and errors in `IRQ1_STATUS1`,
//! written 1 to clear; and has a DSP ([`Halo`]).
//!
//! What a driver does wrong is noted in [`Cs35l41::problems`]: talking to
//! it while it is held in reset or has not booted, frames of the wrong
//! length, protected registers written while locked, its own boost
//! converter enabled on a board whose boost circuit is external, a gain
//! above Linux's 4.5 dB without the DSP's protection in the path.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec;

use vcs35l41::regs::*;

use crate::halo::Halo;

/// The highest gain without the DSP's protection (Linux's 4.5 dB).
const BYPASS_GAIN: u32 = 4;

/// Microseconds from reset to OTP boot done.
pub const BOOT_US: u64 = 1000;

pub struct Cs35l41 {
    pub name: &'static str,
    /// Missing (or dead): its port never answers.
    pub absent: bool,
    pub device: u32,
    pub revision: u32,
    pub otp_id: u32,
    pub otp: [u32; OTP_WORDS],
    regs: BTreeMap<u32, u32>,
    status1: u32,
    /// The reset line is released.
    released: bool,
    /// When OTP boot completes (µs on the board's clock), or `None` held.
    boots_at: Option<u64>,
    key: [u32; 2],
    /// The chip select is asserted; the frame so far.
    selected: bool,
    frame: Vec<u8>,
    reading: u32,
    /// Powered up (global enable done).
    pub active: bool,
    /// The speaker output switched on, once powered up (by the last step
    /// of the sequence, or by the DSP's firmware when told to).
    pub speaker_out: bool,
    pub dsp: Halo,
    /// Register writes taken, in order.
    pub writes: Vec<(u32, u32)>,
    pub problems: Vec<String>,
}

/// The test and trim registers: written only with the test key unlocked.
fn protected(r: u32) -> bool {
    matches!(
        r,
        0x2030 | 0x208C | 0x2090 | 0x300C | 0x3014 | 0x3854 | 0x393C | 0x394C | 0x3950..=0x395C
            | 0x4000 | 0x400C | 0x410C | 0x4160..=0x4170 | 0x4360 | 0x4448 | 0x444C | 0x6E30..=0x6E64
            | 0x7068 | 0x7414..=0x7438 | 0x17040 | 0x17044
    )
}

/// What the registers hold after a reset (Linux's `cs35l41_reg` defaults,
/// those the sequences read back).
fn defaults() -> BTreeMap<u32, u32> {
    BTreeMap::from([
        (PWR_CTRL1, 0),
        (PWR_CTRL2, 0),
        (GPIO_PAD_CONTROL, 0),
        (GLOBAL_CLK_CTRL, 0x03),
        (SP_ENABLES, 0),
        (SP_RATE_CTRL, 0x28),
        (SP_FORMAT, 0x1818_0200),
        (SP_HIZ_CTRL, 0x02),
        (SP_FRAME_RX_SLOT, 0x0100),
        (AMP_DIG_VOL_CTRL, 0x8000),
        (AMP_GAIN_CTRL, 0),
        (GPIO1_CTRL1, 0x8100_0001),
        (GPIO2_CTRL1, 0x8100_0001),
    ])
}

impl Cs35l41 {
    /// A revision B2 amplifier with OTP id 8, held in reset.
    pub fn new(name: &'static str) -> Cs35l41 {
        let mut otp = [0u32; OTP_WORDS];
        for (i, w) in otp.iter_mut().enumerate() {
            *w = (i as u32).wrapping_mul(0x9E37_79B9) ^ 0x0F0F_A5A5;
        }
        Cs35l41 {
            name,
            absent: false,
            device: CHIP_ID,
            revision: REVID_B2,
            otp_id: 8,
            otp,
            regs: defaults(),
            status1: 0,
            released: false,
            boots_at: None,
            key: [0; 2],
            selected: false,
            frame: Vec::new(),
            reading: 0,
            active: false,
            speaker_out: false,
            dsp: Halo::new(name),
            writes: Vec::new(),
            problems: Vec::new(),
        }
    }

    pub fn reg(&self, r: u32) -> u32 {
        self.regs.get(&r).copied().unwrap_or(0)
    }

    fn booted(&self, now: u64) -> bool {
        self.boots_at.is_some_and(|t| now >= t)
    }

    fn unlocked(&self) -> bool {
        self.key == [0x55, 0xAA]
    }

    /// Whether its speaker plays: powered up with the speaker output on,
    /// amplifier on, the external boost supply switched on, not muted, and
    /// the DAC fed: straight from the audio port, or by the DSP's firmware
    /// running (not paused) with the speaker's voltage and current
    /// measured for it.
    pub fn plays(&self) -> bool {
        let fed = match self.reg(DAC_PCM1_SRC) {
            SRC_ASPRX1 => true,
            SRC_DSP1TX1 => {
                self.dsp.running()
                    && self.dsp.status == crate::halo::STATUS_RUNNING
                    && self.reg(PWR_CTRL2) & (VMON_EN | IMON_EN) == VMON_EN | IMON_EN
            }
            _ => false,
        };
        self.active
            && self.speaker_out
            && fed
            && self.reg(PWR_CTRL2) & AMP_EN != 0
            && self.reg(GPIO1_CTRL1) == 0x8001
            && self.reg(AMP_DIG_VOL_CTRL) & 0x7FF != 0x678
    }

    /// The PCM gain (`AMP_GAIN_CTRL`'s field: 0.5 dB more than this).
    pub fn gain(&self) -> u32 {
        (self.reg(AMP_GAIN_CTRL) & AMP_GAIN_PCM_MASK) >> AMP_GAIN_PCM_SHIFT
    }

    /// What a register holds, the DSP's included, without side effects.
    pub fn peek(&self, r: u32) -> u32 {
        if Halo::has(r) { self.dsp.peek(r) } else { self.reg(r) }
    }

    /// Latches errors (as when it overheats or a speaker shorts).
    pub fn fail(&mut self, errors: u32) {
        self.status1 |= errors;
    }

    /// The reset line: low holds it in reset; released, it boots.
    pub fn set_reset(&mut self, released: bool, now: u64) {
        if released == self.released {
            return;
        }
        self.released = released;
        self.reset_registers();
        self.boots_at = released.then_some(now + BOOT_US);
    }

    fn reset_registers(&mut self) {
        self.regs = defaults();
        self.status1 = 0;
        self.key = [0; 2];
        self.active = false;
        self.speaker_out = false;
        self.dsp.reset();
    }

    /// Notes a problem (each once).
    fn note(&mut self, problem: String) {
        if !self.problems.contains(&problem) {
            self.problems.push(problem);
        }
    }

    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// The chip select (active low): a frame begins and ends with it.
    pub fn select(&mut self, asserted: bool) {
        if asserted == self.selected {
            return;
        }
        self.selected = asserted;
        let len = self.frame.len();
        if !asserted && len != 0 && (len < 10 || !(len - 6).is_multiple_of(4)) {
            self.note(format!("{}: a frame of {} bytes (6, then whole registers)", self.name, len));
        }
        self.frame.clear();
    }

    /// One byte shifted through its port while selected: the byte it
    /// shifts out.
    pub fn shift(&mut self, byte: u8, now: u64, clock: bool) -> u8 {
        if self.absent {
            return 0xFF;
        }
        if !self.released {
            self.note(format!("{}: talked to while held in reset", self.name));
            return 0xFF;
        }
        let i = self.frame.len();
        self.frame.push(byte);
        if i < 6 {
            return 0;
        }
        let f = &self.frame;
        let address = u32::from_be_bytes([f[0], f[1], f[2], f[3]]);
        // The register this byte belongs to, and where in it.
        let (register, at) = ((address & !(1 << 31)).wrapping_add(4 * ((i - 6) / 4) as u32), (i - 6) % 4);
        if address & 1 << 31 != 0 {
            if at == 0 {
                self.reading = self.read(register, now);
            }
            return self.reading.to_be_bytes()[at];
        }
        if at == 3 {
            let value = u32::from_be_bytes([f[i - 3], f[i - 2], f[i - 1], f[i]]);
            self.write(register, value, now, clock);
        }
        0
    }

    fn read(&mut self, r: u32, now: u64) -> u32 {
        let booted = self.booted(now);
        if !booted && r != IRQ1_STATUS4 {
            self.note(format!("{}: register {:#x} read before it booted", self.name, r));
        }
        match r {
            DEVID => self.device,
            REVID => self.revision,
            OTPID => self.otp_id,
            r if (OTP_MEM0..OTP_MEM0 + 4 * OTP_WORDS as u32).contains(&r) => self.otp[((r - OTP_MEM0) / 4) as usize],
            IRQ1_STATUS4 if booted => OTP_BOOT_DONE,
            IRQ1_STATUS4 | IRQ1_STATUS3 => 0,
            IRQ1_STATUS1 => self.status1,
            r if Halo::has(r) => self.dsp.read(r, now),
            r => self.reg(r),
        }
    }

    /// Notes a gain above Linux's without the DSP's protection in the path
    /// (the speakers could be driven past their limits).
    fn check_gain(&mut self) {
        let protected = self.reg(DAC_PCM1_SRC) == SRC_DSP1TX1 && self.dsp.running();
        if self.gain() > BYPASS_GAIN && !protected {
            let gain = self.gain();
            self.note(format!("{}: at {}.5 dB without its DSP's protection", self.name, gain));
        }
    }

    fn write(&mut self, r: u32, v: u32, now: u64, clock: bool) {
        if !self.booted(now) {
            self.note(format!("{}: register {:#x} written before it booted", self.name, r));
            return;
        }
        if protected(r) && !self.unlocked() {
            self.note(format!("{}: register {:#x} written with the test key locked", self.name, r));
            return;
        }
        self.writes.push((r, v));
        match r {
            TEST_KEY_CTL => self.key = [self.key[1], v],
            SFT_RESET if v == SOFTWARE_RESET => {
                self.reset_registers();
                self.boots_at = Some(now + BOOT_US);
            }
            IRQ1_STATUS1 => self.status1 &= !v,
            PWR_CTRL1 => {
                let on = v & GLOBAL_EN != 0;
                if on && self.reg(PWR_CTRL2) & BST_EN_MASK != 0 {
                    self.note(format!("{}: powered up with its own boost converter on", self.name));
                }
                // Up and down are sequenced by its PLL, which locks to the
                // port's bit clock: without it, neither completes.
                if clock && on != self.active {
                    self.active = on;
                    self.status1 |= if on { PUP_DONE } else { PDN_DONE };
                }
                if !on {
                    self.speaker_out = false;
                }
                self.regs.insert(r, v);
            }
            // The last step of powering up with an external boost supply
            // (`safe_to_active_en_spk`).
            0x742C if v == 0xF9 => {
                self.speaker_out = self.active;
                self.regs.insert(r, v);
            }
            DSP_VIRT1_MBOX_1 if v == 7 => {
                let told = self.dsp.takes_speaker_on();
                self.dsp.write(r, v, now);
                if told {
                    if !self.active {
                        self.note(format!("{}: its speaker output switched on before it powered up", self.name));
                    }
                    self.speaker_out = self.active;
                }
            }
            r if Halo::has(r) => self.dsp.write(r, v, now),
            AMP_GAIN_CTRL | DAC_PCM1_SRC => {
                self.regs.insert(r, v);
                self.check_gain();
            }
            _ => {
                self.regs.insert(r, v);
            }
        }
    }
}
