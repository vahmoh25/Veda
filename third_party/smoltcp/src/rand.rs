#![allow(unsafe_code)]
#![allow(unused)]

//! Random numbers for protocol fields: TCP initial sequence numbers, DHCP
//! transaction ids, IPv4 identification, DNS ids, ephemeral ports.
//!
//! VINDOWS PATCH (see `VINDOWS-PATCHES.md`): upstream smoltcp uses an
//! sPCG32 generator here. Its 64-bit state can be recovered from a few
//! outputs (for example the initial sequence numbers a server sees), which
//! makes the sequence numbers of other connections predictable. This
//! version adds a keyed mode: given `Config::random_key` (256 bits from
//! the operating system's CSPRNG) it produces a ChaCha20 keystream, which
//! is unpredictable. Without a key (`Config::random_seed` only) it behaves
//! exactly like upstream, so upstream's tests still pass byte for byte.

pub(crate) struct Rand {
    /// sPCG32 state (unkeyed mode).
    state: u64,
    /// ChaCha20 key (keyed mode).
    key: Option<[u32; 8]>,
    counter: u64,
    block: [u32; 16],
    /// Next unused word of `block`; 16 means "empty".
    used: usize,
}

impl core::fmt::Debug for Rand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never print the key.
        f.write_str("Rand { .. }")
    }
}

const fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The ChaCha20 block function (RFC 8439) with a 64-bit block counter.
fn chacha20(key: &[u32; 8], counter: u64) -> [u32; 16] {
    let mut init = [0u32; 16];
    init[0] = 0x6170_7865;
    init[1] = 0x3320_646e;
    init[2] = 0x7962_2d32;
    init[3] = 0x6b20_6574;
    init[4..12].copy_from_slice(key);
    init[12] = counter as u32;
    init[13] = (counter >> 32) as u32;
    let mut s = init;
    for _ in 0..10 {
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 1, 5, 9, 13);
        quarter_round(&mut s, 2, 6, 10, 14);
        quarter_round(&mut s, 3, 7, 11, 15);
        quarter_round(&mut s, 0, 5, 10, 15);
        quarter_round(&mut s, 1, 6, 11, 12);
        quarter_round(&mut s, 2, 7, 8, 13);
        quarter_round(&mut s, 3, 4, 9, 14);
    }
    for i in 0..16 {
        s[i] = s[i].wrapping_add(init[i]);
    }
    s
}

impl Rand {
    /// Upstream's deterministic generator, seeded with 64 bits. Not
    /// suitable when the output must be unpredictable.
    pub(crate) const fn new(seed: u64) -> Self {
        Self { state: seed, key: None, counter: 0, block: [0; 16], used: 16 }
    }

    /// A generator keyed with 32 bytes from a cryptographically secure
    /// source.
    pub(crate) fn with_key(bytes: [u8; 32]) -> Self {
        let mut key = [0u32; 8];
        for (i, k) in key.iter_mut().enumerate() {
            *k = u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]);
        }
        Self { state: 0, key: Some(key), counter: 0, block: [0; 16], used: 16 }
    }

    pub(crate) fn rand_u32(&mut self) -> u32 {
        let Some(key) = self.key else {
            // sPCG32 from https://www.pcg-random.org/paper.html (upstream).
            const M: u64 = 0xbb2efcec3c39611d;
            const A: u64 = 0x7590ef39;
            let s = self.state.wrapping_mul(M).wrapping_add(A);
            self.state = s;
            let shift = 29 - (s >> 61);
            return (s >> shift) as u32;
        };
        if self.used == 16 {
            self.block = chacha20(&key, self.counter);
            self.counter = self.counter.wrapping_add(1);
            self.used = 0;
        }
        let v = self.block[self.used];
        // Do not keep used keystream around.
        self.block[self.used] = 0;
        self.used += 1;
        v
    }

    pub(crate) fn rand_u16(&mut self) -> u16 {
        let n = self.rand_u32();
        (n ^ (n >> 16)) as u16
    }

    pub(crate) fn rand_source_port(&mut self) -> u16 {
        loop {
            let res = self.rand_u16();
            if res > 1024 {
                return res;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8439, appendix A.1, test vector #1: an all-zero key gives a known
    /// keystream (the 64-bit counter layout equals the RFC's with a zero
    /// nonce for small counters).
    #[test]
    fn chacha20_zero_key() {
        let block = chacha20(&[0; 8], 0);
        assert_eq!(block[0], u32::from_le_bytes([0x76, 0xb8, 0xe0, 0xad]));
        assert_eq!(block[15], u32::from_le_bytes([0xb2, 0xee, 0x65, 0x86]));
    }

    #[test]
    fn keyed_generators_differ_and_do_not_repeat() {
        let mut a = Rand::with_key([1; 32]);
        let mut b = Rand::with_key([2; 32]);
        let xs: [u32; 40] = core::array::from_fn(|_| a.rand_u32());
        let ys: [u32; 40] = core::array::from_fn(|_| b.rand_u32());
        assert_ne!(xs, ys);
        assert_ne!(xs[..16], xs[16..32]);
    }

    #[test]
    fn source_ports_are_not_privileged() {
        let mut r = Rand::new(7);
        for _ in 0..1000 {
            assert!(r.rand_source_port() > 1024);
        }
    }
}
