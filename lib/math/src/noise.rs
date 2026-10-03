//! Deterministic gradient noise: improved Perlin and simplex noise in 2D and
//! 3D, plus fractal (fBm) and ridged sums.
//!
//! A [`Noise`] holds a 512-byte permutation table shuffled from a seed (no
//! allocation), so the same seed always produces the same field on every
//! platform. The raw noise functions return values in roughly `[-1, 1]`
//! (exactly 0 at integer lattice points for Perlin noise); fBm is normalized
//! to the same range and the ridged variant to `[0, 1]`.
//!
//! ```
//! use vmath::Noise;
//!
//! let noise = Noise::new(1234);
//! let h = noise.fbm2(3.7, 1.2, 5, 2.0, 0.5); // terrain height
//! assert!((-1.0..=1.0).contains(&h));
//! assert_eq!(h, Noise::new(1234).fbm2(3.7, 1.2, 5, 2.0, 0.5));
//! ```
//!
//! Perlin noise is the classic choice for fBm; simplex noise has fewer
//! directional artifacts and is cheaper in 3D. Coordinates should stay well
//! below 2^24 in magnitude (beyond that `f32` cannot resolve a lattice cell).

use crate::Rng;
use crate::f32 as m;

/// A seeded noise generator (see the [module docs](self)).
#[derive(Clone)]
pub struct Noise {
    /// A permutation of 0..=255, repeated twice so lookups need no wrapping.
    perm: [u8; 512],
}

impl core::fmt::Debug for Noise {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Noise").finish_non_exhaustive()
    }
}

/// `floor(x)` as an integer (valid for |x| < 2^31).
#[inline(always)]
fn floor_i(x: f32) -> i32 {
    let i = x as i32;
    if (i as f32) > x { i - 1 } else { i }
}

/// Perlin's quintic fade curve 6t⁵ - 15t⁴ + 10t³.
#[inline(always)]
fn fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline(always)]
fn lerp(t: f32, a: f32, b: f32) -> f32 {
    a + t * (b - a)
}

/// Dot product with one of the 12 cube-edge gradients (improved Perlin noise).
#[inline(always)]
fn grad3(hash: u8, x: f32, y: f32, z: f32) -> f32 {
    let h = hash & 15;
    let u = if h < 8 { x } else { y };
    let v = if h < 4 {
        y
    } else if h == 12 || h == 14 {
        x
    } else {
        z
    };
    (if h & 1 == 0 { u } else { -u }) + (if h & 2 == 0 { v } else { -v })
}

/// Dot product with one of 8 gradients (4 diagonals, 4 axes).
#[inline(always)]
fn grad2(hash: u8, x: f32, y: f32) -> f32 {
    match hash & 7 {
        0 => x + y,
        1 => -x + y,
        2 => x - y,
        3 => -x - y,
        4 => x,
        5 => -x,
        6 => y,
        _ => -y,
    }
}

/// Offsets that decorrelate octaves (and avoid every octave of Perlin noise
/// being zero at the origin).
const OCTAVE_OFFSET: [f32; 3] = [19.191_7, 7.377_3, 31.721_3];

/// Normalization of 3D simplex noise with a squared kernel radius of 0.5
/// (its peak raw value is 0.013007, found by hill-climbing searches).
const SIMPLEX3_SCALE: f32 = 76.88;

/// Improved 3D Perlin noise peaks at about ±1.036; this keeps it within ±1.
const PERLIN3_SCALE: f32 = 0.965;

impl Noise {
    /// Creates a generator; equal seeds give identical noise fields.
    pub fn new(seed: u64) -> Self {
        let mut p: [u8; 256] = core::array::from_fn(|i| i as u8);
        Rng::new(seed).shuffle(&mut p);
        Self { perm: core::array::from_fn(|i| p[i & 255]) }
    }

    #[inline(always)]
    fn p(&self, i: usize) -> usize {
        self.perm[i] as usize
    }

    /// Improved Perlin noise in 2D, in `[-1, 1]`.
    pub fn perlin2(&self, x: f32, y: f32) -> f32 {
        let (xi, yi) = (floor_i(x), floor_i(y));
        let (xf, yf) = (x - xi as f32, y - yi as f32);
        let (u, v) = (fade(xf), fade(yf));
        let (xx, yy) = ((xi & 255) as usize, (yi & 255) as usize);
        let a = self.p(xx) + yy;
        let b = self.p(xx + 1) + yy;
        let (aa, ab, ba, bb) = (self.perm[a], self.perm[a + 1], self.perm[b], self.perm[b + 1]);
        lerp(
            v,
            lerp(u, grad2(aa, xf, yf), grad2(ba, xf - 1.0, yf)),
            lerp(u, grad2(ab, xf, yf - 1.0), grad2(bb, xf - 1.0, yf - 1.0)),
        )
    }

