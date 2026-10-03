//! Deterministic hashing, gradient noise and fractal sums.

/// Integer hash of a lattice point (well mixed, platform independent).
#[inline]
pub fn hash(x: i32, y: i32, seed: u32) -> u32 {
    let mut h =
        seed.wrapping_mul(0x9E37_79B9) ^ (x as u32).wrapping_mul(0x27D4_EB2D) ^ (y as u32).wrapping_mul(0x1656_67B1);
    h = (h ^ (h >> 15)).wrapping_mul(0x85EB_CA6B);
    h = (h ^ (h >> 13)).wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

/// A hash mapped to `[0, 1)`.
#[inline]
pub fn hash01(x: i32, y: i32, seed: u32) -> f32 {
    (hash(x, y, seed) >> 8) as f32 / (1u32 << 24) as f32
}

/// `1 / sqrt(2)`.
const H: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Sixteen unit gradient directions.
const GRADS: [(f32, f32); 16] = [
    (1.0, 0.0),
    (0.92388, 0.38268),
    (H, H),
    (0.38268, 0.92388),
    (0.0, 1.0),
    (-0.38268, 0.92388),
    (-H, H),
    (-0.92388, 0.38268),
    (-1.0, 0.0),
    (-0.92388, -0.38268),
    (-H, -H),
    (-0.38268, -0.92388),
    (0.0, -1.0),
    (0.38268, -0.92388),
    (H, -H),
    (0.92388, -0.38268),
];

#[inline]
fn fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// 2D gradient noise in roughly `[-1, 1]`.
pub fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (xf, yf) = (x.floor(), y.floor());
    let (fx, fy) = (x - xf, y - yf);
    let (ix, iy) = (xf as i32, yf as i32);
    let g = |dx: i32, dy: i32| {
        let (gx, gy) = GRADS[(hash(ix + dx, iy + dy, seed) & 15) as usize];
        gx * (fx - dx as f32) + gy * (fy - dy as f32)
    };
    let (u, v) = (fade(fx), fade(fy));
    let a = g(0, 0) + (g(1, 0) - g(0, 0)) * u;
    let b = g(0, 1) + (g(1, 1) - g(0, 1)) * u;
    (a + (b - a) * v) * 1.41
}

/// 1D noise (a slice through the 2D noise).
pub fn noise1(x: f32, seed: u32) -> f32 {
    noise(x, 0.371, seed)
}

/// Fractal Brownian motion: `octaves` layers of noise, each at twice the frequency and half the
/// amplitude, rotated to hide lattice artefacts. Roughly `[-1, 1]`.
pub fn fbm(x: f32, y: f32, octaves: u32, seed: u32) -> f32 {
    let (mut x, mut y) = (x, y);
    let (mut sum, mut amp, mut norm) = (0.0, 1.0, 0.0);
    for i in 0..octaves {
        sum += amp * noise(x, y, seed.wrapping_add(i * 101));
        norm += amp;
        amp *= 0.5;
        // Rotate by ~37 degrees and scale by 2.
        let (nx, ny) = (1.6 * x - 1.2 * y, 1.2 * x + 1.6 * y);
        x = nx + 17.3;
        y = ny - 9.1;
    }
    sum / norm
}

/// Ridged fractal noise in `[0, 1]`: sharp crests like mountain ridges.
pub fn ridged(x: f32, y: f32, octaves: u32, seed: u32) -> f32 {
    let (mut x, mut y) = (x, y);
    let (mut sum, mut amp, mut norm, mut weight) = (0.0, 1.0, 0.0, 1.0f32);
    for i in 0..octaves {
        let n = 1.0 - noise(x, y, seed.wrapping_add(i * 211)).abs();
        let n = n * n * weight;
        weight = (n * 1.6).clamp(0.0, 1.0);
        sum += amp * n;
        norm += amp;
        amp *= 0.5;
        let (nx, ny) = (1.6 * x - 1.2 * y, 1.2 * x + 1.6 * y);
        x = nx + 31.7;
        y = ny + 4.3;
    }
    sum / norm
}

/// A small deterministic random number generator (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform in `[a, b)`.
    pub fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.f32()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_bounded_and_deterministic() {
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for i in 0..10_000 {
            let (x, y) = (i as f32 * 0.137, i as f32 * 0.071 + 3.0);
            let n = noise(x, y, 7);
            lo = lo.min(n);
            hi = hi.max(n);
            assert_eq!(n, noise(x, y, 7));
        }
        assert!(lo > -1.2 && hi < 1.2 && hi - lo > 1.0, "{lo} {hi}");
        let r = ridged(1.3, 2.7, 5, 1);
        assert!((0.0..=1.0).contains(&r));
    }
}
