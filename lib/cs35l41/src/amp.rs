//! One amplifier: brought up from reset, its DSP firmware started, then
//! started and stopped with the codec's stream. The sequences and values
//! are Linux's (`cs35l41_hda.c`, `cs35l41-lib.c`), for a board with an
//! external boost circuit, with the firmware running or without it.

use alloc::vec::Vec;
use core::fmt;

use crate::dsp::{self, Command, DspError, Loaded};
use crate::regs::*;
use crate::wmfw::{Coefficients, Firmware};
use crate::{Bus, Channel, otp};

/// Why an amplifier could not be brought up, started or stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The register bus failed.
    Bus,
    /// It did not finish booting from its OTP memory.
    Boot,
    /// It booted with an OTP error (IRQ1_STATUS3).
    OtpBoot(u32),
    /// It is not a CS35L41: the device and revision ids read.
    Id { device: u32, revision: u32 },
    /// Its OTP id is one whose packing is not known.
    OtpMap(u32),
    /// It did not power up (`true`) or down.
    Power(bool),
    /// Its DSP or the firmware on it failed.
    Dsp(DspError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bus => f.write_str("its bus failed"),
            Error::Boot => f.write_str("it did not boot from its OTP memory"),
            Error::OtpBoot(s) => write!(f, "it reported an OTP error at boot ({:#010x})", s),
            Error::Id { device, revision } => {
                write!(f, "it is not a CS35L41 (device id {:#x}, revision {:#x})", device, revision)
            }
            Error::OtpMap(id) => write!(f, "its OTP id {} is not known", id),
            Error::Power(true) => f.write_str("it did not power up"),
            Error::Power(false) => f.write_str("it did not power down"),
            Error::Dsp(e) => write!(f, "{}", e),
        }
    }
}

/// Register writes, each with the time to wait after it (µs).
type Sequence = &'static [(u32, u32, u64)];

const UNLOCK: Sequence = &[(TEST_KEY_CTL, 0x55, 0), (TEST_KEY_CTL, 0xAA, 0)];
const LOCK: Sequence = &[(TEST_KEY_CTL, 0xCC, 0), (TEST_KEY_CTL, 0x33, 0)];

/// Silicon revision A0's fixes.
const ERRATA_A0: Sequence = &[
    (0x3854, 0x0518_0240, 0),
    (VIMON_SPKMON_RESYNC, 0, 0),
    (0x4310, 0, 0),
    (VPVBST_FS_SEL, 0, 0),
    (OTP_TRIM_30, 0x9091_A1C8, 0),
    (0x3014, 0x0200_EE0E, 0),
    (BSTCVRT_DCM_CTRL, 0x51, 0),
    (0x0054, 0x04, 0),
    (IRQ1_DB3, 0, 0),
    (IRQ2_DB3, 0, 0),
    (DSP1_YM_ACCEL_PL0_PRI, 0, 0),
    (DSP1_XM_ACCEL_PL0_PRI, 0, 0),
    (PWR_CTRL2, 0, 0),
    (AMP_GAIN_CTRL, 0, 0),
    (ASP_TX3_SRC, 0, 0),
    (ASP_TX4_SRC, 0, 0),
];

/// Revisions B0 and B2.
const ERRATA_B: Sequence = &[
    (VIMON_SPKMON_RESYNC, 0, 0),
    (0x4310, 0, 0),
    (VPVBST_FS_SEL, 0, 0),
    (BSTCVRT_DCM_CTRL, 0x51, 0),
    (DSP1_YM_ACCEL_PL0_PRI, 0, 0),
    (DSP1_XM_ACCEL_PL0_PRI, 0, 0),
    (PWR_CTRL2, 0, 0),
    (AMP_GAIN_CTRL, 0, 0),
    (ASP_TX3_SRC, 0, 0),
    (ASP_TX4_SRC, 0, 0),
];

/// From reset to the safe state external boost needs (`reset_to_safe`).
const RESET_TO_SAFE: Sequence = &[
    (TEST_KEY_CTL, 0x55, 0),
    (TEST_KEY_CTL, 0xAA, 0),
    (0x7438, 0x0058_5941, 0),
    (0x7414, 0x08C8_2222, 0),
    (0x742C, 0x09, 0),
    (TEST_KEY_CTL, 0xCC, 0),
    (TEST_KEY_CTL, 0x33, 0),
];

