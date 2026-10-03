//! Global descriptor table and task state segment (one of each per CPU).
//!
//! The segment order is dictated by `syscall`/`sysret`:
//! `STAR[47:32] = KERNEL_CS` (kernel SS = +8) and `STAR[63:48] = 0x10`
//! (user SS = 0x18, user CS = 0x20).

use core::arch::asm;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
pub const USER_DS: u16 = 0x18 | 3;
pub const USER_CS: u16 = 0x20 | 3;
pub const TSS_SEL: u16 = 0x28;

/// IST slots (1-based in the IDT).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;

#[repr(C, packed(4))]
pub struct Tss {
    _reserved0: u32,
    /// Stack pointers for privilege levels 0-2.
    pub rsp: [u64; 3],
    _reserved1: u64,
    /// Interrupt stack table.
    pub ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    pub const fn new() -> Self {
        Tss {
            _reserved0: 0,
            rsp: [0; 3],
            _reserved1: 0,
            ist: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            // No I/O permission bitmap: all ports are denied in user mode.
            iomap_base: core::mem::size_of::<Tss>() as u16,
        }
    }
}

#[repr(C, align(16))]
pub struct Gdt {
    entries: [u64; 7],
}

impl Gdt {
    pub const fn new() -> Self {
        Gdt {
            entries: [
                0,
                0x00AF_9A00_0000_FFFF, // kernel code: 64-bit, DPL 0
                0x00CF_9200_0000_FFFF, // kernel data
                0x00CF_F200_0000_FFFF, // user data, DPL 3
                0x00AF_FA00_0000_FFFF, // user code: 64-bit, DPL 3
                0,                     // TSS (low)
                0,                     // TSS (high)
            ],
        }
    }
}

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

/// Fills in the TSS descriptor and loads the GDT and TSS on this CPU.
///
/// # Safety
/// `gdt` and `tss` must live forever (they are per-CPU statics).
pub unsafe fn load(gdt: *mut Gdt, tss: *const Tss) {
    let base = tss as u64;
    let limit = (core::mem::size_of::<Tss>() - 1) as u64;
    let low = (limit & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (0x89 << 40) // present, 64-bit available TSS
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = base >> 32;
    // SAFETY: the caller guarantees `gdt` is valid and per-CPU.
    unsafe {
        (*gdt).entries[5] = low;
        (*gdt).entries[6] = high;
        let ptr = DescriptorPointer { limit: (core::mem::size_of::<Gdt>() - 1) as u16, base: gdt as u64 };
        asm!(
            "lgdt [{ptr}]",
            // Reload CS with a far return.
            "push {kcs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov {tmp:e}, {kds}",
            "mov ds, {tmp:e}",
            "mov es, {tmp:e}",
            "mov ss, {tmp:e}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:e}",
            // Loading GS clears the GS base; per-CPU data is installed after.
            "mov gs, {tmp:e}",
            "mov {tmp:e}, {tss}",
            "ltr {tmp:x}",
            ptr = in(reg) &ptr,
            kcs = const KERNEL_CS as u64,
            kds = const KERNEL_DS as u32,
            tss = const TSS_SEL as u32,
            tmp = out(reg) _,
        );
    }
}
