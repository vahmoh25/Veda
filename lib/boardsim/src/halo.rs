//! A CS35L41's DSP (a Cirrus Logic HALO core) as its registers behave: its
//! three memories through their windows (24-bit X and Y data words, four
//! to three registers packed or one to a register; 40-bit program words,
//! four to five registers), its core's enable and resets, its memory
//! protection and sample rates; and, standing in for the firmware it runs
//! (which a model cannot run), what Cirrus's speaker protection does that
//! a driver sees: a few milliseconds after its core starts it reports
//! running in its state word, then takes its mailbox commands.
//!
//! What a driver does wrong is noted in [`Halo::problems`]: memory written
//! while the core runs, the core started with its memory protection closed
//! or its sample rates unset, commands the firmware does not know.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

use vcs35l41::regs::*;

/// Microseconds from the core's start to the firmware reporting it runs.
pub const START_US: u64 = 3000;
/// Where Cirrus's speaker protection reports its state (its `HALO_STATE`,
/// an X memory word), and the state once running.
pub const STATE_WORD: u32 = 0x1F4;
const RUNNING: u32 = 2;
/// Its mailbox statuses.
pub const STATUS_RUNNING: u32 = 0;
pub const STATUS_PAUSED: u32 = 1;

const XM_WORDS: usize = 4096;
const YM_WORDS: usize = 2048;
const PM_REGISTERS: usize = ((DSP1_PMEM_LAST - DSP1_PMEM_0) / 4 + 1) as usize;
/// The DSP's block of control registers.
const CONTROLS: std::ops::RangeInclusive<u32> = DSP1_CTRL_BASE..=0x02BC_FFFF;
/// The mailboxes.
const MAILBOXES: std::ops::RangeInclusive<u32> = 0x0001_3000..=0x0001_305C;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Core {
    Off,
    /// Started; the firmware reports running at this time (µs).
    Starting(u64),
    Running,
    /// Started without firmware in its memory: it runs nothing sensible.
    Lost,
}

pub struct Halo {
    name: &'static str,
    pub xm: Vec<u32>,
    pub ym: Vec<u32>,
    pub pm: Vec<u32>,
    controls: BTreeMap<u32, u32>,
    pub core: Core,
    /// The firmware's mailbox status (`DSP_MBOX_2`).
    pub status: u32,
    /// The memory protection: its last two key writes, and the windows
    /// opened while it was unlocked.
    mpu_key: [u32; 2],
    mpu_open: [bool; 8],
    /// The firmware never reports running.
    pub hangs: bool,
    /// The firmware ignores its mailbox.
    pub deaf: bool,
    /// Its memories (X, Y, program) as they were when the core last
    /// started: what the firmware found there.
    pub started_with: Option<(Vec<u32>, Vec<u32>, Vec<u32>)>,
    pub problems: Vec<String>,
}

/// The 96 bits of a group of four 24-bit words, the first lowest.
fn group(words: &[u32], first: usize) -> u128 {
    (0..4).fold(0, |v, i| v | (words.get(first + i).copied().unwrap_or(0) as u128 & 0xFF_FFFF) << (24 * i))
}

/// Register `index` of a packed window: the 32 bits of its group of words
/// it holds.
fn packed_read(words: &[u32], index: usize) -> u32 {
    (group(words, index / 3 * 4) >> (32 * (index % 3))) as u32
}

fn packed_write(words: &mut [u32], index: usize, value: u32) {
    let first = index / 3 * 4;
    if first + 4 > words.len() {
        return;
    }
    let shift = 32 * (index % 3);
    let v = group(words, first) & !(0xFFFF_FFFFu128 << shift) | (value as u128) << shift;
    for (i, w) in words[first..first + 4].iter_mut().enumerate() {
        *w = (v >> (24 * i)) as u32 & 0xFF_FFFF;
    }
}

