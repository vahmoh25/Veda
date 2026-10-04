//! The algorithms behind the kernel's random number generator.
//!
//! * [`chacha20_block`]: the ChaCha20 block function (RFC 8439).
//! * [`Blake2s`]: the BLAKE2s-256 hash (RFC 7693), used to condense entropy.
//! * [`Generator`]: a cryptographically secure generator in the style of
//!   Linux's: an entropy pool hashed with BLAKE2s feeds the key of a
//!   ChaCha20 stream, and every request replaces the key with fresh
//!   keystream before returning ("fast key erasure"), so a later compromise
//!   of the state does not reveal earlier output.
//!
//! The crate has no platform code: the kernel supplies the entropy (firmware
//! seed, RDSEED/RDRAND, timing jitter, interrupt timing) and the locking.
//! It is `no_std` and dependency-free so the kernel can use it.

#![no_std]

#[cfg(test)]
extern crate std;

mod blake2s;
mod chacha;

pub use blake2s::Blake2s;
pub use chacha::chacha20_block;

/// Bytes in a generator key.
pub const KEY_LEN: usize = 32;

/// A ChaCha20-based generator with an entropy pool.
///
/// Feed entropy with [`Generator::mix`], fold the pool into the key with
/// [`Generator::reseed`], and draw output with [`Generator::fill`]. The
/// caller decides when to reseed (the kernel does it at boot and then
/// periodically).
#[derive(Clone)]
pub struct Generator {
    key: [u8; KEY_LEN],
    pool: Blake2s,
    /// Number of entropy inputs mixed into the pool since the last reseed.
    pending_inputs: u32,
    reseeds: u64,
}

impl Generator {
    /// A generator whose key and pool start from fixed values: it must be
    /// seeded with [`Generator::mix`] and [`Generator::reseed`] before its
    /// output is unpredictable.
    pub const fn new() -> Generator {
        Generator { key: [0; KEY_LEN], pool: Blake2s::new_const(), pending_inputs: 0, reseeds: 0 }
    }

    /// Mixes `data` into the entropy pool. Inputs never reduce the
    /// unpredictability of the pool, so untrusted data may be mixed too.
    pub fn mix(&mut self, data: &[u8]) {
        // Length-prefix each input so concatenations cannot collide.
        self.pool.update(&(data.len() as u64).to_le_bytes());
        self.pool.update(data);
        self.pending_inputs = self.pending_inputs.saturating_add(1);
    }

    /// Folds the entropy pool into the key: `key = BLAKE2s(key ‖ pool)`.
    /// The pool keeps accumulating (it is chained, not cleared).
    pub fn reseed(&mut self) {
        let pool_digest = self.pool.clone().finalize();
        let mut h = Blake2s::new();
        h.update(b"veda reseed");
        h.update(&self.key);
        h.update(&pool_digest);
        h.update(&self.reseeds.to_le_bytes());
        self.key = h.finalize();
        // Chain the pool so the next digest depends on everything so far.
        self.pool.update(&pool_digest);
        self.reseeds += 1;
        self.pending_inputs = 0;
    }

    /// How many times the generator has been reseeded.
    pub fn reseed_count(&self) -> u64 {
        self.reseeds
    }

    /// Entropy inputs mixed since the last reseed.
    pub fn pending_inputs(&self) -> u32 {
        self.pending_inputs
    }

    /// Fills `out` with random bytes.
    ///
    /// The first 32 bytes of keystream become the next key and are never
    /// output; the rest of the stream (block 0's second half, then blocks
    /// 1, 2, ...) is the output.
    pub fn fill(&mut self, out: &mut [u8]) {
        let key = self.key;
        let first = chacha20_block(&key, 0, &[0; 12]);
        self.key.copy_from_slice(&first[..KEY_LEN]);
        let mut done = 0;
        let take = (first.len() - KEY_LEN).min(out.len());
        out[..take].copy_from_slice(&first[KEY_LEN..KEY_LEN + take]);
        done += take;
        let mut counter = 1u32;
        while done < out.len() {
            let block = chacha20_block(&key, counter, &[0; 12]);
            let n = block.len().min(out.len() - done);
            out[done..done + n].copy_from_slice(&block[..n]);
            done += n;
            counter = counter.wrapping_add(1);
        }
    }