    /// Improved Perlin noise in 3D (Perlin 2002), in `[-1, 1]` (scaled by
    /// 0.965 because the raw noise slightly exceeds that range).
    pub fn perlin3(&self, x: f32, y: f32, z: f32) -> f32 {
        let (xi, yi, zi) = (floor_i(x), floor_i(y), floor_i(z));
        let (xf, yf, zf) = (x - xi as f32, y - yi as f32, z - zi as f32);
        let (u, v, w) = (fade(xf), fade(yf), fade(zf));
        let (xx, yy, zz) = ((xi & 255) as usize, (yi & 255) as usize, (zi & 255) as usize);
        let a = self.p(xx) + yy;
        let aa = self.p(a) + zz;
        let ab = self.p(a + 1) + zz;
        let b = self.p(xx + 1) + yy;
        let ba = self.p(b) + zz;
        let bb = self.p(b + 1) + zz;
        let p = &self.perm;
        let (x1, y1, z1) = (xf - 1.0, yf - 1.0, zf - 1.0);
        PERLIN3_SCALE
            * lerp(
                w,
                lerp(
                    v,
                    lerp(u, grad3(p[aa], xf, yf, zf), grad3(p[ba], x1, yf, zf)),
                    lerp(u, grad3(p[ab], xf, y1, zf), grad3(p[bb], x1, y1, zf)),
                ),
                lerp(
                    v,
                    lerp(u, grad3(p[aa + 1], xf, yf, z1), grad3(p[ba + 1], x1, yf, z1)),
                    lerp(u, grad3(p[ab + 1], xf, y1, z1), grad3(p[bb + 1], x1, y1, z1)),
                ),
            )
    }

    /// Simplex noise in 2D (Perlin 2001, after Gustavson), roughly in `[-1, 1]`.
    pub fn simplex2(&self, x: f32, y: f32) -> f32 {
        const F2: f32 = 0.366_025_42; // (√3 - 1) / 2
        const G2: f32 = 0.211_324_87; // (3 - √3) / 6
        // Skew to find the simplex cell.
        let s = (x + y) * F2;
        let (i, j) = (floor_i(x + s), floor_i(y + s));
        let t = (i.wrapping_add(j)) as f32 * G2;
        let x0 = x - (i as f32 - t);
        let y0 = y - (j as f32 - t);
        // The lower or upper triangle of the cell.
        let (i1, j1) = if x0 > y0 { (1, 0) } else { (0, 1) };
        let (x1, y1) = (x0 - i1 as f32 + G2, y0 - j1 as f32 + G2);
        let (x2, y2) = (x0 - 1.0 + 2.0 * G2, y0 - 1.0 + 2.0 * G2);
        let (ii, jj) = ((i & 255) as usize, (j & 255) as usize);
        let g0 = self.perm[ii + self.p(jj)];
        let g1 = self.perm[ii + i1 + self.p(jj + j1)];
        let g2 = self.perm[ii + 1 + self.p(jj + 1)];
        let corner = |g: u8, x: f32, y: f32| {
            let t = 0.5 - x * x - y * y;
            if t <= 0.0 {
                0.0
            } else {
                let t2 = t * t;
                t2 * t2 * grad2(g, x, y)
            }
        };
        70.0 * (corner(g0, x0, y0) + corner(g1, x1, y1) + corner(g2, x2, y2))
    }

    /// Simplex noise in 3D, roughly in `[-1, 1]`. Uses a kernel radius of
    /// √0.5 so that it is continuous everywhere.
    pub fn simplex3(&self, x: f32, y: f32, z: f32) -> f32 {
        const F3: f32 = 1.0 / 3.0;
        const G3: f32 = 1.0 / 6.0;
        let s = (x + y + z) * F3;
        let (i, j, k) = (floor_i(x + s), floor_i(y + s), floor_i(z + s));
        let t = (i.wrapping_add(j).wrapping_add(k)) as f32 * G3;
        let x0 = x - (i as f32 - t);
        let y0 = y - (j as f32 - t);
        let z0 = z - (k as f32 - t);
        // Which of the six tetrahedra of the skewed cube contains the point.
        let (i1, j1, k1, i2, j2, k2) = if x0 >= y0 {
            if y0 >= z0 {
                (1, 0, 0, 1, 1, 0)
            } else if x0 >= z0 {
                (1, 0, 0, 1, 0, 1)
            } else {
                (0, 0, 1, 1, 0, 1)
            }
        } else if y0 < z0 {
            (0, 0, 1, 0, 1, 1)
        } else if x0 < z0 {
            (0, 1, 0, 0, 1, 1)
        } else {
            (0, 1, 0, 1, 1, 0)
        };
        let (x1, y1, z1) = (x0 - i1 as f32 + G3, y0 - j1 as f32 + G3, z0 - k1 as f32 + G3);
        let (x2, y2, z2) = (x0 - i2 as f32 + 2.0 * G3, y0 - j2 as f32 + 2.0 * G3, z0 - k2 as f32 + 2.0 * G3);
        let (x3, y3, z3) = (x0 - 1.0 + 3.0 * G3, y0 - 1.0 + 3.0 * G3, z0 - 1.0 + 3.0 * G3);
        let (ii, jj, kk) = ((i & 255) as usize, (j & 255) as usize, (k & 255) as usize);
        let g0 = self.perm[ii + self.p(jj + self.p(kk))];
        let g1 = self.perm[ii + i1 + self.p(jj + j1 + self.p(kk + k1))];
        let g2 = self.perm[ii + i2 + self.p(jj + j2 + self.p(kk + k2))];
        let g3 = self.perm[ii + 1 + self.p(jj + 1 + self.p(kk + 1))];
        let corner = |g: u8, x: f32, y: f32, z: f32| {
            let t = 0.5 - x * x - y * y - z * z;
            if t <= 0.0 {
                0.0
            } else {
                let t2 = t * t;
                t2 * t2 * grad3(g, x, y, z)
            }
        };
        SIMPLEX3_SCALE
            * (corner(g0, x0, y0, z0) + corner(g1, x1, y1, z1) + corner(g2, x2, y2, z2) + corner(g3, x3, y3, z3))
    }

