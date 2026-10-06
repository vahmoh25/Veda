//! Cirrus Logic's DSP firmware files, read as Linux's `cs_dsp` reads them
//! for a HALO core (the DSP inside the CS35L41):
//!
//! * a `.wmfw` file is the firmware: blocks of program and data memory,
//!   and descriptions of its algorithms' controls (named places in their
//!   memory, such as the state the firmware reports);
//! * a `.bin` file ("WMDR") holds coefficients: blocks for an algorithm's
//!   memory, wherever the loaded firmware says that is (the tuning of one
//!   board's speakers);
//! * a `.bincfg` file holds the driver's parameters for that tuning (the
//!   amplifier's gain).
//!
//! Reading touches nothing: the result says what goes where.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Why a file cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// Not a file of this kind.
    Magic,
    /// A format version this does not read.
    Version(u32),
    /// Firmware for another kind of DSP core.
    Core(u32),
    /// A block runs past the end of the file, or a length does not add up.
    Truncated,
    /// A block for a memory a HALO core does not have.
    Memory(u32),
    /// A control of a type Linux does not know, or with flags it refuses.
    Control(u32),
    /// Data that is not whole registers.
    Unaligned,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Magic => f.write_str("not a firmware file of this kind"),
            FormatError::Version(v) => write!(f, "file format {} is not known", v),
            FormatError::Core(c) => write!(f, "it is for another DSP core ({})", c),
            FormatError::Truncated => f.write_str("it is cut short"),
            FormatError::Memory(t) => write!(f, "a block for memory {:#x}, which the DSP does not have", t),
            FormatError::Control(t) => write!(f, "a control of unknown type {:#x}", t),
            FormatError::Unaligned => f.write_str("a block that is not whole registers"),
        }
    }
}

/// The memories of a HALO core, as the files name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Memory {
    /// Program memory: 40-bit words, four to five registers.
    Pm,
    /// The X and Y data memories: 24-bit words, four to three registers...
    XmPacked,
    YmPacked,
    /// ...or one to a register.
    Xm,
    Ym,
}

impl Memory {
    /// The memory of a block or control type (`WMFW_HALO_*`,
    /// `WMFW_ADSP2_XM`/`YM`).
    pub fn of(kind: u32) -> Option<Memory> {
        match kind {
            PM_PACKED => Some(Memory::Pm),
            XM_PACKED => Some(Memory::XmPacked),
            YM_PACKED => Some(Memory::YmPacked),
            XM => Some(Memory::Xm),
            YM => Some(Memory::Ym),
            _ => None,
        }
    }

    /// The type number the files use.
    pub fn kind(self) -> u32 {
        match self {
            Memory::Pm => PM_PACKED,
            Memory::XmPacked => XM_PACKED,
            Memory::YmPacked => YM_PACKED,
            Memory::Xm => XM,
            Memory::Ym => YM,
        }
    }
}

// Block and memory types (`wmfw.h`).
const ABSOLUTE: u32 = 0xF0;
const ALGORITHM_DATA: u32 = 0xF2;
const METADATA: u32 = 0xFC;
const NAME_TEXT: u32 = 0xFE;
const INFO_TEXT: u32 = 0xFF;
/// The older cores' memories, which a HALO core does not have.
const ADSP1_PM: u32 = 2;
const ADSP1_DM: u32 = 3;
const ADSP1_ZM: u32 = 4;
pub const XM: u32 = 5;
pub const YM: u32 = 6;
const PM_PACKED: u32 = 0x10;
const XM_PACKED: u32 = 0x11;
const YM_PACKED: u32 = 0x12;
/// Coefficient blocks with a 32-bit offset (`WMFW_*_LONG`).
const XM_LONG: u32 = 0xF405;
const YM_LONG: u32 = 0xF406;
const XM_PACKED_LONG: u32 = 0xF411;
const YM_PACKED_LONG: u32 = 0xF412;

/// A HALO core (`WMFW_HALO`), and the only `.wmfw` format it has.
const HALO: u32 = 4;
const HALO_FORMAT: u32 = 3;

