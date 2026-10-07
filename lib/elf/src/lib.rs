//! `velf` — ELF64 executables for Veda's POSIX programs.
//!
//! C programs built by Veda's GCC are static ELF64 executables for the
//! System V x86-64 ABI, as on Linux. Loading one takes:
//!
//! 1. [`Elf::parse`], which validates the header and every program header,
//!    so a loader can trust what it gets from then on;
//! 2. copying each [`Segment`]'s file bytes into memory at its address
//!    (plus the load bias of a position-independent executable), the rest
//!    of the segment zero;
//! 3. mapping the [`page_runs`], the image's pages grouped by permissions;
//! 4. building the [`initial_stack`] (argument and environment strings, the
//!    auxiliary vector) and starting the program at [`Elf::entry`] with the
//!    stack pointer the builder returns.
//!
//! Executables that need a dynamic linker (`PT_INTERP`) are refused: Veda
//! links C programs statically.
//!
//! The crate is plain data processing, unit-tested on the host.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// Errors produced while parsing an executable or laying out its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// The file is shorter than a header it claims to contain.
    Truncated,
    /// Not an ELF file.
    BadMagic,
    /// Not a 64-bit little-endian ELF file of the current version.
    WrongClass,
    /// Not an x86-64 file.
    WrongMachine,
    /// A relocatable object or core dump, not an executable.
    NotExecutable,
    /// The executable needs a dynamic linker.
    Dynamic,
    /// A program header is malformed or segments overlap.
    BadSegment,
    /// Nothing to load, or the program headers are not in memory.
    NoSegments,
    /// The entry point is not in an executable segment.
    BadEntry,
    /// The image or its arguments are larger than the space for them.
    TooLarge,
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ElfError::Truncated => "the file is truncated",
            ElfError::BadMagic => "not an ELF file",
            ElfError::WrongClass => "not a 64-bit little-endian ELF file",
            ElfError::WrongMachine => "not an x86-64 program",
            ElfError::NotExecutable => "not an executable (an object file or core dump)",
            ElfError::Dynamic => "dynamically linked (Veda runs static executables)",
            ElfError::BadSegment => "malformed program header",
            ElfError::NoSegments => "nothing to load",
            ElfError::BadEntry => "the entry point is not in executable code",
            ElfError::TooLarge => "too large",
        })
    }
}

pub const PAGE_SIZE: u64 = 4096;
/// Size of an ELF64 file header.
const EHDR_SIZE: usize = 64;
/// Size of an ELF64 program header.
pub const PHDR_SIZE: usize = 56;
/// Largest image (from its lowest to its highest page) accepted.
const MAX_IMAGE: u64 = 4 << 30;

const EM_X86_64: u16 = 62;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;

/// Program header types.
pub mod pt {
    pub const LOAD: u32 = 1;
    pub const DYNAMIC: u32 = 2;
    pub const INTERP: u32 = 3;
    pub const PHDR: u32 = 6;
    pub const TLS: u32 = 7;
    pub const GNU_STACK: u32 = 0x6474_e551;
}

/// Segment permission flags (`p_flags`).
pub mod pf {
    pub const X: u32 = 1;
    pub const W: u32 = 2;
    pub const R: u32 = 4;
}

/// Memory permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Perms {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perms {
    fn from_flags(flags: u32) -> Perms {
        Perms { read: flags & pf::R != 0, write: flags & pf::W != 0, exec: flags & pf::X != 0 }
    }

    fn union(self, o: Perms) -> Perms {
        Perms { read: self.read || o.read, write: self.write || o.write, exec: self.exec || o.exec }
    }
}

fn u16_at(d: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([d[off], d[off + 1]])
}

fn u32_at(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(d[off..off + 4].try_into().unwrap())
}

fn u64_at(d: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(d[off..off + 8].try_into().unwrap())
}

fn page_down(a: u64) -> u64 {
    a & !(PAGE_SIZE - 1)
}

fn page_up(a: u64) -> Option<u64> {
    Some(a.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1))
}

/// A program header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramHeader {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub file_size: u64,
    pub mem_size: u64,
    pub align: u64,
}

impl ProgramHeader {
    fn read(d: &[u8], off: usize) -> ProgramHeader {
        ProgramHeader {
            kind: u32_at(d, off),
            flags: u32_at(d, off + 4),
            offset: u64_at(d, off + 8),
            vaddr: u64_at(d, off + 16),
            file_size: u64_at(d, off + 32),
            mem_size: u64_at(d, off + 40),
            align: u64_at(d, off + 48),
        }
    }
}

