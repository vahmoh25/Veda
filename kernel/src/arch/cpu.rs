//! CPU identification, model-specific registers, control registers and
//! per-CPU feature initialisation.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const MSR_EFER: u32 = 0xC000_0080;
pub const MSR_STAR: u32 = 0xC000_0081;
pub const MSR_LSTAR: u32 = 0xC000_0082;
pub const MSR_SFMASK: u32 = 0xC000_0084;
pub const MSR_FS_BASE: u32 = 0xC000_0100;
pub const MSR_GS_BASE: u32 = 0xC000_0101;
pub const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
pub const MSR_APIC_BASE: u32 = 0x1B;
pub const MSR_PAT: u32 = 0x277;
pub const MSR_TSC_DEADLINE: u32 = 0x6E0;

#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reading MSRs is side-effect free for the MSRs this kernel uses.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// # Safety
/// Writing MSRs changes fundamental CPU behaviour.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: forwarded to the caller.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags))
    };
}

/// Result of a CPUID query.
#[derive(Debug, Clone, Copy)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    // SAFETY: CPUID is always available on x86-64.
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, subleaf) };
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

#[inline]
pub fn rdtsc() -> u64 {
    // SAFETY: RDTSC is always available on x86-64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// `pml4` must be a valid top-level page table that maps the running code.
#[inline]
pub unsafe fn write_cr3(pml4: u64) {
    // SAFETY: forwarded to the caller.
    unsafe { asm!("mov cr3, {}", in(reg) pml4, options(nostack, preserves_flags)) };
}

#[inline]
pub fn invlpg(addr: u64) {
    // SAFETY: invalidating a TLB entry is always safe.
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags)) };
}

/// Flushes all non-global TLB entries.
#[inline]
pub fn flush_tlb() {
    // SAFETY: reloading CR3 with its current value only flushes the TLB.
    unsafe { write_cr3(read_cr3()) };
}

#[inline]
pub fn halt() {
    // SAFETY: HLT waits for the next interrupt.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

/// Enables interrupts and halts atomically (no wake-up can be lost between
/// the two instructions), then disables interrupts again.
#[inline]
pub fn enable_interrupts_and_halt() {
    // SAFETY: STI's one-instruction interrupt shadow makes STI;HLT atomic.
    unsafe { asm!("sti", "hlt", "cli", options(nomem, nostack)) };
}

#[inline]
pub fn disable_interrupts() {
    // SAFETY: masking interrupts is always safe.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

#[inline]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: reading RFLAGS has no side effects.
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

/// Halts this CPU forever.
pub fn halt_forever() -> ! {
    loop {
        disable_interrupts();
        halt();
    }
}

/// CPU features detected on the bootstrap processor.
#[derive(Debug, Clone, Copy, Default)]
pub struct Features {
    pub xsave: bool,
    pub avx: bool,
    pub smep: bool,
    pub smap: bool,
    pub fsgsbase: bool,
    pub x2apic: bool,
    pub tsc_deadline: bool,
    pub invariant_tsc: bool,
    pub rdrand: bool,
    pub page_1g: bool,
    pub pat: bool,
    pub umip: bool,
    /// Size of the XSAVE area for the features we enable.
    pub xsave_size: u32,
    /// XCR0 value we program.
    pub xcr0: u64,
}

static FEATURES_READY: AtomicBool = AtomicBool::new(false);
static mut FEATURES: Features = Features {
    xsave: false,
    avx: false,
    smep: false,
    smap: false,
    fsgsbase: false,
    x2apic: false,
    tsc_deadline: false,
    invariant_tsc: false,
    rdrand: false,
    page_1g: false,
    pat: false,
    umip: false,
    xsave_size: 512,
    xcr0: 0,
};

/// Returns the features detected by [`detect_features`].
pub fn features() -> Features {
    debug_assert!(FEATURES_READY.load(Ordering::Acquire));
    // SAFETY: written once during early boot before any reader exists.
    unsafe { *core::ptr::addr_of!(FEATURES) }
}

/// Probes CPUID once on the bootstrap processor.
pub fn detect_features() -> Features {
    let l1 = cpuid(1, 0);
    let l7 = cpuid(7, 0);
    let ext = cpuid(0x8000_0001, 0);
    let max_ext = cpuid(0x8000_0000, 0).eax;
    let mut f = Features {
        xsave: l1.ecx & (1 << 26) != 0,
        avx: l1.ecx & (1 << 28) != 0,
        smep: l7.ebx & (1 << 7) != 0,
        smap: l7.ebx & (1 << 20) != 0,
        fsgsbase: l7.ebx & (1 << 0) != 0,
        x2apic: l1.ecx & (1 << 21) != 0,
        tsc_deadline: l1.ecx & (1 << 24) != 0,
        invariant_tsc: max_ext >= 0x8000_0007 && cpuid(0x8000_0007, 0).edx & (1 << 8) != 0,
        rdrand: l1.ecx & (1 << 30) != 0,
        page_1g: ext.edx & (1 << 26) != 0,
        pat: l1.edx & (1 << 16) != 0,
        umip: l7.ecx & (1 << 2) != 0,
        xsave_size: 512,
        xcr0: 0,
    };
    if f.xsave {
        // x87 | SSE, plus AVX state when available.
        let supported = {
            let r = cpuid(0xD, 0);
            (r.edx as u64) << 32 | r.eax as u64
        };
        f.xcr0 = 0b11 | if f.avx && supported & 0b100 != 0 { 0b100 } else { 0 };
        // ECX: size needed for every supported component (an upper bound
        // for whatever subset we enable).
        f.xsave_size = cpuid(0xD, 0).ecx.max(576);
    }
    // SAFETY: single writer during early boot.
    unsafe { *core::ptr::addr_of_mut!(FEATURES) = f };
    FEATURES_READY.store(true, Ordering::Release);
    f
}

/// Configures control registers and MSRs on the current CPU: SSE/XSAVE for
/// user-space floating point, NX, SMEP/SMAP, write-combining PAT entry and
/// the global-page bit.
pub fn init_cpu_features() {
    let f = features();
    // SAFETY: only enables features that CPUID reported as present.
    unsafe {
        // CR0: monitor coprocessor, native FPU errors, write protect; clear
        // emulation and task-switched bits.
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0);
        cr0 |= (1 << 1) | (1 << 5) | (1 << 16);
        cr0 &= !((1 << 2) | (1 << 3));
        asm!("mov cr0, {}", in(reg) cr0);

        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4);
        cr4 |= (1 << 9) | (1 << 10) | (1 << 7); // OSFXSR, OSXMMEXCPT, PGE
        if f.xsave {
            cr4 |= 1 << 18; // OSXSAVE
        }
        if f.smep {
            cr4 |= 1 << 20;
        }
        if f.smap {
            cr4 |= 1 << 21;
        }
        if f.fsgsbase {
            cr4 |= 1 << 16;
        }
        if f.umip {
            cr4 |= 1 << 11;
        }
        asm!("mov cr4, {}", in(reg) cr4);

        if f.xsave {
            asm!("xsetbv", in("ecx") 0, in("eax") f.xcr0 as u32, in("edx") (f.xcr0 >> 32) as u32);
        }

        // EFER: NXE and SCE (syscall enable).
        wrmsr(MSR_EFER, rdmsr(MSR_EFER) | (1 << 11) | (1 << 0));

        if f.pat {
            // PAT0=WB PAT1=WC PAT2=UC- PAT3=UC PAT4=WB PAT5=WP PAT6=UC- PAT7=WT
            // (PTE flags: WC = PWT, UC = PCD|PWT).
            wrmsr(MSR_PAT, 0x0407_0005_0007_0106);
        }
        asm!("fninit");
    }
}