/// Back from the safe state to one where reset is harmless.
const SAFE_TO_RESET: Sequence = &[
    (TEST_KEY_CTL, 0x55, 0),
    (TEST_KEY_CTL, 0xAA, 0),
    (0x393C, 0xC0, 6000),
    (0x393C, 0x00, 0),
    (0x7414, 0x00C8_2222, 0),
    (0x742C, 0x00, 0),
    (TEST_KEY_CTL, 0xCC, 0),
    (TEST_KEY_CTL, 0x33, 0),
];

/// Global enable with external boost: the test key stays unlocked until
/// the speaker output is on.
const SAFE_TO_ACTIVE_START: Sequence = &[
    (TEST_KEY_CTL, 0x55, 0),
    (TEST_KEY_CTL, 0xAA, 0),
    (0x742C, 0x0F, 0),
    (0x742C, 0x79, 0),
    (0x7438, 0x0058_5941, 0),
    (PWR_CTRL1, GLOBAL_EN, 0),
];
const SAFE_TO_ACTIVE_SPEAKER: Sequence = &[(0x742C, 0xF9, 0), (0x7438, 0x0058_0941, 0)];
const ACTIVE_TO_SAFE_START: Sequence =
    &[(TEST_KEY_CTL, 0x55, 0), (TEST_KEY_CTL, 0xAA, 0), (0x7438, 0x0058_5941, 0), (PWR_CTRL1, 0, 0), (0x742C, 0x09, 0)];
const ACTIVE_TO_SAFE_END: Sequence = &[(0x7438, 0x0058_0941, 0), (TEST_KEY_CTL, 0xCC, 0), (TEST_KEY_CTL, 0x33, 0)];

/// The audio port and clocks, for the codec's I2S: 48 kHz, two 32-bit
/// slots (a 3.072 MHz bit clock the amplifier's PLL locks to), 24-bit
/// samples, the codec driving the clocks.
const HDA_CONFIG: Sequence = &[
    (PLL_CLK_CTRL, 0x0430, 0),
    (DSP_CLK_CTRL, 0x03, 0),
    (GLOBAL_CLK_CTRL, 0x03, 0),
    (SP_RATE_CTRL, 0x21, 0),
    (SP_FORMAT, 0x2020_0200, 0),
    (SP_TX_WL, 0x18, 0),
    (SP_RX_WL, 0x18, 0),
    // Voltage and current monitoring out of the port, unused here.
    (ASP_TX1_SRC, 0x18, 0),
    (ASP_TX2_SRC, 0x19, 0),
    (DSP1_RX3_SRC, 0x18, 0),
    (DSP1_RX4_SRC, 0x19, 0),
];

/// Without DSP firmware: the port's first channel straight to the DAC.
const HDA_CONFIG_NO_DSP: Sequence = &[
    (SP_HIZ_CTRL, 0x02, 0),
    (DAC_PCM1_SRC, SRC_ASPRX1, 0),
    (ASP_TX3_SRC, 0x00, 0),
    (ASP_TX4_SRC, 0x00, 0),
    (DSP1_RX5_SRC, 0x20, 0),
    (DSP1_RX6_SRC, 0x21, 0),
];

/// With DSP firmware: the DSP's first output to the DAC, the supply and
/// boost voltages out of the port and into the DSP.
const HDA_CONFIG_DSP: Sequence = &[
    (SP_HIZ_CTRL, 0x03, 0),
    (DAC_PCM1_SRC, SRC_DSP1TX1, 0),
    (ASP_TX3_SRC, 0x28, 0),
    (ASP_TX4_SRC, SRC_VBSTMON, 0),
    (DSP1_RX6_SRC, SRC_VBSTMON, 0),
];

/// High-pass filter on, 0 dB, and the amplifier at 4.5 dB.
const UNMUTE: Sequence = &[(AMP_DIG_VOL_CTRL, 0x8000, 0), (AMP_GAIN_CTRL, 0x84, 0)];
const MUTE: Sequence = &[(AMP_GAIN_CTRL, 0x00, 0), (AMP_DIG_VOL_CTRL, 0xA678, 0)];

/// The PCM gain with DSP firmware, unless its tuning parameters say
/// otherwise (17.5 dB: the firmware's protection keeps the speakers safe
/// at it), and the PDM path's (unused).
pub const DSP_GAIN_PCM: u32 = 17;
const DSP_GAIN_PDM: u32 = 19;
/// Firmware after this version switches the speaker output on itself
/// (`CS35L41_FIRMWARE_OLD_VERSION`, v0.28.0).
const FIRMWARE_OLD_VERSION: u32 = 0x00_1C00;

/// GPIO1 switches the external boost supply: off, and on while playing.
const VSPK_OFF: u32 = 0x0000_0001;
const VSPK_ON: u32 = 0x0000_8001;

