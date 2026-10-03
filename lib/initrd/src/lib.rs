//! The `VINITRD` archive format.
//!
//! The initial ramdisk is a flat, read-only archive of files that the
//! bootloader loads into memory next to the kernel. It contains `init`, the
//! system services, drivers, applications and their assets.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! +-----------------------------+ 0
//! | Header (64 bytes)           |
//! +-----------------------------+ 64
//! | Entry[entry_count] (32 B)   |   sorted by path, for binary search
//! +-----------------------------+ strings_offset
//! | path strings (UTF-8)        |
//! +-----------------------------+ (4 KiB aligned)
//! | file data, each file 4 KiB  |   page alignment lets the kernel map file
//! | aligned                     |   contents directly, without copying
//! +-----------------------------+ total_size
//! ```
//!
//! Paths are relative (`bin/init.exe`), use `/` as separator, and never
//! contain `.` or `..` components. Directories are implicit.

#![no_std]

#[cfg(feature = "std")]
extern crate std;

/// Magic bytes at offset 0.
pub const MAGIC: [u8; 8] = *b"VINITRD\0";
/// Current format version.
pub const VERSION: u32 = 1;
/// Alignment of every file's data.
pub const DATA_ALIGN: u64 = 4096;

const HEADER_SIZE: usize = 64;
const ENTRY_SIZE: usize = 32;

/// Errors produced while opening an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitrdError {
    BadMagic,
    UnsupportedVersion(u32),
    Truncated,
    BadEntry,
}

impl core::fmt::Display for InitrdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InitrdError::BadMagic => f.write_str("not a VINITRD archive"),
            InitrdError::UnsupportedVersion(v) => write!(f, "unsupported VINITRD version {v}"),
            InitrdError::Truncated => f.write_str("archive is truncated"),
            InitrdError::BadEntry => f.write_str("archive entry is out of bounds"),
        }
    }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// A validated, read-only view of an archive in memory.
#[derive(Clone, Copy)]
pub struct Archive<'a> {
    data: &'a [u8],
    count: usize,
    entries_off: usize,
    strings_off: usize,
    strings_size: usize,
}

impl core::fmt::Debug for Archive<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Archive").field("files", &self.count).field("bytes", &self.data.len()).finish()
    }
}

/// One file in the archive.
#[derive(Debug, Clone, Copy)]
pub struct File<'a> {
    /// Relative path, e.g. `bin/init.exe`.
    pub path: &'a str,
    /// Offset of the data from the start of the archive (page aligned).
    pub offset: u64,
    /// The file contents.
    pub data: &'a [u8],
}

impl<'a> Archive<'a> {
    /// Validates the header and every entry of `data`.
    pub fn open(data: &'a [u8]) -> Result<Self, InitrdError> {
        if data.len() < HEADER_SIZE {
            return Err(InitrdError::Truncated);
        }
        if data[0..8] != MAGIC {
            return Err(InitrdError::BadMagic);
        }
        let version = u32_at(data, 8);
        if version != VERSION {
            return Err(InitrdError::UnsupportedVersion(version));
        }
        let count = u32_at(data, 12) as usize;
        let entries_off = u64_at(data, 16) as usize;
        let strings_off = u64_at(data, 24) as usize;
        let strings_size = u64_at(data, 32) as usize;
        let total = u64_at(data, 40) as usize;
        if total > data.len()
            || entries_off.checked_add(count * ENTRY_SIZE).is_none_or(|e| e > total)
            || strings_off.checked_add(strings_size).is_none_or(|e| e > total)
        {
            return Err(InitrdError::Truncated);
        }
        let archive = Archive { data: &data[..total], count, entries_off, strings_off, strings_size };
        for i in 0..count {
            archive.entry(i).ok_or(InitrdError::BadEntry)?;
        }
        Ok(archive)
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The whole archive.
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    /// Returns the `index`-th file (in path order).
    pub fn entry(&self, index: usize) -> Option<File<'a>> {
        if index >= self.count {
            return None;
        }
        let e = self.entries_off + index * ENTRY_SIZE;
        let name_off = u32_at(self.data, e) as usize;
        let name_len = u32_at(self.data, e + 4) as usize;
        let data_off = u64_at(self.data, e + 8);
        let size = u64_at(self.data, e + 16) as usize;
        if name_off + name_len > self.strings_size {
            return None;
        }
        let name_start = self.strings_off + name_off;
        let path = core::str::from_utf8(&self.data[name_start..name_start + name_len]).ok()?;
        let start = data_off as usize;
        let data = self.data.get(start..start.checked_add(size)?)?;
        Some(File { path, offset: data_off, data })
    }

    /// Iterates over all files in path order.
    pub fn files(&self) -> impl Iterator<Item = File<'a>> + '_ {
        (0..self.count).filter_map(move |i| self.entry(i))
    }