/// A loadable segment: `data` belongs at `vaddr`, followed by zeros up to
/// `mem_size` bytes.
#[derive(Debug, Clone, Copy)]
pub struct Segment<'a> {
    pub vaddr: u64,
    pub mem_size: u64,
    pub data: &'a [u8],
    pub perms: Perms,
}

/// A validated ELF64 x86-64 executable.
#[derive(Debug, Clone, Copy)]
pub struct Elf<'a> {
    data: &'a [u8],
    pie: bool,
    entry: u64,
    phoff: usize,
    phnum: usize,
    phdr_vaddr: u64,
    lo: u64,
    hi: u64,
}

/// Whether `data` starts like an ELF file.
pub fn is_elf(data: &[u8]) -> bool {
    data.starts_with(b"\x7fELF")
}

impl<'a> Elf<'a> {
    /// Parses and validates an executable.
    pub fn parse(data: &'a [u8]) -> Result<Elf<'a>, ElfError> {
        if data.len() < EHDR_SIZE {
            return Err(if is_elf(data) || data.len() < 4 { ElfError::Truncated } else { ElfError::BadMagic });
        }
        if !is_elf(data) {
            return Err(ElfError::BadMagic);
        }
        // Class 64, little endian, version 1.
        if data[4] != 2 || data[5] != 1 || data[6] != 1 || u32_at(data, 20) != 1 {
            return Err(ElfError::WrongClass);
        }
        if u16_at(data, 18) != EM_X86_64 {
            return Err(ElfError::WrongMachine);
        }
        let pie = match u16_at(data, 16) {
            ET_EXEC => false,
            ET_DYN => true,
            _ => return Err(ElfError::NotExecutable),
        };
        let entry = u64_at(data, 24);
        let phoff = u64_at(data, 32);
        let phentsize = u16_at(data, 54) as usize;
        let phnum = u16_at(data, 56) as usize;
        if phnum == 0 {
            return Err(ElfError::NoSegments);
        }
        if phentsize != PHDR_SIZE {
            return Err(ElfError::BadSegment);
        }
        let ph_end = phoff.checked_add((phnum * PHDR_SIZE) as u64).ok_or(ElfError::Truncated)?;
        if ph_end > data.len() as u64 {
            return Err(ElfError::Truncated);
        }
        let phoff = phoff as usize;

        let mut lo = u64::MAX;
        let mut hi = 0u64;
        let mut prev_end = 0u64;
        let mut phdr_vaddr = None;
        let mut loads = 0;
        for i in 0..phnum {
            let ph = ProgramHeader::read(data, phoff + i * PHDR_SIZE);
            match ph.kind {
                pt::INTERP => return Err(ElfError::Dynamic),
                pt::PHDR => phdr_vaddr = Some(ph.vaddr),
                pt::LOAD if ph.mem_size > 0 => {
                    let file_end = ph.offset.checked_add(ph.file_size).ok_or(ElfError::BadSegment)?;
                    let end = ph.vaddr.checked_add(ph.mem_size).ok_or(ElfError::BadSegment)?;
                    if ph.file_size > ph.mem_size || file_end > data.len() as u64 {
                        return Err(ElfError::BadSegment);
                    }
                    // Segments come in ascending address order and must not
                    // overlap (they may share a page).
                    if loads > 0 && ph.vaddr < prev_end {
                        return Err(ElfError::BadSegment);
                    }
                    prev_end = end;
                    lo = lo.min(page_down(ph.vaddr));
                    hi = hi.max(page_up(end).ok_or(ElfError::BadSegment)?);
                    // The program headers live wherever their file bytes
                    // were loaded.
                    if phdr_vaddr.is_none() && ph.offset <= phoff as u64 && ph_end <= file_end {
                        phdr_vaddr = Some(ph.vaddr + (phoff as u64 - ph.offset));
                    }
                    loads += 1;
                }
                _ => {}
            }
        }
        if loads == 0 {
            return Err(ElfError::NoSegments);
        }
        let phdr_vaddr = phdr_vaddr.ok_or(ElfError::NoSegments)?;
        if hi - lo > MAX_IMAGE {
            return Err(ElfError::TooLarge);
        }
        let elf = Elf { data, pie, entry, phoff, phnum, phdr_vaddr, lo, hi };
        if !elf.segments().any(|s| s.perms.exec && s.vaddr <= entry && entry < s.vaddr + s.mem_size) {
            return Err(ElfError::BadEntry);
        }
        // The C library reads the program headers where AT_PHDR says (to
        // find its thread-local storage): among the bytes loaded from the
        // file.
        let phdr_end = phdr_vaddr.checked_add((phnum * PHDR_SIZE) as u64).ok_or(ElfError::NoSegments)?;
        if !elf.segments().any(|s| s.vaddr <= phdr_vaddr && phdr_end <= s.vaddr + s.data.len() as u64) {
            return Err(ElfError::NoSegments);
        }
        Ok(elf)
    }

    /// A position-independent executable, loadable at any page-aligned bias.
    pub fn is_position_independent(&self) -> bool {
        self.pie
    }

    /// The entry point (before the load bias).
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// Where the program headers are in memory (before the load bias).
    pub fn phdr_vaddr(&self) -> u64 {
        self.phdr_vaddr
    }

    /// The number of program headers.
    pub fn phnum(&self) -> usize {
        self.phnum
    }

    /// The page-aligned address range `[lo, hi)` the image occupies (before
    /// the load bias).
    pub fn span(&self) -> (u64, u64) {
        (self.lo, self.hi)
    }

    /// Every program header.
    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + 'a {
        let (data, phoff) = (self.data, self.phoff);
        (0..self.phnum).map(move |i| ProgramHeader::read(data, phoff + i * PHDR_SIZE))
    }