// Control types and flags.
pub const CTL_BYTES: u32 = 0x0004;
const CTL_ACKED: u32 = 0x1000;
const CTL_HOSTEVENT: u32 = 0x1001;
const CTL_HOST_BUFFER: u32 = 0x1002;
const CTL_FWEVENT: u32 = 0x1004;
pub const FLAG_SYS: u16 = 0x8000;
pub const FLAG_VOLATILE: u16 = 0x0004;
pub const FLAG_WRITEABLE: u16 = 0x0002;
pub const FLAG_READABLE: u16 = 0x0001;

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

/// Where a firmware block goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Into a memory, from a word.
    Memory(Memory, u32),
    /// Into the registers from this one.
    Register(u32),
}

/// A block of a `.wmfw` file to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<'a> {
    pub place: Place,
    pub data: &'a [u8],
}

/// A named place in an algorithm's memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub algorithm: u32,
    pub name: String,
    /// Its memory's type number (`XM`: an X memory word per register).
    pub memory: u32,
    /// Words from the start of the algorithm's part of that memory.
    pub offset: u32,
    pub flags: u16,
    pub kind: u32,
    /// Bytes.
    pub len: u32,
}

/// A `.wmfw` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firmware<'a> {
    /// When it was built, as the file has it.
    pub timestamp: u64,
    /// Its text blocks (a build date, a name).
    pub text: Vec<&'a [u8]>,
    pub blocks: Vec<Block<'a>>,
    pub controls: Vec<Control>,
    /// Blocks of types Linux does not know, skipped (as it does).
    pub skipped: Vec<u32>,
}

impl Firmware<'_> {
    /// A control, by its name, memory and algorithm (`cs_dsp_get_ctl`).
    pub fn control(&self, name: &str, memory: u32, algorithm: u32) -> Option<&Control> {
        self.controls.iter().find(|c| c.name == name && c.memory == memory && c.algorithm == algorithm)
    }
}

/// Reads a `.wmfw` file for a HALO core (`cs_dsp_load`).
pub fn firmware(data: &[u8]) -> Result<Firmware<'_>, FormatError> {
    // The header: magic, length, revision, core, format; the memories'
    // sizes; a footer (timestamp, checksum).
    if data.len() <= 12 {
        return Err(FormatError::Truncated);
    }
    if &data[..4] != b"WMFW" {
        return Err(FormatError::Magic);
    }
    let (core, format) = (data[10] as u32, data[11] as u32);
    if format != HALO_FORMAT {
        return Err(FormatError::Version(format));
    }
    if core != HALO {
        return Err(FormatError::Core(core));
    }
    let footer = 12 + 16;
    let mut pos = footer + 12;
    let timestamp = u64_at(data, footer).ok_or(FormatError::Truncated)?;
    if data.len() < pos || u32_at(data, 4) != Some(pos as u32) {
        return Err(FormatError::Truncated);
    }
    let mut fw =
        Firmware { timestamp, text: Vec::new(), blocks: Vec::new(), controls: Vec::new(), skipped: Vec::new() };
    while pos < data.len() {
        let (Some(head), Some(len)) = (u32_at(data, pos), u32_at(data, pos + 4)) else {
            return Err(FormatError::Truncated);
        };
        let start = pos + 8;
        let body = data.get(start..start + len as usize).ok_or(FormatError::Truncated)?;
        // The first three bytes are the offset, the fourth the type.
        let (offset, kind) = (head & 0xFF_FFFF, head >> 24);
        match kind {
            INFO_TEXT | NAME_TEXT => fw.text.push(body),
            ALGORITHM_DATA => controls(body, &mut fw.controls)?,
            ABSOLUTE => fw.blocks.push(Block { place: Place::Register(offset), data: body }),
            ADSP1_PM | ADSP1_DM | ADSP1_ZM => return Err(FormatError::Memory(kind)),
            k => match Memory::of(k) {
                Some(m) => fw.blocks.push(Block { place: Place::Memory(m, offset), data: body }),
                None => fw.skipped.push(k),
            },
        }
        pos = start + len as usize;
    }
    if fw.blocks.iter().any(|b| b.data.len() % 4 != 0) {
        return Err(FormatError::Unaligned);
    }
    Ok(fw)
}