    /// Looks up a file by path (a leading `/` is ignored).
    pub fn find(&self, path: &str) -> Option<File<'a>> {
        let path = path.trim_start_matches('/');
        let (mut lo, mut hi) = (0usize, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let f = self.entry(mid)?;
            match f.path.cmp(path) {
                core::cmp::Ordering::Equal => return Some(f),
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }
}

/// Builds archives on the host.
#[cfg(feature = "std")]
pub mod builder {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    /// Accumulates files and serialises them into a `VINITRD` image.
    #[derive(Default)]
    pub struct Builder {
        files: Vec<(String, Vec<u8>)>,
    }

    impl Builder {
        pub fn new() -> Self {
            Self::default()
        }

        /// Adds a file. Panics on invalid or duplicate paths, which are
        /// build-script bugs.
        pub fn add(&mut self, path: &str, data: Vec<u8>) -> &mut Self {
            let path = path.trim_start_matches('/');
            assert!(!path.is_empty(), "empty initrd path");
            assert!(path.split('/').all(|c| !c.is_empty() && c != "." && c != ".."), "invalid initrd path: {path}");
            assert!(!self.files.iter().any(|(p, _)| p == path), "duplicate initrd path: {path}");
            self.files.push((path.into(), data));
            self
        }

        /// Serialises the archive.
        pub fn build(mut self) -> Vec<u8> {
            self.files.sort_by(|a, b| a.0.cmp(&b.0));
            let count = self.files.len();
            let entries_off = HEADER_SIZE;
            let strings_off = entries_off + count * ENTRY_SIZE;
            let strings_size: usize = self.files.iter().map(|(p, _)| p.len()).sum();
            let align = |x: u64| x.div_ceil(DATA_ALIGN) * DATA_ALIGN;
            let mut data_off = align((strings_off + strings_size) as u64);

            let mut entries = Vec::with_capacity(count * ENTRY_SIZE);
            let mut strings = Vec::with_capacity(strings_size);
            let mut offsets = Vec::with_capacity(count);
            for (path, data) in &self.files {
                entries.extend_from_slice(&(strings.len() as u32).to_le_bytes());
                entries.extend_from_slice(&(path.len() as u32).to_le_bytes());
                entries.extend_from_slice(&data_off.to_le_bytes());
                entries.extend_from_slice(&(data.len() as u64).to_le_bytes());
                entries.extend_from_slice(&[0u8; 8]);
                strings.extend_from_slice(path.as_bytes());
                offsets.push(data_off);
                data_off = align(data_off + data.len() as u64);
            }
            let total = data_off;

            let mut out = Vec::with_capacity(total as usize);
            out.extend_from_slice(&MAGIC);
            out.extend_from_slice(&VERSION.to_le_bytes());
            out.extend_from_slice(&(count as u32).to_le_bytes());
            out.extend_from_slice(&(entries_off as u64).to_le_bytes());
            out.extend_from_slice(&(strings_off as u64).to_le_bytes());
            out.extend_from_slice(&(strings_size as u64).to_le_bytes());
            out.extend_from_slice(&total.to_le_bytes());
            out.resize(HEADER_SIZE, 0);
            out.extend_from_slice(&entries);
            out.extend_from_slice(&strings);
            for ((_, data), off) in self.files.iter().zip(offsets) {
                out.resize(off as usize, 0);
                out.extend_from_slice(data);
            }
            out.resize(total as usize, 0);
            out
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::builder::Builder;
    use super::*;
    use std::vec;

    #[test]
    fn round_trip() {
        let mut b = Builder::new();
        b.add("bin/init.exe", vec![1, 2, 3]);
        b.add("/fonts/ui.ttf", vec![9; 5000]);
        b.add("a.txt", vec![]);
        let img = b.build();
        let ar = Archive::open(&img).unwrap();
        assert_eq!(ar.len(), 3);
        let init = ar.find("/bin/init.exe").unwrap();
        assert_eq!(init.data, &[1, 2, 3]);
        assert_eq!(init.offset % DATA_ALIGN, 0);
        assert_eq!(ar.find("fonts/ui.ttf").unwrap().data.len(), 5000);
        assert_eq!(ar.find("a.txt").unwrap().data.len(), 0);
        assert!(ar.find("missing").is_none());
        let paths: std::vec::Vec<_> = ar.files().map(|f| f.path).collect();
        assert_eq!(paths, ["a.txt", "bin/init.exe", "fonts/ui.ttf"]);
    }

    #[test]
    fn rejects_corruption() {
        assert_eq!(Archive::open(b"nope").unwrap_err(), InitrdError::Truncated);
        let mut img = Builder::new().build();
        img[0] = b'X';
        assert_eq!(Archive::open(&img).unwrap_err(), InitrdError::BadMagic);
    }
}
