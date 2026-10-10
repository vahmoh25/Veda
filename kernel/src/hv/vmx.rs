//! Intel VMX: what the processor offers, its instructions, and the fields
//! of a virtual machine control structure (VMCS).
//!
//! VMX operation is entered on a processor the first time a virtual
//! processor runs there ([`enable_here`]) and never left. The capabilities
//! the hypervisor relies on are checked once ([`caps`]): EPT with
//! write-back paging structures and `invept`, the VMX-preemption timer,
//! controls that let `cr3` accesses run unintercepted, and MSR bitmaps.

use core::arch::asm;
use core::sync::atomic::Ordering;

use crate::arch::cpu::{self, cpuid, rdmsr, wrmsr};
use crate::arch::percpu;
use crate::mm::{PAGE_SIZE, phys, phys_to_virt};
use crate::sync::Once;

const MSR_FEATURE_CONTROL: u32 = 0x3A;
const FEATURE_CONTROL_LOCKED: u64 = 1 << 0;
const FEATURE_CONTROL_VMX: u64 = 1 << 2;
const MSR_VMX_BASIC: u32 = 0x480;
const MSR_VMX_MISC: u32 = 0x485;
const MSR_VMX_CR0_FIXED0: u32 = 0x486;
const MSR_VMX_CR0_FIXED1: u32 = 0x487;
const MSR_VMX_CR4_FIXED0: u32 = 0x488;
const MSR_VMX_CR4_FIXED1: u32 = 0x489;
const MSR_VMX_PROCBASED2: u32 = 0x48B;
const MSR_VMX_EPT_VPID: u32 = 0x48C;
const MSR_VMX_TRUE_PINBASED: u32 = 0x48D;
const MSR_VMX_TRUE_PROCBASED: u32 = 0x48E;
const MSR_VMX_TRUE_EXIT: u32 = 0x48F;
const MSR_VMX_TRUE_ENTRY: u32 = 0x490;

pub const CR4_VMXE: u64 = 1 << 13;

/// Pin-based controls.
pub mod pin {
    pub const EXTERNAL_INTERRUPT_EXITING: u32 = 1 << 0;
    pub const NMI_EXITING: u32 = 1 << 3;
    pub const VIRTUAL_NMIS: u32 = 1 << 5;
    pub const PREEMPTION_TIMER: u32 = 1 << 6;
}

/// Primary processor-based controls.
pub mod proc {
    pub const INTERRUPT_WINDOW_EXITING: u32 = 1 << 2;
    pub const HLT_EXITING: u32 = 1 << 7;
    pub const MWAIT_EXITING: u32 = 1 << 10;
    pub const RDPMC_EXITING: u32 = 1 << 11;
    pub const CR3_LOAD_EXITING: u32 = 1 << 15;
    pub const CR3_STORE_EXITING: u32 = 1 << 16;
    pub const CR8_LOAD_EXITING: u32 = 1 << 19;
    pub const CR8_STORE_EXITING: u32 = 1 << 20;
    pub const MOV_DR_EXITING: u32 = 1 << 23;
    pub const UNCONDITIONAL_IO_EXITING: u32 = 1 << 24;
    pub const USE_MSR_BITMAPS: u32 = 1 << 28;
    pub const MONITOR_EXITING: u32 = 1 << 29;
    pub const SECONDARY_CONTROLS: u32 = 1 << 31;
}

/// Secondary processor-based controls.
pub mod proc2 {
    pub const ENABLE_EPT: u32 = 1 << 1;
    pub const ENABLE_RDTSCP: u32 = 1 << 3;
    pub const ENABLE_VPID: u32 = 1 << 5;
    pub const ENABLE_INVPCID: u32 = 1 << 12;
}

