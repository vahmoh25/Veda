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
cargo xtask test --ui          # ... plus every GUI script in tests/ui
cargo xtask script docs/screenshots.vts   # retake the README screenshots
cargo xtask run --vm virtualbox            # the same in VirtualBox (VM "Veda")
cargo xtask run --vm virtualbox --net bridged   # ... on the host's real network
cargo xtask run --vm virtualbox --resolution 1600x1000 --scale 2   # larger desktop, window at 2x
cargo xtask test --ui --vm virtualbox      # the test suite in VirtualBox
cargo xtask run --disk-bus ahci             # QEMU with SATA disks (as VirtualBox has)
cargo xtask run --sound ac97   # QEMU with an AC'97 sound card (as VirtualBox has)
cargo xtask run --input usb    # USB keyboard, tablet (behind a hub) and mouse, no PS/2
cargo xtask script tests/ui/usb-input.vts   # USB input, with devices plugged in and out
cargo xtask script tests/agent/sim-basics.vts   # the agent, with a stand-in for Deepgram
cargo xtask script tests/real/agent-wake.vts    # ... with the real Deepgram ($DEEPGRAM_API_KEY)
cargo xtask run --net wifi     # boot with the virtual Wi-Fi radio and airsim
cargo xtask run --net both     # ... plus the wired card (or --net none)
cargo xtask script tests/ui/wifi-recovery.vts   # Wi-Fi failure-recovery test
```

* The serial console (kernel log plus every program's `println!`) is saved
  to `target/veda/serial.log` by `shot`/`script`/`test`.
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
* `--input usb` replaces QEMU's PS/2 controller and virtio tablet with
  USB devices on an xHCI controller: a hub with a keyboard and a tablet,
  and a mouse (in VirtualBox: a USB keyboard and tablet). Scripts ask for
  it with an `input usb` line, and plug devices in and out with `qmp`
  commands (`qmp device_add '{"driver":"usb-kbd","bus":"xhci.0","port":"2.3","id":"kbd2"}'`
  and `qmp device_del '{"id":"kbd2"}'`); the driver logs each device it
  finds as `xhci: port 6.3: ...` (root port, then hub ports).
* Headless runs record audio to `target/veda/audio.wav`.
* The agent's scripts (`tests/agent/`) talk to a stand-in for Deepgram
  that xtask starts on the host, and feed the agent's microphone from the
  host (`testmic`). The scripts in `tests/real/` talk to the real Deepgram
  with the key in `DEEPGRAM_API_KEY` (in PowerShell
  `$env:DEEPGRAM_API_KEY = "..."`); they type it into Settings with
  `type-env`, so it is in neither the script nor the logs. See
  [The agent](AGENT.md#testing).
* With `--vm virtualbox`, xtask creates the VirtualBox machine "Veda"
  (files in `target/veda/vbox`) on first use and updates it from the
  options on every run. Its disks are VMDK descriptors (`veda.vmdk`,
  `home.vmdk`) that point at the raw images, so VirtualBox and QEMU use the
  same files (not at the same time). The serial console goes to
  `target/veda/serial-vbox.log` and the terminal; Ctrl+C powers the
  machine off. The window enlarges the screen by the whole part of the
  host's display scaling, as QEMU's window (GTK) does, lowered until the
  window fits on the screen, and opens in the middle of the screen when the
  resolution or the scale changes; `--scale 2` or `--scale 250%` chooses
  the factor and `--scale 1` shows the screen pixel for pixel. Resolutions
  missing from the firmware's list become a custom video mode. Inside the
  window, View > Virtual Screen changes the scale, Host+F switches to full
  screen and Host+C to scaled mode, whose window can be resized freely.
  Scripts drive VirtualBox through `VBoxManage` (keys as PS/2 scan codes,
  screenshots) and its COM API (the mouse, through a helper PowerShell
  process); `test` skips scripts that need QEMU's simulated Wi-Fi.
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
  log, screenshots) to private paths, e.g. in PowerShell
  `$env:CARGO_TARGET_DIR="target/agent-x"; $env:VEDA_OUT="target/agent-x/veda"`.
  With `--vm virtualbox`, each output directory also gets its own machine
  (here "Veda-target-agent-x-veda").
  Paths in automation scripts (`shot FILE`) are relative to the repository.

## The system image

`cargo xtask build` puts these into the initrd, mounted at `/system`:

| Source | Installed as |
|--------|--------------|
| every program listed in `xtask/src/components.rs` (`PROGRAMS`) | `/system/bin/<name>.exe` |
| `LICENSE` (the GNU General Public License, version 3) | `/system/LICENSE.txt` |
| everything under `assets/` | `/system/<same relative path>` |
| everything under `target/generated/` (build-time generated media) | `/system/<same relative path>` |

Files under `samples/` in the system image are also copied into the user's
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

## Performance notes

Veda usually runs under QEMU's TCG emulator (no hardware
virtualisation), which is several times slower than native code. Keep
per-frame work proportional to what changed, avoid per-pixel floating point
in hot loops, and render 3D scenes at a modest internal resolution.
