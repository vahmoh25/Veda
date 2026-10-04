//! `vwlan` — IEEE 802.11 for Veda.
//!
//! The protocol side of Wi-Fi, independent of any radio:
//!
//! * [`frame`], [`ie`]: MAC frames and information elements;
//! * [`rsn`]: RSN elements, cipher/AKM suites and the choice of security;
//! * [`crypto`], [`ccmp`]: key derivation, EAPOL-Key MICs, AES key wrap,
//!   CCMP-128 and BIP-CMAC-128;
//! * [`eapol`], [`handshake`]: EAPOL-Key frames and the 4-way and group key
//!   handshakes (station and access point sides);
//! * [`sae`]: Simultaneous Authentication of Equals (WPA3-Personal);
//! * [`scan`], [`station`], [`ap`]: scan results and the station and access
//!   point state machines;
//! * [`profile`], [`policy`]: saved networks, and the choice of network,
//!   retry delays and roaming.
//!
//! Everything here parses untrusted frames: parsers return errors instead
//! of panicking, and the cryptographic checks of the standard are applied
//! before any received key material is used. The code is `no_std` and runs
//! both inside Veda and in host tests, which use the published
//! IEEE 802.11 test vectors.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod ap;
pub mod ccmp;
pub mod crypto;
pub mod eapol;
pub mod frame;
pub mod handshake;
pub mod ie;
pub mod policy;
pub mod profile;
pub mod rsn;
pub mod sae;
pub mod scan;
pub mod station;
#[cfg(test)]
mod tests;
