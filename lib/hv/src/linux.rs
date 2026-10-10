//! Booting Linux on the platform: the x86 boot protocol's 64-bit entry.
//!
//! A `bzImage` holds a setup header (protocol 2.12 or later, with a 64-bit
//! entry) and the protected-mode kernel, which is loaded as it is at a
//! guest-physical address and entered 0x200 bytes in, in 64-bit mode with
//! interrupts off, on page tables that map it (and the boot parameters and
//! command line) as they are, a GDT with flat code at 0x10 and data at
//! 0x18, and the boot parameters (the "zero page") in `rsi`. See Linux's
//! `Documentation/arch/x86/boot.rst` and `zero-page.rst`.

use vabi::{VcpuSegment, VcpuState};

/// Where the setup header starts in the image and in the boot parameters.
const HEADER: usize = 0x1F1;
/// The protocol version that brought the 64-bit entry.
const MIN_VERSION: u16 = 0x020C;
/// `xloadflags`: the kernel has the 64-bit entry.
const XLF_KERNEL_64: u16 = 1 << 0;
/// The 64-bit entry, from the start of the protected-mode kernel.
const ENTRY_64: u64 = 0x200;

/// What is wrong with a kernel image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootError {
    /// Not a `bzImage` (no `HdrS` header).
    NotBzImage,
    /// A boot protocol older than 2.12.
    TooOld(u16),
    /// No 64-bit entry point.
    No64BitEntry,
    /// The header says the kernel is longer than the file.
    Truncated,
}

impl core::fmt::Display for BootError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BootError::NotBzImage => f.write_str("not a bzImage"),
            BootError::TooOld(v) => write!(f, "boot protocol {}.{} (2.12 or later needed)", v >> 8, v & 0xFF),
            BootError::No64BitEntry => f.write_str("no 64-bit entry point"),
            BootError::Truncated => f.write_str("the image is truncated"),
        }
    }
}

/// A Linux kernel image.
#[derive(Debug, Clone, Copy)]
pub struct Kernel<'a> {
    /// The setup header, as the boot parameters take it.
    header: &'a [u8],
    /// The protected-mode kernel, loaded as it is.
    pub protected: &'a [u8],
    /// Where the kernel wants to be loaded (it relocates itself otherwise).
    pub preferred_address: u64,
    /// How much memory it needs from where it is loaded, to unpack itself.
    pub init_size: u64,
    /// The longest command line it takes, without the NUL.
    pub cmdline_size: u32,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u32_at(b, at) as u64 | (u32_at(b, at + 4) as u64) << 32
}

/// Reads a `bzImage`.
pub fn parse(image: &[u8]) -> Result<Kernel<'_>, BootError> {
    if image.len() < 0x268 || &image[0x202..0x206] != b"HdrS" {
        return Err(BootError::NotBzImage);
    }
    let version = u16_at(image, 0x206);
    if version < MIN_VERSION {
        return Err(BootError::TooOld(version));
    }
    if u16_at(image, 0x236) & XLF_KERNEL_64 == 0 {
        return Err(BootError::No64BitEntry);
    }
    let setup_sects = match image[0x1F1] {
        0 => 4,
        n => n as usize,
    };
    let protected_at = (setup_sects + 1) * 512;
    let header_end = 0x202 + image[0x201] as usize;
    if protected_at > image.len() || header_end > image.len() || header_end > protected_at {
        return Err(BootError::Truncated);
    }
    Ok(Kernel {
        header: &image[HEADER..header_end],
        protected: &image[protected_at..],
        preferred_address: u64_at(image, 0x258),
        init_size: u32_at(image, 0x260) as u64,
        cmdline_size: u32_at(image, 0x238),
    })
}

/// Kinds of memory in the guest's memory map.
pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;

/// A range of the guest's memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRange {
    pub start: u64,
    pub len: u64,
    pub kind: u32,
}

/// Where everything is for a boot.
#[derive(Debug, Clone, Copy)]
pub struct Boot<'a> {
    pub kernel: &'a Kernel<'a>,
    /// Where the protected-mode kernel is loaded.
    pub kernel_at: u64,
    /// The command line, NUL-terminated.
    pub cmdline_at: u64,
    /// The initial RAM file system: where, and how long.
    pub initrd: Option<(u64, u64)>,
    pub memory: &'a [MemoryRange],
}

