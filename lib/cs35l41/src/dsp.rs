//! The amplifier's DSP (a Cirrus Logic HALO core) and the firmware Linux
//! runs on it, driven as Linux's `cs_dsp` and `cs35l41_hda` drive them:
//! loading the firmware and its tuning (`cs_dsp_power_up`), starting it
//! (`cs_dsp_run`, then waiting for the firmware to report it runs),
//! stopping it, and the firmware's mailbox (pause, resume, speaker output
//! on).
//!
//! The firmware is Cirrus's speaker protection ("CSPL"): between the
//! codec's audio and the amplifier it keeps the speakers within their
//! limits (their temperature, from the voltage and current it measures,
//! and how far their cones move), which lets the amplifier run at its full
//! gain, with the board maker's tuning for its speakers.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::Bus;
use crate::amp::{Error, update};
use crate::regs::*;
use crate::wmfw::{self, Coefficients, Firmware, Memory, Place, Target};

/// The firmware's state word, of its own algorithm in X memory, and the
/// value it holds once running (`HALO_STATE`).
const STATE_CONTROL: &str = "HALO_STATE";
const STATE_ALGORITHM: u32 = 0x0004_00A4;
const STATE_RUNNING: u32 = 2;
/// How long the firmware may take to report it runs (read every
/// millisecond).
const START_TIMEOUT_US: u64 = 15_000;
/// The memory protection's settings for every region (`lock_regions`).
const LOCK_REGIONS: u32 = 0xFFFF_FFFF;
/// Bytes written in one transfer: whole groups of packed words (three
/// registers of data words, five of program words).
const CHUNK: usize = 960;

/// A command to the speaker protection firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Pause = 1,
    Resume = 2,
    /// The speaker output on, after power-up with an external boost
    /// supply (firmware after v0.28.0).
    SpeakerOutEnable = 7,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::SpeakerOutEnable => "switch the speaker on",
        }
    }
}

/// The firmware's status in its mailbox.
const STATUS_RUNNING: u32 = 0;
const STATUS_PAUSED: u32 = 1;
/// An error (-1, sign-extended or not).
const STATUS_ERRORS: [u32; 2] = [u32::MAX, 0x00FF_FFFF];

/// Why the DSP could not be loaded, started or told something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspError {
    /// A block of the firmware falls outside the DSP's memories (its
    /// first register).
    Range(u32),
    /// The loaded firmware lists no algorithms, or too many.
    Algorithms(u32),
    /// The firmware has no state word to watch (`HALO_STATE`).
    NoState,
    /// It did not report running in time (the state it last reported).
    Start(u32),
    /// Once started, its status was neither running nor paused.
    Status(u32),
    /// It did not take a command (its last status).
    Mailbox(Command, u32),
}

impl fmt::Display for DspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DspError::Range(r) => write!(f, "a firmware block at register {:#x} falls outside the DSP's memory", r),
            DspError::Algorithms(n) => write!(f, "the loaded firmware lists {} algorithms", n),
            DspError::NoState => f.write_str("the firmware has no state to watch (HALO_STATE)"),
            DspError::Start(s) => write!(f, "the firmware did not start (state {})", s),
            DspError::Status(s) => write!(f, "the firmware's status was {:#x} once started", s),
            DspError::Mailbox(c, s) => write!(f, "the firmware did not {} (status {:#x})", c.name(), s),
        }
    }
}

fn fail<T>(e: DspError) -> Result<T, Error> {
    Err(Error::Dsp(e))
}

/// A memory's window of registers: its first and last.
fn window(memory: Memory) -> (u32, u32) {
    match memory {
        Memory::Pm => (DSP1_PMEM_0, DSP1_PMEM_LAST),
        Memory::XmPacked => (DSP1_XMEM_PACK_0, DSP1_XMEM_PACK_LAST),
        Memory::YmPacked => (DSP1_YMEM_PACK_0, DSP1_YMEM_PACK_LAST),
        Memory::Xm => (DSP1_XMEM_UNPACK24_0, DSP1_XMEM_UNPACK24_LAST),
        Memory::Ym => (DSP1_YMEM_UNPACK24_0, DSP1_YMEM_UNPACK24_LAST),
    }
}

/// The register holding (the start of) word `word` of a memory
/// (`cs_dsp_halo_region_to_reg`); past the end of the register space for
/// words no memory has.
pub fn register(memory: Memory, word: u32) -> u32 {
    let base = window(memory).0;
    match memory {
        Memory::Pm => base.saturating_add(word.saturating_mul(5)),
        Memory::XmPacked | Memory::YmPacked => base.saturating_add(word.saturating_mul(3)) & !3,
        Memory::Xm | Memory::Ym => base.saturating_add(word.saturating_mul(4)),
    }
}

/// Whether `len` bytes from `register` stay inside a memory's window.
fn inside(memory: Memory, register: u32, len: usize) -> bool {
    let (first, last) = window(memory);
    register >= first && (register as u64 + len as u64) <= last as u64 + 4
}

