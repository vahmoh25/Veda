# Developing Veda

This guide explains how to build, run and test Veda, and how to write
programs for it. Read `docs/CODING.md` (conventions) and
`docs/ARCHITECTURE.md` (design) first.

## Everyday commands

```text
cargo xtask build              # build everything -> target/veda/veda.img
cargo xtask iso                # the live system for a USB stick -> target/veda/veda.iso
cargo xtask run                # build and boot in a QEMU window
cargo xtask run --live         # boot the live system in QEMU from a virtual USB stick
cargo xtask run --smp 2 --memory 2048
cargo xtask shot --wait 15     # boot headless, save target/veda/screen.png
cargo xtask shot --cmdline "run=about"   # also start bin/about.exe at boot
cargo xtask script tests/ui/about-interaction.vts   # scripted GUI test
cargo xtask test               # host unit tests + in-system integration tests
cargo xtask test --ui          # ... plus every GUI script in tests/ui and tests/agent, several at once
cargo xtask test --ui --jobs 2 # ... two at a time (default: one per five processors)
cargo xtask scripts tests/ui/drivervm-audio.vts tests/ui/nvme.vts   # some scripts, built once, side by side
cargo xtask script docs/screenshots.vts   # retake the README screenshots
cargo xtask run --disk-bus ahci             # QEMU with SATA disks
cargo xtask run --disk-bus nvme             # ... with NVM Express disks
cargo xtask script tests/ui/nvme.vts        # starting from NVMe, the home directory on NVMe
cargo xtask run --sound hda    # Intel HD Audio, as most PCs have
cargo xtask script tests/ui/drivervm-audio.vts   # HD Audio, Linux's: music plays and reaches the recording
cargo xtask run --input usb    # USB keyboard, tablet (behind a hub) and mouse, no PS/2
cargo xtask script tests/ui/usb-input.vts   # USB input, with devices plugged in and out
cargo xtask script tests/agent/sim-basics.vts   # the agent, with a stand-in for Deepgram
cargo xtask script tests/real/agent-wake.vts    # ... with the real Deepgram ($DEEPGRAM_API_KEY)
cargo xtask run --net wifi     # boot with the virtual Wi-Fi radio and airsim
cargo xtask run --net both     # ... plus the wired card (or --net none)
cargo xtask script tests/ui/drivervm-wifi-recovery.vts   # Wi-Fi failure-recovery test
cargo xtask run --no-gpu       # QEMU without the 3D GPU (OpenGL ES renders in software)
cargo xtask script tests/ui/drivervm-display.vts   # the window system's flips, on QEMU's VGA, which Linux drives
cargo xtask script tests/ui/drivervm-compose.vts   # ... its frames drawn by a GPU (the renderer on softpipe; slow)
cargo xtask script tests/ui/drivervm-gpu.vts       # Prism on the host's GPU, through Mesa's virgl in the driver VM
cargo xtask script tests/ui/prism.vts      # OpenGL ES: Prism renders (on the GPU if QEMU has one)
VGL_TEST_BACKEND=virgl cargo test -p vgl    # the OpenGL ES tests on the host's GPU (desktop OpenGL)
VGL_TEST_HOST=gles VGL_TEST_BACKEND=virgl cargo test -p vgl      # ... on OpenGL ES
VGL_TEST_RENDERNODE=/dev/dri/renderD129 VGL_TEST_BACKEND=virgl cargo test -p vgl   # ... on another GPU
VGL_TEST_BACKEND=gallium cargo test -p vgl  # ... through Veda's renderer on Mesa's softpipe (vgallium.so)
VR_DEPTH_LOW=1 VGL_TEST_BACKEND=gallium cargo test -p vgl   # ... with depth kept as on iris (lower 24 bits)
VGL_TEST_BACKEND=gallium VGL_GALLIUM_DEVICE=iris cargo test -p vgl -p prism-scene   # ... on this PC's Intel GPU, as `cargo xtask test` does where there is one
cargo xtask script tests/ui/drivervm-renderer.vts   # Prism through the renderer in the driver VM, on softpipe
cargo xtask toolchain          # build the C toolchain from ports/ (GCC, binutils, musl) -> target/toolchain
cargo xtask script tests/ui/c-compile.vts   # GCC inside Veda: write, compile and run a C program
cargo xtask linux              # build the driver VM's Linux, its toolchain and Mesa again -> target/linux
cargo xtask run --no-iommu     # QEMU without its IOMMU: no device goes to the driver VM
cargo xtask run --cmdline "drivervm=off"    # ... or no driver VM at all
cargo xtask script tests/ui/drivervm-pci.vts   # the driver VM's Linux drives QEMU's HD Audio, behind the IOMMU
cargo xtask script tests/ui/drivervm-unplaced.vts   # ... and its xHCI, their BARs left unplaced for devmgr to place
cargo xtask script tests/ui/drivervm-gpio.vts   # GPIO pins for Linux: a simulated controller's, their interrupts
cargo xtask script tests/ui/drivervm-variables.vts   # the firmware's variables for Linux, read-only, through efivarfs
cargo xtask script tests/ui/drivervm-nhlt.vts   # an Intel audio controller's NHLT goes to Linux with it
cargo xtask acpi target/acpi   # what devmgr makes of a PC's ACPI tables (sudo cp -r /sys/firmware/acpi/tables target/acpi)
cargo xtask script tests/ui/drivervm-wifi.vts  # Wi-Fi through Linux's 802.11 stack, against airsim's networks
```

