//! The HD Audio controller (Intel HD Audio 1.0a, chapter 3): registers,
//! reset and the codecs on the link, the command rings, the stream
//! descriptors and their DMA buffers, and interrupts.
//!
//! **Coherency.** Many HD Audio controllers can move audio without
//! snooping the CPU's caches. The driver asks for snooped transfers where
//! the chipset allows it, and also writes every line it hands the
//! controller back to memory (and drops every line before reading what the
//! controller wrote), so that the audio is right either way.
//!
//! **Chipsets.** What particular controllers need beyond the specification
//! ([`Quirks`]) follows Linux's `snd-hda-intel`, which has met them all.

use alloc::collections::VecDeque;
use core::arch::x86_64::{_mm_clflush, _mm_mfence};
use core::ptr::{read_volatile, write_volatile};

use vabi::map_flags;
use vproto::pci::{self, DeviceInfo, pcidev};
use vrt::object::{Interrupt, Resource};
use vrt::println;
use vrt::time::{Duration, now_ns};
use vrt::vm::Mapping;
use vvirtio::DmaBuffer;

/// Global registers (section 3.3).
mod reg {
    pub const GCAP: usize = 0x00;
    pub const VMIN: usize = 0x02;
    pub const VMAJ: usize = 0x03;
    pub const GCTL: usize = 0x08;
    pub const STATESTS: usize = 0x0E;
    /// Where the extended capabilities start (Intel's, since Skylake).
    pub const LLCH: usize = 0x14;
    pub const INTCTL: usize = 0x20;
    pub const INTSTS: usize = 0x24;
    /// Counts the link's bit clock, 24 MHz.
    pub const WALLCLK: usize = 0x30;
    /// The DMA position buffer: where the controller writes each stream's
    /// position (bit 0 of the low half turns it on).
    pub const DPLBASE: usize = 0x70;
    pub const DPUBASE: usize = 0x74;
    pub const CORBLBASE: usize = 0x40;
    pub const CORBUBASE: usize = 0x44;
    pub const CORBWP: usize = 0x48;
    pub const CORBRP: usize = 0x4A;
    pub const CORBCTL: usize = 0x4C;
    pub const CORBSIZE: usize = 0x4E;
    pub const RIRBLBASE: usize = 0x50;
    pub const RIRBUBASE: usize = 0x54;
    pub const RIRBWP: usize = 0x58;
    pub const RINTCNT: usize = 0x5A;
    pub const RIRBCTL: usize = 0x5C;
    pub const RIRBSTS: usize = 0x5D;
    pub const RIRBSIZE: usize = 0x5E;
    /// The immediate command interface: command, response, status.
    pub const IC: usize = 0x60;
    pub const IR: usize = 0x64;
    pub const IRS: usize = 0x68;
    /// The stream descriptors: input streams first, then output streams,
    /// then bidirectional ones.
    pub const SD_BASE: usize = 0x80;
    pub const SD_STRIDE: usize = 0x20;
    /// Within a stream descriptor.
    pub const SD_CTL: usize = 0x00;
    pub const SD_STS: usize = 0x03;
    pub const SD_LPIB: usize = 0x04;
    pub const SD_CBL: usize = 0x08;
    pub const SD_LVI: usize = 0x0C;
    pub const SD_FIFOS: usize = 0x10;
    pub const SD_FMT: usize = 0x12;
    pub const SD_BDPL: usize = 0x18;
    pub const SD_BDPU: usize = 0x1C;
}

const GCTL_CRST: u32 = 1 << 0;
const GCTL_UNSOL: u32 = 1 << 8;
const INTCTL_GIE: u32 = 1 << 31;
const INTCTL_CIE: u32 = 1 << 30;
const CORBRP_RST: u16 = 1 << 15;
const CORBCTL_RUN: u8 = 1 << 1;
const RIRBWP_RST: u16 = 1 << 15;
const RIRBCTL_INTERRUPT: u8 = 1 << 0;
const RIRBCTL_DMA: u8 = 1 << 1;
/// RIRBSTS: a response came, the ring overran (cleared by writing 1).
const RIRBSTS_CLEAR: u8 = 0x05;
const RIRBSTS_OVERRUN: u8 = 0x04;
/// IRS: a command is on its way; its response is in IR (cleared by
/// writing 1).
const IRS_BUSY: u16 = 1 << 0;
const IRS_VALID: u16 = 1 << 1;
const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;
/// Bidirectional streams: the stream goes out.
const SD_CTL_DIR_OUT: u32 = 1 << 19;
/// SDnSTS: buffer completion; a FIFO error (an underrun going out, an
/// overrun coming in); a descriptor error (the controller could not read
/// its buffer descriptors, and stopped).
const SD_STS_BCIS: u8 = 1 << 2;
pub const SD_STS_FIFOE: u8 = 1 << 3;
pub const SD_STS_DESE: u8 = 1 << 4;
const SD_STS_CLEAR: u8 = SD_STS_BCIS | SD_STS_FIFOE | SD_STS_DESE;
/// A response's extended word: the codec address, and the unsolicited bit.
const RESPONSE_UNSOLICITED: u32 = 1 << 4;
/// Buffer descriptor list entries: interrupt on completion.
const BDL_IOC: u32 = 1;
/// How long a codec may take to answer.
const RESPONSE_TIMEOUT_NS: u64 = 300_000_000;
const CACHE_LINE: usize = 64;
/// The DMA buffer of a stream starts after its descriptor list.
const BDL_BYTES: usize = 4096;