/// A string field: its length in `bytes` bytes, then the string, padded
/// to whole 32-bit words (`cs_dsp_coeff_parse_string`).
fn string<'a>(d: &'a [u8], pos: &mut usize, bytes: usize) -> Result<&'a [u8], FormatError> {
    let avail = d.len().saturating_sub(*pos);
    if avail < 4 {
        return Err(FormatError::Truncated);
    }
    let len = if bytes == 1 { d[*pos] as usize } else { u16_at(d, *pos).ok_or(FormatError::Truncated)? as usize };
    let total = (len + bytes + 3) & !3;
    if total > avail {
        return Err(FormatError::Truncated);
    }
    let s = &d[*pos + bytes..*pos + bytes + len];
    *pos += total;
    Ok(s)
}

/// An algorithm's description and its controls (`cs_dsp_parse_coeff`, in
/// the format of firmware files 2 and later).
fn controls(d: &[u8], out: &mut Vec<Control>) -> Result<(), FormatError> {
    let algorithm = u32_at(d, 0).ok_or(FormatError::Truncated)?;
    let mut pos = 4;
    string(d, &mut pos, 1)?;
    string(d, &mut pos, 2)?;
    let count = u32_at(d, pos).ok_or(FormatError::Truncated)?;
    pos += 4;
    if count > i32::MAX as u32 {
        return Err(FormatError::Truncated);
    }
    for _ in 0..count {
        let (Some(offset), Some(memory), Some(size)) = (u16_at(d, pos), u16_at(d, pos + 2), u32_at(d, pos + 4)) else {
            return Err(FormatError::Truncated);
        };
        let end = (pos + 8).checked_add(size as usize).filter(|&e| e <= d.len()).ok_or(FormatError::Truncated)?;
        let mut p = pos + 8;
        let name = string(d, &mut p, 1)?;
        string(d, &mut p, 1)?;
        string(d, &mut p, 2)?;
        let (Some(kind), Some(flags), Some(len)) = (u16_at(d, p), u16_at(d, p + 2), u32_at(d, p + 4)) else {
            return Err(FormatError::Truncated);
        };
        pos = end;
        let kind = kind as u32;
        let required = match kind {
            CTL_BYTES => 0,
            // Linux leaves the firmware's own acked controls alone.
            CTL_ACKED if flags & FLAG_SYS != 0 => continue,
            CTL_ACKED => FLAG_VOLATILE | FLAG_WRITEABLE | FLAG_READABLE,
            CTL_HOSTEVENT | CTL_FWEVENT => FLAG_SYS | FLAG_VOLATILE | FLAG_WRITEABLE | FLAG_READABLE,
            CTL_HOST_BUFFER => FLAG_SYS | FLAG_VOLATILE | FLAG_READABLE,
            _ => return Err(FormatError::Control(kind)),
        };
        if flags & required != required {
            return Err(FormatError::Control(kind));
        }
        out.push(Control {
            algorithm,
            name: String::from_utf8_lossy(name).into_owned(),
            memory: memory as u32,
            offset: offset as u32,
            flags,
            kind,
            len,
        });
    }
    Ok(())
}

/// Where a coefficient block goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Into an algorithm's part of a memory, `offset` bytes of registers
    /// past its start; written for version `version` of the algorithm.
    Algorithm { memory: Memory, algorithm: u32, version: u32, offset: u32 },
    /// Into the registers from `register`; or, if `algorithm` is the
    /// firmware's own and `register` 0, the global coefficients of older
    /// cores, which a HALO core does not have.
    Absolute { algorithm: u32, register: u32 },
    /// A memory of the older cores (its type): skipped, as Linux does.
    Other(u32),
}

/// A block of a `.bin` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coefficient<'a> {
    pub target: Target,
    pub data: &'a [u8],
}

/// A `.bin` (coefficient) file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coefficients<'a> {
    /// Its format (1 to 3).
    pub format: u32,
    /// The firmware version it was made for (`0x00MMmmpp`).
    pub version: u32,
    /// Its name, if it has one (the tuning tool's file name).
    pub name: Option<&'a [u8]>,
    pub blocks: Vec<Coefficient<'a>>,
}