    /// Fractal Brownian motion: `octaves` layers of [`perlin2`](Self::perlin2),
    /// each `lacunarity` times the frequency and `gain` times the amplitude of
    /// the previous one (typically 2.0 and 0.5), normalized to about `[-1, 1]`.
    pub fn fbm2(&self, x: f32, y: f32, octaves: u32, lacunarity: f32, gain: f32) -> f32 {
        let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
        for o in 0..octaves {
            let off = OCTAVE_OFFSET[o as usize % 3] * o as f32;
            sum += amp * self.perlin2(x * freq + off, y * freq - off);
            norm += amp;
            amp *= gain;
            freq *= lacunarity;
        }
        if norm > 0.0 { sum / norm } else { 0.0 }
    }

    /// Same as [`fbm2`](Self::fbm2).
    #[inline]
    pub fn fbm(&self, x: f32, y: f32, octaves: u32, lacunarity: f32, gain: f32) -> f32 {
        self.fbm2(x, y, octaves, lacunarity, gain)
    }

    /// Same as [`ridged2`](Self::ridged2).
    #[inline]
    pub fn ridged(&self, x: f32, y: f32, octaves: u32, lacunarity: f32, gain: f32) -> f32 {
        self.ridged2(x, y, octaves, lacunarity, gain)
    }

    /// Fractal Brownian motion of [`perlin3`](Self::perlin3) (see [`fbm2`](Self::fbm2)).
    pub fn fbm3(&self, x: f32, y: f32, z: f32, octaves: u32, lacunarity: f32, gain: f32) -> f32 {
        let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
        for o in 0..octaves {
            let off = OCTAVE_OFFSET[o as usize % 3] * o as f32;
            sum += amp * self.perlin3(x * freq + off, y * freq - off, z * freq + 0.5 * off);
            norm += amp;
            amp *= gain;
            freq *= lacunarity;
        }
        if norm > 0.0 { sum / norm } else { 0.0 }
    }

    /// Ridged multifractal noise in `[0, 1]`: like [`fbm2`](Self::fbm2) but
    /// summing `(1 - |noise|)²`, which turns zero crossings into sharp ridges
    /// (mountain ranges, veins).
    pub fn ridged2(&self, x: f32, y: f32, octaves: u32, lacunarity: f32, gain: f32) -> f32 {
        let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
        for o in 0..octaves {
            let off = OCTAVE_OFFSET[o as usize % 3] * o as f32;
            let n = 1.0 - m::abs(self.perlin2(x * freq + off, y * freq - off));
            sum += amp * n * n;
            norm += amp;
            amp *= gain;
            freq *= lacunarity;
        }
        if norm > 0.0 { sum / norm } else { 0.0 }
    }

