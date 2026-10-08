//! The Trash: what the user deletes goes here rather than away, and can be
//! put back where it was or deleted for good.
//!
//! The layout is the home trash of freedesktop.org's Trash specification,
//! as Linux desktops keep it: [`FILES`] holds the items, and [`INFO`] a
//! `NAME.trashinfo` for each, saying where it was and when it was deleted
//! (in local time):
//!
//! ```text
//! [Trash Info]
//! Path=/home/user/Documents/Notes%20for%20Monday.txt
//! DeletionDate=2026-10-08T14:03:12
//! ```
//!
//! An item keeps its name in the Trash unless an item there has it already
//! (then `notes (2).txt`). Its info is written first, as a file that must
//! not exist yet, which reserves the name against another program trashing
//! at the same time; then the item is renamed into `files` in one step
//! (`/home` and `/tmp` are one file system). Items in `files` without info
//! (put there some other way) have no known origin; info without an item
//! is ignored, and cleared away when the Trash is emptied.
//!
//! The parts that only handle text (info records, their paths and dates)
//! are pure and unit-tested on the host.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vrt::time::DateTime;

use crate::fs::{Error, Fs, FsError, Result};
use crate::path::{file_name, is_within, join, natural_cmp, parent, unique_name};

/// The Trash.
pub const DIR: &str = "/home/user/.local/share/Trash";
/// The items in the Trash.
pub const FILES: &str = "/home/user/.local/share/Trash/files";
/// What is known about each item: `NAME.trashinfo`.
pub const INFO: &str = "/home/user/.local/share/Trash/info";

const INFO_SUFFIX: &str = ".trashinfo";
const NS_PER_SEC: u64 = 1_000_000_000;
/// Names tried before giving up when other programs keep taking them.
const ATTEMPTS: usize = 64;

/// Where an item in the Trash was, and when it was deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Its path before it was deleted.
    pub origin: String,
    /// When it was deleted: nanoseconds since the epoch, local time (0 if
    /// not known).
    pub deleted: u64,
}

/// An item in the Trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Its name in [`FILES`].
    pub name: String,
    pub is_dir: bool,
    /// Bytes for a file, entries for a folder (as `DirEntry::size`).
    pub size: u64,
    /// `None` for an item put in the Trash without its info.
    pub info: Option<Info>,
}

impl Item {
    /// Its path in the Trash.
    pub fn path(&self) -> String {
        join(FILES, &self.name)
    }

    /// The name it had before it was deleted.
    pub fn original_name(&self) -> &str {
        self.info.as_ref().map(|i| file_name(&i.origin)).filter(|n| !n.is_empty()).unwrap_or(&self.name)
    }

    /// When it was deleted (0 if not known).
    pub fn deleted(&self) -> u64 {
        self.info.as_ref().map_or(0, |i| i.deleted)
    }
}

/// True for a path in the Trash, and for the folder of its items itself.
pub fn contains(path: &str) -> bool {
    is_within(path, FILES)
}

/// The name of the item in the Trash that `path` is or lies within.
pub fn item_name(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(FILES)?.strip_prefix('/')?;
    rest.split('/').next().filter(|n| !n.is_empty())
}

fn info_path(name: &str) -> String {
    format!("{INFO}/{name}{INFO_SUFFIX}")
}

/// Makes the Trash's folders if they are missing.
pub fn ensure(fs: &Fs) -> Result<()> {
    fs.mkdir_all(FILES)?;
    fs.mkdir_all(INFO)
}

/// Moves `path` to the Trash; returns its name there.
pub fn put(fs: &Fs, path: &str) -> Result<String> {
    if is_within(path, DIR) {
        return Err(Error::InTrash);
    }
    if is_within(DIR, path) {
        return Err(Error::HoldsTrash);
    }
    if fs.stat(path)?.read_only {
        return Err(Error::Fs(FsError::ReadOnly));
    }
    ensure(fs)?;
    let record = info_text(path, vrt::time::unix_time_ns());
    for _ in 0..ATTEMPTS {
        let name = unique_name(file_name(path), |n| fs.exists(&join(FILES, n)) || fs.exists(&info_path(n)));
        match fs.create_new(&info_path(&name), record.as_bytes()) {
            Ok(()) => {}
            Err(Error::Fs(FsError::Exists)) => continue,
            Err(e) => return Err(e),
        }
        match fs.rename(path, &join(FILES, &name)) {
            Ok(()) => return Ok(name),
            Err(e) => {
                let _ = fs.remove(&info_path(&name));
                if e != Error::Fs(FsError::Exists) {
                    return Err(e);
                }
            }
        }
    }
    Err(Error::Fs(FsError::Exists))
}

/// What is known about the item called `name`.
pub fn info(fs: &Fs, name: &str) -> Option<Info> {
    let bytes = fs.read(&info_path(name)).ok()?;
    parse_info(core::str::from_utf8(&bytes).ok()?)
}

