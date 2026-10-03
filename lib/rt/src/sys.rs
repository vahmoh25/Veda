//! Raw system calls. Prefer the safe wrappers in [`crate::object`] and the
//! other modules; these exist for completeness and for low-level code.

use core::arch::asm;

pub use vabi::nr;
use vabi::{Error, RawHandle};

/// Result of a raw system call.
pub type SysResult = Result<usize, Error>;

#[inline(always)]
fn decode(ret: isize) -> SysResult {
    Error::from_return(ret)
}

#[inline(always)]
pub unsafe fn syscall6(n: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> (isize, usize) {
    let ret: isize;
    let rdx: usize;
    // SAFETY: the kernel preserves all registers except rax, rdx (second
    // result), rcx and r11.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a0,
            in("rsi") a1,
            inlateout("rdx") a2 => rdx,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (ret, rdx)
}

#[inline(always)]
pub fn call(n: usize, a: [usize; 6]) -> SysResult {
    // SAFETY: system calls validate all of their arguments; memory passed by
    // pointer is owned by the safe wrappers that call this.
    decode(unsafe { syscall6(n, a[0], a[1], a[2], a[3], a[4], a[5]) }.0)
}

#[inline(always)]
pub fn call2(n: usize, a: [usize; 6]) -> Result<(usize, usize), Error> {
    // SAFETY: as in `call`.
    let (ret, rdx) = unsafe { syscall6(n, a[0], a[1], a[2], a[3], a[4], a[5]) };
    decode(ret).map(|v| (v, rdx))
}

pub fn debug_write(text: &[u8]) {
    let _ = call(nr::DEBUG_WRITE, [text.as_ptr() as usize, text.len(), 0, 0, 0, 0]);
}

pub fn handle_close(h: RawHandle) -> SysResult {
    call(nr::HANDLE_CLOSE, [h as usize, 0, 0, 0, 0, 0])
}

pub fn clock_get(id: usize) -> u64 {
    call(nr::CLOCK_GET, [id, 0, 0, 0, 0, 0]).unwrap_or(0) as u64
}

pub fn process_exit(code: i64) -> ! {
    let _ = call(nr::PROCESS_EXIT, [code as usize, 0, 0, 0, 0, 0]);
    // The kernel never returns from process_exit.
    loop {
        core::hint::spin_loop();
    }
}

pub fn thread_exit() -> ! {
    let _ = call(nr::THREAD_EXIT, [0; 6]);
    loop {
        core::hint::spin_loop();
    }
}