/// VM-exit controls.
pub mod exit_ctl {
    pub const SAVE_DEBUG_CONTROLS: u32 = 1 << 2;
    pub const HOST_ADDRESS_SPACE_SIZE: u32 = 1 << 9;
    pub const SAVE_PAT: u32 = 1 << 18;
    pub const LOAD_PAT: u32 = 1 << 19;
    pub const SAVE_EFER: u32 = 1 << 20;
    pub const LOAD_EFER: u32 = 1 << 21;
}

/// VM-entry controls.
pub mod entry_ctl {
    pub const LOAD_DEBUG_CONTROLS: u32 = 1 << 2;
    pub const IA32E_MODE_GUEST: u32 = 1 << 9;
    pub const LOAD_PAT: u32 = 1 << 14;
    pub const LOAD_EFER: u32 = 1 << 15;
}

/// VMCS field encodings (Intel SDM volume 3, appendix B).
#[allow(dead_code)]
pub mod field {
    pub const VPID: u32 = 0x0000;
    pub const GUEST_ES_SELECTOR: u32 = 0x0800;
    pub const GUEST_CS_SELECTOR: u32 = 0x0802;
    pub const GUEST_SS_SELECTOR: u32 = 0x0804;
    pub const GUEST_DS_SELECTOR: u32 = 0x0806;
    pub const GUEST_FS_SELECTOR: u32 = 0x0808;
    pub const GUEST_GS_SELECTOR: u32 = 0x080A;
    pub const GUEST_LDTR_SELECTOR: u32 = 0x080C;
    pub const GUEST_TR_SELECTOR: u32 = 0x080E;
    pub const HOST_ES_SELECTOR: u32 = 0x0C00;
    pub const HOST_CS_SELECTOR: u32 = 0x0C02;
    pub const HOST_SS_SELECTOR: u32 = 0x0C04;
    pub const HOST_DS_SELECTOR: u32 = 0x0C06;
    pub const HOST_FS_SELECTOR: u32 = 0x0C08;
    pub const HOST_GS_SELECTOR: u32 = 0x0C0A;
    pub const HOST_TR_SELECTOR: u32 = 0x0C0C;
    pub const IO_BITMAP_A: u32 = 0x2000;
    pub const IO_BITMAP_B: u32 = 0x2002;
    pub const MSR_BITMAP: u32 = 0x2004;
    pub const EXIT_MSR_STORE_ADDR: u32 = 0x2006;
    pub const EXIT_MSR_LOAD_ADDR: u32 = 0x2008;
    pub const ENTRY_MSR_LOAD_ADDR: u32 = 0x200A;
    pub const TSC_OFFSET: u32 = 0x2010;
    pub const EPT_POINTER: u32 = 0x201A;
    pub const GUEST_PHYSICAL_ADDRESS: u32 = 0x2400;
    pub const VMCS_LINK_POINTER: u32 = 0x2800;
    pub const GUEST_DEBUGCTL: u32 = 0x2802;
    pub const GUEST_PAT: u32 = 0x2804;
    pub const GUEST_EFER: u32 = 0x2806;
    pub const HOST_PAT: u32 = 0x2C00;
    pub const HOST_EFER: u32 = 0x2C02;
    pub const PIN_CONTROLS: u32 = 0x4000;
    pub const PROC_CONTROLS: u32 = 0x4002;
    pub const EXCEPTION_BITMAP: u32 = 0x4004;
    pub const PF_ERROR_MASK: u32 = 0x4006;
    pub const PF_ERROR_MATCH: u32 = 0x4008;
    pub const CR3_TARGET_COUNT: u32 = 0x400A;
    pub const EXIT_CONTROLS: u32 = 0x400C;
    pub const EXIT_MSR_STORE_COUNT: u32 = 0x400E;
    pub const EXIT_MSR_LOAD_COUNT: u32 = 0x4010;
    pub const ENTRY_CONTROLS: u32 = 0x4012;
    pub const ENTRY_MSR_LOAD_COUNT: u32 = 0x4014;
    pub const ENTRY_INTERRUPTION_INFO: u32 = 0x4016;
    pub const ENTRY_EXCEPTION_ERROR_CODE: u32 = 0x4018;
    pub const ENTRY_INSTRUCTION_LENGTH: u32 = 0x401A;
    pub const PROC_CONTROLS2: u32 = 0x401E;
    pub const INSTRUCTION_ERROR: u32 = 0x4400;
    pub const EXIT_REASON: u32 = 0x4402;
    pub const EXIT_INTERRUPTION_INFO: u32 = 0x4404;
    pub const EXIT_INTERRUPTION_ERROR_CODE: u32 = 0x4406;
    pub const IDT_VECTORING_INFO: u32 = 0x4408;
    pub const IDT_VECTORING_ERROR_CODE: u32 = 0x440A;
    pub const EXIT_INSTRUCTION_LENGTH: u32 = 0x440C;
    pub const GUEST_ES_LIMIT: u32 = 0x4800;
    pub const GUEST_CS_LIMIT: u32 = 0x4802;
    pub const GUEST_SS_LIMIT: u32 = 0x4804;
    pub const GUEST_DS_LIMIT: u32 = 0x4806;
    pub const GUEST_FS_LIMIT: u32 = 0x4808;
    pub const GUEST_GS_LIMIT: u32 = 0x480A;
    pub const GUEST_LDTR_LIMIT: u32 = 0x480C;
    pub const GUEST_TR_LIMIT: u32 = 0x480E;
    pub const GUEST_GDTR_LIMIT: u32 = 0x4810;
    pub const GUEST_IDTR_LIMIT: u32 = 0x4812;
    pub const GUEST_ES_ACCESS: u32 = 0x4814;
    pub const GUEST_CS_ACCESS: u32 = 0x4816;
    pub const GUEST_SS_ACCESS: u32 = 0x4818;
    pub const GUEST_DS_ACCESS: u32 = 0x481A;
    pub const GUEST_FS_ACCESS: u32 = 0x481C;
    pub const GUEST_GS_ACCESS: u32 = 0x481E;
    pub const GUEST_LDTR_ACCESS: u32 = 0x4820;
    pub const GUEST_TR_ACCESS: u32 = 0x4822;
    pub const GUEST_INTERRUPTIBILITY: u32 = 0x4824;
    pub const GUEST_ACTIVITY_STATE: u32 = 0x4826;
    pub const GUEST_SYSENTER_CS: u32 = 0x482A;
    pub const PREEMPTION_TIMER_VALUE: u32 = 0x482E;
    pub const HOST_SYSENTER_CS: u32 = 0x4C00;
    pub const CR0_MASK: u32 = 0x6000;
    pub const CR4_MASK: u32 = 0x6002;
    pub const CR0_READ_SHADOW: u32 = 0x6004;
    pub const CR4_READ_SHADOW: u32 = 0x6006;
    pub const EXIT_QUALIFICATION: u32 = 0x6400;
    pub const GUEST_LINEAR_ADDRESS: u32 = 0x640A;
    pub const GUEST_CR0: u32 = 0x6800;
    pub const GUEST_CR3: u32 = 0x6802;
    pub const GUEST_CR4: u32 = 0x6804;
    pub const GUEST_ES_BASE: u32 = 0x6806;
    pub const GUEST_CS_BASE: u32 = 0x6808;
    pub const GUEST_SS_BASE: u32 = 0x680A;
    pub const GUEST_DS_BASE: u32 = 0x680C;
    pub const GUEST_FS_BASE: u32 = 0x680E;
    pub const GUEST_GS_BASE: u32 = 0x6810;
    pub const GUEST_LDTR_BASE: u32 = 0x6812;
    pub const GUEST_TR_BASE: u32 = 0x6814;
    pub const GUEST_GDTR_BASE: u32 = 0x6816;
    pub const GUEST_IDTR_BASE: u32 = 0x6818;
    pub const GUEST_DR7: u32 = 0x681A;
    pub const GUEST_RSP: u32 = 0x681C;
    pub const GUEST_RIP: u32 = 0x681E;
    pub const GUEST_RFLAGS: u32 = 0x6820;
    pub const GUEST_PENDING_DEBUG: u32 = 0x6822;
    pub const GUEST_SYSENTER_ESP: u32 = 0x6824;
    pub const GUEST_SYSENTER_EIP: u32 = 0x6826;
    pub const HOST_CR0: u32 = 0x6C00;
    pub const HOST_CR3: u32 = 0x6C02;
    pub const HOST_CR4: u32 = 0x6C04;
    pub const HOST_FS_BASE: u32 = 0x6C06;
    pub const HOST_GS_BASE: u32 = 0x6C08;
    pub const HOST_TR_BASE: u32 = 0x6C0A;
    pub const HOST_GDTR_BASE: u32 = 0x6C0C;
    pub const HOST_IDTR_BASE: u32 = 0x6C0E;
    pub const HOST_SYSENTER_ESP: u32 = 0x6C10;
    pub const HOST_SYSENTER_EIP: u32 = 0x6C12;
    pub const HOST_RSP: u32 = 0x6C14;
    pub const HOST_RIP: u32 = 0x6C16;
}

