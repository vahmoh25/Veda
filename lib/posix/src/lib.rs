//! `vposix` — Veda's POSIX layer.
//!
//! C programs on Veda use musl, a C library written for Linux, with one
//! change: where musl would make a Linux system call, it calls
//! [`__veda_syscall`], which carries the call out with Veda's kernel objects
//! and services. The rest of musl — stdio, strings, math, locales, threads —
//! runs unchanged.
//!
//! | Linux | Veda |
//! |-------|------|
//! | files, directories | the VFS service: a `file` connection per open file ([`file`]) |
//! | pipes, `socketpair`, the terminal | socket endpoints ([`stream`], [`socket`], [`tty`]) |
//! | `mmap` | the kernel's private memory, or a VMO per mapping ([`mem`]) |
//! | threads, TLS, futexes | kernel threads, `FS` base, kernel futexes ([`thread`]) |
//! | `fork` + `exec` | `posix_spawn`, which loads the program directly ([`process`]) |
//! | signals | synchronous delivery (`raise`, `abort`, `SIGPIPE`) ([`signal`]) |
//! | the GPU's render node (i915's ioctls) | the GPU's driver, over the GEM protocol ([`drm`]) |
//!
//! musl's hooks into this layer are:
//!
//! * `_start` stores the bootstrap channel the kernel passed in `rdi` in
//!   [`__veda_bootstrap`]; `__init_libc` then calls [`__veda_init`], which
//!   reads the startup message: the working directory, the descriptors the
//!   program was given (role `FD + n`), the terminal and the registry;
//! * `__veda_syscall(n, a1, ..., a6)` replaces the `syscall` instruction;
//! * `__clone`, `__set_thread_area` and `__unmapself` jump to
//!   [`thread::__veda_clone`], [`thread::__veda_set_thread_area`] and
//!   `__veda_unmapself`;
//! * `posix_spawn` calls [`process::__veda_spawn`].
//!
//! The layer is built as a static library for `x86_64-unknown-none` and
//! linked into musl's `libc.a` as one object that exports only these
//! symbols. Its own memory comes from `vrt`'s heap, not from `malloc`.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod drm;
mod error;
mod fd;
mod file;
mod fs;
mod io;
mod linux;
mod mem;
mod native;
mod path;
mod process;
mod signal;
mod socket;
mod stream;
mod syscall;
mod system;
mod thread;
mod time;
mod tty;
mod user;
mod vfs;

use core::sync::atomic::{AtomicU32, Ordering};

use vabi::startup::role;
use vrt::object::Vmo;

/// The bootstrap channel the kernel passed the process, stored by `_start`.
#[unsafe(no_mangle)]
pub static __veda_bootstrap: AtomicU32 = AtomicU32::new(0);

/// Sets the layer up from the startup message (called by `__init_libc`,
/// once the thread pointer is set, before any constructor runs).
#[unsafe(no_mangle)]
pub extern "C" fn __veda_init() {
    // Thread-local storage is set up: threads can be told apart.
    signal::threads_ready();
    vrt::env::adopt(__veda_bootstrap.swap(0, Ordering::Relaxed));
    vfs::set_cwd(vrt::env::cwd());
    if let Some(h) = vrt::env::take_handle(role::TERMINAL) {
        tty::adopt(Vmo::from_handle(h));
    }
    io::adopt_startup_fds();
}

/// A Linux system call. Returns its result, or `-errno`.
///
/// # Safety
/// The arguments are those of the Linux system call `n`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __veda_syscall(n: isize, a: isize, b: isize, c: isize, d: isize, e: isize, f: isize) -> isize {
    let args = [a, b, c, d, e, f].map(|v| v as usize);
    // SAFETY: per the caller.
    error::encode(unsafe { syscall::dispatch(n as usize, args) })
}

/// The layer's own memory (separate from `malloc`, which calls into the
/// layer for its memory).
#[cfg(target_os = "none")]
#[global_allocator]
static HEAP: vrt::heap::Heap = vrt::heap::Heap::new();

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Formatted without allocating, in case the heap is what failed.
    struct Buf {
        data: [u8; 512],
        len: usize,
    }
    impl core::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.data.len() - self.len);
            self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    let mut b = Buf { data: [0; 512], len: 0 };
    let _ = core::fmt::write(&mut b, format_args!("POSIX layer panic: {info}\n"));
    vrt::sys::debug_write(&b.data[..b.len]);
    process::exit_by_signal(linux::sig::ABRT)
}
