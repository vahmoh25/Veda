//! `vrt` — the Vindows user-space runtime.
//!
//! Every Vindows program links this crate. It provides:
//!
//! * the process entry point and startup protocol ([`entry!`], [`env`]),
//! * safe wrappers for kernel objects and system calls ([`object`], [`vm`]),
//! * the global heap ([`heap`]), threads ([`thread`]) and synchronisation
//!   ([`sync`]), clocks ([`time`]), logging ([`println!`]),
//! * process creation from PE images ([`process`]),
//! * the few C runtime symbols compiled code relies on.
//!
//! The runtime pieces that only make sense in a real Vindows program (entry
//! point, allocator, panic handler, C symbols) are behind the default
//! `runtime` feature. Libraries depend on `vrt` with
//! `default-features = false`, which keeps them unit-testable on the host.

#![no_std]

extern crate alloc;

pub use alloc::{boxed, format, rc, string, vec};
pub use vabi;

#[cfg(feature = "runtime")]
mod crt;
pub mod env;
pub mod heap;
pub mod io;
pub mod object;
pub mod process;
#[cfg(feature = "runtime")]
pub mod start;
pub mod sync;
pub mod sys;
pub mod thread;
pub mod time;
pub mod vm;

pub use object::{Channel, Event, Handle, Interrupt, IoPorts, Message, Process, Resource, Thread, Vmo};

#[cfg(feature = "runtime")]
#[global_allocator]
static HEAP: heap::Heap = heap::Heap::new();

/// Heap usage: (bytes in use, bytes managed).
#[cfg(feature = "runtime")]
pub fn heap_stats() -> (usize, usize) {
    HEAP.stats()
}

#[cfg(feature = "runtime")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Format without allocating in case the heap is what failed.
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
    let _ = core::fmt::write(&mut b, format_args!("panic: {info}\n"));
    sys::debug_write(&b.data[..b.len]);
    sys::process_exit(vabi::EXIT_CODE_PANICKED)
}