/// Basic exit reasons (Intel SDM volume 3, appendix C).
#[allow(dead_code)]
pub mod reason {
    pub const EXCEPTION_OR_NMI: u32 = 0;
    pub const EXTERNAL_INTERRUPT: u32 = 1;
    pub const TRIPLE_FAULT: u32 = 2;
    pub const INIT: u32 = 3;
    pub const SIPI: u32 = 4;
    pub const INTERRUPT_WINDOW: u32 = 7;
    pub const NMI_WINDOW: u32 = 8;
    pub const TASK_SWITCH: u32 = 9;
    pub const CPUID: u32 = 10;
    pub const GETSEC: u32 = 11;
    pub const HLT: u32 = 12;
    pub const INVD: u32 = 13;
    pub const INVLPG: u32 = 14;
    pub const RDPMC: u32 = 15;
    pub const RDTSC: u32 = 16;
    pub const VMCALL: u32 = 18;
    pub const VMCLEAR: u32 = 19;
    pub const VMLAUNCH: u32 = 20;
    pub const VMPTRLD: u32 = 21;
    pub const VMPTRST: u32 = 22;
    pub const VMREAD: u32 = 23;
    pub const VMRESUME: u32 = 24;
    pub const VMWRITE: u32 = 25;
    pub const VMXOFF: u32 = 26;
    pub const VMXON: u32 = 27;
    pub const CR_ACCESS: u32 = 28;
    pub const MOV_DR: u32 = 29;
    pub const IO: u32 = 30;
    pub const RDMSR: u32 = 31;
    pub const WRMSR: u32 = 32;
    pub const INVALID_GUEST_STATE: u32 = 33;
    pub const MSR_LOADING: u32 = 34;
    pub const MWAIT: u32 = 36;
    pub const MONITOR_TRAP: u32 = 37;
    pub const MONITOR: u32 = 39;
    pub const PAUSE: u32 = 40;
    pub const MACHINE_CHECK: u32 = 41;
    pub const EPT_VIOLATION: u32 = 48;
    pub const EPT_MISCONFIG: u32 = 49;
    pub const INVEPT: u32 = 50;
    pub const RDTSCP: u32 = 51;
    pub const PREEMPTION_TIMER: u32 = 52;
    pub const INVVPID: u32 = 53;
    pub const WBINVD: u32 = 54;
    pub const XSETBV: u32 = 55;
    pub const RDRAND: u32 = 57;
    pub const INVPCID: u32 = 58;
    pub const VMFUNC: u32 = 59;
    pub const RDSEED: u32 = 61;
    pub const XSAVES: u32 = 63;
    pub const XRSTORS: u32 = 64;
    pub const UMWAIT: u32 = 67;
    pub const TPAUSE: u32 = 68;
}