/// Polling for boot and power transitions: every millisecond, for 100.
const POLL_US: u64 = 1000;
const POLL_TIMEOUT_US: u64 = 100_000;

fn run(bus: &mut impl Bus, sequence: Sequence) -> Result<(), Error> {
    for &(register, value, delay) in sequence {
        bus.write(register, value)?;
        if delay > 0 {
            bus.sleep_us(delay);
        }
    }
    Ok(())
}

pub(crate) fn update(bus: &mut impl Bus, register: u32, mask: u32, value: u32) -> Result<(), Error> {
    let old = bus.read(register)?;
    bus.write(register, (old & !mask) | (value & mask))
}

/// Waits for any of `bits` in `register`: the value, or `None` on timeout.
fn poll(bus: &mut impl Bus, register: u32, bits: u32) -> Result<Option<u32>, Error> {
    let mut waited = 0;
    loop {
        let v = bus.read(register)?;
        if v & bits != 0 {
            return Ok(Some(v));
        }
        if waited >= POLL_TIMEOUT_US {
            return Ok(None);
        }
        bus.sleep_us(POLL_US);
        waited += POLL_US;
    }
}

/// The firmware running on an amplifier's DSP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dsp {
    /// The firmware's version (`0x00MMmmpp`).
    pub version: u32,
    /// The PCM gain it plays at (`AMP_GAIN_CTRL`'s field: 0.5 dB more
    /// than this).
    pub gain: u32,
}

/// An amplifier that has been brought up.
#[derive(Debug, Clone)]
pub struct Amp {
    pub channel: Channel,
    /// Silicon revision (`0xB2`).
    pub revision: u32,
    pub otp_id: u32,
    /// Trims written from its OTP memory.
    pub trims: usize,
    playing: bool,
    muted: bool,
    /// Errors it reported while playing, released when it stops.
    errors: u32,
    /// Its DSP's firmware, running (paused while nothing plays); without
    /// it the codec's audio goes straight to the amplifier.
    dsp: Option<Dsp>,
}

impl Amp {
    /// Brings up an amplifier just out of hardware reset: a software reset,
    /// its boot from OTP, its id, its revision's fixes and its trims (with
    /// the test key unlocked), then the safe state of an external boost
    /// circuit, its GPIOs (GPIO1 switches the boost supply, GPIO2 is its
    /// interrupt output) and the slot of the port it plays.
    pub fn probe(bus: &mut impl Bus, channel: Channel) -> Result<Amp, Error> {
        bus.write(SFT_RESET, SOFTWARE_RESET)?;
        bus.sleep_us(2000);
        poll(bus, IRQ1_STATUS4, OTP_BOOT_DONE)?.ok_or(Error::Boot)?;
        let status = bus.read(IRQ1_STATUS3)?;
        if status & OTP_BOOT_ERR != 0 {
            return Err(Error::OtpBoot(status));
        }
        let device = bus.read(DEVID)?;
        let revision = bus.read(REVID)?;
        let expected = if (revision & MTLREVID_MASK) % 2 == 1 { CHIP_ID_R } else { CHIP_ID };
        if device != expected {
            return Err(Error::Id { device, revision });
        }

        run(bus, UNLOCK)?;
        // An unknown revision gets no fixes (as on Linux, which carries on).
        match revision {
            REVID_A0 => run(bus, ERRATA_A0)?,
            REVID_B0 | REVID_B2 => run(bus, ERRATA_B)?,
            _ => {}
        }
        bus.write(DSP1_CCM_CORE_CTRL, 0)?;
        let otp_id = bus.read(OTPID)?;
        let mut words = [0u32; OTP_WORDS];
        for (i, w) in words.iter_mut().enumerate() {
            *w = bus.read(OTP_MEM0 + 4 * i as u32)?;
        }
        let trims = otp::unpack(otp_id, &words).ok_or(Error::OtpMap(otp_id))?;
        for t in &trims {
            update(bus, t.register, t.mask, t.value)?;
        }
        run(bus, LOCK)?;

        // External boost: its switch off, the safe state, the internal
        // boost converter off.
        bus.write(GPIO1_CTRL1, VSPK_OFF)?;
        run(bus, RESET_TO_SAFE)?;
        update(bus, PWR_CTRL2, BST_EN_MASK, 0)?;
        // GPIO1 an output (the switch), GPIO2 an input, neither inverted;
        // then their functions.
        update(bus, GPIO1_CTRL1, GPIO_POL | GPIO_DIR, 0)?;
        update(bus, GPIO2_CTRL1, GPIO_POL | GPIO_DIR, GPIO_DIR)?;
        update(bus, GPIO_PAD_CONTROL, GPIO1_CTRL_MASK, GPIO1_GPIO << GPIO1_CTRL_SHIFT)?;
        update(bus, GPIO_PAD_CONTROL, GPIO2_CTRL_MASK, GPIO2_INT_OPEN_DRAIN << GPIO2_CTRL_SHIFT)?;
        // The port's slot it takes its samples from.
        let slot = match channel {
            Channel::Right => 1,
            Channel::Left | Channel::Center => 0,
        };
        update(bus, SP_FRAME_RX_SLOT, 0x3F, slot)?;
        Ok(Amp { channel, revision, otp_id, trims: trims.len(), playing: false, muted: false, errors: 0, dsp: None })
    }

