//! SHA-2 hashes and HMAC for rustls.
//!
//! The TLS 1.3 key schedule (HKDF) and the TLS 1.2 PRF are rustls' generic
//! implementations over the HMAC defined here (`HkdfUsingHmac`,
//! `PrfUsingHmac`).

use alloc::boxed::Box;
use core::marker::PhantomData;

use hmac::{EagerHash, KeyInit, Mac};
use rustls::crypto::hash::{self, HashAlgorithm};
use rustls::crypto::hmac as tls_hmac;
use sha2::{Digest, Sha256, Sha384};

/// A hash function of the `sha2` crate.
pub(crate) struct Sha2<D> {
    algorithm: HashAlgorithm,
    _digest: PhantomData<fn() -> D>,
}

impl<D> Sha2<D> {
    const fn new(algorithm: HashAlgorithm) -> Self {
        Sha2 { algorithm, _digest: PhantomData }
    }
}

pub(crate) static SHA256: Sha2<Sha256> = Sha2::new(HashAlgorithm::SHA256);
pub(crate) static SHA384: Sha2<Sha384> = Sha2::new(HashAlgorithm::SHA384);

impl<D: Digest + Clone + Send + Sync + 'static> hash::Hash for Sha2<D> {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(Context(D::new()))
    }

    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(&D::digest(data))
    }

    fn output_len(&self) -> usize {
        <D as Digest>::output_size()
    }

    fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }
}

/// An incremental hash computation.
struct Context<D>(D);

impl<D: Digest + Clone + Send + Sync + 'static> hash::Context for Context<D> {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(&self.0.clone().finalize())
    }

    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(Context(self.0.clone()))
    }

    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(&self.0.finalize())
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

/// HMAC with a hash function of the `sha2` crate.
pub(crate) struct Hmac<D>(PhantomData<fn() -> D>);

pub(crate) static HMAC_SHA256: Hmac<Sha256> = Hmac(PhantomData);
pub(crate) static HMAC_SHA384: Hmac<Sha384> = Hmac(PhantomData);

impl<D: EagerHash + Send + Sync + 'static> tls_hmac::Hmac for Hmac<D>
where
    hmac::Hmac<D>: Send + Sync,
{
    fn with_key(&self, key: &[u8]) -> Box<dyn tls_hmac::Key> {
        // HMAC takes keys of any length (longer ones are hashed first).
        let mac = <hmac::Hmac<D> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
        Box::new(HmacKey(mac))
    }

    fn hash_output_len(&self) -> usize {
        <D as Digest>::output_size()
    }
}

/// An HMAC instance that has absorbed its key; each tag starts from a copy.
struct HmacKey<D: EagerHash>(hmac::Hmac<D>);

impl<D: EagerHash + Send + Sync + 'static> tls_hmac::Key for HmacKey<D>
where
    hmac::Hmac<D>: Send + Sync,
{
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> tls_hmac::Tag {
        let mut mac = self.0.clone();
        mac.update(first);
        for part in middle {
            mac.update(part);
        }
        mac.update(last);
        tls_hmac::Tag::new(&mac.finalize().into_bytes())
    }

    fn tag_len(&self) -> usize {
        <D as Digest>::output_size()
    }
}

#[cfg(test)]
mod tests {
    use rustls::crypto::hash::Hash;
    use rustls::crypto::hmac::Hmac;
    use rustls::crypto::tls12::{Prf, PrfUsingHmac};

    use super::*;
    use crate::tests::support::hex;

    #[test]
    fn sha2_known_answers() {
        // FIPS 180-2 "abc" examples.
        assert_eq!(
            SHA256.hash(b"abc").as_ref(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            SHA384.hash(b"abc").as_ref(),
            hex("cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7")
        );
        // Incremental hashing, forks included, agrees with one-shot hashing.
        let mut ctx = SHA256.start();
        ctx.update(b"a");
        let fork = ctx.fork();
        ctx.update(b"bc");
        assert_eq!(ctx.fork_finish().as_ref(), SHA256.hash(b"abc").as_ref());
        assert_eq!(fork.finish().as_ref(), SHA256.hash(b"a").as_ref());
        assert_eq!(ctx.finish().as_ref(), SHA256.hash(b"abc").as_ref());
        assert_eq!((SHA256.output_len(), SHA384.output_len()), (32, 48));
    }

    #[test]
    fn hmac_known_answers() {
        // RFC 4231 test case 2.
        let key = HMAC_SHA256.with_key(b"Jefe");
        assert_eq!(
            key.sign(&[b"what do ya want ", b"for nothing?"]).as_ref(),
            hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        let key = HMAC_SHA384.with_key(b"Jefe");
        assert_eq!(
            key.sign_concat(b"what do ya", &[b" want ", b"for "], b"nothing?").as_ref(),
            hex("af45d2e376484031617f78d2b58a6b1b9c7ef464f5a01b47e42ec3736322445e8e2240ca5e69e2c78b3239ecfab21649")
        );
        // RFC 4231 test case 6: a key longer than the block size.
        let key = HMAC_SHA256.with_key(&[0xaa; 131]);
        assert_eq!(
            key.sign(&[b"Test Using Larger Than Block-Size Key - Hash Key First"]).as_ref(),
            hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn tls12_prf_known_answer() {
        // The TLS 1.2 PRF (P_SHA256) test vector published on the TLS
        // working group list (also used by rustls' own tests).
        let secret = hex("9bbe436ba940f017b17652849a71db35");
        let seed = hex("a0ba9f936cda311827a6f796ffd5198c");
        let mut out = [0u8; 100];
        PrfUsingHmac(&HMAC_SHA256).for_secret(&mut out, &secret, b"test label", &seed);
        assert_eq!(
            out.as_slice(),
            hex(concat!(
                "e3f229ba727be17b8d122620557cd453c2aab21d07c3d495329b52d4e61edb5a6b301791e90d35c9c9a46b4e14baf9af",
                "0fa022f7077def17abfd3797c0564bab4fbc91666e9def9b97fce34f796789baa48082d122ee42c5a72e5a5110fff70187",
                "347b66"
            ))
        );
    }
}
