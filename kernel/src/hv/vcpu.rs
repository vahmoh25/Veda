//! Virtual processors: a guest's processors, run on Intel VMX.
//!
//! A thread of the virtual machine monitor runs a virtual processor with
//! `vcpu_run`: the kernel enters the guest and handles what the guest does
//! that the processor does not let it do alone, until something needs the
//! monitor (a hypercall, an I/O port, memory that is not mapped, a
//! shutdown). The kernel handles itself what must be fast or is the
//! processor's: the local APIC and its timer, `cpuid`, model-specific
//! registers, `hlt`, control and debug registers, `xsetbv`.
//!
//! The guest runs with the BKL released, as user code does; any exit takes
//! it again. Interrupts of the host make the guest exit (they stay pending
//! and are taken right after); the CPU leaves the guest for the scheduler
//! as it would leave user code. Another thread that raises an interrupt
//! for a virtual processor in guest mode on another CPU makes that CPU
//! leave it ([`Vcpu::kick`]); one halted in its `hlt` is woken.
//!
//! The virtual processor's VMCS is current only while a thread runs it:
//! on the way out of the kernel's loop (to the monitor, or to the
//! scheduler) it is cleared, and the processor's own registers that the
//! guest shares with the host (the system call MSRs, `KERNEL_GS_BASE`,
//! `TSC_AUX`, the FPU) are given back. So a virtual processor may run on
//! any CPU next; there it forgets the translations an earlier run may have
//! left (`invvpid`, `invept`).

use alloc::sync::{Arc, Weak};
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vabi::{Error, VcpuExit, VcpuSegment, VcpuState, vcpu_exit};
use vhv::lapic::{self, Delivery, Ipi, Lapic};

use super::guest::Guest;
use super::vmx::{self, entry_ctl, field, proc, reason};
use crate::arch::cpu::{self, rdmsr, wrmsr};
use crate::arch::{apic, gdt, idt, percpu};
use crate::mm::{phys, phys_to_virt};
use crate::object::interrupt::Interrupt;
use crate::sched::{self, Thread, WakeReason, thread::FpuArea};
use crate::sync::{SpinLock, bkl};

const RFLAGS_IF: u64 = 1 << 9;
const BLOCKING_STI: u64 = 1 << 0;
const BLOCKING_MOV_SS: u64 = 1 << 1;
const BLOCKING_NMI: u64 = 1 << 3;
/// Interruption information: valid; types.
const EVENT_VALID: u64 = 1 << 31;
const EVENT_ERROR_CODE: u64 = 1 << 11;
const TYPE_EXTERNAL: u64 = 0;
const TYPE_NMI: u64 = 2 << 8;
const TYPE_HARDWARE_EXCEPTION: u64 = 3 << 8;
const EFER_SCE: u64 = 1 << 0;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const EFER_NXE: u64 = 1 << 11;
const CR0_PE: u64 = 1 << 0;
const CR0_PG: u64 = 1 << 31;
/// The power-on value of IA32_PAT.
const PAT_DEFAULT: u64 = 0x0007_0406_0007_0406;

const MSR_TSC: u32 = 0x10;
const MSR_APIC_BASE: u32 = 0x1B;
const MSR_FEATURE_CONTROL: u32 = 0x3A;
const MSR_BIOS_SIGN_ID: u32 = 0x8B;
const MSR_MTRR_CAP: u32 = 0xFE;
const MSR_SYSENTER_CS: u32 = 0x174;
const MSR_MISC_ENABLE: u32 = 0x1A0;
const MSR_DEBUGCTL: u32 = 0x1D9;
const MSR_PAT: u32 = 0x277;
const MSR_MTRR_DEF_TYPE: u32 = 0x2FF;
const MSR_CSTAR: u32 = 0xC000_0083;
const MSR_TSC_AUX: u32 = 0xC000_0103;
/// The guest's local APIC: at the architectural address, enabled, in
/// x2APIC mode.
const APIC_BASE: u64 = 0xFEE0_0000 | (1 << 11) | (1 << 10);
/// MTRRs: write-combining supported, no fixed or variable ranges.
const MTRR_CAP: u64 = 1 << 10;
/// MTRRs on, everything write-back (the EPT's memory types decide).
const MTRR_DEF_TYPE: u64 = (1 << 11) | 6;

/// The MSRs the guest reads and writes as it likes: kept in the VMCS
/// (`SYSENTER_*`, `PAT`, `FS_BASE`, `GS_BASE`) or swapped with the host's
/// as the virtual processor comes and goes (the rest).
const PASSTHROUGH_MSRS: [u32; 12] = [
    MSR_SYSENTER_CS,
    MSR_SYSENTER_CS + 1,
    MSR_SYSENTER_CS + 2,
    MSR_PAT,
    cpu::MSR_STAR,
    cpu::MSR_LSTAR,
    MSR_CSTAR,
    cpu::MSR_SFMASK,
    cpu::MSR_FS_BASE,
    cpu::MSR_GS_BASE,
    cpu::MSR_KERNEL_GS_BASE,
    MSR_TSC_AUX,
];

/// The guest's general registers as the entry and exit code moves them:
/// rax to r15 in the order instructions number them (rsp's slot unused:
/// the VMCS holds it), then cr2.
#[repr(C)]
#[derive(Default)]
struct GuestRegs {
    gprs: [u64; 16],
    cr2: u64,
}

const RSP: usize = 4;

unsafe extern "sysv64" {
    /// Enters the guest with `regs` (VMLAUNCH unless `launched`); returns 0
    /// at a VM exit, with `regs` holding the guest's, or 1 if the entry
    /// failed.
    fn vk_vmx_run(regs: *mut GuestRegs, launched: u64) -> u64;
    /// Where VM exits land (the VMCS's host RIP).
    fn vk_vmx_exit();
}

