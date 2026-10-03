//! `vaudio` — audio formats, mixing, resampling, analysis and synthesis.
//!
//! Everything here is `no_std` + `alloc`, so the same code runs inside
//! Vindows (the audio service, the Music player) and on the host (the
//! `musicgen` tool and the unit tests).
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`wav`] | RIFF/WAVE decoding (8/16/24/32-bit PCM, 32/64-bit float, up to 8 channels, `LIST/INFO` tags) and writing |
//! | [`qoa`] | the "Quite OK Audio" codec (encoder and decoder) used for the music that ships with Vindows |
//! | [`tags`] | track metadata and the `VTAG` trailer appended to QOA files |
//! | [`source`] | one streaming, seekable decoder interface over WAV and QOA files |
//! | [`resample`] | exact rational sample-rate conversion (polyphase windowed sinc, integer arithmetic) |
//! | [`mix`] | gains, volume curves, mixing and sample conversion |
//! | [`fft`] | a real FFT, windows and log-spaced spectrum bands for visualisers |
//! | [`synth`] | oscillators, envelopes, filters, drums, effects and a sequencer for offline rendering |
//!
//! Samples are interleaved. Integer PCM is `i16`; the synthesiser works in
//! `f32`. Hot paths that run inside Vindows (decoding, resampling, mixing)
//! use integer arithmetic because the system usually runs under CPU
//! emulation, where integer code is much cheaper than floating point.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod aec;
pub mod fft;
pub mod mix;
pub mod qoa;
pub mod resample;
pub mod source;
pub mod synth;
pub mod tags;
#[cfg(test)]
mod testsig;
pub mod wav;

pub use source::Source;
pub use tags::Tags;

use core::fmt;

/// Errors from parsing or decoding audio data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioError {
    /// The data is not in a recognised format.
    UnknownFormat,
    /// A header or chunk is malformed or truncated.
    Malformed,
    /// The encoding (sample format, channel count, rate) is not supported.
    Unsupported,
    /// An argument is out of range (e.g. a seek beyond the end).
    OutOfRange,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AudioError::UnknownFormat => "unknown audio format",
            AudioError::Malformed => "malformed audio data",
            AudioError::Unsupported => "unsupported audio encoding",
            AudioError::OutOfRange => "position out of range",
        })
    }
}

/// Reads a little-endian `u16` at `off` (callers check bounds).
#[inline]
pub(crate) fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// Reads a little-endian `u32` at `off` (callers check bounds).
#[inline]
pub(crate) fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
