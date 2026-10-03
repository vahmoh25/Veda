//! Record protection: AES-128-GCM, AES-256-GCM and ChaCha20-Poly1305 for
//! TLS 1.3 (RFC 8446 section 5.2) and TLS 1.2 (RFC 5288, RFC 7905).
//!
//! In both versions the nonce is the connection's IV XORed with the record
//! sequence number. TLS 1.2 GCM records also carry the last 8 bytes of that
//! nonce ("explicit nonce") in front of the ciphertext; the receiver uses
//! whatever the sender put there.

use alloc::boxed::Box;

use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use rustls::crypto::cipher::{
    AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, KeyBlockShape, MessageDecrypter, MessageEncrypter,
    NONCE_LEN, Nonce, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload, Tls12AeadAlgorithm,
    Tls13AeadAlgorithm, UnsupportedOperationError, make_tls12_aad, make_tls13_aad,
};
use rustls::{ConnectionTrafficSecrets, ContentType, Error, ProtocolVersion};

/// Length of the authentication tag of every supported AEAD.
const TAG_LEN: usize = 16;
/// Length of the explicit nonce in TLS 1.2 GCM records.
const GCM_EXPLICIT_NONCE_LEN: usize = 8;
/// Largest plaintext of a record (2^14 bytes).
const MAX_FRAGMENT_LEN: usize = 16384;

/// An AEAD algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Aes128Gcm,
    Aes256Gcm,
    ChaCha20Poly1305,
}

impl Algorithm {
    const fn key_len(self) -> usize {
        match self {
            Algorithm::Aes128Gcm => 16,
            Algorithm::Aes256Gcm | Algorithm::ChaCha20Poly1305 => 32,
        }
    }

    /// The traffic secrets in rustls' exportable form.
    fn secrets(self, key: AeadKey, iv: Iv) -> ConnectionTrafficSecrets {
        match self {
            Algorithm::Aes128Gcm => ConnectionTrafficSecrets::Aes128Gcm { key, iv },
            Algorithm::Aes256Gcm => ConnectionTrafficSecrets::Aes256Gcm { key, iv },
            Algorithm::ChaCha20Poly1305 => ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv },
        }
    }
}

/// A keyed AEAD cipher (always boxed whole, so the variants' sizes do not
/// matter).
#[allow(clippy::large_enum_variant)]
enum Cipher {
    Aes128Gcm(Aes128Gcm),
    Aes256Gcm(Aes256Gcm),
    ChaCha20Poly1305(ChaCha20Poly1305),
}

impl Cipher {
    /// The cipher for `key`, or `None` if the key has the wrong length.
    fn new(algorithm: Algorithm, key: &[u8]) -> Option<Box<Cipher>> {
        let cipher = match algorithm {
            Algorithm::Aes128Gcm => Cipher::Aes128Gcm(Aes128Gcm::new_from_slice(key).ok()?),
            Algorithm::Aes256Gcm => Cipher::Aes256Gcm(Aes256Gcm::new_from_slice(key).ok()?),
            Algorithm::ChaCha20Poly1305 => Cipher::ChaCha20Poly1305(ChaCha20Poly1305::new_from_slice(key).ok()?),
        };
        Some(Box::new(cipher))
    }

    /// Encrypts `data` in place and returns the tag.
    fn seal(&self, nonce: [u8; NONCE_LEN], aad: &[u8], data: &mut [u8]) -> Result<[u8; TAG_LEN], Error> {
        let tag = match self {
            Cipher::Aes128Gcm(c) => c.encrypt_inout_detached(&nonce.into(), aad, data.into()),
            Cipher::Aes256Gcm(c) => c.encrypt_inout_detached(&nonce.into(), aad, data.into()),
            Cipher::ChaCha20Poly1305(c) => c.encrypt_inout_detached(&nonce.into(), aad, data.into()),
        };
        tag.map(Into::into).map_err(|_| Error::EncryptError)
    }

