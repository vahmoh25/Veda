//! Assembly entry and exit paths.
//!
//! * `vk_isr_stubs`: 256 interrupt stubs, 16 bytes apart, that normalise the
//!   stack (push a dummy error code when the CPU does not) and jump to
//!   `vk_interrupt_common`.
//! * `vk_interrupt_common` / `vk_return_from_trap`: save and restore the full
//!   register state as a [`TrapFrame`] and call into Rust.
//! * `vk_syscall_entry`: the `syscall` instruction target. It builds the same
//!   [`TrapFrame`] layout so that scheduling code can treat both kinds of
//!   kernel entry uniformly, and returns with `sysretq`.
//! * `vk_context_switch`: switches kernel stacks between two threads.
//!
//! `swapgs` is executed exactly when crossing the user/kernel boundary, which
//! is detected from the privilege level of the saved `CS`.

use core::arch::global_asm;

use super::gdt::{USER_CS, USER_DS};
use super::percpu::{OFF_KERNEL_RSP, OFF_USER_RSP};

/// Vector number recorded in the trap frame of a system call.
pub const SYSCALL_VECTOR: u64 = 0x100;

/// Register state saved on kernel entry. Field order mirrors the push order
/// in the assembly below (lowest address first).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == 176);

impl TrapFrame {
    pub fn is_user(&self) -> bool {
        self.cs & 3 == 3
    }

    /// A frame that enters user mode at `rip` with stack `rsp`.
    pub fn new_user(rip: u64, rsp: u64, arg0: u64, arg1: u64) -> TrapFrame {
        TrapFrame {
            rip,
            rsp,
            rdi: arg0,
            rsi: arg1,
            cs: USER_CS as u64,
            ss: USER_DS as u64,
            rflags: 0x202, // IF
            ..TrapFrame::default()
        }
    }
}

// NOTE: on the UEFI target `extern "C"` is the Win64 ABI (rcx, rdx, ...),
// while the assembly uses the System V convention (rdi, rsi, ...).
unsafe extern "sysv64" {
    /// Start of the 256 interrupt stubs (16 bytes each).
    pub static vk_isr_stubs: [u8; 4096];
    pub fn vk_syscall_entry();
    pub fn vk_thread_trampoline();
    pub fn vk_kernel_thread_trampoline();
    pub fn vk_context_switch(old_rsp: *mut u64, new_rsp: u64);
}

// The stubs are written in AT&T syntax so that `$vec` is an immediate.
global_asm!(
    r#"
    .section .text
    .balign 16
    .globl vk_isr_stubs
vk_isr_stubs:
    .set vec, 0
    .rept 256
        .balign 16
        .if (vec == 8) || (vec == 10) || (vec == 11) || (vec == 12) || (vec == 13) || (vec == 14) || (vec == 17) || (vec == 21) || (vec == 29) || (vec == 30)
        .else
            pushq $0
        .endif
        pushq $vec
        jmp vk_interrupt_common
        .set vec, vec + 1
    .endr
"#,
    options(att_syntax)
);

global_asm!(
    r#"
    .section .text
    .balign 16
    .globl vk_interrupt_common
vk_interrupt_common:
    test qword ptr [rsp + 24], 3
    jz 1f
    swapgs
1:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, rsp
    call {trap_dispatch}

    .globl vk_return_from_trap
vk_return_from_trap:
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    test qword ptr [rsp + 24], 3
    jz 2f
    swapgs
2:
    add rsp, 16
    iretq

    .balign 16
    .globl vk_syscall_entry
vk_syscall_entry:
    swapgs
    mov gs:[{user_rsp}], rsp
    mov rsp, gs:[{kernel_rsp}]
    push {user_ds}
    push qword ptr gs:[{user_rsp}]
    push r11
    push {user_cs}
    push rcx
    push 0
    push {syscall_vector}
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, rsp
    call {syscall_dispatch}
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    // [rsp] rip, [rsp+8] cs, [rsp+16] rflags, [rsp+24] rsp, [rsp+32] ss
    mov rcx, [rsp]
    mov r11, [rsp + 16]
    mov rsp, [rsp + 24]
    swapgs
    sysretq

    .balign 16
    .globl vk_context_switch
vk_context_switch:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

    // First code run by a new user thread (entered through `ret` in
    // vk_context_switch with its TrapFrame at the top of the stack).
    .balign 16
    .globl vk_thread_trampoline
vk_thread_trampoline:
    call {thread_start_hook}
    jmp vk_return_from_trap

    // First code run by a new kernel thread: r12 = entry, r13 = argument.
    .balign 16
    .globl vk_kernel_thread_trampoline
vk_kernel_thread_trampoline:
    call {kernel_thread_start_hook}
    mov rdi, r13
    call r12
    ud2
"#,
    trap_dispatch = sym crate::arch::idt::trap_dispatch,
    syscall_dispatch = sym crate::syscall::syscall_dispatch,
    thread_start_hook = sym crate::sched::thread_start_hook,
    kernel_thread_start_hook = sym crate::sched::kernel_thread_start_hook,
    user_rsp = const OFF_USER_RSP,
    kernel_rsp = const OFF_KERNEL_RSP,
    user_ds = const USER_DS as u64,
    user_cs = const USER_CS as u64,
    syscall_vector = const SYSCALL_VECTOR,
);
