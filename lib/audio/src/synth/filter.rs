//! Filters: a topology-preserving state-variable filter (stable under fast
//! modulation), RBJ biquads for equalisation, one-pole smoothers and a DC
//! blocker.

use core::f32::consts::PI;

use vmath::FloatExt;

/// Filter response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    LowPass,
    BandPass,
    HighPass,
}

/// A TPT state-variable filter (Zavalishin).
#[derive(Debug, Clone)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
    pub mode: Mode,
}

impl Svf {
    pub fn new(mode: Mode) -> Svf {
        let mut f = Svf { ic1: 0.0, ic2: 0.0, a1: 0.0, a2: 0.0, a3: 0.0, k: 1.0, mode };
        f.set(1000.0, 0.707, 44_100.0);
        f
    }

    /// Sets the cut-off (Hz) and resonance (Q, 0.5 = soft, 10 = ringing).
    pub fn set(&mut self, cutoff: f32, q: f32, sr: f32) {
        let fc = cutoff.clamp(10.0, sr * 0.45);
        let g = vmath::f32::tan(PI * fc / sr);
        self.k = 1.0 / q.max(0.1);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    #[inline(always)]
    pub fn process(&mut self, v0: f32) -> f32 {
        let v3 = v0 - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        match self.mode {
            Mode::LowPass => v2,
            Mode::BandPass => v1,
            Mode::HighPass => v0 - self.k * v1 - v2,
        }
    }
}

/// Biquad types (RBJ audio EQ cookbook).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BiquadKind {
    LowPass,
    HighPass,
    /// Shelf gain in dB.
    LowShelf(f32),
    HighShelf(f32),
    /// Peak gain in dB.
    Peak(f32),
}

/// A biquad filter (direct form II transposed).
#[derive(Debug, Clone)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn new(kind: BiquadKind, freq: f32, q: f32, sr: f32) -> Biquad {
        let w0 = 2.0 * PI * freq.clamp(10.0, sr * 0.45) / sr;
        let (sin, cos) = (FloatExt::sin(w0), FloatExt::cos(w0));
        let alpha = sin / (2.0 * q.max(0.1));
        let gain_a = |db: f32| FloatExt::powf(10.0f32, db / 40.0);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            BiquadKind::LowPass => {
                ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            BiquadKind::HighPass => {
                ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            BiquadKind::Peak(db) => {
                let a = gain_a(db);
                (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
            }
            BiquadKind::LowShelf(db) => {
                let a = gain_a(db);
                let s = 2.0 * FloatExt::sqrt(a) * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            BiquadKind::HighShelf(db) => {
                let a = gain_a(db);
                let s = 2.0 * FloatExt::sqrt(a) * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
        };
        Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z1: 0.0, z2: 0.0 }
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// A one-pole low-pass (also used for parameter smoothing).
#[derive(Debug, Clone)]
pub struct OnePole {
    a: f32,
    pub y: f32,
}

impl OnePole {
    pub fn new(cutoff: f32, sr: f32) -> OnePole {
        OnePole { a: 1.0 - FloatExt::exp(-2.0 * PI * cutoff / sr), y: 0.0 }
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        self.y
    }

    /// High-pass output (input minus the low-pass).
    #[inline(always)]
    pub fn highpass(&mut self, x: f32) -> f32 {
        x - self.process(x)
    }
}

/// Removes DC offset.
#[derive(Debug, Clone, Default)]
pub struct DcBlock {
    x1: f32,
    y1: f32,
}

impl DcBlock {
    #[inline(always)]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + 0.995 * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_at(f: &mut dyn FnMut(f32) -> f32, freq: f32, sr: f32) -> f32 {
        let mut peak = 0.0f32;
        for i in 0..(sr as usize) {
            let y = f((i as f32 * freq / sr * core::f32::consts::TAU).sin());
            if i > sr as usize / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn svf_responses() {
        let sr = 44_100.0;
        let mut lp = Svf::new(Mode::LowPass);
        lp.set(1000.0, 0.707, sr);
        assert!(gain_at(&mut |x| lp.process(x), 100.0, sr) > 0.95);
        let mut lp = Svf::new(Mode::LowPass);
        lp.set(1000.0, 0.707, sr);
        assert!(gain_at(&mut |x| lp.process(x), 10_000.0, sr) < 0.02);
        let mut hp = Svf::new(Mode::HighPass);
        hp.set(1000.0, 0.707, sr);
        assert!(gain_at(&mut |x| hp.process(x), 100.0, sr) < 0.02);
    }

    #[test]
    fn biquad_shelves() {
        let sr = 44_100.0;
        let mut b = Biquad::new(BiquadKind::LowShelf(6.0), 200.0, 0.707, sr);
        let low = gain_at(&mut |x| b.process(x), 50.0, sr);
        assert!((low - 2.0).abs() < 0.1, "{low}");
        let mut b = Biquad::new(BiquadKind::HighPass, 30.0, 0.707, sr);
        assert!(gain_at(&mut |x| b.process(x), 1000.0, sr) > 0.99);
    }
}
