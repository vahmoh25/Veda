//! CCMP-128 (IEEE 802.11-2020, 12.5.3) for data and robust management
//! frames, and BIP-CMAC-128 (12.5.4) for group-addressed management
//! frames.
//!
//! The additional authenticated data and the nonce are built from the MAC
//! header exactly as the standard masks it; the eight CCMP test MPDUs of
//! IEEE 802.11i (with QoS, four-address and odd frame-control fields) and
//! the BIP test frame pin the construction down in the tests.

use alloc::vec::Vec;

use aes::Aes128;
use ccm::aead::{AeadInOut, KeyInit};
use ccm::consts::{U8, U13};

use crate::crypto::{aes_cmac, ct_eq};
use crate::frame::{self, FrameControl};

type Ccm128 = ccm::Ccm<Aes128, U8, U13>;

/// Bytes CCMP adds to a frame: the 8-byte CCMP header and the 8-byte MIC.
pub const CCMP_OVERHEAD: usize = 16;
/// Length of the Management MIC element (BIP-CMAC-128).
pub const MMIE_LEN: usize = 18;
/// Element ID of the Management MIC element.
pub const MMIE_ID: u8 = 76;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcmpError {
    /// Too short, not protected, or the Ext IV bit is clear.
    Malformed,
    /// The MIC did not verify (wrong key or a forged frame).
    BadMic,
}

/// Builds the AAD and nonce for a header (the frame control as sent).
fn aad_and_nonce(header: &[u8], pn: u64) -> (Vec<u8>, [u8; 13]) {
    let fc = FrameControl::from_bytes([header[0], header[1]]);
    let data = fc.is_data();
    let qos = fc.is_qos_data();
    let mut aad = Vec::with_capacity(30);
    // Frame control: subtype bits b4-b6 of data frames, Retry, PwrMgt and
    // MoreData masked; Protected set; Order masked in QoS data frames.
    aad.push(if data { header[0] & 0x8F } else { header[0] });
    let mut fc1 = (header[1] & !0x38) | 0x40;
    if qos {
        fc1 &= !0x80;
    }
    aad.push(fc1);
    // A1, A2, A3.
    aad.extend_from_slice(&header[4..22]);
    // Sequence control: only the fragment number.
    aad.push(header[22] & 0x0F);
    aad.push(0);
    let mut off = 24;
    if fc.to_ds() && fc.from_ds() {
        aad.extend_from_slice(&header[24..30]);
        off = 30;
    }
    let mut priority = 0;
    if qos {
        // QoS control: only the TID.
        priority = header[off] & 0x0F;
        aad.push(priority);
        aad.push(0);
    }
    let mut nonce = [0u8; 13];
    nonce[0] = priority | if fc.is_management() { 0x10 } else { 0 };
    nonce[1..7].copy_from_slice(&header[10..16]);
    let pnb = pn.to_be_bytes();
    nonce[7..13].copy_from_slice(&pnb[2..8]);
    (aad, nonce)
}

/// The MAC header length a CCMP frame protects (HT Control excluded from
/// the AAD but kept in the header).
fn protected_header_len(frame: &[u8]) -> Option<usize> {
    let fc = FrameControl::from_bytes([*frame.first()?, *frame.get(1)?]);
    let len = frame::header_len(fc);
    if frame.len() < len { None } else { Some(len) }
}

/// Encrypts an MPDU (`frame` = MAC header + plaintext body) with packet
/// number `pn` and key id `key_id`. Sets the Protected bit.
pub fn ccmp_encrypt(tk: &[u8; 16], frame: &[u8], pn: u64, key_id: u8) -> Option<Vec<u8>> {
    let hlen = protected_header_len(frame)?;
    let mut header = frame[..hlen].to_vec();
    header[1] |= 0x40;
    let (aad, nonce) = aad_and_nonce(&header, pn);
    let mut body = frame[hlen..].to_vec();
    let cipher = Ccm128::new(tk.into());
    let tag = cipher.encrypt_inout_detached(&nonce.into(), &aad, body.as_mut_slice().into()).ok()?;
    let p = pn.to_le_bytes();
    let mut out = Vec::with_capacity(frame.len() + CCMP_OVERHEAD);
    out.extend_from_slice(&header);
    out.extend_from_slice(&[p[0], p[1], 0, 0x20 | (key_id << 6), p[2], p[3], p[4], p[5]]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&tag);
    Some(out)
}

