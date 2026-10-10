//! `logkeeper` — keeps the system's log on the stick the live system
//! started from, so that what happened can be looked into afterwards (on
//! another computer, or on this one started again), however the system
//! fared: with nothing on the screen, say. Two logs: Veda's (its programs',
//! its kernel's, and the console of Linux in the driver VM: Linux's
//! messages but the debugging ones), and Linux's own, whole (its drivers'
//! debugging messages too: its display drivers' say what they do).
//!
//! The stick is a USB disk with a FAT32 file system named `VEDA` that has
//! a `VEDA/LOGS` directory: the live system's image has one (`cargo xtask
//! iso`), so the stick it is written to keeps the logs; other disks are
//! left alone. Each start of the system gets a directory of its own there,
//! numbered (`VEDA/LOGS/0007`), with `VEDA.TXT` and `LINUX.TXT` in it; the
//! oldest go, so that [`KEPT`] starts' are kept at most, with room for
//! this one's. What is written is synced to the stick every second, so a
//! computer switched off (or that stopped responding) keeps all of it but
//! the last second.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where the stick is mounted.
const MOUNTED: &str = "/stick";
/// The stick's directory of logs, which says it keeps them.
const LOGS: &str = "VEDA/LOGS";
/// The starts whose logs are kept, this one's among them.
const KEPT: usize = 8;
/// The most a start's logs take: Veda's, Linux's.
const VEDA_LIMIT: u64 = 8 << 20;
const LINUX_LIMIT: u64 = 24 << 20;
/// Room on the stick the logs leave free.
const SPARE: u64 = 4 << 20;
/// How long the stick is looked for (a USB disk comes within seconds).
const SEARCH: Duration = Duration::from_secs(60);
/// How often the logs are read, and synced to the stick.
const READ_EVERY: Duration = Duration::from_millis(250);
const SYNC_EVERY: Duration = Duration::from_secs(1);
const O_NONBLOCK: i32 = 0o4000;
const EACCES: i32 = 13;
const EROFS: i32 = 30;
const EPIPE: i32 = 32;

/// Stays, doing nothing (init would start it again were it to end).
fn idle() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// The disks and partitions on USB (`sdb`, `sda1`), as sysfs has them.
fn usb_disks() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/block")
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let device = fs::read_link(e.path()).ok()?;
                    (name.starts_with("sd") && device.to_string_lossy().contains("/usb")).then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Whether `sector`, a volume's first, is that of a FAT32 file system
/// named `VEDA` (the live system's image has one, as has a stick its
/// files are copied onto).
fn is_veda(sector: &[u8; 512]) -> bool {
    sector[510..] == [0x55, 0xAA] && sector[82..90] == *b"FAT32   " && sector[71..82] == *b"VEDA       "
}