* The serial console (kernel log plus every program's `println!`) is saved
  to `target/veda/serial.log` by `shot`/`script`/`test`.
* QEMU's display is its standard VGA, and beside it a 3D GPU
  (`virtio-gpu-gl-pci`: virtio-gpu with virgl) when its build has one, as
  distributions' do (on Debian and Ubuntu with `qemu-system-modules-opengl`):
  the window then uses `-display gtk,gl=on` (the host's desktop OpenGL),
  and headless runs `-display egl-headless` (the host's EGL, on the GPU the
  firmware showed its screen on, where the host has several, such as a
  laptop's integrated one, or on the one whose render node
  `VEDA_RENDERNODE` names, such as `/dev/dri/renderD129`). `--no-gpu`
  leaves the GPU out and `--gpu` insists on it. OpenGL ES programs render on it through Veda's renderer, in the
  driver VM, which gets it, and in software without it.
* Where QEMU has the 3D GPU, `cargo xtask test` also runs the OpenGL ES
  tests on the host's GPU, through the system's virglrenderer
  (`libvirglrenderer1` on Debian and Ubuntu), which QEMU runs: on desktop
  OpenGL, as QEMU renders, and on OpenGL ES, through EGL on headless
  QEMU's GPU (`VGL_TEST_RENDERNODE` names another).
  Once the driver VM's Linux is built (`cargo xtask linux`, which builds
  Mesa for the host too), the tests also run through Veda's renderer: on
  softpipe, and on Mesa's virgl over virglrenderer's test server
  (`virgl_test_server`, in `virgl-server` on Debian and Ubuntu), as the
  driver VM's renderer renders under QEMU.
  `PRISM_SIZE=960x600 PRISM_SHOT=soft.png PRISM_GPU_SHOT=gpu.png cargo test
  -p prism-scene` saves Prism's frame as both renderers draw it.
* QEMU's window (GTK) takes the pointer as absolute from power-on: the
  tablet is bound to the display, so a click never grabs the mouse.
* `--cmdline "run=NAME"` makes `init` start `/system/bin/NAME.exe` after the
  system services, which is the quickest way to test an application.
* `cargo xtask iso` writes a hybrid ISO image (`xtask/src/image/iso9660.rs`)
  that boots with UEFI from a disc, from a stick Rufus made from it in ISO
  mode, and from a stick it was written to as it is. Its boot configuration
  puts `live` on the kernel command line (no disk driver starts; `/home`
  stays in memory), asks for 1920x1080 or the screen's largest mode below
  it (`--resolution` changes it), and adds no time zone, as a PC's clock
  keeps local time under Windows. Scripts boot it with a `live` line
  (`tests/ui/live-usb.vts`); `--out FILE` writes it elsewhere.
* A stick the live system started from keeps the logs of its last eight
  starts: `VEDA/LOGS/NNNN/VEDA.TXT` (Veda's log, Linux's console among it)
  and `LINUX.TXT` (the driver VM's Linux's whole log, its display drivers'
  debugging messages too), synced every second, so they hold what
  happened even when the screen showed nothing. Read them on any computer
  (the stick's EFI system partition mounts as `VEDA`). Scripts boot from a
  stick that can be written to with `live writable`, and check what it
  keeps with `expect-stick` (`tests/ui/live-logs.vts`).
* `--input usb` replaces QEMU's PS/2 controller and virtio tablet with
  USB devices on an xHCI controller: a hub with a keyboard and a tablet,
  and a mouse. Scripts ask for
  it with an `input usb` line, and plug devices in and out with `qmp`
  commands (`qmp device_add '{"driver":"usb-kbd","bus":"xhci.0","port":"2.3","id":"kbd2"}'`
  and `qmp device_del '{"id":"kbd2"}'`). Linux, which drives them in the
  driver VM, logs each device it finds (`usb 1-2.3: new full-speed USB
  device ...`, then its product's name), and Veda's input driver for Linux
  each input device (`input: QEMU QEMU USB Keyboard (usb 0627:0001):
  keyboard`).
* Headless runs record audio to `target/veda/audio.wav`. A script's
  `expect-audio-gapless` fails if that sound drops out while it plays
  (digital silence over 20 ms between the first sound and the last),
  which is what a driver that falls behind sounds like.
* At the start, `devmgr: coming: the display's driver: ...; sound: ...`
  says what the window system and the audio service wait for (the devices
  given to the driver VM, or `none`), and `compositor: the splash comes to
  life (held N ms)` when the display's driver showed its first picture
  (or `... did not show its first picture in 20 s` if it never did). The
  sequence then logs `compositor: the desktop appears (N ms after the
  splash came to life)` (with `the shell did not say the desktop may
  appear` first if the startup sound was not ready in time), and ends with
  `compositor: desktop shown after N ms (H ms held; F frames, R a second;
  ...)`, counted from the window system's start: how long the splash
  stayed (held still, then alive at least 1.8 s and until the desktop has
  drawn itself), how smoothly it ran, and where its frames went: into the
  firmware's framebuffer, or flipped by a display driver, with how many
  flips over how many vertical blanks (as many as there were blanks: not
  a frame missed), and whether the GPU or the processor drew them. Where
  a driver flips, `compositor: frames are drawn by the GPU (...) from now
  on` says when the GPU took over, or `frames stay with the processor:`
  why it did not; a frame the GPU took long over is logged too. The boot
  loader's splash is reported by the kernel (`boot: the loader painted its
  splash in N ms; ...`, with the framebuffer's memory type: firmware
  leaves it uncached, and the loader paints it write-combining).
  `shell: startup sound: playing` says
  the sound started (as the desktop appeared), or why not.
  `tests/ui/startup.vts` checks both, and that the sound reaches the
  recording. Its look is in
  `services/compositor/src/startup.rs` (timings, colours, the tagline);
  the sound is `tools/musicgen/src/songs/startup.rs`. `startup-sound=off`
  on the kernel command line (`--cmdline`, or the `BOOT.CFG` of an image)
  leaves the sound out; the scripts that check a recording of the sound
  output, or feed it back into the microphone, boot with it.
* Displays and GPUs are Linux's, in the driver VM, as are input devices,
  USB and networks: `dmesg drivervm` shows what Linux and Veda's drivers
  for Linux say (the display `kms` drives, its mode, whether it shows the
  compositor's pictures as they are or copies them, and scaled to what; the
  GPU the renderer serves `gpu` on; the input devices `input` takes, the
  cards `net` attaches), and `dmesg compositor` the display attaching.
  QEMU's VGA is a display that flips once Linux drives it.
  `expect-same A.png B.png [left top right bottom]` fails unless two
  screenshots are alike, pixel for pixel, in a region (fractions of the
  screen): `tests/ui/drivervm-display.vts` checks with it that a window
  that came and went left nothing behind in either picture.
  Screenshots that a script saves under `target/veda/` go to `$VEDA_OUT`
  when that is set.
* `--sound hda` gives QEMU the ICH9's HD Audio controller with QEMU's
  codec (`hda-output` when recording to a WAV file, `hda-duplex`, with a
  line input, otherwise), which Linux drives in the driver VM, as it does
  a PC's sound. When sound plays wrongly on a PC, Linux's log, which
  Veda's carries (`drivervm: linux:` lines), says what Linux made of it:
  `dmesg snd_hda` its controllers and codecs, `dmesg cs35l41` a laptop's
  speaker amplifiers (their firmware, the speaker id, which codec they
  bound to), `dmesg alsa` Veda's driver for Linux (the card it attached,
  and underruns).
* `dmesg devmgr` starts with what the firmware's ACPI tables gave
  (`devmgr: acpi: N tables, ...`, and the conditional definitions left
  out when their condition reads what the interpreter does not), then,
  for each driver's device with devices of its own in the tables, which
  ones (`acpi: 00:1e.3 is \_SB.PC00.SPI1, with ...`). `gpio:` lines show
  a GPIO controller's register windows and every pad `devmgr` set up for
  a driver, with its configuration before and after, and the pins that
  interrupt; the driver VM's monitor says which of them the guest has
  (`'s GPIO pin 303 interrupts on the guest's GSI 256`), and Linux's
  controller how many (`veda-gpio VEDA0001:00: 4 pins, 1 interrupting`).
  The loader says how many of the firmware's variables it took (`[vboot]
  variables: ...`, on the serial port only), the monitor how many the
  guest has and which it read (`drivervm: the guest read the firmware
  variable ...`), and which of the firmware's tables come with a function
  (`... comes with the firmware's NHLT (7054 bytes)`), the memory the
  firmware keeps for one (`the memory the firmware keeps for it at
  0x64000000-0x687fffff`), and what an Intel GPU gets besides (`its
  OpRegion is the guest's at 0xc0000`, `the PC's host bridge (8086:4621)
  is at the guest's 00:00.0`); i915 says which firmware it loaded and how
  it found the stolen memory. Linux's SOF says
  what it found (`DMICs detected in NHLT tables: 4`, the topology it
  loads), and `alsa` which device it records (`records
  (/dev/snd/pcmC0D6c, 4 channels)`).
  Before booting a machine, `cargo xtask acpi
  DIR` shows what devmgr makes of its ACPI tables, on the host, with the
  interpreter devmgr runs: copy them first (`sudo cp -r
  /sys/firmware/acpi/tables DIR`, under Linux). It lists the PCI root
  bridges' windows (where devmgr places BARs the firmware left unplaced),
  what the motherboard reserves, and for each function on a root bus its
  ACPI device, its INTx route and the devices below it, with anything
  that could not be evaluated. The variables the tables keep in the
  firmware's memory, which Linux does not show, read as zeros (so devices
  they enable may be missing), but those given with `--set NAME=VALUE`; PCI configuration space comes from
  `DIR/pci/BB:DD.F` files (`/sys/bus/pci/devices/*/config`; all of it as
  root), else from this machine.
* The agent's scripts (`tests/agent/`) talk to a stand-in for Deepgram
  that xtask starts on the host, and feed the agent's microphone from the
  host (`testmic`). The scripts in `tests/real/` talk to the real Deepgram
  with the key in `DEEPGRAM_API_KEY` (`export DEEPGRAM_API_KEY=...`);
  they type it into Settings with
  `type-env`, so it is in neither the script nor the logs. See
  [The agent](AGENT.md#testing).
* With `--net wifi`, xtask builds and starts `airsim` (the simulated Wi-Fi
  environment) next to QEMU; it logs to `target/veda/airsim.log` and
  prints its control port, which takes commands such as `ap home off`,
  `ap home signal -80`, `wired down` or `dns servfail` (see
  [NETWORKING.md](NETWORKING.md)). Scripts use the same commands with
  `air`, `air-expect` and `air-wait` after a `net wifi` line.
* `--cmdline "run=nettest"` checks the Internet connection from inside
  Veda; `run=nettest:wifi` first joins the simulated "Veda Home"
  network (`run=NAME:ARG1,ARG2` passes arguments).
* The home directory is kept on its own disk, `target/veda/home.img`,
  across builds and runs; `--fresh-home` starts over with a new one.
  Scripted runs (`shot`, `script`, `test`) always use a fresh home disk.
* `--no-generate` reuses the media in `target/generated` instead of running
  the generators; `--skip PROGRAM` leaves a program out of the image (both
  help while a component is being worked on).
* Several builds can run concurrently if each uses its own directories:
  set `CARGO_TARGET_DIR` (cargo output) and `VEDA_OUT` (disk image, serial
  log, screenshots) to private paths, e.g.
  `export CARGO_TARGET_DIR=target/agent-x VEDA_OUT=target/agent-x/veda`.
  Paths in automation scripts (`shot FILE`) are relative to the repository.

## The system image

`cargo xtask build` puts these into the initrd, mounted at `/system`:

| Source | Installed as |
|--------|--------------|
| every program listed in `xtask/src/components.rs` (`PROGRAMS`) | `/system/bin/<name>.exe` |
| `LICENSE` (the GNU General Public License, version 3) | `/system/LICENSE.txt` |
| everything under `assets/` | `/system/<same relative path>` |
| everything under `target/generated/` (build-time generated media) | `/system/<same relative path>` |
| `tests/c/*.c` and `*.cc`, compiled by the cross toolchain (each also as `<name>-pie`) | `/system/tests/c/<name>` |
| the native C toolchain, `target/toolchain/native/system` | `/system/bin/gcc` ..., `/system/include`, `/system/lib`, `/system/libexec` |

The last two need `cargo xtask toolchain` (see [C on Veda](C.md)); without
it, the build says so and leaves them out. With `cargo xtask linux` done,
the image also holds the driver VM's Linux: `/system/linux/bzImage` and
`/system/linux/initramfs.cpio`, made of the programs in `guest/` (see
[the driver VM](DRIVERVM.md)). Files under `samples/` in the
system image are also copied into the user's
home directory at boot (`/system/samples/Pictures/x.png` →
`/home/user/Pictures/x.png`).

## Writing an application

1. Create a crate under `apps/<name>`, add it to the
   workspace `members` in `Cargo.toml` and to `PROGRAMS` in
   `xtask/src/components.rs`.
2. `Cargo.toml` depends on `vrt`, `vabi`, `vui` (and others as needed) and has
   `vbuild` as a build dependency; `build.rs` is `fn main() { vbuild::user_program(); }`.
3. `src/main.rs`:

```rust
#![no_std]
#![no_main]
extern crate alloc;

vrt::entry!(main);

struct MyApp { /* state */ }

impl vui::App for MyApp {
    fn update(&mut self, ui: &mut vui::Ui) {
        // Draw and handle input for one frame. ui.rect() is the client area.
        let r = ui.rect().centered(200, 40);
        if ui.primary_button(r, "Hello") { /* ... */ }
    }
}

fn main() -> i32 {
    let mut spec = vui::WindowSpec::new("My App", 800, 560);
    spec.app_id = "myapp".into();
    vui::run(spec, MyApp { })
}
```

4. Add a manifest `assets/apps/<name>.app` so the start menu lists it:

```text
name=My App
exe=/system/bin/myapp.exe
icon=document
category=Accessories
description=What it does
pinned=false
```

`icon` names are mapped to vector icons by `vui::Icon::by_name`.

5. Make it agent-compatible: implement `App::agent_info` (a summary and
   the actions it offers, with their risk), `App::agent_state` (what the
   window shows, as JSON) and `App::agent_invoke` (run an action through
   the same code as the keyboard and mouse). `vui::run` registers the
   application with the agent; see
   [The agent](AGENT.md#making-an-application-agent-compatible).

### The `vui` toolkit in brief

* `Ui` is immediate mode: widgets take explicit `Rect`s and return their
  interaction (`button` → `bool`, `text_input` → `TextInputResponse`,
  `list` → `ListResponse`, `menu_bar` → chosen item, ...). `vgfx::Rect` has
  `split_top/left/right/bottom`, `inset`, `centered` for layout.
* `ui.canvas` is a `vgfx::Canvas` for custom drawing (fills, gradients,
  rounded rectangles, paths, bitmaps, shadows); `ui.ctx.text` draws text.
* `ui.input` holds this frame's input (pointer, buttons, keys, typed text,
  scroll). Key codes are `vproto::input::keys`.
* Animations: call `ui.repaint_at(deadline_ns)` (or `ui.repaint()`); frames
  are otherwise drawn only when events arrive.
* The window: `ui.set_title`, `ui.window_state()` and
  `ui.set_window_state` (maximise, minimise, full screen), `ui.activate()`
  (bring it to the front), `ui.close_window()`.
* Long-running work must not block `update`: use threads
  (`vrt::thread::spawn`) and signal the UI through an `Event` returned from
  `App::wait_handles`.
* Files: `vproto::connect(vproto::vfs::NAME)` then `vproto::vfs::Client`
  (`read_file` returns a VMO plus length, `write_file`, `read_dir`, ...).
* Starting other programs: `vproto::launcher::Client` (`launch`, `launch_app`).

### Programs without `vui`

Games and other full-screen renderers can use `vui::window::Window`
directly: `begin_frame()` returns a `Canvas` for the back buffer,
`present()` shows it, `poll_events()` returns `WindowEvent`s and
`can_draw()` says when the compositor is ready for the next frame.

### Programs in C

C programs are compiled with GCC, inside Veda or on the development machine
with the cross compiler, `target/toolchain/cross/bin/x86_64-veda-gcc`:

```text
target/toolchain/cross/bin/x86_64-veda-gcc -O2 -Wall -o hello hello.c
```

They are static ELF executables on musl and run from the Terminal like any
command. [C on Veda](C.md) describes the environment, what is different
from Linux, and how the toolchain is built and tested. The test programs
in `tests/c/` are compiled by `cargo xtask build` and installed in
`/system/tests/c`, for `systest` and the GUI scripts to run.

## Performance notes

Veda usually runs under QEMU's TCG emulator (no hardware
virtualisation), which is several times slower than native code. Keep
per-frame work proportional to what changed, avoid per-pixel floating point
in hot loops, and render 3D scenes at a modest internal resolution.
