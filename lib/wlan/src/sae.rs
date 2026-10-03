//! SAE — Simultaneous Authentication of Equals (IEEE 802.11-2020, 12.4),
//! the password-authenticated key exchange of WPA3-Personal, over the
//! NIST P-256 group (group 19).
//!
//! Both peers derive a password element (PWE) from the password and their
//! two MAC addresses, exchange Commit messages (a scalar and an element),
//! compute a shared secret, and prove knowledge of it with Confirm
//! messages. The result is the PMK for the 4-way handshake.
//!
//! The PWE comes from either method of the standard:
//!
//! * hunting and pecking (12.4.4.2.2): always [`HNP_ITERATIONS`] rounds,
//!   with the candidate test done in constant time, so the timing does not
//!   depend on the password;
//! * hash-to-element (12.4.4.2.3): the password-dependent point PT is
//!   computed once per network with the simplified SWU map, and the PWE
//!   is PT multiplied by a value derived from the addresses.
//!
//! Received scalars and elements are validated (range, curve membership,
//! identity), and a peer echoing our own commit is refused. The tests use
//! the vectors of IEEE 802.11-2020 Annex J.10.

use alloc::vec::Vec;

use p256::elliptic_curve::bigint::{ArrayEncoding, U256};
use p256::elliptic_curve::hazmat::FieldArithmetic;
use p256::elliptic_curve::ops::Reduce;
use p256::elliptic_curve::point::{AffineCoordinates, DecompressPoint};
use p256::elliptic_curve::{Curve, Field, Group, PrimeField};
use p256::hash2curve::MapToCurve;
use p256::{AffinePoint, FieldBytes, NistP256, ProjectivePoint, Scalar};
use subtle::{Choice, ConditionallySelectable};
use zeroize::Zeroize;

use crate::crypto::{Random, ct_eq, hmac_sha256, kdf_sha256};
use crate::frame::Mac;

type FieldElement = <NistP256 as FieldArithmetic>::FieldElement;

/// The only group implemented: NIST P-256.
pub const GROUP_P256: u16 = 19;
/// Rounds of hunting and pecking (the minimum the standard recommends).
pub const HNP_ITERATIONS: u8 = 40;
/// Bytes of a group 19 commit without token or extra elements.
pub const COMMIT_LEN: usize = 2 + 32 + 64;

/// The prime of P-256, big-endian.
const P256_PRIME: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaeError {
    Malformed,
    UnsupportedGroup,
    /// The peer's scalar is out of range.
    BadScalar,
    /// The peer's element is not a valid point.
    BadElement,
    /// The peer sent back our own commit.
    Reflected,
    /// The confirm did not verify (with a valid exchange: the passwords
    /// differ).
    BadConfirm,
    /// No password element could be derived (should not happen).
    NoPwe,
    /// A step was called out of order.
    State,
}

/// `max(a, b) || min(a, b)` of two MAC addresses.
fn addr_concat(a: &Mac, b: &Mac) -> [u8; 12] {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    let mut out = [0u8; 12];
    out[..6].copy_from_slice(hi);
    out[6..].copy_from_slice(lo);
    out
}

fn scalar_bytes(s: &Scalar) -> [u8; 32] {
    s.to_repr().into()
}

fn point_bytes(p: &AffinePoint) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&p.x());
    out[32..].copy_from_slice(&p.y());
    out
}

fn random_scalar(rng: &mut dyn Random) -> Scalar {
    loop {
        let mut b = [0u8; 32];
        rng.fill(&mut b);
        let s = Scalar::from_repr(b.into());
        b.zeroize();
        if let Some(s) = Option::<Scalar>::from(s)
            && !bool::from(s.is_zero())
            && s != Scalar::ONE
        {
            return s;
        }
    }
}

/// Derives the PWE by hunting and pecking.
pub fn pwe_hunting_and_pecking(
    addr1: &Mac,
    addr2: &Mac,
    password: &[u8],
    identifier: Option<&[u8]>,
) -> Result<ProjectivePoint, SaeError> {
    let key = addr_concat(addr1, addr2);
    let mut base = password.to_vec();
    if let Some(id) = identifier {
        base.extend_from_slice(id);
    }
    let mut found = Choice::from(0);
    let mut x_saved = FieldBytes::default();
    let mut seed_odd = Choice::from(0);
    for counter in 1..=HNP_ITERATIONS {
        let seed = hmac_sha256(&key, &[&base, &[counter]]);
        let mut value = [0u8; 32];
        kdf_sha256(&seed, b"SAE Hunting and Pecking", &P256_PRIME, &mut value);
        // A candidate is a value below p for which a point exists with
        // that x coordinate; every round does the same work.
        let below_p = FieldElement::from_repr(value.into()).is_some();
        let odd = Choice::from(seed[31] & 1);
        let point = AffinePoint::decompress(&value.into(), odd);
        let take = !found & below_p & point.is_some();
        x_saved = FieldBytes::conditional_select(&x_saved, &value.into(), take);
        seed_odd = Choice::conditional_select(&seed_odd, &odd, take);
        found |= take;
        value.zeroize();
    }
    base.zeroize();
    if !bool::from(found) {
        return Err(SaeError::NoPwe);
    }
    Option::<AffinePoint>::from(AffinePoint::decompress(&x_saved, seed_odd))
        .map(ProjectivePoint::from)
        .ok_or(SaeError::NoPwe)
}