    /// The loadable segments, in ascending address order.
    pub fn segments(&self) -> impl Iterator<Item = Segment<'a>> + 'a {
        let data = self.data;
        self.program_headers().filter(|ph| ph.kind == pt::LOAD && ph.mem_size > 0).map(move |ph| Segment {
            vaddr: ph.vaddr,
            mem_size: ph.mem_size,
            data: &data[ph.offset as usize..(ph.offset + ph.file_size) as usize],
            perms: Perms::from_flags(ph.flags),
        })
    }

    /// The main thread's stack size the program asks for (`PT_GNU_STACK`),
    /// if any.
    pub fn stack_size(&self) -> Option<u64> {
        self.program_headers().find(|ph| ph.kind == pt::GNU_STACK && ph.mem_size > 0).map(|ph| ph.mem_size)
    }
}

/// Pages `[start, start+len)` of an image, all with the same permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRun {
    pub start: u64,
    pub len: u64,
    pub perms: Perms,
}

/// The image's pages grouped into runs of equal permissions, in address
/// order (before the load bias). A page shared by two segments gets the
/// permissions of both; pages no segment touches are left out.
pub fn page_runs(elf: &Elf) -> Vec<PageRun> {
    let segs: Vec<(u64, u64, Perms)> =
        elf.segments().map(|s| (page_down(s.vaddr), page_up(s.vaddr + s.mem_size).unwrap(), s.perms)).collect();
    let mut bounds: Vec<u64> = segs.iter().flat_map(|&(a, b, _)| [a, b]).collect();
    bounds.sort_unstable();
    bounds.dedup();
    let mut runs: Vec<PageRun> = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        let covering = segs.iter().filter(|&&(s, e, _)| s <= a && b <= e);
        let Some(perms) = covering.map(|&(_, _, p)| p).reduce(Perms::union) else { continue };
        match runs.last_mut() {
            Some(r) if r.start + r.len == a && r.perms == perms => r.len += b - a,
            _ => runs.push(PageRun { start: a, len: b - a, perms }),
        }
    }
    runs
}

/// Auxiliary vector entry types.
pub mod at {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const PLATFORM: u64 = 15;
    pub const HWCAP: u64 = 16;
    pub const CLKTCK: u64 = 17;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const HWCAP2: u64 = 26;
    pub const EXECFN: u64 = 31;
}

/// What the auxiliary vector tells a starting program.
#[derive(Debug, Clone, Copy)]
pub struct AuxInfo<'a> {
    /// Address of the program headers in memory (load bias applied).
    pub phdr: u64,
    pub phnum: u64,
    /// The entry point (load bias applied).
    pub entry: u64,
    /// The CPU's features (`CPUID.1:EDX`).
    pub hwcap: u64,
    pub uid: u32,
    pub gid: u32,
    /// 16 random bytes (stack protector and pointer guard seeds).
    pub random: [u8; 16],
    /// The path the program was started from.
    pub execfn: &'a [u8],
}

