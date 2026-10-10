//! `kmspause` — pauses Veda's display driver for Linux (`kms`) for a few
//! seconds once it has set a display's mode, for the compositor's test of
//! a driver that stops answering (`tests/ui/drivervm-display-pause.vts`):
//! a flip asked for meanwhile waits longer than the compositor waits, and
//! is carried out once the driver goes on. It says `kmspause: kms paused`
//! and `kmspause: kms goes on`.

use std::time::{Duration, Instant};

/// How long kms is paused: longer than the compositor waits for a flip
/// (a second).
const PAUSE: Duration = Duration::from_secs(4);
/// How long the compositor's frames have been flipped before: the
/// startup sequence is over by then.
const SETTLE: Duration = Duration::from_secs(5);
/// How long to wait for kms to set a mode.
const WAIT: Duration = Duration::from_secs(120);
const SIGCONT: i32 = 18;
const SIGSTOP: i32 = 19;

/// The process id of `kms`, if it runs.
fn kms() -> Option<i32> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: i32 = e.file_name().to_str()?.parse().ok()?;
        let name = std::fs::read_to_string(e.path().join("comm")).ok()?;
        (name.trim() == "kms").then_some(pid)
    })
}

/// Whether a display is on: a connector with a CRTC (kms set its mode).
fn display_on() -> bool {
    std::fs::read_dir("/sys/class/drm")
        .map(|d| {
            d.flatten().any(|e| std::fs::read_to_string(e.path().join("enabled")).is_ok_and(|s| s.trim() == "enabled"))
        })
        .unwrap_or(false)
}

fn main() {
    let start = Instant::now();
    while !display_on() {
        if start.elapsed() > WAIT {
            println!("kmspause: no display was set up");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(SETTLE);
    let Some(pid) = kms() else {
        println!("kmspause: kms does not run");
        return;
    };
    if let Err(e) = guest_sys::kill(pid, SIGSTOP) {
        println!("kmspause: cannot pause kms: {e}");
        return;
    }
    println!("kmspause: kms paused");
    std::thread::sleep(PAUSE);
    match guest_sys::kill(pid, SIGCONT) {
        Ok(()) => println!("kmspause: kms goes on"),
        Err(e) => println!("kmspause: cannot let kms go on: {e}"),
    }
}
