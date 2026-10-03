//! Instruments. Each renders one note at a time into a stereo buffer
//! (interleaved, including its release tail), which keeps offline
//! rendering simple: notes never steal voices from each other.
//!
//! * [`Analog`]: a subtractive synthesiser (up to three oscillators with
//!   unison detune and stereo spread, sub oscillator, noise, drive, a
//!   resonant filter with its own envelope, vibrato, pitch envelope) for
//!   pads, leads, basses and plucks.
//! * [`Fm`]: two-operator FM with an extra decaying "tine" modulator and
//!   tremolo — electric pianos, bells, mallets.
//! * [`Pluck`]: Karplus-Strong plucked strings.
//! * [`Chip`]: NES-style pulse/triangle voices with arpeggios.
//! * [`Kit`]: plays drum one-shots by note number.
//! * [`Bed`]: plays a precomputed stereo buffer (ambience, crackle).

use alloc::vec;
use alloc::vec::Vec;

use vmath::FloatExt;

use super::env::{Adsr, Env};
use super::filter::{Mode, Svf};
use super::fx::soft_clip;
use super::osc::{Osc, Wave, fract, sin_turns};
use super::theory::mtof;

/// Something that can play a note.
pub trait Instrument {
    /// Renders one note: `pitch` (MIDI), `vel` (0..1), held for `secs`.
    /// Returns interleaved stereo frames including the release tail.
    fn render(&self, pitch: f32, vel: f32, secs: f32, sr: f32, seed: u32) -> Vec<f32>;
}

/// Equal-power gains for a pan position in -1..1.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let a = (pan.clamp(-1.0, 1.0) + 1.0) * core::f32::consts::FRAC_PI_4;
    (FloatExt::cos(a), FloatExt::sin(a))
}

