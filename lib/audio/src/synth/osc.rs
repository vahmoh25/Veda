//! Oscillators: a table sine, band-limited (PolyBLEP) saw, square and
//! pulse waves, an integrated triangle, white noise and an NES-style
//! stepped triangle and LFSR noise for chiptune sounds.

/// Entries of the sine table (one period).
const TABLE: usize = 2048;

/// One period of a sine, plus a guard entry for interpolation.
static SINE: [f32; TABLE + 1] = {
    let mut t = [0.0f32; TABLE + 1];
    let mut i = 0;
    while i <= TABLE {
        t[i] = vmath::f32::sin(i as f32 * (core::f32::consts::TAU / TABLE as f32));
        i += 1;
    }
    t
};

/// Fractional part for any finite `x` (result in 0..1).
#[inline(always)]
pub fn fract(x: f32) -> f32 {
    let f = x - (x as i64) as f32;
    if f < 0.0 { f + 1.0 } else { f }
}

/// `sin(2 pi phase)` from the table with linear interpolation.
#[inline(always)]
pub fn sin_turns(phase: f32) -> f32 {
    let x = fract(phase) * TABLE as f32;
    let i = (x as usize).min(TABLE - 1);
    let f = x - i as f32;
    let a = SINE[i];
    a + (SINE[i + 1] - a) * f
}

/// The PolyBLEP residual for a discontinuity at phase 0 (`t` in 0..1,
/// `dt` = phase increment per sample).
#[inline(always)]
pub fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// Oscillator wave shapes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wave {
    Sine,
    Saw,
    Square,
    /// Pulse with the given duty cycle (0..1).
    Pulse(f32),
    Triangle,
    Noise,
    /// 4-bit stepped triangle (NES).
    ChipTriangle,
    /// Pulse without band-limiting (classic chip sound).
    ChipPulse(f32),
}

/// A phase-accumulating oscillator.
#[derive(Debug, Clone)]
pub struct Osc {
    pub wave: Wave,
    phase: f32,
    tri: f32,
    rng: u32,
}

impl Osc {
    pub fn new(wave: Wave, phase: f32, seed: u32) -> Osc {
        let p = fract(phase);
        // Start the integrated triangle on its ideal value (no DC offset).
        let tri = if p < 0.5 { -1.0 + 4.0 * p } else { 3.0 - 4.0 * p };
        Osc { wave, phase: p, tri, rng: seed | 1 }
    }

    /// White noise in -1..1 (xorshift).
    #[inline(always)]
    pub fn noise(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as i32) as f32 * (1.0 / 2_147_483_648.0)
    }

    /// The next sample; `inc` is frequency / sample rate.
    #[inline(always)]
    pub fn next(&mut self, inc: f32) -> f32 {
        let inc = inc.clamp(0.0, 0.49);
        let t = self.phase;
        let out = match self.wave {
            Wave::Sine => sin_turns(t),
            Wave::Saw => 2.0 * t - 1.0 - poly_blep(t, inc),
            Wave::Square => {
                let mut v = if t < 0.5 { 1.0 } else { -1.0 };
                v += poly_blep(t, inc);
                v -= poly_blep(fract(t + 0.5), inc);
                v
            }
            Wave::Pulse(duty) => {
                let d = duty.clamp(0.05, 0.95);
                let mut v = if t < d { 1.0 } else { -1.0 };
                v += poly_blep(t, inc);
                v -= poly_blep(fract(t + 1.0 - d), inc);
                v
            }
            Wave::Triangle => {
                let mut sq = if t < 0.5 { 1.0 } else { -1.0 };
                sq += poly_blep(t, inc);
                sq -= poly_blep(fract(t + 0.5), inc);
                // Leaky integration of the square.
                self.tri = self.tri * 0.9995 + 4.0 * inc * sq;
                self.tri
            }
            Wave::Noise => self.noise(),
            Wave::ChipTriangle => {
                let x = if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 };
                ((x * 15.0) as i32 as f32 / 7.5) - 1.0
            }
            Wave::ChipPulse(duty) => {
                if t < duty {
                    1.0
                } else {
                    -1.0
                }
            }
        };
        self.phase = fract(t + inc);
        out
    }
}

/// NES-style noise: a 15-bit LFSR clocked at `rate` Hz (short mode gives
/// metallic tones).
#[derive(Debug, Clone)]
pub struct ChipNoise {
    lfsr: u16,
    acc: f32,
    short: bool,
}

impl ChipNoise {
    pub fn new(short: bool) -> ChipNoise {
        ChipNoise { lfsr: 1, acc: 0.0, short }
    }

    /// The next sample (`clock` = LFSR steps per output sample).
    pub fn next(&mut self, clock: f32) -> f32 {
        self.acc += clock;
        while self.acc >= 1.0 {
            self.acc -= 1.0;
            let tap = if self.short { 6 } else { 1 };
            let bit = (self.lfsr ^ (self.lfsr >> tap)) & 1;
            self.lfsr = (self.lfsr >> 1) | (bit << 14);
        }
        if self.lfsr & 1 == 0 { 1.0 } else { -1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_sine_is_accurate() {
        for i in 0..1000 {
            let p = i as f32 / 997.0 - 0.3;
            let want = (p * core::f32::consts::TAU).sin();
            assert!((sin_turns(p) - want).abs() < 2e-5, "{p}");
        }
    }

    #[test]
    fn waves_are_bounded_and_centred() {
        for wave in [Wave::Sine, Wave::Saw, Wave::Square, Wave::Pulse(0.25), Wave::Triangle, Wave::ChipTriangle] {
            let mut o = Osc::new(wave, 0.0, 1);
            let n = 44_100;
            let (mut sum, mut peak) = (0f64, 0f32);
            for _ in 0..n {
                let v = o.next(220.0 / 44_100.0);
                sum += v as f64;
                peak = peak.max(v.abs());
            }
            assert!(peak <= 1.3, "{wave:?} peak {peak}");
            let mean = sum / n as f64;
            let tol = if matches!(wave, Wave::Pulse(_)) { 0.6 } else { 0.08 };
            assert!(mean.abs() < tol, "{wave:?} mean {mean}");
        }
    }
}
