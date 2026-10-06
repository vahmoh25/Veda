use alloc::vec::Vec;
use std::collections::BTreeMap;

use crate::amp::{self, Amp, Error};
use crate::config::{self, Boost};
use crate::otp;
use crate::regs::*;
use crate::wmfw::{self, FormatError, Memory, Place, Target};
use crate::{Bus, Channel};
use crate::{dsp, group};

/// An amplifier as its registers behave: it boots after a software reset,
/// powers up and down when told (latching `PUP_DONE`/`PDN_DONE`, which
/// are cleared by writing them), and keeps a log of the writes.
struct Sim {
    regs: BTreeMap<u32, u32>,
    /// What the registers hold after a reset.
    defaults: BTreeMap<u32, u32>,
    writes: Vec<(u32, u32)>,
    device: u32,
    revision: u32,
    otp_id: u32,
    otp: [u32; OTP_WORDS],
    booted: bool,
    boots: bool,
    powers: bool,
    status1: u32,
    key: Vec<u32>,
    slept_us: u64,
}

impl Sim {
    fn new() -> Sim {
        Sim {
            regs: BTreeMap::new(),
            defaults: BTreeMap::new(),
            writes: Vec::new(),
            device: CHIP_ID,
            revision: REVID_B2,
            otp_id: 0x08,
            otp: [0; OTP_WORDS],
            booted: false,
            boots: true,
            powers: true,
            status1: 0,
            key: Vec::new(),
            slept_us: 0,
        }
    }

    fn reg(&self, r: u32) -> u32 {
        self.regs.get(&r).copied().unwrap_or(0)
    }

    /// Whether the test key is unlocked (the last two key writes).
    fn unlocked(&self) -> bool {
        self.key.ends_with(&[0x55, 0xAA])
    }

    fn writes_to(&self, r: u32) -> Vec<u32> {
        self.writes.iter().filter(|(x, _)| *x == r).map(|(_, v)| *v).collect()
    }
}

impl Bus for Sim {
    fn read(&mut self, r: u32) -> Result<u32, Error> {
        Ok(match r {
            DEVID => self.device,
            REVID => self.revision,
            OTPID => self.otp_id,
            r if (OTP_MEM0..OTP_MEM0 + 4 * OTP_WORDS as u32).contains(&r) => self.otp[((r - OTP_MEM0) / 4) as usize],
            IRQ1_STATUS4 => {
                if self.booted {
                    OTP_BOOT_DONE
                } else {
                    0
                }
            }
            IRQ1_STATUS1 => self.status1,
            r => self.reg(r),
        })
    }

    fn write(&mut self, r: u32, v: u32) -> Result<(), Error> {
        self.writes.push((r, v));
        match r {
            SFT_RESET if v == SOFTWARE_RESET => {
                self.regs = self.defaults.clone();
                self.booted = self.boots;
            }
            TEST_KEY_CTL => self.key.push(v),
            IRQ1_STATUS1 => self.status1 &= !v,
            PWR_CTRL1 => {
                if self.powers {
                    self.status1 |= if v & GLOBAL_EN != 0 { PUP_DONE } else { PDN_DONE };
                }
                self.regs.insert(r, v);
            }
            _ => {
                self.regs.insert(r, v);
            }
        }
        Ok(())
    }

    fn sleep_us(&mut self, us: u64) {
        self.slept_us += us;
    }
}

/// OTP memory with `values` packed in, element after element, from word
/// 2 bit 16 (as the factory writes them), for the map of OTP id 8.
fn packed(values: &[(u32, u32)]) -> [u32; OTP_WORDS] {
    let mut words = [0u32; OTP_WORDS];
    let mut bit = 2 * 32 + 16;
    for &(value, size) in values {
        for i in 0..size {
            if value >> i & 1 != 0 {
                words[bit / 32] |= 1 << (bit % 32);
            }
            bit += 1;
        }
    }
    words
}

/// The element sizes of the OTP map, in order.
const SIZES: [u32; 99] = [
    4, 1, 6, 4, 4, 4, 2, 7, 7, 8, 8, 7, 7, 8, 8, 7, 7, 8, 8, 7, 7, 8, 8, 8, 7, 8, 10, 12, 1, 6, 1, 6, 1, 9, 5, 9, 8, 8,
    8, 8, 3, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 5, 5, 4, 4, 1, 7, 9, 1, 7, 1, 8, 8, 8, 8, 24,
];

