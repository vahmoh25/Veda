//! BLAKE2s-256 (RFC 7693), unkeyed.

const IV: [u32; 8] = [0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

/// An incremental BLAKE2s-256 hasher.
#[derive(Clone)]
pub struct Blake2s {
    h: [u32; 8],
    t: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Blake2s {
    /// A hasher for a 32-byte digest without a key.
    pub fn new() -> Blake2s {
        Blake2s::new_const()
    }

    /// [`Blake2s::new`] usable in constant contexts.
    pub const fn new_const() -> Blake2s {
        let mut h = IV;
        // Parameter block: digest length 32, key length 0, fanout 1, depth 1.
        h[0] ^= 0x0101_0020;
        Blake2s { h, t: 0, buf: [0; 64], buf_len: 0 }
    }

    fn compress(&mut self, block: &[u8; 64], last: bool) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= self.t as u32;
        v[13] ^= (self.t >> 32) as u32;
        if last {
            v[14] = !v[14];
        }
        #[inline(always)]
        fn g(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
            v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
            v[d] = (v[d] ^ v[a]).rotate_right(16);
            v[c] = v[c].wrapping_add(v[d]);
            v[b] = (v[b] ^ v[c]).rotate_right(12);
            v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
            v[d] = (v[d] ^ v[a]).rotate_right(8);
            v[c] = v[c].wrapping_add(v[d]);
            v[b] = (v[b] ^ v[c]).rotate_right(7);
        }
        for s in &SIGMA {
            g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            self.h[i] ^= v[i] ^ v[i + 8];
        }
    }

    /// Adds data to the hash.
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // A full buffer is compressed only once more data arrives: the
            // final block must be compressed with the "last" flag.
            if self.buf_len == 64 {
                self.t += 64;
                let block = self.buf;
                self.compress(&block, false);
                self.buf_len = 0;
            }
            let n = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
        }
    }

    /// Finishes the hash and returns the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        self.t += self.buf_len as u64;
        self.buf[self.buf_len..].fill(0);
        let block = self.buf;
        self.compress(&block, true);
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}

impl Default for Blake2s {
    fn default() -> Self {
        Blake2s::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> std::vec::Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn digest(data: &[u8]) -> [u8; 32] {
        let mut h = Blake2s::new();
        h.update(data);
        h.finalize()
    }

    /// RFC 7693, appendix B.
    #[test]
    fn rfc7693_abc() {
        assert_eq!(
            digest(b"abc").as_slice(),
            hex("508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982").as_slice()
        );
    }

    #[test]
    fn empty_input() {
        assert_eq!(
            digest(b"").as_slice(),
            hex("69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9").as_slice()
        );
    }

    #[test]
    fn incremental_updates_match_one_shot() {
        let data: std::vec::Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let whole = digest(&data);
        for split in [0usize, 1, 63, 64, 65, 128, 999] {
            let mut h = Blake2s::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finalize(), whole, "split at {split}");
        }
        // Exactly one block: the block must be compressed as the last one.
        let mut h = Blake2s::new();
        h.update(&[0u8; 64]);
        assert_eq!(h.finalize(), digest(&[0u8; 64]));
    }
}