    /// Checks `tag` and decrypts `data` in place.
    fn open(&self, nonce: [u8; NONCE_LEN], aad: &[u8], data: &mut [u8], tag: [u8; TAG_LEN]) -> Result<(), Error> {
        let tag = tag.into();
        let result = match self {
            Cipher::Aes128Gcm(c) => c.decrypt_inout_detached(&nonce.into(), aad, data.into(), &tag),
            Cipher::Aes256Gcm(c) => c.decrypt_inout_detached(&nonce.into(), aad, data.into(), &tag),
            Cipher::ChaCha20Poly1305(c) => c.decrypt_inout_detached(&nonce.into(), aad, data.into(), &tag),
        };
        result.map_err(|_| Error::DecryptError)
    }
}

/// Splits the trailing tag off a ciphertext.
fn split_tag(payload: &mut [u8]) -> Option<(&mut [u8], [u8; TAG_LEN])> {
    let at = payload.len().checked_sub(TAG_LEN)?;
    let (data, tag) = payload.split_at_mut(at);
    Some((data, tag.try_into().ok()?))
}

/// A cipher that fails every operation: what an encrypter or decrypter
/// gets if rustls ever handed over a key of the wrong length.
fn cipher_or_fail(cipher: &Option<Box<Cipher>>) -> Result<&Cipher, Error> {
    cipher.as_deref().ok_or(Error::General("invalid AEAD key length".into()))
}

/// A TLS 1.3 AEAD algorithm.
pub(crate) struct Tls13Aead(pub(crate) Algorithm);

impl Tls13AeadAlgorithm for Tls13Aead {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Encrypter { cipher: Cipher::new(self.0, key.as_ref()), iv })
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Decrypter { cipher: Cipher::new(self.0, key.as_ref()), iv })
    }

    fn key_len(&self) -> usize {
        self.0.key_len()
    }

    fn extract_keys(&self, key: AeadKey, iv: Iv) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(self.0.secrets(key, iv))
    }
}

struct Tls13Encrypter {
    cipher: Option<Box<Cipher>>,
    iv: Iv,
}

impl MessageEncrypter for Tls13Encrypter {
    fn encrypt(&mut self, msg: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, Error> {
        let cipher = cipher_or_fail(&self.cipher)?;
        // TLSInnerPlaintext: the content, then its real type.
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&msg.payload);
        payload.extend_from_slice(&msg.typ.to_array());
        let tag = cipher.seal(Nonce::new(&self.iv, seq).0, &make_tls13_aad(total_len), payload.as_mut())?;
        payload.extend_from_slice(&tag);
        // Every protected TLS 1.3 record looks like TLS 1.2 application data.
        Ok(OutboundOpaqueMessage::new(ContentType::ApplicationData, ProtocolVersion::TLSv1_2, payload))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG_LEN
    }
}

struct Tls13Decrypter {
    cipher: Option<Box<Cipher>>,
    iv: Iv,
}

impl MessageDecrypter for Tls13Decrypter {
    fn decrypt<'a>(&mut self, mut msg: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, Error> {
        let cipher = cipher_or_fail(&self.cipher)?;
        let aad = make_tls13_aad(msg.payload.len());
        let (data, tag) = split_tag(&mut msg.payload).ok_or(Error::DecryptError)?;
        cipher.open(Nonce::new(&self.iv, seq).0, &aad, data, tag)?;
        let plain_len = data.len();
        msg.payload.truncate(plain_len);
        // Strips the padding and recovers the real content type.
        msg.into_tls13_unpadded_message()
    }
}

/// A TLS 1.2 AEAD algorithm.
pub(crate) struct Tls12Aead(pub(crate) Algorithm);

impl Tls12Aead {
    fn is_gcm(&self) -> bool {
        self.0 != Algorithm::ChaCha20Poly1305
    }

