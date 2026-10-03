//! Process entry point and exit codes.

use alloc::string::ToString;
use alloc::vec::Vec;

use vabi::RawHandle;
use vabi::startup::StartupView;

use crate::env::{STARTUP, Startup};
use crate::object::{Channel, Handle};

/// Values a program's `main` may return.
pub trait IntoExitCode {
    fn into_exit_code(self) -> i32;
}

impl IntoExitCode for () {
    fn into_exit_code(self) -> i32 {
        0
    }
}

impl IntoExitCode for i32 {
    fn into_exit_code(self) -> i32 {
        self
    }
}

impl<E: core::fmt::Debug> IntoExitCode for Result<(), E> {
    fn into_exit_code(self) -> i32 {
        match self {
            Ok(()) => 0,
            Err(e) => {
                crate::println!("error: {:?}", e);
                1
            }
        }
    }
}

/// Reads and decodes the startup message from the bootstrap channel.
fn read_startup(bootstrap: RawHandle) -> Startup {
    if bootstrap == vabi::INVALID_HANDLE {
        return Startup::default();
    }
    // SAFETY: the kernel passes us ownership of the bootstrap channel.
    let ch = Channel(unsafe { Handle::from_raw(bootstrap) });
    let Ok(msg) = ch.read() else { return Startup::default() };
    let Ok(view) = StartupView::parse(&msg.bytes) else { return Startup::default() };
    let args = view.args().map(|s| s.to_string()).collect();
    let env = view
        .env()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), alloc::string::String::new()),
        })
        .collect();
    let handles: Vec<(u32, Handle)> =
        msg.handles.into_iter().enumerate().map(|(i, h)| (view.role(i).unwrap_or(0), h)).collect();
    Startup { args, env, handles }
}

unsafe extern "Rust" {
    /// Defined by [`crate::entry!`] in the program crate.
    fn __vrt_main() -> i32;
}

/// The process entry point (`/ENTRY:_vrt_start`). The kernel passes the
/// bootstrap channel handle in `rdi`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn _vrt_start(bootstrap: RawHandle, _arg: usize) -> ! {
    let startup = read_startup(bootstrap);
    *STARTUP.lock() = Some(startup);
    // SAFETY: provided by the program through `vrt::entry!`.
    let code = unsafe { __vrt_main() };
    crate::sys::process_exit(code as i64)
}

/// Declares the program's `main` function:
///
/// ```ignore
/// #![no_std]
/// #![no_main]
/// vrt::entry!(main);
/// fn main() { vrt::println!("hello"); }
/// ```
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub fn __vrt_main() -> i32 {
            $crate::start::IntoExitCode::into_exit_code($main())
        }
    };
}
