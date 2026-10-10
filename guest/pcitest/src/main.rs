//! `pcitest` — checks, inside the driver VM's Linux, the PCI functions
//! Veda gave it: each is on the guest's PCI with a driver of Linux's, and
//! described by an ACPI device of the platform's tables (with the devices
//! they describe below it, which it lists). An HD Audio controller's
//! codec has answered, which takes the controller's DMA both ways, and its
//! MSIs, which announce the answers, arrived (other functions interrupt
//! only when something happens). It says `pcitest: PASS` when all is well.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEVICES: &str = "/sys/bus/pci/devices";

struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool) {
        println!("pcitest: {} {}", if ok { "ok  " } else { "FAIL" }, what);
        if !ok {
            self.failed += 1;
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The functions on the guest's PCI.
fn functions() -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> =
        fs::read_dir(DEVICES).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    list.sort();
    list
}

/// The driver Linux bound to a function, if any.
fn driver(function: &Path) -> Option<String> {
    let link = fs::read_link(function.join("driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// The ACPI device that describes a function (its companion), and the
/// devices the tables describe below that one, with their ids.
fn described(function: &Path) -> (String, Vec<String>) {
    let companion = read(&function.join("firmware_node").join("path"));
    let mut below = Vec::new();
    if !companion.is_empty() {
        for d in fs::read_dir("/sys/bus/acpi/devices").into_iter().flatten().flatten() {
            let path = read(&d.path().join("path"));
            let child = path.strip_prefix(&companion).and_then(|r| r.strip_prefix('.'));
            if child.is_some_and(|r| !r.contains('.')) {
                below.push(format!("{} ({})", path, read(&d.path().join("hid"))));
            }
        }
        below.sort();
    }
    (companion, below)
}

/// Polls `done` for up to `timeout`.
fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// How many times interrupt `irq` came, on all processors.
fn interrupt_count(irq: &str) -> u64 {
    let table = fs::read_to_string("/proc/interrupts").unwrap_or_default();
    table
        .lines()
        .find_map(|l| {
            let (n, rest) = l.trim_start().split_once(':')?;
            (n == irq).then(|| rest.split_whitespace().map_while(|c| c.parse::<u64>().ok()).sum())
        })
        .unwrap_or(0)
}

fn main() {
    let mut c = Checks { failed: 0 };
    let found = wait_for(Duration::from_secs(10), || !functions().is_empty());
    c.check("the guest's PCI has functions", found);
    // Drivers bind as they probe.
    wait_for(Duration::from_secs(10), || functions().iter().all(|f| driver(f).is_some()));
    for f in functions() {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let id = format!(
            "{}:{}",
            read(&f.join("vendor")).trim_start_matches("0x"),
            read(&f.join("device")).trim_start_matches("0x")
        );
        let class = read(&f.join("class"));
        let driver = driver(&f);
        c.check(
            &format!("{name} {id} (class {class}) has a driver: {}", driver.as_deref().unwrap_or("none")),
            driver.is_some(),
        );
        let (companion, below) = described(&f);
        let with = if below.is_empty() { String::new() } else { format!(", with {}", below.join(", ")) };
        let is = if companion.is_empty() { "described by nothing" } else { &companion };
        c.check(&format!("{name} is {is}{with}"), !companion.is_empty());
        if class.starts_with("0x0403") {
            // The codec answered the controller: its commands went out
            // and its answers came back by DMA, announced by interrupts.
            let codec = Path::new("/proc/asound/card0/codec#0");
            let answered = wait_for(Duration::from_secs(10), || codec.exists());
            let what = fs::read_to_string(codec)
                .ok()
                .and_then(|t| t.lines().find(|l| l.starts_with("Codec:")).map(str::to_string))
                .unwrap_or_default();
            c.check(&format!("{name}'s codec answered ({what})"), answered);
            let irqs: Vec<String> = fs::read_dir(f.join("msi_irqs"))
                .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            let counts: Vec<u64> = irqs.iter().map(|i| interrupt_count(i)).collect();
            c.check(
                &format!("{name}'s MSIs arrive (interrupts {} came {:?} times)", irqs.join(", "), counts),
                !irqs.is_empty() && counts.iter().any(|&n| n > 0),
            );
        }
    }
    if c.failed == 0 {
        println!("pcitest: PASS");
    } else {
        println!("pcitest: FAIL ({} checks)", c.failed);
    }
}