    /// The full 12-byte starting IV: for GCM the 4-byte salt from the key
    /// block plus 8 more bytes of the key block (an unpredictable start for
    /// the explicit nonces), for ChaCha20-Poly1305 the 12-byte IV itself.
    fn full_iv(&self, iv: &[u8], extra: &[u8]) -> Option<Iv> {
        let mut full = [0u8; NONCE_LEN];
        if self.is_gcm() {
            if iv.len() + extra.len() != NONCE_LEN {
                return None;
            }
            full[..iv.len()].copy_from_slice(iv);
            full[iv.len()..].copy_from_slice(extra);
        } else {
            full.copy_from_slice(iv.get(..NONCE_LEN)?);
        }
        Some(Iv::from(full))
    }
}

impl Tls12AeadAlgorithm for Tls12Aead {
    fn encrypter(&self, key: AeadKey, iv: &[u8], extra: &[u8]) -> Box<dyn MessageEncrypter> {
        let iv = self.full_iv(iv, extra);
        let cipher = Cipher::new(self.0, key.as_ref()).filter(|_| iv.is_some());
        Box::new(Tls12Encrypter { cipher, iv: iv.unwrap_or_default(), gcm: self.is_gcm() })
    }

    fn decrypter(&self, key: AeadKey, iv: &[u8]) -> Box<dyn MessageDecrypter> {
        let mut salt = [0u8; NONCE_LEN];
        let valid_iv = if self.is_gcm() { iv.len() == 4 } else { iv.len() == NONCE_LEN };
        if valid_iv {
            salt[..iv.len()].copy_from_slice(iv);
        }
        let cipher = Cipher::new(self.0, key.as_ref()).filter(|_| valid_iv);
        Box::new(Tls12Decrypter { cipher, salt, gcm: self.is_gcm() })
    }

    fn key_block_shape(&self) -> KeyBlockShape {
        if self.is_gcm() {
            KeyBlockShape { enc_key_len: self.0.key_len(), fixed_iv_len: 4, explicit_nonce_len: GCM_EXPLICIT_NONCE_LEN }
        } else {
            KeyBlockShape { enc_key_len: self.0.key_len(), fixed_iv_len: NONCE_LEN, explicit_nonce_len: 0 }
        }
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: &[u8],
        explicit: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        let iv = self.full_iv(iv, explicit).ok_or(UnsupportedOperationError)?;
        Ok(self.0.secrets(key, iv))
    }
}

struct Tls12Encrypter {
    cipher: Option<Box<Cipher>>,
    iv: Iv,
    gcm: bool,
}

impl MessageEncrypter for Tls12Encrypter {
    fn encrypt(&mut self, msg: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, Error> {
        let cipher = cipher_or_fail(&self.cipher)?;
        let nonce = Nonce::new(&self.iv, seq).0;
        let aad = make_tls12_aad(seq, msg.typ, msg.version, msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(self.encrypted_payload_len(msg.payload.len()));
        let start = if self.gcm {
            payload.extend_from_slice(&nonce[4..]);
            GCM_EXPLICIT_NONCE_LEN
        } else {
            0
        };
        payload.extend_from_chunks(&msg.payload);
        let tag = cipher.seal(nonce, &aad, &mut payload.as_mut()[start..])?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(msg.typ, msg.version, payload))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + if self.gcm { GCM_EXPLICIT_NONCE_LEN } else { 0 } + TAG_LEN
    }
}

struct Tls12Decrypter {
    cipher: Option<Box<Cipher>>,
    /// GCM: the 4-byte salt (the rest of the nonce comes with each record);
    /// ChaCha20-Poly1305: the IV.
    salt: [u8; NONCE_LEN],
    gcm: bool,
}

impl MessageDecrypter for Tls12Decrypter {
    fn decrypt<'a>(&mut self, mut msg: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, Error> {
        let cipher = cipher_or_fail(&self.cipher)?;
        let explicit_len = if self.gcm { GCM_EXPLICIT_NONCE_LEN } else { 0 };
        if msg.payload.len() < explicit_len + TAG_LEN {
            return Err(Error::DecryptError);
        }
        let plain_len = msg.payload.len() - explicit_len - TAG_LEN;
        if plain_len > MAX_FRAGMENT_LEN {
            return Err(Error::PeerSentOversizedRecord);
        }
        let nonce = if self.gcm {
            let mut nonce = self.salt;
            nonce[4..].copy_from_slice(&msg.payload[..GCM_EXPLICIT_NONCE_LEN]);
            nonce
        } else {
            Nonce::new(&Iv::from(self.salt), seq).0
        };
        let aad = make_tls12_aad(seq, msg.typ, msg.version, plain_len);
        let (data, tag) = split_tag(&mut msg.payload[explicit_len..]).ok_or(Error::DecryptError)?;
        cipher.open(nonce, &aad, data, tag)?;
        Ok(msg.into_plain_message_range(explicit_len..explicit_len + plain_len))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::tests::support::hex;

