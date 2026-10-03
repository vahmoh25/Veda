//! Ephemeral key exchange: X25519 (RFC 7748), secp256r1 and secp384r1
//! (ECDH with uncompressed points, RFC 8446 section 4.2.8.2).
//!
//! Secrets come from the [`RandomSource`](super::RandomSource) the group
//! belongs to and are zeroed when the exchange ends. Peer keys are checked:
//! X25519 results that do not depend on our secret (low-order points) and
//! NIST points that are not on the curve are rejected.

use alloc::boxed::Box;
use core::fmt;

use p256::elliptic_curve::sec1::ToSec1Point;
use rustls::crypto::{ActiveKeyExchange, GetRandomFailed, SharedSecret, SupportedKxGroup};
use rustls::ffdhe_groups::FfdheGroup;
use rustls::{Error, NamedGroup, PeerMisbehaved};
use zeroize::Zeroizing;

/// The key exchange groups, in order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    X25519,
    Secp256r1,
    Secp384r1,
}

impl Group {
    pub(crate) const ALL: [Group; 3] = [Group::X25519, Group::Secp256r1, Group::Secp384r1];

    fn name(self) -> NamedGroup {
        match self {
            Group::X25519 => NamedGroup::X25519,
            Group::Secp256r1 => NamedGroup::secp256r1,
            Group::Secp384r1 => NamedGroup::secp384r1,
        }
    }
}

/// A key exchange group whose secrets come from `fill`.
pub(crate) struct KxGroup {
    group: Group,
    fill: fn(&mut [u8]),
}

impl KxGroup {
    pub(crate) const fn new(group: Group, fill: fn(&mut [u8])) -> KxGroup {
        KxGroup { group, fill }
    }
}

impl fmt::Debug for KxGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.group)
    }
}

/// How many random scalars to try before giving up on a NIST curve secret
/// (each attempt fails with probability below 2^-32).
const SCALAR_ATTEMPTS: usize = 8;

impl SupportedKxGroup for KxGroup {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        Ok(match self.group {
            Group::X25519 => {
                let mut seed = Zeroizing::new([0u8; 32]);
                (self.fill)(seed.as_mut());
                let secret = x25519_dalek::StaticSecret::from(*seed);
                let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
                Box::new(X25519Exchange { secret, public })
            }
            Group::Secp256r1 => {
                let secret = random_scalar(self.fill, |b| p256::SecretKey::from_slice(b).ok(), 32)?;
                let public = secret.public_key().to_sec1_point(false).as_bytes().into();
                Box::new(Secp256r1Exchange { secret, public })
            }
            Group::Secp384r1 => {
                let secret = random_scalar(self.fill, |b| p384::SecretKey::from_slice(b).ok(), 48)?;
                let public = secret.public_key().to_sec1_point(false).as_bytes().into();
                Box::new(Secp384r1Exchange { secret, public })
            }
        })
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        // Not a finite-field group (and skipping rustls' default lookup keeps
        // the FFDHE parameters out of the binary).
        None
    }

    fn name(&self) -> NamedGroup {
        self.group.name()
    }
}

/// A uniformly random secret scalar: random bytes, retried in the rare case
/// they are zero or not below the group order.
fn random_scalar<K>(fill: fn(&mut [u8]), parse: impl Fn(&[u8]) -> Option<K>, len: usize) -> Result<K, Error> {
    let mut bytes = Zeroizing::new([0u8; 48]);
    let bytes = &mut bytes[..len];
    for _ in 0..SCALAR_ATTEMPTS {
        fill(bytes);
        if let Some(key) = parse(bytes) {
            return Ok(key);
        }
    }
    Err(GetRandomFailed.into())
}

fn invalid_key_share() -> Error {
    PeerMisbehaved::InvalidKeyShare.into()
}

struct X25519Exchange {
    secret: x25519_dalek::StaticSecret,
    public: [u8; 32],
}

impl ActiveKeyExchange for X25519Exchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        let peer: [u8; 32] = peer_pub_key.try_into().map_err(|_| invalid_key_share())?;
        let shared = self.secret.diffie_hellman(&x25519_dalek::PublicKey::from(peer));
        // An all-zero result means the peer sent a low-order point.
        if !shared.was_contributory() {
            return Err(invalid_key_share());
        }
        Ok(SharedSecret::from(&shared.as_bytes()[..]))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

/// Whether `point` has the shape of an uncompressed point (`04 || X || Y`)
/// for a curve with `field_len`-byte coordinates; TLS allows no other form.
fn is_uncompressed(point: &[u8], field_len: usize) -> bool {
    point.len() == 1 + 2 * field_len && point[0] == 0x04
}

