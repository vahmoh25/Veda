//! Tests of the whole client: handshakes against a rustls server in memory,
//! certificate chains captured from real servers, and (ignored by default)
//! connections to real servers over the host's network.

mod handshake;
pub(crate) mod support;