    /// Loads DSP firmware and its tuning into the amplifier and starts it,
    /// paused until playback starts (`cs35l41_smart_amp`); with it the
    /// amplifier plays at PCM gain `gain`. If anything fails, the DSP is
    /// stopped again and the amplifier plays without it, as on Linux.
    pub fn start_firmware(
        &mut self,
        bus: &mut impl Bus,
        firmware: &Firmware,
        tuning: Option<&Coefficients>,
        gain: u32,
    ) -> Result<Loaded, Error> {
        let started = dsp::load(bus, firmware, tuning).and_then(|loaded| dsp::start(bus, &loaded).map(|_| loaded));
        match started {
            Ok(loaded) => {
                self.dsp = Some(Dsp { version: loaded.version, gain });
                Ok(loaded)
            }
            Err(e) => {
                self.dsp = None;
                let _ = dsp::stop(bus);
                Err(e)
            }
        }
    }

    /// Its DSP's firmware, if it runs.
    pub fn dsp(&self) -> Option<Dsp> {
        self.dsp
    }

    /// Before the stream starts: the port, clocks and routing (through
    /// the DSP if its firmware runs, which then measures the speaker and
    /// is resumed), the amplifier on, the boost supply switched on.
    pub fn play_start(&mut self, bus: &mut impl Bus) -> Result<(), Error> {
        if self.playing {
            return Ok(());
        }
        self.playing = true;
        run(bus, HDA_CONFIG)?;
        let mut enables = ASP_RX1_EN;
        if self.dsp.is_some() {
            enables |= ASP_TX1_EN;
            run(bus, HDA_CONFIG_DSP)?;
            // The boost voltage, of the external boost supply.
            bus.write(DSP1_RX5_SRC, SRC_VBSTMON)?;
        } else {
            run(bus, HDA_CONFIG_NO_DSP)?;
        }
        let rx2 = match self.channel {
            Channel::Center => {
                enables |= ASP_RX2_EN;
                SRC_ASPRX2
            }
            _ => SRC_ASPRX1,
        };
        bus.write(SP_ENABLES, enables)?;
        bus.write(DSP1_RX1_SRC, SRC_ASPRX1)?;
        bus.write(DSP1_RX2_SRC, rx2)?;
        // As on Linux, a firmware that does not resume is reported once
        // the rest is done.
        let resumed = match self.dsp {
            Some(_) => {
                update(bus, PWR_CTRL2, VMON_EN | IMON_EN, VMON_EN | IMON_EN)?;
                dsp::mailbox(bus, Command::Resume)
            }
            None => Ok(()),
        };
        update(bus, PWR_CTRL2, AMP_EN, AMP_EN)?;
        bus.write(GPIO1_CTRL1, VSPK_ON)?;
        resumed
    }

    /// Once the codec clocks the port: power up (global enable), then the
    /// sound on.
    pub fn play_done(&mut self, bus: &mut impl Bus) -> Result<(), Error> {
        self.global_enable(bus, true)?;
        self.apply_mute(bus)
    }

    /// Before the stream stops: the sound off, then power down while the
    /// codec still clocks the port.
    pub fn pause_start(&mut self, bus: &mut impl Bus) -> Result<(), Error> {
        let muted = self.muted;
        self.muted = true;
        let r = self.apply_mute(bus);
        self.muted = muted;
        r?;
        self.global_enable(bus, false)
    }

