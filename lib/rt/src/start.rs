//! Process entry point and exit codes.

use vabi::RawHandle;

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

unsafe extern "Rust" {
    /// Defined by [`crate::entry!`] in the program crate.
    fn __vrt_main() -> i32;
}

/// The process entry point (`/ENTRY:_vrt_start`). The kernel passes the
/// bootstrap channel handle in `rdi`.
#[unsafe(no_mangle)]
pub extern "sysv64" fn _vrt_start(bootstrap: RawHandle, _arg: usize) -> ! {
    crate::env::adopt(bootstrap);
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