    fn iv(bytes: &[u8]) -> Iv {
        Iv::from(<[u8; NONCE_LEN]>::try_from(bytes).unwrap())
    }

    fn tls13_encrypter(alg: Algorithm, key: &[u8], iv_bytes: &[u8]) -> Tls13Encrypter {
        Tls13Encrypter { cipher: Cipher::new(alg, key), iv: iv(iv_bytes) }
    }

    fn tls13_decrypter(alg: Algorithm, key: &[u8], iv_bytes: &[u8]) -> Tls13Decrypter {
        Tls13Decrypter { cipher: Cipher::new(alg, key), iv: iv(iv_bytes) }
    }

    /// Protects one TLS 1.3 record and returns it encoded.
    fn seal13(enc: &mut dyn MessageEncrypter, typ: ContentType, plain: &[u8], seq: u64) -> Vec<u8> {
        let msg = OutboundPlainMessage { typ, version: ProtocolVersion::TLSv1_3, payload: plain.into() };
        enc.encrypt(msg, seq).unwrap().encode()
    }

    /// Removes the protection of an encoded record: the content type and
    /// plaintext.
    fn open(dec: &mut dyn MessageDecrypter, record: &[u8], seq: u64) -> Result<(ContentType, Vec<u8>), Error> {
        let mut payload = record[5..].to_vec();
        let typ = ContentType::from(record[0]);
        let version = ProtocolVersion::from(u16::from_be_bytes([record[1], record[2]]));
        let plain = dec.decrypt(InboundOpaqueMessage::new(typ, version, &mut payload), seq)?;
        Ok((plain.typ, plain.payload.to_vec()))
    }