global_asm!(
    r#"
    .section .text
    .balign 16
    .globl vk_vmx_run
vk_vmx_run:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    push rdi
    mov rax, {host_rsp}
    vmwrite rax, rsp
    mov rax, [rdi + 128]
    mov cr2, rax
    test rsi, rsi
    mov rax, [rdi + 0]
    mov rcx, [rdi + 8]
    mov rdx, [rdi + 16]
    mov rbx, [rdi + 24]
    mov rbp, [rdi + 40]
    mov rsi, [rdi + 48]
    mov r8, [rdi + 64]
    mov r9, [rdi + 72]
    mov r10, [rdi + 80]
    mov r11, [rdi + 88]
    mov r12, [rdi + 96]
    mov r13, [rdi + 104]
    mov r14, [rdi + 112]
    mov r15, [rdi + 120]
    mov rdi, [rdi + 56]
    jnz 2f
    vmlaunch
    jmp 3f
2:
    vmresume
3:
    // The entry failed (the flags say how): back to the caller.
    mov eax, 1
    jmp 5f

    .balign 16
    .globl vk_vmx_exit
vk_vmx_exit:
    // The stack is as the entry left it: the register area on top.
    push rdi
    mov rdi, [rsp + 8]
    mov [rdi + 0], rax
    mov [rdi + 8], rcx
    mov [rdi + 16], rdx
    mov [rdi + 24], rbx
    mov [rdi + 40], rbp
    mov [rdi + 48], rsi
    mov [rdi + 64], r8
    mov [rdi + 72], r9
    mov [rdi + 80], r10
    mov [rdi + 88], r11
    mov [rdi + 96], r12
    mov [rdi + 104], r13
    mov [rdi + 112], r14
    mov [rdi + 120], r15
    pop rax
    mov [rdi + 56], rax
    mov rax, cr2
    mov [rdi + 128], rax
    xor eax, eax
5:
    pop rdi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
"#,
    host_rsp = const field::HOST_RSP,
);

/// What the virtual processor's last exit to the monitor waits for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Awaiting {
    Nothing,
    /// The hypercall's result, for rax.
    HypercallResult,
    /// The value an I/O port read, of this many bytes.
    PortRead(u8),
}

/// State that other threads reach (under the BKL): what raises the
/// virtual processor's interrupts, and whom to wake.
struct Shared {
    lapic: Lapic,
    nmi: bool,
    /// The thread waiting in the guest's `hlt`.
    halted: Option<Arc<Thread>>,
    /// The device line each level-triggered vector came from, which the
    /// guest's end-of-interrupt ends.
    level: [Option<Weak<Interrupt>>; 256],
}

/// The guest's registers that only the thread running the virtual
/// processor touches.
struct Context {
    regs: GuestRegs,
    /// The state the virtual processor starts in, until its VMCS has it.
    initial: Option<VcpuState>,
    /// The VMCS was set up; it is current here with the guest's registers
    /// swapped in (`load`); it was launched since it was last cleared.
    configured: bool,
    loaded: bool,
    launched: bool,
    /// The CPU the virtual processor last ran on.
    last_cpu: Option<u32>,
    awaiting: Awaiting,
    fpu: FpuArea,
    xcr0: u64,
    /// The swapped MSRs: the guest's while it is away, the host's while it
    /// runs.
    msrs: [u64; SWAPPED_MSRS.len()],
    /// Debug registers DR0-DR3, DR6 and DR7, which the guest may set and
    /// read but which do nothing.
    dr: [u64; 8],
    mtrr_def_type: u64,
    /// Interrupt-window exiting is on.
    window: bool,
}

/// The MSRs swapped as the virtual processor comes and goes.
const SWAPPED_MSRS: [u32; 6] =
    [cpu::MSR_KERNEL_GS_BASE, cpu::MSR_STAR, cpu::MSR_LSTAR, MSR_CSTAR, cpu::MSR_SFMASK, MSR_TSC_AUX];

pub struct Vcpu {
    pub koid: u64,
    pub guest: Arc<Guest>,
    /// The local APIC id.
    pub id: u32,
    vpid: u16,
    /// The VMCS and MSR bitmap pages.
    vmcs: u64,
    msr_bitmap: u64,
    /// A thread runs the virtual processor.
    running: AtomicBool,
    /// The CPU (its id + 1) the virtual processor is in guest mode on; 0
    /// if none.
    in_guest: AtomicU32,
    shared: SpinLock<Shared>,
    ctx: UnsafeCell<Context>,
}

// SAFETY: `ctx` is only reached by the thread that holds `running`;
// `shared` is behind a lock.
unsafe impl Sync for Vcpu {}
// SAFETY: as above.
unsafe impl Send for Vcpu {}

/// Virtual processor identifiers (1 to 0xFFFF; 0 is the host's), handed
/// out in turn. One that comes round again is flushed wherever it runs
/// (a virtual processor forgets its translations on every CPU it moves
/// to).
static NEXT_VPID: SpinLock<u16> = SpinLock::new(0);

fn next_vpid() -> u16 {
    let mut n = NEXT_VPID.lock();
    *n = if *n == u16::MAX { 1 } else { *n + 1 };
    *n
}

/// What the run loop does after an exit.
enum Next {
    Resume,
    /// Back to the monitor, with the exit filled in.
    Monitor,
}

impl Vcpu {
    pub fn new(guest: Arc<Guest>, id: u32, state: VcpuState) -> Result<Arc<Vcpu>, Error> {
        let caps = vmx::caps().map_err(|_| Error::NotSupported)?;
        let fpu = FpuArea::new().ok_or(Error::NoMemory)?;
        let vmcs = phys::alloc_zeroed().ok_or(Error::NoMemory)?;
        // SAFETY: our own fresh page, through the direct map.
        unsafe { (phys_to_virt(vmcs) as *mut u32).write(caps.revision) };
        let Some(msr_bitmap) = phys::alloc_zeroed() else {
            phys::free(vmcs);
            return Err(Error::NoMemory);
        };
        let vcpu = Arc::new(Vcpu {
            koid: crate::object::new_koid(),
            guest: guest.clone(),
            id,
            vpid: next_vpid(),
            vmcs,
            msr_bitmap,
            running: AtomicBool::new(false),
            in_guest: AtomicU32::new(0),
            shared: SpinLock::new(Shared {
                lapic: Lapic::new(id),
                nmi: false,
                halted: None,
                level: [const { None }; 256],
            }),
            ctx: UnsafeCell::new(Context {
                regs: GuestRegs { gprs: state.gprs, cr2: state.cr2 },
                initial: Some(state),
                configured: false,
                loaded: false,
                launched: false,
                last_cpu: None,
                awaiting: Awaiting::Nothing,
                fpu,
                xcr0: vmx::host_xcr0(),
                msrs: [0; SWAPPED_MSRS.len()],
                dr: [0, 0, 0, 0, 0, 0, 0xFFFF_0FF0, 0x400],
                mtrr_def_type: MTRR_DEF_TYPE,
                window: false,
            }),
        });
        vcpu.init_msr_bitmap();
        if !guest.add_vcpu(&vcpu) {
            return Err(Error::AlreadyExists);
        }
        Ok(vcpu)
    }

