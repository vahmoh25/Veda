# Veda architecture

Veda is an x86-64 operating system built around a capability-based
microkernel. The kernel implements only isolation, scheduling, memory
management and inter-process communication; device drivers, file systems,
the window system and every application are ordinary user-space processes
that communicate over kernel channels.

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ Applications   shell · editor · photos · music · terminal · games …      │
 │                C programs: gcc · as · ld … (musl, vposix)                │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Libraries      vui (widgets) · vgfx (2D) · vgl (OpenGL ES) · v3d …       │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Services       init (registry, launcher) · vfs · compositor · audio ·    │
 │                agent (the voice agent) · netd · wlan                     │
 │ Drivers        virtio-blk · ahci · nvme (disks) · pci · the driver VM    │
 │                (Linux's: GPUs, displays, input, USB, networks, Wi-Fi,    │
 │                sound …)                                                  │
 ├──────────────── channels · VMOs · events · interrupts ───────────────────┤
 │ vkernel        scheduler · address spaces · handles · IPC · interrupts   │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ vboot (UEFI)   loads kernel + initrd, sets graphics mode, builds paging  │
 └──────────────────────────────────────────────────────────────────────────┘
```

## Boot

1. OVMF loads `\EFI\BOOT\BOOTX64.EFI` (`boot/`, the `vboot` loader) from the
   FAT32 EFI system partition built by `cargo xtask build`.
2. `vboot` reads `\VEDA\BOOT.CFG`, `VKERNEL.EXE` and `INITRD.IMG`, picks a
   GOP graphics mode, paints the splash screen (`vsplash`), loads the
   kernel's PE sections, builds page tables (identity map, direct map at
   `0xFFFF800000000000`, kernel at `0xFFFFFFFF80000000`), exits boot services
   and jumps to the kernel with a `bootinfo::BootInfo`. Firmware usually
   leaves its framebuffer uncached, where every write is a bus transaction
   of its own and a PC's screen shows the picture being painted from the
   top down; so the loader paints through page tables of its own that map
   the framebuffer write-combining by its page attributes (which the
   firmware's MTRRs cannot overrule), and the picture is there at once.
   The kernel logs how it went (`boot: the loader painted its splash in
   ...`).
3. The kernel initialises memory, ACPI, APICs, timers and the other CPUs, then
   starts `bin/init.exe` from the initrd (the only program it loads itself).
4. `init` starts the system services and the desktop shell, then supervises
   them: the window system, the shell, audio, the network and Wi-Fi services
   and the agent are restarted if they crash (at most three times a
   minute). Drivers reconnect to a
   restarted compositor through the registry, which queues connections
   until a service registers again.

**The startup sequence.** The loader's splash stays on the screen while the
kernel and the services start. The window system takes it over without a
seam, drawing the same picture through `vsplash` (`lib/splash`) pixel for
pixel, and brings it to life (`services/compositor/src/startup.rs`): a soft
light gathers around the ring and breathes, with a glint going round while
the system works, and the name and the tagline ("The agentic-native
operating system") rise into view. Once the shell's desktop and taskbar
have each presented a frame, and the splash has been up for at least
1.8 s, the ring swells and fades, the words lift away and the desktop
dissolves in, in under a second (`compositor: desktop shown after N ms`
says when, and at what frame rate). Meanwhile the shell plays the startup
sound, `/system/sounds/startup.wav` (composed by `tools/musicgen`), as soon
as a sound device is attached. `init` asks for the sequence (`splash`) and
the sound (`startup`) only at system start: a window system or shell
restarted after a crash shows the desktop at once, and quietly.

The live system (`cargo xtask iso`) starts the same way from a USB stick or
a disc. Its image is a hybrid ISO 9660 image: the boot files in the ISO 9660
tree (what Rufus copies onto a stick), and a FAT file system with the same
files, which is both the El Torito UEFI boot image and, through an MBR
partition table in the system area, the EFI system partition of a stick the
image is written to as it is. Its `BOOT.CFG` puts `live` on the kernel
command line; `init` passes it on to `devmgr`, which then starts no disk
driver, and to `vfs`, which keeps `/home` in memory, so the computer's disks
are never read or written.

## The kernel (`kernel/`)

| Module | Responsibility |
|--------|----------------|
| `arch` | GDT/TSS, IDT and entry paths, x2APIC/xAPIC and I/O APIC, SMP bring-up, TLB shootdowns |
| `mm` | frame allocator, page tables, kernel heap, kernel stacks/MMIO windows, VMOs, address spaces, user copies |
| `sched` | threads, priority round-robin scheduling, blocking with timeouts, idle loop |
| `object` | handles, signals, processes, channels, sockets, events, interrupts, I/O ports, resources |
| `syscall` | the system call layer (see `lib/abi`) |
| `hv` | the hypervisor: VMX and EPT, guests and their virtual processors (see [the driver VM](DRIVERVM.md)) |
| `iommu` | Intel VT-d: every device's interrupts remapped, devices' DMA passed through or confined to a guest's memory |
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
for each driver. A process's handle (with `MANAGE`) can end the process
alone or with every process it started, and those they started: its job,
as the Terminal's Ctrl+C ends a program.

**Memory.** VMOs are the unit of memory. Anonymous VMOs are committed lazily
on page faults. Address spaces map VMOs with per-mapping permissions, which
`vm_protect` can change for any part of a mapping but never beyond what the
handle the VMO was mapped with allows; parts of mappings can be unmapped.
*Private memory* (`vm_allocate`) is a VMO that no handle reaches, made for
one mapping: its pages are freed as soon as they are unmapped, and
`vm_decommit` frees them while the mapping stays (they read as zeros
afterwards), which is what C's `munmap` and `madvise` need. Frames are freed
only after the TLB shootdown that makes them unreachable. The kernel half
of every address space is shared.

**IPC.** Channels carry messages of up to 64 KiB plus up to 64 handles.
Bulk data (window buffers, audio rings, file contents) travels in shared
VMOs. *Sockets* are byte streams for pipes and terminals: a 256 KiB buffer
each way, writes of up to 4096 bytes that are all-or-nothing (as POSIX
promises for pipes), half-closing to end a stream, and endpoints that can
be duplicated, so several processes can share one. Threads wait on signals
of several objects at once (`object_wait_many`).

**Time.** The monotonic clock comes from the TSC, calibrated against the
HPET (or the PIT without one). On an Intel processor not under a
hypervisor, the TSC frequency the processor reports itself (CPUID leaves
0x15 and 0x16, as Linux reads them) wins over a timer that measures more
than 5% off it; `dmesg time` says which was used. The clock and every
timer read the TSC of the processor they run on, so all processors' TSCs
must agree. They count the same clock, each offset by its
`IA32_TSC_ADJUST`, which firmware may leave different from one processor
to another; as Linux does, the kernel sets the boot processor's to 0
before the clock starts and each other processor's alike as it starts,
then checks with an exchange of readings that they agree and corrects
what is left (`dmesg smp` says by how much). The wall clock starts from
the firmware's RTC, which keeps UTC; the `tz=` boot option (xtask passes
the host's offset) gives local time, which people see, while protocols and
certificates use the UTC clock.

**Processors.** Each processor turns its caches on as it starts (a
processor started with INIT keeps the setting the firmware left it, which
may be off), and on Intel processors with hardware P-states lets the
processor choose its own speed, up to turbo, leaning towards performance;
without them it would stay at whatever speed the firmware left it at.
`dmesg cpu` says how fast the boot processor runs while busy.

**Scheduling.** 32 priorities, round-robin within a priority, 10 ms slices,
preemption on wake-up of a higher-priority thread, tickless one-shot timers,
eager FPU/SSE/AVX state switching with XSAVE. A thread can name an *exit
futex*: a word the kernel clears, and wakes a waiter on, once the thread
has ended and left its stack, which is how thread libraries join threads.

## User space

* **Executables.** Veda's own programs are PE32+ images (built for
  `x86_64-pc-windows-msvc` with `#![no_std]`, linked at `0x140000000` with
  no imports): this target gives stable Rust hard-float SSE code, while the
  kernel is soft-float. C programs are static ELF64 executables for the
  System V ABI, built with GCC and musl (see [C on Veda](C.md)).
