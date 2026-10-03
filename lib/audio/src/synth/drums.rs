//! Synthesised drum one-shots (mono `f32` buffers): electronic kicks,
//! snares, claps, metallic hi-hats and cymbals, toms, shakers, chiptune
//! drums, noise sweeps and vinyl crackle.

use alloc::vec::Vec;

use vmath::FloatExt;

use super::filter::{Biquad, BiquadKind, Mode, OnePole, Svf};
use super::fx::soft_clip;
use super::osc::{ChipNoise, Osc, Wave, sin_turns};

fn samples(secs: f32, sr: f32) -> usize {
    (secs * sr) as usize
}

/// Kick drum parameters.
#[derive(Debug, Clone, Copy)]
pub struct Kick {
    /// Pitch at the start and the end of the sweep (Hz).
    pub start: f32,
    pub end: f32,
    /// Time constant of the pitch sweep (s).
    pub sweep: f32,
    /// Amplitude decay (s, to about -60 dB).
    pub decay: f32,
    /// Click transient level.
    pub click: f32,
    /// Saturation (1 = clean).
    pub drive: f32,
}

impl Kick {
    pub const DEEP: Kick = Kick { start: 150.0, end: 46.0, sweep: 0.045, decay: 0.55, click: 0.25, drive: 1.6 };
    pub const PUNCHY: Kick = Kick { start: 190.0, end: 52.0, sweep: 0.03, decay: 0.32, click: 0.45, drive: 2.2 };
    pub const SOFT: Kick = Kick { start: 120.0, end: 50.0, sweep: 0.04, decay: 0.35, click: 0.08, drive: 1.2 };
}

pub fn kick(p: Kick, sr: f32) -> Vec<f32> {
    let n = samples(p.decay * 1.1 + 0.02, sr);
    let mut osc_phase = 0.0f32;
    let mut noise = Osc::new(Wave::Noise, 0.0, 77);
    let mut hp = Svf::new(Mode::HighPass);
    hp.set(3000.0, 0.7, sr);
    let norm = soft_clip(p.drive);
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let f = p.end + (p.start - p.end) * FloatExt::exp(-t / p.sweep);
            osc_phase += f / sr;
            let body = sin_turns(osc_phase);
            let amp = FloatExt::exp(-t * 6.9 / p.decay) * (t * 1000.0).min(1.0);
            let click = hp.process(noise.noise()) * FloatExt::exp(-t / 0.003) * p.click;
            soft_clip((body * amp + click) * p.drive) / norm
        })
        .collect()
}

/// An electronic snare: two tuned bodies plus filtered noise.
pub fn snare(tone: f32, snappy: f32, decay: f32, sr: f32) -> Vec<f32> {
    let n = samples(decay * 1.2 + 0.02, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, 91);
    let mut hp = Svf::new(Mode::HighPass);
    hp.set(1400.0, 0.7, sr);
    let mut lp = Svf::new(Mode::LowPass);
    lp.set(9000.0, 0.7, sr);
    let (mut p1, mut p2) = (0.0f32, 0.0f32);
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            p1 += tone * (1.0 + 0.5 * FloatExt::exp(-t / 0.01)) / sr;
            p2 += tone * 1.62 / sr;
            let body = (sin_turns(p1) * 0.7 + sin_turns(p2) * 0.35) * FloatExt::exp(-t * 6.9 / (decay * 0.45));
            let nz = lp.process(hp.process(noise.noise())) * FloatExt::exp(-t * 6.9 / decay) * snappy;
            soft_clip((body * 0.9 + nz * 1.4) * 1.2) * (t * 2000.0).min(1.0)
        })
        .collect()
}

/// A hand clap: a few quick noise bursts and a short tail.
pub fn clap(sr: f32) -> Vec<f32> {
    let n = samples(0.35, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, 13);
    let mut bp = Svf::new(Mode::BandPass);
    bp.set(1250.0, 1.4, sr);
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let mut env = 0.0;
            for k in 0..3 {
                let tk = t - k as f32 * 0.011;
                if (0.0..0.011).contains(&tk) {
                    env += FloatExt::exp(-tk / 0.0035);
                }
            }
            let tail = t - 0.033;
            if tail >= 0.0 {
                env += 0.85 * FloatExt::exp(-tail / 0.06);
            }
            bp.process(noise.noise()) * env * 2.4
        })
        .collect()
}

/// A metallic hi-hat (six detuned square waves, high-passed), `decay` in
/// seconds; `bright` scales the partials.
pub fn hat(decay: f32, bright: f32, sr: f32) -> Vec<f32> {
    const RATIOS: [f32; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];
    let n = samples(decay * 1.3 + 0.01, sr);
    let mut oscs: Vec<Osc> = (0..6).map(|i| Osc::new(Wave::Square, i as f32 * 0.17, 5 + i)).collect();
    let mut noise = Osc::new(Wave::Noise, 0.0, 29);
    let mut hp = Svf::new(Mode::HighPass);
    hp.set(7200.0, 0.9, sr);
    let mut bp = Svf::new(Mode::BandPass);
    bp.set(10_500.0, 0.8, sr);
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let mut m = 0.0;
            for (o, r) in oscs.iter_mut().zip(RATIOS) {
                m += o.next(r * bright * 1.7 / sr);
            }
            let x = m * 0.18 + noise.noise() * 0.35;
            let y = bp.process(hp.process(x));
            y * FloatExt::exp(-t * 6.9 / decay) * (t * 4000.0).min(1.0) * 2.2
        })
        .collect()
}

