# C on Veda

Veda runs C programs and builds them itself: the system image comes with
GCC 16, GNU binutils and the musl C library, so a C program can be written,
compiled and run inside Veda — in an emulator or on a PC booted from a USB
stick alike. The same toolchain, built to run on the development machine,
cross-compiles C and C++ programs for Veda.

This document covers using it, how it works, and how it is built. The
goal is a standard environment for porting software written in C (and,
later, C++): browser engines, drivers, tools.

## Writing, compiling and running a program

In the Terminal:

```text
user@veda:~$ cd /tmp
user@veda:/tmp$ echo '#include <stdio.h>' > hello.c
user@veda:/tmp$ echo 'int main(void) { printf("Hello from Veda\n"); return 0; }' >> hello.c
user@veda:/tmp$ gcc -O2 -Wall -o hello hello.c
user@veda:/tmp$ ./hello
Hello from Veda
user@veda:/tmp$ echo $?
0
```

The Text Editor writes longer programs as well (save them in your home
folder: `/home` is kept across restarts when Veda has its home disk, and in
memory on the live system).

* `gcc` (also `cc`) compiles and links. Programs are linked statically: the
  executable holds what it uses of the C library and needs nothing else
  to run. `-static-pie`, with the code compiled `-fPIE`, makes a
  position-independent executable instead.
* `as`, `ld`, `ar`, `nm`, `objdump`, `objcopy`, `strip`, `readelf`, `size`,
  `strings`, `addr2line` and `c++filt` are GNU binutils.
* The headers are in `/system/include`, the C library (`libc.a`, `libm.a`,
  ...) and its start files in `/system/lib`, GCC's own files in
  `/system/lib/gcc` and `/system/libexec/gcc`.