/// Which memory a register is in, and its index in the window.
enum Place {
    Packed(bool, usize),
    Unpacked(bool, usize),
    Program(usize),
}

fn place(r: u32) -> Option<Place> {
    let index = |base: u32| ((r - base) / 4) as usize;
    Some(match r {
        DSP1_XMEM_PACK_0..=DSP1_XMEM_PACK_LAST => Place::Packed(true, index(DSP1_XMEM_PACK_0)),
        DSP1_YMEM_PACK_0..=DSP1_YMEM_PACK_LAST => Place::Packed(false, index(DSP1_YMEM_PACK_0)),
        DSP1_XMEM_UNPACK24_0..=DSP1_XMEM_UNPACK24_LAST => Place::Unpacked(true, index(DSP1_XMEM_UNPACK24_0)),
        DSP1_YMEM_UNPACK24_0..=DSP1_YMEM_UNPACK24_LAST => Place::Unpacked(false, index(DSP1_YMEM_UNPACK24_0)),
        DSP1_PMEM_0..=DSP1_PMEM_LAST => Place::Program(index(DSP1_PMEM_0)),
        _ => return None,
    })
}

impl Halo {
    pub fn new(name: &'static str) -> Halo {
        let mut h = Halo {
            name,
            xm: Vec::new(),
            ym: Vec::new(),
            pm: Vec::new(),
            controls: BTreeMap::new(),
            core: Core::Off,
            status: 0,
            mpu_key: [0; 2],
            mpu_open: [false; 8],
            hangs: false,
            deaf: false,
            started_with: None,
            problems: Vec::new(),
        };
        h.reset();
        h
    }

    /// As a reset of the amplifier leaves it: memories cleared, the core
    /// off.
    pub fn reset(&mut self) {
        self.xm = vec![0; XM_WORDS];
        self.ym = vec![0; YM_WORDS];
        self.pm = vec![0; PM_REGISTERS];
        self.controls = BTreeMap::from([(DSP1_CCM_CORE_CTRL, 0x0101)]);
        self.core = Core::Off;
        self.status = 0;
        self.mpu_key = [0; 2];
        self.mpu_open = [false; 8];
    }

    /// Whether a register is the DSP's.
    pub fn has(r: u32) -> bool {
        place(r).is_some() || CONTROLS.contains(&r) || MAILBOXES.contains(&r)
    }

    fn note(&mut self, problem: String) {
        let problem = format!("{}: {}", self.name, problem);
        if !self.problems.contains(&problem) {
            self.problems.push(problem);
        }
    }

    pub fn running(&self) -> bool {
        self.core == Core::Running
    }

    /// The firmware reports running once its time has come.
    fn tick(&mut self, now: u64) {
        if let Core::Starting(at) = self.core
            && now >= at
        {
            self.core = Core::Running;
            self.xm[STATE_WORD as usize] = RUNNING;
            self.status = STATUS_RUNNING;
        }
    }

    /// What a register holds, without side effects.
    pub fn peek(&self, r: u32) -> u32 {
        match r {
            DSP_MBOX_2 => self.status,
            r => Self::peek_memory((&self.xm, &self.ym, &self.pm), r)
                .unwrap_or_else(|| self.controls.get(&r).copied().unwrap_or(0)),
        }
    }

    /// What a register of its memories held when the core last started.
    pub fn peek_started(&self, r: u32) -> Option<u32> {
        let (xm, ym, pm) = self.started_with.as_ref()?;
        Self::peek_memory((xm, ym, pm), r)
    }

    fn peek_memory((xm, ym, pm): (&[u32], &[u32], &[u32]), r: u32) -> Option<u32> {
        Some(match place(r)? {
            Place::Packed(x, i) => packed_read(if x { xm } else { ym }, i),
            Place::Unpacked(x, i) => (if x { xm } else { ym }).get(i).copied().unwrap_or(0),
            Place::Program(i) => pm[i],
        })
    }