/// Mounts the stick at [`MOUNTED`]: the first USB disk with Veda's file
/// system that keeps logs. Looks for it for [`SEARCH`] at most; says why
/// one of Veda's is not.
fn mount_stick() -> Option<String> {
    let _ = fs::create_dir_all(MOUNTED);
    let start = Instant::now();
    let mut seen: Vec<String> = Vec::new();
    while start.elapsed() < SEARCH {
        for name in usb_disks() {
            if seen.contains(&name) {
                continue;
            }
            let device = format!("/dev/{name}");
            let mut sector = [0u8; 512];
            match File::open(&device).and_then(|mut f| f.read_exact(&mut sector)) {
                Ok(()) => seen.push(name.clone()),
                // Its node may not be there yet.
                Err(_) => continue,
            }
            if !is_veda(&sector) {
                continue;
            }
            match guest_sys::mount(&device, MOUNTED, "vfat") {
                Ok(()) if Path::new(MOUNTED).join(LOGS).is_dir() => return Some(name),
                Ok(()) => {
                    println!("logkeeper: {name} has Veda's files but no {LOGS}: it is left alone");
                    let _ = guest_sys::unmount(MOUNTED);
                }
                Err(e) if matches!(e.raw_os_error(), Some(EROFS | EACCES)) => {
                    println!("logkeeper: {name} has Veda's files but cannot be written to");
                }
                Err(e) => println!("logkeeper: cannot mount {name}: {e}"),
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// The numbers of the starts whose logs a directory of `names` holds, in
/// order.
fn starts(names: impl Iterator<Item = String>) -> Vec<u32> {
    let mut starts: Vec<u32> = names
        .filter(|n| (1..=8).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|n| n.parse().ok())
        .collect();
    starts.sort_unstable();
    starts
}

/// Has what directory `path` lists reach the stick (a FAT file system
/// keeps a file's place there, in its directory's entries, which syncing
/// the file does not write).
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// The directory for this start's logs, in `logs`: numbered after the
/// last start's. The oldest starts' go first, so that [`KEPT`] are left
/// with this one, and room for its logs.
fn new_start(logs: &Path) -> io::Result<PathBuf> {
    let mut kept = starts(fs::read_dir(logs)?.flatten().filter_map(|e| e.file_name().into_string().ok()));
    let number = kept.last().map_or(1, |n| n + 1);
    let room = VEDA_LIMIT + LINUX_LIMIT + SPARE;
    while !kept.is_empty() && (kept.len() >= KEPT || guest_sys::free_bytes(MOUNTED)? < room) {
        let oldest = kept.remove(0);
        fs::remove_dir_all(logs.join(format!("{oldest:04}")))?;
    }
    let dir = logs.join(format!("{number:04}"));
    fs::create_dir(&dir)?;
    sync_dir(logs)?;
    Ok(dir)
}

/// A log being kept on the stick.
struct Kept {
    file: File,
    name: String,
    written: u64,
    limit: u64,
    /// What was written since the last sync.
    unsynced: bool,
    /// Nothing more is written (it is as large as it may be, or the stick
    /// failed).
    done: bool,
}

impl Kept {
    fn create(path: PathBuf, limit: u64) -> io::Result<Kept> {
        let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        let name = path.strip_prefix(MOUNTED).unwrap_or(&path).display().to_string();
        Ok(Kept { file, name, written: 0, limit, unsynced: false, done: false })
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.done || bytes.is_empty() {
            return;
        }
        if self.written + bytes.len() as u64 > self.limit {
            let note = format!("logkeeper: the log reached {} MiB: the rest is not kept\n", self.limit >> 20);
            let _ = self.file.write_all(note.as_bytes());
            self.unsynced = true;
            self.done = true;
            return;
        }
        match self.file.write_all(bytes) {
            Ok(()) => {
                self.written += bytes.len() as u64;
                self.unsynced = true;
            }
            Err(e) => {
                println!("logkeeper: cannot write {}: {e}; it is kept no more", self.name);
                self.done = true;
            }
        }
    }

    fn sync(&mut self) {
        if self.unsynced {
            self.unsynced = false;
            if let Err(e) = self.file.sync_all() {
                println!("logkeeper: cannot sync {}: {e}", self.name);
            }
        }
    }
}

/// Veda's log, read on from where reading it left off.
struct VedaLog {
    offset: u64,
    buf: Vec<u8>,
    failed: bool,
}

impl VedaLog {
    fn take(&mut self, into: &mut Kept) {
        loop {
            match vrt::object::log_read(self.offset, &mut self.buf) {
                Ok((0, _)) => return,
                Ok((n, next)) => {
                    let from = next - n as u64;
                    if from > self.offset {
                        let note = format!(
                            "logkeeper: {} bytes of the log were gone before they could be kept\n",
                            from - self.offset
                        );
                        into.write(note.as_bytes());
                    }
                    into.write(&self.buf[..n]);
                    self.offset = next;
                }
                Err(e) => {
                    if !self.failed {
                        self.failed = true;
                        println!("logkeeper: cannot read Veda's log: {e}");
                    }
                    return;
                }
            }
        }
    }
}

/// Linux's log (`/dev/kmsg`), from its oldest message on.
struct LinuxLog {
    file: File,
    buf: Vec<u8>,
}

impl LinuxLog {
    fn open() -> io::Result<LinuxLog> {
        let file = OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open("/dev/kmsg")?;
        // Room for the longest message.
        Ok(LinuxLog { file, buf: vec![0; 8192] })
    }

    fn take(&mut self, into: &mut Kept) {
        loop {
            match self.file.read(&mut self.buf) {
                Ok(0) => return,
                Ok(n) => {
                    if let Some(line) = message(&self.buf[..n]) {
                        into.write(line.as_bytes());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.raw_os_error() == Some(EPIPE) => {
                    into.write(b"logkeeper: messages of Linux's were gone before they could be kept\n");
                }
                Err(_) => return,
            }
        }
    }
}

/// A message of Linux's log as a line: when it came (seconds since Linux
/// started) and what it says. `/dev/kmsg` gives each as `LEVEL,NUMBER,
/// MICROSECONDS,FLAGS;TEXT`, then its dictionary's lines.
fn message(record: &[u8]) -> Option<String> {
    let record = String::from_utf8_lossy(record);
    let (head, rest) = record.split_once(';')?;
    let us: u64 = head.split(',').nth(2)?.parse().ok()?;
    let text = rest.split('\n').next()?;
    Some(format!("[{:5}.{:06}] {text}\n", us / 1_000_000, us % 1_000_000))
}

fn main() {
    let Some(device) = mount_stick() else {
        println!("logkeeper: no stick keeps the log: it stays in memory");
        idle();
    };
    let logs = Path::new(MOUNTED).join(LOGS);
    let made = new_start(&logs).and_then(|dir| {
        let veda = Kept::create(dir.join("VEDA.TXT"), VEDA_LIMIT)?;
        let linux = Kept::create(dir.join("LINUX.TXT"), LINUX_LIMIT)?;
        sync_dir(&dir)?;
        Ok((veda, linux, dir))
    });
    let (mut veda, mut linux, dir) = match made {
        Ok(made) => made,
        Err(e) => {
            println!("logkeeper: cannot keep the log on the stick ({device}): {e}");
            idle();
        }
    };
    let dir = dir.strip_prefix(MOUNTED).unwrap_or(&dir).display().to_string();
    println!("logkeeper: keeping the log on the stick ({device}), in {dir}");
    let t = vrt::time::DateTime::now();
    let when = format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02} (the system's clock)",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    );
    veda.write(format!("logkeeper: Veda's log of the start of {when}, from its beginning\n").as_bytes());
    linux.write(format!("logkeeper: the driver VM's Linux's log of the start of {when}, whole\n").as_bytes());
    let mut veda_log = VedaLog { offset: 0, buf: vec![0; 64 * 1024], failed: false };
    let mut linux_log = match LinuxLog::open() {
        Ok(log) => Some(log),
        Err(e) => {
            println!("logkeeper: cannot read Linux's log: {e}");
            None
        }
    };
    let mut synced = Instant::now();
    loop {
        veda_log.take(&mut veda);
        if let Some(log) = &mut linux_log {
            log.take(&mut linux);
        }
        if synced.elapsed() >= SYNC_EVERY {
            veda.sync();
            linux.sync();
            synced = Instant::now();
        }
        if veda.done && linux.done {
            idle();
        }
        std::thread::sleep(READ_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_messages_become_lines() {
        assert_eq!(
            message(b"6,1234,5678901,-;i915 0000:00:02.0: [drm] GT0: GUC: submission enabled\n").as_deref(),
            Some("[    5.678901] i915 0000:00:02.0: [drm] GT0: GUC: submission enabled\n")
        );
        // The dictionary's lines are left out; a record that is not one is.
        assert_eq!(
            message(b"7,5,12,-,caller=T1;usb 1-1: new device\n SUBSYSTEM=usb\n DEVICE=c189:1\n").as_deref(),
            Some("[    0.000012] usb 1-1: new device\n")
        );
        assert_eq!(message(b"garbage"), None);
    }

    #[test]
    fn the_starts_kept_are_numbered() {
        let names = ["0002", "0010", "0001", "LOST.DIR", "12345678", "123456789", ""].map(String::from);
        assert_eq!(starts(names.into_iter()), [1, 2, 10, 12_345_678]);
    }

    #[test]
    fn veda_has_a_fat32_file_system_named_so() {
        let mut sector = [0u8; 512];
        sector[71..82].copy_from_slice(b"VEDA       ");
        sector[82..90].copy_from_slice(b"FAT32   ");
        sector[510..].copy_from_slice(&[0x55, 0xAA]);
        assert!(is_veda(&sector));
        sector[71..82].copy_from_slice(b"NO NAME    ");
        assert!(!is_veda(&sector));
    }
}
