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
4. `init` starts the system services and the desktop shell, then supervises
   them: the window system, the shell, audio and the PS/2 driver are restarted
   if they crash (at most three times a minute). Drivers reconnect to a
   restarted compositor through the registry, which queues connections
   until a service registers again.

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
  `#![no_std]`, linked at `0x140000000` with no imports): this target gives
  stable Rust hard-float SSE code, while the kernel is soft-float.
* **`vrt`** is the runtime every program links: entry point and startup
  message, syscall wrappers, heap (TLSF), threads, futex-based locks, time,
  logging and process creation.
* **Process creation** happens in user space: create a process, map the PE
  sections from a VMO, create a thread, and start it with a bootstrap channel
  carrying the startup message (arguments, environment, role-tagged handles).
* **Services** register with the registry in `init`; clients connect by name.
  Protocols are declared with the `vipc` macros, which generate typed client
  stubs and server dispatch code.

## Storage

* `virtio-blk` (QEMU) and `ahci` (SATA disks: VirtualBox, QEMU's q35
  controller, most PCs) serve each disk through the `block` protocol under
  the name `block/<serial>`, so clients find a disk by its serial number
  whatever the controller. `devmgr` matches drivers by vendor and device,
  and AHCI controllers by their PCI class.
* `vfs` serves `/system` straight from the initrd and keeps `/home` and
  `/tmp` in memory. If a disk with serial `vindows-home` is attached, `/home`
  is restored from it at boot and written back half a second after changes
  stop (and before a shutdown from the start menu). The disk holds two
  snapshot slots with checksums; saves alternate between them, so an
  interrupted save never damages the previous one. Unchanged sample files
  are stored as references to the system image.
* Applications work with files through `vfiles`: paths (resolving against a
  working directory and `~`, the read-only `/system`, wildcards, natural
  name order, free names for new items), size and time formatting, a VFS
  client with whole-file reads and writes, recursive copies, moves and
  removals and the space left on a file system, and, with the `thumbnails`
  feature, image thumbnails made on a background thread. Its table of file
  types is the system's only list of which application opens which file:
  Files, the Terminal's `open`, the desktop icons and the pickers of Photos,
  Music and Settings all use it.

## Networking

Networking is three layers of processes (details in
[NETWORKING.md](NETWORKING.md)):

* **Drivers** only move frames: `virtio-net` and `e1000` (Intel PRO/1000)
  offer Ethernet frames to the network service, `vwifi` (the virtual radio under QEMU, a virtio-serial
  port connected to the `airsim` simulator on the host) offers raw 802.11
  frames to the Wi-Fi service. Frames travel through shared-memory rings
  (`vproto::netring`) with wake-up events; neither side trusts the other's
  indices or lengths.
* **`wlan`** (the Wi-Fi service, built on `vwlan`) scans, authenticates
  (open system, SAE), associates, runs the key handshakes, encrypts with
  CCMP, protects management frames, keeps the saved networks and decides
  when to reconnect or roam. It presents each radio to `netd` as `wlan0`.
* **`netd`** (on `vnetstack` and smoltcp) runs interfaces, DHCP, IPv6
  autoconfiguration, routes, a caching DNS resolver and the sockets that
  applications use through `vnet`, one channel per socket with
  credit-based flow control in both directions.

`netd` and `wlan` are restarted by `init` if they exit; drivers attach to
the new instance. Calls from a service to a driver have timeouts and the
driver reports its state with one-way events, so a hung or crashed driver
cannot block a service. Random numbers (TCP sequence numbers, DHCP and DNS
ids, Wi-Fi nonces and keys) come from the kernel's ChaCha20 generator.

## The window system (`services/compositor`)

The compositor owns the framebuffer and serves two protocols: `display`
for applications and `input` for drivers.

* **Surfaces.** A client creates a window and attaches two pixel buffers in
  a shared VMO. It draws into the back buffer and `present`s it; the
  compositor answers `FrameDone` once the previous buffer is no longer
  needed, which paces every client to the display without copying pixels.
* **Composition** is damage driven: changed rectangles are recomposed from
  the bottom up into a back buffer (premultiplied alpha, shadows,
  open/close/minimise animations) and copied to the framebuffer, at most
  once per display frame.
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

There is no GPU: `v3d` renders on the CPU, usually under QEMU's TCG
emulator, where integer instructions are cheap and floating point is very
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

## Audio

* `virtio-snd` drives the sound card. It connects to the audio service's
  private `audiodev` protocol, so the service also runs without sound
  hardware (a null output then consumes audio in real time).
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
  to render the bundled album at build time.

## Testing

* Host unit tests for the libraries with platform-independent logic (ABI,
  heap, IPC codec, service protocols, math, rasteriser, fonts, image
  codecs, 2D graphics, text editing, paths and file types, audio, build
  tool).
* `systest`, a program that runs inside Vindows and exercises kernel objects,
  threads, the file system, the launcher, crash reports and the restart of
  the window system; `nettest` checks DNS, UDP, TCP, HTTP and ping over
  Ethernet or Wi-Fi.
* Network host tests: the 802.11 protocol and its cryptography against
  published vectors, a station against an access point, two TCP/IP stacks
  over a simulated cable, and the Wi-Fi simulator.
* GUI automation scripts (`tests/ui/*.vts`) that drive QEMU through QMP —
  mouse, keyboard, waits on log lines, screenshots — and fail on panics.

`cargo xtask test --ui` runs all three.

## Repository layout

| Path | Contents |
|------|----------|
| `boot/` | UEFI loader |
| `kernel/` | microkernel |
| `lib/abi`, `lib/bootinfo`, `lib/initrd`, `lib/pe` | shared formats and the kernel ABI |
| `lib/rt`, `lib/heap`, `lib/build` | user runtime, allocator, build helper |
| `lib/math`, `lib/raster`, `lib/font`, `lib/image` | math, vector rasterisation, fonts, image codecs |
| `lib/ipc`, `lib/proto` | message encoding and the service protocols |
| `lib/gfx`, `lib/ui`, `lib/text` | 2D drawing, the GUI toolkit, the text editing model |
| `lib/files` | files for applications: paths, file types and the apps that open them, formatting, VFS access, thumbnails |
| `lib/v3d` | the fixed-point software 3D renderer and the game harness |
| `lib/audio` | audio formats, resampling, mixing, FFT and the synthesiser |
| `lib/virtio` | virtio device access shared by the drivers |
| `lib/entropy` | the ChaCha20 random number generator and BLAKE2s entropy pool |
| `lib/netstack`, `lib/net` | the TCP/IP stack around smoltcp, and the networking API for applications |
| `lib/wlan`, `lib/radiolink` | IEEE 802.11 (frames, RSN, handshakes, SAE, station and access point), and the virtual radio's link format |
| `third_party/` | vendored crates with documented patches (smoltcp) |
| `services/`, `drivers/`, `apps/` | system services, drivers and applications (the games included) |
| `tests/` | in-system tests and GUI automation scripts |
| `tools/` | host programs: media generators, `airsim` (the simulated Wi-Fi environment) |
| `xtask/` | build orchestration, disk image creation, QEMU automation |
| `assets/` | fonts and other data shipped in the initrd |
| `docs/` | documentation |