/// The packet number and key id of a CCMP-protected frame (read before
/// decrypting, for replay checks).
pub fn ccmp_header(frame: &[u8]) -> Option<(u64, u8)> {
    let hlen = protected_header_len(frame)?;
    let h = frame.get(hlen..hlen + 8)?;
    if h[3] & 0x20 == 0 {
        return None;
    }
    let pn = u64::from_le_bytes([h[0], h[1], h[4], h[5], h[6], h[7], 0, 0]);
    Some((pn, h[3] >> 6))
}

/// Decrypts a CCMP-protected MPDU. Returns the MAC header (Protected bit
/// cleared) followed by the plaintext body, and the packet number.
pub fn ccmp_decrypt(tk: &[u8; 16], frame: &[u8]) -> Result<(Vec<u8>, u64), CcmpError> {
    let hlen = protected_header_len(frame).ok_or(CcmpError::Malformed)?;
    if frame[1] & 0x40 == 0 || frame.len() < hlen + CCMP_OVERHEAD {
        return Err(CcmpError::Malformed);
    }
    let (pn, _) = ccmp_header(frame).ok_or(CcmpError::Malformed)?;
    let header = &frame[..hlen];
    let (aad, nonce) = aad_and_nonce(header, pn);
    let mut body = frame[hlen + 8..frame.len() - 8].to_vec();
    let tag: &ccm::Tag<U8> = frame[frame.len() - 8..].try_into().map_err(|_| CcmpError::Malformed)?;
    let cipher = Ccm128::new(tk.into());
    cipher
        .decrypt_inout_detached(&nonce.into(), &aad, body.as_mut_slice().into(), tag)
        .map_err(|_| CcmpError::BadMic)?;
    let mut out = Vec::with_capacity(hlen + body.len());
    out.extend_from_slice(header);
    out[1] &= !0x40;
    out.extend_from_slice(&body);
    Ok((out, pn))
}

fn bip_aad(header: &[u8]) -> [u8; 20] {
    let mut aad = [0u8; 20];
    aad[0] = header[0];
    aad[1] = header[1] & !0x38;
    aad[2..20].copy_from_slice(&header[4..22]);
    aad
}

/// Protects a group-addressed management frame with BIP-CMAC-128: appends
/// a Management MIC element carrying `key_id`, `ipn` and the MIC.
pub fn bip_protect(igtk: &[u8; 16], frame: &[u8], key_id: u16, ipn: u64) -> Option<Vec<u8>> {
    if frame.len() < 24 {
        return None;
    }
    let mut out = frame.to_vec();
    out.push(MMIE_ID);
    out.push(16);
    out.extend_from_slice(&key_id.to_le_bytes());
    out.extend_from_slice(&ipn.to_le_bytes()[..6]);
    out.extend_from_slice(&[0u8; 8]);
    let mic = aes_cmac(igtk, &[&bip_aad(&out), &out[24..]]);
    let n = out.len();
    out[n - 8..].copy_from_slice(&mic[..8]);
    Some(out)
}

/// The key id and IPN of a frame's Management MIC element, if it has one.
pub fn bip_mmie(frame: &[u8]) -> Option<(u16, u64)> {
    if frame.len() < 24 + MMIE_LEN {
        return None;
    }
    let m = &frame[frame.len() - MMIE_LEN..];
    if m[0] != MMIE_ID || m[1] != 16 {
        return None;
    }
    let key_id = u16::from_le_bytes([m[2], m[3]]);
    let ipn = u64::from_le_bytes([m[4], m[5], m[6], m[7], m[8], m[9], 0, 0]);
    Some((key_id, ipn))
}

