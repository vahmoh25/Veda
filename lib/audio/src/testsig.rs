//! Synthetic signals for the echo canceller and voice activity tests.
//!
//! Everything is deterministic (seeded) and needs no files:
//!
//! * [`speech`]: a speech-like voice — glottal pulses with a moving pitch,
//!   shaped by three gliding formant resonators, fricative noise bursts,
//!   syllable envelopes grouped into utterances — with the ground truth of
//!   where the utterances are.
//! * [`white`], [`pink`] and [`fan`]: background noises.
//! * [`room`]: a decaying random room impulse response, and [`convolve`].
//! * Measurements: [`power`], [`db`], [`correlation`].

use alloc::vec;
use alloc::vec::Vec;
use core::f32::consts::{PI, TAU};

use vmath::Rng;

use crate::fft::RealFft;

/// The character of a synthetic voice.
#[derive(Debug, Clone, Copy)]
pub struct Voice {
    /// Mean pitch in Hz.
    pub f0: f32,
    /// Pitch excursion as a fraction of `f0`.
    pub f0_range: f32,
    /// Scales the formant frequencies (vocal tract length).
    pub formants: f32,
    /// RMS level of the active parts (1.0 = full scale).
    pub level: f32,
}

/// A low voice.
pub const MALE: Voice = Voice { f0: 110.0, f0_range: 0.25, formants: 1.0, level: 0.1 };
/// A high voice.
pub const FEMALE: Voice = Voice { f0: 205.0, f0_range: 0.3, formants: 1.17, level: 0.1 };

/// How speech is laid out in time.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// Silence before the first utterance, in seconds.
    pub lead_in: f32,
    /// Syllables per utterance (inclusive range).
    pub syllables: (u32, u32),
    /// Pause between utterances in seconds.
    pub pause: (f32, f32),
}

/// Talking with natural pauses, like a person in a conversation.
pub const CONVERSATION: Pacing = Pacing { lead_in: 0.5, syllables: (4, 12), pause: (0.6, 1.6) };
/// Talking almost without pauses, like an assistant reading a long answer.
pub const MONOLOGUE: Pacing = Pacing { lead_in: 0.0, syllables: (8, 20), pause: (0.15, 0.35) };

/// Synthesised speech and its ground truth.
#[derive(Debug, Clone)]
pub struct Speech {
    pub samples: Vec<f32>,
    /// Per sample: inside an utterance (from its first syllable's start to
    /// its last syllable's end).
    pub active: Vec<bool>,
}

/// Vowel formants (F1, F2, F3) in Hz, after Peterson and Barney.
const VOWELS: [(f32, f32, f32); 7] = [
    (730.0, 1090.0, 2440.0), // a
    (270.0, 2290.0, 3010.0), // i
    (300.0, 870.0, 2240.0),  // u
    (530.0, 1840.0, 2480.0), // e
    (570.0, 840.0, 2410.0),  // o
    (660.0, 1720.0, 2410.0), // ae
    (490.0, 1350.0, 1690.0), // er
];

struct Syllable {
    start: usize,
    len: usize,
    vowel: usize,
    amp: f32,
    /// Pitch offset (fraction of f0) for this syllable.
    accent: f32,
    /// Fricative onset: length in samples and centre frequency.
    fricative: Option<(usize, f32)>,
}

/// A two-pole resonator with unit gain at DC (as in a cascade formant
/// synthesiser: the formant peaks of a cascade multiply).
#[derive(Default, Clone, Copy)]
struct Resonator {
    a1: f32,
    a2: f32,
    g: f32,
    y1: f32,
    y2: f32,
}