/// The password-dependent point of hash-to-element, computed once per
/// network (SSID, password and optional password identifier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pt(ProjectivePoint);

/// Derives PT (12.4.4.2.3).
pub fn derive_pt(ssid: &[u8], password: &[u8], identifier: Option<&[u8]>) -> Pt {
    let mut ikm = password.to_vec();
    if let Some(id) = identifier {
        ikm.extend_from_slice(id);
    }
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(ssid), &ikm);
    ikm.zeroize();
    let mut point = ProjectivePoint::IDENTITY;
    for info in [&b"SAE Hash to Element u1 P1"[..], b"SAE Hash to Element u2 P2"] {
        let mut value = p256::elliptic_curve::array::Array::<u8, p256::elliptic_curve::consts::U48>::default();
        hk.expand(info, &mut value).expect("48 bytes is a valid HKDF output length");
        let u = FieldElement::reduce(&value);
        point += <NistP256 as MapToCurve>::map_to_curve(u);
        value.zeroize();
    }
    Pt(point)
}

/// Derives the PWE from PT for a pair of addresses.
pub fn pwe_from_pt(pt: &Pt, addr1: &Mac, addr2: &Mac) -> Result<ProjectivePoint, SaeError> {
    let val = hmac_sha256(&[0u8; 32], &[&addr_concat(addr1, addr2)]);
    // val = (val mod (r - 1)) + 1, a scalar in [1, r - 1]. (The value
    // comes from the public addresses only, so plain comparisons are
    // fine; one subtraction suffices because 2^256 < 2 (r - 1).)
    let order_minus_1 = NistP256::ORDER.wrapping_sub(&U256::ONE);
    let mut v = U256::from_be_slice(&val);
    if v >= order_minus_1 {
        v = v.wrapping_sub(&order_minus_1);
    }
    v = v.wrapping_add(&U256::ONE);
    let s = Option::<Scalar>::from(Scalar::from_repr(v.to_be_byte_array())).ok_or(SaeError::NoPwe)?;
    let pwe = pt.0 * s;
    if bool::from(pwe.is_identity()) { Err(SaeError::NoPwe) } else { Ok(pwe) }
}

/// One side of an SAE exchange (the same for station and access point).
pub struct Sae {
    pwe: ProjectivePoint,
    rand: Scalar,
    own_scalar: Scalar,
    own_element: AffinePoint,
    peer_scalar: Option<Scalar>,
    peer_element: Option<AffinePoint>,
    kck: [u8; 32],
    pmk: [u8; 32],
    pmkid: [u8; 16],
    keys: bool,
    send_confirm: u16,
    /// The commit uses hash-to-element (status code 126 on the air).
    pub h2e: bool,
}

impl Drop for Sae {
    fn drop(&mut self) {
        self.kck.zeroize();
        self.pmk.zeroize();
        self.rand = Scalar::ZERO;
    }
}

impl Sae {
    /// Starts an exchange with a PWE: picks the random values.
    pub fn new(pwe: ProjectivePoint, h2e: bool, rng: &mut dyn Random) -> Sae {
        loop {
            let rand = random_scalar(rng);
            let mask = random_scalar(rng);
            if let Some(sae) = Self::with_values(pwe, h2e, rand, mask) {
                return sae;
            }
        }
    }

    /// Starts an exchange with given random values (`None` if they give a
    /// degenerate commit; also used by the test vectors).
    pub fn with_values(pwe: ProjectivePoint, h2e: bool, rand: Scalar, mask: Scalar) -> Option<Sae> {
        let own_scalar = rand + mask;
        if bool::from(own_scalar.is_zero()) || own_scalar == Scalar::ONE {
            return None;
        }
        let element = -(pwe * mask);
        if bool::from(element.is_identity()) {
            return None;
        }
        Some(Sae {
            pwe,
            rand,
            own_scalar,
            own_element: element.to_affine(),
            peer_scalar: None,
            peer_element: None,
            kck: [0; 32],
            pmk: [0; 32],
            pmkid: [0; 16],
            keys: false,
            send_confirm: 0,
            h2e,
        })
    }

