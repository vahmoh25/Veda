//! The error type shared by every codec and image operation.

use core::fmt;

/// Errors returned by `vimage` decoders, encoders and image operations.
///
/// Decoders never panic on malformed input; every problem is reported as one of these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// The data does not start with the signature of a supported format.
    UnknownFormat,
    /// The data ended before the image or compressed stream was complete.
    Truncated,
    /// The data is malformed; the message describes the problem.
    Invalid(&'static str),
    /// The data is valid but uses a feature this crate does not implement.
    Unsupported(&'static str),
    /// The image dimensions exceed the limits in [`DecodeOptions`](crate::DecodeOptions).
    TooLarge {
        /// Width declared by the file.
        width: u32,
        /// Height declared by the file.
        height: u32,
    },
    /// A checksum (PNG chunk CRC-32 or zlib Adler-32) does not match the data; the message names it.
    ChecksumMismatch(&'static str),
    /// Decompression would produce more than the caller-supplied output limit.
    OutputLimit,
    /// A memory allocation failed.
    OutOfMemory,
    /// An argument passed by the caller is invalid (for example an empty image given to an encoder).
    InvalidArgument(&'static str),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::UnknownFormat => f.write_str("unknown image format"),
            ImageError::Truncated => f.write_str("image data is truncated"),
            ImageError::Invalid(msg) => write!(f, "invalid image data: {msg}"),
            ImageError::Unsupported(msg) => write!(f, "unsupported image feature: {msg}"),
            ImageError::TooLarge { width, height } => write!(f, "image size {width}x{height} exceeds the limits"),
            ImageError::ChecksumMismatch(what) => write!(f, "{what} checksum mismatch"),
            ImageError::OutputLimit => f.write_str("decompressed data exceeds the output limit"),
            ImageError::OutOfMemory => f.write_str("out of memory"),
            ImageError::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl From<alloc::collections::TryReserveError> for ImageError {
    fn from(_: alloc::collections::TryReserveError) -> Self {
        ImageError::OutOfMemory
    }
}