/// Writes whole registers, a few hundred bytes at a time.
fn write(bus: &mut impl Bus, register: u32, data: &[u8]) -> Result<(), Error> {
    for (i, chunk) in data.chunks(CHUNK).enumerate() {
        bus.write_block(register + (i * CHUNK) as u32, chunk)?;
    }
    Ok(())
}

/// One of the loaded firmware's algorithms (the firmware itself first),
/// and where its part of each data memory starts, in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Algorithm {
    pub id: u32,
    /// `0x00MMmmpp`.
    pub version: u32,
    pub xm: u32,
    pub ym: u32,
}

/// Firmware loaded into the DSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The firmware's id, vendor and version (`0x00MMmmpp`), as it
    /// describes itself in X memory.
    pub id: u32,
    pub vendor: u32,
    pub version: u32,
    pub algorithms: Vec<Algorithm>,
    /// The register of its state word.
    state: Option<u32>,
    /// What was not as expected, and skipped (as Linux does).
    pub warnings: Vec<String>,
}

impl Loaded {
    /// Where an algorithm's part of a data memory starts, in words.
    fn base(&self, memory: Memory, id: u32) -> Option<u32> {
        let a = self.algorithms.iter().find(|a| a.id == id)?;
        match memory {
            Memory::Xm | Memory::XmPacked => Some(a.xm),
            Memory::Ym | Memory::YmPacked => Some(a.ym),
            Memory::Pm => None,
        }
    }
}

/// Loads firmware and its coefficients into a DSP that is not running
/// (`cs_dsp_power_up`): the firmware's blocks; then the algorithms the
/// firmware lists at the start of its X memory; then the coefficients,
/// into the algorithms they are for.
pub fn load(bus: &mut impl Bus, firmware: &Firmware, coefficients: Option<&Coefficients>) -> Result<Loaded, Error> {
    for b in &firmware.blocks {
        match b.place {
            // Linux writes nothing for register 0.
            Place::Register(0) => {}
            Place::Register(r) => write(bus, r, b.data)?,
            Place::Memory(m, word) => {
                let r = register(m, word);
                if !inside(m, r, b.data.len()) {
                    return fail(DspError::Range(r));
                }
                write(bus, r, b.data)?;
            }
        }
    }
    // The firmware's header: its core, block revision, vendor, id and
    // version, where its own part of each data memory is (and how big),
    // and how many algorithms follow, each with its id, version and parts
    // (six words); then 0xBEDEAD.
    let mut word = |w: u32| -> Result<u32, Error> { Ok(bus.read(register(Memory::Xm, w))? & 0xFF_FFFF) };
    let mut header = [0u32; 10];
    for (i, h) in header.iter_mut().enumerate() {
        *h = word(i as u32)?;
    }
    let count = header[9];
    if count == 0 || count > 1024 || !inside(Memory::Xm, register(Memory::Xm, 10 + 6 * count), 4) {
        return fail(DspError::Algorithms(count));
    }
    let mut warnings = Vec::new();
    let end = word(10 + 6 * count)?;
    if end != 0xBE_DEAD {
        warnings.push(format!("its algorithm list ends with {:#x}, not 0xbedead", end));
    }
    let mut algorithms = vec![Algorithm { id: header[3], version: header[4], xm: header[5], ym: header[7] }];
    for i in 0..count {
        let at = 10 + 6 * i;
        let (id, version, xm, ym) = (word(at)?, word(at + 1)?, word(at + 2)?, word(at + 4)?);
        algorithms.push(Algorithm { id, version, xm, ym });
    }
    let state = firmware.control(STATE_CONTROL, wmfw::XM, STATE_ALGORITHM).and_then(|c| {
        let base = algorithms.iter().find(|a| a.id == c.algorithm)?.xm;
        let r = register(Memory::Xm, base.checked_add(c.offset)?);
        inside(Memory::Xm, r, 4).then_some(r)
    });
    let mut loaded = Loaded { id: header[3], vendor: header[2], version: header[4], algorithms, state, warnings };
    if let Some(c) = coefficients {
        load_coefficients(bus, &mut loaded, c)?;
    }
    Ok(loaded)
}

