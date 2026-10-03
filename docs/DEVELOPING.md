# Developing Vindows

This guide explains how to build, run and test Vindows, and how to write
programs for it. Read `docs/CODING.md` (conventions) and
`docs/ARCHITECTURE.md` (design) first.

## Everyday commands

```text
cargo xtask build              # build everything -> target/vindows/vindows.img
cargo xtask run                # build and boot in a QEMU window
cargo xtask run --smp 2 --memory 2048
cargo xtask shot --wait 15     # boot headless, save target/vindows/screen.png
cargo xtask shot --cmdline "run=about"   # also start bin/about.exe at boot
cargo xtask script tests/ui/about-interaction.vts   # scripted GUI test
cargo xtask test               # host unit tests + in-system integration tests
cargo xtask test --ui          # ... plus every GUI script in tests/ui
cargo xtask script docs/screenshots.vts   # retake the README screenshots
```

* The serial console (kernel log plus every program's `println!`) is saved
  to `target/vindows/serial.log` by `shot`/`script`/`test`.
* `--cmdline "run=NAME"` makes `init` start `/system/bin/NAME.exe` after the
  system services, which is the quickest way to test an application.
* Headless runs record audio to `target/vindows/audio.wav`.
* The home directory is kept on its own disk, `target/vindows/home.img`,
  across builds and runs; `--fresh-home` starts over with a new one.
  Scripted runs (`shot`, `script`, `test`) always use a fresh home disk.
* `--no-generate` reuses the media in `target/generated` instead of running
  the generators; `--skip PROGRAM` leaves a program out of the image (both
  help while a component is being worked on).
* Several builds can run concurrently if each uses its own directories:
  set `CARGO_TARGET_DIR` (cargo output) and `VINDOWS_OUT` (disk image, serial
  log, screenshots) to private paths, e.g. in PowerShell
  `$env:CARGO_TARGET_DIR="target/agent-x"; $env:VINDOWS_OUT="target/agent-x/vindows"`.
  Paths in automation scripts (`shot FILE`) are relative to the repository.

## The system image

`cargo xtask build` puts these into the initrd, mounted at `/system`:

| Source | Installed as |
|--------|--------------|
| every program listed in `xtask/src/components.rs` (`PROGRAMS`) | `/system/bin/<name>.exe` |
| everything under `assets/` | `/system/<same relative path>` |
| everything under `target/generated/` (build-time generated media) | `/system/<same relative path>` |

Files under `samples/` in the system image are also copied into the user's
home directory at boot (`/system/samples/Pictures/x.png` →
`/home/user/Pictures/x.png`).

## Writing an application

1. Create a crate under `apps/<name>` (or `games/<name>`), add it to the
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

Vindows usually runs under QEMU's TCG emulator (no hardware
virtualisation), which is several times slower than native code. Keep
per-frame work proportional to what changed, avoid per-pixel floating point
in hot loops, and render 3D scenes at a modest internal resolution.
