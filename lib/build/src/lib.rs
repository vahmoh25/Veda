//! Helpers for the `build.rs` scripts of Veda user-space crates:
//! [`user_program`] emits the linker options that turn a `no_std` binary
//! into a Veda PE executable (fixed base, custom entry point, no CRT), in
//! the Microsoft linker's syntax, which LLVM's linker (`rust-lld`) takes.

/// Image base of every Veda executable.
pub const IMAGE_BASE: &str = "0x140000000";

fn is_veda_target() -> bool {
    std::env::var("TARGET").is_ok_and(|t| t == "x86_64-pc-windows-msvc")
}

/// Emits the linker arguments for a Veda user-space executable. Call this
/// from the `build.rs` of every program crate.
pub fn user_program() {
    println!("cargo:rerun-if-changed=build.rs");
    if !is_veda_target() {
        return;
    }
    for arg in [
        "/NODEFAULTLIB",
        "/ENTRY:_vrt_start",
        "/SUBSYSTEM:NATIVE",
        &format!("/BASE:{IMAGE_BASE}"),
        "/FIXED",
        "/DYNAMICBASE:NO",
        "/INCREMENTAL:NO",
        "/MANIFEST:NO",
    ] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}