/// Writes coefficients into the loaded firmware's algorithms
/// (`cs_dsp_load_coeff`). Blocks for algorithms or memories it does not
/// have are skipped with a warning, as Linux skips them.
fn load_coefficients(bus: &mut impl Bus, loaded: &mut Loaded, file: &Coefficients) -> Result<(), Error> {
    for b in &file.blocks {
        let register = match b.target {
            Target::Absolute { algorithm, register: 0 } if algorithm == loaded.id => {
                loaded.warnings.push("its tuning has global coefficients, which the DSP does not have".into());
                continue;
            }
            Target::Absolute { register, .. } => register,
            Target::Algorithm { memory, algorithm, version, offset } => {
                let Some(base) = loaded.base(memory, algorithm) else {
                    loaded.warnings.push(format!("its tuning is for an algorithm it does not have ({:#x})", algorithm));
                    continue;
                };
                let expected = loaded.algorithms.iter().find(|a| a.id == algorithm).map_or(0, |a| a.version);
                if version != expected {
                    loaded.warnings.push(format!(
                        "its tuning is for v{} of algorithm {:#x}, which is v{}",
                        version_name(version),
                        algorithm,
                        version_name(expected)
                    ));
                }
                let r = register(memory, base).saturating_add(offset);
                if !inside(memory, r, b.data.len()) {
                    return fail(DspError::Range(r));
                }
                r
            }
            Target::Other(kind) => {
                loaded
                    .warnings
                    .push(format!("its tuning has a block for memory {:#x}, which the DSP does not have", kind));
                continue;
            }
        };
        if register == 0 {
            continue;
        }
        if b.data.len() % 4 != 0 {
            loaded.warnings.push(format!("its tuning has a block of {} bytes, not whole registers", b.data.len()));
            continue;
        }
        write(bus, register, b.data)?;
    }
    Ok(())
}

/// `M.m.p` of a version word (`0x00MMmmpp`).
pub fn version_name(v: u32) -> String {
    format!("{}.{}.{}", v >> 16 & 0xFF, v >> 8 & 0xFF, v & 0xFF)
}

/// Starts loaded firmware (`cs35l41_smart_amp`): the DSP's sample rates
/// set, its memory protection opened to the firmware, the core out of
/// reset and running (`cs_dsp_run`); then waits for the firmware to report
/// it runs, checks its status, and pauses it until playback starts.
pub fn start(bus: &mut impl Bus, loaded: &Loaded) -> Result<(), Error> {
    let Some(state) = loaded.state else { return fail(DspError::NoState) };
    // All of its inputs and outputs at the base rate (Linux's "fs
    // errata").
    for i in 0..8 {
        bus.write(DSP1_RX1_RATE + 8 * i, 1)?;
    }
    for i in 0..8 {
        bus.write(DSP1_TX1_RATE + 8 * i, 1)?;
    }
    // Memory protection: unlocked, every window open, locked again
    // (`cs_dsp_halo_configure_mpu`).
    bus.write(DSP1_MPU_LOCK_CONFIG, 0x5555)?;
    bus.write(DSP1_MPU_LOCK_CONFIG, 0xAAAA)?;
    for w in 0..4 {
        let base = DSP1_MPU_XMEM_ACCESS_0 + 0x18 * w;
        for (offset, value) in [(0x00, 0xFFFF_FFFF), (0x04, 0xFFFF_FFFF), (0x08, LOCK_REGIONS), (0x0C, LOCK_REGIONS)] {
            bus.write(base + offset, value)?;
        }
        bus.write(base + 0x14, LOCK_REGIONS)?;
    }
    bus.write(DSP1_MPU_LOCK_CONFIG, 0)?;
    // The core enabled in reset, then out of reset.
    update(bus, DSP1_CCM_CORE_CTRL, HALO_CORE_RESET | HALO_CORE_EN, HALO_CORE_RESET | HALO_CORE_EN)?;
    update(bus, DSP1_CCM_CORE_CTRL, HALO_CORE_RESET, 0)?;
    let mut waited = 0;
    loop {
        let s = bus.read(state)?;
        if s == STATE_RUNNING {
            break;
        }
        if waited >= START_TIMEOUT_US {
            return fail(DspError::Start(s));
        }
        bus.sleep_us(1000);
        waited += 1000;
    }
    let status = bus.read(DSP_MBOX_2)?;
    if status != STATUS_RUNNING && status != STATUS_PAUSED {
        return fail(DspError::Status(status));
    }
    mailbox(bus, Command::Pause)
}

/// Stops the DSP (`cs_dsp_stop`): its watchdog off, the core disabled and
/// reset. The firmware stays in its memory, unused.
pub fn stop(bus: &mut impl Bus) -> Result<(), Error> {
    update(bus, DSP1_WDT_CONTROL, HALO_WDT_EN, 0)?;
    update(bus, DSP1_CCM_CORE_CTRL, HALO_CORE_EN, 0)?;
    update(bus, DSP1_CORE_SOFT_RESET, HALO_CORE_SOFT_RESET, HALO_CORE_SOFT_RESET)
}

/// Sends the firmware a command, then waits for its status to agree
/// (`cs35l41_set_cspl_mbox_cmd`): five looks, a millisecond apart.
pub fn mailbox(bus: &mut impl Bus, command: Command) -> Result<(), Error> {
    bus.write(DSP_VIRT1_MBOX_1, command as u32)?;
    let expected = match command {
        Command::Pause => STATUS_PAUSED,
        Command::Resume | Command::SpeakerOutEnable => STATUS_RUNNING,
    };
    let mut status = 0;
    for _ in 0..5 {
        bus.sleep_us(1000);
        status = bus.read(DSP_MBOX_2)?;
        if STATUS_ERRORS.contains(&status) {
            break;
        }
        if status == expected {
            return Ok(());
        }
    }
    fail(DspError::Mailbox(command, status))
}
