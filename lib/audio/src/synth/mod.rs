//! Synthesis for offline rendering (the `musicgen` tool).
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`osc`] | table sine, PolyBLEP saw/square/pulse, triangle, noise, chip waves |
//! | [`env`] | ADSR and decay envelopes |
//! | [`filter`] | state-variable filter, RBJ biquads, one-pole, DC blocker |
//! | [`fx`] | delay, reverb, chorus, compressor, limiter, saturation |
//! | [`drums`] | synthesised drum one-shots, sweeps and vinyl crackle |
//! | [`voice`] | instruments: subtractive, FM, plucked string, chip, drum kit |
//! | [`theory`] | note names, chords, scales |
//! | [`seq`] | songs, parts and the mixing/mastering renderer |
//!
//! All processing is `f32` at any sample rate; rendering is deterministic
//! (every random choice is seeded), so the same song always produces the
//! same samples.

pub mod drums;
pub mod env;
pub mod filter;
pub mod fx;
pub mod osc;
pub mod seq;
pub mod theory;
pub mod voice;

pub use env::Adsr;
pub use filter::{BiquadKind, Mode};
pub use osc::Wave;
pub use seq::{Master, Note, Part, Song, render};
pub use voice::{Analog, Bed, Chip, FilterSpec, Fm, Instrument, Kit, OscSpec, Pluck};
