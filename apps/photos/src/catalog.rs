//! The pictures of one folder, and small helpers for paths and sizes.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use vproto::fs::FsError;
use vproto::vfs;

use crate::loader::{Meta, Thumbs};

/// The default folder.
pub const PICTURES: &str = "/home/user/Pictures";

/// Thumbnail state of an entry.
pub enum Thumb {
    /// Not requested yet.
    Missing,
    /// Queued or being made.
    Requested,
    Ready(Thumbs),
    /// The file could not be decoded (the message says why).
    Failed(String),
}

/// One picture file.
pub struct Entry {
    pub name: String,
    pub path: String,
    /// File size from the directory listing.
    pub size: u64,
    /// Format, dimensions, ... once the file has been read.
    pub meta: Option<Meta>,
    pub thumb: Thumb,
    /// When the thumbnails were last drawn (the least recently used are dropped first).
    pub used: u64,
}

/// File extensions Photos opens.
pub fn is_picture(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "jpe" | "jfif" | "bmp" | "dib" | "qoi")
        }
        _ => false,
    }
}

/// Lists the pictures in `dir`, in natural name order.
pub fn list(vfs: &vfs::Client, dir: &str) -> Result<Vec<Entry>, String> {
    let items = match vfs.read_dir(dir.into()) {
        Ok(Ok(items)) => items,
        Ok(Err(FsError::NotFound)) => return Err(format!("The folder {dir} does not exist.")),
        Ok(Err(e)) => return Err(format!("The folder {dir} could not be read ({e}).")),
        Err(_) => return Err("The file system is not responding.".into()),
    };
    let mut entries: Vec<Entry> = items
        .into_iter()
        .filter(|e| !e.is_dir && is_picture(&e.name))
        .map(|e| Entry {
            path: join(dir, &e.name),
            name: e.name,
            size: e.size,
            meta: None,
            thumb: Thumb::Missing,
            used: 0,
        })
        .collect();
    entries.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    Ok(entries)
}

/// Compares names case-insensitively, with runs of digits compared by value ("2" < "10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (da, db) = (trim_zeros(&a[..na]), trim_zeros(&b[..nb]));
                let ord = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
                if ord != Ordering::Equal {
                    return ord;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let n = d.iter().take_while(|&&c| c == b'0').count();
    &d[n.min(d.len().saturating_sub(1))..]
}

/// `dir/name`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// Splits a path into its folder and file name.
pub fn split(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some(("", name)) => ("/".into(), name.into()),
        Some((dir, name)) => (dir.into(), name.into()),
        None => (PICTURES.into(), path.into()),
    }
}

/// The last component of a folder path, for headings.
pub fn folder_name(dir: &str) -> &str {
    dir.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("/")
}

/// Human-readable size: `512 bytes`, `12.4 KB`, `3.1 MB` (powers of 1024).
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let (value, unit) = if bytes < 1024 * 1024 {
        (bytes * 10 / 1024, "KB")
    } else if bytes < 1024 * 1024 * 1024 {
        (bytes * 10 / (1024 * 1024), "MB")
    } else {
        (bytes * 10 / (1024 * 1024 * 1024), "GB")
    };
    if value >= 1000 { format!("{} {unit}", value / 10) } else { format!("{}.{} {unit}", value / 10, value % 10) }
}
