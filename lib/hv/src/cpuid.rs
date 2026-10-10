//! What `cpuid` tells a guest.
//!
//! The answers start from the processor's own and keep only what a guest
//! can use under Veda's hypervisor: no virtualization of its own (VMX), no
//! MONITOR/MWAIT, performance counters, machine checks, power and thermal
//! controls, speculation controls, debug stores, the compacted XSAVE
//! formats (XSAVEC, XSAVES) or protection keys. Every processor has an x2APIC with a TSC-deadline
//! timer and an invariant TSC, and says it runs under a hypervisor, whose
//! leaves ([`crate::platform`]) describe the platform. The topology is
//! flat: one package with one thread per core, as many cores as the guest
//! has processors, local APIC ids from 0.
//!
//! Leaves past the last one this module knows read as zeros, as a
//! processor answers leaves beyond its highest.

use crate::platform;

/// What the answers depend on beyond the processor's own.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// The local APIC id of the processor asking.
    pub apic_id: u32,
    /// How many processors the guest has.
    pub cpus: u32,
    /// The TSC's frequency, in kHz.
    pub tsc_khz: u32,
    /// The guest set `CR4.OSXSAVE`.
    pub osxsave: bool,
    /// The extended state components the guest may enable (`XCR0`): the
    /// host's.
    pub xcr0: u64,
}

/// The highest basic leaf a guest sees.
const MAX_BASIC: u32 = 0xD;
/// The highest extended leaf a guest sees.
const MAX_EXTENDED: u32 = 0x8000_0008;

/// Leaf 1, ECX: SSE3, PCLMULQDQ, SSSE3, FMA, CMPXCHG16B, PCID, SSE4.1,
/// SSE4.2, MOVBE, POPCNT, AES, XSAVE, AVX, F16C, RDRAND.
const LEAF1_ECX: u32 = (1 << 0)
    | (1 << 1)
    | (1 << 9)
    | (1 << 12)
    | (1 << 13)
    | (1 << 17)
    | (1 << 19)
    | (1 << 20)
    | (1 << 22)
    | (1 << 23)
    | (1 << 25)
    | (1 << 26)
    | (1 << 28)
    | (1 << 29)
    | (1 << 30);
const X2APIC: u32 = 1 << 21;
const TSC_DEADLINE: u32 = 1 << 24;
const OSXSAVE: u32 = 1 << 27;
const HYPERVISOR: u32 = 1 << 31;
/// Leaf 1, EDX: FPU, VME, DE, PSE, TSC, MSR, PAE, CX8, APIC, SEP, MTRR,
/// PGE, CMOV, PAT, PSE36, CLFSH, MMX, FXSR, SSE, SSE2, HTT. Not MCE, MCA,
/// PSN, DS, ACPI, SS, TM or PBE.
const LEAF1_EDX: u32 = (1 << 0)
    | (1 << 1)
    | (1 << 2)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 8)
    | (1 << 9)
    | (1 << 11)
    | (1 << 12)
    | (1 << 13)
    | (1 << 15)
    | (1 << 16)
    | (1 << 17)
    | (1 << 19)
    | (1 << 23)
    | (1 << 24)
    | (1 << 25)
    | (1 << 26)
    | (1 << 28);
/// Leaf 7, EBX: FSGSBASE, BMI1, AVX2, SMEP, BMI2, ERMS, INVPCID, RDSEED,
/// ADX, SMAP, CLFLUSHOPT, CLWB, SHA. Not TSC_ADJUST, SGX, TSX, the PQ
/// monitoring and enforcement, MPX, AVX-512 or processor trace.
const LEAF7_EBX: u32 = (1 << 0)
    | (1 << 3)
    | (1 << 5)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 10)
    | (1 << 18)
    | (1 << 19)
    | (1 << 20)
    | (1 << 23)
    | (1 << 24)
    | (1 << 29);