struct Secp256r1Exchange {
    secret: p256::SecretKey,
    public: Box<[u8]>,
}

impl ActiveKeyExchange for Secp256r1Exchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        if !is_uncompressed(peer_pub_key, 32) {
            return Err(invalid_key_share());
        }
        // Decoding checks that the point is on the curve (and not infinity).
        let peer = p256::PublicKey::from_sec1_bytes(peer_pub_key).map_err(|_| invalid_key_share())?;
        let shared = self.secret.diffie_hellman(&peer);
        Ok(SharedSecret::from(&shared.raw_secret_bytes()[..]))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }
}

struct Secp384r1Exchange {
    secret: p384::SecretKey,
    public: Box<[u8]>,
}

impl ActiveKeyExchange for Secp384r1Exchange {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        if !is_uncompressed(peer_pub_key, 48) {
            return Err(invalid_key_share());
        }
        let peer = p384::PublicKey::from_sec1_bytes(peer_pub_key).map_err(|_| invalid_key_share())?;
        let shared = self.secret.diffie_hellman(&peer);
        Ok(SharedSecret::from(&shared.raw_secret_bytes()[..]))
    }

    fn pub_key(&self) -> &[u8] {
        &self.public
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn group(&self) -> NamedGroup {
        NamedGroup::secp384r1
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::tests::support::{hex, test_fill};

    /// "Randomness" that is the RFC 8448 client's X25519 private key.
    fn rfc8448_client_key(buf: &mut [u8]) {
        buf.copy_from_slice(&hex("49af42ba7f7994852d713ef2784bcbcaa7911de26adc5642cb634540e7ea5005"));
    }

    #[test]
    fn x25519_rfc8448() {
        let kx = KxGroup::new(Group::X25519, rfc8448_client_key).start().unwrap();
        assert_eq!(kx.group(), NamedGroup::X25519);
        assert_eq!(kx.pub_key(), hex("99381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c"));
        let server = hex("c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e751f0f");
        let shared = kx.complete(&server).unwrap();
        assert_eq!(shared.secret_bytes(), hex("8bd4054fb55b9d63fdfbacf9f04b9f0d35e6d63f537563efd46272900f89492d"));
    }

    #[test]
    fn x25519_rejects_bad_shares() {
        let group = KxGroup::new(Group::X25519, test_fill);
        // Wrong lengths, and low-order points (all-zero shared secret).
        for bad in [Vec::new(), alloc::vec![9u8; 31], alloc::vec![9u8; 33], alloc::vec![0u8; 32], {
            let mut one = alloc::vec![0u8; 32];
            one[0] = 1;
            one
        }] {
            assert!(group.start().unwrap().complete(&bad).is_err(), "{bad:02x?}");
        }
    }

    /// Both sides of an exchange agree, for every group.
    #[test]
    fn exchanges_agree() {
        for group in Group::ALL {
            let group = KxGroup::new(group, test_fill);
            let (a, b) = (group.start().unwrap(), group.start().unwrap());
            assert_ne!(a.pub_key(), b.pub_key());
            let a_pub = a.pub_key().to_vec();
            let ab = a.complete(b.pub_key()).unwrap();
            let ba = b.complete(&a_pub).unwrap();
            assert_eq!(ab.secret_bytes(), ba.secret_bytes());
            assert!(!ab.secret_bytes().iter().all(|&x| x == 0));
        }
    }

    #[test]
    fn nist_curves_reject_bad_points() {
        for (group, field) in [(Group::Secp256r1, 32), (Group::Secp384r1, 48)] {
            let group = KxGroup::new(group, test_fill);
            let other = group.start().unwrap().pub_key().to_vec();
            assert_eq!(other.len(), 1 + 2 * field);
            // Compressed form (not allowed in TLS), off-curve, truncated,
            // the point at infinity, garbage.
            let mut compressed = alloc::vec![0x02 | (other[2 * field] & 1)];
            compressed.extend_from_slice(&other[1..1 + field]);
            let mut off_curve = other.clone();
            off_curve[2 * field] ^= 1;
            for bad in [compressed, off_curve, other[..other.len() - 1].to_vec(), alloc::vec![0], alloc::vec![]] {
                assert!(group.start().unwrap().complete(&bad).is_err());
            }
            assert!(group.start().unwrap().complete(&other).is_ok());
        }
    }

    #[test]
    fn scalar_out_of_range_is_retried_or_reported() {
        // Bytes that are never a valid scalar (all 0xff is above the order).
        fn all_ones(buf: &mut [u8]) {
            buf.fill(0xff);
        }
        assert!(KxGroup::new(Group::Secp256r1, all_ones).start().is_err());
        assert!(KxGroup::new(Group::Secp384r1, all_ones).start().is_err());
    }
}