/// Writes the cache lines of `len` bytes at `p` back to memory (and drops
/// them), so a controller that does not snoop sees what the CPU wrote, and
/// the CPU then reads what the controller wrote.
fn flush(p: *const u8, len: usize) {
    let start = p as usize & !(CACHE_LINE - 1);
    let end = p as usize + len;
    // SAFETY: CLFLUSH of lines this process has mapped; SSE2 is part of
    // the target's baseline.
    unsafe {
        _mm_mfence();
        for line in (start..end).step_by(CACHE_LINE) {
            _mm_clflush(line as *const u8);
        }
        _mm_mfence();
    }
}

fn flush_all(buffer: &DmaBuffer) {
    flush(buffer.ptr(), buffer.len());
}

/// The memory-mapped registers.
struct Registers {
    _map: Mapping,
    base: *mut u8,
    len: usize,
}

impl Registers {
    fn fits(&self, off: usize, width: usize) -> bool {
        off.is_multiple_of(width) && off + width <= self.len
    }

    fn r8(&self, off: usize) -> u8 {
        // SAFETY: an aligned register inside the mapped BAR.
        if self.fits(off, 1) { unsafe { read_volatile(self.base.add(off)) } } else { u8::MAX }
    }

    fn r16(&self, off: usize) -> u16 {
        // SAFETY: as above.
        if self.fits(off, 2) { unsafe { read_volatile(self.base.add(off) as *const u16) } } else { u16::MAX }
    }

    fn r32(&self, off: usize) -> u32 {
        // SAFETY: as above.
        if self.fits(off, 4) { unsafe { read_volatile(self.base.add(off) as *const u32) } } else { u32::MAX }
    }

    fn w8(&self, off: usize, v: u8) {
        if self.fits(off, 1) {
            // SAFETY: as above.
            unsafe { write_volatile(self.base.add(off), v) }
        }
    }

    fn w16(&self, off: usize, v: u16) {
        if self.fits(off, 2) {
            // SAFETY: as above.
            unsafe { write_volatile(self.base.add(off) as *mut u16, v) }
        }
    }

    fn w32(&self, off: usize, v: u32) {
        if self.fits(off, 4) {
            // SAFETY: as above.
            unsafe { write_volatile(self.base.add(off) as *mut u32, v) }
        }
    }

