//! Signal levels for meters and animations.
//!
//! [`rms_dbfs`] and [`peak_dbfs`] measure a block of 16-bit samples
//! (relative to full scale: a full-scale square wave is 0 dBFS RMS).
//! [`LevelMeter`] smooths the RMS level with meter ballistics and holds the
//! peak for a moment, which suits a circle that pulses with a voice: the
//! level rises like an RMS detector (smoothing the mean square, so a syllable
//! shows within a block or two) and falls evenly on a decibel scale, so the
//! circle shrinks between syllables instead of lingering. The sums are
//! integer arithmetic; a few transcendental functions per block.

use vmath::FloatExt;

/// The level reported for silence, in dBFS.
pub const SILENCE_DB: f32 = -120.0;

/// Mean square and peak magnitude of a block (`0.0`, `0` when empty).
fn measure(samples: &[i16]) -> (f32, u32) {
    let (mut sum, mut peak) = (0u64, 0u32);
    for &s in samples {
        let v = (s as i32).unsigned_abs();
        sum += (v * v) as u64;
        peak = peak.max(v);
    }
    let mean = if samples.is_empty() { 0.0 } else { sum as f64 / samples.len() as f64 };
    ((mean / (32768.0 * 32768.0)) as f32, peak)
}

/// Power ratio to dB, with silence at [`SILENCE_DB`].
fn power_db(p: f32) -> f32 {
    if p > 1e-12 { (10.0 * FloatExt::log10(p)).max(SILENCE_DB) } else { SILENCE_DB }
}

/// dB to a power ratio, with [`SILENCE_DB`] and below as `0.0`.
fn db_power(db: f32) -> f32 {
    if db > SILENCE_DB { FloatExt::powf(10.0, db / 10.0) } else { 0.0 }
}

/// `x` limited to `lo..=hi`, with NaN as `lo`.
fn limit(x: f32, lo: f32, hi: f32) -> f32 {
    if x.is_nan() { lo } else { x.clamp(lo, hi) }
}

/// The one-pole smoothing coefficient for a step of `dt` seconds with time
/// constant `tau` (a step response for `tau == 0`).
fn smoothing(dt: f32, tau: f32) -> f32 {
    if tau > 0.0 { 1.0 - FloatExt::exp(-dt / tau) } else { 1.0 }
}

/// The RMS level of `samples` in dBFS ([`SILENCE_DB`] for silence or an
/// empty block).
pub fn rms_dbfs(samples: &[i16]) -> f32 {
    power_db(measure(samples).0)
}

/// The peak level of `samples` in dBFS ([`SILENCE_DB`] for silence or an
/// empty block).
pub fn peak_dbfs(samples: &[i16]) -> f32 {
    let p = measure(samples).1 as f32 / 32768.0;
    power_db(p * p)
}

/// A level meter with attack, release and peak hold (see the module
/// documentation).
///
/// ```
/// use vaudio::level::LevelMeter;
///
/// let mut meter = LevelMeter::new(16_000);
/// meter.process(&[8_000i16; 160]);
/// assert!(meter.level_db() > -20.0 && meter.level_db() < -12.0);
/// assert!(meter.normalized() > 0.5);
/// ```
#[derive(Debug, Clone)]
pub struct LevelMeter {
    rate: u32,
    /// Smoothed RMS level and held peak in dBFS.
    level_db: f32,
    peak_db: f32,
    /// Seconds since the peak was last raised.
    peak_age: f32,
    attack_s: f32,
    release_s: f32,
    hold_s: f32,
    floor_db: f32,
}

impl LevelMeter {
    /// A meter for `rate` Hz with a 10 ms attack, a 300 ms release and a 1 s
    /// peak hold, mapping -60..0 dBFS to 0..1.
    pub fn new(rate: u32) -> LevelMeter {
        LevelMeter {
            rate: rate.max(1),
            level_db: SILENCE_DB,
            peak_db: SILENCE_DB,
            peak_age: 0.0,
            attack_s: 0.01,
            release_s: 0.3,
            hold_s: 1.0,
            floor_db: -60.0,
        }
    }

    /// Sets the attack time constant (of the mean square) and the release
    /// time constant (of the level in dB), in seconds; 0 follows each block.
    pub fn set_ballistics(&mut self, attack_s: f32, release_s: f32) {
        self.attack_s = limit(attack_s, 0.0, 10.0);
        self.release_s = limit(release_s, 0.0, 10.0);
    }

    /// Sets the level that [`normalized`](LevelMeter::normalized) maps to 0
    /// (default -60 dBFS; 0 dBFS maps to 1). In silence the level settles
    /// 20 dB below it.
    pub fn set_floor_db(&mut self, floor_db: f32) {
        self.floor_db = limit(floor_db, SILENCE_DB, -1.0);
    }

