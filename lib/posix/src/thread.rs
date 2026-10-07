//! Threads, thread-local storage and futexes.
//!
//! musl creates threads with `__clone`, which the C library hands to
//! [`__veda_clone`]: a kernel thread in this process that starts in
//! [`__veda_thread_entry`], which installs the thread pointer and the
//! thread's exit futex and calls the thread function. When a thread ends,
//! the kernel clears its exit futex word and wakes a waiter, which is how
//! musl knows a joined thread is gone for good (`CLONE_CHILD_CLEARTID`).

use core::ffi::c_void;

use vabi::{Error, nr};
use vrt::object::{Handle, Thread};
use vrt::sys::call;

use crate::error::{self, SysResult};
use crate::linux::errno::*;
use crate::linux::{Timespec, arch, clone, futex};
use crate::{process, time, user};

/// The start of a new thread, at the top of its stack.
#[repr(C)]
struct Start {
    func: usize,
    arg: usize,
    tls: usize,
    exit_futex: usize,
}

/// A thread id for a kernel object id (positive, below 2^30: musl keeps
/// flags in the high bits of words that hold thread ids).
pub fn tid_of(koid: u64) -> i32 {
    ((koid & 0x3fff_ffff) as i32).max(1)
}

/// `__clone(func, stack, flags, arg, ptid, tls, ctid)`: a new thread in this
/// process. Processes are made with `posix_spawn` instead.
///
/// # Safety
/// The arguments are musl's: `stack` the top of a stack for the thread,
/// the pointers valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __veda_clone(
    func: usize,
    stack: usize,
    flags: i32,
    arg: usize,
    ptid: usize,
    tls: usize,
    ctid: usize,
) -> i32 {
    let flags = flags as u32;
    if flags & (clone::VM | clone::THREAD) != clone::VM | clone::THREAD || stack == 0 {
        return -(ENOSYS as i32);
    }
    let name = "pthread";
    let t = match call(nr::THREAD_CREATE, [0, name.as_ptr() as usize, name.len(), 0, 0, 0]) {
        Ok(h) => h,
        Err(e) => return -(error::kernel(e) as i32),
    };
    // SAFETY: fresh handle from the kernel.
    let thread = Thread(unsafe { Handle::from_raw(t as u32) });
    let tid = tid_of(thread.0.koid());
    if flags & clone::SETTLS != 0 {
        // A thread that ended without telling (freeing its own stack) may
        // have had this thread pointer.
        crate::signal::thread_started(tls);
    }
    let block = (stack & !15) - size_of::<Start>();
    let start = Start {
        func,
        arg,
        tls: if flags & clone::SETTLS != 0 { tls } else { 0 },
        exit_futex: if flags & clone::CHILD_CLEARTID != 0 { ctid } else { 0 },
    };
    // SAFETY: the top of the thread's stack, which is ours until it runs.
    unsafe { core::ptr::write(block as *mut Start, start) };
    // The thread may use its id as soon as it runs.
    if flags & clone::PARENT_SETTID != 0 && ptid != 0 {
        // SAFETY: musl passes where to store the id.
        unsafe { core::ptr::write_volatile(ptid as *mut i32, tid) };
    }
    if flags & clone::CHILD_SETTID != 0 && ctid != 0 {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(ctid as *mut i32, tid) };
    }
    let entry = thread_entry_address();
    match call(nr::THREAD_START, [thread.raw() as usize, entry, block, block, 0, 0]) {
        Ok(_) => tid,
        Err(e) => -(error::kernel(e) as i32),
    }
}

#[cfg(target_os = "none")]
fn thread_entry_address() -> usize {
    unsafe extern "C" {
        fn __veda_thread_entry();
    }
    __veda_thread_entry as *const () as usize
}

#[cfg(not(target_os = "none"))]
fn thread_entry_address() -> usize {
    0
}

// A new thread starts here with rdi = rsp = its `Start`.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    ".global __veda_thread_entry",
    ".hidden __veda_thread_entry",
    ".type __veda_thread_entry, @function",
    "__veda_thread_entry:",
    "    mov rbx, rdi",
    "    mov rdi, [rbx + 16]",
    "    mov eax, {set_fs}",
    "    syscall",
    "    mov rdi, [rbx + 24]",
    "    mov eax, {set_exit_futex}",
    "    syscall",
    "    mov rdi, [rbx + 8]",
    "    xor ebp, ebp",
    "    call [rbx]",
    "    mov eax, {exit}",
    "    syscall",
    "    ud2",
    ".size __veda_thread_entry, . - __veda_thread_entry",
    set_fs = const nr::THREAD_SET_FS_BASE,
    set_exit_futex = const nr::THREAD_SET_EXIT_FUTEX,
    exit = const nr::THREAD_EXIT,
);