    /// Waits up to `ms` milliseconds for `done`.
    fn wait(&self, ms: u64, mut done: impl FnMut(&Registers) -> bool) -> bool {
        let end = now_ns() + ms * 1_000_000;
        loop {
            if done(self) {
                return true;
            }
            if now_ns() > end {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

const INTEL: u16 = 0x8086;
const ATI: u16 = 0x1002;
const AMD: u16 = 0x1022;
const NVIDIA: u16 = 0x10DE;

/// Intel's controllers since Skylake (the 100-series chipsets of 2015):
/// chipsets and systems-on-chip, then Arc graphics (HDMI only).
const INTEL_SKYLAKE_AND_LATER: &[u16] = &[
    0xA170, 0x9D70, 0xA171, 0x9D71, 0xA2F0, // Skylake, Kaby Lake
    0xA348, 0x9DC8, // Coffee Lake, Cannon Lake
    0x02C8, 0x06C8, 0xA3F0, 0xF0C8, 0xF1C8, // Comet Lake, Rocket Lake
    0x34C8, 0x3DC8, 0x38C8, 0x4DC8, // Ice Lake, Jasper Lake
    0xA0C8, 0x43C8, 0x4B55, 0x4B58, // Tiger Lake, Elkhart Lake
    0x7AD0, 0x51C8, 0x51C9, 0x51CC, 0x51CD, 0x54C8, // Alder Lake
    0x7A50, 0x51CA, 0x51CB, 0x51CE, 0x51CF, // Raptor Lake
    0x7E28, 0x7728, 0x7F50, // Meteor Lake, Arrow Lake
    0x5A98, 0x3198, // Apollo Lake, Gemini Lake
    0xA828, 0xE428, 0xE328, 0x4D28, 0xD328, 0x6E50, // Lunar, Panther, Wildcat, Nova Lake
    0x490D, 0x4F90, 0x4F91, 0x4F92, 0xE2F7, // Arc graphics
];
/// Intel's controllers since Lunar Lake, whose codec commands go through
/// the immediate command registers.
const INTEL_IMMEDIATE_COMMANDS: &[u16] = &[0xA828, 0xE428, 0xE328, 0x4D28, 0xD328, 0x6E50];
/// ATI's and AMD's south bridges with a snoop switch (SB450 to SB900,
/// Hudson, the Zen chipsets and processors).
const ATI_SNOOP: &[(u16, u16)] =
    &[(ATI, 0x437B), (ATI, 0x4383), (AMD, 0x780D), (AMD, 0x1457), (AMD, 0x1487), (AMD, 0x157A), (AMD, 0x15E3)];

/// What a controller needs beyond the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Quirks {
    /// Intel since Skylake: dynamic clock gating off while the link
    /// resets, or the codecs may go unnoticed.
    clock_gating: bool,
    /// Intel since Lunar Lake: commands through the immediate command
    /// registers rather than the command ring.
    immediate_commands: bool,
    /// ATI and AMD south bridges: a snoop switch, which must be on.
    ati_snoop: bool,
    /// NVIDIA chipsets: their MSI is unreliable; poll instead.
    no_msi: bool,
    /// Intel since Skylake: stream positions from the DMA position buffer,
    /// as Linux takes them there (its `POS_FIX_SKL`), and the processing
    /// pipe (an audio DSP between the streams and the link) kept out.
    skylake: bool,
}

impl Quirks {
    fn of(info: &DeviceInfo) -> Quirks {
        let intel = |list: &[u16]| info.vendor == INTEL && list.contains(&info.device);
        Quirks {
            clock_gating: intel(INTEL_SKYLAKE_AND_LATER),
            immediate_commands: intel(INTEL_IMMEDIATE_COMMANDS),
            ati_snoop: ATI_SNOOP.contains(&(info.vendor, info.device)),
            no_msi: info.vendor == NVIDIA,
            skylake: intel(INTEL_SKYLAKE_AND_LATER),
        }
    }
}

/// The processing pipe capability of Intel's controllers since Skylake:
/// PPCTL's bit 30 sends the streams through the audio DSP ("decoupled"),
/// and bits 0-29 single streams.
mod pp {
    pub const CAP_ID: u32 = 0x3;
    pub const PPCTL: usize = 0x04;
    pub const GPROCEN: u32 = 1 << 30;
    pub const STREAMS: u32 = (1 << 30) - 1;
}

/// Intel since Skylake: keeps every stream coupled to the link, as a
/// controller without its DSP's firmware must run (the firmware could
/// have left them decoupled). Returns PPCTL as it was found.
fn couple_streams(regs: &Registers) -> Option<u32> {
    let ppctl = find_capability(regs, pp::CAP_ID)? + pp::PPCTL;
    let found = regs.r32(ppctl);
    if found & (pp::GPROCEN | pp::STREAMS) != 0 {
        regs.w32(ppctl, found & !(pp::GPROCEN | pp::STREAMS));
    }
    Some(found)
}

/// Intel since Skylake: each stream's DMA position as a register, besides
/// the position buffer (for the log).
const INTEL_DPIB: usize = 0x1084;

/// Intel's clock gating control (PCI configuration space): bit 6 lets the
/// controller stop its clocks.
const INTEL_CGCTL: u16 = 0x48;
const INTEL_CGCTL_MISCBDCGE: u32 = 1 << 6;

/// The multi-link capability of Intel's controllers since Skylake: the
/// first link (to the codecs) has its capabilities and its control.
mod ml {
    pub const CAP_ID: u32 = 0x2;
    pub const LINK0: usize = 0x40;
    /// The link's clocks: bit `n` for clock `n` of [`MHZ`].
    pub const LCAP: usize = 0x00;
    /// The clock in use (an index into [`MHZ`]), and the power wanted and
    /// reached.
    pub const LCTL: usize = 0x04;
    pub const SCF: u32 = 0xF;
    pub const SPA: u32 = 1 << 16;
    pub const CPA: u32 = 1 << 23;
    pub const MHZ: [u32; 6] = [6, 12, 24, 48, 96, 192];
    /// The clocks to move a 6 MHz link to, best first (Linux's choice).
    pub const PREFERRED: [u32; 5] = [2, 3, 1, 4, 5];
}

/// Finds an extended capability by its ID: the offset of its header.
fn find_capability(regs: &Registers, id: u32) -> Option<usize> {
    let mut offset = regs.r16(reg::LLCH) as usize;
    for _ in 0..10 {
        if offset == 0 || !regs.fits(offset, 4) {
            return None;
        }
        let header = regs.r32(offset);
        if header == u32::MAX {
            return None;
        }
        if (header >> 16) & 0xFFF == id {
            return Some(offset);
        }
        offset = (header & 0xFFFF) as usize;
    }
    None
}

/// Busy-waits `us` microseconds.
fn delay_us(us: u64) {
    let end = now_ns() + us * 1000;
    while now_ns() < end {
        core::hint::spin_loop();
    }
}

/// Intel since Skylake: a link the firmware left at a 6 MHz clock is moved
/// to the best other clock it has, powered down meanwhile (as Linux's
/// `intel_init_lctl` does). Returns the link's clock in MHz before and
/// after, if the controller has links to configure.
fn init_link_clock(regs: &Registers) -> Option<(u32, u32)> {
    let link = find_capability(regs, ml::CAP_ID)? + ml::LINK0;
    let (lcap, lctl) = (link + ml::LCAP, link + ml::LCTL);
    let clock = |v: u32| ml::MHZ.get((v & ml::SCF) as usize).copied().unwrap_or(0);
    let mut v = regs.r32(lctl);
    let before = clock(v);
    // Power must have reached what was asked for before it is changed.
    if v & ml::SCF == 0 && (v & ml::SPA != 0) == (v & ml::CPA != 0) {
        let power = |on: bool| {
            let v = regs.r32(lctl) & !ml::SPA | if on { ml::SPA } else { 0 };
            regs.w32(lctl, v);
            let reached = regs.wait(1, |r| (r.r32(lctl) & ml::CPA != 0) == on);
            delay_us(100);
            reached
        };
        if power(false) {
            let caps = regs.r32(lcap);
            if let Some(scf) = ml::PREFERRED.into_iter().find(|&c| caps & 1 << c != 0) {
                v = (v & !ml::SCF) | scf;
                regs.w32(lctl, v);
            }
        }
        power(true);
    }
    Some((before, clock(regs.r32(lctl))))
}

fn config_read(pci: &pcidev::Client, off: u16, width: u8) -> Option<u32> {
    pci.config_read(off, width).ok().and_then(|r| r.ok())
}

fn config_write(pci: &pcidev::Client, off: u16, width: u8, v: u32) {
    let _ = pci.config_write(off, width, v);
}

/// Lets an Intel controller since Skylake stop its clocks, or not.
fn set_clock_gating(pci: &pcidev::Client, on: bool) {
    if let Some(v) = config_read(pci, INTEL_CGCTL, 4) {
        let v = if on { v | INTEL_CGCTL_MISCBDCGE } else { v & !INTEL_CGCTL_MISCBDCGE };
        config_write(pci, INTEL_CGCTL, 4, v);
    }
}

/// Asks the chipset for transfers that snoop the CPU's caches: PCI Express
/// devices may not mark their transfers "no snoop", Intel's controllers
/// use traffic class 0, and ATI's, AMD's and NVIDIA's have snooping
/// switches of their own.
fn enable_snooping(pci: &pcidev::Client, info: &DeviceInfo, quirks: Quirks) {
    const PCI_EXPRESS: u8 = 0x10;
    const NO_SNOOP_ENABLE: u32 = 1 << 11;
    if let Some(at) = pci::find_capability(pci, PCI_EXPRESS)
        && let Some(control) = config_read(pci, at + 8, 2)
        && control & NO_SNOOP_ENABLE != 0
    {
        config_write(pci, at + 8, 2, control & !NO_SNOOP_ENABLE);
    }
    // TCSEL: traffic class 0 (some codecs play static otherwise).
    if info.vendor == INTEL
        && let Some(tcsel) = config_read(pci, 0x44, 1)
    {
        config_write(pci, 0x44, 1, tcsel & !0x07);
    }
    // The snoop field of MISC_CNTR2.
    if quirks.ati_snoop
        && let Some(misc) = config_read(pci, 0x42, 1)
    {
        config_write(pci, 0x42, 1, (misc & !0x07) | 0x02);
    }
    // The coherence bits of the transfers.
    if info.vendor == NVIDIA {
        if let Some(v) = config_read(pci, 0x4E, 1) {
            config_write(pci, 0x4E, 1, (v & !0x0F) | 0x0F);
        }
        for off in [0x4C, 0x4D] {
            if let Some(v) = config_read(pci, off, 1) {
                config_write(pci, off, 1, v | 0x01);
            }
        }
    }
}

/// A running controller.
pub struct Controller {
    regs: Registers,
    dma: Resource,
    pub irq: Option<Interrupt>,
    pub version: (u8, u8),
    pub input_streams: usize,
    pub output_streams: usize,
    pub bidirectional_streams: usize,
    /// The codecs on the link (bit `n`: address `n`).
    pub codecs: u16,
    /// The command ring (CORB) and the response ring (RIRB), which also
    /// brings the unsolicited responses when commands go through the
    /// immediate command registers.
    rings: DmaBuffer,
    corb_entries: usize,
    rirb_entries: usize,
    corb_wp: usize,
    rirb_rp: usize,
    /// Commands go through the immediate command registers.
    pub immediate_commands: bool,
    /// Intel since Skylake: the link's clock in MHz, as the firmware left
    /// it and as it is now.
    pub link_clock: Option<(u32, u32)>,
    /// Intel since Skylake: the processing pipe's control as the firmware
    /// left it (every stream is coupled to the link now).
    pub processing_pipe: Option<u32>,
    /// Stream positions come from the DMA position buffer.
    pub position_buffer: bool,
    /// Buffers each stream completed, by the interrupts seen.
    pub completions: [u32; 30],
    /// Unsolicited responses: (codec address, response).
    pub unsolicited: VecDeque<(u8, u32)>,
    /// The response ring overran (responses were lost).
    overruns: u32,
}

/// Where the response ring starts in the ring buffer, and the DMA position
/// buffer between the two rings (8 bytes for each stream).
const RIRB_OFFSET: usize = 2048;
const POSITIONS_OFFSET: usize = 1024;

/// The size code (0, 1, 2) and entries of the largest ring the controller
/// offers, from the capability bits of CORBSIZE or RIRBSIZE.
fn ring_size(size_register: u8) -> (u8, usize) {
    let caps = size_register >> 4;
    if caps & 0b100 != 0 {
        (2, 256)
    } else if caps & 0b010 != 0 {
        (1, 16)
    } else {
        (0, 2)
    }
}

/// Resets the controller and the link. Returns the codecs that announced
/// themselves (bit `n`: address `n`).
fn reset_link(regs: &Registers) -> Result<u16, &'static str> {
    regs.w16(reg::STATESTS, 0x7FFF);
    regs.w32(reg::GCTL, regs.r32(reg::GCTL) & !GCTL_CRST);
    if !regs.wait(100, |r| r.r32(reg::GCTL) & GCTL_CRST == 0) {
        return Err("the controller does not enter reset");
    }
    vrt::time::sleep(Duration::from_millis(1));
    regs.w32(reg::GCTL, regs.r32(reg::GCTL) | GCTL_CRST);
    if !regs.wait(100, |r| r.r32(reg::GCTL) & GCTL_CRST != 0) {
        return Err("the controller does not leave reset");
    }
    // Codecs request an address within 25 frames of the reset; give slow
    // ones longer.
    vrt::time::sleep(Duration::from_millis(1));
    regs.wait(100, |r| r.r16(reg::STATESTS) & 0x7FFF != 0);
    let codecs = regs.r16(reg::STATESTS) & 0x7FFF;
    regs.w16(reg::STATESTS, 0x7FFF);
    if codecs == 0 {
        return Err("no codec on the link");
    }
    Ok(codecs)
}

/// Starts the response ring in `rings`, and the command ring unless
/// commands go through the immediate command registers. Returns the
/// entries of each.
fn start_rings(regs: &Registers, rings: &DmaBuffer, immediate_commands: bool) -> (usize, usize) {
    let (corb_code, corb_entries) = ring_size(regs.r8(reg::CORBSIZE));
    let (rirb_code, rirb_entries) = ring_size(regs.r8(reg::RIRBSIZE));
    let corb = rings.phys();
    let rirb = rings.phys() + RIRB_OFFSET as u64;
    regs.w32(reg::CORBLBASE, corb as u32);
    regs.w32(reg::CORBUBASE, (corb >> 32) as u32);
    regs.w8(reg::CORBSIZE, (regs.r8(reg::CORBSIZE) & !0x03) | corb_code);
    regs.w16(reg::CORBWP, 0);
    // Reset the read pointer (some controllers do not show the bit while
    // it is set).
    regs.w16(reg::CORBRP, CORBRP_RST);
    regs.wait(10, |r| r.r16(reg::CORBRP) & CORBRP_RST != 0);
    regs.w16(reg::CORBRP, 0);
    regs.wait(10, |r| r.r16(reg::CORBRP) & CORBRP_RST == 0);
    regs.w32(reg::RIRBLBASE, rirb as u32);
    regs.w32(reg::RIRBUBASE, (rirb >> 32) as u32);
    regs.w8(reg::RIRBSIZE, (regs.r8(reg::RIRBSIZE) & !0x03) | rirb_code);
    regs.w16(reg::RIRBWP, RIRBWP_RST);
    regs.w16(reg::RINTCNT, 1);
    regs.w8(reg::RIRBSTS, RIRBSTS_CLEAR);
    // The response interrupt flag is wanted even without interrupts: after
    // RINTCNT responses a controller may wait for it to be cleared before
    // it sends more commands.
    regs.w8(reg::RIRBCTL, RIRBCTL_DMA | RIRBCTL_INTERRUPT);
    if !immediate_commands {
        regs.w8(reg::CORBCTL, CORBCTL_RUN);
        regs.wait(10, |r| r.r8(reg::CORBCTL) & CORBCTL_RUN != 0);
    }
    (corb_entries, rirb_entries)
}

/// Waits for `done` to give a codec's response: spinning at first (one
/// takes a frame or two of the link, 21 µs each), then sleeping between
/// looks. `None`: none came in time.
fn await_response<T>(mut done: impl FnMut() -> Option<T>) -> Option<T> {
    let start = now_ns();
    loop {
        if let Some(v) = done() {
            return Some(v);
        }
        let waited = now_ns() - start;
        if waited > RESPONSE_TIMEOUT_NS {
            return None;
        }
        if waited > 1_000_000 {
            vrt::time::sleep(Duration::from_millis(1));
        } else {
            core::hint::spin_loop();
        }
    }
}

impl Controller {
    /// Resets the controller, finds the codecs and starts the command
    /// rings.
    pub fn start(pci: &pcidev::Client, info: &DeviceInfo) -> Result<Controller, &'static str> {
        if !matches!(pci.enable(true), Ok(Ok(()))) {
            return Err("cannot enable the device");
        }
        let Ok(Ok(dma)) = pci.dma_resource() else { return Err("no DMA resource") };
        let Ok(Ok(vmo)) = pci.map_bar(0) else { return Err("cannot map the registers") };
        let len = vmo.size().map_err(|_| "cannot map the registers")?;
        let map = Mapping::new(vmo, len, map_flags::READ | map_flags::WRITE).map_err(|_| "cannot map the registers")?;
        let regs = Registers { base: map.as_ptr(), _map: map, len };
        let quirks = Quirks::of(info);
        enable_snooping(pci, info, quirks);

        let gcap = regs.r16(reg::GCAP);
        if gcap == u16::MAX {
            return Err("the controller does not answer");
        }
        let output_streams = ((gcap >> 12) & 0xF) as usize;
        let input_streams = ((gcap >> 8) & 0xF) as usize;
        let bidirectional_streams = ((gcap >> 3) & 0x1F) as usize;
        let streams = input_streams + output_streams + bidirectional_streams;
        if streams == 0 || reg::SD_BASE + streams * reg::SD_STRIDE > len {
            return Err("the registers do not fit their window");
        }

        // Stop whatever the firmware left running.
        for s in 0..streams {
            let at = reg::SD_BASE + s * reg::SD_STRIDE;
            regs.w32(at + reg::SD_CTL, regs.r32(at + reg::SD_CTL) & 0x00FF_FFFF & !SD_CTL_RUN);
        }
        regs.w8(reg::CORBCTL, 0);
        regs.w8(reg::RIRBCTL, 0);
        regs.wait(10, |r| r.r8(reg::CORBCTL) & CORBCTL_RUN == 0 && r.r8(reg::RIRBCTL) & RIRBCTL_DMA == 0);
        regs.w32(reg::INTCTL, 0);

        let rings = DmaBuffer::new_below_4g(&dma, 4096).map_err(|_| "out of DMA memory")?;
        flush_all(&rings);
        // Reset the controller and the link, and start the command rings.
        if quirks.clock_gating {
            set_clock_gating(pci, false);
        }
        let reset = reset_link(&regs);
        let entries = if reset.is_ok() { start_rings(&regs, &rings, quirks.immediate_commands) } else { (0, 0) };
        if quirks.clock_gating {
            set_clock_gating(pci, true);
        }
        let codecs = reset?;
        let (corb_entries, rirb_entries) = entries;
        let link_clock = if quirks.clock_gating { init_link_clock(&regs) } else { None };
        let processing_pipe = if quirks.skylake { couple_streams(&regs) } else { None };
        // The DMA position buffer, written by the controller as streams move.
        let positions = rings.phys() + POSITIONS_OFFSET as u64;
        regs.w32(reg::DPUBASE, (positions >> 32) as u32);
        regs.w32(reg::DPLBASE, positions as u32 | 1);

        let irq = if quirks.no_msi { None } else { pci::enable_msi(pci) };
        if irq.is_some() {
            regs.w32(reg::INTCTL, INTCTL_GIE | INTCTL_CIE);
        }
        // Accept unsolicited responses (jacks).
        regs.w32(reg::GCTL, regs.r32(reg::GCTL) | GCTL_UNSOL);
        let version = (regs.r8(reg::VMAJ), regs.r8(reg::VMIN));
        Ok(Controller {
            regs,
            dma,
            irq,
            version,
            input_streams,
            output_streams,
            bidirectional_streams,
            codecs,
            rings,
            corb_entries,
            rirb_entries,
            corb_wp: 0,
            rirb_rp: 0,
            immediate_commands: quirks.immediate_commands,
            link_clock,
            processing_pipe,
            position_buffer: quirks.skylake,
            completions: [0; 30],
            unsolicited: VecDeque::new(),
            overruns: 0,
        })
    }

    /// Stream `index`'s position as the controller last wrote it to the
    /// DMA position buffer.
    pub fn buffered_position(&self, index: usize) -> u32 {
        let at = POSITIONS_OFFSET + index * 8;
        // SAFETY: inside the ring buffer; drop a stale cache line first.
        let bytes = unsafe {
            flush(self.rings.ptr().add(at), 4);
            self.rings.bytes(at, 4)
        };
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    /// Sends a command to node `nid` of the codec at `codec` and waits for
    /// its response (`None`: none came in time).
    pub fn command(&mut self, codec: u8, nid: u8, verb: u32) -> Option<u32> {
        // A late answer to a command that timed out is not this one's.
        self.collect_responses();
        let command = vhda::verb::command(codec, nid, verb);
        if self.immediate_commands {
            return self.command_immediate(command);
        }
        self.corb_wp = (self.corb_wp + 1) % self.corb_entries;
        let entry = self.corb_wp * 4;
        self.rings.write(entry, &command.to_le_bytes());
        // SAFETY: inside the ring buffer.
        flush(unsafe { self.rings.ptr().add(entry) }, 4);
        self.regs.w16(reg::CORBWP, self.corb_wp as u16);
        await_response(|| {
            while let Some((response, extended)) = self.next_response() {
                if extended & RESPONSE_UNSOLICITED != 0 {
                    self.unsolicited.push_back(((extended & 0xF) as u8, response));
                } else if (extended & 0xF) as u8 == codec {
                    return Some(response);
                }
            }
            None
        })
    }

    /// Sends a command through the immediate command registers.
    fn command_immediate(&self, command: u32) -> Option<u32> {
        let r = &self.regs;
        await_response(|| (r.r16(reg::IRS) & IRS_BUSY == 0).then_some(()))?;
        r.w16(reg::IRS, IRS_VALID);
        r.w32(reg::IC, command);
        r.w16(reg::IRS, IRS_BUSY);
        let response = await_response(|| (r.r16(reg::IRS) & IRS_VALID != 0).then(|| r.r32(reg::IR)));
        r.w16(reg::IRS, IRS_VALID);
        response
    }

    /// Takes the responses the controller has written: unsolicited ones
    /// are kept, any other is a late answer to a command that timed out.
    fn collect_responses(&mut self) {
        while let Some((response, extended)) = self.next_response() {
            if extended & RESPONSE_UNSOLICITED != 0 {
                self.unsolicited.push_back(((extended & 0xF) as u8, response));
            }
        }
    }

    /// The next response the controller wrote, if any: (response, extended
    /// word).
    fn next_response(&mut self) -> Option<(u32, u32)> {
        let wp = (self.regs.r16(reg::RIRBWP) & 0xFF) as usize % self.rirb_entries;
        if wp == self.rirb_rp {
            // Everything read: clear the response flag, which lets the
            // controller send the next commands.
            let status = self.regs.r8(reg::RIRBSTS);
            if status & RIRBSTS_CLEAR != 0 {
                self.regs.w8(reg::RIRBSTS, status & RIRBSTS_CLEAR);
            }
            return None;
        }
        self.rirb_rp = (self.rirb_rp + 1) % self.rirb_entries;
        let at = RIRB_OFFSET + self.rirb_rp * 8;
        // SAFETY: inside the ring buffer; the controller has written it.
        let entry = unsafe {
            flush(self.rings.ptr().add(at), 8);
            self.rings.bytes(at, 8)
        };
        Some((
            u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]),
            u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]),
        ))
    }

    /// Acknowledges the controller's interrupts and collects unsolicited
    /// responses. A stream's errors stay for its owner ([`Stream::errors`]).
    pub fn service(&mut self) {
        // Until nothing is pending: with MSI, no new interrupt comes while
        // an old cause is still set.
        for _ in 0..10 {
            let status = self.regs.r32(reg::INTSTS);
            if status == 0 || status == u32::MAX {
                break;
            }
            for s in 0..30 {
                if status & 1 << s != 0 {
                    self.regs.w8(reg::SD_BASE + s * reg::SD_STRIDE + reg::SD_STS, SD_STS_BCIS);
                    self.completions[s] = self.completions[s].wrapping_add(1);
                }
            }
            let rirb = self.regs.r8(reg::RIRBSTS);
            if rirb & RIRBSTS_CLEAR != 0 {
                self.regs.w8(reg::RIRBSTS, rirb & RIRBSTS_CLEAR);
                if rirb & RIRBSTS_OVERRUN != 0 {
                    self.overruns += 1;
                    if self.overruns <= 3 {
                        println!("the response ring overran");
                    }
                }
            }
            self.collect_responses();
        }
        // Without interrupts, responses are found here.
        self.collect_responses();
    }

    /// Whether the controller still answers (it reads all ones when gone).
    pub fn healthy(&self) -> bool {
        self.regs.r32(reg::GCTL) != u32::MAX
    }

    /// The link's bit clock count (24 MHz), for checking the link's speed.
    pub fn wall_clock(&self) -> u32 {
        self.regs.r32(reg::WALLCLK)
    }

    fn stream_base(&self, index: usize) -> usize {
        reg::SD_BASE + index * reg::SD_STRIDE
    }

    /// A stream: descriptor `index` (see [`reg::SD_BASE`]) with `periods`
    /// buffers of `period_bytes` each.
    pub fn stream(
        &self,
        index: usize,
        tag: u8,
        output: bool,
        periods: usize,
        period_bytes: usize,
    ) -> Result<Stream, &'static str> {
        let memory =
            DmaBuffer::new_below_4g(&self.dma, BDL_BYTES + periods * period_bytes).map_err(|_| "out of DMA memory")?;
        for i in 0..periods {
            let mut entry = [0u8; 16];
            let address = memory.phys() + (BDL_BYTES + i * period_bytes) as u64;
            entry[..8].copy_from_slice(&address.to_le_bytes());
            entry[8..12].copy_from_slice(&(period_bytes as u32).to_le_bytes());
            entry[12..16].copy_from_slice(&BDL_IOC.to_le_bytes());
            memory.write(i * 16, &entry);
        }
        flush_all(&memory);
        let bidirectional = index >= self.input_streams + self.output_streams;
        let stream = Stream { index, tag, output, bidirectional, memory, periods, period_bytes };
        stream.reset(self);
        Ok(stream)
    }
}