#[test]
fn otp_trims_unpack_across_words() {
    // A distinct value in each element (cut to its width).
    let values: Vec<(u32, u32)> = SIZES
        .iter()
        .enumerate()
        .map(|(i, &s)| (((i as u32).wrapping_mul(0x9E37_79B9) ^ 0x5A5A_5A5A) & ((1u64 << s) - 1) as u32, s))
        .collect();
    assert_eq!(SIZES.iter().sum::<u32>(), 688);
    let words = packed(&values);
    let trims = otp::unpack(0x08, &words).unwrap();
    // Element 93 is a spare bit for this OTP id: not written.
    assert_eq!(trims.len(), 98);
    let first = trims[0];
    assert_eq!((first.register, first.mask, first.value), (0x2030, 0xF, values[0].0));
    let lot = trims[97];
    assert_eq!((lot.register, lot.mask, lot.value), (0x17044, 0xFF_FFFF, values[98].0));
    // An element that straddles two words: IMON_OFFSET (10 bits at 16).
    let imon = trims[26];
    assert_eq!((imon.register, imon.mask, imon.value), (0x4160, 0x3FF << 16, values[26].0 << 16));
    // Every trim is the value packed for its element.
    let written: Vec<u32> = (0..99).filter(|&i| i != 93).map(|i| values[i].0).collect();
    for (t, v) in trims.iter().zip(&written) {
        assert_eq!(t.value >> t.mask.trailing_zeros(), *v, "{:#x}", t.register);
    }
    // OTP ids 2, 3 and 6 have VMON_POL in that bit; unknown ids nothing.
    let trims = otp::unpack(0x02, &words).unwrap();
    assert_eq!(trims.len(), 99);
    assert_eq!((trims[93].register, trims[93].mask), (0x4000, 1 << 11));
    assert!(otp::unpack(0x05, &words).is_none());
}

#[test]
fn probe_brings_an_amplifier_up() {
    let mut sim = Sim::new();
    sim.otp = packed(&SIZES.iter().map(|&s| (1u32, s)).collect::<Vec<_>>());
    sim.defaults.insert(0x2030, 0xFFFF_FF00);
    let amp = Amp::probe(&mut sim, Channel::Right).unwrap();
    assert_eq!((amp.revision, amp.otp_id, amp.trims), (REVID_B2, 0x08, 98));
    // Booted, fixed for revision B2, trimmed, locked again.
    assert_eq!(sim.writes[0], (SFT_RESET, SOFTWARE_RESET));
    assert!(sim.slept_us >= 2000);
    assert_eq!(sim.reg(BSTCVRT_DCM_CTRL), 0x51);
    assert_eq!(sim.reg(DSP1_CCM_CORE_CTRL), 0);
    // Both 0x2030 trims (bits 0-3 and 7) kept the rest of the register.
    assert_eq!(sim.reg(0x2030), 0xFFFF_FF00 & !0x8F | 0x81);
    assert!(!sim.unlocked());
    // External boost: switch off, safe state, own converter off.
    assert_eq!(sim.writes_to(GPIO1_CTRL1)[0], 0x0000_0001);
    assert_eq!(sim.reg(0x7414), 0x08C8_2222);
    assert_eq!(sim.reg(PWR_CTRL2) & BST_EN_MASK, 0);
    assert_eq!(sim.reg(GPIO_PAD_CONTROL), 0x0201_0000);
    assert_eq!(sim.reg(GPIO1_CTRL1), 0x0000_0001);
    assert_eq!(sim.reg(GPIO2_CTRL1) & (GPIO_DIR | GPIO_POL), GPIO_DIR);
    // The right channel plays slot 1.
    assert_eq!(sim.reg(SP_FRAME_RX_SLOT) & 0x3F, 1);
}

#[test]
fn probe_refuses_what_is_not_a_working_cs35l41() {
    let mut sim = Sim::new();
    sim.device = 0x1234;
    assert_eq!(Amp::probe(&mut sim, Channel::Left).unwrap_err(), Error::Id { device: 0x1234, revision: REVID_B2 });
    // Nothing but the reset was written.
    assert_eq!(sim.writes, [(SFT_RESET, SOFTWARE_RESET)]);

    let mut sim = Sim::new();
    sim.boots = false;
    assert_eq!(Amp::probe(&mut sim, Channel::Left).unwrap_err(), Error::Boot);
    assert!(sim.slept_us >= 100_000);

    let mut sim = Sim::new();
    sim.defaults.insert(IRQ1_STATUS3, OTP_BOOT_ERR);
    assert_eq!(Amp::probe(&mut sim, Channel::Left).unwrap_err(), Error::OtpBoot(OTP_BOOT_ERR));

    // A CS35L41R (odd metal revision) has its own id.
    let mut sim = Sim::new();
    sim.revision = 0xB1;
    sim.device = CHIP_ID_R;
    assert!(Amp::probe(&mut sim, Channel::Left).is_ok());
}

