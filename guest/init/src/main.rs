//! `init` — the first program of Linux in Veda's driver VM.
//!
//! It mounts the kernel's file systems, says on the console (which is
//! Veda's log) how Linux came up, and starts Veda's drivers for Linux that
//! the guest's devices need (`alsa` for sound cards), again if they end.
//! Then it runs the programs the kernel's command line names
//! (`veda.run=bridgetest,...`, from `/bin`, one after the other) and stays:
//! the first program may never end. With `veda.poweroff` it powers the
//! machine off once they are done instead, for the tests of the driver VM.

use std::process::Command;
use std::time::Duration;

/// Veda's drivers for Linux, and the PCI class (its first byte, as sysfs
/// gives it) of the functions each serves.
const DRIVERS: &[(&str, &str)] = &[("alsa", "0x04")];

/// Whether the guest has a PCI function of `class`.
fn has_class(class: &str) -> bool {
    std::fs::read_dir("/sys/bus/pci/devices")
        .map(|d| {
            d.flatten().any(|f| std::fs::read_to_string(f.path().join("class")).is_ok_and(|c| c.starts_with(class)))
        })
        .unwrap_or(false)
}

/// Runs driver `name` (from `/bin`) on a thread of its own, and again a
/// second after it ends.
fn keep_running(name: &'static str) {
    std::thread::spawn(move || {
        loop {
            match Command::new(format!("/bin/{name}")).status() {
                Ok(status) => println!("veda: {name} ended ({status}); starting it again"),
                Err(e) => {
                    println!("veda: cannot run {name}: {e}");
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

fn main() {
    for (source, target, kind) in
        [("proc", "/proc", "proc"), ("sysfs", "/sys", "sysfs"), ("devtmpfs", "/dev", "devtmpfs")]
    {
        let _ = std::fs::create_dir_all(target);
        if let Err(e) = guest_sys::mount(source, target, kind) {
            println!("veda: cannot mount {target}: {e}");
        }
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let cpus = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())
        })
        .unwrap_or_default();
    println!("veda: Linux {} is up, processors {}, {} KiB of memory", release.trim(), cpus.trim(), memory);
    for &(name, class) in DRIVERS {
        if has_class(class) {
            keep_running(name);
        }
    }
    let options: Vec<&str> = cmdline.split_whitespace().collect();
    for name in options.iter().filter_map(|o| o.strip_prefix("veda.run=")).flat_map(|l| l.split(',')) {
        match Command::new(format!("/bin/{name}")).status() {
            Ok(status) => println!("veda: {name} ended ({status})"),
            Err(e) => println!("veda: cannot run {name}: {e}"),
        }
    }
    if options.contains(&"veda.poweroff") {
        println!("veda: powering off, as asked");
        if let Err(e) = guest_sys::power_off() {
            println!("veda: cannot power off: {e}");
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