/// A stream descriptor and its DMA buffer: a ring of equal periods that
/// the controller goes round and round, interrupting after each.
pub struct Stream {
    index: usize,
    tag: u8,
    output: bool,
    bidirectional: bool,
    memory: DmaBuffer,
    pub periods: usize,
    pub period_bytes: usize,
}

impl Stream {
    fn base(&self, hc: &Controller) -> usize {
        hc.stream_base(self.index)
    }

    /// Stops the stream and resets its descriptor.
    pub fn reset(&self, hc: &Controller) {
        let at = self.base(hc) + reg::SD_CTL;
        let r = &hc.regs;
        let control = r.r32(at) & 0x00FF_FFFF & !SD_CTL_RUN;
        r.w32(at, control);
        r.wait(10, |r| r.r32(at) & SD_CTL_RUN == 0);
        r.w32(at, control | SD_CTL_SRST);
        r.wait(10, |r| r.r32(at) & SD_CTL_SRST != 0);
        r.w32(at, control & !SD_CTL_SRST);
        r.wait(10, |r| r.r32(at) & SD_CTL_SRST == 0);
        r.w8(self.base(hc) + reg::SD_STS, SD_STS_CLEAR);
    }

    /// Starts the stream in `format` from the first period, programmed in
    /// Linux's order: the stream tag, the buffer and the format, the
    /// interrupt, then RUN once the controller has sized the stream's FIFO
    /// for the format.
    pub fn start(&self, hc: &Controller, format: u16) {
        self.reset(hc);
        let (base, r) = (self.base(hc), &hc.regs);
        let direction = if self.bidirectional && self.output { SD_CTL_DIR_OUT } else { 0 };
        let mut control = (self.tag as u32 & 0xF) << 20 | direction;
        r.w32(base + reg::SD_CTL, control);
        r.w32(base + reg::SD_CBL, (self.periods * self.period_bytes) as u32);
        r.w16(base + reg::SD_FMT, format);
        r.w16(base + reg::SD_LVI, (self.periods - 1) as u16);
        let list = self.memory.phys();
        r.w32(base + reg::SD_BDPL, list as u32);
        r.w32(base + reg::SD_BDPU, (list >> 32) as u32);
        if hc.irq.is_some() {
            control |= SD_CTL_IOCE;
            r.w32(base + reg::SD_CTL, control);
            r.w32(reg::INTCTL, r.r32(reg::INTCTL) | 1 << self.index);
        }
        r.wait(1, |r| r.r16(base + reg::SD_FIFOS) != 0);
        r.w8(base + reg::SD_STS, SD_STS_CLEAR);
        r.w32(base + reg::SD_CTL, control | SD_CTL_RUN);
    }