    /// Every MSR access exits, but those of [`PASSTHROUGH_MSRS`].
    fn init_msr_bitmap(&self) {
        // SAFETY: our own page, through the direct map.
        let bitmap = unsafe { &mut *(phys_to_virt(self.msr_bitmap) as *mut [u8; 4096]) };
        bitmap.fill(0xFF);
        for msr in PASSTHROUGH_MSRS {
            // Reads of the low and high ranges, then writes.
            let (base, index) = if msr >= 0xC000_0000 { (1024, msr - 0xC000_0000) } else { (0, msr) };
            for half in [0, 2048] {
                bitmap[half + base + index as usize / 8] &= !(1 << (msr % 8));
            }
        }
    }

    /// Raises interrupt `vector` at the virtual processor's local APIC.
    pub fn interrupt(&self, vector: u8) {
        self.shared.lock().lapic.request(vector);
        self.kick();
    }

    /// Raises level-triggered interrupt `vector` for `line`, which the
    /// guest's end-of-interrupt ends ([`Interrupt::end_of_level`]).
    pub fn interrupt_level(&self, vector: u8, line: &Arc<Interrupt>) {
        let mut shared = self.shared.lock();
        shared.lapic.request_level(vector);
        shared.level[vector as usize] = Some(Arc::downgrade(line));
        drop(shared);
        self.kick();
    }

    /// Makes the virtual processor look at its interrupts: wakes it if it
    /// is halted, makes its CPU leave the guest if it is in it.
    pub fn kick(&self) {
        if let Some(t) = self.shared.lock().halted.clone() {
            sched::wake(&t, WakeReason::Signaled);
        }
        let on = self.in_guest.load(Ordering::Acquire);
        if on != 0 && on - 1 != percpu::cpu_id_or_boot() {
            apic::send_ipi(percpu::get(on as usize - 1).apic_id, idt::KICK_VECTOR as u32);
        }
    }

