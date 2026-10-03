# Vindows architecture

Vindows is an x86-64 operating system built around a capability-based
microkernel. The kernel implements only isolation, scheduling, memory
management and inter-process communication; device drivers, file systems,
the window system and every application are ordinary user-space processes
that communicate over kernel channels.

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ Applications   shell · editor · photos · music · terminal · games …      │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Libraries      vui (widgets) · vgfx (2D) · v3d (3D) · vfont · vimage …   │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Services       init (registry, launcher) · vfs · compositor · audio      │
 │ Drivers        ps2 · virtio-input · virtio-snd · virtio-blk · pci        │
 ├──────────────── channels · VMOs · events · interrupts ───────────────────┤
 │ vkernel        scheduler · address spaces · handles · IPC · interrupts   │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ vboot (UEFI)   loads kernel + initrd, sets graphics mode, builds paging  │
 └──────────────────────────────────────────────────────────────────────────┘
```

## Boot

1. OVMF loads `\EFI\BOOT\BOOTX64.EFI` (`boot/`, the `vboot` loader) from the
   FAT32 EFI system partition built by `cargo xtask build`.
2. `vboot` reads `\VINDOWS\BOOT.CFG`, `VKERNEL.EXE` and `INITRD.IMG`, picks a
   GOP graphics mode, paints the splash screen, loads the kernel's PE
   sections, builds page tables (identity map, direct map at
   `0xFFFF800000000000`, kernel at `0xFFFFFFFF80000000`), exits boot services
   and jumps to the kernel with a `bootinfo::BootInfo`.
3. The kernel initialises memory, ACPI, APICs, timers and the other CPUs, then
   starts `bin/init.exe` from the initrd (the only program it loads itself).
4. `init` starts the system services and the desktop shell.

## The kernel (`kernel/`)

| Module | Responsibility |
|--------|----------------|
| `arch` | GDT/TSS, IDT and entry paths, x2APIC/xAPIC and I/O APIC, SMP bring-up, TLB shootdowns |
| `mm` | frame allocator, page tables, kernel heap, kernel stacks/MMIO windows, VMOs, address spaces, user copies |
| `sched` | threads, priority round-robin scheduling, blocking with timeouts, idle loop |
| `object` | handles, signals, processes, channels, events, interrupts, I/O ports, resources |
| `syscall` | the system call layer (see `lib/abi`) |
| `acpi`, `time`, `futex`, `loader`, `log`, `panic` | supporting subsystems |

**Concurrency.** The kernel runs with interrupts disabled and holds a big
kernel lock (BKL) whenever it touches shared state, so kernel code is
effectively single-threaded while user code runs on all CPUs in parallel.
Kernel operations are short, which keeps contention low. Per-object spinlocks
provide interior mutability (and catch accidental recursion); TLB shootdowns
are acknowledged without the BKL so they cannot deadlock.

**Objects and capabilities.** Everything user space can touch is a kernel
object reached through a per-process handle with rights (`vabi::Rights`).
There is no ambient authority: a process can only use the objects it was
given. Hardware access requires *resource* handles: `init` receives the root
resource and derives narrow ones (an I/O port range, one IRQ, one MMIO range)
for each driver.

**Memory.** VMOs are the unit of memory. Anonymous VMOs are committed lazily
on page faults. Address spaces map VMOs with per-mapping permissions; the
kernel half of every address space is shared.

**IPC.** Channels carry messages of up to 64 KiB plus up to 64 handles.
Bulk data (window buffers, audio rings, file contents) travels in shared
VMOs. Threads wait on signals of several objects at once
(`object_wait_many`).

**Scheduling.** 32 priorities, round-robin within a priority, 10 ms slices,
preemption on wake-up of a higher-priority thread, tickless one-shot timers,
eager FPU/SSE/AVX state switching with XSAVE.

## User space

* **Executables** are PE32+ images (built for `x86_64-pc-windows-msvc` with
  `#![no_std]`, linked at `0x140000000` with no imports), so C++ compiled by
  MSVC links straight in.
* **`vrt`** is the runtime every program links: entry point and startup
  message, syscall wrappers, heap (TLSF), threads, futex-based locks, time,
  logging and process creation.
* **Process creation** happens in user space: create a process, map the PE
  sections from a VMO, create a thread, and start it with a bootstrap channel
  carrying the startup message (arguments, environment, role-tagged handles).
* **Services** register with the registry in `init`; clients connect by name.
  Protocols are declared with the `vipc` macros, which generate typed client
  stubs and server dispatch code.

## Repository layout

| Path | Contents |
|------|----------|
| `boot/` | UEFI loader |
| `kernel/` | microkernel |
| `lib/abi`, `lib/bootinfo`, `lib/initrd`, `lib/pe` | shared formats and the kernel ABI |
| `lib/rt`, `lib/heap`, `lib/build` | user runtime, allocator, build helper |
| `lib/math`, `lib/raster`, `lib/font`, `lib/image` | math, vector rasterisation, fonts, image codecs |
| `services/`, `drivers/`, `apps/` | system services, drivers and applications |
| `xtask/` | build orchestration, disk image creation, QEMU automation |
| `assets/` | fonts and other data shipped in the initrd |
| `docs/` | documentation |
