//! Symbols that compiled Rust code expects from a C runtime.
//!
//! Veda programs link no CRT, so the runtime provides the memory
//! primitives LLVM emits calls to, the stack probe and floating-point
//! marker of the MSVC target, and a stub exception personality (we always
//! build with `panic = "abort"`, so it is never called).
//!
//! `memcpy`/`memset` use 16-byte SSE moves in a loop rather than
//! `rep movsb`, which is markedly faster under QEMU's TCG emulator.

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text
    .balign 16
    .globl memcpy
memcpy:
    mov rax, rcx
    cmp r8, 64
    jb 3f
2:
    movdqu xmm0, [rdx]
    movdqu xmm1, [rdx + 16]
    movdqu xmm2, [rdx + 32]
    movdqu xmm3, [rdx + 48]
    movdqu [rcx], xmm0
    movdqu [rcx + 16], xmm1
    movdqu [rcx + 32], xmm2
    movdqu [rcx + 48], xmm3
    add rdx, 64
    add rcx, 64
    sub r8, 64
    cmp r8, 64
    jae 2b
3:
    cmp r8, 8
    jb 5f
4:
    mov r9, [rdx]
    mov [rcx], r9
    add rdx, 8
    add rcx, 8
    sub r8, 8
    cmp r8, 8
    jae 4b
5:
    test r8, r8
    jz 7f
6:
    mov r9b, [rdx]
    mov [rcx], r9b
    inc rdx
    inc rcx
    dec r8
    jnz 6b
7:
    ret

    .balign 16
    .globl memset
memset:
    mov rax, rcx
    movzx edx, dl
    mov r9, 0x0101010101010101
    imul rdx, r9
    movq xmm0, rdx
    punpcklqdq xmm0, xmm0
    cmp r8, 64
    jb 3f
2:
    movdqu [rcx], xmm0
    movdqu [rcx + 16], xmm0
    movdqu [rcx + 32], xmm0
    movdqu [rcx + 48], xmm0
    add rcx, 64
    sub r8, 64
    cmp r8, 64
    jae 2b
3:
    cmp r8, 8
    jb 5f
4:
    mov [rcx], rdx
    add rcx, 8
    sub r8, 8
    cmp r8, 8
    jae 4b
5:
    test r8, r8
    jz 7f
6:
    mov [rcx], dl
    inc rcx
    dec r8
    jnz 6b
7:
    ret

    // MSVC stack probe: rax = bytes to allocate; touch each page below rsp.
    // Must preserve every register except r10/r11 (and flags).
    .balign 16
    .globl __chkstk
__chkstk:
    push r10
    push r11
    mov r10, rsp
    add r10, 24
    mov r11, rax
2:
    cmp r11, 4096
    jb 3f
    sub r10, 4096
    test byte ptr [r10], 0
    sub r11, 4096
    jmp 2b
3:
    sub r10, r11
    test byte ptr [r10], 0
    pop r11
    pop r10
    ret

    // memmove: forward copy is safe when dst <= src or the ranges do not
    // overlap; otherwise copy backwards.
    .balign 16
    .globl memmove
memmove:
    cmp rcx, rdx
    jbe memcpy
    lea r9, [rdx + r8]
    cmp rcx, r9
    jae memcpy
    mov rax, rcx
    add rcx, r8
    add rdx, r8
2:
    cmp r8, 8
    jb 3f
    sub rcx, 8
    sub rdx, 8
    mov r9, [rdx]
    mov [rcx], r9
    sub r8, 8
    jmp 2b
3:
    test r8, r8
    jz 4f
    dec rcx
    dec rdx
    mov r9b, [rdx]
    mov [rcx], r9b
    dec r8
    jmp 3b
4:
    ret

    .balign 16
    .globl memcmp
    .globl bcmp
memcmp:
bcmp:
    xor eax, eax
    test r8, r8
    jz 3f
2:
    movzx eax, byte ptr [rcx]
    movzx r9d, byte ptr [rdx]
    sub eax, r9d
    jnz 3f
    inc rcx
    inc rdx
    dec r8
    jnz 2b
3:
    ret

    .balign 16
    .globl strlen
strlen:
    mov rax, rcx
2:
    cmp byte ptr [rax], 0
    je 3f
    inc rax
    jmp 2b
3:
    sub rax, rcx
    ret
"#
);

/// Tells the MSVC toolchain that floating point is used.
#[unsafe(no_mangle)]
pub static _fltused: i32 = 0;

/// Exception personality referenced by unwind tables in the precompiled
/// `core`/`alloc`. Veda builds with `panic = "abort"`, so it never runs.
#[unsafe(no_mangle)]
pub extern "C" fn __CxxFrameHandler3() -> ! {
    crate::sys::process_exit(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn fmodf(x: f32, y: f32) -> f32 {
    fmod(x as f64, y as f64) as f32
}

#[unsafe(no_mangle)]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    if y == 0.0 || x.is_nan() || y.is_nan() || x.is_infinite() {
        return f64::NAN;
    }
    if y.is_infinite() {
        return x;
    }
    // x - trunc(x / y) * y, computed exactly by repeated scaled subtraction.
    let (ax, ay) = (x.abs(), y.abs());
    if ax < ay {
        return x;
    }
    let mut r = ax;
    while r >= ay {
        let mut d = ay;
        while d * 2.0 <= r {
            d *= 2.0;
        }
        r -= d;
    }
    if x < 0.0 { -r } else { r }
}