/// Leaf 7, ECX: UMIP, GFNI, VAES, VPCLMULQDQ, RDPID, MOVDIRI, MOVDIR64B.
/// Not protection keys, WAITPKG or 5-level paging (guests page in four
/// levels, which the processors' start through the platform assumes).
const LEAF7_ECX: u32 = (1 << 2) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 22) | (1 << 27) | (1 << 28);
/// Leaf 7, EDX: fast short REP MOV, SERIALIZE. Not the speculation
/// controls, the hybrid flag or the architectural capabilities MSR.
const LEAF7_EDX: u32 = (1 << 4) | (1 << 14);
/// Leaf 0xD sub-leaf 1, EAX: XSAVEOPT, XGETBV with ECX = 1. Not XSAVEC
/// (whose compacted size follows XCR0 as the guest sets it) or XSAVES.
const LEAF_D1_EAX: u32 = 0b101;
/// Leaf 0x8000_0001, ECX: LAHF in 64-bit mode, LZCNT, PREFETCHW.
const EXT1_ECX: u32 = (1 << 0) | (1 << 5) | (1 << 8);
/// Leaf 0x8000_0001, EDX: SYSCALL, NX, 1 GiB pages, RDTSCP, long mode.
const EXT1_EDX: u32 = (1 << 11) | (1 << 20) | (1 << 26) | (1 << 27) | (1 << 29);
/// Leaf 0x8000_0007, EDX: the invariant TSC.
const INVARIANT_TSC: u32 = 1 << 8;

/// The answer to `cpuid` with `leaf` and `subleaf` (`eax`, `ebx`, `ecx`,
/// `edx`), from the processor's own (`host`).
pub fn guest(leaf: u32, subleaf: u32, ctx: &Context, host: impl Fn(u32, u32) -> [u32; 4]) -> [u32; 4] {
    match leaf {
        0 => {
            let [max, b, c, d] = host(0, 0);
            [max.min(MAX_BASIC), b, c, d]
        }
        1 if host(0, 0)[0] >= 1 => {
            let [a, b, c, d] = host(1, 0);
            // Initial APIC id, logical processors, CLFLUSH line size.
            let b = (ctx.apic_id.min(0xFF) << 24) | (ctx.cpus.min(0xFF) << 16) | (b & 0xFFFF);
            let mut c = (c & LEAF1_ECX) | X2APIC | TSC_DEADLINE | HYPERVISOR;
            if ctx.osxsave && c & (1 << 26) != 0 {
                c |= OSXSAVE;
            }
            [a, b, c, d & LEAF1_EDX]
        }
        // Cache descriptors (leaf 2) and parameters (leaf 4), with the
        // topology made flat.
        2 if host(0, 0)[0] >= 2 => host(2, 0),
        4 if host(0, 0)[0] >= 4 => {
            let [a, b, c, d] = host(4, subleaf);
            if a & 0x1F == 0 {
                return [0; 4];
            }
            let a = (a & 0x3FFF) | ((ctx.cpus.max(1) - 1).min(0x3F) << 26);
            [a, b, c, d]
        }
        7 if subleaf == 0 && host(0, 0)[0] >= 7 => {
            let [_, b, c, d] = host(7, 0);
            [0, b & LEAF7_EBX, c & LEAF7_ECX, d & LEAF7_EDX]
        }
        0xB => topology(subleaf, ctx),
        0xD if host(0, 0)[0] >= 0xD => xsave(subleaf, ctx, &host),
        platform::CPUID_SIGNATURE => {
            let [b, c, d] = platform::signature_registers();
            [platform::CPUID_MAX, b, c, d]
        }
        platform::CPUID_PLATFORM => [0, ctx.cpus, 0, 0],
        platform::CPUID_TIMING => [ctx.tsc_khz, ctx.tsc_khz, 0, 0],
        0x8000_0000 => {
            let [max, b, c, d] = host(0x8000_0000, 0);
            [max.min(MAX_EXTENDED), b, c, d]
        }
        0x8000_0001..=MAX_EXTENDED if host(0x8000_0000, 0)[0] >= leaf => {
            let [a, b, c, d] = host(leaf, 0);
            match leaf {
                0x8000_0001 => [a, 0, c & EXT1_ECX, d & EXT1_EDX],
                // The brand string and the caches.
                0x8000_0002..=0x8000_0006 => [a, b, c, d],
                0x8000_0007 => [0, 0, 0, d & INVARIANT_TSC | INVARIANT_TSC],
                // Address sizes only.
                0x8000_0008 => [a & 0xFFFF, 0, 0, 0],
                _ => [0; 4],
            }
        }
        _ => [0; 4],
    }
}