* `__veda__` and `__unix__` are defined; `__linux__` is not. The C
  library's interface is musl's, which is Linux's: code that builds on
  Linux with musl generally builds here, except where it relies on what
  is listed under [Limits](#limits).

A program runs in the Terminal like any command: found by its path
(`./hello`) or on `PATH` (`/system/bin`). Its standard input, output and
error are the terminal: `isatty` is true and `TIOCGWINSZ` gives the
window's size. Standard output is line-buffered there, as everywhere: a
prompt without a newline shows once the program calls `fflush(stdout)`
(musl, unlike glibc, does not flush it when the program reads its input).
The shell waits for the program and `$?` is its exit status (128 plus the
signal for a program that was ended: 130 after Ctrl+C, 134 after `abort`,
139 after a crash). Keys typed while it runs go to it a line at a time,
edited in the window, as on a Unix terminal in canonical mode: a read
returns one line at most, and lines typed ahead stay for whoever reads
next — the program, a program it started, or the shell once it has ended.
A program can turn that off with `termios` and read keys one by one
(`VMIN` and `VTIME` work as POSIX says). Ctrl+C ends it, with the programs
it started; Ctrl+D ends its input. `>`, `>>` and `<` give it files, `|`
connects it with other programs and built-in commands.

## How it works

```text
 C program ── musl (libc.a) ── POSIX layer (lib/posix, in libc.a) ── Veda's kernel and services
```

**Executables** are ELF64 files for the System V x86-64 ABI, as on Linux.
Veda's own (Rust) programs stay PE; `vrt::process::Spawn` starts either
kind. For an ELF program it maps the segments (`lib/elf` validates them and
groups the pages by permissions), gives it an 8 MiB stack with guard pages
below, and lays out the top of the stack as Linux does: arguments,
environment and the auxiliary vector (program headers, entry point, page
size, 16 random bytes, the hardware capabilities). It also sends the
startup message every Veda process gets — for an ELF program, the working
directory and the handles: the registry, the terminal, and the files and
streams that become its descriptors (role `FD + n`).

**The C library** is musl 1.2.6 (`ports/musl`). It is Linux's C library
almost unchanged: where it would make a Linux system call, it calls
`__veda_syscall`, and a handful of entry points that cannot be C (thread
creation, the thread pointer, a thread freeing its own stack) jump into
the POSIX layer. `_start` stores the bootstrap channel the kernel passes,
and `__init_libc` calls the POSIX layer to set itself up before any
constructor runs. `posix_spawn` is Veda's own (there is no `fork`).

**The POSIX layer** (`lib/posix`, crate `vposix`) is written in Rust and
carries out Linux system calls with Veda's objects and services:

| What a C program uses | What it is on Veda |
|-----------------------|--------------------|
| files | the VFS's open files: each a connection of the `file` protocol, the offset kept by the VFS (`dup` and child processes share it); files removed while open stay readable |
| directories | the VFS's listings (`getdents64`), `.` and `..` included |
| `/dev/null`, `zero`, `full`, `random`, `urandom` | devices of the VFS |
| `/dev/tty`, `/dev/stdin`, `/dev/fd/N` | the program's own descriptors |
| pipes, the terminal | socket endpoints: kernel byte streams that several processes can share |
| `isatty`, window size, `termios` | the terminal's shared state page (`vproto::tty`) |
| `mmap`, `mprotect`, `munmap`, `madvise` | private anonymous memory is the kernel's private memory, whose pages are freed as soon as they are unmapped (parts of mappings too) or dropped with `MADV_DONTNEED`; files are mapped as private copies; any part of a mapping can be protected or unmapped |
| threads (`pthread_create`) | kernel threads; the thread pointer in `FS`; `pthread_join` through the thread's exit futex |
| futexes | kernel futexes |
| `posix_spawn`, `waitpid` | processes started from the program image, with the descriptors the file actions leave |
| signals | `raise`, `abort`, `SIGPIPE` and handlers, per-thread masks |
| clocks, sleeping | the kernel's clocks |
| `stat` | the VFS's metadata: inode numbers, times, and "executable" for programs |

The layer is linked into `libc.a` as one object that exports these few
symbols and nothing else, so no symbol of the Rust code (or of its
compiler-builtins) can clash with the C library's. Its memory comes from
its own heap (`vrt`'s), not from `malloc`, which calls into it.

**Veda's own interfaces** are there too, for C programs that are Veda
services or talk to them directly (`<veda/ipc.h>`): handles, channels and
the service registry (`veda_service_register`, `_accept`, `_connect`),
waiting on several objects at once (`veda_wait`), memory objects
(`veda_vmo_create`, `_map`) and events. They are the kernel's calls but for
the registry's, made by the POSIX layer (`lib/posix/src/native.rs`); every
call returns 0 or a count, or a negative Veda error number. A program
speaks a protocol by writing its messages itself: a 12-byte header (method,
transaction, request or response), then the values in order, as the
header describes. Veda's renderer (`services/renderer/src/main.c`) serves
the `gpu` protocol so.

**The GPU's render node**, `/dev/dri/renderD128`, is Linux's i915
interface, as Mesa's iris driver uses it: its ioctls, buffers mapped with
`mmap`, and syncobjs (`lib/posix/src/drm.rs`; `tests/c/drm.c` checks it).
It opens where a GPU driver serves the `gem` protocol, and is not there
otherwise (`ENOENT`).

**The Terminal** runs a program on a fresh socket pair: the program's
descriptors 0, 1 and 2 are one end, the window keeps the other. The
terminal's state page tells programs which socket is the terminal, its
size and its modes (`ICANON`, `ECHO`, `ISIG`), which the Terminal follows
when keys are typed. Programs a program starts inherit all of it.

**The kernel** has what a POSIX environment needs: sockets (byte streams
with shareable endpoints, a bounded buffer, all-or-nothing writes up to
4096 bytes, and half-closing for the end of input), memory protection of
parts of mappings that never exceeds what the VMO's handle allows, private
memory whose pages can be freed while it stays mapped (what allocators do
with `madvise`), and each thread's exit futex (a word the kernel clears,
and wakes a waiter on, once the thread has left its stack).

## Limits

* **No `fork` or `exec`.** Processes are started with `posix_spawn`.
  Software that forks needs porting to it — most build tools and browsers
  already use `posix_spawn` or `vfork`+`exec`, which ports to it directly.
  A started program inherits at most 62 descriptors (more fail with
  `EMFILE`): open files that children need not have with `O_CLOEXEC`.
* **No `/bin/sh` yet.** The Terminal's shell is part of the Terminal, so
  `system` and `popen`, which run their command with `/bin/sh -c`, fail
  with `ENOENT`.
* **Signals are synchronous.** A program gets the signals it raises
  itself (`raise`, `abort`, `SIGPIPE`). Other processes end a program
  instead of signalling it (`kill` does that to children); `alarm` and
  interval timers are not available.
* **No links.** Veda's file system has neither hard nor symbolic links;
  `link` and `symlink` fail, `readlink` finds none.
* **No permissions.** Files have no owners or permission bits: a program
  file (ELF, PE or `#!`) is executable, everything else is not; `chmod` and
  `chown` change nothing. Programs run as user 1000.
* **Static linking only**: no shared libraries, no `dlopen`.
* **No network sockets yet** (`socket` fails with `EAFNOSUPPORT`).
  `socketpair` works, with `send`, `recv`, `sendmsg`, `recvmsg` and
  `shutdown`, but without `MSG_PEEK`, out-of-band data or passing
  descriptors.
* **C only inside Veda, for now**: the image has no C++ compiler. The cross
  toolchain compiles C++ for Veda, with libstdc++.
* **The Terminal shows text with colours**, with carriage returns and line
  erasing, but no cursor positioning: full-screen programs (editors) do
  not display correctly yet.

## The toolchain

`cargo xtask toolchain` builds it from the sources in `ports/`:

1. downloads the source archives and checks them against the SHA-256 each
   `ports/*/port.toml` pins (their GPG signatures were checked against the
   GNU and musl keys when they were pinned);
2. builds the POSIX layer for `x86_64-unknown-none`;
3. runs `ports/build.sh` (under MSYS2 on Windows) to unpack and patch the
   sources and build, in `target/toolchain`:
   * `cross/` — the cross toolchain for this machine: `x86_64-veda-gcc`,
     `x86_64-veda-g++` with the C++ library (libstdc++), and the binutils,
     with a sysroot laid out like Veda's `/system`. GCC is written in C++,
     so the C++ compiler is what builds the native GCC; programs in C++
     can use it too;
   * `native/system/` — the toolchain built to run inside Veda: GCC (built
     on this machine, to run on Veda, making programs for Veda), binutils,
     GMP, MPFR and MPC, the C library and its headers.

The first build takes about an hour and a half on a recent PC; later ones
rebuild what changed (each stage keeps a fingerprint of what went into it,
and where: a toolchain directory that has moved is built again). When the
POSIX layer changes, `cargo xtask build` puts the new one into the C
library, and links the native programs again with it (GCC, binutils: they
are C programs too), which recompiles nothing.

`cargo xtask build` installs the native toolchain in the image (in
`/system`) and the C test programs in `/system/tests/c` when the
toolchain exists, and says so when it does not. The image grows by about
80 MB with it, which Veda keeps in memory; compiling is comfortable in
QEMU's default 1 GiB.

On Windows the build needs MSYS2 (`C:\msys64`, or `VEDA_MSYS2`) with:

```text
pacman -S make m4 bison flex texinfo diffutils patch mingw-w64-ucrt-x86_64-gcc
pacman -S meson ninja python-mako python-yaml python-packaging
```

The second line is for Mesa (the renderer's Gallium; `ports/mesa`), whose
build is meson's. The cross toolchain's programs are linked statically and
run without MSYS2.

### The ports

| Port | Version | What changes for Veda |
|------|---------|-----------------------|
| `binutils` | 2.47 | the `x86_64-veda` target (ELF, `elf_x86_64`, separate code pages) |
| `gcc` | 16.2.0 | the `x86_64-veda` target: `gcc/config/veda.h` and `i386/veda.h` (musl, static linking, `/system`), `veda.opt`, libgcc's configuration, libstdc++'s checks of the C library |
| `gmp`, `mpfr`, `mpc` | 6.3.0, 4.2.2, 1.4.1 | `config.sub` knows Veda |
| `musl` | 1.2.6 | system calls through the POSIX layer, `_start`, thread entry points, `posix_spawn`, the call to set the layer up |

Each port is a directory with `port.toml` (version, address, SHA-256,
licence) and `veda.patch`; `ports/README.md` describes how to change one.

### Licences

GCC, binutils and the GCC runtime library are under the GNU GPL version 3
(libgcc with the GCC Runtime Library Exception, which leaves programs GCC
builds under their own licences); GMP, MPFR and MPC under the GNU LGPL
version 3; musl under the MIT licence. The source of what Veda's image
contains is the upstream archive each `port.toml` names plus the patch
beside it.

## Tests

* `tests/c/posix.c` checks the C library and the POSIX layer — the
  startup, standard I/O, the heap, `mmap` and `madvise`, files,
  directories, pipes, threads and TLS, clocks, signals, `posix_spawn` and
  wait statuses — and `tests/c/cxx.cc` the C++ library: exceptions (across
  threads too), run-time types, static constructors, containers, streams,
  threads. `systest` runs them inside Veda with `tests/c/hello.c`, each
  linked both at a fixed address and position-independent, and tests the
  kernel's sockets, memory protection, private memory and exit futexes and
  the VFS's open files.
* `tests/ui/c-programs.vts` runs programs in the Terminal: exit statuses,
  an interactive program (`tests/c/greet.c`), Ctrl+C and Ctrl+D, pipes and
  files.
* `tests/ui/c-compile.vts` writes a C program in the Terminal, compiles it
  with GCC inside Veda and runs it.
* `lib/elf` (the ELF loader's checks and the stack layout) and `lib/posix`
  (paths, structures, flag conversions) are unit-tested on the host.

`cargo xtask test --ui` runs them; the scripts that need the toolchain are
skipped when it has not been built.