    /// The Commit message fields: group, scalar, element, and an
    /// anti-clogging token if the peer asked for one (before the scalar,
    /// or in a token container element with hash-to-element).
    pub fn commit(&self, token: Option<&[u8]>) -> Vec<u8> {
        let mut m = Vec::with_capacity(COMMIT_LEN + 4 + token.map_or(0, |t| t.len()));
        m.extend_from_slice(&GROUP_P256.to_le_bytes());
        if let (Some(t), false) = (token, self.h2e) {
            m.extend_from_slice(t);
        }
        m.extend_from_slice(&scalar_bytes(&self.own_scalar));
        m.extend_from_slice(&point_bytes(&self.own_element));
        if let (Some(t), true) = (token, self.h2e) {
            m.extend(crate::ie::Builder::new().extension(crate::ie::ext::ANTI_CLOGGING_TOKEN, t).build());
        }
        m
    }

    /// Processes the peer's Commit message (`token_len`: bytes of a token
    /// we required before the scalar, 0 otherwise) and derives the keys.
    pub fn process_commit(&mut self, msg: &[u8], token_len: usize) -> Result<(), SaeError> {
        if msg.len() < 2 {
            return Err(SaeError::Malformed);
        }
        if u16::from_le_bytes([msg[0], msg[1]]) != GROUP_P256 {
            return Err(SaeError::UnsupportedGroup);
        }
        let body = msg.get(2 + token_len..).ok_or(SaeError::Malformed)?;
        if body.len() < 96 {
            return Err(SaeError::Malformed);
        }
        let scalar_b: [u8; 32] = body[..32].try_into().unwrap();
        let scalar = Option::<Scalar>::from(Scalar::from_repr(scalar_b.into())).ok_or(SaeError::BadScalar)?;
        if bool::from(scalar.is_zero()) || scalar == Scalar::ONE {
            return Err(SaeError::BadScalar);
        }
        let x: [u8; 32] = body[32..64].try_into().unwrap();
        let y: [u8; 32] = body[64..96].try_into().unwrap();
        let element = Option::<AffinePoint>::from(AffinePoint::from_coordinates(&x.into(), &y.into()))
            .ok_or(SaeError::BadElement)?;
        if bool::from(element.is_identity()) {
            return Err(SaeError::BadElement);
        }
        if scalar == self.own_scalar && element == self.own_element {
            return Err(SaeError::Reflected);
        }
        // K = rand * (peer-scalar * PWE + PEER-ELEMENT); k = x(K).
        let k_point = (self.pwe * scalar + ProjectivePoint::from(element)) * self.rand;
        if bool::from(k_point.is_identity()) {
            return Err(SaeError::BadElement);
        }
        let mut k: [u8; 32] = k_point.to_affine().x().into();
        let mut keyseed = hmac_sha256(&[0u8; 32], &[&k]);
        k.zeroize();
        let context = scalar_bytes(&(self.own_scalar + scalar));
        let mut both = [0u8; 64];
        kdf_sha256(&keyseed, b"SAE KCK and PMK", &context, &mut both);
        keyseed.zeroize();
        self.kck.copy_from_slice(&both[..32]);
        self.pmk.copy_from_slice(&both[32..]);
        both.zeroize();
        self.pmkid.copy_from_slice(&context[..16]);
        self.peer_scalar = Some(scalar);
        self.peer_element = Some(element);
        self.keys = true;
        Ok(())
    }

    fn confirm_value(
        &self,
        send_confirm: u16,
        first: (&Scalar, &AffinePoint),
        second: (&Scalar, &AffinePoint),
    ) -> [u8; 32] {
        hmac_sha256(
            &self.kck,
            &[
                &send_confirm.to_le_bytes(),
                &scalar_bytes(first.0),
                &point_bytes(first.1),
                &scalar_bytes(second.0),
                &point_bytes(second.1),
            ],
        )
    }

    /// The next Confirm message (send-confirm counter and confirm value).
    pub fn confirm(&mut self) -> Result<Vec<u8>, SaeError> {
        let (Some(ps), Some(pe)) = (self.peer_scalar, self.peer_element) else { return Err(SaeError::State) };
        self.send_confirm = self.send_confirm.saturating_add(1);
        let value = self.confirm_value(self.send_confirm, (&self.own_scalar, &self.own_element), (&ps, &pe));
        let mut m = Vec::with_capacity(34);
        m.extend_from_slice(&self.send_confirm.to_le_bytes());
        m.extend_from_slice(&value);
        Ok(m)
    }