#[test]
fn playing_and_stopping() {
    let mut sim = Sim::new();
    let mut amp = Amp::probe(&mut sim, Channel::Left).unwrap();
    sim.writes.clear();

    amp.play_start(&mut sim).unwrap();
    assert_eq!(sim.reg(PLL_CLK_CTRL), 0x430);
    assert_eq!(sim.reg(SP_FORMAT), 0x2020_0200);
    assert_eq!(sim.reg(DAC_PCM1_SRC), 0x08);
    assert_eq!(sim.reg(SP_ENABLES), ASP_RX1_EN);
    assert_eq!(sim.reg(PWR_CTRL2) & AMP_EN, AMP_EN);
    assert_eq!(sim.reg(GPIO1_CTRL1), 0x8001);
    assert_eq!(sim.reg(PWR_CTRL1), 0);

    let before = sim.writes.len();
    amp.play_done(&mut sim).unwrap();
    // Linux's global enable for external boost, then the unmute.
    assert_eq!(
        sim.writes[before..],
        [
            (TEST_KEY_CTL, 0x55),
            (TEST_KEY_CTL, 0xAA),
            (0x742C, 0x0F),
            (0x742C, 0x79),
            (0x7438, 0x0058_5941),
            (PWR_CTRL1, GLOBAL_EN),
            (IRQ1_STATUS1, PUP_DONE),
            (0x742C, 0xF9),
            (0x7438, 0x0058_0941),
            (TEST_KEY_CTL, 0xCC),
            (TEST_KEY_CTL, 0x33),
            (AMP_DIG_VOL_CTRL, 0x8000),
            (AMP_GAIN_CTRL, 0x84),
        ]
    );
    assert_eq!(sim.status1, 0);
    // Enabling again does nothing.
    let before = sim.writes.len();
    amp.play_done(&mut sim).unwrap();
    assert_eq!(sim.writes.len(), before + 2);

    // Headphones: muted while playing, and it stays so across a restart.
    amp.mute(&mut sim, true).unwrap();
    assert_eq!((sim.reg(AMP_GAIN_CTRL), sim.reg(AMP_DIG_VOL_CTRL)), (0, 0xA678));

    let before = sim.writes.len();
    amp.pause_start(&mut sim).unwrap();
    assert_eq!(
        sim.writes[before..],
        [
            (AMP_GAIN_CTRL, 0),
            (AMP_DIG_VOL_CTRL, 0xA678),
            (TEST_KEY_CTL, 0x55),
            (TEST_KEY_CTL, 0xAA),
            (0x7438, 0x0058_5941),
            (PWR_CTRL1, 0),
            (0x742C, 0x09),
            (IRQ1_STATUS1, PDN_DONE),
            (0x7438, 0x0058_0941),
            (TEST_KEY_CTL, 0xCC),
            (TEST_KEY_CTL, 0x33),
        ]
    );
    amp.pause_done(&mut sim).unwrap();
    assert_eq!(sim.reg(PWR_CTRL2) & AMP_EN, 0);
    assert_eq!(sim.reg(GPIO1_CTRL1), 0x0001);
    assert!(!amp.playing());

    amp.play_start(&mut sim).unwrap();
    amp.play_done(&mut sim).unwrap();
    assert_eq!((sim.reg(AMP_GAIN_CTRL), sim.reg(AMP_DIG_VOL_CTRL)), (0, 0xA678));
    amp.mute(&mut sim, false).unwrap();
    assert_eq!((sim.reg(AMP_GAIN_CTRL), sim.reg(AMP_DIG_VOL_CTRL)), (0x84, 0x8000));
}

#[test]
fn a_failed_power_up_leaves_the_key_locked() {
    let mut sim = Sim::new();
    let mut amp = Amp::probe(&mut sim, Channel::Left).unwrap();
    sim.powers = false;
    amp.play_start(&mut sim).unwrap();
    assert_eq!(amp.play_done(&mut sim).unwrap_err(), Error::Power(true));
    assert!(!sim.unlocked());
}