    /// Once stopped: the amplifier off, the boost supply switched off, the
    /// firmware paused (and the speaker no longer measured), and any errors
    /// released.
    pub fn pause_done(&mut self, bus: &mut impl Bus) -> Result<(), Error> {
        update(bus, PWR_CTRL2, AMP_EN, 0)?;
        bus.write(GPIO1_CTRL1, VSPK_OFF)?;
        let paused = match self.dsp {
            Some(_) => {
                let r = dsp::mailbox(bus, Command::Pause);
                update(bus, PWR_CTRL2, VMON_EN | IMON_EN, 0)?;
                r
            }
            None => Ok(()),
        };
        bus.write(PROTECT_REL_ERR_IGN, 0)?;
        let release = release_bits(self.errors);
        if release != 0 {
            bus.write(PROTECT_REL_ERR_IGN, release)?;
            bus.write(PROTECT_REL_ERR_IGN, 0)?;
        }
        self.errors = 0;
        self.playing = false;
        paused
    }

    /// Mutes or unmutes the speaker (taking effect while playing).
    pub fn mute(&mut self, bus: &mut impl Bus, muted: bool) -> Result<(), Error> {
        self.muted = muted;
        self.apply_mute(bus)
    }

    fn apply_mute(&mut self, bus: &mut impl Bus) -> Result<(), Error> {
        if !self.playing {
            return Ok(());
        }
        match self.dsp {
            _ if self.muted => run(bus, MUTE),
            // High-pass filter on, 0 dB, and the gain the firmware's
            // protection allows.
            Some(d) => {
                bus.write(AMP_DIG_VOL_CTRL, 0x8000)?;
                bus.write(AMP_GAIN_CTRL, (d.gain & 0x1F) << AMP_GAIN_PCM_SHIFT | DSP_GAIN_PDM)
            }
            None => run(bus, UNMUTE),
        }
    }

    /// The errors it has latched since the last look (it shuts itself down
    /// on them); they are released when playback stops.
    pub fn take_errors(&mut self, bus: &mut impl Bus) -> Result<u32, Error> {
        let errors = bus.read(IRQ1_STATUS1)? & ERRORS;
        if errors != 0 {
            bus.write(IRQ1_STATUS1, errors)?;
            self.errors |= errors;
        }
        Ok(errors)
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    fn global_enable(&mut self, bus: &mut impl Bus, on: bool) -> Result<(), Error> {
        let enabled = bus.read(PWR_CTRL1)? & GLOBAL_EN != 0;
        if enabled == on {
            return Ok(());
        }
        let (start, done) = if on { (SAFE_TO_ACTIVE_START, PUP_DONE) } else { (ACTIVE_TO_SAFE_START, PDN_DONE) };
        run(bus, start)?;
        if poll(bus, IRQ1_STATUS1, done)?.is_none() {
            run(bus, LOCK)?;
            return Err(Error::Power(on));
        }
        bus.write(IRQ1_STATUS1, done)?;
        if !on {
            return run(bus, ACTIVE_TO_SAFE_END);
        }
        // The speaker output on: by the firmware, if it does that, or here.
        let r = match self.dsp {
            Some(d) if d.version > FIRMWARE_OLD_VERSION => dsp::mailbox(bus, Command::SpeakerOutEnable),
            _ => run(bus, SAFE_TO_ACTIVE_SPEAKER),
        };
        run(bus, LOCK)?;
        r
    }
}

/// Puts an amplifier that will be held in reset into a state where that is
/// harmless (its boost supply switched off first).
pub fn safe_reset(bus: &mut impl Bus) -> Result<(), Error> {
    bus.write(GPIO1_CTRL1, VSPK_OFF)?;
    run(bus, SAFE_TO_RESET)
}

/// The release bits for latched errors.
fn release_bits(errors: u32) -> u32 {
    let mut r = 0;
    for (error, release) in [
        (AMP_SHORT_ERR, AMP_SHORT_ERR_RLS),
        (BST_SHORT_ERR, BST_SHORT_ERR_RLS),
        (BST_OVP_ERR, BST_OVP_ERR_RLS),
        (BST_DCM_UVP_ERR, BST_UVP_ERR_RLS),
        (TEMP_WARN, TEMP_WARN_ERR_RLS),
        (TEMP_ERR, TEMP_ERR_RLS),
    ] {
        if errors & error != 0 {
            r |= release;
        }
    }
    r
}

/// Names of the errors in `errors`, for the log.
pub fn error_names(errors: u32) -> Vec<&'static str> {
    [
        (AMP_SHORT_ERR, "speaker short"),
        (BST_SHORT_ERR, "boost inductor short"),
        (BST_OVP_ERR, "boost overvoltage"),
        (BST_DCM_UVP_ERR, "boost undervoltage"),
        (TEMP_WARN, "temperature warning"),
        (TEMP_ERR, "overheated"),
    ]
    .iter()
    .filter(|(bit, _)| errors & bit != 0)
    .map(|&(_, name)| name)
    .collect()
}