impl Resonator {
    fn tune(&mut self, freq: f32, bw: f32, rate: f32) {
        let r = (-PI * bw / rate).exp();
        let th = TAU * freq.min(rate * 0.45) / rate;
        self.a1 = 2.0 * r * th.cos();
        self.a2 = -r * r;
        self.g = 1.0 - self.a1 - self.a2;
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.g * x + self.a1 * self.y1 + self.a2 * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Glottal flow (Rosenberg pulse) at phase `p` in [0, 1).
fn glottal(p: f32) -> f32 {
    const OPEN: f32 = 0.55;
    const CLOSE: f32 = 0.18;
    if p < OPEN {
        0.5 * (1.0 - (PI * p / OPEN).cos())
    } else if p < OPEN + CLOSE {
        (0.5 * PI * (p - OPEN) / CLOSE).cos()
    } else {
        0.0
    }
}

/// Synthesises `seconds` of speech-like audio at `rate`.
pub fn speech(rate: u32, seconds: f32, voice: Voice, pacing: Pacing, seed: u64) -> Speech {
    let fs = rate as f32;
    let n = (seconds * fs) as usize;
    let mut rng = Rng::new(seed);
    // Lay out the syllables.
    let mut syllables: Vec<Syllable> = Vec::new();
    let mut utterances: Vec<(usize, usize)> = Vec::new();
    let mut t = pacing.lead_in;
    while t < seconds {
        let count = rng.range_i32(pacing.syllables.0 as i32, pacing.syllables.1 as i32 + 1);
        let first = (t * fs) as usize;
        let mut end = first;
        for i in 0..count {
            let dur = rng.range_f32(0.12, 0.3);
            if t + dur > seconds {
                break;
            }
            let fricative = (rng.next_f32() < 0.3).then(|| {
                ((rng.range_f32(0.04, 0.09) * fs) as usize, if rng.next_f32() < 0.6 { 4800.0 } else { 2600.0 })
            });
            let accent = if i == 0 || rng.next_f32() < 0.25 { rng.range_f32(0.1, 0.35) } else { 0.0 };
            syllables.push(Syllable {
                start: (t * fs) as usize,
                len: (dur * fs) as usize,
                vowel: rng.below_usize(VOWELS.len()),
                amp: rng.range_f32(0.5, 1.0),
                accent,
                fricative,
            });
            end = ((t + dur) * fs) as usize;
            // Syllables within a word touch or leave short gaps.
            t += dur + rng.range_f32(-0.02, 0.06).max(0.0);
        }
        if end > first {
            utterances.push((first, end.min(n)));
        }
        t += rng.range_f32(pacing.pause.0, pacing.pause.1);
    }
    let mut out = vec![0.0f32; n];
    let mut active = vec![false; n];
    for &(a, b) in &utterances {
        active[a..b].fill(true);
    }
    // Render.
    let mut formant = [Resonator::default(); 3];
    let mut fric = Resonator::default();
    let mut f = [VOWELS[0].0, VOWELS[0].1, VOWELS[0].2];
    let mut phase = 0.0f32;
    let mut prev_flow = 0.0f32;
    let glide = 1.0 - (-1.0 / (0.03 * fs)).exp();
    let mut si = 0;
    // Period-to-period jitter of the pitch (about 1 %), as in real voices.
    let mut jitter = 0.0f32;
    for (i, o) in out.iter_mut().enumerate() {
        while si + 1 < syllables.len() && syllables[si + 1].start <= i {
            si += 1;
        }
        let Some(s) = syllables.get(si) else { break };
        let pos = i as isize - s.start as isize;
        let target = VOWELS[s.vowel];
        let target = [target.0 * voice.formants, target.1 * voice.formants, target.2 * voice.formants];
        for (fv, tv) in f.iter_mut().zip(target) {
            *fv += glide * (tv - *fv);
        }
        if i % 16 == 0 {
            for (k, r) in formant.iter_mut().enumerate() {
                r.tune(f[k], 60.0 + 40.0 * k as f32, fs);
            }
        }
        // Pitch: syllable accent, a slow wobble and some jitter.
        let tt = i as f32 / fs;
        let wobble = 0.04 * (TAU * 4.7 * tt).sin() + 0.02 * (TAU * 1.3 * tt).sin();
        let f0 = voice.f0 * (1.0 + voice.f0_range * (s.accent + wobble - 0.1)) * (1.0 + jitter);
        phase += f0 / fs;
        if phase >= 1.0 {
            phase -= 1.0;
            jitter = rng.gaussian(0.0, 0.01);
        }
        let flow = glottal(phase);
        // Glottal flow derivative plus breath noise while the glottis is open.
        let source = flow - prev_flow + 0.004 * flow * rng.gaussian(0.0, 1.0);
        prev_flow = flow;
        // Envelope: raised-cosine attack and release.
        let mut env = 0.0;
        let mut noise = 0.0;
        if pos >= 0 && (pos as usize) < s.len {
            let p = pos as usize;
            let (att, rel) = ((0.03 * fs) as usize, (0.05 * fs) as usize);
            env = if p < att {
                0.5 - 0.5 * (PI * p as f32 / att as f32).cos()
            } else if p + rel > s.len {
                0.5 - 0.5 * (PI * (s.len - p) as f32 / rel as f32).cos()
            } else {
                1.0 - 0.3 * (p - att) as f32 / s.len as f32
            };
            env *= s.amp;
            if let Some((flen, fc)) = s.fricative
                && p < flen
            {
                if p == 0 {
                    fric.tune(fc, 1500.0, fs);
                }
                let fe = (p as f32 / flen as f32 * PI).sin();
                noise = fric.run(rng.gaussian(0.0, 1.0)) * fe * 0.35;
                // The vowel starts as the fricative fades.
                env *= p as f32 / flen as f32;
            }
        }
        let mut v = source * 40.0;
        for r in formant.iter_mut() {
            v = r.run(v);
        }
        *o = v * env + noise * s.amp;
    }
    // Scale to the requested level over the active parts.
    let (mut e, mut c) = (0.0f64, 0usize);
    for (x, &a) in out.iter().zip(&active) {
        if a {
            e += (*x as f64) * (*x as f64);
            c += 1;
        }
    }
    let rms = (e / c.max(1) as f64).sqrt() as f32;
    if rms > 0.0 {
        let g = voice.level / rms;
        for x in &mut out {
            *x *= g;
        }
    }
    Speech { samples: out, active }
}

/// Gaussian white noise with the given RMS level.
pub fn white(n: usize, level: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| rng.gaussian(0.0, level)).collect()
}

/// Pink (1/f) noise with the given RMS level (Paul Kellet's filter).
pub fn pink(n: usize, level: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut b = [0.0f32; 7];
    let mut out: Vec<f32> = (0..n)
        .map(|_| {
            let w = rng.gaussian(0.0, 1.0);
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.153_852;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.0168980;
            let y = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
            b[6] = w * 0.115926;
            y
        })
        .collect();
    normalize(&mut out, level);
    out
}

/// Fan-like noise: rumbling low-passed noise, a blade hum with harmonics
/// and a slow, slight swell, at the given RMS level.
pub fn fan(rate: u32, n: usize, level: f32, seed: u64) -> Vec<f32> {
    let fs = rate as f32;
    let mut rng = Rng::new(seed);
    let c1 = 1.0 - (-TAU * 600.0 / fs).exp();
    let c2 = 1.0 - (-TAU * 3000.0 / fs).exp();
    let (mut l1, mut l2) = (0.0f32, 0.0f32);
    let mut out: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / fs;
            let w = rng.gaussian(0.0, 1.0);
            l1 += c1 * (w - l1);
            l2 += c2 * (w - l2);
            let hum =
                0.6 * (TAU * 118.0 * t).sin() + 0.3 * (TAU * 236.0 * t + 0.4).sin() + 0.15 * (TAU * 354.0 * t).sin();
            let swell = 1.0 + 0.12 * (TAU * 0.7 * t).sin();
            (3.0 * l1 + 0.6 * l2 + 0.05 * hum) * swell
        })
        .collect();
    normalize(&mut out, level);
    out
}