    /// Ridged multifractal noise of [`perlin3`](Self::perlin3) in `[0, 1]`.
    pub fn ridged3(&self, x: f32, y: f32, z: f32, octaves: u32, lacunarity: f32, gain: f32) -> f32 {
        let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
        for o in 0..octaves {
            let off = OCTAVE_OFFSET[o as usize % 3] * o as f32;
            let n = 1.0 - m::abs(self.perlin3(x * freq + off, y * freq - off, z * freq + 0.5 * off));
            sum += amp * n * n;
            norm += amp;
            amp *= gain;
            freq *= lacunarity;
        }
        if norm > 0.0 { sum / norm } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Samples a function over a dense pseudo-random set of points.
    fn sample(mut f: impl FnMut(f32, f32, f32) -> f32) -> (f32, f32, f64) {
        let mut rng = Rng::new(77);
        let (mut lo, mut hi, mut sum) = (f32::MAX, f32::MIN, 0.0f64);
        let n = 300_000;
        for _ in 0..n {
            let v = f(rng.range_f32(-300.0, 300.0), rng.range_f32(-300.0, 300.0), rng.range_f32(-300.0, 300.0));
            assert!(v.is_finite());
            lo = lo.min(v);
            hi = hi.max(v);
            sum += v as f64;
        }
        (lo, hi, sum / n as f64)
    }

    #[test]
    fn ranges() {
        let noise = Noise::new(42);
        for (name, (lo, hi, mean), (min, max)) in [
            ("perlin2", sample(|x, y, _| noise.perlin2(x, y)), (-1.0, 1.0)),
            ("perlin3", sample(|x, y, z| noise.perlin3(x, y, z)), (-1.0, 1.0)),
            ("simplex2", sample(|x, y, _| noise.simplex2(x, y)), (-1.0, 1.0)),
            ("simplex3", sample(|x, y, z| noise.simplex3(x, y, z)), (-1.0, 1.0)),
            ("fbm2", sample(|x, y, _| noise.fbm2(x, y, 6, 2.0, 0.5)), (-1.0, 1.0)),
            ("fbm3", sample(|x, y, z| noise.fbm3(x, y, z, 4, 2.0, 0.5)), (-1.0, 1.0)),
            ("ridged2", sample(|x, y, _| noise.ridged2(x, y, 5, 2.0, 0.5)), (0.0, 1.0)),
            ("ridged3", sample(|x, y, z| noise.ridged3(x, y, z, 4, 2.0, 0.5)), (0.0, 1.0)),
        ] {
            std::println!("{name:<9} min {lo:+.4} max {hi:+.4} mean {mean:+.4}");
            assert!(lo >= min && hi <= max, "{name}: [{lo}, {hi}]");
            // The noise is not degenerate: it spans a good part of its range.
            assert!(hi - lo > 0.5 * (max - min), "{name}: [{lo}, {hi}]");
            if min < 0.0 {
                assert!(mean.abs() < 0.03, "{name}: mean {mean}");
            }
        }
    }

    #[test]
    fn deterministic_and_seeded() {
        let a = Noise::new(1);
        let b = Noise::new(1);
        let c = Noise::new(2);
        let mut differs = false;
        for i in 0..100 {
            let (x, y, z) = (i as f32 * 0.37, i as f32 * 0.11, i as f32 * 0.73);
            assert_eq!(a.perlin3(x, y, z), b.perlin3(x, y, z));
            assert_eq!(a.simplex2(x, y), b.simplex2(x, y));
            differs |= a.perlin3(x, y, z) != c.perlin3(x, y, z);
        }
        assert!(differs);
    }

    #[test]
    fn perlin_is_zero_on_lattice_and_periodic() {
        let n = Noise::new(9);
        for i in -5..5 {
            for j in -5..5 {
                assert_eq!(n.perlin2(i as f32, j as f32), 0.0);
                assert_eq!(n.perlin3(i as f32, j as f32, 3.0), 0.0);
            }
        }
        // The permutation table repeats every 256 units (up to f32 rounding of
        // the coordinates).
        assert_eq!(n.perlin2(1.25, 2.75), n.perlin2(257.25, 2.75));
        assert!((n.perlin2(1.3, 2.7) - n.perlin2(257.3, 2.7)).abs() < 1e-4);
        assert!((n.perlin3(1.3, 2.7, 0.4) - n.perlin3(1.3, 258.7, 0.4)).abs() < 1e-4);
    }

    #[test]
    fn continuity() {
        // Small steps produce small changes everywhere (also across cell and
        // simplex boundaries).
        let n = Noise::new(5);
        let mut rng = Rng::new(6);
        let h = 1e-3;
        for _ in 0..20_000 {
            let (x, y, z) = (rng.range_f32(-20.0, 20.0), rng.range_f32(-20.0, 20.0), rng.range_f32(-20.0, 20.0));
            assert!((n.perlin2(x + h, y) - n.perlin2(x, y)).abs() < 0.01);
            assert!((n.perlin3(x, y + h, z) - n.perlin3(x, y, z)).abs() < 0.01);
            assert!((n.simplex2(x + h, y + h) - n.simplex2(x, y)).abs() < 0.02);
            assert!((n.simplex3(x, y, z + h) - n.simplex3(x, y, z)).abs() < 0.02);
        }
        // Octaves at the origin are decorrelated.
        assert_ne!(n.fbm2(0.0, 0.0, 4, 2.0, 0.5), 0.0);
        assert_eq!(n.fbm2(1.0, 2.0, 0, 2.0, 0.5), 0.0);
    }
}