/// What the processor's VMX offers the hypervisor, checked once.
pub struct Caps {
    /// The VMCS revision identifier.
    pub revision: u32,
    pub pin: u32,
    pub proc: u32,
    pub proc2: u32,
    pub exit: u32,
    pub entry: u32,
    /// Whether interrupt-window exiting may be turned on.
    pub interrupt_window: bool,
    /// Virtual processor identifiers, flushed by context (or else all).
    pub vpid: bool,
    pub invvpid_single: bool,
    pub invept_single: bool,
    /// The VMX-preemption timer counts the TSC divided by `1 << rate`.
    pub preemption_rate: u32,
    pub cr0_fixed0: u64,
    pub cr0_fixed1: u64,
    pub cr4_fixed0: u64,
    pub cr4_fixed1: u64,
}

static CAPS: Once<Result<Caps, &'static str>> = Once::new();

/// The controls of `msr` with `required` bits on (`None` if the processor
/// cannot), and of `optional` those it can.
fn controls(msr: u32, required: u32, optional: u32) -> Option<u32> {
    let caps = rdmsr(msr);
    let (must, may) = (caps as u32, (caps >> 32) as u32);
    (required & !may == 0).then_some((required | (optional & may) | must) & may)
}

/// Whether control bit `bit` of `msr` may be 1.
fn allowed(msr: u32, bit: u32) -> bool {
    (rdmsr(msr) >> 32) as u32 & bit != 0
}

