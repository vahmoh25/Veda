//! The virtual file system protocol.
//!
//! Paths are absolute, `/`-separated UTF-8. The standard namespace:
//!
//! | Path | Contents |
//! |------|----------|
//! | `/system` | the read-only system image (programs, fonts, assets) |
//! | `/home/user` | the user's documents, pictures and music |
//! | `/tmp` | scratch space |
//! | `/dev` | devices: `null`, `zero`, `full`, `random`, `urandom` |

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{Bytes, enumeration, message, protocol};
use vrt::object::{Channel, Vmo};

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
        /// The path is in another program's private directory.
        Denied = 12,
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
            FsError::Denied => "permission denied",
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
        /// Identifies the file within its file system (never reused while
        /// the system runs): the inode number of POSIX programs.
        pub inode: u64,
        /// Identifies the file system the file is in (see [`device`]).
        pub device: u64,
        /// A program: an ELF or PE executable, or a `#!` script. (Veda keeps
        /// no permission bits; this is what makes a file executable.)
        pub executable: bool,
        /// A device such as `/dev/null` rather than a regular file.
        pub char_device: bool,
    }
}

/// The file systems of [`Stat::device`].
pub mod device {
    /// The writable file system: `/home`, `/tmp` and the root.
    pub const RAM: u64 = 1;
    /// `/system`, the read-only system image.
    pub const SYSTEM: u64 = 2;
    /// `/dev`, the devices.
    pub const DEV: u64 = 3;
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DirEntry {
        pub name: String,
        pub is_dir: bool,
        pub size: u64,
        pub modified: u64,
        /// As [`Stat::inode`].
        pub inode: u64,
    }
}

message! {
    /// How full the file system holding a path is (see `vfs::space`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Space {
        /// Bytes it can hold.
        pub total: u64,
        /// Bytes in use.
        pub used: u64,
        /// Its contents survive a restart (the system image, or a home
        /// directory kept on the home disk).
        pub persistent: bool,
    }
}

/// Flags for `open`.
pub mod open_flags {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const CREATE: u32 = 4;
    pub const TRUNCATE: u32 = 8;
    /// Writes go to the end of the file. A modifier: writing takes `WRITE`.
    pub const APPEND: u32 = 16;
    /// With `CREATE`: fail with `Exists` if the file is already there.
    pub const EXCLUSIVE: u32 = 32;
}

/// `whence` of [`file::Client::seek`].
pub mod seek {
    /// From the start of the file.
    pub const SET: u32 = 0;
    /// From the current offset.
    pub const CURRENT: u32 = 1;
    /// From the end of the file.
    pub const END: u32 = 2;
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
        /// How full the file system holding `path` is. Writes that would
        /// not fit fail with `NoSpace`.
        14 => fn space(path: String) -> Result<Space, FsError>;
        /// Opens a file (`open_flags`) as a connection of its own, which
        /// speaks [`file`] and can be passed to another process. Closing
        /// the channel closes the file. Directories cannot be opened so.
        15 => fn open_file(path: String, flags: u32) -> Result<(Channel, Stat), FsError>;
        /// Renames `from` to `to`, replacing `to` if it exists (a file, or
        /// an empty directory if `from` is a directory), in one step.
        16 => fn replace(from: String, to: String) -> Result<(), FsError>;
        /// Sets the modification time (nanoseconds since the Unix epoch; 0:
        /// now).
        17 => fn set_modified(path: String, modified: u64) -> Result<(), FsError>;
    }
}

protocol! {
    /// An open file: a connection made by [`vfs::Client::open_file`]. The
    /// offset and the flags belong to the open file and are shared by every
    /// connection [duplicated](file::Client::duplicate) from it, as POSIX shares
    /// an open file description between processes.
    pub mod file = "file" {
        /// Reads up to `len` (at most `MAX_IO`) bytes at the offset and
        /// moves it past them. Fewer (none at the end) when the file ends.
        1 => fn read(len: u32) -> Result<Bytes, FsError>;
        /// Writes at the offset (at the end of the file in append mode) and
        /// moves it past the data.
        2 => fn write(data: Bytes) -> Result<u32, FsError>;
        /// Reads at `offset`, leaving the offset alone.
        3 => fn read_at(offset: u64, len: u32) -> Result<Bytes, FsError>;
        /// Writes at `offset`, leaving the offset alone.
        4 => fn write_at(offset: u64, data: Bytes) -> Result<u32, FsError>;
        /// Moves the offset (`seek::SET`, `CURRENT` or `END`); returns it.
        5 => fn seek(offset: i64, whence: u32) -> Result<u64, FsError>;
        6 => fn stat() -> Result<Stat, FsError>;
        7 => fn truncate(len: u64) -> Result<(), FsError>;
        /// Another connection to this open file.
        8 => fn duplicate() -> Result<Channel, FsError>;
        /// The `open_flags` it was opened with.
        9 => fn flags() -> u32;
        /// Turns append mode on or off (the one flag that can change).
        10 => fn set_append(append: bool) -> Result<(), FsError>;
        /// Sets the modification time (nanoseconds since the Unix epoch;
        /// 0: now).
        11 => fn set_modified(modified: u64) -> Result<(), FsError>;
    }
}