    /// The format the stream descriptor holds, and the size of its FIFO,
    /// for the log.
    pub fn format_and_fifo(&self, hc: &Controller) -> (u16, u16) {
        let base = self.base(hc);
        (hc.regs.r16(base + reg::SD_FMT), hc.regs.r16(base + reg::SD_FIFOS))
    }

    pub fn stop(&self, hc: &Controller) {
        let r = &hc.regs;
        r.w32(reg::INTCTL, r.r32(reg::INTCTL) & !(1 << self.index));
        self.reset(hc);
    }

    /// Where the controller is in the buffer, in bytes: from the DMA
    /// position buffer where Linux takes it from there (Intel since
    /// Skylake; reading the link position first brings it up to date, as
    /// Linux does), the link position (LPIB) elsewhere.
    pub fn position(&self, hc: &Controller) -> usize {
        let link = self.link_position(hc);
        if hc.position_buffer { self.buffer_position(hc) } else { link }
    }

    /// The link position register (LPIB), in bytes.
    pub fn link_position(&self, hc: &Controller) -> usize {
        hc.regs.r32(self.base(hc) + reg::SD_LPIB) as usize % (self.periods * self.period_bytes)
    }

    /// The DMA position buffer's entry for this stream, in bytes.
    pub fn buffer_position(&self, hc: &Controller) -> usize {
        hc.buffered_position(self.index) as usize % (self.periods * self.period_bytes)
    }