/// Leaf 0xB: one thread per core, the guest's processors as cores.
fn topology(subleaf: u32, ctx: &Context) -> [u32; 4] {
    let id = ctx.apic_id;
    match subleaf {
        // Threads: one per core.
        0 => [0, 1, 1 << 8, id],
        // Cores: the bits of the id that number them.
        1 => {
            let cpus = ctx.cpus.max(1);
            let shift = 32 - (cpus - 1).leading_zeros();
            [shift, cpus, 1 | (2 << 8), id]
        }
        n => [0, 0, n & 0xFF, id],
    }
}

/// Leaf 0xD: the extended states of [`Context::xcr0`].
fn xsave(subleaf: u32, ctx: &Context, host: &impl Fn(u32, u32) -> [u32; 4]) -> [u32; 4] {
    match subleaf {
        0 => {
            let [_, b, c, _] = host(0xD, 0);
            [ctx.xcr0 as u32, b, c, (ctx.xcr0 >> 32) as u32]
        }
        1 => [host(0xD, 1)[0] & LEAF_D1_EAX, 0, 0, 0],
        n if n < 64 && ctx.xcr0 & (1 << n) != 0 => host(0xD, n),
        _ => [0; 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Alder Lake's answers, as far as the tests go.
    fn host(leaf: u32, sub: u32) -> [u32; 4] {
        match (leaf, sub) {
            (0, _) => [0x20, 0x756E_6547, 0x6C65_746E, 0x4965_6E69],
            (1, _) => [0x906A3, 0x0080_0800, 0x7FFA_FBFF, 0xBFEB_FBFF],
            (4, 0) => [0xFC00_4121, 0x02C0_003F, 0x3F, 0],
            (4, 4) => [0, 0, 0, 0],
            (7, 0) => [2, 0x239C_A7EB, 0x9840_07BC, 0xFC1C_4410],
            (0xD, 0) => [0x207, 0x340, 0xA88, 0],
            (0xD, 1) => [0xF, 0, 0x1900, 0],
            (0xD, 2) => [0x100, 0x240, 0, 0],
            (0x8000_0000, _) => [0x8000_0008, 0, 0, 0],
            (0x8000_0001, _) => [0, 0, 0x121, 0x2C10_0800],
            (0x8000_0007, _) => [0, 0, 0, 0x100],
            (0x8000_0008, _) => [0x3027, 0, 0, 0],
            _ => [0; 4],
        }
    }

    fn ctx() -> Context {
        Context { apic_id: 1, cpus: 2, tsc_khz: 2_918_400, osxsave: true, xcr0: 0b111 }
    }

    #[test]
    fn basic_leaves_stop_at_xsave() {
        assert_eq!(guest(0, 0, &ctx(), host)[0], 0xD);
        assert_eq!(guest(0x15, 0, &ctx(), host), [0; 4]);
        assert_eq!(guest(6, 0, &ctx(), host), [0; 4]);
        assert_eq!(guest(0xA, 0, &ctx(), host), [0; 4]);
    }

    #[test]
    fn leaf_1() {
        let [a, b, c, d] = guest(1, 0, &ctx(), host);
        assert_eq!(a, 0x906A3);
        assert_eq!(b, 0x0102_0800);
        // No VMX, MONITOR or performance capabilities; an x2APIC, the
        // deadline timer and a hypervisor.
        assert_eq!(c & (1 << 5), 0);
        assert_eq!(c & (1 << 3), 0);
        assert_eq!(c & (1 << 15), 0);
        assert_ne!(c & X2APIC, 0);
        assert_ne!(c & TSC_DEADLINE, 0);
        assert_ne!(c & HYPERVISOR, 0);
        assert_ne!(c & OSXSAVE, 0);
        // No machine checks.
        assert_eq!(d & ((1 << 7) | (1 << 14)), 0);
        let quiet = Context { osxsave: false, ..ctx() };
        assert_eq!(guest(1, 0, &quiet, host)[2] & OSXSAVE, 0);
    }

    #[test]
    fn leaf_7_hides_what_guests_cannot_use() {
        let [_, b, c, d] = guest(7, 0, &ctx(), host);
        // No TSC_ADJUST, SGX, PT; no WAITPKG, PKU, LA57; no speculation
        // controls or hybrid flag.
        assert_eq!(b & ((1 << 1) | (1 << 2) | (1 << 25)), 0);
        assert_eq!(c & ((1 << 3) | (1 << 5) | (1 << 16)), 0);
        assert_eq!(d & ((1 << 15) | (1 << 26) | (1 << 29)), 0);
        assert_ne!(b & (1 << 5), 0);
        assert_eq!(guest(7, 1, &ctx(), host), [0; 4]);
    }

    #[test]
    fn topology_is_flat() {
        assert_eq!(guest(0xB, 0, &ctx(), host), [0, 1, 0x100, 1]);
        assert_eq!(guest(0xB, 1, &ctx(), host), [1, 2, 0x201, 1]);
        assert_eq!(guest(0xB, 2, &ctx(), host), [0, 0, 2, 1]);
        let four = Context { cpus: 4, ..ctx() };
        assert_eq!(guest(0xB, 1, &four, host)[0], 2);
        let one = Context { cpus: 1, ..ctx() };
        assert_eq!(guest(0xB, 1, &one, host)[0], 0);
        // Caches shared by as many cores as the guest has.
        assert_eq!(guest(4, 0, &ctx(), host)[0] >> 26, 1);
        assert_eq!(guest(4, 4, &ctx(), host), [0; 4]);
    }

    #[test]
    fn xsave_states() {
        assert_eq!(guest(0xD, 0, &ctx(), host), [0b111, 0x340, 0xA88, 0]);
        // No XSAVEC or XSAVES.
        assert_eq!(guest(0xD, 1, &ctx(), host), [0b101, 0, 0, 0]);
        assert_eq!(guest(0xD, 2, &ctx(), host), [0x100, 0x240, 0, 0]);
        assert_eq!(guest(0xD, 9, &ctx(), host), [0; 4]);
    }

    #[test]
    fn platform_leaves() {
        let [max, b, c, d] = guest(platform::CPUID_SIGNATURE, 0, &ctx(), host);
        assert_eq!(max, platform::CPUID_TIMING);
        assert_eq!([b, c, d], platform::signature_registers());
        assert_eq!(guest(platform::CPUID_PLATFORM, 0, &ctx(), host), [0, 2, 0, 0]);
        assert_eq!(guest(platform::CPUID_TIMING, 0, &ctx(), host), [2_918_400, 2_918_400, 0, 0]);
    }

    #[test]
    fn extended_leaves() {
        assert_eq!(guest(0x8000_0000, 0, &ctx(), host)[0], 0x8000_0008);
        let [_, _, c, d] = guest(0x8000_0001, 0, &ctx(), host);
        assert_eq!(c, 0x121);
        assert_eq!(d, 0x2C10_0800 & EXT1_EDX);
        assert_eq!(guest(0x8000_0007, 0, &ctx(), host), [0, 0, 0, INVARIANT_TSC]);
        assert_eq!(guest(0x8000_0008, 0, &ctx(), host), [0x3027, 0, 0, 0]);
        assert_eq!(guest(0x8000_0009, 0, &ctx(), host), [0; 4]);
    }
}
