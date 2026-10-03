//! A small, fast, seedable pseudo-random number generator.
//!
//! [`Rng`] is PCG32 (O'Neill's "XSH RR" output function on a 64-bit LCG):
//! 16 bytes of state, one 64-bit multiply per 32 random bits, period 2^64,
//! and statistically solid output. It is deterministic: the same seed always
//! yields the same sequence on every platform, which makes it suitable for
//! procedural generation, replays and tests. It is **not** cryptographically
//! secure.
//!
//! ```
//! use vmath::Rng;
//!
//! let mut rng = Rng::new(42);
//! let roll = rng.range_i32(1, 7); // a die: 1..=6
//! assert!((1..7).contains(&roll));
//! let x = rng.next_f32(); // [0, 1)
//! assert!((0.0..1.0).contains(&x));
//! ```

use crate::f32 as m;
use crate::{Vec2, Vec3};

/// PCG32 pseudo-random number generator (see the [module docs](self)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng {
    state: u64,
    inc: u64,
}

const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const DEFAULT_STREAM: u64 = 0x6d1f_1ce5_ca5c_aded; // inc = 0xda3e39cb94b95bdb

/// SplitMix64 finalizer, used to spread seeds over the state space.
#[inline]
const fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Default for Rng {
    /// Same as `Rng::new(0)`.
    fn default() -> Self {
        Self::new(0)
    }
}

impl Rng {
    /// Creates a generator from a seed. Different seeds give unrelated sequences.
    pub const fn new(seed: u64) -> Self {
        Self::with_stream(seed, DEFAULT_STREAM)
    }

    /// Creates a generator from a seed and a stream selector; generators with
    /// the same seed but different streams produce independent sequences.
    pub const fn with_stream(seed: u64, stream: u64) -> Self {
        let mut rng = Rng { state: 0, inc: (stream << 1) | 1 };
        rng.step();
        rng.state = rng.state.wrapping_add(splitmix64(seed));
        rng.step();
        rng
    }

    #[inline(always)]
    const fn step(&mut self) {
        self.state = self.state.wrapping_mul(MULTIPLIER).wrapping_add(self.inc);
    }

    /// The next 32 random bits.
    #[inline]
    pub const fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.step();
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// The next 64 random bits.
    #[inline]
    pub const fn next_u64(&mut self) -> u64 {
        let hi = self.next_u32() as u64;
        (hi << 32) | self.next_u32() as u64
    }

    /// A uniformly distributed `f32` in `[0, 1)` (24 random bits).
    #[inline]
    pub const fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// A uniformly distributed `f64` in `[0, 1)` (53 random bits).
    #[inline]
    pub const fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// A random boolean (50/50).
    #[inline]
    pub const fn next_bool(&mut self) -> bool {
        self.next_u32() >> 31 != 0
    }

    /// A uniformly distributed `f32` in `[a, b)`. (Rounding can make the result
    /// equal to `b` for some ranges; `a` and `b` may be given in either order.)
    #[inline]
    pub const fn range_f32(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.next_f32()
    }

    /// A uniformly distributed `f64` in `[a, b)` (see [`range_f32`](Self::range_f32)).
    #[inline]
    pub const fn range_f64(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.next_f64()
    }