    /// Intel since Skylake: the DMA position register, in bytes (for the
    /// log).
    pub fn dma_position(&self, hc: &Controller) -> Option<usize> {
        hc.position_buffer.then(|| {
            hc.regs.r32(INTEL_DPIB + self.index * reg::SD_STRIDE) as usize % (self.periods * self.period_bytes)
        })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// The stream's errors ([`SD_STS_FIFOE`], [`SD_STS_DESE`]) since the
    /// last look.
    pub fn errors(&self, hc: &Controller) -> u8 {
        let at = self.base(hc) + reg::SD_STS;
        let errors = hc.regs.r8(at) & (SD_STS_FIFOE | SD_STS_DESE);
        if errors != 0 {
            hc.regs.w8(at, errors);
        }
        errors
    }

    /// The stream descriptor's control register with its status in the
    /// top byte, for the log.
    pub fn state(&self, hc: &Controller) -> u32 {
        hc.regs.r32(self.base(hc) + reg::SD_CTL)
    }

    fn period_at(&self, period: usize) -> usize {
        BDL_BYTES + (period % self.periods) * self.period_bytes
    }

    /// Fills `period` with `samples`, then silence.
    pub fn write_period(&self, period: usize, samples: &[i16]) {
        let at = self.period_at(period);
        // SAFETY: i16 is little-endian on x86; the slice is initialised.
        let bytes = unsafe { core::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * 2) };
        let n = bytes.len().min(self.period_bytes);
        self.memory.write(at, &bytes[..n]);
        // SAFETY: inside this period, which the controller is not reading.
        unsafe {
            core::ptr::write_bytes(self.memory.ptr().add(at + n), 0, self.period_bytes - n);
            flush(self.memory.ptr().add(at), self.period_bytes);
        }
    }

    /// Fills `period` with silence.
    pub fn clear_period(&self, period: usize) {
        self.write_period(period, &[]);
    }

    /// Silences the whole buffer.
    pub fn clear(&self) {
        for p in 0..self.periods {
            self.clear_period(p);
        }
    }

    /// Reads `period` (which the controller has finished writing) into
    /// `out`.
    pub fn read_period(&self, period: usize, out: &mut [i16]) {
        let at = self.period_at(period);
        // SAFETY: inside the buffer; drop stale cache lines first.
        let bytes = unsafe {
            flush(self.memory.ptr().add(at), self.period_bytes);
            self.memory.bytes(at, self.period_bytes)
        };
        for (o, b) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *o = i16::from_le_bytes(*b);
        }
    }
}
