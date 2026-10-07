//! `/dev`: the devices every POSIX system has.
//!
//! | Name | Reading gives | Writing |
//! |------|---------------|---------|
//! | `null` | nothing (the end of the file) | discards the data |
//! | `zero` | zero bytes | discards the data |
//! | `full` | zero bytes | fails: no space left |
//! | `random`, `urandom` | the kernel's random bytes | discards the data |
//!
//! The terminal (`/dev/tty`) and a program's own descriptors (`/dev/stdin`,
//! `/dev/fd/N`) differ from process to process; the POSIX layer provides
//! them.

use alloc::vec::Vec;

use vproto::fs::{DirEntry, FsError, Stat, device};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dev {
    Null,
    Zero,
    Full,
    Random,
}

/// Every device with its name.
const ALL: [(&str, Dev); 5] =
    [("full", Dev::Full), ("null", Dev::Null), ("random", Dev::Random), ("urandom", Dev::Random), ("zero", Dev::Zero)];

/// Inode of `/dev` itself; the devices follow.
const DIR_INODE: u64 = 1;

impl Dev {
    /// The device a path inside `/dev` names.
    pub fn find(comps: &[&str]) -> Result<Option<Dev>, FsError> {
        match comps {
            [] => Ok(None),
            [name] => ALL.iter().find(|(n, _)| n == name).map(|&(_, d)| Some(d)).ok_or(FsError::NotFound),
            [name, ..] if ALL.iter().any(|(n, _)| n == name) => Err(FsError::NotDir),
            _ => Err(FsError::NotFound),
        }
    }

    fn inode(self) -> u64 {
        DIR_INODE + 1 + self as u64
    }

    pub fn stat(dev: Option<Dev>) -> Stat {
        Stat {
            size: if dev.is_none() { ALL.len() as u64 } else { 0 },
            is_dir: dev.is_none(),
            read_only: false,
            modified: 0,
            inode: dev.map_or(DIR_INODE, Dev::inode),
            device: device::DEV,
            executable: false,
            char_device: dev.is_some(),
        }
    }

    pub fn list() -> Vec<DirEntry> {
        ALL.iter()
            .map(|&(name, d)| DirEntry { name: name.into(), is_dir: false, size: 0, modified: 0, inode: d.inode() })
            .collect()
    }

    /// Reads up to `len` bytes.
    pub fn read(self, len: usize) -> Vec<u8> {
        match self {
            Dev::Null => Vec::new(),
            Dev::Zero | Dev::Full => alloc::vec![0; len],
            Dev::Random => {
                let mut v = alloc::vec![0; len];
                vrt::object::random_bytes(&mut v);
                v
            }
        }
    }

    /// Writes `len` bytes; returns how many were taken.
    pub fn write(self, len: usize) -> Result<u32, FsError> {
        match self {
            Dev::Full => Err(FsError::NoSpace),
            _ => Ok(len as u32),
        }
    }
}