    /// Back to silence.
    pub fn reset(&mut self) {
        self.level_db = SILENCE_DB;
        self.peak_db = SILENCE_DB;
        self.peak_age = 0.0;
    }

    /// Adds a block of samples and returns the smoothed level in dBFS.
    pub fn process(&mut self, samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return self.level_db;
        }
        let (power, peak) = measure(samples);
        let dt = samples.len() as f32 / self.rate as f32;
        let block_db = power_db(power);
        if block_db > self.level_db {
            // Attack: smooth the mean square, like an RMS detector.
            let current = db_power(self.level_db);
            self.level_db = power_db(current + smoothing(dt, self.attack_s) * (power - current));
        } else {
            // Release: fall in dB towards the block's level, but no further
            // than 20 dB below the display floor, so that the level leaves
            // the display within a few time constants whatever the noise.
            let target = block_db.max((self.floor_db - 20.0).max(SILENCE_DB)).min(self.level_db);
            self.level_db += smoothing(dt, self.release_s) * (target - self.level_db);
        }
        let p = peak as f32 / 32768.0;
        let p_db = power_db(p * p);
        if p_db >= self.peak_db {
            self.peak_db = p_db;
            self.peak_age = 0.0;
        } else {
            self.peak_age += dt;
            if self.peak_age > self.hold_s {
                // Fall at 20 dB per second after the hold.
                self.peak_db = (self.peak_db - 20.0 * dt).max(p_db);
            }
        }
        self.level_db
    }

    /// The smoothed RMS level in dBFS.
    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    /// The held peak level in dBFS.
    pub fn peak_db(&self) -> f32 {
        self.peak_db
    }

    /// The smoothed level mapped linearly in dB to 0..=1 (the floor to
    /// 0 dBFS), for animations.
    pub fn normalized(&self) -> f32 {
        ((self.level_db() - self.floor_db) / -self.floor_db).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn sine(amplitude: f32, n: usize) -> Vec<i16> {
        (0..n).map(|i| (amplitude * 32767.0 * (i as f32 * 0.3).sin()) as i16).collect()
    }

    #[test]
    fn levels_of_known_signals() {
        assert_eq!(rms_dbfs(&[]), SILENCE_DB);
        assert_eq!(rms_dbfs(&[0; 100]), SILENCE_DB);
        assert_eq!(peak_dbfs(&[0; 100]), SILENCE_DB);
        // A full-scale square wave: 0 dBFS RMS and peak.
        let square: Vec<i16> = (0..1000).map(|i| if i % 2 == 0 { -32768 } else { 32767 }).collect();
        assert!(rms_dbfs(&square).abs() < 0.01 && peak_dbfs(&square).abs() < 0.01);
        // A sine at half scale: -9 dBFS RMS (-6 dB peak, -3 dB crest).
        let s = sine(0.5, 16_000);
        assert!((rms_dbfs(&s) + 9.03).abs() < 0.1, "{}", rms_dbfs(&s));
        assert!((peak_dbfs(&s) + 6.02).abs() < 0.1, "{}", peak_dbfs(&s));
    }

    #[test]
    fn meter_attacks_fast_and_releases_slowly() {
        let mut m = LevelMeter::new(16_000);
        assert_eq!(m.level_db(), SILENCE_DB);
        assert_eq!(m.normalized(), 0.0);
        let loud = sine(0.5, 160);
        // 80 ms of tone: within 1 dB of its level (10 ms attack).
        for _ in 0..8 {
            m.process(&loud);
        }
        assert!((m.level_db() + 9.0).abs() < 1.0, "{}", m.level_db());
        assert!((m.normalized() - 0.85).abs() < 0.03, "{}", m.normalized());
        // 100 ms of silence: still on display (300 ms release) ...
        for _ in 0..10 {
            m.process(&[0; 160]);
        }
        assert!(m.level_db() > -40.0 && m.normalized() > 0.3, "{}", m.level_db());
        // ... and the peak is held.
        assert!((m.peak_db() + 6.0).abs() < 0.2);
        // One more second: off the display, the peak falling.
        for _ in 0..100 {
            m.process(&[0; 160]);
        }
        assert!(m.level_db() < -60.0 && m.normalized() == 0.0, "{}", m.level_db());
        assert!(m.peak_db() < -6.5);
        assert!(m.process(&[]).is_finite());
        m.reset();
        assert_eq!(m.level_db(), SILENCE_DB);
        // Odd settings are tamed: an instant attack here.
        m.set_ballistics(f32::NAN, f32::INFINITY);
        m.set_floor_db(f32::NAN);
        m.process(&loud);
        assert!((m.level_db() + 9.0).abs() < 1.0 && m.normalized().is_finite(), "{}", m.level_db());
    }
}