    /// The records of RFC 8448 section 3 (a TLS 1.3 handshake with
    /// TLS_AES_128_GCM_SHA256), with the traffic keys it derives.
    #[test]
    fn rfc8448_records() {
        let alg = Algorithm::Aes128Gcm;

        // The server's encrypted handshake flight (EncryptedExtensions to
        // Finished), under the server handshake traffic key.
        let mut dec = tls13_decrypter(alg, &hex("3fce516009c21727d0f2e4e86ee403bc"), &hex("5d313eb2671276ee13000b30"));
        let (typ, plain) = open(&mut dec, &hex(RFC8448_SERVER_FLIGHT), 0).unwrap();
        assert_eq!(typ, ContentType::Handshake);
        assert_eq!(plain.len(), 657);
        assert_eq!(plain[..8], hex("080000240022000a")[..]);
        assert_eq!(plain[plain.len() - 36..], hex(RFC8448_SERVER_FINISHED)[..]);

        // The client's Finished under the client handshake traffic key.
        let mut enc = tls13_encrypter(alg, &hex("dbfaa693d1762c5b666af5d950258d01"), &hex("5bd3c71b836e0b76bb73265f"));
        let finished = hex("14000020a8ec436d677634ae525ac1fcebe11a039ec17694fac6e98527b642f2edd5ce61");
        assert_eq!(
            seal13(&mut enc, ContentType::Handshake, &finished, 0),
            hex(concat!(
                "170303003575ec4dc238cce60b298044a71e219c56cc77b0517fe9b93c7a4bfc44d87f38f80338ac98fc46deb384bd",
                "1caeacab6867d726c40546"
            ))
        );

        // Application data and close_notify alerts in both directions.
        let counting: Vec<u8> = (0..50).collect();
        let mut enc = tls13_encrypter(alg, &hex("17422dda596ed5d9acd890e3c63f5051"), &hex("5b78923dee08579033e523d9"));
        assert_eq!(
            seal13(&mut enc, ContentType::ApplicationData, &counting, 0),
            hex(concat!(
                "1703030043a23f7054b62c94d0affafe8228ba55cbefacea42f914aa66bcab3f2b9819a8a5b46b395bd54a9a20441e2b",
                "62974e1f5a6292a2977014bd1e3deae63aeebb21694915e4"
            ))
        );
        assert_eq!(
            seal13(&mut enc, ContentType::Alert, &[1, 0], 1),
            hex("1703030013c9872760655666b74d7ff1153efd6db6d0b0e3")
        );
        let mut dec = tls13_decrypter(alg, &hex("9f02283b6c9c07efc26bb9f2ac92e356"), &hex("cf782b88dd83549aadf1e984"));
        let (typ, plain) = open(&mut dec, &hex(RFC8448_SERVER_TICKET), 0).unwrap();
        assert_eq!((typ, plain.len(), plain[0]), (ContentType::Handshake, 205, 4));
        let server_data = hex(concat!(
            "17030300432e937e11ef4ac740e538ad36005fc4a46932fc3225d05f82aa1b36e30efaf97d90e6dffc602dcb501a59a8",
            "fcc49c4bf2e5f0a21c0047c2abf332540dd032e167c2955d"
        ));
        assert_eq!(open(&mut dec, &server_data, 1).unwrap(), (ContentType::ApplicationData, counting.clone()));
        assert_eq!(
            open(&mut dec, &hex("1703030013b58fd67166ebf599d24720cfbe7efa7a8864a9"), 2).unwrap(),
            (ContentType::Alert, alloc::vec![1, 0])
        );
        // The same record under the wrong sequence number is rejected.
        assert_eq!(open(&mut dec, &server_data, 2), Err(Error::DecryptError));
    }