/// The items in the Trash, most recently deleted first.
pub fn items(fs: &Fs) -> Result<Vec<Item>> {
    let entries = match fs.read_dir(FILES) {
        Ok(entries) => entries,
        Err(Error::Fs(FsError::NotFound)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut items: Vec<Item> = entries
        .into_iter()
        .map(|e| Item { info: info(fs, &e.name), name: e.name, is_dir: e.is_dir, size: e.size })
        .collect();
    items.sort_by(|a, b| b.deleted().cmp(&a.deleted()).then_with(|| natural_cmp(&a.name, &b.name)));
    Ok(items)
}

/// How many items are in the Trash.
pub fn count(fs: &Fs) -> usize {
    fs.read_dir(FILES).map_or(0, |e| e.len())
}

/// Puts the item called `name` back where it was: under a free name like
/// its own if that one is taken now, in its folder made again if that is
/// gone. Returns its path.
pub fn restore(fs: &Fs, name: &str) -> Result<String> {
    fs.stat(&join(FILES, name))?;
    let origin = info(fs, name)
        .map(|i| i.origin)
        .filter(|o| o.starts_with('/') && !file_name(o).is_empty() && !is_within(o, DIR))
        .ok_or(Error::UnknownOrigin)?;
    restore_to(fs, name, parent(&origin), file_name(&origin))
}

/// Moves the item called `name` out of the Trash into the folder `dir`, as
/// `base` or a free name like it. Returns its path.
pub fn restore_to(fs: &Fs, name: &str, dir: &str, base: &str) -> Result<String> {
    let from = join(FILES, name);
    if is_within(dir, &from) {
        return Err(Error::IntoItself);
    }
    fs.mkdir_all(dir)?;
    for _ in 0..ATTEMPTS {
        let to = join(dir, &fs.unique_name(dir, base));
        match fs.rename(&from, &to) {
            Ok(()) => {
                let _ = fs.remove(&info_path(name));
                return Ok(to);
            }
            Err(Error::Fs(FsError::Exists)) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Error::Fs(FsError::Exists))
}

/// Deletes the item called `name` for good.
pub fn delete(fs: &Fs, name: &str) -> Result<()> {
    // The item goes before its info: info left alone is ignored.
    match fs.remove_all(&join(FILES, name)) {
        Ok(()) | Err(Error::Fs(FsError::NotFound)) => {}
        Err(e) => return Err(e),
    }
    match fs.remove(&info_path(name)) {
        Ok(()) | Err(Error::Fs(FsError::NotFound)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Deletes everything in the Trash for good; returns how many items went.
/// Whatever cannot be deleted stays, and the first error is returned.
pub fn empty(fs: &Fs) -> Result<usize> {
    let mut first = None;
    let mut gone = 0;
    for e in fs.read_dir(FILES).unwrap_or_default() {
        match fs.remove_all(&join(FILES, &e.name)) {
            Ok(()) => gone += 1,
            Err(err) => {
                first.get_or_insert(err);
            }
        }
    }
    // Info goes unless its item stayed.
    let left: Vec<String> = fs.read_dir(FILES).unwrap_or_default().into_iter().map(|e| e.name).collect();
    for e in fs.read_dir(INFO).unwrap_or_default() {
        let item = e.name.strip_suffix(INFO_SUFFIX);
        if item.is_none_or(|n| !left.iter().any(|l| l == n)) {
            let _ = fs.remove(&join(INFO, &e.name));
        }
    }
    match first {
        Some(e) => Err(e),
        None => Ok(gone),
    }
}

// ---- info records ------------------------------------------------------------

/// The info record of an item deleted from `origin` at `deleted`
/// (nanoseconds since the epoch, local time).
pub fn info_text(origin: &str, deleted: u64) -> String {
    let d = DateTime::from_unix(deleted / NS_PER_SEC);
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={:04}-{:02}-{:02}T{:02}:{:02}:{:02}\n",
        escape(origin),
        d.year,
        d.month,
        d.day,
        d.hour,
        d.minute,
        d.second
    )
}

/// Reads an info record: `None` if it is not one, or names no path. A
/// missing or unreadable date is 0.
pub fn parse_info(text: &str) -> Option<Info> {
    let mut in_group = false;
    let (mut origin, mut deleted) = (None, 0);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Trash Info]";
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_group) else { continue };
        match key.trim() {
            "Path" => origin = Some(unescape(value.trim())),
            "DeletionDate" => deleted = parse_date(value.trim()).unwrap_or(0),
            _ => {}
        }
    }
    Some(Info { origin: origin.filter(|o| !o.is_empty())?, deleted })
}

/// A path as info records hold it: bytes other than letters, digits, `-`,
/// `.`, `_`, `~` and `/` escaped as `%XX`, as in URLs.
fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path an info record holds, `%XX` decoded (a `%` that starts no
/// escape stays as it is).
fn unescape(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A deletion date (`2026-10-08T14:03:12`, local time) in nanoseconds since
/// the epoch. Anything after the seconds (a fraction, a zone) is ignored.
fn parse_date(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| {
        b[from..to].iter().try_fold(0u64, |n, &c| c.is_ascii_digit().then(|| n * 10 + (c - b'0') as u64))
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400) + hour * 3_600 + minute * 60 + second) * NS_PER_SEC)
}

/// Days from 1970-01-01 to a date on or after it (the inverse of
/// `DateTime::from_unix`'s civil-from-days).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let (era, yoe) = (y / 400, y % 400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    const DAY: u64 = 86_400 * NS_PER_SEC;

    #[test]
    fn info_records_round_trip() {
        let t = (20_369 * 86_400 + 14 * 3_600 + 3 * 60 + 12) * NS_PER_SEC + 999;
        let text = info_text("/home/user/Documents/Notes for Monday.txt", t);
        assert_eq!(
            text,
            "[Trash Info]\nPath=/home/user/Documents/Notes%20for%20Monday.txt\nDeletionDate=2025-10-08T14:03:12\n"
        );
        let info = parse_info(&text).unwrap();
        assert_eq!(info.origin, "/home/user/Documents/Notes for Monday.txt");
        // Records keep whole seconds.
        assert_eq!(info.deleted, t - 999);
    }

    #[test]
    fn paths_are_escaped_as_in_urls() {
        assert_eq!(escape("/home/user/a-b_c.d~e"), "/home/user/a-b_c.d~e");
        assert_eq!(escape("/tmp/100% ü#?.txt"), "/tmp/100%25%20%C3%BC%23%3F.txt");
        assert_eq!(unescape("/tmp/100%25%20%C3%BC%23%3F.txt"), "/tmp/100% ü#?.txt");
        // Lower-case digits, and a % that starts no escape.
        assert_eq!(unescape("/a%c3%bc"), "/aü");
        assert_eq!(unescape("/50%zz/%4"), "/50%zz/%4");
        for p in ["/home/user/x y", "/home/user/日本語 (2).txt", "/tmp/%41"] {
            assert_eq!(unescape(&escape(p)), p);
        }
    }

    #[test]
    fn reading_info_records() {
        let text = "[Desktop Entry]\nPath=/elsewhere\n[Trash Info]\nDeletionDate = 1970-01-02T01:05:00\n \
                    Path = /home/user/a%20b \nOther=1\n";
        let info = parse_info(text).unwrap();
        assert_eq!(info.origin, "/home/user/a b");
        assert_eq!(info.deleted, DAY + 3_900 * NS_PER_SEC);
        // A date that cannot be read is unknown; a record without a path is
        // none.
        assert_eq!(parse_info("[Trash Info]\nPath=/a\nDeletionDate=yesterday").unwrap().deleted, 0);
        assert_eq!(parse_info("[Trash Info]\nDeletionDate=2026-10-08T14:03:12"), None);
        assert_eq!(parse_info("Path=/a"), None);
        assert_eq!(parse_info(""), None);
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("1970-01-01T00:00:00"), Some(0));
        assert_eq!(parse_date("1970-03-01T00:00:00"), Some(59 * DAY));
        assert_eq!(parse_date("2000-02-29T12:00:00.25+02:00"), Some(11_016 * DAY + DAY / 2));
        for days in [0, 58, 59, 365, 10_957, 11_016, 20_369, 24_837, 47_482] {
            let t = days * 86_400 + 45_296;
            let d = DateTime::from_unix(t);
            let text =
                format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute, d.second);
            assert_eq!(parse_date(&text), Some(t * NS_PER_SEC), "{text}");
        }
        for bad in [
            "",
            "2026-10-08",
            "2026-10-08 14:03:12",
            "2026-13-01T00:00:00",
            "1969-12-31T23:59:59",
            "+026-10-08T14:03:12",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn places_in_the_trash() {
        assert!(contains(FILES));
        assert!(contains(&join(FILES, "a.txt")));
        assert!(!contains(DIR) && !contains(INFO) && !contains("/home/user/x"));
        assert_eq!(item_name(&join(FILES, "a.txt")), Some("a.txt"));
        assert_eq!(item_name(&format!("{FILES}/Trips/Beacon.jpg")), Some("Trips"));
        assert_eq!(item_name(FILES), None);
        assert_eq!(item_name(&format!("{FILES}x/a")), None);
        assert_eq!(item_name("/home/user/a"), None);
    }

    #[test]
    fn items_know_their_original_names() {
        let info = Info { origin: "/home/user/notes.txt".to_string(), deleted: 5 };
        let item = Item { name: "notes (2).txt".to_string(), is_dir: false, size: 3, info: Some(info) };
        assert_eq!(item.original_name(), "notes.txt");
        assert_eq!(item.deleted(), 5);
        assert_eq!(item.path(), format!("{FILES}/notes (2).txt"));
        let stray = Item { name: "x".to_string(), is_dir: true, size: 0, info: None };
        assert_eq!(stray.original_name(), "x");
        assert_eq!(stray.deleted(), 0);
    }
}