* **`vrt`** is the runtime every Rust program links: entry point and
  startup message, syscall wrappers, heap (TLSF), threads, futex-based
  locks, time, logging and process creation.
* **Process creation** happens in user space (`vrt::process::Spawn`):
  create a process, map the program's image from VMOs, create a thread, and
  start it with a bootstrap channel carrying the startup message
  (arguments, environment, working directory, role-tagged handles). An ELF
  program's segments are checked and mapped by `lib/elf`, and its stack
  starts the way Linux starts one: arguments, environment and the
  auxiliary vector.
* **C programs** run on musl, whose system calls go to `vposix`
  (`lib/posix`), a POSIX layer in Rust linked into the C library: files are
  the VFS's open files, pipes and terminals are sockets, threads are kernel
  threads, `posix_spawn` starts processes. The Terminal runs them with a
  terminal of their own (canonical line editing, `termios`, Ctrl+C). See
  [C on Veda](C.md).
* **Services** register with the registry in `init`; clients connect by name.
  Protocols are declared with the `vipc` macros, which generate typed client
  stubs and server dispatch code. With every connection the registry hands
  the service the client's identity (process, program name, whether it is
  a system service or an application), so a service can decide who may do
  what; system service names cannot be registered by other programs.

## Devices (`services/devmgr`)