    /// Protect-then-unprotect through every algorithm and both versions,
    /// against tampering and short records.
    #[test]
    fn round_trips_and_tampering() {
        for alg in [Algorithm::Aes128Gcm, Algorithm::Aes256Gcm, Algorithm::ChaCha20Poly1305] {
            let key = &[7u8; 32][..alg.key_len()];
            let iv13 = [9u8; 12];

            // TLS 1.3.
            let mut enc = tls13_encrypter(alg, key, &iv13);
            let mut dec = tls13_decrypter(alg, key, &iv13);
            let record = seal13(&mut enc, ContentType::ApplicationData, b"hello, world", 5);
            assert_eq!(record.len(), 5 + enc.encrypted_payload_len(12));
            assert_eq!(open(&mut dec, &record, 5).unwrap(), (ContentType::ApplicationData, b"hello, world".to_vec()));
            assert_eq!(open(&mut dec, &record, 6), Err(Error::DecryptError));
            let mut tampered = record.clone();
            tampered[8] ^= 1;
            assert_eq!(open(&mut dec, &tampered, 5), Err(Error::DecryptError));
            for len in 0..=TAG_LEN {
                let short = [&record[..5], &alloc::vec![0u8; len][..]].concat();
                assert!(open(&mut dec, &short, 0).is_err());
            }

            // TLS 1.2: a key block shaped as rustls expects.
            let tls12 = Tls12Aead(alg);
            let shape = tls12.key_block_shape();
            assert_eq!(shape.enc_key_len, alg.key_len());
            let block: Vec<u8> = (1..=(shape.fixed_iv_len + shape.explicit_nonce_len) as u8).collect();
            let (fixed, extra) = block.split_at(shape.fixed_iv_len);
            let gcm = alg != Algorithm::ChaCha20Poly1305;
            let mut enc =
                Tls12Encrypter { cipher: Cipher::new(alg, key), iv: tls12.full_iv(fixed, extra).unwrap(), gcm };
            let mut salt = [0u8; NONCE_LEN];
            salt[..fixed.len()].copy_from_slice(fixed);
            let mut dec = Tls12Decrypter { cipher: Cipher::new(alg, key), salt, gcm };
            let msg = OutboundPlainMessage {
                typ: ContentType::ApplicationData,
                version: ProtocolVersion::TLSv1_2,
                payload: b"tls 1.2 data".as_slice().into(),
            };
            let record = enc.encrypt(msg, 3).unwrap().encode();
            assert_eq!(record.len(), 5 + enc.encrypted_payload_len(12));
            assert_eq!(open(&mut dec, &record, 3).unwrap(), (ContentType::ApplicationData, b"tls 1.2 data".to_vec()));
            // The sequence number and the record header are authenticated.
            assert_eq!(open(&mut dec, &record, 4), Err(Error::DecryptError));
            let mut retyped = record.clone();
            retyped[0] = 0x16;
            assert_eq!(open(&mut dec, &retyped, 3), Err(Error::DecryptError));
            let mut tampered = record.clone();
            *tampered.last_mut().unwrap() ^= 0x80;
            assert_eq!(open(&mut dec, &tampered, 3), Err(Error::DecryptError));
            for len in 0..TAG_LEN + GCM_EXPLICIT_NONCE_LEN {
                let short = [&record[..5], &alloc::vec![0u8; len][..]].concat();
                assert!(open(&mut dec, &short, 0).is_err());
            }
        }
    }

    /// TLS 1.2 GCM receivers take the explicit nonce from the record, so
    /// records from a peer that numbers its nonces differently decrypt too.
    #[test]
    fn tls12_gcm_uses_the_explicit_nonce_it_receives() {
        let key = [3u8; 16];
        let salt = [0xa0, 0xa1, 0xa2, 0xa3];
        // RFC 5288: nonce = salt || explicit; AAD = seq || type || version || length.
        let explicit = [0x55u8; 8];
        let mut nonce = [0u8; NONCE_LEN];
        nonce[..4].copy_from_slice(&salt);
        nonce[4..].copy_from_slice(&explicit);
        let mut body = b"independent nonce".to_vec();
        let aad = make_tls12_aad(7, ContentType::ApplicationData, ProtocolVersion::TLSv1_2, body.len());
        let tag = Aes128Gcm::new_from_slice(&key)
            .unwrap()
            .encrypt_inout_detached(&nonce.into(), &aad, body.as_mut_slice().into())
            .unwrap();
        let payload = [&explicit[..], &body, &tag].concat();
        let record = [&[0x17, 0x03, 0x03, 0, payload.len() as u8][..], &payload].concat();
        let mut full_salt = [0u8; NONCE_LEN];
        full_salt[..4].copy_from_slice(&salt);
        let mut dec = Tls12Decrypter { cipher: Cipher::new(Algorithm::Aes128Gcm, &key), salt: full_salt, gcm: true };
        assert_eq!(open(&mut dec, &record, 7).unwrap().1, b"independent nonce");
    }

