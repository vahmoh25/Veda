//! Link the kernel as a fixed-address, higher-half PE image.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target != "x86_64-unknown-uefi" {
        // Building for the host (e.g. `cargo check --workspace`): nothing to do.
        return;
    }
    for arg in [
        "/ENTRY:kernel_entry",
        "/SUBSYSTEM:NATIVE",
        "/BASE:0xFFFFFFFF80000000",
        "/FIXED",
        "/DYNAMICBASE:NO",
        "/MERGE:.eh_fram=.rdata",
    ] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
    // A linker map lets the build tool produce a symbol table for backtraces.
    let out = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=/MAP:{out}/vkernel.map");
    println!("cargo:rustc-env=VKERNEL_MAP={out}/vkernel.map");
    println!("cargo:rerun-if-changed=build.rs");
}