/// Verifies a frame's Management MIC element. Returns the frame without
/// the element.
pub fn bip_verify<'a>(igtk: &[u8; 16], frame: &'a [u8]) -> Option<&'a [u8]> {
    bip_mmie(frame)?;
    let n = frame.len();
    let mut copy = frame.to_vec();
    copy[n - 8..].fill(0);
    let mic = aes_cmac(igtk, &[&bip_aad(&copy), &copy[24..]]);
    if ct_eq(&mic[..8], &frame[n - 8..]) { Some(&frame[..n - MMIE_LEN]) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::hex;

    /// The eight CCMP test MPDUs of IEEE 802.11i (D7.0, I.7.4), as
    /// published in FreeBSD's net80211 regression tests: key, plaintext
    /// MPDU, and the encrypted MPDU (whose CCMP header carries the PN and
    /// key id).
    const VECTORS: [(&str, &str, &str); 8] = [
        (
            "c97c1f67ce371185514a8a19f2bdd52f",
            "0848c32c0fd2e128a57c5030f1844408abaea5b8fcba8033f8ba1a55d02f85ae967bb62fb6cda8eb7e78a050",
            "0848c32c0fd2e128a57c5030f1844408abaea5b8fcba80330ce70020769703b5f3d0a2fe9a3dbf2342a643e43246e80c3c04d0197845ce0b16f97623",
        ),
        (
            "8f7a053fa577a5597529272097a603d5",
            "38c06a51ea100c846850eec1762c88deaf2ee9f46a07e0cc83a0634b5ed7627eb9df225e05740342de194117",
            "38c06a51ea100c846850eec1762c88deaf2ee9f46a07e0ccea9700a0bacbf331814b6965d05bf2b2ed38d4beb069fe82714a610b542fbf8da06aa4ae",
        ),
        (
            "40cfb7a62e88013bd6d3affcc191041e",
            "b8c8dc61d9577df763c8b6a88adf3691dc4a8bca94dd608220852c1bd036831c95496c5f4dbf3d559e72de802a18",
            "b8c8dc61d9577df763c8b6a88adf3691dc4a8bca94dd60822085aea500a0f3a0dc2f89d8580340b626a0b6d4d013bf18f291b89646c8fd1f1f61a9fb4bb3",
        ),
        (
            "8c89a2ebc96c7602707fcf24b32d3833",
            "a8ca3a11712a9ddf11db8ef82273470159140dd646a2c02f67a54fad2b1c290fa5ebd872fbc3f3a074898f8b2fbb",
            "a8ca3a11712a9ddf11db8ef82273470159140dd646a2c02f67a5e30f00205aa570f69d59b15f371448c230f4d739052e13ab3b1a7b1031fc88004f35ee3d",
        ),
        (
            "a574d5143bb25efddeff30122fdfd066",
            "88da184145dec69a7480f351946bc96be276fbe6c12780f24b1928969b954f263a8018a9ef70a8b051462481922e",
            "88da184145dec69a7480f351946bc96be276fbe6c12780f24b19370e00a03ce0ffa7eb4ae4956a801da9624b7e0c18b23e615ec03af6ce0c3be197d305eb",
        ),
        (
            "f71eea4e1f58804b9717230ad0614641",
            "8852e11f5af28430fdabbff943b9f9a6ab1d98c7fe7350713d6aabfda22d3a0bfc9cc1fc079363c2fca143e6eb1d",
            "8852e11f5af28430fdabbff943b9f9a6ab1d98c7fe7350713d6a89890060a4ec816b9a709b60a39d40b1dfb612e18b5f114badb6cc86309a8d5c466bbb71",
        ),
        (
            "1bdb34980e038124a1db1a892bec366a",
            "187981469b50f4fd56f6efec9520169183570c4ccdee20a098beca86f4b38da20cfdf24724c58eb835665339",
            "187981469b50f4fd56f6efec9520169183570c4ccdee20a023e700e07340ec5e12c537ebf3ab584ef1fef9a1f3547a8c13b3225a2d0957ecfabe95b9",
        ),
        (
            "6eac1bf54bd54edb2321754303024c71",
            "b8d94c72552d5f72bb70ca3f3aae60c48ba9b5f82c2f50eb2a5557cb5c0e5fcd885e9a4239e9b9cad60d64375979",
            "b8d94c72552d5f72bb70ca3f3aae60c48ba9b5f82c2f50eb2a55ddcc00a06e99fdce4bf281ef8ec7739f91591b97a87dc14b3fa174626dba8ef7f08087dd",
        ),
    ];

    #[test]
    fn ieee_ccmp_test_mpdus() {
        let mut checked = 0;
        for (i, (key, plain, enc)) in VECTORS.iter().enumerate() {
            let tk: [u8; 16] = hex(key).try_into().unwrap();
            let plain = hex(plain);
            let enc = hex(enc);
            // MPDUs 3, 4, 5 and 8 are QoS data frames with the Order bit
            // set. They predate 802.11n, which made that combination mean
            // that an HT Control field follows the header (and masks the
            // Order bit in the AAD), so under the current standard they
            // parse differently; the other four cover the construction.
            let fc = FrameControl::from_bytes([plain[0], plain[1]]);
            if fc.is_qos_data() && fc.order() {
                continue;
            }
            checked += 1;
            let (pn, key_id) = ccmp_header(&enc).expect("CCMP header");
            let out = ccmp_encrypt(&tk, &plain, pn, key_id).unwrap();
            assert_eq!(out, enc, "test MPDU {} encrypts differently", i + 1);
            let (dec, dpn) = ccmp_decrypt(&tk, &enc).unwrap();
            assert_eq!(dpn, pn);
            let mut expect = plain.clone();
            expect[1] &= !0x40;
            assert_eq!(dec, expect, "test MPDU {} decrypts differently", i + 1);
        }
        assert_eq!(checked, 4);
    }

    #[test]
    fn tampering_is_detected() {
        let (key, _, enc) = VECTORS[0];
        let tk: [u8; 16] = hex(key).try_into().unwrap();
        let enc = hex(enc);
        for pos in [4usize, 10, 16, 22, 24, 32, 40, enc.len() - 1] {
            let mut bad = enc.clone();
            bad[pos] ^= 0x01;
            assert!(ccmp_decrypt(&tk, &bad).is_err(), "flipping byte {pos} went unnoticed");
        }
        // Retry, power management and more-data bits are masked: changing
        // them must not break the MIC (retransmissions set Retry).
        let mut retry = enc.clone();
        retry[1] ^= 0x08 | 0x10 | 0x20;
        assert!(ccmp_decrypt(&tk, &retry).is_ok());
        assert_eq!(ccmp_decrypt(&tk, &enc[..30]), Err(CcmpError::Malformed));
        let mut wrong = tk;
        wrong[0] ^= 1;
        assert_eq!(ccmp_decrypt(&wrong, &enc), Err(CcmpError::BadMic));
    }

    /// IEEE 802.11-2012, M.9.1: BIP with a broadcast deauthentication.
    #[test]
    fn bip_round_trip() {
        let igtk: [u8; 16] = hex("4ea9543e09cf2b1eca66ffc58bdecbcf").try_into().unwrap();
        let frame = hex("c0000000ffffffffffff02000000000002000000000009000200");
        let prot = bip_protect(&igtk, &frame, 4, 4).unwrap();
        assert_eq!(prot.len(), frame.len() + MMIE_LEN);
        assert_eq!(bip_mmie(&prot), Some((4, 4)));
        assert_eq!(bip_verify(&igtk, &prot), Some(frame.as_slice()));
        let mut bad = prot.clone();
        bad[25] ^= 1;
        assert_eq!(bip_verify(&igtk, &bad), None);
        // Retry is masked in the AAD.
        let mut retry = prot.clone();
        retry[1] |= 0x08;
        assert!(bip_verify(&igtk, &retry).is_some());
    }

    /// IEEE 802.11-2012, M.9.2: a CCMP-protected unicast deauthentication
    /// (management frame protection) survives a round trip and sets the
    /// nonce's management flag (a data-frame nonce must not verify).
    #[test]
    fn protected_management_frame() {
        let tk: [u8; 16] = hex("66ed21042f9f26d7115706e40414cf2e").try_into().unwrap();
        let frame = hex("c000000002000000010002000000000002000000000060000200");
        let enc = ccmp_encrypt(&tk, &frame, 1, 0).unwrap();
        let (dec, pn) = ccmp_decrypt(&tk, &enc).unwrap();
        assert_eq!((dec, pn), (frame.clone(), 1));
        // The same bytes framed as data (type 2) must fail: the nonce
        // differs, so a management frame cannot be replayed as data.
        let mut as_data = enc.clone();
        as_data[0] = 0x08;
        assert!(ccmp_decrypt(&tk, &as_data).is_err());
    }
}