    #[test]
    fn wrong_key_lengths_fail_closed() {
        assert!(Cipher::new(Algorithm::Aes128Gcm, &[0; 32]).is_none());
        let mut enc = Tls13Encrypter { cipher: Cipher::new(Algorithm::Aes256Gcm, &[0; 16]), iv: iv(&[0; 12]) };
        let msg = OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_3,
            payload: b"x".as_slice().into(),
        };
        assert!(enc.encrypt(msg, 0).is_err());
        // Key blocks of the wrong shape give failing encrypters, not panics.
        let mut enc = Tls12Aead(Algorithm::Aes128Gcm).encrypter(AeadKey::from([0u8; 32]), &[0; 3], &[0; 8]);
        let msg = OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_2,
            payload: b"x".as_slice().into(),
        };
        assert!(enc.encrypt(msg, 0).is_err());
    }

    /// RFC 8448 section 3: the server's protected handshake flight.
    const RFC8448_SERVER_FLIGHT: &str = concat!(
        "17030302a2d1ff334a56f5bff6594a07cc87b580233f500f45e489e7f33af35edf7869fcf40aa40aa2b8ea73f848a7ca",
        "07612ef9f945cb960b4068905123ea78b111b429ba9191cd05d2a389280f526134aadc7fc78c4b729df828b5ecf7b13b",
        "d9aefb0e57f271585b8ea9bb355c7c79020716cfb9b1183ef3ab20e37d57a6b9d7477609aee6e122a4cf51427325250c",
        "7d0e509289444c9b3a648f1d71035d2ed65b0e3cdd0cbae8bf2d0b227812cbb360987255cc744110c453baa4fcd61092",
        "8d809810e4b7ed1a8fd991f06aa6248204797e36a6a73b70a2559c09ead686945ba246ab66e5edd8044b4c6de3fcf2a8",
        "9441ac66272fd8fb330ef8190579b3684596c960bd596eea520a56a8d650f563aad27409960dca63d3e688611ea5e22f",
        "4415cf9538d51a200c27034272968a264ed6540c84838d89f72c24461aad6d26f59ecaba9acbbb317b66d902f4f292a3",
        "6ac1b639c637ce343117b659622245317b49eeda0c6258f100d7d961ffb138647e92ea330faeea6dfa31c7a84dc3bd7e",
        "1b7a6c7178af36879018e3f252107f243d243dc7339d5684c8b0378bf30244da8c87c843f5e56eb4c5e8280a2b48052c",
        "f93b16499a66db7cca71e4599426f7d461e66f99882bd89fc50800becca62d6c74116dbd2972fda1fa80f85df881edbe",
        "5a37668936b335583b599186dc5c6918a396fa48a181d6b6fa4f9d62d513afbb992f2b992f67f8afe67f76913fa388cb",
        "5630c8ca01e0c65d11c66a1e2ac4c85977b7c7a6999bbf10dc35ae69f5515614636c0b9b68c19ed2e31c0b3b66763038",
        "ebba42f3b38edc0399f3a9f23faa63978c317fc9fa66a73f60f0504de93b5b845e275592c12335ee340bbc4fddd50278",
        "4016e4b3be7ef04dda49f4b440a30cb5d2af939828fd4ae3794e44f94df5a631ede42c1719bfdabf0253fe5175be898e",
        "750edc53370d2b",
    );

    /// RFC 8448 section 3: the server's Finished message.
    const RFC8448_SERVER_FINISHED: &str = "140000209b9b141d906337fbd2cbdce71df4deda4ab42c309572cb7fffee5454b78f0718";

    /// RFC 8448 section 3: the server's NewSessionTicket record.
    const RFC8448_SERVER_TICKET: &str = concat!(
        "17030300de3a6b8f90414a97d6959c3487680de5134a2b240e6cffac116e95d41d6af8f6b580dcf3d11d63c758db289a",
        "015940252f55713e061dc13e078891a38efbcf5753ad8ef170ad3c7353d16d9da773b9ca7f2b9fa1b6c0d4a3d03f75e0",
        "9c30ba1e62972ac46f75f7b981be63439b2999ce13064615139891d5e4c5b406f16e3fc181a77ca475840025db2f0a77",
        "f81b5ab05b94c01346755f69232c86519d86cbeeac87aac347d143f9605d64f650db4d023e70e952ca49fe5137121c74",
        "bc2697687e248746d6df353005f3bce18696129c8153556b3b6c6779b37bf15985684f",
    );
}
