//! `init` — the first program of Linux in Veda's driver VM.
//!
//! It mounts the kernel's file systems, says on the console (which is
//! Veda's log) how Linux came up, and starts Veda's drivers for Linux that
//! the guest's devices need (`alsa` for sound cards, `net` for Ethernet
//! cards, `wifi` for Wi-Fi radios and `airlink` for QEMU's virtual one,
//! `kms` for displays; with `veda.usb`, `usbip` for the USB devices Veda
//! lends), again if they end.
//! Then it runs the programs the kernel's command line names
//! (`veda.run=bridgetest,...`, from `/bin`, one after the other) and stays:
//! the first program may never end. With `veda.poweroff` it powers the
//! machine off once they are done instead, for the tests of the driver VM;
//! with `veda.crash=SECONDS` Linux crashes (a panic) that long after it
//! started, for the tests of its restart.

use std::process::Command;
use std::time::Duration;

/// Veda's drivers for Linux, the PCI classes (their first bytes, as sysfs
/// gives them) of the functions each serves (multimedia devices, Ethernet
/// cards, other network cards (Wi-Fi's), the communication controllers
/// whose ports QEMU's virtual radio is on, and displays), and whether USB
/// devices Veda lends may be its (network adapters, Wi-Fi adapters).
const DRIVERS: &[(&str, &[&str], bool)] = &[
    ("alsa", &["0x04"], false),
    ("net", &["0x0200"], true),
    ("wifi", &["0x0280", "0x0780"], true),
    ("airlink", &["0x0780"], false),
    ("kms", &["0x03"], false),
];

/// Whether the guest has a PCI function of one of `classes`.
fn has_class(classes: &[&str]) -> bool {
    std::fs::read_dir("/sys/bus/pci/devices")
        .map(|d| {
            d.flatten().any(|f| {
                std::fs::read_to_string(f.path().join("class")).is_ok_and(|c| classes.iter().any(|k| c.starts_with(k)))
            })
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
    let usb = cmdline.split_whitespace().any(|o| o == "veda.usb");
    if usb {
        keep_running("usbip");
    }
    for &(name, classes, over_usb) in DRIVERS {
        if has_class(classes) || (usb && over_usb) {
            keep_running(name);
        }
    }
    let options: Vec<&str> = cmdline.split_whitespace().collect();
    if let Some(seconds) = options.iter().find_map(|o| o.strip_prefix("veda.crash=")?.parse().ok()) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            println!("veda: crashing, as asked");
            let _ = std::fs::write("/proc/sysrq-trigger", "c");
        });
    }
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