/// Reads a `.bin` coefficient file (`cs_dsp_load_coeff`).
pub fn coefficients(data: &[u8]) -> Result<Coefficients<'_>, FormatError> {
    if data.len() <= 16 {
        return Err(FormatError::Truncated);
    }
    if &data[..4] != b"WMDR" {
        return Err(FormatError::Magic);
    }
    // The revision is big-endian; the version, in the same word, little.
    let format = data[11] as u32;
    if !(1..=3).contains(&format) {
        return Err(FormatError::Version(format));
    }
    let version = u32_at(data, 8).unwrap_or(0);
    let mut out = Coefficients { format, version, name: None, blocks: Vec::new() };
    let mut pos = u32_at(data, 4).unwrap_or(0) as usize;
    while pos < data.len() {
        let field = |at: usize| u32_at(data, pos + at).ok_or(FormatError::Truncated);
        let (offset, kind) = (u16_at(data, pos).ok_or(FormatError::Truncated)?, u16_at(data, pos + 2));
        let kind = kind.ok_or(FormatError::Truncated)? as u32;
        let (algorithm, block_version, offset32, len) = (field(4)?, field(8)?, field(12)?, field(16)? as usize);
        let start = pos + 20;
        let body = data.get(start..start.checked_add(len).ok_or(FormatError::Truncated)?);
        let body = body.ok_or(FormatError::Truncated)?;
        let version = block_version >> 8;
        let target = match kind {
            k if k == NAME_TEXT << 8 => {
                out.name = Some(body);
                None
            }
            k if k == INFO_TEXT << 8 || k == METADATA << 8 => None,
            k if k == ABSOLUTE << 8 => Some(Target::Absolute { algorithm, register: offset as u32 }),
            XM_LONG | YM_LONG | XM_PACKED_LONG | YM_PACKED_LONG => {
                Memory::of(kind & 0xFF).map(|memory| Target::Algorithm { memory, algorithm, version, offset: offset32 })
            }
            k => Some(match Memory::of(k) {
                Some(memory) => Target::Algorithm { memory, algorithm, version, offset: offset as u32 },
                None => Target::Other(k),
            }),
        };
        if let Some(target) = target {
            out.blocks.push(Coefficient { target, data: body });
        }
        pos = start + ((len + 3) & !3);
    }
    Ok(out)
}

/// The signature of a tuning parameter file (`.bincfg`).
const TUNING_SIGNATURE: u32 = 0x109A_4A35;
/// Its gain parameter.
const TUNING_GAIN: u32 = 0;

/// The amplifier gain a tuning parameter file (`.bincfg`) sets, if any
/// (`cs35l41_read_tuning_params`): the PCM gain's register value.
pub fn tuning_gain(data: &[u8]) -> Result<Option<u32>, FormatError> {
    let header = |at: usize| u32_at(data, at).ok_or(FormatError::Truncated);
    if header(8)? as usize != data.len() {
        return Err(FormatError::Truncated);
    }
    if header(4)? != 1 {
        return Err(FormatError::Version(header(4)?));
    }
    if header(0)? != TUNING_SIGNATURE {
        return Err(FormatError::Magic);
    }
    let entries = header(12)?;
    let params = &data[16..];
    let end = params.len();
    let (mut offset, mut gain) = (0usize, None);
    for _ in 0..entries {
        if offset >= end || offset + 12 >= end {
            return Err(FormatError::Truncated);
        }
        let (kind, size) = (u32_at(params, offset + 4), u32_at(params, offset + 8));
        let (kind, size) = (kind.ok_or(FormatError::Truncated)?, size.ok_or(FormatError::Truncated)?);
        if kind == TUNING_GAIN {
            gain = Some(u32_at(params, offset + 12).ok_or(FormatError::Truncated)?);
        }
        offset = offset.checked_add(size as usize).ok_or(FormatError::Truncated)?;
        if offset > end {
            return Err(FormatError::Truncated);
        }
    }
    Ok(gain)
}
