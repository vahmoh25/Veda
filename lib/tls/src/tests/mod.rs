//! Tests of the whole client: handshakes against a rustls server in memory,
//! certificate chains captured from real servers, and (ignored by default)
//! connections to real servers over the host's network.

mod chains;
mod handshake;
mod interop;
pub(crate) mod support;
