//! Fast Fourier transforms and spectrum analysis.
//!
//! * [`Fft`]: an in-place radix-2 complex FFT with precomputed twiddles.
//! * [`RealFft`]: the spectrum of `n` real samples computed with one
//!   complex FFT of size `n / 2` (twice as fast as the naive approach).
//! * [`Analyzer`]: turns a block of samples into log-spaced frequency band
//!   levels (0..=1) for a spectrum visualiser — Hann window, real FFT,
//!   magnitudes in decibels.

use alloc::vec;
use alloc::vec::Vec;

use vmath::FloatExt;

/// An in-place complex FFT of a fixed power-of-two size.
#[derive(Debug, Clone)]
pub struct Fft {
    n: usize,
    /// `e^(-2 pi i k / n)` for `k < n / 2`.
    twiddles: Vec<(f32, f32)>,
    /// Bit-reversal permutation.
    rev: Vec<u32>,
}

impl Fft {
    /// An FFT of size `n` (rounded up to a power of two, at least 2).
    pub fn new(n: usize) -> Fft {
        let n = n.max(2).next_power_of_two();
        let bits = n.trailing_zeros();
        let twiddles = (0..n / 2)
            .map(|k| {
                let a = -core::f64::consts::TAU * k as f64 / n as f64;
                (FloatExt::cos(a) as f32, FloatExt::sin(a) as f32)
            })
            .collect();
        let rev = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        Fft { n, twiddles, rev }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Forward transform of `re + i im` in place (both of length `n`).
    pub fn transform(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        assert!(re.len() >= n && im.len() >= n);
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut size = 2;
        while size <= n {
            let half = size / 2;
            let step = n / size;
            let mut start = 0;
            while start < n {
                for k in 0..half {
                    let (wr, wi) = self.twiddles[k * step];
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
                start += size;
            }
            size *= 2;
        }
    }
}

/// The FFT of real input of a fixed power-of-two length.
#[derive(Debug, Clone)]
pub struct RealFft {
    n: usize,
    half: Fft,
    /// `e^(-2 pi i k / n)` for `k <= n / 2`.
    post: Vec<(f32, f32)>,
    zr: Vec<f32>,
    zi: Vec<f32>,
}

impl RealFft {
    /// A real FFT of `n` samples (rounded up to a power of two, at least 4).
    pub fn new(n: usize) -> RealFft {
        let n = n.max(4).next_power_of_two();
        let post = (0..=n / 2)
            .map(|k| {
                let a = -core::f64::consts::TAU * k as f64 / n as f64;
                (FloatExt::cos(a) as f32, FloatExt::sin(a) as f32)
            })
            .collect();
        RealFft { n, half: Fft::new(n / 2), post, zr: vec![0.0; n / 2], zi: vec![0.0; n / 2] }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Computes bins `0..=n/2` of the spectrum of `input` (length `n`)
    /// into `re` and `im` (length `n / 2 + 1`).
    pub fn forward(&mut self, input: &[f32], re: &mut [f32], im: &mut [f32]) {
        let (n, h) = (self.n, self.n / 2);
        assert!(input.len() >= n && re.len() > h && im.len() > h);
        for k in 0..h {
            self.zr[k] = input[2 * k];
            self.zi[k] = input[2 * k + 1];
        }
        self.half.transform(&mut self.zr, &mut self.zi);
        for k in 0..=h {
            let (ar, ai) = (self.zr[k % h], self.zi[k % h]);
            let (br, bi) = (self.zr[(h - k) % h], -self.zi[(h - k) % h]);
            // even = (Z[k] + conj Z[h-k]) / 2, odd = (Z[k] - conj Z[h-k]) / 2i
            let (er, ei) = ((ar + br) * 0.5, (ai + bi) * 0.5);
            let (dr, di) = ((ar - br) * 0.5, (ai - bi) * 0.5);
            let (or, oi) = (di, -dr);
            let (wr, wi) = self.post[k];
            re[k] = er + or * wr - oi * wi;
            im[k] = ei + or * wi + oi * wr;
        }
    }
}

/// A Hann window of length `n`.
pub fn hann(n: usize) -> Vec<f32> {
    let d = (n.max(2) - 1) as f64;
    (0..n).map(|i| (0.5 - 0.5 * FloatExt::cos(core::f64::consts::TAU * i as f64 / d)) as f32).collect()
}

/// Turns blocks of samples into log-spaced band levels for a visualiser.
#[derive(Debug, Clone)]
pub struct Analyzer {
    fft: RealFft,
    window: Vec<f32>,
    /// Fractional FFT bin range of every band.
    bands: Vec<(f32, f32)>,
    input: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    mag: Vec<f32>,
    /// Converts a magnitude to "full scale sine = 1".
    norm: f32,
    floor_db: f32,
    ceil_db: f32,
}

impl Analyzer {
    /// `n` samples per analysis (power of two), `rate` in Hz, `bands`
    /// log-spaced bands from `f_lo` to `f_hi` Hz.
    pub fn new(n: usize, rate: u32, bands: usize, f_lo: f32, f_hi: f32) -> Analyzer {
        let fft = RealFft::new(n);
        let n = fft.len();
        let window = hann(n);
        let wsum: f32 = window.iter().sum();
        let bin_hz = rate as f32 / n as f32;
        let nyq = rate as f32 / 2.0;
        let (lo, hi) = (f_lo.max(bin_hz).min(nyq * 0.5), f_hi.min(nyq));
        let ratio = hi / lo;
        let bands = (0..bands.max(1))
            .map(|b| {
                let f0 = lo * FloatExt::powf(ratio, b as f32 / bands as f32);
                let f1 = lo * FloatExt::powf(ratio, (b + 1) as f32 / bands as f32);
                (f0 / bin_hz, f1 / bin_hz)
            })
            .collect();
        Analyzer {
            window,
            bands,
            input: vec![0.0; n],
            re: vec![0.0; n / 2 + 1],
            im: vec![0.0; n / 2 + 1],
            mag: vec![0.0; n / 2 + 1],
            norm: 2.0 / wsum,
            floor_db: -72.0,
            ceil_db: -6.0,
            fft,
        }
    }

    /// Samples per analysis block.
    pub fn len(&self) -> usize {
        self.fft.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn band_count(&self) -> usize {
        self.bands.len()
    }

    /// Sets the decibel range mapped to 0..=1 (defaults: -72 .. -6 dB).
    pub fn set_range(&mut self, floor_db: f32, ceil_db: f32) {
        self.floor_db = floor_db;
        self.ceil_db = ceil_db.max(floor_db + 1.0);
    }

    /// Analyses `samples` (mono, at least `len()`; extra samples at the
    /// front are ignored) and writes one level in 0..=1 per band.
    pub fn analyze(&mut self, samples: &[f32], levels: &mut [f32]) {
        let n = self.fft.len();
        let src = &samples[samples.len().saturating_sub(n)..];
        for (i, x) in self.input.iter_mut().enumerate() {
            *x = src.get(i).copied().unwrap_or(0.0) * self.window[i];
        }
        self.fft.forward(&self.input, &mut self.re, &mut self.im);
        for ((m, r), i) in self.mag.iter_mut().zip(&self.re).zip(&self.im) {
            *m = FloatExt::sqrt(r * r + i * i) * self.norm;
        }
        let last = self.mag.len() - 1;
        for (b, level) in self.bands.iter().zip(levels.iter_mut()) {
            let (b0, b1) = *b;
            let (i0, i1) = ((b0 as usize).min(last), (FloatExt::ceil(b1) as usize).min(last + 1));
            let peak = if i1 <= i0 + 1 {
                // Narrower than a bin: interpolate.
                let t = b0 - i0 as f32;
                let a = self.mag[i0];
                let c = self.mag[(i0 + 1).min(last)];
                a + (c - a) * t
            } else {
                self.mag[i0..i1].iter().fold(0.0f32, |m, &v| m.max(v))
            };
            let db = 20.0 * FloatExt::log10(peak.max(1e-9));
            *level = ((db - self.floor_db) / (self.ceil_db - self.floor_db)).clamp(0.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(x: &[f32]) -> Vec<(f64, f64)> {
        let n = x.len();
        (0..n)
            .map(|k| {
                let (mut r, mut i) = (0f64, 0f64);
                for (t, &v) in x.iter().enumerate() {
                    let a = -core::f64::consts::TAU * (k * t) as f64 / n as f64;
                    r += v as f64 * a.cos();
                    i += v as f64 * a.sin();
                }
                (r, i)
            })
            .collect()
    }

    #[test]
    fn complex_and_real_fft_match_dft() {
        let mut rng = vmath::Rng::new(1);
        for n in [4usize, 8, 64, 512] {
            let x: Vec<f32> = (0..n).map(|_| rng.range_f32(-1.0, 1.0)).collect();
            let want = naive(&x);
            let mut re = x.clone();
            let mut im = vec![0.0; n];
            Fft::new(n).transform(&mut re, &mut im);
            let mut rf = RealFft::new(n);
            let (mut rr, mut ri) = (vec![0.0; n / 2 + 1], vec![0.0; n / 2 + 1]);
            rf.forward(&x, &mut rr, &mut ri);
            for k in 0..n {
                assert!((re[k] as f64 - want[k].0).abs() < 1e-3 * n as f64, "n={n} k={k}");
                assert!((im[k] as f64 - want[k].1).abs() < 1e-3 * n as f64, "n={n} k={k}");
                if k <= n / 2 {
                    assert!((rr[k] as f64 - want[k].0).abs() < 1e-3 * n as f64, "real n={n} k={k}");
                    assert!((ri[k] as f64 - want[k].1).abs() < 1e-3 * n as f64, "real n={n} k={k}");
                }
            }
        }
    }

    #[test]
    fn analyzer_finds_the_tone() {
        let rate = 44_100;
        let mut a = Analyzer::new(2048, rate, 32, 40.0, 16_000.0);
        let x: Vec<f32> =
            (0..2048).map(|i| 0.5 * (i as f32 * 1000.0 / rate as f32 * core::f32::consts::TAU).sin()).collect();
        let mut levels = vec![0.0; 32];
        a.analyze(&x, &mut levels);
        let (best, _) = levels.iter().enumerate().fold((0, 0.0f32), |b, (i, &v)| if v > b.1 { (i, v) } else { b });
        // 1 kHz lies in band log(1000/40)/log(16000/40)*32 = 17.2.
        assert_eq!(best, 17, "{levels:?}");
        assert!(levels[best] > 0.8);
        assert!(levels[2] < 0.2 && levels[30] < 0.2);
        // Silence maps to zero.
        a.analyze(&vec![0.0; 2048], &mut levels);
        assert!(levels.iter().all(|&l| l == 0.0));
    }
}
