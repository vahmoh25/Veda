//! The block device protocol: sector-level access to a disk.
//!
//! A disk driver registers one service per disk, named after the disk's
//! serial number (see [`service_name`]), so that clients can find a
//! particular disk (such as the user's home disk) without probing.

use alloc::format;
use alloc::string::String;
use vipc::{Bytes, enumeration, message, protocol};

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BlockError {
        /// The device reported an error.
        Io = 1,
        /// The request goes past the end of the disk.
        OutOfRange = 2,
        ReadOnly = 3,
        /// Bad arguments (unaligned data, too large a transfer).
        Invalid = 4,
    }
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            BlockError::Io => "I/O error",
            BlockError::OutOfRange => "beyond the end of the disk",
            BlockError::ReadOnly => "the disk is read-only",
            BlockError::Invalid => "invalid request",
        })
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BlockInfo {
        /// Capacity in sectors.
        pub sectors: u64,
        /// Bytes per sector (512 for virtio).
        pub sector_size: u32,
        pub read_only: bool,
        /// The disk's serial number ("" if it has none).
        pub serial: String,
    }
}

/// Largest transfer of one `read` or `write`, in bytes.
pub const MAX_TRANSFER: u32 = 32 * 1024;

/// The service name of the disk with serial number `serial`.
pub fn service_name(serial: &str) -> String {
    format!("block/{serial}")
}

protocol! {
    /// A disk.
    pub mod block = "block" {
        1 => fn info() -> BlockInfo;
        /// Reads `count` sectors starting at sector `lba` (at most
        /// `MAX_TRANSFER` bytes).
        2 => fn read(lba: u64, count: u32) -> Result<Bytes, BlockError>;
        /// Writes whole sectors starting at sector `lba`.
        3 => fn write(lba: u64, data: Bytes) -> Result<(), BlockError>;
        /// Makes previous writes durable.
        4 => fn flush() -> Result<(), BlockError>;
    }
}
