//! The virtual file system protocol.
//!
//! Paths are absolute, `/`-separated UTF-8. The standard namespace:
//!
//! | Path | Contents |
//! |------|----------|
//! | `/system` | the read-only system image (programs, fonts, assets) |
//! | `/home/user` | the user's documents, pictures and music |
//! | `/tmp` | scratch space |

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{Bytes, enumeration, message, protocol};
use vrt::object::Vmo;

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FsError {
        NotFound = 1,
        NotDir = 2,
        IsDir = 3,
        Exists = 4,
        ReadOnly = 5,
        Invalid = 6,
        NoSpace = 7,
        Io = 8,
        BadFd = 9,
        TooMany = 10,
        NotEmpty = 11,
    }
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            FsError::NotFound => "no such file or directory",
            FsError::NotDir => "not a directory",
            FsError::IsDir => "is a directory",
            FsError::Exists => "already exists",
            FsError::ReadOnly => "read-only file system",
            FsError::Invalid => "invalid path or argument",
            FsError::NoSpace => "no space left",
            FsError::Io => "I/O error",
            FsError::BadFd => "bad file descriptor",
            FsError::TooMany => "too many open files",
            FsError::NotEmpty => "directory not empty",
        })
    }
}

message! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Stat {
        pub size: u64,
        pub is_dir: bool,
        pub read_only: bool,
        /// Modification time, nanoseconds since the Unix epoch (0 = unknown).
        pub modified: u64,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DirEntry {
        pub name: String,
        pub is_dir: bool,
        pub size: u64,
        pub modified: u64,
    }
}

/// Flags for `open`.
pub mod open_flags {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const CREATE: u32 = 4;
    pub const TRUNCATE: u32 = 8;
    pub const APPEND: u32 = 16;
}

/// Largest payload of a single `read`/`write` call.
pub const MAX_IO: u32 = 48 * 1024;

protocol! {
    /// File system service.
    pub mod vfs = "vfs" {
        /// Opens a file; returns a descriptor local to this connection.
        1 => fn open(path: String, flags: u32) -> Result<u32, FsError>;
        2 => fn close(fd: u32) -> Result<(), FsError>;
        /// Reads up to `len` (at most `MAX_IO`) bytes at `offset`.
        3 => fn read(fd: u32, offset: u64, len: u32) -> Result<Bytes, FsError>;
        /// Writes at `offset`; returns the number of bytes written.
        4 => fn write(fd: u32, offset: u64, data: Bytes) -> Result<u32, FsError>;
        5 => fn stat(path: String) -> Result<Stat, FsError>;
        6 => fn read_dir(path: String) -> Result<Vec<DirEntry>, FsError>;
        7 => fn mkdir(path: String) -> Result<(), FsError>;
        8 => fn remove(path: String) -> Result<(), FsError>;
        9 => fn rename(from: String, to: String) -> Result<(), FsError>;
        /// Returns the whole file in a VMO plus its exact size (fast path
        /// for loading programs, images and music).
        10 => fn read_file(path: String) -> Result<(Vmo, u64), FsError>;
        /// Replaces (or creates) a file with the first `len` bytes of `data`.
        11 => fn write_file(path: String, data: Vmo, len: u64) -> Result<(), FsError>;
        12 => fn truncate(fd: u32, len: u64) -> Result<(), FsError>;
        /// Writes all changes to persistent storage now (before power-off).
        13 => fn sync() -> Result<(), FsError>;
    }
}