    /// The registers, while no thread runs the virtual processor.
    pub fn read_state(&self) -> Result<VcpuState, Error> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(Error::Busy);
        }
        // SAFETY: `running` gives this thread the context.
        let ctx = unsafe { &mut *self.ctx.get() };
        let result = match ctx.initial {
            Some(state) => Ok(state),
            None => vmx::enable_here().map_err(|_| Error::NotSupported).and_then(|_| {
                if !vmx::vmptrld(self.vmcs) {
                    return Err(Error::Internal);
                }
                let state = self.state(ctx);
                vmx::vmclear(self.vmcs);
                Ok(state)
            }),
        };
        self.running.store(false, Ordering::Release);
        result
    }

    /// The registers, from the current VMCS.
    fn state(&self, ctx: &Context) -> VcpuState {
        let segment = |sel, base, limit, access| VcpuSegment {
            base: vmx::read(base),
            limit: vmx::read(limit) as u32,
            access: vmx::read(access) as u32,
            selector: vmx::read(sel) as u16,
            _reserved: [0; 3],
        };
        let mut gprs = ctx.regs.gprs;
        gprs[RSP] = vmx::read(field::GUEST_RSP);
        VcpuState {
            gprs,
            rip: vmx::read(field::GUEST_RIP),
            rflags: vmx::read(field::GUEST_RFLAGS),
            cr0: vmx::read(field::CR0_READ_SHADOW),
            cr2: ctx.regs.cr2,
            cr3: vmx::read(field::GUEST_CR3),
            cr4: vmx::read(field::CR4_READ_SHADOW),
            efer: vmx::read(field::GUEST_EFER),
            cs: segment(field::GUEST_CS_SELECTOR, field::GUEST_CS_BASE, field::GUEST_CS_LIMIT, field::GUEST_CS_ACCESS),
            ds: segment(field::GUEST_DS_SELECTOR, field::GUEST_DS_BASE, field::GUEST_DS_LIMIT, field::GUEST_DS_ACCESS),
            es: segment(field::GUEST_ES_SELECTOR, field::GUEST_ES_BASE, field::GUEST_ES_LIMIT, field::GUEST_ES_ACCESS),
            fs: segment(field::GUEST_FS_SELECTOR, field::GUEST_FS_BASE, field::GUEST_FS_LIMIT, field::GUEST_FS_ACCESS),
            gs: segment(field::GUEST_GS_SELECTOR, field::GUEST_GS_BASE, field::GUEST_GS_LIMIT, field::GUEST_GS_ACCESS),
            ss: segment(field::GUEST_SS_SELECTOR, field::GUEST_SS_BASE, field::GUEST_SS_LIMIT, field::GUEST_SS_ACCESS),
            tr: segment(field::GUEST_TR_SELECTOR, field::GUEST_TR_BASE, field::GUEST_TR_LIMIT, field::GUEST_TR_ACCESS),
            ldtr: segment(
                field::GUEST_LDTR_SELECTOR,
                field::GUEST_LDTR_BASE,
                field::GUEST_LDTR_LIMIT,
                field::GUEST_LDTR_ACCESS,
            ),
            gdt_base: vmx::read(field::GUEST_GDTR_BASE),
            idt_base: vmx::read(field::GUEST_IDTR_BASE),
            gdt_limit: vmx::read(field::GUEST_GDTR_LIMIT) as u32,
            idt_limit: vmx::read(field::GUEST_IDTR_LIMIT) as u32,
        }
    }

    /// Runs the virtual processor until it needs the monitor; `exit`
    /// brings the monitor's answer to the last exit, and takes the next.
    pub fn run(&self, exit: &mut VcpuExit) -> Result<(), Error> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(Error::Busy);
        }
        // SAFETY: `running` gives this thread the context.
        let ctx = unsafe { &mut *self.ctx.get() };
        self.answer(ctx, exit);
        let result = self.load(ctx).and_then(|_| self.run_loop(ctx, exit));
        self.put(ctx);
        self.running.store(false, Ordering::Release);
        result
    }

    /// Gives the guest the monitor's answer to the last exit.
    fn answer(&self, ctx: &mut Context, exit: &VcpuExit) {
        match ctx.awaiting {
            Awaiting::Nothing => {}
            Awaiting::HypercallResult => ctx.regs.gprs[0] = exit.data[0],
            Awaiting::PortRead(size) => {
                let rax = &mut ctx.regs.gprs[0];
                *rax = match size {
                    1 => (*rax & !0xFF) | (exit.data[3] & 0xFF),
                    2 => (*rax & !0xFFFF) | (exit.data[3] & 0xFFFF),
                    // A 32-bit result clears the upper half, as on hardware.
                    _ => exit.data[3] & 0xFFFF_FFFF,
                };
            }
        }
        ctx.awaiting = Awaiting::Nothing;
    }

    /// Makes the VMCS current on this CPU (entering VMX operation here if
    /// the thread came to a CPU that is not in it yet), with the host's
    /// state as it is here, and swaps the guest's registers in.
    fn load(&self, ctx: &mut Context) -> Result<(), Error> {
        vmx::enable_here().map_err(|_| Error::NotSupported)?;
        let cpu = percpu::cpu_id_or_boot();
        if !ctx.configured && !vmx::vmclear(self.vmcs) {
            crate::kwarn!("vmx: vmclear failed");
            return Err(Error::Internal);
        }
        if !vmx::vmptrld(self.vmcs) {
            crate::kwarn!("vmx: vmptrld failed");
            return Err(Error::Internal);
        }
        if !ctx.configured {
            self.configure(ctx)?;
            ctx.configured = true;
        }
        let here = percpu::current();
        let mut ok = vmx::write(field::HOST_CR3, cpu::read_cr3());
        ok &= vmx::write(field::HOST_CR4, read_cr4());
        ok &= vmx::write(field::HOST_FS_BASE, rdmsr(cpu::MSR_FS_BASE));
        ok &= vmx::write(field::HOST_GS_BASE, rdmsr(cpu::MSR_GS_BASE));
        ok &= vmx::write(field::HOST_TR_BASE, here.tss.get() as u64);
        ok &= vmx::write(field::HOST_GDTR_BASE, here.gdt.get() as u64);
        ok &= vmx::write(field::HOST_IDTR_BASE, idt_base());
        if !ok {
            crate::kwarn!("vmx: the processor refused the host's state");
            return Err(Error::Internal);
        }
        if ctx.last_cpu != Some(cpu) {
            vmx::invvpid(self.vpid);
            vmx::invept(self.guest.ept_pointer());
            ctx.last_cpu = Some(cpu);
        }
        sched::current().save_fpu();
        ctx.fpu.restore();
        for (msr, value) in SWAPPED_MSRS.iter().zip(ctx.msrs.iter_mut()) {
            let host = rdmsr(*msr);
            // SAFETY: the guest's values of MSRs that only take effect in
            // user mode or in the guest, which this CPU enters next; the
            // host's are given back before it goes anywhere else.
            unsafe { wrmsr(*msr, *value) };
            *value = host;
        }
        ctx.loaded = true;
        Ok(())
    }

    /// Gives the host its registers back (if `load` got as far as swapping
    /// them) and clears the VMCS.
    fn put(&self, ctx: &mut Context) {
        if core::mem::replace(&mut ctx.loaded, false) {
            for (msr, value) in SWAPPED_MSRS.iter().zip(ctx.msrs.iter_mut()) {
                let guest = rdmsr(*msr);
                // SAFETY: the host's own values, taken in `load`.
                unsafe { wrmsr(*msr, *value) };
                *value = guest;
            }
            ctx.fpu.save();
            sched::current().restore_fpu();
        }
        // (A CPU that could not enter VMX operation has no VMCS current.)
        if percpu::current().vmx_on.load(Ordering::Relaxed) {
            vmx::vmclear(self.vmcs);
        }
        ctx.launched = false;
    }

    /// Writes every field of a new VMCS: the controls, the host's state
    /// that does not depend on the CPU, the initial guest state.
    fn configure(&self, ctx: &mut Context) -> Result<(), Error> {
        let caps = vmx::caps().map_err(|_| Error::NotSupported)?;
        let state = ctx.initial.take().ok_or(Error::BadState)?;
        let mut failed: Option<u32> = None;
        let mut w = |f: u32, v: u64| {
            if !vmx::write(f, v) && failed.is_none() {
                failed = Some(f);
            }
        };
        let long_mode = state.efer & EFER_LMA != 0;
        w(field::PIN_CONTROLS, caps.pin as u64);
        w(field::PROC_CONTROLS, caps.proc as u64);
        w(field::PROC_CONTROLS2, caps.proc2 as u64);
        w(field::EXIT_CONTROLS, caps.exit as u64);
        let mut entry = caps.entry & !entry_ctl::IA32E_MODE_GUEST;
        if long_mode {
            entry |= entry_ctl::IA32E_MODE_GUEST;
        }
        w(field::ENTRY_CONTROLS, entry as u64);
        w(field::EXCEPTION_BITMAP, 0);
        w(field::PF_ERROR_MASK, 0);
        w(field::PF_ERROR_MATCH, 0);
        w(field::CR3_TARGET_COUNT, 0);
        w(field::EXIT_MSR_STORE_COUNT, 0);
        w(field::EXIT_MSR_LOAD_COUNT, 0);
        w(field::ENTRY_MSR_LOAD_COUNT, 0);
        w(field::ENTRY_INTERRUPTION_INFO, 0);
        w(field::MSR_BITMAP, self.msr_bitmap);
        w(field::EPT_POINTER, self.guest.ept_pointer());
        if caps.vpid {
            w(field::VPID, self.vpid as u64);
        }
        w(field::VMCS_LINK_POINTER, u64::MAX);
        // CR0's and CR4's bits that VMX fixes are the host's; the guest sees
        // what it wrote.
        w(field::CR0_MASK, caps.cr0_fixed0);
        w(field::CR0_READ_SHADOW, state.cr0);
        w(field::GUEST_CR0, (state.cr0 | caps.cr0_fixed0) & caps.cr0_fixed1);
        w(field::CR4_MASK, caps.cr4_fixed0);
        w(field::CR4_READ_SHADOW, state.cr4);
        w(field::GUEST_CR4, (state.cr4 | caps.cr4_fixed0) & caps.cr4_fixed1);
        w(field::GUEST_CR3, state.cr3);
        w(field::GUEST_DR7, 0x400);
        w(field::GUEST_RSP, state.gprs[RSP]);
        w(field::GUEST_RIP, state.rip);
        w(field::GUEST_RFLAGS, state.rflags | 2);
        w(field::GUEST_EFER, state.efer);
        w(field::GUEST_PAT, PAT_DEFAULT);
        w(field::GUEST_DEBUGCTL, 0);
        w(field::GUEST_SYSENTER_CS, 0);
        w(field::GUEST_SYSENTER_ESP, 0);
        w(field::GUEST_SYSENTER_EIP, 0);
        w(field::GUEST_INTERRUPTIBILITY, 0);
        w(field::GUEST_ACTIVITY_STATE, 0);
        w(field::GUEST_PENDING_DEBUG, 0);
        let segments = [
            (state.es, field::GUEST_ES_SELECTOR, field::GUEST_ES_BASE, field::GUEST_ES_LIMIT, field::GUEST_ES_ACCESS),
            (state.cs, field::GUEST_CS_SELECTOR, field::GUEST_CS_BASE, field::GUEST_CS_LIMIT, field::GUEST_CS_ACCESS),
            (state.ss, field::GUEST_SS_SELECTOR, field::GUEST_SS_BASE, field::GUEST_SS_LIMIT, field::GUEST_SS_ACCESS),
            (state.ds, field::GUEST_DS_SELECTOR, field::GUEST_DS_BASE, field::GUEST_DS_LIMIT, field::GUEST_DS_ACCESS),
            (state.fs, field::GUEST_FS_SELECTOR, field::GUEST_FS_BASE, field::GUEST_FS_LIMIT, field::GUEST_FS_ACCESS),
            (state.gs, field::GUEST_GS_SELECTOR, field::GUEST_GS_BASE, field::GUEST_GS_LIMIT, field::GUEST_GS_ACCESS),
            (
                state.ldtr,
                field::GUEST_LDTR_SELECTOR,
                field::GUEST_LDTR_BASE,
                field::GUEST_LDTR_LIMIT,
                field::GUEST_LDTR_ACCESS,
            ),
            (state.tr, field::GUEST_TR_SELECTOR, field::GUEST_TR_BASE, field::GUEST_TR_LIMIT, field::GUEST_TR_ACCESS),
        ];
        for (s, selector, base, limit, access) in segments {
            w(selector, s.selector as u64);
            w(base, s.base);
            w(limit, s.limit as u64);
            w(access, s.access as u64);
        }
        w(field::GUEST_GDTR_BASE, state.gdt_base);
        w(field::GUEST_GDTR_LIMIT, state.gdt_limit as u64);
        w(field::GUEST_IDTR_BASE, state.idt_base);
        w(field::GUEST_IDTR_LIMIT, state.idt_limit as u64);
        // The host's state that is the same on every CPU.
        w(field::HOST_CR0, read_cr0());
        w(field::HOST_CS_SELECTOR, gdt::KERNEL_CS as u64);
        w(field::HOST_SS_SELECTOR, gdt::KERNEL_DS as u64);
        w(field::HOST_DS_SELECTOR, gdt::KERNEL_DS as u64);
        w(field::HOST_ES_SELECTOR, gdt::KERNEL_DS as u64);
        w(field::HOST_FS_SELECTOR, 0);
        w(field::HOST_GS_SELECTOR, 0);
        w(field::HOST_TR_SELECTOR, gdt::TSS_SEL as u64);
        w(field::HOST_PAT, rdmsr(MSR_PAT));
        w(field::HOST_EFER, rdmsr(cpu::MSR_EFER));
        w(field::HOST_SYSENTER_CS, 0);
        w(field::HOST_SYSENTER_ESP, 0);
        w(field::HOST_SYSENTER_EIP, 0);
        w(field::HOST_RIP, vk_vmx_exit as *const () as u64);
        match failed {
            None => Ok(()),
            Some(f) => {
                crate::kwarn!("vmx: the processor refused VMCS field {:#06x}", f);
                Err(Error::Internal)
            }
        }
    }

    fn run_loop(&self, ctx: &mut Context, exit: &mut VcpuExit) -> Result<(), Error> {
        let caps = vmx::caps().map_err(|_| Error::NotSupported)?;
        let host_xcr0 = vmx::host_xcr0();
        loop {
            let thread = sched::current();
            if thread.kill_pending.load(Ordering::Acquire) {
                return Err(Error::Canceled);
            }
            if sched::need_resched() {
                self.put(ctx);
                sched::schedule();
                self.load(ctx)?;
                continue;
            }
            drop(thread);
            let now = cpu::rdtsc();
            let deadline = {
                let mut s = self.shared.lock();
                s.lapic.expire_timer(now);
                s.lapic.timer_deadline()
            };
            self.inject(ctx, caps.interrupt_window);
            // The preemption timer brings the guest back for its own timer.
            let ticks = deadline.map_or(u32::MAX as u64, |d| d.saturating_sub(now) >> caps.preemption_rate);
            vmx::write(field::PREEMPTION_TIMER_VALUE, ticks.min(u32::MAX as u64));
            if ctx.xcr0 != host_xcr0 {
                // SAFETY: a value `xsetbv` checked against the host's.
                unsafe { vmx::xsetbv(ctx.xcr0) };
            }
            let cpu = percpu::cpu_id_or_boot();
            self.in_guest.store(cpu + 1, Ordering::Release);
            bkl::release();
            // SAFETY: the VMCS is current and complete; the guest's
            // registers are in `ctx.regs`.
            let failed = unsafe { vk_vmx_run(&mut ctx.regs, ctx.launched as u64) } != 0;
            self.in_guest.store(0, Ordering::Release);
            if ctx.xcr0 != host_xcr0 {
                // SAFETY: the host's own value.
                unsafe { vmx::xsetbv(host_xcr0) };
            }
            let exit_reason = vmx::read(field::EXIT_REASON) as u32;
            if !failed {
                ctx.launched = true;
                match exit_reason {
                    // Take the host's interrupt that made the guest exit.
                    reason::EXTERNAL_INTERRUPT => cpu::take_pending_interrupts(),
                    reason::EXCEPTION_OR_NMI if is_nmi(vmx::read(field::EXIT_INTERRUPTION_INFO)) => {
                        // SAFETY: hands the NMI to the host's handler (which
                        // may change memory: a barrier to the compiler).
                        unsafe { core::arch::asm!("int 2", options(nostack)) };
                    }
                    _ => {}
                }
            }
            bkl::acquire();
            if failed {
                let error = vmx::read(field::INSTRUCTION_ERROR);
                self.fail(exit, u32::MAX, error);
                return Ok(());
            }
            if exit_reason & (1 << 31) != 0 {
                // The processor refused the guest's state.
                let qualification = vmx::read(field::EXIT_QUALIFICATION);
                self.fail(exit, exit_reason, qualification);
                return Ok(());
            }
            self.reinject();
            match self.handle_exit(ctx, exit_reason & 0xFFFF, exit)? {
                Next::Resume => {}
                Next::Monitor => return Ok(()),
            }
        }
    }

    /// The exit failed the guest: the monitor learns why.
    fn fail(&self, exit: &mut VcpuExit, why: u32, detail: u64) {
        let rip = vmx::read(field::GUEST_RIP);
        crate::kwarn!("vcpu {}: exit {:#x} ({:#x}) at rip {:#x}", self.id, why, detail, rip);
        *exit = VcpuExit { reason: vcpu_exit::FAILED, _reserved: 0, data: [why as u64, detail, rip, 0, 0, 0, 0] };
    }

    /// An event whose delivery the exit interrupted is delivered again.
    fn reinject(&self) {
        let info = vmx::read(field::IDT_VECTORING_INFO);
        if info & EVENT_VALID == 0 {
            return;
        }
        vmx::write(field::ENTRY_INTERRUPTION_INFO, info & !(1 << 12));
        if info & EVENT_ERROR_CODE != 0 {
            vmx::write(field::ENTRY_EXCEPTION_ERROR_CODE, vmx::read(field::IDT_VECTORING_ERROR_CODE));
        }
        // Software interrupts and exceptions need the instruction's length.
        if matches!((info >> 8) & 7, 4..=6) {
            vmx::write(field::ENTRY_INSTRUCTION_LENGTH, vmx::read(field::EXIT_INSTRUCTION_LENGTH));
        }
    }

    /// Delivers the NMI or the interrupt the guest can take now, if no
    /// event is on its way already, and asks to come back once the guest
    /// can take the next.
    fn inject(&self, ctx: &mut Context, window_allowed: bool) {
        let mut s = self.shared.lock();
        let busy = vmx::read(field::ENTRY_INTERRUPTION_INFO) & EVENT_VALID != 0;
        let blocking = vmx::read(field::GUEST_INTERRUPTIBILITY);
        let mut want_window = false;
        if !busy {
            if s.nmi && blocking & (BLOCKING_STI | BLOCKING_MOV_SS | BLOCKING_NMI) == 0 {
                s.nmi = false;
                vmx::write(field::ENTRY_INTERRUPTION_INFO, EVENT_VALID | TYPE_NMI | 2);
            } else if s.lapic.pending().is_some() {
                let open =
                    vmx::read(field::GUEST_RFLAGS) & RFLAGS_IF != 0 && blocking & (BLOCKING_STI | BLOCKING_MOV_SS) == 0;
                if open {
                    if let Some(v) = s.lapic.acknowledge() {
                        vmx::write(field::ENTRY_INTERRUPTION_INFO, EVENT_VALID | TYPE_EXTERNAL | v as u64);
                    }
                } else {
                    want_window = true;
                }
            }
        }
        // One more waits: come back once the guest takes interrupts again.
        if s.lapic.pending().is_some() || s.nmi {
            want_window = true;
        }
        drop(s);
        let want_window = want_window && window_allowed;
        if want_window != ctx.window {
            let mut controls = vmx::read(field::PROC_CONTROLS) as u32;
            if want_window {
                controls |= proc::INTERRUPT_WINDOW_EXITING;
            } else {
                controls &= !proc::INTERRUPT_WINDOW_EXITING;
            }
            vmx::write(field::PROC_CONTROLS, controls as u64);
            ctx.window = want_window;
        }
    }

    fn handle_exit(&self, ctx: &mut Context, why: u32, exit: &mut VcpuExit) -> Result<Next, Error> {
        let qualification = vmx::read(field::EXIT_QUALIFICATION);
        match why {
            reason::EXTERNAL_INTERRUPT
            | reason::INTERRUPT_WINDOW
            | reason::NMI_WINDOW
            | reason::PREEMPTION_TIMER
            | reason::INIT
            | reason::SIPI => {}
            reason::EXCEPTION_OR_NMI if is_nmi(vmx::read(field::EXIT_INTERRUPTION_INFO)) => {}
            reason::CPUID => {
                let (leaf, subleaf) = (ctx.regs.gprs[0] as u32, ctx.regs.gprs[1] as u32);
                let cr4 = vmx::read(field::GUEST_CR4);
                let c = vhv::cpuid::Context {
                    apic_id: self.id,
                    cpus: self.guest.cpus,
                    tsc_khz: (crate::time::tsc_hz() / 1000) as u32,
                    osxsave: cr4 & (1 << 18) != 0,
                    xcr0: vmx::host_xcr0(),
                };
                let r = vhv::cpuid::guest(leaf, subleaf, &c, |l, s| {
                    let r = cpu::cpuid(l, s);
                    [r.eax, r.ebx, r.ecx, r.edx]
                });
                for (i, v) in [0, 3, 1, 2].into_iter().zip(r) {
                    ctx.regs.gprs[i] = v as u64;
                }
                skip_instruction();
            }
            reason::RDMSR => match self.read_msr(ctx, ctx.regs.gprs[1] as u32) {
                Some(v) => {
                    ctx.regs.gprs[0] = v & 0xFFFF_FFFF;
                    ctx.regs.gprs[2] = v >> 32;
                    skip_instruction();
                }
                None => inject_exception(13, Some(0)),
            },
            reason::WRMSR => {
                let value = (ctx.regs.gprs[2] << 32) | (ctx.regs.gprs[0] & 0xFFFF_FFFF);
                if self.write_msr(ctx, ctx.regs.gprs[1] as u32, value) {
                    skip_instruction();
                } else {
                    inject_exception(13, Some(0));
                }
            }
            reason::HLT => {
                skip_instruction();
                self.halt(ctx)?;
            }
            reason::VMCALL => {
                skip_instruction();
                let g = &ctx.regs.gprs;
                *exit = VcpuExit {
                    reason: vcpu_exit::HYPERCALL,
                    _reserved: 0,
                    data: [g[0], g[3], g[1], g[2], g[6], g[7], 0],
                };
                ctx.awaiting = Awaiting::HypercallResult;
                return Ok(Next::Monitor);
            }
            reason::IO => {
                // String instructions (`ins`, `outs`) are not for this
                // platform.
                if qualification & (1 << 4) != 0 {
                    self.fail(exit, why, qualification);
                    return Ok(Next::Monitor);
                }
                let size = (qualification & 7) + 1;
                let read = qualification & (1 << 3) != 0;
                let port = qualification >> 16;
                skip_instruction();
                let value = if read { 0 } else { ctx.regs.gprs[0] & mask(size) };
                *exit = VcpuExit {
                    reason: vcpu_exit::IO,
                    _reserved: 0,
                    data: [port, size, (!read) as u64, value, 0, 0, 0],
                };
                if read {
                    ctx.awaiting = Awaiting::PortRead(size as u8);
                }
                return Ok(Next::Monitor);
            }
            reason::EPT_VIOLATION => {
                let gpa = vmx::read(field::GUEST_PHYSICAL_ADDRESS);
                let rip = vmx::read(field::GUEST_RIP);
                *exit = VcpuExit {
                    reason: vcpu_exit::MEMORY,
                    _reserved: 0,
                    data: [gpa, qualification & 7, rip, 0, 0, 0, 0],
                };
                return Ok(Next::Monitor);
            }
            reason::TRIPLE_FAULT => {
                *exit = VcpuExit { reason: vcpu_exit::SHUTDOWN, ..VcpuExit::default() };
                return Ok(Next::Monitor);
            }
            reason::CR_ACCESS => {
                if !self.cr_access(ctx, qualification) {
                    self.fail(exit, why, qualification);
                    return Ok(Next::Monitor);
                }
            }
            reason::MOV_DR => {
                let dr = (qualification & 7) as usize;
                let gpr = ((qualification >> 8) & 0xF) as usize;
                if qualification & (1 << 4) != 0 {
                    set_gpr(ctx, gpr, ctx.dr[dr]);
                } else {
                    ctx.dr[dr] = gpr_value(ctx, gpr);
                }
                skip_instruction();
            }
            reason::XSETBV => {
                let value = (ctx.regs.gprs[2] << 32) | (ctx.regs.gprs[0] & 0xFFFF_FFFF);
                let host = vmx::host_xcr0();
                // x87 always; AVX needs SSE; nothing the host lacks.
                let valid = ctx.regs.gprs[1] as u32 == 0
                    && value & 1 != 0
                    && value & !host == 0
                    && (value & 4 == 0 || value & 2 != 0);
                if valid {
                    ctx.xcr0 = value;
                    skip_instruction();
                } else {
                    inject_exception(13, Some(0));
                }
            }
            reason::INVD | reason::WBINVD => skip_instruction(),
            reason::RDPMC => inject_exception(13, Some(0)),
            reason::MWAIT
            | reason::MONITOR
            | reason::GETSEC
            | reason::VMCLEAR
            | reason::VMLAUNCH
            | reason::VMPTRLD
            | reason::VMPTRST
            | reason::VMREAD
            | reason::VMRESUME
            | reason::VMWRITE
            | reason::VMXOFF
            | reason::VMXON
            | reason::INVEPT
            | reason::INVVPID
            | reason::VMFUNC => inject_exception(6, None),
            _ => {
                self.fail(exit, why, qualification);
                return Ok(Next::Monitor);
            }
        }
        Ok(Next::Resume)
    }

    /// `hlt`: waits off the CPU until the guest has an interrupt it can
    /// take (or an NMI), or its timer fires.
    fn halt(&self, ctx: &mut Context) -> Result<(), Error> {
        let interrupts_on = vmx::read(field::GUEST_RFLAGS) & RFLAGS_IF != 0;
        loop {
            let now = cpu::rdtsc();
            let deadline = {
                let mut s = self.shared.lock();
                s.lapic.expire_timer(now);
                if s.nmi || (interrupts_on && s.lapic.pending().is_some()) {
                    return Ok(());
                }
                s.lapic.timer_deadline()
            };
            let thread = sched::current();
            self.put(ctx);
            self.shared.lock().halted = Some(thread);
            let woke = sched::block(deadline.map(crate::time::tsc_to_ns));
            self.shared.lock().halted = None;
            self.load(ctx)?;
            if woke == WakeReason::Killed {
                return Err(Error::Canceled);
            }
        }
    }

    fn read_msr(&self, ctx: &Context, msr: u32) -> Option<u64> {
        if lapic::is_lapic_msr(msr) {
            return self.shared.lock().lapic.read(msr, cpu::rdtsc());
        }
        Some(match msr {
            MSR_TSC => cpu::rdtsc(),
            MSR_APIC_BASE => APIC_BASE | if self.id == 0 { 1 << 8 } else { 0 },
            // Locked, with VMX off.
            MSR_FEATURE_CONTROL => 1,
            MSR_BIOS_SIGN_ID => 0,
            MSR_MTRR_CAP => MTRR_CAP,
            MSR_MTRR_DEF_TYPE => ctx.mtrr_def_type,
            // Fast strings.
            MSR_MISC_ENABLE => 1,
            MSR_DEBUGCTL => 0,
            cpu::MSR_EFER => vmx::read(field::GUEST_EFER),
            _ => return None,
        })
    }

    /// Writes an MSR (`false`: the guest may not).
    fn write_msr(&self, ctx: &mut Context, msr: u32, value: u64) -> bool {
        if lapic::is_lapic_msr(msr) {
            let written = self.shared.lock().lapic.write(msr, value, cpu::rdtsc());
            return match written {
                lapic::Write::Done => true,
                lapic::Write::Refused => false,
                lapic::Write::Ipi(ipi) => {
                    self.send_ipi(ipi);
                    true
                }
                lapic::Write::EndOfLevel(vector) => {
                    let line = self.shared.lock().level[vector as usize].take();
                    if let Some(line) = line.and_then(|l| l.upgrade()) {
                        line.end_of_level();
                    }
                    true
                }
            };
        }
        match msr {
            // The local APIC stays enabled, in x2APIC mode.
            MSR_APIC_BASE => value & !(1 << 8) == APIC_BASE,
            MSR_MTRR_DEF_TYPE => {
                ctx.mtrr_def_type = value & 0xCFF;
                true
            }
            // Nothing to change: the TSC is the host's, there is no
            // microcode to update, debug controls stay off.
            MSR_TSC | MSR_BIOS_SIGN_ID | MSR_MISC_ENABLE => true,
            MSR_DEBUGCTL => value == 0,
            cpu::MSR_EFER => {
                let current = vmx::read(field::GUEST_EFER);
                // System calls and no-execute may change; long mode may not.
                if value & !(EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE) != 0
                    || (value ^ current) & (EFER_LME | EFER_LMA) != 0
                {
                    return false;
                }
                vmx::write(field::GUEST_EFER, value)
            }
            _ => false,
        }
    }

    /// Delivers an inter-processor interrupt to the guest's processors it
    /// names.
    fn send_ipi(&self, ipi: Ipi) {
        for v in self.guest.vcpus() {
            let me = v.id == self.id;
            let mut s = v.shared.lock();
            if !s.lapic.is_destination(ipi.destination, me) {
                continue;
            }
            match ipi.delivery {
                Delivery::Fixed(vector) => s.lapic.request(vector),
                Delivery::Nmi => s.nmi = true,
                // Processors start through the platform's hypercall.
                Delivery::Unsupported(_) => continue,
            }
            drop(s);
            if !me {
                v.kick();
            }
        }
    }

    /// A control register access the guest may not make alone: CR0's and
    /// CR4's bits that VMX fixes, and CR8 (the local APIC's task
    /// priority). `false` for what this platform does not do (leaving
    /// protected mode or paging, turning VMX on).
    fn cr_access(&self, ctx: &mut Context, qualification: u64) -> bool {
        let Ok(caps) = vmx::caps() else { return false };
        let cr = qualification & 0xF;
        let kind = (qualification >> 4) & 3;
        let gpr = ((qualification >> 8) & 0xF) as usize;
        match (cr, kind) {
            // mov to cr0
            (0, 0) => {
                let value = gpr_value(ctx, gpr);
                if value & (CR0_PE | CR0_PG) != CR0_PE | CR0_PG {
                    return false;
                }
                vmx::write(field::CR0_READ_SHADOW, value);
                vmx::write(field::GUEST_CR0, (value | caps.cr0_fixed0) & caps.cr0_fixed1);
            }
            // lmsw: the low four bits; PE cannot be cleared by it.
            (0, 3) => {
                let source = (qualification >> 16) & 0xF;
                let shadow = vmx::read(field::CR0_READ_SHADOW);
                let value = (shadow & !0xE) | (source & 0xF) | (shadow & CR0_PE);
                vmx::write(field::CR0_READ_SHADOW, value);
                vmx::write(field::GUEST_CR0, (value | caps.cr0_fixed0) & caps.cr0_fixed1);
            }
            // mov to cr4
            (4, 0) => {
                let value = gpr_value(ctx, gpr);
                if value & vmx::CR4_VMXE != 0 {
                    inject_exception(13, Some(0));
                    return true;
                }
                vmx::write(field::CR4_READ_SHADOW, value);
                vmx::write(field::GUEST_CR4, (value | caps.cr4_fixed0) & caps.cr4_fixed1);
            }
            // mov to and from cr8
            (8, 0) => {
                let tpr = (gpr_value(ctx, gpr) & 0xF) << 4;
                let _ = self.shared.lock().lapic.write(0x808, tpr, cpu::rdtsc());
            }
            (8, 1) => {
                let tpr = self.shared.lock().lapic.read(0x808, 0).unwrap_or(0);
                set_gpr(ctx, gpr, tpr >> 4);
            }
            _ => return false,
        }
        skip_instruction();
        true
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        phys::free(self.vmcs);
        phys::free(self.msr_bitmap);
    }
}