#[test]
fn errors_are_acknowledged_and_released() {
    let mut sim = Sim::new();
    let mut amp = Amp::probe(&mut sim, Channel::Left).unwrap();
    amp.play_start(&mut sim).unwrap();
    amp.play_done(&mut sim).unwrap();
    sim.status1 = TEMP_ERR | TEMP_WARN | PUP_DONE;
    assert_eq!(amp.take_errors(&mut sim).unwrap(), TEMP_ERR | TEMP_WARN);
    assert_eq!(sim.status1, PUP_DONE);
    assert_eq!(amp::error_names(TEMP_ERR | TEMP_WARN), ["temperature warning", "overheated"]);
    amp.pause_start(&mut sim).unwrap();
    sim.writes.clear();
    amp.pause_done(&mut sim).unwrap();
    assert_eq!(sim.writes_to(PROTECT_REL_ERR_IGN), [0, TEMP_ERR_RLS | TEMP_WARN_ERR_RLS, 0]);
}

#[test]
fn the_zenbook_pro_16x() {
    let c = config::find("10431f62").unwrap();
    assert_eq!((c.amps, c.boost), (2, Boost::External));
    assert_eq!((c.reset_gpio, c.speaker_id_gpio, c.cs_gpio), (Some(1), Some(2), Some(0)));
    assert_eq!(c.channels[..2], [Channel::Left, Channel::Right]);
    assert!(config::find("10431F63").is_none());
    assert_eq!(config::CONFIGS.len(), 102);
    // Safe reset switches the boost supply off first.
    let mut sim = Sim::new();
    amp::safe_reset(&mut sim).unwrap();
    assert_eq!(sim.writes[0], (GPIO1_CTRL1, 1));
    assert_eq!(sim.writes[3], (0x393C, 0xC0));
    assert_eq!(sim.slept_us, 6000);
}

const FIRMWARE: &[u8] = include_bytes!("../../../assets/firmware/cirrus/cs35l41-dsp1-spk-prot-10431f62.wmfw");
const TUNING_LEFT: &[u8] =
    include_bytes!("../../../assets/firmware/cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-l0.bin");
const TUNING_RIGHT: &[u8] =
    include_bytes!("../../../assets/firmware/cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-r0.bin");

#[test]
fn the_speaker_protection_firmware() {
    let fw = wmfw::firmware(FIRMWARE).unwrap();
    assert_eq!(fw.timestamp, 0x6128_E16B);
    assert_eq!(fw.text, [&b"Fri 27 Aug 2021 14:58:19 W. Europe Daylight Time"[..]]);
    // Its X memory (from word 0: its header and algorithm list), Y memory
    // and program memory.
    let places: Vec<(Place, usize)> = fw.blocks.iter().map(|b| (b.place, b.data.len())).collect();
    assert_eq!(
        places,
        [
            (Place::Memory(Memory::XmPacked, 0), 72),
            (Place::Memory(Memory::XmPacked, 0x34), 528),
            (Place::Memory(Memory::XmPacked, 0x144), 1596),
            (Place::Memory(Memory::XmPacked, 0x35C), 1260),
            (Place::Memory(Memory::YmPacked, 0), 6000),
            (Place::Memory(Memory::Pm, 0), 20420),
        ]
    );
    assert!(fw.skipped.is_empty());
    // Its algorithms' controls: the firmware's own (its state), the speaker
    // protection's (CSPL, with the calibration Linux can write), the event
    // logger's.
    assert_eq!(fw.controls.len(), 12 + 36 + 13);
    let state = fw.control("HALO_STATE", wmfw::XM, 0x400A4).unwrap();
    assert_eq!((state.offset, state.flags, state.len), (0, wmfw::FLAG_VOLATILE | wmfw::FLAG_READABLE, 4));
    let cal: Vec<u32> = ["CAL_R", "CAL_AMBIENT", "CAL_STATUS", "CAL_CHECKSUM"]
        .iter()
        .map(|n| fw.control(n, wmfw::XM, 0xCD).unwrap().offset)
        .collect();
    assert_eq!(cal, [6, 7, 8, 9]);
    assert_eq!(fw.control("CSPL_UPDATE_PARAMS_CONFIG", wmfw::YM, 0xCD).unwrap().len, 400);
}