fn probe() -> Result<Caps, &'static str> {
    if cpuid(1, 0).ecx & (1 << 5) == 0 {
        return Err("the processor has no VMX");
    }
    let fc = rdmsr(MSR_FEATURE_CONTROL);
    if fc & FEATURE_CONTROL_LOCKED != 0 && fc & FEATURE_CONTROL_VMX == 0 {
        return Err("the firmware turned VMX off");
    }
    let basic = rdmsr(MSR_VMX_BASIC);
    if (basic >> 32) & 0x1FFF > PAGE_SIZE || (basic >> 50) & 0xF != 6 {
        return Err("the VMCS does not fit a write-back page");
    }
    // Without the "true" controls, CR3 accesses would always exit.
    if basic & (1 << 55) == 0 {
        return Err("the processor lacks VMX's true controls");
    }
    let pin = controls(
        MSR_VMX_TRUE_PINBASED,
        pin::EXTERNAL_INTERRUPT_EXITING | pin::NMI_EXITING | pin::VIRTUAL_NMIS | pin::PREEMPTION_TIMER,
        0,
    )
    .ok_or("no VMX-preemption timer")?;
    let proc = controls(
        MSR_VMX_TRUE_PROCBASED,
        proc::HLT_EXITING
            | proc::MWAIT_EXITING
            | proc::RDPMC_EXITING
            | proc::CR8_LOAD_EXITING
            | proc::CR8_STORE_EXITING
            | proc::MOV_DR_EXITING
            | proc::UNCONDITIONAL_IO_EXITING
            | proc::USE_MSR_BITMAPS
            | proc::MONITOR_EXITING
            | proc::SECONDARY_CONTROLS,
        0,
    )
    .ok_or("missing processor-based controls")?;
    if proc & (proc::CR3_LOAD_EXITING | proc::CR3_STORE_EXITING) != 0 {
        return Err("CR3 accesses cannot run unintercepted");
    }
    let proc2 = controls(
        MSR_VMX_PROCBASED2,
        proc2::ENABLE_EPT | proc2::ENABLE_RDTSCP | proc2::ENABLE_INVPCID,
        proc2::ENABLE_VPID,
    )
    .ok_or("no EPT")?;
    let exit = controls(
        MSR_VMX_TRUE_EXIT,
        exit_ctl::HOST_ADDRESS_SPACE_SIZE
            | exit_ctl::SAVE_PAT
            | exit_ctl::LOAD_PAT
            | exit_ctl::SAVE_EFER
            | exit_ctl::LOAD_EFER
            | exit_ctl::SAVE_DEBUG_CONTROLS,
        0,
    )
    .ok_or("missing VM-exit controls")?;
    let entry = controls(
        MSR_VMX_TRUE_ENTRY,
        entry_ctl::IA32E_MODE_GUEST | entry_ctl::LOAD_PAT | entry_ctl::LOAD_EFER | entry_ctl::LOAD_DEBUG_CONTROLS,
        0,
    )
    .ok_or("missing VM-entry controls")?;
    let ept = rdmsr(MSR_VMX_EPT_VPID);
    // Four-level walks, write-back structures, invept for all contexts.
    if ept & (1 << 6) == 0 || ept & (1 << 14) == 0 || ept & (1 << 20) == 0 || ept & (1 << 26) == 0 {
        return Err("EPT lacks four-level write-back tables or invept");
    }
    let vpid = proc2 & proc2::ENABLE_VPID != 0 && ept & (1 << 32) != 0 && ept & (3 << 41) != 0;
    let misc = rdmsr(MSR_VMX_MISC);
    Ok(Caps {
        revision: basic as u32 & 0x7FFF_FFFF,
        pin,
        proc,
        proc2: if vpid { proc2 } else { proc2 & !proc2::ENABLE_VPID },
        exit,
        entry,
        interrupt_window: allowed(MSR_VMX_TRUE_PROCBASED, proc::INTERRUPT_WINDOW_EXITING),
        vpid,
        invvpid_single: ept & (1 << 41) != 0,
        invept_single: ept & (1 << 25) != 0,
        preemption_rate: misc as u32 & 0x1F,
        cr0_fixed0: rdmsr(MSR_VMX_CR0_FIXED0),
        cr0_fixed1: rdmsr(MSR_VMX_CR0_FIXED1),
        cr4_fixed0: rdmsr(MSR_VMX_CR4_FIXED0),
        cr4_fixed1: rdmsr(MSR_VMX_CR4_FIXED1),
    })
}

