//! CRC-32 (ISO-HDLC, as used by PNG, gzip and ZIP) and Adler-32 (zlib) checksums.
//!
//! CRC-32 uses the "slice-by-8" method: eight 256-entry tables (built at compile time) let the
//! loop consume eight bytes per iteration. Both functions follow zlib's conventions, so a
//! checksum can be computed incrementally by feeding the previous result back in.

/// The eight slice-by-8 tables; `TABLES[k][b]` is the CRC of byte `b` followed by `k` zero bytes.
static TABLES: [[u32; 256]; 8] = make_tables();

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

/// Extends a CRC-32 with `data`. Start with `crc = 0`; `crc32_update(crc32_update(0, a), b)` equals
/// the CRC of `a` followed by `b`.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let t = &TABLES;
    let mut c = !crc;
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        c = t[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Computes the CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// The Adler-32 modulus.
const ADLER_MOD: u32 = 65521;
/// Largest number of bytes that can be summed before `b` may overflow a `u32`.
const ADLER_NMAX: usize = 5552;

/// Extends an Adler-32 with `data`. Start with `adler = 1`.
pub fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let mut a = adler & 0xFFFF;
    let mut b = adler >> 16;
    for block in data.chunks(ADLER_NMAX) {
        let mut quads = block.chunks_exact(4);
        for q in &mut quads {
            a += q[0] as u32;
            b += a;
            a += q[1] as u32;
            b += a;
            a += q[2] as u32;
            b += a;
            a += q[3] as u32;
            b += a;
        }
        for &x in quads.remainder() {
            a += x as u32;
            b += a;
        }
        a %= ADLER_MOD;
        b %= ADLER_MOD;
    }
    (b << 16) | a
}

/// Computes the Adler-32 of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    adler32_update(1, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn incremental_and_long() {
        let data: alloc::vec::Vec<u8> = (0..100_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        // Bytewise reference implementations.
        let mut c = !0u32;
        for &b in &data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        assert_eq!(crc32(&data), !c);
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!(adler32(&data), (b << 16) | a);
        for split in [0, 1, 7, 8, 9, 5551, 5552, 5553, 99_999] {
            let (x, y) = data.split_at(split);
            assert_eq!(crc32_update(crc32(x), y), crc32(&data));
            assert_eq!(adler32_update(adler32(x), y), adler32(&data));
        }
        // All 0xFF bytes is the worst case for Adler-32 overflow.
        let ff = alloc::vec![0xFFu8; 20_000];
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &ff {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!(adler32(&ff), (b << 16) | a);
    }
}
