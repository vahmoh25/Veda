# Vindows coding conventions

These rules apply to every crate in the repository.

## Language and dependencies

* Rust 2024 edition, **stable** toolchain (tested with 1.99). No nightly
  features, no `RUSTC_BOOTSTRAP`.
* **Few, mature external crates.** The system is written in this
  repository. Where a protocol or cryptographic primitive has a mature,
  widely reviewed Rust implementation, that is used instead of a new one:
  smoltcp (TCP/IP, vendored in `third_party/` with documented patches),
  the RustCrypto crates (hashes, HMAC, PBKDF2, AES, CCM, CMAC, AES key wrap,
  P-256), `subtle`, `zeroize` and `httparse`. A new dependency must build
  `no_std` (check with `--target x86_64-unknown-uefi`), be declared in the
  workspace's `[workspace.dependencies]`, and come with a reason; avoid
  crates with large dependency trees. `core`, `alloc` and (for host tools
  only) `std` are available.
* **Rust only**, performance-critical code included (see Performance below).

## Targets

| Component | Target | Notes |
|-----------|--------|-------|
| `boot/` (UEFI loader), `kernel/` | `x86_64-unknown-uefi` | freestanding, soft-float, PE |
| user space (`lib/`, `services/`, `drivers/`, `apps/`) | `x86_64-pc-windows-msvc`, `#![no_std]` | hard-float SSE2, PE `.exe` |
| `xtask/`, host tools | host (`std`) | |

## Library crates

* Libraries are `#![no_std]` (plus `extern crate alloc;` when they allocate)
  so they can run inside Vindows **and** be unit-tested on the host.
  Tests use `#[cfg(test)] extern crate std;` — never `cfg_attr(not(test), no_std)`.
* Floating-point functions such as `sqrt`, `sin`, `floor` or `powf` are **not**
  available in `core` on stable. Use `vmath` (`use vmath::FloatExt;` gives
  `x.sqrt()`, `x.sin()`, ... on `f32`/`f64`), or small local helpers in crates
  that must not depend on `vmath`.
* Verify that a library really is `no_std`-clean with
  `cargo build -p <crate> --target x86_64-unknown-uefi`.

## Pixels and colors

* Pixels are `u32` in `0xAARRGGBB` order, i.e. bytes `B, G, R, A` in memory,
  which matches the BGRX framebuffer.
* Decoded images (`vimage`) use straight (non-premultiplied) alpha.
* Render targets in `vgfx` use **premultiplied** alpha.

## Performance

Vindows usually runs under QEMU's TCG emulator, which is roughly 5-20x
slower than native code. Hot loops (pixel processing, rasterisation, codecs)
should avoid per-pixel divisions, allocation and bounds-check-heavy indexing,
prefer integer/fixed-point arithmetic where it is natural, and process data in
cache-friendly order. TCG emulates SSE integer multiplies and shuffles with
slow helper calls, so a hot loop that LLVM auto-vectorises can become
several times slower than its scalar form: check the generated code of hot
loops and keep them scalar where needed (`scalar_loop` in
`lib/v3d/src/pipeline`). Always build the OS with optimisations (`--release`
is the default for `cargo xtask`).

## Robustness and errors

* Never panic on malformed input (files, IPC messages, user data): return a
  crate-specific error enum that implements `core::fmt::Display`.
* `unsafe` blocks carry a `// SAFETY:` comment explaining why they are sound.
* Public items have doc comments; every module starts with a `//!` overview.

## Formatting and testing

* `cargo fmt` (see `rustfmt.toml`, `max_width = 120`).
* Unit tests live next to the code (`#[cfg(test)] mod tests`). Run them with
  `cargo test -p <crate>`; `cargo xtask test` runs the full suite plus the
  QEMU integration tests.