* `devmgr` enumerates the PCI bus and starts a driver for each device it
  has one for, matched by vendor and device or by PCI class. A driver gets
  a channel speaking the `pcidev` protocol for exactly its device: that
  device's configuration space (its BAR registers stay as the firmware set
  them), its BARs mapped, MSI interrupts (or the line its INTx is wired
  to, where the firmware's `_PRT` says) and DMA memory. Drivers never see
  other devices. A memory BAR the firmware leaves unplaced (a laptop's
  does so for its serial bus controllers) devmgr places first, in a
  window of the PCI root bridge's (`_CRS`) where nothing else is, from
  the top down; one it cannot place reaches no driver, and no physical
  VMO reaches the kernel's RAM. A function woken from D3hot that lost its
  BARs (it resets, unless it says it does not) gets them back.
* **ACPI.** `devmgr` also reads the firmware's ACPI tables (the kernel
  hands `init` the RSDP and the firmware's ACPI memory ranges, `init`
  hands them to `devmgr`), because some devices exist only there: a
  laptop's touchpad on an I2C bus, its speaker amplifiers on an SPI bus,
  the GPIO pins wired to them.
  `vacpi` (`lib/acpi`) loads the DSDT and SSDTs into a namespace and
  evaluates its objects with a small AML interpreter: integer, string,
  buffer and package operations, buffer fields, control flow, method
  calls, and the fields of firmware memory regions (the firmware's
  settings, which decide what exists and where) and of PCI configuration
  space (the registers of the function at the `_ADR` of the device a
  region is in, on its root bridge's bus or below its bridges, as ACPICA
  finds it: an Intel PC's root bridge sizes its windows by its host
  bridge's). It changes nothing: what a method stores lasts only for that
  evaluation, and it reads no I/O ports or embedded controller. Definitions
  inside an `If` outside any method exist only when their condition holds
  (that is how a board leaves out what its settings turn off); a region
  whose address depends on a later table is placed when first read, and a
  name whose data refers to objects defined after it (a `_PRT` package
  naming interrupt link devices) is evaluated again once they are, as
  ACPICA does. Every evaluation is bounded. The tables and the firmware's
  ACPI memory are mapped cached, anything else AML reads uncached. On the
  laptop this was written for, all of a 480 KiB DSDT and sixteen SSDTs
  load, every conditional definition with them; `cargo xtask acpi` shows
  what devmgr makes of a machine's tables, on the host.
* **The IOMMU.** Where the firmware describes one (VT-d), the kernel
  remaps every interrupt: a device's MSI raises an entry that names the
  device, so no other can raise it, and the compatibility format is
  blocked. Devices reach memory untranslated, as without an IOMMU, until
  one is given to a guest. MSIs are therefore made for a PCI function
  (`msi_create` takes a resource naming it, `resource_kind::PCI`), which
  `devmgr` holds for all of them.
* **The driver VM.** Every other device goes to Linux in a virtual machine
  ([the driver VM](DRIVERVM.md)): all but the disks and the platform's own
  functions. `devmgr`
  hands their `pcidev` channels to `drivervm`, which gives them to its
  guest whole, their DMA confined to the guest's memory by the IOMMU, and
  describes them in ACPI tables of its own, as a PC's firmware would. It
  starts when the processors run virtual machines and an IOMMU confines
  devices (the kernel says both in the boot information, `vabi::platform`),
  and the system image has its Linux. When it ends without Linux having
  powered it off, `devmgr` resets its devices and starts it again.
* A driver asks for the devices the firmware describes below its PCI
  function (its ACPI companion, found by `_ADR` under the PCI root bridge):
  their ids (`_HID`, `_CID`, `_UID`, `_SUB`), status and resources (`_CRS`:
  memory, ports, interrupts, GPIO and SPI or I2C connections, their
  controllers resolved), and what the driver VM's monitor passes on: their
  interrupt lines, what their `_DSM` answers, constant data (`_DSD`, an
  I2C controller's timing).
* **GPIO.** `devmgr` drives the GPIO pins those devices are wired to on
  the driver's behalf, and no others: a driver names a pin of one of its
  devices (counting the pins of its GPIO connections), `devmgr` finds the
  controller (its registers from its own `_CRS`) and the pad (`vgpio`,
  `lib/gpio`). The controllers are Intel's (Tiger Lake-LP's pads, which
  Alder Lake-P kept; the layout as Linux's `pinctrl-tigerlake` has it): a
  pad owned by the firmware, or whose settings are locked, is refused; one
  in another function becomes a GPIO only when needed; a level is set
  before the output is enabled. A pin's interrupt (a `GpioInt`
  connection) is an interrupt `devmgr` raises: a controller has one line
  for all its pins, which `devmgr` waits on, and when it fires each pin
  with an interrupt pending raises its own — an edge, or level-triggered
  as its connection says, the pin masked until the interrupt is ended (the
  ending signals an event, and `devmgr` unmasks the pin, which fires again
  if its level lasts). Only a pad the firmware leaves in GPIO driver mode
  interrupts (one in ACPI mode raises the firmware's events; Linux refuses
  those too). The kernel's part is mechanism: an interrupt a program
  raises (`irq_raise`, with a handle's `SIGNAL` right; whoever it is for
  gets a handle without it), which reaches its driver, or a guest's
  processor, as a line's does, and signals the event its maker gave when
  a level-triggered one ends (as KVM's resample event does for VFIO). A
  test's ACPI table may describe a simulated controller (`VTST0002`):
  `vgpio::sim`'s registers, as the hardware keeps them, its even pins
  wired to the odd ones after them.

## Storage

* `virtio-blk` (QEMU), `ahci` (SATA disks: QEMU's q35 controller, many
  PCs) and `nvme` (NVM Express: the SSDs of most PCs since about 2016,
  QEMU's `nvme`) serve each disk (each namespace of an NVMe controller)
  through the `block` protocol under the name `block/<serial>`, so clients
  find a disk by its serial number whatever the controller. `devmgr`
  matches drivers by vendor and device, and AHCI and NVMe controllers by
  their PCI class. The live system starts none of them (see Boot).
* `nvme` resets the controller and gives it an admin queue pair and one
  I/O queue pair, and runs a request at a time: READ, WRITE and FLUSH, the
  data in one buffer that PRP entries describe (a list past two pages), a
  request the controller cannot take whole split into several; IDENTIFY
  gives the serial number, the namespaces and their block sizes (512 or
  4096 bytes). It waits on an MSI-X or MSI interrupt, or polls.
* `vfs` serves `/system` straight from the initrd and keeps `/home` and
  `/tmp` in memory. If a disk with serial `veda-home` is attached, `/home`
  is restored from it at boot and written back half a second after changes
  stop (and before a shutdown from the start menu). The disk holds two
  snapshot slots with checksums; saves alternate between them, so an
  interrupted save never damages the previous one. Unchanged sample files
  are stored as references to the system image.
* Besides whole-file requests, the VFS opens files as connections of the
  `file` protocol, each an open file description with its own offset;
  duplicates share it, as POSIX descriptors do. A file removed while open
  stays readable and writable through its connections until the last one
  closes. Stat results carry inode numbers and whether a file is a program
  (by its first bytes: ELF, PE, `#!`), and `/dev` holds `null`, `zero`,
  `full`, `random` and `urandom`.
* Applications work with files through `vfiles`: paths (resolving against a
  working directory and `~`, the read-only `/system`, wildcards, natural
  name order, free names for new items), size and time formatting, a VFS
  client with whole-file reads and writes, recursive copies, moves and
  removals and the space left on a file system, the Trash, and, with the
  `thumbnails` feature, image thumbnails made on a background thread. Its
  table of file types is the system's only list of which application opens
  which file: Files, the Terminal's `open`, the desktop icons and the
  pickers of Photos, Music and Settings all use it.
* The Trash (`vfiles::trash`) is the home trash of freedesktop.org's Trash
  specification: deleted items go to `~/.local/share/Trash/files`, and for
  each a `NAME.trashinfo` in `info` records where it was and when it was
  deleted. The info is created first, as a file that must not exist yet,
  which reserves the name against other programs trashing at the same time;
  then the item is renamed into the Trash in one step (`/home` and `/tmp`
  are one file system). Files deletes to the Trash (Shift+Delete deletes
  for good), lists it with where each item was deleted from and when, and
  restores and empties it; the desktop's first icon is the Trash, which
  shows whether anything is in it and takes items of the desktop dragged
  onto it. The agent's `files` function and Files' agent actions delete to
  the Trash with the user's OK, restore without asking, and ask every time
  before emptying it. The Terminal's `rm` deletes for good, as Unix's does.

## USB

USB is Linux's: the controllers go to [the driver VM](DRIVERVM.md) whole,
and Linux drives them, their hubs and the devices on them. Keyboards,
mice, tablets and touchscreens reach the window system's `input` service
through `input` (`guest/input`), which reads Linux's event devices;
network adapters reach the network service through `net`; Bluetooth
adapters get Linux's Bluetooth stack, which no service of Veda's uses
yet. Disks on USB are left alone: disks are Veda's, which has no driver
for them yet.

## Networking

Networking is three layers of processes (details in
[NETWORKING.md](NETWORKING.md)):

* **Drivers** only move frames, and they are Linux's, in [the driver
  VM](DRIVERVM.md): `net` (`guest/net`) offers its network cards' Ethernet
  frames to the network service; `wifi` (`guest/wifi`) offers its Wi-Fi
  radios to the Wi-Fi service as *managed* radios, which scan, join access
  points, encrypt and move Ethernet frames on the service's commands
  (under QEMU, the virtual radio: a virtio-serial port connected to the
  `airsim` simulator on the host, which `airlink` makes a radio of
  Linux's). Frames travel through shared-memory rings (`vproto::netring`)
  with wake-up events; neither side trusts the other's indices or lengths.
* **`wlan`** (the Wi-Fi service, built on `vwlan`) decides what to join,
  authenticates (open system; SAE, which it computes), runs the key
  handshakes and gives the radio only the session keys, keeps the saved
  networks and decides when to reconnect or roam. It presents each radio
  to `netd` as `wlan0`.
* **`netd`** (on `vnetstack` and smoltcp) runs interfaces, DHCP, IPv6
  autoconfiguration, routes, a caching DNS resolver and the sockets that
  applications use through `vnet`, one channel per socket with
  credit-based flow control in both directions.

TLS is not a service: `vtls` (rustls with a pure-Rust cryptography
provider) runs inside each application, over its `vnet` sockets, so keys
and plaintext never leave the process that uses them.

`netd` and `wlan` are restarted by `init` if they exit; drivers attach to
the new instance. Calls from a service to a driver have timeouts and the
driver reports its state with one-way events, so a hung or crashed driver
cannot block a service. Random numbers (TCP sequence numbers, DHCP and DNS
ids, Wi-Fi nonces and keys, TLS secrets) come from the kernel's ChaCha20
generator.

## The window system (`services/compositor`)

The compositor owns the screen and serves three protocols: `display` for
applications, `input` for input drivers and `displaydev` for a display
driver.

* **Surfaces.** A client creates a window and attaches two pixel buffers in
  a shared VMO. It draws into the back buffer and `present`s it; the
  compositor answers `FrameDone` once the previous buffer is no longer
  needed, which paces every client to the display without copying pixels.
* **Composition** is damage driven: changed rectangles are recomposed from
  the bottom up (premultiplied alpha, shadows, rounded corners,
  open/close/minimise animations), at most once per display frame. At
  system start the screen shows the startup sequence instead (see
  [Boot](#boot)) until the desktop has drawn itself and dissolved in.
* **On the GPU** (`gpu.rs`). Where a display flips and the GPU can draw
  into its pictures (Linux's drivers of both in the driver VM: `kms`, and
  the renderer on Mesa's driver of the GPU), the GPU composes every frame
  with OpenGL ES (`vgl`),
  straight into the picture the display shows next, as modern window
  systems do. Each window is a texture, brought up to date where its
  client drew; shadows, borders, rounded corners and the startup
  sequence's gradient, light and ring are shaders, from the formulas the
  processor draws with; title bars, cursors, the switcher and the
  sequence's words are drawn once by the processor into textures. What a
  picture lacks of the screen is copied from the one that shows it, and a
  picture is asked for once the GPU's fence after its frame has
  signaled. The GPU is set up in a thread of its own once a driver
  attaches (the screen keeps moving meanwhile), then checked: the
  processor reads back what the GPU drew into a picture. If that fails,
  or the GPU later takes two seconds over a frame, frames are composed by
  the processor again (`compositor: frames are drawn by ...` says which).
* **On the processor** frames are composed into a back buffer and copied
  to the screen: in a virtual machine, or wherever the GPU cannot draw
  into the display's pictures.
* **The screen** (`screen.rs`). Frames go into the framebuffer the
  firmware left, written in place and paced by a 60 Hz timer, until a
  display driver that can flip attaches (`displaydev`; see
  [Displays and GPUs](#displays-and-gpus)). From then on each frame goes into
  one of the driver's pictures that is not on the screen; the compositor
  asks for it, the driver shows it from the next vertical blank, and the
  next frame is composed once it is shown. So the screen never shows a
  frame half drawn, frames come at the screen's own pace, and animations
  are timed for the vertical blank at which their frames will be seen.
  Each picture keeps what changed while the other was drawn into, and is
  brought up to date before its next frame. Only drivers (started by
  `devmgr`) may attach, and only to the screen the compositor draws on:
  the firmware's framebuffer address, the size and the pixel format must
  match. If the driver goes away, or leaves a flip undone for a second,
  frames go into all its pictures and the firmware's framebuffer, in
  place, whichever the screen shows.
* **Window kinds** form layers: the desktop, normal and borderless windows,
  panels (which reserve screen space), popups (closed when they lose focus)
  and notifications. A focused full-screen window rises above the panels.
  Decorations are drawn by the compositor, so a hung client can still be
  moved and closed.
* **Input**: keyboard events go through a keymap (US layout, modifiers, key
  repeat) to the focused window; pointer events go to the window under the
  pointer, or to the one that grabbed it while a button is held. Dragging a
  window to the top or a side edge maximises it or snaps it to that half of
  the screen; Super+arrows do the same from the keyboard and Super+D shows
  the desktop. Alt+Tab shows the window switcher (live thumbnails), Alt+F4
  closes, and tapping Super sends `StartMenuKey` to the shell.

## Displays and GPUs

Drawing into the picture the screen is showing tears, and frames paced by
a timer drift against the screen's own rhythm: what looks smooth in a
virtual machine's window, whose host shows whole frames, judders on a
laptop's panel. Displays and GPUs are Linux's, in
[the driver VM](DRIVERVM.md), as are their drivers' quirks and firmware:

* **Displays.** `kms` (`guest/kms`) drives, through Linux's KMS, the
  display whose memory holds the firmware's framebuffer (a laptop with two
  GPUs has outputs on both), and attaches it to the window system through
  `displaydev`: the compositor draws into pictures of Veda's memory, which
  Linux's driver shows as they are (dma-bufs of the VMOs) and flips at the
  vertical blank, the flips' times on Veda's clock. Until it attaches, and
  whenever the driver VM is gone, the compositor draws into the
  firmware's framebuffer.
* **GPUs.** The renderer (`guest/renderer`) carries out applications'
  OpenGL ES command streams on Mesa's Gallium driver of the GPU, over the
  GPU's Linux driver (see below).

Under QEMU, the driver VM gets QEMU's VGA (Linux's bochs driver) and, for
3D, a virtio-gpu (Linux's virtio_gpu, Mesa's virgl), whose virglrenderer
renders on the host's GPU; `tests/ui/drivervm-*.vts` cover both.

## The desktop shell (`apps/shell`)

The shell is an ordinary process that the compositor trusts with shell
surfaces: the desktop (wallpaper and icons), the taskbar (a panel), the
start menu and calendar (popups) and notifications. Each surface is a
`vui::Host`; they draw from one shared model and request changes as actions
(launch an app, activate or minimise a window, toggle a popup) that the
main loop performs. Running windows come from the compositor
(`list_windows`, refreshed on `WindowsChanged`); installed applications
come from the launcher in `init`, which reads the `.app` manifests. The
shell serves the `shell` protocol so applications can change the wallpaper
and post notifications; `vproto::shell::ShellLink` makes these calls from a
background thread, because setting a wallpaper decodes and scales a large
picture first. When an application crashes (a CPU fault or a panic),
`init` reports it on the channels handed out by `launcher::watch`, and the
shell tells the user with a notification.

## Graphics and the toolkit

| Library | Role |
|---------|------|
| `vraster` | paths (lines, quadratic and cubic curves, arcs), transforms, strokes, anti-aliased scanline rasterisation with coverage spans |
| `vfont` | TrueType/OpenType (CFF) parsing, glyph outlines, metrics, kerning, a glyph bitmap cache |
| `vimage` | PNG, JPEG (baseline and progressive), BMP and QOI decoders and encoders, resampling |
| `vgfx` | `Canvas` drawing on pixel buffers: fills, gradients, rounded rectangles, paths, bitmaps, shadows, text layout and rendering |
| `vui` | the immediate-mode toolkit: windows, frame pacing, input, widgets (buttons, toggles, sliders, text boxes, lists, scroll areas, tabs), menus, modal dialogs, the file dialog, vector icons, the dark theme |
| `vtext` | the text editing model behind the Text Editor: line buffer, selections, grouped undo, search, soft-wrap layout, syntax highlighting |

`vui` is immediate mode: each frame the application's `update` lays out and
draws widgets, and the widgets report their interactions in the same call.
State that must survive between frames (focus, scroll offsets, text cursors,
open menus, animations) lives in a per-window `UiState` keyed by widget ids.
Frames are drawn only when input arrives or an animation asks for one.

## 3D graphics (`lib/v3d`)

`v3d` renders on the CPU (the games predate OpenGL ES in Veda), usually
under QEMU's TCG emulator, where integer instructions are cheap and floating point is very
expensive. Floating point is therefore used once per draw call (matrices
and light parameters); everything per vertex and per pixel is fixed-point
integer code (`src/pipeline`). Each render mode gets its own monomorphised
rasteriser, and hot loops are kept scalar: TCG emulates SSE integer
multiplies with slow helper calls, so auto-vectorised loops would be
several times slower there.

* **Geometry.** Objects outside the view frustum are skipped. Vertices are
  transformed and lit (Gouraud: sun, hemisphere ambient, point lights, fog)
  in batches; triangles are back-face culled, clipped against the near
  plane and a guard band, set up, and binned into 64x32-pixel screen tiles.
* **Rasterisation.** A thread pool sized to the CPU count fills the tiles in
  parallel, each worker owning whole tiles: depth testing, perspective-
  correct mip-mapped textures, opaque, alpha, additive and multiplicative
  blending, then billboards and particles.
* **Output.** The image is rendered at an internal resolution and scaled to
  the window with a bilinear (or fast 2x) upscale. The game harness
  (`v3d::app`) adjusts the internal resolution to hold the frame rate,
  draws the 2D HUD with `vgfx`, shows statistics on F3 and lets the
  computer play on F8 (for demos and the GUI tests).

The window system cooperates: a game's window is opaque, so the compositor
copies its rows and skips everything underneath, and the scheduler starts
the pool's workers on different CPUs at once.

## OpenGL ES and the GPU (`lib/glsl`, `lib/gl`, `guest/renderer`)

Applications get OpenGL ES 3.0, with GLSL ES 1.00 and 3.00, from `vgl`,
in pure Rust. *Prism* (`apps/prism`) shows it off: a reflective knot under
a sky cube map, shadow-mapped crystals drawn with instancing, and sparks
simulated with transform feedback.

* **The shading language.** `vglsl` preprocesses, parses and checks
  shaders, links programs (attribute, uniform and varying locations,
  std140 blocks, transform feedback), and lowers each stage to an SSA form
  over structured control flow, which it optimises. Constants are folded
  with `vglsl::ops`, the one definition of what every operation computes.
  Two back ends take the SSA form: bytecode for the software renderer's
  SIMD interpreter, and TGSI text for virglrenderer (`vglsl::tgsi`).
* **The API.** `vgl::Context` has every OpenGL ES 3.0 entry point, as a
  method named after the C function, and checks every call as the
  specification requires; it keeps the GL's objects and state and converts
  pixel data. Vertex and index data always come from buffers (as in
  WebGL 2). Below it, a `Backend` with Gallium's shape renders: resources,
  surfaces, and draws described by complete state.
* **The software renderer** (`vgl::soft`) records draws per framebuffer,
  shades vertices, bins triangles into 64-pixel tiles and rasterises the
  tiles in parallel, running fragment shaders on 16 lanes at a time (four
  2x2 quads): 4x multisampling, every ES 3.0 format, ETC2, exact sRGB.
* **The GPU renderer** (`vgl::virgl`) speaks virglrenderer's protocol,
  which Veda's renderer carries out on the GPU (below), and which
  virglrenderer replays with a host's OpenGL in vgl's host tests. State objects are made once per distinct state
  and cached, draws set only what changed, data moves through a staging
  buffer shared with the device, in command order, and presenting blits
  the frame (resolved, flipped and scaled) into an image the window's
  size that is read back ready to copy. What hosts refuse is handled
  first: draws that would read past a buffer are dropped (ANGLE would
  give up the context), integer constants are built at run time (the
  host's compiler flushes them as denormal floats), and separate transform
  feedback buffers take a pass each on OpenGL ES hosts. So is what would
  end the host: on OpenGL ES, virglrenderer binds a 3D texture's slice to
  a framebuffer with a function ANGLE lacks, and the process aborts. 3D
  textures there are only sampled and written; their slices are read by
  drawing their texels into a 2D image, drawn into through a 2D copy
  that is written back, and their mipmaps are made in guest memory, as
  the software renderer makes them. Copies between images, which hosts
  without `glCopyImageSubData` make through framebuffers (a layer at a
  time, and not for formats they cannot render to), are made by blits, a
  layer at a time, or through guest memory. Some drivers err in ways only
  the host's desktop OpenGL shows (QEMU's window renders on it; headless
  QEMU on ANGLE): Intel's on Windows records only zeros as transform
  feedback from any vertex shader that writes `gl_ClipDistance`, which
  virglrenderer's all do there, and does not order
  `glCopyImageSubData` after draws into its source. The first draw that
  captures checks the host with a known point, and where it fails the
  captured vertices are shaded in guest memory by the software
  renderer's vertex stage (the host still draws what is seen); and blits
  carry a scissor that keeps virglrenderer from turning them into image
  copies. New render targets are cleared to zero, as the software
  renderer starts them (the host's memory may hold another context's
  frames), and stencil-only ones are kept in a depth-stencil format
  (hosts find stencil-only attachments incomplete), whose depth draws
  ignore: a test without its attachment is turned off, as OpenGL ES has
  it pass.
* **The renderer** (`guest/renderer`) serves the `gpu` protocol from the
  driver VM: each connection gets a context and a block of Veda's memory
  shared with it (commands, staging, query results, the fence word), and
  its commands are carried out on one of Mesa's Gallium drivers, over the
  GPU's Linux driver: iris on i915 or xe (Intel's GPUs), virgl on
  virtio-gpu (QEMU's, which renders on the host's GPU), or softpipe
  (`drivervm.renderer=softpipe`, for tests). The program is Mesa's build
  of the decoder (`decoder/`, C) around its Rust half, which serves the
  protocol over the bridge; `cargo xtask linux` builds Mesa for the guest
  with a toolchain of its own (`ports/linux/build.sh`). The decoder takes
  virgl's commands straight to Gallium's calls, checking every word first
  (a client can fail its own context, never the renderer), with Gallium's
  state cache (`cso_context`) between it and the driver, so that a client
  deleting an object leaves nothing behind that the driver still uses.
  Commands are copied out of the shared memory before they are decoded.
  Formats are virgl's, which OpenGL hosts take: 24-bit depth with stencil
  is `S8_UINT_Z24_UNORM`, depth in the upper bits as
  `GL_UNSIGNED_INT_24_8` packs it. iris has depth only in the lower bits
  (`Z24_UNORM_S8_UINT`), so on iris the renderer keeps those formats that
  way round and turns each texel round when it is copied in or out
  (`VR_DEPTH_LOW=1` makes it do so on softpipe too, for the host's tests).
  A buffer may be bound anywhere later, whatever it was made for, and
  drivers are told so, but virgl, whose host takes a buffer for one use
  and lets it serve them all; a cube map's faces are moved one at a time,
  as Mesa's OpenGL moves them.
  It also draws into memory it is given (`gpu::import`): a display's
  picture, which the compositor makes a render target of
  (`renderbuffer_storage_external`, as `glEGLImageTargetRenderbufferStorageOES`
  makes one of a display's buffer). For a GPU the memory becomes a dma-buf
  of the VMO (the bridge's), which its driver imports as Linux's would,
  linear, and reaches through the IOMMU; softpipe draws into it as the
  renderer maps it, through a window system of the renderer's own
  (`device.c`). virgl cannot (its host draws into memory of its own).

`vgl::veda::context` renders on the GPU when the `gpu` service exists and
in software on every CPU otherwise. Under QEMU the difference is large:
Prism runs at about 1 frame a second in software, and at 75 at 1024x640 on
the host's GPU through the driver VM.

The virgl renderer is tested on the host: `vgl::virgl::host` calls
virglrenderer as QEMU does, on OpenGL contexts made as QEMU makes them,
and runs the whole `vgl` test suite on the host's GPU
(`VGL_TEST_BACKEND=virgl`): it loads the system's virglrenderer and
renders through EGL on a GPU's render node (GBM), on desktop OpenGL as
QEMU does and on OpenGL ES (`VGL_TEST_HOST=gles`). Prism's scene,
rendered both ways, must look the same to within the GPU's rounding. The
suite also runs through the renderer's decoder (`VGL_TEST_BACKEND=gallium`,
`vgl::virgl::gallium`, which loads the decoder built with Mesa for the
host as `vgallium.so`): on softpipe, where it falls short of a GPU (no
multisampling, points an eighth of a pixel off, depth filtered before it
is compared) the tests say so; and on Mesa's virgl over virglrenderer's
test server (`VGL_GALLIUM_DEVICE=virgl`), as the driver VM's renderer
renders under QEMU.

## Audio

* Sound devices are Linux's, in [the driver VM](DRIVERVM.md): HD Audio
  controllers (most PCs' sound, QEMU's `intel-hda`) with their codecs,
  virtio's sound devices, and a laptop's speaker amplifiers on its SPI
  controller (Cirrus Logic's CS35L41, which the codec's driver plays
  through; the guest gets the GPIO pins they are wired to). Linux's
  drivers drive them, and `alsa`, Veda's driver for Linux, attaches the
  card to the audio service's private `audiodev` protocol, as a native
  driver would; the service also runs without sound hardware (a null
  output then consumes audio in real time).
* **Recording.** An input device produces into a ring with a capture
  clock; the service converts it for each capture stream, and runs the
  echo canceller for streams that ask for it (the agent's microphone) with
  the mixed output as reference, aligned by the two clocks; such a stream
  may carry the reference and the uncancelled microphone beside the
  cleaned one. The device records only while a capture stream is open.
* The `audio` service mixes every client stream: exact rational resampling
  (polyphase windowed sinc in integer arithmetic), ramped gains, click-free
  pause and seek, master volume and mute.
* PCM never travels in messages. Each stream, and the device link, is a
  single-producer/single-consumer ring of 16-bit frames in a shared VMO,
  with an event for back-pressure and a time-stamped play position, so
  players get accurate, smoothly extrapolated progress. Each side keeps
  its own counter and clamps the peer's: a misbehaving client can only
  garble its own audio.
* `vaudio` holds the formats (WAV, QOA), the resampler, mixing, an FFT for
  visualisers, and a synthesiser and sequencer that `tools/musicgen` uses
  to render the bundled album and the startup sound at build time.
* `vaudio` also holds the voice processing for an always-listening
  assistant. `aec::EchoCanceller` removes the loudspeakers' sound from the
  microphone, given the mixed speaker output as reference: a
  partitioned-block frequency-domain adaptive filter (tails up to 1 s)
  whose step follows the estimated residual echo, so it holds still during
  double talk and re-converges after the echo path changes, a bulk delay
  estimator (0 to 500 ms) and a residual echo suppressor; it adds 16 ms of
  latency at 16 kHz. `vad::Vad` flags speech every 10 ms (minimum-statistics
  noise floors, a likelihood ratio test, click rejection, hysteresis per
  utterance), and `level` meters RMS and peak levels in dBFS.

## The voice agent (`services/agent`)

The agent service holds the conversation with Deepgram's Voice Agent API
(a WebSocket over TLS through `vweb` and `vtls`), the microphone and the
voice (two audio streams), and listens for its name while asleep (a local
voice detector, then Deepgram's streaming recognition). A worker thread
runs the functions the language model calls: system functions (windows,
files, volume, Wi-Fi, timers, memory, ...) and applications' actions,
which every `vui` application offers over the `agentapp` protocol
(describe, state, invoke) through `vui::App`. Actions that need the
user's consent wait for an approval from the shell (the agent's window or
a notification); only the shell may approve and only Settings may change
the agent's settings, by identity. The agent's settings, Deepgram key,
memory and permissions live in `/home/.private/agent`, which the file
system service opens to the agent alone. The decisions that need no I/O
(the protocol, tools and their risks, the prompt, memory, the approval
policy, the wake word) are in `vagent`, tested on the host. See
[The agent](AGENT.md).

## Testing

* Host unit tests for the libraries with platform-independent logic (ABI,
  heap, IPC codec, service protocols, ELF loading, the POSIX layer's
  conversions, math, rasteriser, fonts, image codecs, 2D graphics, text
  editing, paths and file types, audio, build tool).
* `systest`, a program that runs inside Veda and exercises kernel objects
  (sockets, memory protection, private memory, exit futexes, ending a job),
  threads, the file system, the launcher, crash reports and the restart of
  the window system, and runs C and C++ programs: `tests/c/posix.c` checks
  the C library and the POSIX layer, `tests/c/cxx.cc` the C++ library.
  `nettest` checks DNS, UDP, TCP, HTTP, HTTPS and ping over Ethernet or
  Wi-Fi.
* Network host tests: the 802.11 protocol and its cryptography against
  published vectors, a station against an access point, two TCP/IP stacks
  over a simulated cable, the Wi-Fi simulator, and the TLS client against
  a rustls server and real certificate chains.
* Hardware no emulator has is simulated where Veda's own code drives it:
  `vgpio::sim` is a GPIO controller as its registers behave (Intel's
  pads, its pins wired in pairs), which devmgr drives in place of a PC's
  when a test's ACPI table describes one (`drivervm-gpio.vts`, through to
  Linux's GPIO character device in the guest). `cargo xtask acpi` shows
  what devmgr makes of a real machine's ACPI tables. The real machine
  stays the final check of what Linux drives there.
* GUI automation scripts (`tests/ui/*.vts`) that drive QEMU through QMP —
  mouse, keyboard, waits on log lines, screenshots — and fail on panics.
  QEMU's machine has an IOMMU, as PCs do, so the driver VM runs in every
  script, with the pointer, USB, the networks and the display.
  The sound cards' scripts also check QEMU's recording of the output for
  dropouts.
  `drivervm-display.vts` gives the window system a display that flips
  (QEMU's VGA, which Linux drives in the driver VM) and compares
  screenshots of the desktop before and after a window came and went, in
  both of its pictures; `drivervm-display-crash.vts` has Linux crash
  under it, and `drivervm-display-restart.vts` the window system restart
  while it is attached. `drivervm-compose.vts` has the GPU compose: the
  renderer on softpipe draws every frame into its pictures, slowly, under
  emulation; `drivervm-gpu.vts` renders Prism on the host's GPU through
  QEMU's virtio-gpu, given to the driver VM.
* The agent's scripts (`tests/agent/*.vts`) with a stand-in for Deepgram on
  the host and a test microphone fed from the host; `tests/real/` talks to
  the real Deepgram.

`cargo xtask test --ui` runs all three.

## Repository layout

| Path | Contents |
|------|----------|
| `boot/` | UEFI loader |
| `kernel/` | microkernel |
| `lib/abi`, `lib/bootinfo`, `lib/initrd`, `lib/pe`, `lib/elf` | shared formats and the kernel ABI |
| `lib/rt`, `lib/heap`, `lib/build` | user runtime, allocator, build helper |
| `lib/posix` | the POSIX layer under the C library |
| `lib/math`, `lib/raster`, `lib/font`, `lib/image` | math, vector rasterisation, fonts, image codecs |
| `lib/ipc`, `lib/proto` | message encoding and the service protocols |
| `lib/gfx`, `lib/ui`, `lib/text` | 2D drawing, the GUI toolkit, the text editing model |
| `lib/files` | files for applications: paths, file types and the apps that open them, formatting, VFS access, thumbnails |
| `lib/v3d` | the fixed-point software 3D renderer and the game harness |
| `lib/glsl`, `lib/gl` | the GLSL ES compiler (with its TGSI back end), and OpenGL ES 3.0 with its software and GPU (virgl) renderers |
| `lib/audio` | audio formats, resampling, mixing, FFT, the synthesiser, echo cancellation, voice activity detection and level metering |
| `lib/virtio` | virtio device access shared by the drivers |
| `lib/acpi`, `lib/gpio` | the ACPI tables, the AML interpreter and resource templates, and Intel's GPIO pads (for `devmgr`) |
| `lib/hv`, `lib/iommu` | what the hypervisor and the IOMMU driver know that touches no hardware: the virtual APIC, `cpuid`, the guests' platform and boot protocol, the bridge's ABI; VT-d's tables and structures |
| `guest/` | the driver VM's Linux programs: its `init`, Veda's drivers for Linux (`input`, `alsa`, `net`, `wifi`, `kms`), the renderer (OpenGL ES on Mesa's drivers), `airlink` (QEMU's virtual radio as Linux's), and its tests (`bridgetest`, `pcitest`, `gpiotest`) |
| `lib/splash` | the boot splash's picture, which the boot loader and the window system draw alike |
| `lib/entropy` | the ChaCha20 random number generator and BLAKE2s entropy pool |
| `lib/netstack`, `lib/net` | the TCP/IP stack around smoltcp, and the networking API for applications |
| `lib/tls` | the TLS client for applications: rustls and its pure-Rust cryptography provider |
| `lib/web`, `lib/json` | HTTP/1.1 and WebSocket clients over TCP or TLS; JSON |
| `lib/agent` | the voice agent's logic: the Deepgram protocol, tools, prompt, memory, approval policy, wake word |
| `lib/wlan`, `lib/radiolink` | IEEE 802.11 (frames, RSN, handshakes, SAE, station and access point), and the virtual radio's link format |
| `third_party/` | vendored crates with documented patches (smoltcp) |
| `ports/` | third-party software built from source with Veda's patches: GCC, binutils, GMP, MPFR, MPC and musl; for the driver VM, Linux (with zlib and elfutils for its build), a toolchain of its own (the same GCC, binutils and musl, unpatched) and Mesa |
| `services/`, `drivers/`, `apps/` | system services, drivers and applications (the games included) |
| `tests/` | in-system tests, C and C++ test programs (`tests/c`) and GUI automation scripts |
| `tools/` | host programs: media generators, `airsim` (the simulated Wi-Fi environment) |
| `xtask/` | build orchestration, disk image creation, QEMU automation |
| `assets/` | fonts, firmware and other data shipped in the initrd |
| `docs/` | documentation |