/// Reads the CPU brand string ("Intel(R) Core(TM) ...").
pub fn brand_string() -> [u8; 48] {
    let mut out = [0u8; 48];
    if cpuid(0x8000_0000, 0).eax < 0x8000_0004 {
        out[..7].copy_from_slice(b"Unknown");
        return out;
    }
    for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        let r = cpuid(leaf, 0);
        for (j, reg) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
            out[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&reg.to_le_bytes());
        }
    }
    // Trim leading spaces.
    let start = out.iter().position(|&b| b != b' ').unwrap_or(0);
    out.copy_within(start.., 0);
    out[48 - start..].fill(0);
    out
}

/// Number of CPUs brought online.
pub static ONLINE_CPUS: AtomicU32 = AtomicU32::new(1);

/// Temporarily allows supervisor access to user pages (SMAP).
#[inline]
pub fn user_access_begin() {
    if features().smap {
        // SAFETY: STAC only toggles RFLAGS.AC.
        unsafe { asm!("stac", options(nomem, nostack)) };
    }
}

/// Ends a [`user_access_begin`] section.
#[inline]
pub fn user_access_end() {
    if features().smap {
        // SAFETY: CLAC only toggles RFLAGS.AC.
        unsafe { asm!("clac", options(nomem, nostack)) };
    }
}

/// Fills `buf` with hardware random numbers (RDRAND) mixed with the TSC.
pub fn hardware_random(buf: &mut [u8]) {
    let mut seed = rdtsc() ^ 0x9E37_79B9_7F4A_7C15;
    for chunk in buf.chunks_mut(8) {
        let mut v: u64 = 0;
        if features().rdrand {
            let mut ok: u8;
            for _ in 0..10 {
                // SAFETY: RDRAND is supported (checked above).
                unsafe { asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)) };
                if ok != 0 {
                    break;
                }
            }
        }
        // splitmix64 over TSC so the output is unpredictable even without
        // RDRAND.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15 ^ rdtsc());
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let bytes = (v ^ z).to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}