fn normalize(x: &mut [f32], level: f32) {
    let rms = power(x).sqrt() as f32;
    if rms > 0.0 {
        let g = level / rms;
        for v in x {
            *v *= g;
        }
    }
}

/// A room impulse response: the direct path (amplitude 1 at index 0), a few
/// early reflections and an exponentially decaying diffuse tail reaching
/// -60 dB after `rt60` seconds, `length` seconds long, band-limited like a
/// small loudspeaker (about 150 Hz to 6 kHz).
pub fn room(rate: u32, rt60: f32, length: f32, seed: u64) -> Vec<f32> {
    let fs = rate as f32;
    let n = ((length * fs) as usize).max(16);
    let mut rng = Rng::new(seed);
    let mut h = vec![0.0f32; n];
    h[0] = 1.0;
    for _ in 0..6 {
        let at = (rng.range_f32(0.0015, 0.015) * fs) as usize;
        if at < n {
            h[at] += rng.range_f32(0.15, 0.4) * if rng.next_f32() < 0.5 { -1.0 } else { 1.0 };
        }
    }
    // Diffuse tail with about half the energy of the direct path.
    let decay = 6.91 / (rt60 * fs);
    let amp = (0.5 * 2.0 * decay).sqrt();
    let start = (0.003 * fs) as usize;
    for (i, v) in h.iter_mut().enumerate().skip(start) {
        *v += rng.gaussian(0.0, amp) * (-decay * (i - start) as f32).exp();
    }
    // Loudspeaker band: one-pole high-pass and low-pass.
    let hp = (-TAU * 150.0 / fs).exp();
    let lp = 1.0 - (-TAU * 6000.0 / fs).exp();
    let (mut x1, mut y1, mut l) = (0.0f32, 0.0f32, 0.0f32);
    for v in h.iter_mut() {
        let y = hp * (y1 + *v - x1);
        x1 = *v;
        y1 = y;
        l += lp * (y - l);
        *v = l;
    }
    h
}