    /// Returns a random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }
}

impl Default for Generator {
    fn default() -> Self {
        Generator::new()
    }
}

/// A small mixing pool for interrupt timestamps: cheap enough to run on
/// every interrupt, folded into a [`Generator`] at reseed time. It is not a
/// cryptographic hash; it only has to retain the timing variations until
/// BLAKE2s condenses them.
#[derive(Debug, Clone, Copy, Default)]
pub struct FastPool {
    lanes: [u64; 4],
    count: u32,
}

impl FastPool {
    pub const fn new() -> FastPool {
        FastPool { lanes: [0; 4], count: 0 }
    }

    /// Mixes one sample (for example a TSC reading and an interrupt vector).
    pub fn add(&mut self, a: u64, b: u64) {
        // A SipHash-like round over the four lanes.
        let l = &mut self.lanes;
        l[0] ^= a;
        l[1] ^= b;
        l[0] = l[0].wrapping_add(l[1]);
        l[1] = l[1].rotate_left(13) ^ l[0];
        l[0] = l[0].rotate_left(32);
        l[2] = l[2].wrapping_add(l[3]);
        l[3] = l[3].rotate_left(16) ^ l[2];
        l[0] = l[0].wrapping_add(l[3]);
        l[3] = l[3].rotate_left(21) ^ l[0];
        l[2] = l[2].wrapping_add(l[1]);
        l[1] = l[1].rotate_left(17) ^ l[2];
        l[2] = l[2].rotate_left(32);
        self.count = self.count.wrapping_add(1);
    }

    /// Samples mixed since the pool was last drained.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Returns the pool contents and resets it.
    pub fn drain(&mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, lane) in self.lanes.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&lane.to_le_bytes());
        }
        *self = FastPool::new();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(seed: &[u8]) -> Generator {
        let mut g = Generator::new();
        g.mix(seed);
        g.reseed();
        g
    }

    #[test]
    fn output_depends_on_the_seed() {
        let mut a = seeded(b"seed one");
        let mut b = seeded(b"seed two");
        let (mut x, mut y) = ([0u8; 64], [0u8; 64]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_ne!(x, y);
        // Deterministic for a given seed (needed for reproducible tests).
        let mut c = seeded(b"seed one");
        let mut z = [0u8; 64];
        c.fill(&mut z);
        assert_eq!(x, z);
    }

    #[test]
    fn consecutive_requests_differ_and_rekey() {
        let mut g = seeded(b"x");
        let k0 = g.key;
        let (mut a, mut b) = ([0u8; 100], [0u8; 100]);
        g.fill(&mut a);
        let k1 = g.key;
        g.fill(&mut b);
        assert_ne!(a, b);
        assert_ne!(k0, k1);
        assert_ne!(k1, g.key);
        // The new key is never part of the output.
        assert!(!a.windows(32).any(|w| w == k1));
    }

    #[test]
    fn output_spans_many_blocks_without_repeating() {
        let mut g = seeded(b"long");
        let mut big = std::vec![0u8; 64 * 40 + 7];
        g.fill(&mut big);
        let blocks: std::vec::Vec<&[u8]> = big.chunks(64).collect();
        for i in 0..blocks.len() {
            for j in i + 1..blocks.len() {
                assert_ne!(blocks[i], blocks[j]);
            }
        }
    }

    #[test]
    fn reseeding_changes_the_stream() {
        let mut a = seeded(b"same");
        let mut b = seeded(b"same");
        b.mix(b"extra entropy");
        b.reseed();
        let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_ne!(x, y);
        assert_eq!(b.reseed_count(), 2);
    }

    #[test]
    fn small_requests_work() {
        let mut g = seeded(b"tiny");
        let mut one = [0u8; 1];
        g.fill(&mut one);
        let mut none = [0u8; 0];
        g.fill(&mut none);
        let _ = g.next_u64();
    }

    #[test]
    fn fast_pool_retains_variation() {
        let mut p = FastPool::new();
        let mut q = FastPool::new();
        for i in 0..100u64 {
            p.add(1000 + i * 7, 33);
            q.add(1000 + i * 7 + (i == 50) as u64, 33);
        }
        assert_eq!(p.count(), 100);
        assert_ne!(p.drain(), q.drain());
        assert_eq!(p.count(), 0);
    }
}