/// The boot parameters (the zero page) for `boot`.
pub fn boot_params(boot: &Boot) -> [u8; 4096] {
    let mut p = [0u8; 4096];
    p[HEADER..HEADER + boot.kernel.header.len()].copy_from_slice(boot.kernel.header);
    let put32 = |p: &mut [u8; 4096], at: usize, v: u32| p[at..at + 4].copy_from_slice(&v.to_le_bytes());
    // The loader is of no registered kind.
    p[0x210] = 0xFF;
    put32(&mut p, 0x214, boot.kernel_at as u32);
    put32(&mut p, 0x228, boot.cmdline_at as u32);
    put32(&mut p, 0x0C8, (boot.cmdline_at >> 32) as u32);
    if let Some((at, len)) = boot.initrd {
        put32(&mut p, 0x218, at as u32);
        put32(&mut p, 0x21C, len as u32);
        put32(&mut p, 0x0C0, (at >> 32) as u32);
        put32(&mut p, 0x0C4, (len >> 32) as u32);
    }
    let ranges = &boot.memory[..boot.memory.len().min(128)];
    p[0x1E8] = ranges.len() as u8;
    for (i, r) in ranges.iter().enumerate() {
        let at = 0x2D0 + i * 20;
        p[at..at + 8].copy_from_slice(&r.start.to_le_bytes());
        p[at + 8..at + 16].copy_from_slice(&r.len.to_le_bytes());
        put32(&mut p, at + 16, r.kind);
    }
    p
}

/// The GDT a kernel is entered with: flat 64-bit code at 0x10, data at
/// 0x18.
pub const GDT: [u64; 4] = [0, 0, 0x00AF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF];
pub const CODE_SELECTOR: u16 = 0x10;
pub const DATA_SELECTOR: u16 = 0x18;

/// The page tables that map the first 4 GiB as they are, with 2 MiB
/// pages: six tables (PML4, PDPT, four page directories) that go at
/// `at`, one after the other; the PML4 first.
pub fn identity_page_tables(at: u64) -> [[u64; 512]; 6] {
    const PRESENT_WRITABLE: u64 = 0b11;
    const HUGE: u64 = 1 << 7;
    let mut t = [[0u64; 512]; 6];
    t[0][0] = (at + 0x1000) | PRESENT_WRITABLE;
    for i in 0..4 {
        t[1][i] = (at + 0x2000 + i as u64 * 0x1000) | PRESENT_WRITABLE;
        for (j, e) in t[2 + i].iter_mut().enumerate() {
            *e = (((i as u64) << 30) + ((j as u64) << 21)) | PRESENT_WRITABLE | HUGE;
        }
    }
    t
}

/// A processor in 64-bit mode at `rip`, on the page tables at `cr3` and
/// the [`GDT`] at `gdt`, with interrupts off and no-execute pages on.
pub fn long_mode(rip: u64, cr3: u64, gdt: u64) -> VcpuState {
    let flat = |selector, access| VcpuSegment { base: 0, limit: 0xFFFF_FFFF, access, selector, _reserved: [0; 3] };
    let data = flat(DATA_SELECTOR, 0xC093);
    VcpuState {
        rip,
        rflags: 0x2,
        // PE, MP, ET, NE, WP, PG.
        cr0: 0x8005_0033,
        cr3,
        // PAE.
        cr4: 0x20,
        // LME, LMA, NXE: the page tables a processor started later switches
        // to on its own use the NX bit.
        efer: 0xD00,
        cs: flat(CODE_SELECTOR, 0xA09B),
        ds: data,
        es: data,
        fs: data,
        gs: data,
        ss: data,
        // A busy 64-bit TSS; no LDT.
        tr: VcpuSegment { base: 0, limit: 0xFFFF, access: 0x8B, selector: 0, _reserved: [0; 3] },
        ldtr: VcpuSegment { access: 1 << 16, ..VcpuSegment::default() },
        gdt_base: gdt,
        gdt_limit: (GDT.len() * 8 - 1) as u32,
        ..VcpuState::default()
    }
}