/// Linear convolution of `x` with `h`, truncated to `x.len()` samples.
pub fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    let l = h.len().max(x.len().min(4096)).next_power_of_two();
    let n = 2 * l;
    let mut fft = RealFft::new(n);
    let (mut hr, mut hi) = (vec![0.0; l + 1], vec![0.0; l + 1]);
    let mut buf = vec![0.0; n];
    buf[..h.len()].copy_from_slice(h);
    fft.forward(&buf, &mut hr, &mut hi);
    let (mut xr, mut xi) = (vec![0.0; l + 1], vec![0.0; l + 1]);
    let mut out = vec![0.0f32; x.len() + n];
    for (b, chunk) in x.chunks(l).enumerate() {
        buf.fill(0.0);
        buf[..chunk.len()].copy_from_slice(chunk);
        fft.forward(&buf, &mut xr, &mut xi);
        for k in 0..=l {
            let (a, c) = (xr[k], xi[k]);
            xr[k] = a * hr[k] - c * hi[k];
            xi[k] = a * hi[k] + c * hr[k];
        }
        fft.inverse(&xr, &xi, &mut buf);
        for (o, v) in out[b * l..b * l + n].iter_mut().zip(&buf) {
            *o += v;
        }
    }
    out.truncate(x.len());
    out
}

/// `x` delayed by `d` samples (same length).
pub fn delay(x: &[f32], d: usize) -> Vec<f32> {
    let mut out = vec![0.0; x.len()];
    if d < x.len() {
        out[d..].copy_from_slice(&x[..x.len() - d]);
    }
    out
}

/// Mean square of `x`.
pub fn power(x: &[f32]) -> f64 {
    x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len().max(1) as f64
}

/// A power ratio in decibels.
pub fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-30).log10()
}

/// Normalised correlation of two signals.
pub fn correlation(a: &[f32], b: &[f32]) -> f64 {
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        ab += x as f64 * y as f64;
        aa += x as f64 * x as f64;
        bb += y as f64 * y as f64;
    }
    ab / (aa * bb).sqrt().max(1e-30)
}

/// Converts to 16-bit samples (rounded, clamped).
pub fn to_i16(x: &[f32]) -> Vec<i16> {
    x.iter().map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).collect()
}

/// Converts 16-bit samples to floats in [-1, 1).
pub fn to_f32(x: &[i16]) -> Vec<f32> {
    x.iter().map(|&v| v as f32 / 32768.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolution_matches_direct_form() {
        let x = white(3000, 0.3, 1);
        let h = room(16_000, 0.1, 0.05, 2);
        let y = convolve(&x, &h);
        for i in [0usize, 1, 17, 799, 1500, 2999] {
            let want: f32 = (0..=i.min(h.len() - 1)).map(|j| h[j] * x[i - j]).sum();
            assert!((y[i] - want).abs() < 1e-4, "{i}: {} vs {want}", y[i]);
        }
    }

    #[test]
    fn speech_has_the_requested_level_and_layout() {
        let s = speech(16_000, 10.0, MALE, CONVERSATION, 3);
        let on = s.active.iter().filter(|&&a| a).count() as f64 / s.active.len() as f64;
        assert!((0.35..0.85).contains(&on), "active fraction {on}");
        let act: Vec<f32> = s.samples.iter().zip(&s.active).filter(|p| *p.1).map(|p| *p.0).collect();
        assert!((db(power(&act)) - 20.0 * 0.1f64.log10()).abs() < 0.1);
        let idle: Vec<f32> = s.samples.iter().zip(&s.active).filter(|p| !*p.1).map(|p| *p.0).collect();
        assert!(power(&idle) < 1e-12);
        assert!(s.samples.iter().all(|v| v.is_finite() && v.abs() < 1.0));
    }
}
