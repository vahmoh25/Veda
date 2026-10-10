# Ports

Third-party software Veda builds from source: the C toolchain (see
[docs/C.md](../docs/C.md)) and Mesa, for the renderer. Veda's own code is
Rust; what is written in C
comes as a port — the upstream release, unchanged but for a patch that is
kept small, readable and explained.

Each port is a directory:

* `port.toml` — the name, version, download address, the SHA-256 of the
  archive and the licence. `cargo xtask toolchain` downloads the archive
  and refuses one whose hash differs.
* `veda.patch` — the changes for Veda, applied with `patch -p1` to the
  unpacked archive.

`build.sh` builds them all (`cargo xtask toolchain` runs it; the
comments at its top list what it expects and makes).

## The patches

**binutils** — `x86_64-veda` in `config.sub`, BFD (`x86_64_elf64_vec`),
gas (ELF) and ld (`elf_x86_64`, with code on pages of its own, as on
Linux).

**gcc** — `x86_64-veda` in `config.sub`; `gcc/config/veda.h` (the system's
macros, `/system/lib` for start files and libraries, static linking:
`crt1.o` or, with `-static-pie`, `rcrt1.o`; musl's type definitions),
`gcc/config/i386/veda.h` (the System V x86-64 ABI, the stack protector's
canary where musl keeps it), `gcc/config/veda.opt` (`-pthread`,
`-rdynamic`, `-posix`; and `veda.opt.urls`, its links to GCC's manual),
their entries in `gcc/config.gcc`, libgcc's configuration in
`libgcc/config.host`, no `fixincludes` for musl's headers, and libstdc++
checking Veda's C library as it checks Linux's when cross compiled
(`libstdc++-v3/crossconfig.m4`, and the same line in the `configure`
generated from it).

**gmp, mpfr, mpc** — `config.sub` knows `veda`, so they build for Veda as
the native GCC's libraries.

**musl** — system calls go to the POSIX layer (`lib/posix`) instead of
the `syscall` instruction (`arch/x86_64/syscall_arch.h`); `_start` keeps
the bootstrap channel (`crt_arch.h`); thread creation, the thread pointer,
a thread freeing its own stack and cancellable system calls go through
the layer (`src/thread/x86_64/*.s`); `vfork` falls back to the generic
version (which fails: there is no `fork`); `__init_libc` sets the layer
up; `posix_spawn` asks the layer to start the program.

**mesa** — Veda's renderer (`services/renderer`), which carries out the
OpenGL ES command streams of applications on Mesa's Gallium drivers, is a
target of Mesa's build: `src/gallium/targets/veda`, compiling the sources
the `veda-renderer` option names, makes the renderer service for Veda and,
for the machine that builds it, `vgallium.so` (the renderer on softpipe)
for the OpenGL ES tests. Gallium's own state
handling is built without any OpenGL API (`with_gfx_compute`), and
`src/util/detect_os.h` counts Veda as Linux: its POSIX layer carries out
Linux's system calls. iris, Intel's driver, is built with the renderer's
small libdrm (`services/renderer/drm`) and without what needs Mesa's
OpenCL compiler (LLVM): generating indirect draws on the GPU, and blorp's
indirect copies (its bounds check is written in NIR instead). `build.sh`
configures Mesa (meson); `cargo xtask` builds the renderer (ninja) with
the system.

## Updating a port

1. Download the new release and its signature; check the signature
   against the project's keys (GNU: the GNU keyring; musl: its key on
   musl.libc.org).
2. Put the new version, address and SHA-256 into `port.toml`.
3. Unpack the release, apply the old `veda.patch` and resolve what no
   longer applies; then make the patch again (`diff -ruN a b` against the
   pristine release, paths starting at the release's top directory).
4. `cargo xtask toolchain` rebuilds what changed; then `cargo xtask test`.

## Licences

The ports keep their licences: GCC and binutils GPL-3.0-or-later (libgcc
with the GCC Runtime Library Exception), GMP, MPFR and MPC
LGPL-3.0-or-later, musl MIT, and the parts of Mesa the renderer uses MIT.
Distributing Veda's image, which contains them, means offering their
source: the archives `port.toml` names and the patches here.