    pub fn read(&mut self, r: u32, now: u64) -> u32 {
        self.tick(now);
        self.peek(r)
    }

    pub fn write(&mut self, r: u32, v: u32, now: u64) {
        self.tick(now);
        if let Some(p) = place(r) {
            if self.core != Core::Off {
                self.note(format!("DSP memory written while its core runs ({:#x})", r));
            }
            match p {
                Place::Packed(x, i) => packed_write(if x { &mut self.xm } else { &mut self.ym }, i, v),
                Place::Unpacked(x, i) => {
                    if let Some(w) = (if x { &mut self.xm } else { &mut self.ym }).get_mut(i) {
                        *w = v & 0xFF_FFFF;
                    }
                }
                Place::Program(i) => self.pm[i] = v,
            }
            return;
        }
        match r {
            DSP_VIRT1_MBOX_1 => self.command(v),
            DSP1_MPU_LOCK_CONFIG => self.mpu_key = [self.mpu_key[1], v],
            r if (DSP1_MPU_XMEM_ACCESS_0..DSP1_MPU_XMEM_ACCESS_0 + 0x60).contains(&r) => {
                // The X and Y memory access of each of the four windows.
                let (window, offset) = ((r - DSP1_MPU_XMEM_ACCESS_0) / 0x18, (r - DSP1_MPU_XMEM_ACCESS_0) % 0x18);
                if offset <= 4 {
                    self.mpu_open[(window * 2 + offset / 4) as usize] =
                        self.mpu_key == [0x5555, 0xAAAA] && v == u32::MAX;
                }
            }
            DSP1_CCM_CORE_CTRL => {
                let enabled = v & HALO_CORE_EN != 0 && v & HALO_CORE_RESET == 0;
                if enabled && self.core == Core::Off {
                    self.start(now);
                } else if !enabled {
                    self.core = Core::Off;
                }
            }
            DSP1_CORE_SOFT_RESET if v & HALO_CORE_SOFT_RESET != 0 => self.core = Core::Off,
            _ => {}
        }
        if CONTROLS.contains(&r) {
            self.controls.insert(r, v);
        }
    }

    /// The core comes out of reset: the firmware in its memory starts,
    /// if there is one (its header, at the start of X memory, ends its
    /// list of algorithms with 0xBEDEAD).
    fn start(&mut self, now: u64) {
        self.started_with = Some((self.xm.clone(), self.ym.clone(), self.pm.clone()));
        if !self.mpu_open.iter().all(|&o| o) {
            self.note("its DSP started with its memory protection closed".into());
        }
        let rates = (0..8).all(|i| self.peek(DSP1_RX1_RATE + 8 * i) == 1 && self.peek(DSP1_TX1_RATE + 8 * i) == 1);
        if !rates {
            self.note("its DSP started without its sample rates set".into());
        }
        let count = self.xm[9] as usize;
        let firmware =
            self.xm[3] != 0 && (1..=1024).contains(&count) && self.xm.get(10 + 6 * count) == Some(&0xBE_DEAD);
        self.core = match (firmware, self.hangs) {
            (false, _) => Core::Lost,
            (true, true) => Core::Starting(u64::MAX),
            (true, false) => Core::Starting(now + START_US),
        };
    }

    /// Whether the firmware takes the command to switch the speaker output
    /// on: only while it runs, not paused.
    pub fn takes_speaker_on(&self) -> bool {
        self.running() && !self.deaf && self.status == STATUS_RUNNING
    }

    /// A command in the firmware's mailbox.
    fn command(&mut self, v: u32) {
        if self.core != Core::Running || self.deaf {
            return;
        }
        match v {
            1 => self.status = STATUS_PAUSED,
            2 => self.status = STATUS_RUNNING,
            // Switching the speaker on (the amplifier's part) leaves the
            // status as it is.
            7 => {}
            v => self.note(format!("its firmware was sent command {}, which this test does not expect", v)),
        }
    }
}
