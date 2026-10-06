//! `vhda` — Intel High Definition Audio for Veda's sound driver.
//!
//! HD Audio is the sound hardware of nearly every PC since 2005: a
//! controller in the chipset moves PCM between memory and a serial link,
//! and one or more codecs on the link convert it. A codec is a graph of
//! widgets — converters (DACs and ADCs), mixers, selectors and pin
//! complexes (jacks, speakers, microphones) — that the driver discovers
//! and programs with small commands called verbs.
//!
//! This crate holds what does not touch hardware, so that it runs inside
//! Veda and in host tests alike:
//!
//! * [`verb`]: verbs, parameters and payloads, and the stream format word;
//! * [`codec`]: reading a codec's widgets and their capabilities through
//!   the controller ([`codec::Bus`]);
//! * [`route`]: which outputs play and which input records, the paths to
//!   them, and the commands that set the codec up and follow its jacks;
//! * [`vendor`]: what particular codecs need beyond the specification.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod codec;
pub mod route;
pub mod vendor;
pub mod verb;

#[cfg(test)]
mod tests;