/// The VMX capabilities, or why the processor cannot run guests.
pub fn caps() -> Result<&'static Caps, &'static str> {
    if CAPS.get().is_none() {
        // Under the BKL: one probe.
        CAPS.set(probe());
    }
    CAPS.expect().as_ref().map_err(|e| *e)
}

/// Enters VMX operation on this processor, unless it is in it.
pub fn enable_here() -> Result<(), &'static str> {
    let p = percpu::current();
    if p.vmx_on.load(Ordering::Relaxed) {
        return Ok(());
    }
    let caps = caps()?;
    // SAFETY: VMX is supported (checked by `caps`); the feature control
    // MSR is written only while unlocked, and CR4.VMXE only enables VMX.
    unsafe {
        let fc = rdmsr(MSR_FEATURE_CONTROL);
        if fc & FEATURE_CONTROL_LOCKED == 0 {
            wrmsr(MSR_FEATURE_CONTROL, fc | FEATURE_CONTROL_LOCKED | FEATURE_CONTROL_VMX);
        }
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        asm!("mov cr4, {}", in(reg) cr4 | CR4_VMXE, options(nostack, preserves_flags));
    }
    let region = phys::alloc_zeroed().ok_or("out of memory for the VMXON region")?;
    // SAFETY: a fresh page of our own, through the direct map.
    unsafe { (phys_to_virt(region) as *mut u32).write(caps.revision) };
    // SAFETY: the region holds the revision; CR0 and CR4 are as VMX wants
    // them (the kernel's CR0 has PE, NE and PG, CR4 now VMXE).
    if !unsafe { vmxon(region) } {
        phys::free(region);
        return Err("vmxon failed");
    }
    p.vmx_on.store(true, Ordering::Relaxed);
    Ok(())
}

