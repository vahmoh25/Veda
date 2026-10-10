//! The framebuffer's memory type, as the firmware leaves it.
//!
//! Firmware usually leaves its framebuffer uncached: the MTRRs mark the
//! graphics aperture uncached, as all memory-mapped I/O, and the firmware's
//! page tables say nothing else. Every write is then a bus transaction of
//! its own, and on a real PC a picture is seen being painted from the top
//! down (under QEMU the framebuffer is ordinary memory, and it is not). The
//! screen is cleared write-combining all the same (see `blank`); this
//! finds what the firmware gave, for the kernel's log.

use bootinfo::memory_type as mt;

use crate::read_msr;

const IA32_MTRRCAP: u32 = 0xFE;
const IA32_MTRR_DEF_TYPE: u32 = 0x2FF;
const IA32_MTRR_PHYSBASE0: u32 = 0x200;
pub const IA32_PAT: u32 = 0x277;
/// `IA32_MTRR_DEF_TYPE.E`: the MTRRs are on.
const MTRRS_ON: u64 = 1 << 11;
/// `IA32_MTRR_PHYSMASKn.V`: the pair is in use.
const MTRR_VALID: u64 = 1 << 11;
/// The PAT as the processor powers on (WB, WT, UC-, UC, twice).
const PAT_DEFAULT: u64 = 0x0007_0406_0007_0406;
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// CPUID's `eax` and `edx` for `leaf`.
fn cpuid(leaf: u32) -> (u32, u32) {
    let (a, d): (u32, u32);
    // SAFETY: CPUID is always available on x86-64. rbx is reserved by
    // LLVM, so it is saved manually.
    unsafe {
        core::arch::asm!("mov {tmp}, rbx", "cpuid", "mov rbx, {tmp}", tmp = out(reg) _,
            inout("eax") leaf => a, inout("ecx") 0 => _, out("edx") d, options(nostack));
    }
    (a, d)
}

fn has_mtrrs() -> bool {
    cpuid(1).1 & (1 << 12) != 0
}

/// Whether the processor has the page attribute table.
pub fn has_pat() -> bool {
    cpuid(1).1 & (1 << 16) != 0
}

/// Whether the firmware pages with five levels (`CR4.LA57`).
pub fn five_levels() -> bool {
    let cr4: u64;
    // SAFETY: reading a control register.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags)) };
    cr4 & (1 << 12) != 0
}

/// The bits of a physical address (`MAXPHYADDR`), as a mask.
fn physical_mask() -> u64 {
    let bits = if cpuid(0x8000_0000).0 >= 0x8000_0008 { cpuid(0x8000_0008).0 & 0xFF } else { 36 };
    (1u64 << bits.clamp(36, 52)) - 1
}

/// What the MTRRs make of the page at `address` (above the first MiB, where
/// the fixed ranges do not reach).
fn mtrr_type(address: u64) -> u8 {
    if !has_mtrrs() {
        return mt::UNKNOWN;
    }
    let default = read_msr(IA32_MTRR_DEF_TYPE);
    if default & MTRRS_ON == 0 {
        return mt::UNCACHED;
    }
    let count = (read_msr(IA32_MTRRCAP) & 0xFF) as u32;
    let physical = physical_mask() & !0xFFF;
    let mut found: Option<u8> = None;
    for i in 0..count {
        let base = read_msr(IA32_MTRR_PHYSBASE0 + 2 * i);
        let mask = read_msr(IA32_MTRR_PHYSBASE0 + 2 * i + 1);
        let m = mask & physical;
        if mask & MTRR_VALID == 0 || address & m != base & m {
            continue;
        }
        let t = (base & 0xFF) as u8;
        // Where several cover a page, uncached wins, and write-through
        // over write-back; anything else is undefined (taken as uncached).
        found = Some(match found {
            None => t,
            Some(f) if f == t => t,
            Some(f) if f == mt::UNCACHED || t == mt::UNCACHED => mt::UNCACHED,
            Some(f) if f.min(t) == mt::WRITE_THROUGH && f.max(t) == mt::WRITE_BACK => mt::WRITE_THROUGH,
            _ => mt::UNCACHED,
        });
    }
    found.unwrap_or((default & 0xFF) as u8)
}

/// What the firmware's page tables make of the page at `address` (its PAT
/// entry), the tables being identity-mapped.
fn pat_type(address: u64) -> u8 {
    let cr3: u64;
    // SAFETY: reading a control register.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    let levels = if five_levels() { 5 } else { 4 };
    let mut table = cr3 & ADDRESS_MASK;
    for level in (1..=levels).rev() {
        let slot = (address >> (12 + 9 * (level - 1))) & 0x1FF;
        // SAFETY: a present table's entry; the firmware maps its tables at
        // their physical addresses.
        let entry = unsafe { core::ptr::read_volatile((table + slot * 8) as *const u64) };
        if entry & 1 == 0 {
            return mt::UNKNOWN;
        }
        // A page: a level-1 entry, or a large one (2 MiB or 1 GiB).
        if level == 1 || (level <= 3 && entry & (1 << 7) != 0) {
            let pat = if level == 1 { (entry >> 7) & 1 } else { (entry >> 12) & 1 };
            let index = (pat << 2) | (((entry >> 4) & 1) << 1) | ((entry >> 3) & 1);
            let pats = if has_pat() { read_msr(IA32_PAT) } else { PAT_DEFAULT };
            return ((pats >> (index * 8)) & 0x7) as u8;
        }
        table = entry & ADDRESS_MASK;
    }
    mt::UNKNOWN
}

/// The memory type of the page at `address` in the firmware's address
/// space: its MTRRs' and its PAT entry's together, as the processor's
/// manual tabulates them.
pub fn memory_type(address: u64) -> u8 {
    let (mtrr, pat) = (mtrr_type(address), pat_type(address));
    match pat {
        mt::UNKNOWN | mt::WRITE_BACK => mtrr,
        mt::WRITE_COMBINING => mt::WRITE_COMBINING,
        mt::UNCACHED_MINUS if mtrr == mt::WRITE_COMBINING => mt::WRITE_COMBINING,
        mt::UNCACHED_MINUS | mt::UNCACHED => mt::UNCACHED,
        _ if matches!(mtrr, mt::UNCACHED | mt::WRITE_COMBINING) => mt::UNCACHED,
        _ => pat,
    }
}