/// A crash or ride cymbal.
pub fn cymbal(decay: f32, sr: f32) -> Vec<f32> {
    let n = samples(decay, sr);
    let mut h = hat(decay, 1.15, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, 3);
    let mut hp = Svf::new(Mode::HighPass);
    hp.set(5000.0, 0.7, sr);
    for (i, v) in h.iter_mut().enumerate().take(n) {
        let t = i as f32 / sr;
        *v = *v * 0.6 + hp.process(noise.noise()) * FloatExt::exp(-t * 6.9 / decay) * 0.5;
    }
    h
}

/// A shaker: short, soft-attack high noise.
pub fn shaker(sr: f32) -> Vec<f32> {
    let n = samples(0.12, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, 41);
    let mut hp = Svf::new(Mode::HighPass);
    hp.set(6500.0, 0.8, sr);
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let env = (t / 0.012).min(1.0) * FloatExt::exp(-(t - 0.012).max(0.0) / 0.03);
            hp.process(noise.noise()) * env
        })
        .collect()
}

/// A rim shot / side stick.
pub fn rim(sr: f32) -> Vec<f32> {
    let n = samples(0.08, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, 61);
    let mut bp = Svf::new(Mode::BandPass);
    bp.set(2400.0, 3.0, sr);
    let mut ph = 0.0f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            ph += 1750.0 / sr;
            (sin_turns(ph) * 0.6 + bp.process(noise.noise()) * 1.5) * FloatExt::exp(-t / 0.012)
        })
        .collect()
}

/// A tom at `freq` Hz.
pub fn tom(freq: f32, sr: f32) -> Vec<f32> {
    let n = samples(0.45, sr);
    let mut ph = 0.0f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            ph += freq * (1.0 + 0.6 * FloatExt::exp(-t / 0.03)) / sr;
            sin_turns(ph) * FloatExt::exp(-t * 6.9 / 0.4) * (t * 1500.0).min(1.0)
        })
        .collect()
}

/// Chiptune kick: a square sweeping down.
pub fn chip_kick(sr: f32) -> Vec<f32> {
    let n = samples(0.16, sr);
    let mut ph = 0.0f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            ph += (60.0 + 240.0 * FloatExt::exp(-t / 0.025)) / sr;
            let v = if super::osc::fract(ph) < 0.5 { 1.0 } else { -1.0 };
            v * 0.8 * FloatExt::exp(-t * 6.9 / 0.15)
        })
        .collect()
}

/// Chiptune snare or hat from LFSR noise (`clock` in steps per sample).
pub fn chip_noise(decay: f32, clock: f32, short: bool, sr: f32) -> Vec<f32> {
    let n = samples(decay, sr);
    let mut lfsr = ChipNoise::new(short);
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            // 16 volume steps like the hardware.
            let env = (FloatExt::exp(-t * 6.9 / decay) * 15.0) as i32 as f32 / 15.0;
            lfsr.next(clock) * env * 0.6
        })
        .collect()
}

/// White noise through a band-pass sweeping from `f0` to `f1` Hz over
/// `secs`, swelling in (a riser) or fading out (a downlifter).
pub fn sweep(secs: f32, f0: f32, f1: f32, swell: bool, sr: f32) -> Vec<f32> {
    let n = samples(secs, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, 17);
    let mut bp = Svf::new(Mode::BandPass);
    (0..n)
        .map(|i| {
            let x = i as f32 / n as f32;
            if i % 32 == 0 {
                bp.set(f0 * FloatExt::powf(f1 / f0, x), 1.6, sr);
            }
            let env = if swell { x * x } else { (1.0 - x) * (1.0 - x) };
            bp.process(noise.noise()) * env * 1.6
        })
        .collect()
}

/// Stereo vinyl crackle and hiss for `secs` seconds (interleaved).
pub fn crackle(secs: f32, density: f32, seed: u32, sr: f32) -> Vec<f32> {
    let n = samples(secs, sr);
    let mut noise = Osc::new(Wave::Noise, 0.0, seed);
    let mut hiss_hp = OnePole::new(3000.0, sr);
    let mut hiss_lp = Biquad::new(BiquadKind::LowPass, 9000.0, 0.7, sr);
    let mut pop_lp = OnePole::new(4000.0, sr);
    let mut out = Vec::with_capacity(n * 2);
    let mut pop = 0.0f32;
    let mut pop_side = 0.5f32;
    for _ in 0..n {
        let r = noise.noise();
        if r.abs() > 1.0 - density * 0.0004 {
            pop = noise.noise() * 0.9;
            pop_side = 0.5 + 0.5 * noise.noise();
        }
        pop *= 0.93;
        let p = pop_lp.process(pop);
        let hiss = hiss_lp.process(hiss_hp.highpass(noise.noise())) * 0.012;
        out.push(p * pop_side + hiss);
        out.push(p * (1.0 - pop_side) + hiss * 0.9);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shots_are_finite_and_bounded() {
        let sr = 44_100.0;
        let all = [
            kick(Kick::DEEP, sr),
            kick(Kick::PUNCHY, sr),
            snare(190.0, 1.0, 0.2, sr),
            clap(sr),
            hat(0.05, 1.0, sr),
            hat(0.35, 1.0, sr),
            cymbal(1.5, sr),
            shaker(sr),
            rim(sr),
            tom(120.0, sr),
            chip_kick(sr),
            chip_noise(0.1, 0.5, false, sr),
            sweep(2.0, 300.0, 8000.0, true, sr),
            crackle(1.0, 1.0, 5, sr),
        ];
        for (i, s) in all.iter().enumerate() {
            assert!(!s.is_empty());
            let peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak.is_finite() && peak > 0.01 && peak < 3.0, "sound {i}: peak {peak}");
        }
    }
}
