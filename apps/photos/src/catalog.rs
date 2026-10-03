//! The pictures of one folder, and small helpers for paths and sizes.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vfiles::kind::is_image;
use vfiles::path::{join, natural_cmp};
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
        .filter(|e| !e.is_dir && is_image(&e.name))
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