fn hash(seed: u32, i: u32) -> u32 {
    let mut x = seed ^ i.wrapping_mul(0x9E37_79B9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

fn unit(seed: u32, i: u32) -> f32 {
    (hash(seed, i) >> 8) as f32 / (1 << 24) as f32
}

/// One oscillator of an [`Analog`] patch.
#[derive(Debug, Clone, Copy)]
pub struct OscSpec {
    pub wave: Wave,
    pub level: f32,
    /// Transposition in semitones.
    pub semis: f32,
}

impl OscSpec {
    pub const fn new(wave: Wave, level: f32, semis: f32) -> OscSpec {
        OscSpec { wave, level, semis }
    }
}

/// The filter section of an [`Analog`] patch.
#[derive(Debug, Clone, Copy)]
pub struct FilterSpec {
    pub mode: Mode,
    /// Base cut-off in Hz (at middle C).
    pub cutoff: f32,
    pub q: f32,
    /// Envelope modulation depth in octaves.
    pub env_octaves: f32,
    pub env: Adsr,
    /// 0..1: how much the cut-off follows the note.
    pub keytrack: f32,
    /// Extra octaves at full velocity.
    pub vel_octaves: f32,
}

/// A subtractive synthesiser patch.
#[derive(Debug, Clone)]
pub struct Analog {
    pub oscs: Vec<OscSpec>,
    /// Voices per oscillator.
    pub unison: usize,
    /// Total detune spread of the unison voices, in cents.
    pub detune: f32,
    /// Stereo spread of the unison voices (0..1).
    pub width: f32,
    /// Sine one octave down.
    pub sub: f32,
    pub noise: f32,
    pub amp: Adsr,
    pub filter: Option<FilterSpec>,
    /// (rate Hz, depth semitones, fade-in seconds).
    pub vibrato: (f32, f32, f32),
    /// (semitones, seconds): a pitch offset that decays away.
    pub pitch_env: (f32, f32),
    pub drive: f32,
    pub gain: f32,
    /// 0 = velocity ignored, 1 = fully velocity sensitive.
    pub vel_sens: f32,
}

impl Analog {
    /// A plain patch with one oscillator.
    pub fn new(wave: Wave, amp: Adsr) -> Analog {
        Analog {
            oscs: vec![OscSpec::new(wave, 1.0, 0.0)],
            unison: 1,
            detune: 0.0,
            width: 0.0,
            sub: 0.0,
            noise: 0.0,
            amp,
            filter: None,
            vibrato: (5.0, 0.0, 0.0),
            pitch_env: (0.0, 0.01),
            drive: 1.0,
            gain: 1.0,
            vel_sens: 0.6,
        }
    }
}

impl Instrument for Analog {
    fn render(&self, pitch: f32, vel: f32, secs: f32, sr: f32, seed: u32) -> Vec<f32> {
        let total = secs + self.amp.release * 1.5 + 0.01;
        let n = (total * sr) as usize;
        let mut out = Vec::with_capacity(n * 2);
        // Oscillator bank: (osc, ratio, gain_l, gain_r).
        let mut bank = Vec::new();
        let u = self.unison.max(1);
        let norm = 1.0 / FloatExt::sqrt(u as f32);
        for (k, o) in self.oscs.iter().enumerate() {
            for v in 0..u {
                let x = if u == 1 { 0.0 } else { v as f32 / (u - 1) as f32 * 2.0 - 1.0 };
                let cents = x * self.detune * 0.5 + (unit(seed, (k * 31 + v) as u32) - 0.5) * 2.0;
                let ratio = FloatExt::exp2((o.semis + cents / 100.0) / 12.0);
                let (gl, gr) = pan_gains(x * self.width);
                let phase = if matches!(o.wave, Wave::Sine) && u == 1 {
                    0.0
                } else {
                    unit(seed, 1000 + v as u32 + k as u32 * 7)
                };
                bank.push((
                    Osc::new(o.wave, phase, hash(seed, v as u32) | 1),
                    ratio,
                    gl * o.level * norm,
                    gr * o.level * norm,
                ));
            }
        }
        let mut sub = Osc::new(Wave::Sine, 0.0, 1);
        let mut noise = Osc::new(Wave::Noise, 0.0, hash(seed, 99) | 1);
        let mut amp = Env::new(self.amp, sr);
        let mut fenv = self.filter.map(|f| Env::new(f.env, sr));
        let mut filters = [Svf::new(Mode::LowPass), Svf::new(Mode::LowPass)];
        if let Some(f) = &self.filter {
            for flt in filters.iter_mut() {
                flt.mode = f.mode;
            }
        }
        let velg = 1.0 - self.vel_sens + self.vel_sens * vel;
        let release_at = (secs * sr) as usize;
        let drive_norm = if self.drive > 1.0 { 1.0 / soft_clip(self.drive) } else { 1.0 };
        let mut fe = 0.0;
        for i in 0..n {
            let t = i as f32 / sr;
            if i == release_at {
                amp.release();
                if let Some(e) = fenv.as_mut() {
                    e.release();
                }
            }
            let (vr, vd, vf) = self.vibrato;
            let vib = if vd != 0.0 { vd * sin_turns(t * vr) * (t / vf.max(0.001)).min(1.0) } else { 0.0 };
            let pe = if self.pitch_env.0 != 0.0 {
                self.pitch_env.0 * FloatExt::exp(-t / self.pitch_env.1.max(0.001))
            } else {
                0.0
            };
            let f0 = mtof(pitch + vib + pe);
            let inc = f0 / sr;
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (o, ratio, gl, gr) in bank.iter_mut() {
                let s = o.next(inc * *ratio);
                l += s * *gl;
                r += s * *gr;
            }
            if self.sub > 0.0 {
                let s = sub.next(inc * 0.5) * self.sub;
                l += s * core::f32::consts::FRAC_1_SQRT_2;
                r += s * core::f32::consts::FRAC_1_SQRT_2;
            }
            if self.noise > 0.0 {
                let s = noise.noise() * self.noise;
                l += s * core::f32::consts::FRAC_1_SQRT_2;
                r += s * core::f32::consts::FRAC_1_SQRT_2;
            }
            if self.drive > 1.0 {
                l = soft_clip(l * self.drive) * drive_norm;
                r = soft_clip(r * self.drive) * drive_norm;
            }
            if let (Some(f), Some(e)) = (&self.filter, fenv.as_mut()) {
                fe = e.tick();
                if i % 16 == 0 {
                    let oct = f.env_octaves * fe + f.keytrack * (pitch - 60.0) / 12.0 + f.vel_octaves * vel;
                    let fc = f.cutoff * FloatExt::exp2(oct);
                    for flt in filters.iter_mut() {
                        flt.set(fc, f.q, sr);
                    }
                }
                l = filters[0].process(l);
                r = filters[1].process(r);
            }
            let a = amp.tick() * velg * self.gain;
            out.push(l * a);
            out.push(r * a);
            if amp.done() && i > release_at {
                break;
            }
        }
        let _ = fe;
        out
    }
}

/// Two-operator FM with a "tine" modulator and tremolo.
#[derive(Debug, Clone)]
pub struct Fm {
    /// Modulator frequency ratio.
    pub ratio: f32,
    /// Peak modulation index (radians).
    pub index: f32,
    /// Seconds for the index to decay towards `index_sustain x index`.
    pub index_decay: f32,
    pub index_sustain: f32,
    /// Second modulator (metallic attack): ratio and index.
    pub tine_ratio: f32,
    pub tine: f32,
    pub amp: Adsr,
    /// Extra index at full velocity (fraction of `index`).
    pub vel_index: f32,
    /// Detune between the left and right carriers (cents).
    pub detune: f32,
    /// (rate Hz, depth 0..1) auto-pan tremolo.
    pub tremolo: (f32, f32),
    pub gain: f32,
}

impl Instrument for Fm {
    fn render(&self, pitch: f32, vel: f32, secs: f32, sr: f32, seed: u32) -> Vec<f32> {
        let total = secs + self.amp.release * 1.5 + 0.01;
        let n = (total * sr) as usize;
        let mut out = Vec::with_capacity(n * 2);
        let f = mtof(pitch);
        let (dl, dr) = (FloatExt::exp2(-self.detune / 2400.0), FloatExt::exp2(self.detune / 2400.0));
        let (mut pl, mut pr, mut pm, mut pt) = (0.0f32, 0.0f32, unit(seed, 1) * 0.1, 0.0f32);
        let mut amp = Env::new(self.amp, sr);
        let release_at = (secs * sr) as usize;
        let idx_peak = self.index * (1.0 + self.vel_index * (vel - 0.5));
        let idx_coef = FloatExt::exp(-1.0 / (self.index_decay.max(0.001) * sr));
        let tine_coef = FloatExt::exp(-1.0 / (0.012 * sr));
        let mut idx = 1.0f32;
        let mut tine = 1.0f32;
        let trem_phase = unit(seed, 7);
        // Keep the brightest notes from aliasing.
        let bright = (1.0 - (pitch - 84.0).max(0.0) / 24.0).clamp(0.2, 1.0);
        let velg = 0.35 + 0.65 * vel;
        for i in 0..n {
            if i == release_at {
                amp.release();
            }
            let t = i as f32 / sr;
            let index = idx_peak * (self.index_sustain + (1.0 - self.index_sustain) * idx) * bright;
            idx *= idx_coef;
            let m = sin_turns(pm) * index;
            let tn = sin_turns(pt) * self.tine * tine * bright;
            tine *= tine_coef;
            let modulation = (m + tn) / core::f32::consts::TAU;
            let a = amp.tick() * velg * self.gain;
            let (tr, td) = self.tremolo;
            let pan = if td > 0.0 { td * sin_turns(t * tr + trem_phase) } else { 0.0 };
            let (gl, gr) = pan_gains(pan);
            out.push(sin_turns(pl + modulation) * a * gl * core::f32::consts::SQRT_2);
            out.push(sin_turns(pr + modulation) * a * gr * core::f32::consts::SQRT_2);
            pl = fract(pl + f * dl / sr);
            pr = fract(pr + f * dr / sr);
            pm = fract(pm + f * self.ratio / sr);
            pt = fract(pt + f * self.tine_ratio / sr);
            if amp.done() && i > release_at {
                break;
            }
        }
        out
    }
}

/// A Karplus-Strong plucked string.
#[derive(Debug, Clone)]
pub struct Pluck {
    /// Brightness of the excitation (0..1).
    pub brightness: f32,
    /// Seconds for a held note to decay by about 60 dB.
    pub sustain: f32,
    /// Seconds to damp after release.
    pub release: f32,
    pub gain: f32,
    /// Stereo spread between two slightly detuned strings (0..1).
    pub width: f32,
}

impl Pluck {
    fn string(&self, f: f32, vel: f32, secs: f32, sr: f32, seed: u32, total: usize) -> Vec<f32> {
        let period = sr / f.max(20.0);
        // Loop delay: n samples of buffer + 0.5 (averaging) + d (all-pass).
        let n = ((period - 0.5) as usize).max(2);
        let d = (period - 0.5 - n as f32).clamp(0.05, 1.0);
        let c = (1.0 - d) / (1.0 + d);
        let mut buf = vec![0.0f32; n];
        let mut noise = Osc::new(Wave::Noise, 0.0, seed | 1);
        let mut lp = 0.0f32;
        let a = 0.15 + 0.8 * self.brightness * (0.6 + 0.4 * vel);
        for v in buf.iter_mut() {
            lp += a * (noise.noise() - lp);
            *v = lp;
        }
        // Remove DC from the excitation.
        let mean = buf.iter().sum::<f32>() / n as f32;
        buf.iter_mut().for_each(|v| *v -= mean);
        let g_hold = FloatExt::powf(10.0f32, -3.0 * period / (self.sustain.max(0.05) * sr));
        let g_rel = FloatExt::powf(10.0f32, -3.0 * period / (self.release.max(0.01) * sr));
        let release_at = (secs * sr) as usize;
        let mut out = Vec::with_capacity(total);
        let mut pos = 0usize;
        let (mut x1, mut y1) = (0.0f32, 0.0f32);
        for i in 0..total {
            let g = if i < release_at { g_hold } else { g_rel };
            let cur = buf[pos];
            let next = buf[if pos + 1 == n { 0 } else { pos + 1 }];
            let avg = 0.5 * (cur + next) * g;
            let ap = c * avg + x1 - c * y1;
            x1 = avg;
            y1 = ap;
            buf[pos] = ap;
            pos = if pos + 1 == n { 0 } else { pos + 1 };
            out.push(cur);
        }
        out
    }
}

impl Instrument for Pluck {
    fn render(&self, pitch: f32, vel: f32, secs: f32, sr: f32, seed: u32) -> Vec<f32> {
        let total = ((secs + self.release) * sr) as usize + 64;
        let f = mtof(pitch);
        let a = self.string(f * FloatExt::exp2(-1.5 / 1200.0), vel, secs, sr, seed, total);
        let b = self.string(f * FloatExt::exp2(1.5 / 1200.0), vel, secs, sr, hash(seed, 3), total);
        let g = self.gain * (0.4 + 0.6 * vel);
        let w = self.width.clamp(0.0, 1.0);
        let mut out = Vec::with_capacity(total * 2);
        for (x, y) in a.iter().zip(&b) {
            let m = (x + y) * 0.5;
            out.push((m * (1.0 - w) + x * w) * g);
            out.push((m * (1.0 - w) + y * w) * g);
        }
        // Fade the very end to avoid a click when the tail is cut.
        let frames = out.len() / 2;
        let fade = (sr * 0.01) as usize;
        for k in 0..fade.min(frames) {
            let gk = k as f32 / fade as f32;
            let i = frames - 1 - k;
            out[i * 2] *= gk;
            out[i * 2 + 1] *= gk;
        }
        out
    }
}

/// An NES-style voice.
#[derive(Debug, Clone)]
pub struct Chip {
    pub wave: Wave,
    pub amp: Adsr,
    /// Semitone offsets cycled at `arp_hz` (empty = none).
    pub arp: Vec<f32>,
    pub arp_hz: f32,
    /// (rate Hz, depth semitones, delay seconds).
    pub vibrato: (f32, f32, f32),
    /// Initial pitch offset in semitones that slides to 0 over `slide_secs`.
    pub slide: f32,
    pub slide_secs: f32,
    pub pan: f32,
    pub gain: f32,
}

impl Instrument for Chip {
    fn render(&self, pitch: f32, vel: f32, secs: f32, sr: f32, seed: u32) -> Vec<f32> {
        let total = secs + self.amp.release * 1.5 + 0.005;
        let n = (total * sr) as usize;
        let mut out = Vec::with_capacity(n * 2);
        let mut osc = Osc::new(self.wave, 0.0, seed | 1);
        let mut amp = Env::new(self.amp, sr);
        let release_at = (secs * sr) as usize;
        let (gl, gr) = pan_gains(self.pan);
        let velg = 0.5 + 0.5 * vel;
        for i in 0..n {
            if i == release_at {
                amp.release();
            }
            let t = i as f32 / sr;
            let mut p = pitch;
            if !self.arp.is_empty() {
                let k = (t * self.arp_hz) as usize % self.arp.len();
                p += self.arp[k];
            }
            let (vr, vd, vdel) = self.vibrato;
            if vd != 0.0 && t > vdel {
                p += vd * sin_turns((t - vdel) * vr);
            }
            if self.slide != 0.0 && t < self.slide_secs {
                p += self.slide * (1.0 - t / self.slide_secs);
            }
            let s = osc.next(mtof(p) / sr);
            // 16 volume steps.
            let level = ((amp.tick() * velg * 15.0) as i32) as f32 / 15.0;
            let v = s * level * self.gain;
            out.push(v * gl);
            out.push(v * gr);
            if amp.done() && i > release_at {
                break;
            }
        }
        out
    }
}

/// One drum sound of a [`Kit`].
#[derive(Debug, Clone)]
pub struct DrumSound {
    /// MIDI note that triggers it.
    pub key: i32,
    pub data: Vec<f32>,
    pub pan: f32,
    pub gain: f32,
}

/// Plays one-shots by note number (velocity scales the level).
#[derive(Debug, Clone, Default)]
pub struct Kit {
    pub sounds: Vec<DrumSound>,
}

impl Kit {
    pub fn add(&mut self, key: i32, data: Vec<f32>, pan: f32, gain: f32) {
        self.sounds.push(DrumSound { key, data, pan, gain });
    }
}

impl Instrument for Kit {
    fn render(&self, pitch: f32, vel: f32, _secs: f32, _sr: f32, _seed: u32) -> Vec<f32> {
        let key = FloatExt::round(pitch) as i32;
        let Some(s) = self.sounds.iter().find(|s| s.key == key) else { return Vec::new() };
        let (gl, gr) = pan_gains(s.pan);
        let g = s.gain * vel * vel;
        let mut out = Vec::with_capacity(s.data.len() * 2);
        for &v in &s.data {
            out.push(v * g * gl * core::f32::consts::SQRT_2);
            out.push(v * g * gr * core::f32::consts::SQRT_2);
        }
        out
    }
}

/// Plays a precomputed stereo buffer from its start for the note's length.
#[derive(Debug, Clone)]
pub struct Bed {
    pub data: Vec<f32>,
    pub gain: f32,
}

impl Instrument for Bed {
    fn render(&self, _pitch: f32, vel: f32, secs: f32, sr: f32, _seed: u32) -> Vec<f32> {
        let frames = ((secs * sr) as usize).min(self.data.len() / 2);
        let fade = (sr * 0.5) as usize;
        let mut out = self.data[..frames * 2].to_vec();
        for k in 0..frames {
            let g = self.gain * vel * (k as f32 / fade as f32).min(1.0) * ((frames - k) as f32 / fade as f32).min(1.0);
            out[k * 2] *= g;
            out[k * 2 + 1] *= g;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pitch by autocorrelation (with parabolic refinement) of the left
    /// channel after the attack.
    fn freq_of(buf: &[f32], sr: f32) -> f32 {
        let l: Vec<f32> = buf.chunks(2).map(|f| f[0]).collect();
        let s = &l[l.len() / 4..l.len() / 4 + 4096];
        let ac = |lag: usize| -> f32 { s[..s.len() - lag].iter().zip(&s[lag..]).map(|(a, b)| a * b).sum() };
        let (lo, hi) = ((sr / 1000.0) as usize, (sr / 60.0) as usize);
        let best = (lo..hi).max_by(|&a, &b| ac(a).partial_cmp(&ac(b)).unwrap()).unwrap();
        let (a, b, c) = (ac(best - 1), ac(best), ac(best + 1));
        let p = 0.5 * (a - c) / (a - 2.0 * b + c);
        sr / (best as f32 + p)
    }

    #[test]
    fn instruments_play_in_tune() {
        let sr = 44_100.0;
        let a = Analog::new(Wave::Saw, Adsr::new(0.01, 0.1, 0.8, 0.1));
        let f = freq_of(&a.render(57.0, 1.0, 1.0, sr, 1), sr);
        assert!((f - 220.0).abs() < 3.0, "analog {f}");
        let fm = Fm {
            ratio: 1.0,
            index: 1.0,
            index_decay: 0.5,
            index_sustain: 0.2,
            tine_ratio: 14.0,
            tine: 0.0,
            amp: Adsr::new(0.002, 1.0, 0.5, 0.3),
            vel_index: 0.0,
            detune: 0.0,
            tremolo: (0.0, 0.0),
            gain: 1.0,
        };
        let f = freq_of(&fm.render(57.0, 1.0, 1.0, sr, 1), sr);
        assert!((f - 220.0).abs() < 3.0, "fm {f}");
        let p = Pluck { brightness: 0.5, sustain: 2.0, release: 0.2, gain: 1.0, width: 0.0 };
        let f = freq_of(&p.render(57.0, 1.0, 1.0, sr, 1), sr);
        assert!((f - 220.0).abs() < 4.0, "pluck {f}");
    }

    #[test]
    fn notes_end_after_release() {
        let sr = 44_100.0;
        let a = Analog::new(Wave::Square, Adsr::new(0.01, 0.1, 0.8, 0.2));
        let buf = a.render(60.0, 0.8, 0.5, sr, 3);
        let frames = buf.len() / 2;
        assert!(frames < (sr * 1.0) as usize);
        let tail = &buf[buf.len() - 200..];
        assert!(tail.iter().all(|v| v.abs() < 0.01));
    }
}
