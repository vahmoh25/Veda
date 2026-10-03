//! Envelopes.
//!
//! [`Adsr`] describes an attack/decay/sustain/release shape; [`Env`] runs
//! it sample by sample with exponential decay and release segments (which
//! sound natural) and a linear attack. Because notes are rendered offline
//! with known lengths, the release starts at a fixed sample.

use vmath::FloatExt;

/// Envelope shape (times in seconds, sustain as a level 0..1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Adsr {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl Adsr {
    pub const fn new(attack: f32, decay: f32, sustain: f32, release: f32) -> Adsr {
        Adsr { attack, decay, sustain, release }
    }

    /// A percussive shape: instant attack, exponential decay to silence.
    pub const fn pluck(decay: f32, release: f32) -> Adsr {
        Adsr { attack: 0.002, decay, sustain: 0.0, release }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Attack,
    Decay,
    Sustain,
    Release,
    Done,
}

/// A running envelope.
#[derive(Debug, Clone)]
pub struct Env {
    shape: Adsr,
    stage: Stage,
    level: f32,
    attack_step: f32,
    decay_coef: f32,
    release_coef: f32,
}

/// Per-sample multiplier that decays by about 60 dB over `secs`.
fn coef(secs: f32, sr: f32) -> f32 {
    FloatExt::exp(-6.9 / (secs.max(0.0005) * sr))
}

impl Env {
    pub fn new(shape: Adsr, sr: f32) -> Env {
        Env {
            shape,
            stage: Stage::Attack,
            level: 0.0,
            attack_step: 1.0 / (shape.attack.max(0.0005) * sr),
            decay_coef: coef(shape.decay, sr),
            release_coef: coef(shape.release, sr),
        }
    }

    /// Starts the release (note off).
    pub fn release(&mut self) {
        if self.stage != Stage::Done {
            self.stage = Stage::Release;
        }
    }

    pub fn done(&self) -> bool {
        self.stage == Stage::Done
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    #[inline(always)]
    pub fn tick(&mut self) -> f32 {
        match self.stage {
            Stage::Attack => {
                self.level += self.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                let s = self.shape.sustain;
                self.level = s + (self.level - s) * self.decay_coef;
                if self.level - s < 1e-4 {
                    self.level = s;
                    self.stage = if s <= 1e-4 { Stage::Done } else { Stage::Sustain };
                }
            }
            Stage::Sustain => {}
            Stage::Release => {
                self.level *= self.release_coef;
                if self.level < 1e-4 {
                    self.level = 0.0;
                    self.stage = Stage::Done;
                }
            }
            Stage::Done => self.level = 0.0,
        }
        self.level
    }
}

/// A simple exponential decay from 1 (for pitch and click envelopes).
#[derive(Debug, Clone)]
pub struct Decay {
    level: f32,
    coef: f32,
}

impl Decay {
    /// Falls to about -60 dB in `secs`.
    pub fn new(secs: f32, sr: f32) -> Decay {
        Decay { level: 1.0, coef: coef(secs, sr) }
    }

    #[inline(always)]
    pub fn tick(&mut self) -> f32 {
        let v = self.level;
        self.level *= self.coef;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adsr_shape() {
        let sr = 1000.0;
        let mut e = Env::new(Adsr::new(0.01, 0.1, 0.5, 0.2), sr);
        let v: alloc::vec::Vec<f32> = (0..200).map(|_| e.tick()).collect();
        assert!((v[9] - 1.0).abs() < 0.05);
        assert!((v[150] - 0.5).abs() < 0.01);
        e.release();
        let mut n = 0;
        while !e.done() && n < 10_000 {
            e.tick();
            n += 1;
        }
        assert!((150..260).contains(&n), "release took {n} samples");
    }
}