fn is_nmi(info: u64) -> bool {
    info & EVENT_VALID != 0 && info & (7 << 8) == TYPE_NMI
}

fn mask(size: u64) -> u64 {
    if size >= 8 { u64::MAX } else { (1 << (size * 8)) - 1 }
}

fn gpr_value(ctx: &Context, gpr: usize) -> u64 {
    if gpr == RSP { vmx::read(field::GUEST_RSP) } else { ctx.regs.gprs[gpr] }
}

fn set_gpr(ctx: &mut Context, gpr: usize, value: u64) {
    if gpr == RSP {
        vmx::write(field::GUEST_RSP, value);
    } else {
        ctx.regs.gprs[gpr] = value;
    }
}

/// Moves the guest past the instruction that exited; an interrupt shadow
/// it was in (after `sti` or `mov ss`) ends with it.
fn skip_instruction() {
    let rip = vmx::read(field::GUEST_RIP);
    vmx::write(field::GUEST_RIP, rip + vmx::read(field::EXIT_INSTRUCTION_LENGTH));
    let blocking = vmx::read(field::GUEST_INTERRUPTIBILITY);
    if blocking & (BLOCKING_STI | BLOCKING_MOV_SS) != 0 {
        vmx::write(field::GUEST_INTERRUPTIBILITY, blocking & !(BLOCKING_STI | BLOCKING_MOV_SS));
    }
}

/// Raises exception `vector` in the guest at its next entry.
fn inject_exception(vector: u8, error_code: Option<u32>) {
    let mut info = EVENT_VALID | TYPE_HARDWARE_EXCEPTION | vector as u64;
    if let Some(code) = error_code {
        info |= EVENT_ERROR_CODE;
        vmx::write(field::ENTRY_EXCEPTION_ERROR_CODE, code as u64);
    }
    vmx::write(field::ENTRY_INTERRUPTION_INFO, info);
}

fn read_cr0() -> u64 {
    let v: u64;
    // SAFETY: reading CR0 has no side effects.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

fn read_cr4() -> u64 {
    let v: u64;
    // SAFETY: reading CR4 has no side effects.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// The base of the IDT (the same on every CPU).
fn idt_base() -> u64 {
    let mut idtr = [0u8; 10];
    // SAFETY: SIDT stores 10 bytes.
    unsafe { core::arch::asm!("sidt [{}]", in(reg) idtr.as_mut_ptr(), options(nostack, preserves_flags)) };
    u64::from_le_bytes(idtr[2..10].try_into().unwrap())
}