// `__unmapself(base, size)`: frees the stack the thread runs on and ends
// the thread, touching no stack in between.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    ".global __veda_unmapself",
    ".hidden __veda_unmapself",
    ".type __veda_unmapself, @function",
    "__veda_unmapself:",
    "    mov rdx, rsi",
    "    mov rsi, rdi",
    "    xor edi, edi",
    "    mov eax, {unmap}",
    "    syscall",
    "    mov eax, {exit}",
    "    syscall",
    "    ud2",
    ".size __veda_unmapself, . - __veda_unmapself",
    unmap = const nr::VM_UNMAP,
    exit = const nr::THREAD_EXIT,
);

/// `__set_thread_area(p)`: the calling thread's thread pointer.
#[unsafe(no_mangle)]
pub extern "C" fn __veda_set_thread_area(p: *mut c_void) -> i32 {
    match call(nr::THREAD_SET_FS_BASE, [p as usize, 0, 0, 0, 0, 0]) {
        Ok(_) => 0,
        Err(e) => -(error::kernel(e) as i32),
    }
}

pub fn arch_prctl(code: u32, addr: usize) -> SysResult {
    match code {
        arch::SET_FS => call(nr::THREAD_SET_FS_BASE, [addr, 0, 0, 0, 0, 0]).map(|_| 0).map_err(error::kernel),
        _ => Err(EINVAL),
    }
}

/// `set_tid_address`: the word the kernel clears when the calling thread
/// ends. musl calls it once, on the main thread, whose id is the process
/// id.
pub fn set_tid_address(addr: usize) -> SysResult {
    call(nr::THREAD_SET_EXIT_FUTEX, [addr, 0, 0, 0, 0, 0]).map_err(error::kernel)?;
    Ok(process::pid() as usize)
}

/// Ends the calling thread.
pub fn exit_thread() -> ! {
    crate::signal::thread_exited();
    vrt::sys::thread_exit()
}

/// `futex(addr, op, val, timeout, addr2, val3)`.
pub unsafe fn futex(addr: usize, op: u32, val: u32, timeout: usize, val2: usize, val3: u32) -> SysResult {
    let cmd = op & futex::CMD_MASK;
    match cmd {
        futex::WAIT | futex::WAIT_BITSET => {
            let deadline = if timeout == 0 {
                vabi::DEADLINE_INFINITE
            } else {
                // SAFETY: the program passed a timespec.
                let t: Timespec = unsafe { user::read(timeout)? };
                let ns = t.to_ns().ok_or(EINVAL)?;
                if cmd == futex::WAIT {
                    time::monotonic_ns().saturating_add(ns)
                } else if op & futex::CLOCK_REALTIME != 0 {
                    time::monotonic_ns().saturating_add(ns.saturating_sub(time::realtime_ns()))
                } else {
                    ns
                }
            };
            if cmd == futex::WAIT_BITSET && val3 == 0 {
                return Err(EINVAL);
            }
            match call(nr::FUTEX_WAIT, [addr, val as usize, deadline as usize, 0, 0, 0]) {
                Ok(_) => Ok(0),
                Err(Error::ShouldWait) => Err(EAGAIN),
                Err(Error::TimedOut) => Err(ETIMEDOUT),
                Err(Error::Canceled) => Err(EINTR),
                Err(e) => Err(error::kernel(e)),
            }
        }
        futex::WAKE | futex::WAKE_BITSET => {
            call(nr::FUTEX_WAKE, [addr, val as usize, 0, 0, 0, 0]).map_err(error::kernel)
        }
        // Requeueing waiters onto another word is an optimisation: waking
        // them (who then wait on the other word themselves) is correct.
        futex::REQUEUE | futex::CMP_REQUEUE => {
            if cmd == futex::CMP_REQUEUE {
                // SAFETY: the futex word is the program's.
                let now = unsafe { core::ptr::read_volatile(addr as *const u32) };
                if now != val3 {
                    return Err(EAGAIN);
                }
            }
            let n = (val as usize).saturating_add(timeout.min(i32::MAX as usize));
            call(nr::FUTEX_WAKE, [addr, n, 0, 0, 0, 0]).map_err(error::kernel)
        }
        _ => {
            let _ = val2;
            Err(ENOSYS)
        }
    }
}