    /// A uniformly distributed integer in `[0, n)`, without modulo bias
    /// (Lemire's method). Returns 0 when `n == 0`.
    #[inline]
    pub const fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let mut m = self.next_u32() as u64 * n as u64;
        let mut low = m as u32;
        if low < n {
            let threshold = n.wrapping_neg() % n;
            while low < threshold {
                m = self.next_u32() as u64 * n as u64;
                low = m as u32;
            }
        }
        (m >> 32) as u32
    }

    /// A uniformly distributed integer in `[0, n)` for `usize` bounds. Returns 0 when `n == 0`.
    #[inline]
    pub const fn below_usize(&mut self, n: usize) -> usize {
        if n <= u32::MAX as usize {
            return self.below(n as u32) as usize;
        }
        // Larger bounds (64-bit only): 128-bit Lemire without the rejection
        // step; the bias is below 2^-32.
        ((self.next_u64() as u128 * n as u128) >> 64) as usize
    }

    /// A uniformly distributed integer in the half-open range `[lo, hi)`.
    /// Returns `lo` when the range is empty (`hi <= lo`).
    #[inline]
    pub const fn range_i32(&mut self, lo: i32, hi: i32) -> i32 {
        if hi <= lo {
            return lo;
        }
        let span = (hi as i64 - lo as i64) as u32;
        (lo as i64 + self.below(span) as i64) as i32
    }

    /// A uniformly distributed integer in the half-open range `[lo, hi)`.
    /// Returns `lo` when the range is empty (`hi <= lo`).
    #[inline]
    pub const fn range_u32(&mut self, lo: u32, hi: u32) -> u32 {
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo)
    }

    /// `true` with probability `p` (`p <= 0` never, `p >= 1` always).
    #[inline]
    pub const fn chance(&mut self, p: f32) -> bool {
        self.next_f32() < p
    }

    /// A uniformly chosen element of `items`, or `None` if it is empty.
    #[inline]
    pub const fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        let i = self.below_usize(items.len());
        Some(&items[i])
    }

    /// Shuffles `items` in place (Fisher–Yates; every permutation is equally likely).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        let mut i = items.len();
        while i > 1 {
            let j = self.below_usize(i);
            i -= 1;
            items.swap(i, j);
        }
    }

    /// A normally distributed value with the given mean and standard deviation
    /// (Box–Muller transform).
    pub fn gaussian(&mut self, mean: f32, std_dev: f32) -> f32 {
        let u1 = 1.0 - self.next_f32(); // (0, 1]: ln(u1) is finite
        let u2 = self.next_f32();
        let r = m::sqrt(-2.0 * m::ln(u1));
        mean + std_dev * r * m::cos(core::f32::consts::TAU * u2)
    }

    /// A uniformly distributed unit vector in 2D (a random direction).
    pub fn unit_vec2(&mut self) -> Vec2 {
        Vec2::from_angle(self.next_f32() * core::f32::consts::TAU)
    }

    /// A uniformly distributed unit vector in 3D (a random point on the unit sphere).
    pub fn unit_vec3(&mut self) -> Vec3 {
        let z = self.range_f32(-1.0, 1.0);
        let (s, c) = m::sin_cos(self.next_f32() * core::f32::consts::TAU);
        let r = m::sqrt(m::max(1.0 - z * z, 0.0));
        Vec3::new(r * c, r * s, z)
    }

    /// A uniformly distributed point inside the unit disc.
    pub fn in_unit_circle(&mut self) -> Vec2 {
        self.unit_vec2() * m::sqrt(self.next_f32())
    }

    /// A uniformly distributed point inside the unit ball.
    pub fn in_unit_sphere(&mut self) -> Vec3 {
        self.unit_vec3() * m::cbrt(self.next_f32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_seed_dependent() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(1);
        let mut c = Rng::new(2);
        let va: [u32; 8] = core::array::from_fn(|_| a.next_u32());
        let vb: [u32; 8] = core::array::from_fn(|_| b.next_u32());
        let vc: [u32; 8] = core::array::from_fn(|_| c.next_u32());
        assert_eq!(va, vb);
        assert_ne!(va, vc);
        let mut d = Rng::with_stream(1, 7);
        let vd: [u32; 8] = core::array::from_fn(|_| d.next_u32());
        assert_ne!(va, vd);
    }

    #[test]
    fn pcg32_reference_stream() {
        // The PCG reference generator (pcg32_srandom(42, 54)) without seed hashing:
        // first outputs 0xa15c02b7, 0x7b47f409, 0xba1d3330.
        let mut rng = Rng { state: 0, inc: (54 << 1) | 1 };
        rng.step();
        rng.state = rng.state.wrapping_add(42);
        rng.step();
        assert_eq!([rng.next_u32(), rng.next_u32(), rng.next_u32()], [0xa15c_02b7, 0x7b47_f409, 0xba1d_3330]);
    }

    #[test]
    fn ranges() {
        let mut rng = Rng::new(7);
        let mut seen = [false; 6];
        for _ in 0..10_000 {
            let f = rng.next_f32();
            assert!((0.0..1.0).contains(&f));
            let d = rng.next_f64();
            assert!((0.0..1.0).contains(&d));
            let r = rng.range_f32(-3.0, 5.0);
            assert!((-3.0..=5.0).contains(&r));
            let i = rng.range_i32(-2, 4);
            assert!((-2..4).contains(&i));
            seen[(i + 2) as usize] = true;
            let u = rng.range_u32(10, 13);
            assert!((10..13).contains(&u));
            assert!(rng.below(1) == 0);
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(rng.range_i32(5, 5), 5);
        assert_eq!(rng.range_i32(5, -5), 5);
        assert_eq!(rng.below(0), 0);
        let full = rng.range_i32(i32::MIN, i32::MAX);
        assert!(full < i32::MAX);
    }

    #[test]
    fn uniformity_of_below() {
        let mut rng = Rng::new(123);
        let mut counts = [0u32; 10];
        let n = 200_000;
        for _ in 0..n {
            counts[rng.below(10) as usize] += 1;
        }
        for &c in &counts {
            let expected = n as f64 / 10.0;
            assert!((c as f64 - expected).abs() < expected * 0.03, "{counts:?}");
        }
    }

    #[test]
    fn chance_choose_shuffle() {
        let mut rng = Rng::new(99);
        assert!(!rng.chance(0.0));
        assert!(rng.chance(1.0));
        let hits = (0..100_000).filter(|_| rng.chance(0.25)).count();
        assert!((hits as f64 / 100_000.0 - 0.25).abs() < 0.01);

        let empty: [u8; 0] = [];
        assert!(rng.choose(&empty).is_none());
        let items = [1, 2, 3];
        for _ in 0..100 {
            assert!(items.contains(rng.choose(&items).unwrap()));
        }

        let mut v: [u32; 32] = core::array::from_fn(|i| i as u32);
        rng.shuffle(&mut v);
        let mut sorted = v;
        sorted.sort_unstable();
        assert_eq!(sorted, core::array::from_fn::<u32, 32, _>(|i| i as u32));
        assert_ne!(v, sorted, "shuffle left the array unchanged");
        let mut one = [5];
        rng.shuffle(&mut one);
        assert_eq!(one, [5]);
    }

    #[test]
    fn gaussian_moments() {
        let mut rng = Rng::new(2024);
        let n = 200_000;
        let (mut sum, mut sum2) = (0.0f64, 0.0f64);
        for _ in 0..n {
            let g = rng.gaussian(3.0, 2.0) as f64;
            assert!(g.is_finite());
            sum += g;
            sum2 += g * g;
        }
        let mean = sum / n as f64;
        let var = sum2 / n as f64 - mean * mean;
        assert!((mean - 3.0).abs() < 0.02, "mean {mean}");
        assert!((var.sqrt() - 2.0).abs() < 0.02, "std dev {}", var.sqrt());
    }

    #[test]
    fn random_vectors() {
        let mut rng = Rng::new(5);
        for _ in 0..1000 {
            assert!((rng.unit_vec2().length() - 1.0).abs() < 1e-5);
            assert!((rng.unit_vec3().length() - 1.0).abs() < 1e-5);
            assert!(rng.in_unit_circle().length() <= 1.0 + 1e-6);
            assert!(rng.in_unit_sphere().length() <= 1.0 + 1e-6);
        }
    }
}