/// # Safety
/// `region` is a VMXON region; CR0 and CR4 satisfy VMX's fixed bits.
unsafe fn vmxon(region: u64) -> bool {
    let failed: u8;
    // SAFETY: forwarded to the caller.
    unsafe { asm!("vmxon [{r}]", "setna {f}", r = in(reg) &region, f = out(reg_byte) failed, options(nostack)) };
    failed == 0
}

/// Makes `vmcs` inactive on this processor and writes it to memory (its
/// launch state becomes clear).
pub fn vmclear(vmcs: u64) -> bool {
    let failed: u8;
    // SAFETY: VMCLEAR only acts on the given VMCS region, which the caller
    // owns; it is in VMX operation (checked by the processor: VMfail).
    unsafe { asm!("vmclear [{r}]", "setna {f}", r = in(reg) &vmcs, f = out(reg_byte) failed, options(nostack)) };
    failed == 0
}

/// Makes `vmcs` the current VMCS of this processor.
pub fn vmptrld(vmcs: u64) -> bool {
    let failed: u8;
    // SAFETY: as for `vmclear`.
    unsafe { asm!("vmptrld [{r}]", "setna {f}", r = in(reg) &vmcs, f = out(reg_byte) failed, options(nostack)) };
    failed == 0
}

/// Reads a field of the current VMCS.
#[inline]
pub fn read(field: u32) -> u64 {
    let v: u64;
    // SAFETY: VMREAD reads the current VMCS only.
    unsafe { asm!("vmread {v}, {f}", f = in(reg) field as u64, v = out(reg) v, options(nostack)) };
    v
}

/// Writes a field of the current VMCS (`false`: the processor has no such
/// field, or refused the value).
#[inline]
pub fn write(field: u32, value: u64) -> bool {
    let failed: u8;
    // SAFETY: VMWRITE writes the current VMCS only; what the value does
    // takes effect at the next VM entry, which checks it.
    unsafe {
        asm!("vmwrite {f}, {v}", "setna {x}", f = in(reg) field as u64, v = in(reg) value, x = out(reg_byte) failed,
            options(nostack))
    };
    failed == 0
}

/// Forgets every processor's cached EPT translations, of every guest
/// (all-context `invept`), on this processor.
pub fn invept_all() {
    let descriptor = [0u64; 2];
    // SAFETY: invalidation only drops cached translations.
    unsafe { asm!("invept {k}, [{d}]", k = in(reg) 2u64, d = in(reg) &descriptor, options(nostack)) };
}

/// Forgets this processor's cached EPT translations of one guest.
pub fn invept(eptp: u64) {
    if !caps().is_ok_and(|c| c.invept_single) {
        return invept_all();
    }
    let descriptor = [eptp, 0];
    // SAFETY: as above.
    unsafe { asm!("invept {k}, [{d}]", k = in(reg) 1u64, d = in(reg) &descriptor, options(nostack)) };
}

/// Forgets this processor's cached translations of the guest linear
/// addresses of virtual processor `vpid`.
pub fn invvpid(vpid: u16) {
    let Ok(caps) = caps() else { return };
    if !caps.vpid {
        return;
    }
    let (kind, descriptor) = if caps.invvpid_single { (1u64, [vpid as u64, 0]) } else { (2, [0, 0]) };
    // SAFETY: as above.
    unsafe { asm!("invvpid {k}, [{d}]", k = in(reg) kind, d = in(reg) &descriptor, options(nostack)) };
}

/// Sets XCR0 (extended state components enabled).
///
/// # Safety
/// `value` must be valid for this processor.
pub unsafe fn xsetbv(value: u64) {
    // SAFETY: forwarded to the caller.
    unsafe {
        asm!("xsetbv", in("ecx") 0, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nostack));
    }
}

/// The host's XCR0.
pub fn host_xcr0() -> u64 {
    cpu::features().xcr0
}