/// The 64-bit entry of a kernel loaded at `kernel_at`.
pub fn entry(kernel_at: u64) -> u64 {
    kernel_at + ENTRY_64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// A bzImage's first sectors, as `arch/x86/boot/header.S` makes them,
    /// with two setup sectors.
    fn image() -> Vec<u8> {
        let mut b = vec![0u8; 3 * 512 + 100];
        b[0x1F1] = 2;
        b[0x201] = 0x66;
        b[0x202..0x206].copy_from_slice(b"HdrS");
        b[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());
        b[0x236..0x238].copy_from_slice(&0x7Fu16.to_le_bytes());
        b[0x238..0x23C].copy_from_slice(&2047u32.to_le_bytes());
        b[0x258..0x260].copy_from_slice(&0x100_0000u64.to_le_bytes());
        b[0x260..0x264].copy_from_slice(&0x200_0000u32.to_le_bytes());
        b[3 * 512] = 0xE9;
        b
    }

    #[test]
    fn parses_a_bzimage() {
        let img = image();
        let k = parse(&img).unwrap();
        assert_eq!(k.protected.len(), 100);
        assert_eq!(k.protected[0], 0xE9);
        assert_eq!(k.preferred_address, 0x100_0000);
        assert_eq!(k.init_size, 0x200_0000);
        assert_eq!(k.cmdline_size, 2047);
        assert_eq!(k.header.len(), 0x202 + 0x66 - 0x1F1);
        assert_eq!(entry(0x100_0000), 0x100_0200);
    }

    #[test]
    fn refuses_what_it_cannot_boot() {
        let mut img = image();
        img[0x206..0x208].copy_from_slice(&0x020Bu16.to_le_bytes());
        assert_eq!(parse(&img).unwrap_err(), BootError::TooOld(0x020B));
        let mut img = image();
        img[0x236] = 0;
        assert_eq!(parse(&img).unwrap_err(), BootError::No64BitEntry);
        let mut img = image();
        img[0x1F1] = 9;
        assert_eq!(parse(&img).unwrap_err(), BootError::Truncated);
        assert_eq!(parse(&[0; 100]).unwrap_err(), BootError::NotBzImage);
    }

    #[test]
    fn boot_parameters() {
        let img = image();
        let k = parse(&img).unwrap();
        let memory = [
            MemoryRange { start: 0, len: 0x1_0000, kind: E820_RESERVED },
            MemoryRange { start: 0x10_0000, len: 0x3FF0_0000, kind: E820_RAM },
        ];
        let boot = Boot {
            kernel: &k,
            kernel_at: 0x100_0000,
            cmdline_at: 0x9000,
            initrd: Some((0x3F00_0000, 0x12_3456)),
            memory: &memory,
        };
        let p = boot_params(&boot);
        assert_eq!(&p[0x202..0x206], b"HdrS");
        assert_eq!(p[0x210], 0xFF);
        assert_eq!(u32_at(&p, 0x228), 0x9000);
        assert_eq!(u32_at(&p, 0x218), 0x3F00_0000);
        assert_eq!(u32_at(&p, 0x21C), 0x12_3456);
        assert_eq!(p[0x1E8], 2);
        assert_eq!(u64_at(&p, 0x2D0 + 20), 0x10_0000);
        assert_eq!(u64_at(&p, 0x2D0 + 28), 0x3FF0_0000);
        assert_eq!(u32_at(&p, 0x2D0 + 36), E820_RAM);
    }

    #[test]
    fn identity_tables_map_4_gib() {
        let t = identity_page_tables(0x2000);
        assert_eq!(t[0][0], 0x3003);
        assert_eq!(t[1][3], 0x7003);
        assert_eq!(t[2][0], 0x83);
        assert_eq!(t[2][1], 0x20_0083);
        assert_eq!(t[5][511], 0xFFE0_0083);
    }

    #[test]
    fn long_mode_state() {
        let s = long_mode(0x100_0200, 0x2000, 0x1000);
        assert_eq!(s.cs.selector, CODE_SELECTOR);
        assert_eq!(s.cs.access & (1 << 13), 1 << 13);
        assert_eq!(s.gdt_limit, 31);
        assert_eq!(s.efer, 0xD00);
    }
}