#[test]
fn asus_tunings() {
    let left = wmfw::coefficients(TUNING_LEFT).unwrap();
    assert_eq!((left.format, left.version), (1, 0x0100_2B01));
    assert!(left.name.unwrap().ends_with(b"\\UX7602_tooling_sample_Lch_RTL.bin"));
    let speaker_protection = |offset, memory| Target::Algorithm { memory, algorithm: 0xCD, version: 0x1D_3F01, offset };
    let blocks: Vec<(Target, usize)> = left.blocks.iter().map(|b| (b.target, b.data.len())).collect();
    assert_eq!(blocks, [(speaker_protection(0x134, Memory::YmPacked), 4236)]);
    let right = wmfw::coefficients(TUNING_RIGHT).unwrap();
    let blocks: Vec<(Target, &[u8])> = right.blocks.iter().map(|b| (b.target, b.data)).collect();
    assert_eq!(blocks.len(), 2);
    assert_eq!((blocks[0].0, blocks[0].1.len()), (speaker_protection(0x134, Memory::YmPacked), 3924));
    assert_eq!(blocks[1], (speaker_protection(0x1608, Memory::Ym), &[0x00, 0x80, 0x00, 0x00][..]));
}

#[test]
fn damaged_firmware_files() {
    assert_eq!(wmfw::firmware(&FIRMWARE[..20_000]).err(), Some(FormatError::Truncated));
    assert_eq!(wmfw::firmware(&FIRMWARE[..30]).err(), Some(FormatError::Truncated));
    assert_eq!(wmfw::firmware(TUNING_LEFT).err(), Some(FormatError::Magic));
    let mut other_core = FIRMWARE.to_vec();
    other_core[10] = 2;
    assert_eq!(wmfw::firmware(&other_core).err(), Some(FormatError::Core(2)));
    let mut old_format = FIRMWARE.to_vec();
    old_format[11] = 1;
    assert_eq!(wmfw::firmware(&old_format).err(), Some(FormatError::Version(1)));
    // A block for a memory the HALO core does not have (an older core's
    // data memory).
    let mut dm = FIRMWARE.to_vec();
    dm[0x60 + 3] = 3;
    assert_eq!(wmfw::firmware(&dm).err(), Some(FormatError::Memory(3)));
    assert_eq!(wmfw::coefficients(&TUNING_LEFT[..1000]).err(), Some(FormatError::Truncated));
    assert_eq!(wmfw::coefficients(FIRMWARE).err(), Some(FormatError::Magic));
}

#[test]
fn tuning_parameters() {
    // A signature, version 1, the file's size, one entry: the gain (16).
    let mut file = Vec::new();
    for v in [0x109A_4A35u32, 1, 32, 1, 0, 0, 16, 16] {
        file.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(wmfw::tuning_gain(&file), Ok(Some(16)));
    file[8] = 31;
    assert_eq!(wmfw::tuning_gain(&file), Err(FormatError::Truncated));
}

#[test]
fn firmware_names_as_linux_looks_them_up() {
    let names = group::firmware_files("10431F62", Some(0), "L0");
    let wmfw: Vec<&str> = names.iter().map(|(w, _)| w.as_str()).collect();
    assert_eq!(
        wmfw,
        [
            "cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-l0.wmfw",
            "cirrus/cs35l41-dsp1-spk-prot-10431f62-l0.wmfw",
            "cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0.wmfw",
            "cirrus/cs35l41-dsp1-spk-prot-10431f62.wmfw",
            "cirrus/cs35l41-dsp1-spk-prot.wmfw",
        ]
    );
    // The tunings for the firmware this board has.
    assert_eq!(
        names[3].1,
        ["cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0-l0.bin", "cirrus/cs35l41-dsp1-spk-prot-10431f62-spkid0.bin"]
    );
    let without_id = group::firmware_files("17AA38B4", None, "R1");
    assert_eq!(without_id[0].0, "cirrus/cs35l41-dsp1-spk-prot-17aa38b4-r1.wmfw");
    assert_eq!(
        without_id[1].1,
        ["cirrus/cs35l41-dsp1-spk-prot-17aa38b4-r1.bin", "cirrus/cs35l41-dsp1-spk-prot-17aa38b4.bin"]
    );
}

#[test]
fn dsp_memory_registers() {
    // Word 0x34 of X memory, packed: twelve bytes a four words.
    assert_eq!(dsp::register(Memory::XmPacked, 0x34), 0x0200_009C);
    // Word 14 starts in the middle of a group: its register.
    assert_eq!(dsp::register(Memory::YmPacked, 14), 0x02C0_0028);
    assert_eq!(dsp::register(Memory::Ym, 14), 0x0340_0038);
    assert_eq!(dsp::register(Memory::Pm, 4), 0x0380_0014);
    assert_eq!(dsp::register(Memory::Xm, 0x1F4), 0x0280_07D0);
    assert_eq!(dsp::register(Memory::Pm, u32::MAX), u32::MAX);
}