/// The top of a new process's stack, as the System V ABI lays it out.
#[derive(Debug, Clone)]
pub struct StackImage {
    /// The bytes ending at the top of the stack.
    pub bytes: Vec<u8>,
    /// The initial stack pointer (16-byte aligned, pointing at `argc`).
    pub sp: u64,
}

/// Builds the initial stack of a program whose stack ends at `top`:
///
/// ```text
/// sp -> argc, argv[0..argc], NULL, envp[..], NULL, auxv pairs, AT_NULL
///       padding, argument and environment strings, "x86_64", the
///       program path, 16 random bytes                       <- top
/// ```
///
/// Strings must not contain NUL bytes (they would end early). Fails with
/// [`ElfError::TooLarge`] if more than `max` bytes would be needed.
pub fn initial_stack(
    top: u64,
    args: &[&[u8]],
    env: &[&[u8]],
    aux: &AuxInfo,
    max: usize,
) -> Result<StackImage, ElfError> {
    const PLATFORM: &[u8] = b"x86_64";
    // Strings from the top down: random bytes, platform, path, environment
    // and arguments; their addresses are fixed once the total is known.
    let strings_len =
        16 + PLATFORM.len() + 1 + aux.execfn.len() + 1 + args.iter().chain(env).map(|s| s.len() + 1).sum::<usize>();
    let auxv: [(u64, u64); 18] = [
        (at::PHDR, aux.phdr),
        (at::PHENT, PHDR_SIZE as u64),
        (at::PHNUM, aux.phnum),
        (at::PAGESZ, PAGE_SIZE),
        (at::BASE, 0),
        (at::FLAGS, 0),
        (at::ENTRY, aux.entry),
        (at::UID, aux.uid as u64),
        (at::EUID, aux.uid as u64),
        (at::GID, aux.gid as u64),
        (at::EGID, aux.gid as u64),
        (at::SECURE, 0),
        (at::CLKTCK, 100),
        (at::HWCAP, aux.hwcap),
        (at::HWCAP2, 0),
        (at::RANDOM, 0),   // filled in below
        (at::PLATFORM, 0), // filled in below
        (at::EXECFN, 0),   // filled in below
    ];
    let words = 1 + args.len() + 1 + env.len() + 1 + 2 * (auxv.len() + 1);
    let strings_start = top.checked_sub(strings_len as u64).ok_or(ElfError::TooLarge)?;
    let sp = strings_start.checked_sub(8 * words as u64).ok_or(ElfError::TooLarge)? & !15;
    let total = (top - sp) as usize;
    if total > max {
        return Err(ElfError::TooLarge);
    }

    let mut bytes = alloc::vec![0u8; total];
    let at = |addr: u64| (addr - sp) as usize;
    // The strings.
    let mut cursor = top;
    let mut place = |bytes: &mut [u8], s: &[u8], nul: bool| -> u64 {
        cursor -= (s.len() + nul as usize) as u64;
        bytes[at(cursor)..at(cursor) + s.len()].copy_from_slice(s);
        cursor
    };
    let random = place(&mut bytes, &aux.random, false);
    let platform = place(&mut bytes, PLATFORM, true);
    let execfn = place(&mut bytes, aux.execfn, true);
    let env_addrs: Vec<u64> = env.iter().rev().map(|s| place(&mut bytes, s, true)).collect();
    let arg_addrs: Vec<u64> = args.iter().rev().map(|s| place(&mut bytes, s, true)).collect();
    debug_assert_eq!(cursor, strings_start);

    // The vectors.
    let mut word = sp;
    let mut push = |bytes: &mut [u8], v: u64| {
        bytes[at(word)..at(word) + 8].copy_from_slice(&v.to_le_bytes());
        word += 8;
    };
    push(&mut bytes, args.len() as u64);
    for &a in arg_addrs.iter().rev() {
        push(&mut bytes, a);
    }
    push(&mut bytes, 0);
    for &e in env_addrs.iter().rev() {
        push(&mut bytes, e);
    }
    push(&mut bytes, 0);
    for (kind, value) in auxv {
        let value = match kind {
            at::RANDOM => random,
            at::PLATFORM => platform,
            at::EXECFN => execfn,
            _ => value,
        };
        push(&mut bytes, kind);
        push(&mut bytes, value);
    }
    push(&mut bytes, at::NULL);
    push(&mut bytes, 0);
    Ok(StackImage { bytes, sp })
}

#[cfg(test)]
mod tests;