    /// Verifies the peer's Confirm message.
    pub fn verify_confirm(&self, msg: &[u8]) -> Result<(), SaeError> {
        let (Some(ps), Some(pe)) = (self.peer_scalar, self.peer_element) else { return Err(SaeError::State) };
        if msg.len() != 34 {
            return Err(SaeError::Malformed);
        }
        let peer_send = u16::from_le_bytes([msg[0], msg[1]]);
        let expected = self.confirm_value(peer_send, (&ps, &pe), (&self.own_scalar, &self.own_element));
        if ct_eq(&expected, &msg[2..]) { Ok(()) } else { Err(SaeError::BadConfirm) }
    }

    /// The PMK and PMKID once the peer's commit was processed (use them
    /// only after the confirm verified).
    pub fn keys(&self) -> Option<([u8; 32], [u8; 16])> {
        self.keys.then_some((self.pmk, self.pmkid))
    }

    /// The key confirmation key (for tests).
    pub fn kck(&self) -> Option<[u8; 32]> {
        self.keys.then_some(self.kck)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::hex;
    use crate::handshake::tests::TestRandom;

    fn mac(s: &str) -> Mac {
        hex(s).try_into().unwrap()
    }

    fn scalar(s: &str) -> Scalar {
        let b: [u8; 32] = hex(s).try_into().unwrap();
        Option::<Scalar>::from(Scalar::from_repr(b.into())).unwrap()
    }

    /// IEEE 802.11-2020, Annex J.10 (as used by hostap's SAE tests):
    /// hunting and pecking, commit, keys.
    #[test]
    fn annex_j10_hunting_and_pecking() {
        let addr1 = mac("4d3f2fffe387");
        let addr2 = mac("a5d8aa958e3c");
        let pwe = pwe_hunting_and_pecking(&addr1, &addr2, b"mekmitasdigoat", None).unwrap();
        let rand = scalar("992465fd3daa3c60aa6565b7f62a2a7f2e12dd12f198faf4fbed89d7ff1ace94");
        let mask = scalar("9507a90f777a044d6a0830b91ea3d5dd70bece44e1acffb86983b5e1bf9fb322");
        let mut sae = Sae::with_values(pwe, false, rand, mask).unwrap();
        let expected_commit = hex("1300 2e2c0f0db52440ad146d967114ce005ce1eab0aa2c2e5c2871b774f6c2575c65
             d5ad9e00829707aa36ba8b859738fc961d08243505f47c035376d7ac4bc8d7b9
             5083bf43827d0fc31ed778dd3671fd21a46d1091d64b6f9a1e1272621325dbe1");
        assert_eq!(sae.commit(None), expected_commit);
        let peer_commit = hex("1300 591b96f3397fb945100848e7b550543b6720d88337ee93fc49fd6df7e08b5223
             e71b9bb048d3873f20556953a96c91536fd8ee6ca9b4a68a148b056a909be03e
             83ae208f60f8ef5537858074db06687032399862999b511e0a1552a5fea317c2");
        sae.process_commit(&peer_commit, 0).unwrap();
        assert_eq!(
            sae.kck().unwrap().as_slice(),
            hex("1e733f6d9bd53256287304338831b09a39406d121017073a5c30db36f36cb81a").as_slice()
        );
        let (pmk, pmkid) = sae.keys().unwrap();
        assert_eq!(pmk.as_slice(), hex("4e4dfab1a2dd8ac1a91790f953faaa452ae5c6873ab75b63605ba663f8a7fe59").as_slice());
        assert_eq!(pmkid.as_slice(), hex("8747a600eea3f9f22475df58ca1e5498").as_slice());
    }

    /// IEEE 802.11-2020, Annex J.10: hash-to-element PT and PWE.
    #[test]
    fn annex_j10_hash_to_element() {
        let pt = derive_pt(b"byteme", b"mekmitasdigoat", Some(b"psk4internet"));
        let pwe = pwe_from_pt(&pt, &mac("00095b66ec1e"), &mac("000b6bd90246")).unwrap().to_affine();
        assert_eq!(
            point_bytes(&pwe).as_slice(),
            hex("c93049b9e64000f848201649e999f2b5c22dea69b5632c9df4d633b8aa1f6c1e
                 73634e94b53d82e7383a8d258199d9dc1a5ee8269d060382ccbf33e614ff59a0")
            .as_slice()
        );
    }

    /// Runs a whole exchange between two peers; returns both PMKs.
    fn exchange(pw_a: &[u8], pw_b: &[u8], h2e: bool) -> Result<([u8; 32], [u8; 32]), SaeError> {
        let (a, b) = ([2u8, 0, 0, 0, 0, 1], [2u8, 0, 0, 0, 0, 2]);
        let mut rng = TestRandom(42);
        let (pwe_a, pwe_b) = if h2e {
            let pa = derive_pt(b"VindowsNet", pw_a, None);
            let pb = derive_pt(b"VindowsNet", pw_b, None);
            (pwe_from_pt(&pa, &a, &b)?, pwe_from_pt(&pb, &b, &a)?)
        } else {
            (pwe_hunting_and_pecking(&a, &b, pw_a, None)?, pwe_hunting_and_pecking(&b, &a, pw_b, None)?)
        };
        let mut sa = Sae::new(pwe_a, h2e, &mut rng);
        let mut sb = Sae::new(pwe_b, h2e, &mut rng);
        let (ca, cb) = (sa.commit(None), sb.commit(None));
        sa.process_commit(&cb, 0)?;
        sb.process_commit(&ca, 0)?;
        let (fa, fb) = (sa.confirm()?, sb.confirm()?);
        sb.verify_confirm(&fa)?;
        sa.verify_confirm(&fb)?;
        Ok((sa.keys().unwrap().0, sb.keys().unwrap().0))
    }

    #[test]
    fn peers_agree_with_the_same_password() {
        for h2e in [false, true] {
            let (ka, kb) = exchange(b"correct horse battery", b"correct horse battery", h2e).unwrap();
            assert_eq!(ka, kb);
        }
    }

    #[test]
    fn different_passwords_fail_the_confirm() {
        for h2e in [false, true] {
            assert_eq!(exchange(b"password one", b"password two", h2e), Err(SaeError::BadConfirm));
        }
    }

    #[test]
    fn invalid_commits_are_refused() {
        let (a, b) = ([2u8, 0, 0, 0, 0, 1], [2u8, 0, 0, 0, 0, 2]);
        let mut rng = TestRandom(9);
        let pwe = pwe_hunting_and_pecking(&a, &b, b"secret", None).unwrap();
        let mut s = Sae::new(pwe, false, &mut rng);
        let own = s.commit(None);
        // Our own commit sent back.
        assert_eq!(s.process_commit(&own, 0), Err(SaeError::Reflected));
        let mut bad = own.clone();
        bad[0] = 20;
        assert_eq!(s.process_commit(&bad, 0), Err(SaeError::UnsupportedGroup));
        // Scalar of zero, one, and the group order.
        for v in [U256::ZERO, U256::ONE, *NistP256::ORDER] {
            let mut m = own.clone();
            m[2..34].copy_from_slice(&v.to_be_byte_array());
            assert_eq!(s.process_commit(&m, 0), Err(SaeError::BadScalar));
        }
        // An element off the curve.
        let mut off = own.clone();
        off[97] ^= 1;
        assert_eq!(s.process_commit(&off, 0), Err(SaeError::BadElement));
        assert_eq!(s.process_commit(&own[..50], 0), Err(SaeError::Malformed));
        assert_eq!(s.confirm(), Err(SaeError::State));
        let mut seed = 77u32;
        for n in 0..2000usize {
            let mut junk = own.clone();
            for _ in 0..(n % 4) + 1 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let pos = seed as usize % junk.len();
                junk[pos] = (seed >> 8) as u8;
            }
            let _ = s.process_commit(&junk, 0);
        }
    }

    #[test]
    fn anti_clogging_tokens_are_placed_correctly() {
        let (a, b) = ([2u8, 0, 0, 0, 0, 1], [2u8, 0, 0, 0, 0, 2]);
        let mut rng = TestRandom(3);
        let pwe = pwe_hunting_and_pecking(&a, &b, b"secret", None).unwrap();
        let s = Sae::new(pwe, false, &mut rng);
        let m = s.commit(Some(b"TOKEN"));
        assert_eq!(&m[2..7], b"TOKEN");
        assert_eq!(m.len(), COMMIT_LEN + 5);
        let mut peer = Sae::new(pwe_hunting_and_pecking(&b, &a, b"secret", None).unwrap(), false, &mut rng);
        peer.process_commit(&m, 5).unwrap();
        let h = Sae::new(pwe, true, &mut rng);
        let m = h.commit(Some(b"TOKEN"));
        assert_eq!(&m[m.len() - 5..], b"TOKEN");
        assert_eq!(m[COMMIT_LEN], crate::ie::id::EXTENSION);
    }
}
