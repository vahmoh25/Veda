//! Sample-rate conversion.
//!
//! [`Resampler`] is a polyphase windowed-sinc converter. For the common
//! rates the ratio `out / in` is reduced to `L / M` (44 100 → 48 000 is
//! 160 / 147) and every output sample uses the exact filter phase, so the
//! conversion has no timing jitter and never drifts. Very unusual ratios
//! fall back to 1024 phases selected from a 32.32 fixed-point position.
//!
//! The filter is a Kaiser-windowed sinc with 32 taps per phase (more when
//! downsampling), cut off just below the lower of the two Nyquist
//! frequencies. Coefficients are Q14 integers and every phase is normalised
//! to unity gain, so the inner loop is pure integer multiply-add — cheap
//! even under CPU emulation.
//!
//! The converter is streaming: [`Resampler::push`] appends input frames,
//! [`Resampler::pull`] produces as many output frames as the buffered input
//! allows, and [`Resampler::input_needed`] tells a mixer how much input it
//! must provide for a given amount of output.

use alloc::vec;
use alloc::vec::Vec;

use vmath::FloatExt;

/// Largest exact phase count before switching to approximate stepping.
const MAX_EXACT_PHASES: u32 = 1024;
/// Taps per phase when upsampling (or converting between equal-ish rates).
const BASE_TAPS: usize = 32;
/// Kaiser window shape parameter (about 85 dB stop-band attenuation).
const KAISER_BETA: f64 = 8.6;
/// Cut-off as a fraction of the lower Nyquist frequency.
const ROLLOFF: f64 = 0.94;

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0f64, 1.0f64, x * x / 4.0);
    for k in 1..64 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[derive(Debug, Clone, Copy)]
enum Step {
    /// Exact: phase numerator `p` in `0..L`, advanced by `M` per output.
    Exact { l: u32, m: u32, p: u32 },
    /// Approximate: fractional position in 1/2^32 input frames.
    Approx { step: u64, frac: u64 },
    /// Equal rates: no filtering at all.
    Copy,
}

/// A streaming sample-rate converter for interleaved `i16` frames.
pub struct Resampler {
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    taps: usize,
    phases: usize,
    /// `phases` x `taps` Q14 coefficients, each phase reversed so that tap
    /// `j` multiplies input frame `newest - taps + 1 + j`.
    coeffs: Vec<i16>,
    step: Step,
    /// Buffered input frames (interleaved).
    buf: Vec<i16>,
    /// Index (frames) into `buf` of the newest input frame of the next
    /// output.
    newest: usize,
}

impl core::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Resampler({} -> {} Hz, {} ch, {} phases x {} taps)",
            self.in_rate, self.out_rate, self.channels, self.phases, self.taps
        )
    }
}

impl Resampler {
    /// A converter from `in_rate` to `out_rate` for `channels` channels.
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Resampler {
        let (in_rate, out_rate, channels) = (in_rate.max(1), out_rate.max(1), channels.max(1));
        if in_rate == out_rate {
            return Resampler {
                channels,
                in_rate,
                out_rate,
                taps: 1,
                phases: 1,
                coeffs: vec![1 << 14],
                step: Step::Copy,
                buf: Vec::new(),
                newest: 0,
            };
        }
        let g = gcd(in_rate, out_rate);
        let (l, m) = (out_rate / g, in_rate / g);
        let (phases, step) = if l <= MAX_EXACT_PHASES {
            (l as usize, Step::Exact { l, m, p: 0 })
        } else {
            let step = ((in_rate as u64) << 32) / out_rate as u64;
            (MAX_EXACT_PHASES as usize, Step::Approx { step, frac: 0 })
        };
        let ratio = in_rate as f64 / out_rate as f64;
        let mut taps = BASE_TAPS;
        if ratio > 1.0 {
            taps = ((BASE_TAPS as f64 * ratio) as usize).next_multiple_of(2).min(128);
        }
        let coeffs = design(phases, taps, ratio);
        let mut r = Resampler { channels, in_rate, out_rate, taps, phases, coeffs, step, buf: Vec::new(), newest: 0 };
        r.reset();
        r
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// True when the rates are equal and samples are copied unchanged.
    pub fn is_passthrough(&self) -> bool {
        matches!(self.step, Step::Copy)
    }

    /// Forgets all buffered input (e.g. after a seek).
    pub fn reset(&mut self) {
        self.buf.clear();
        match &mut self.step {
            Step::Copy => self.newest = 0,
            Step::Exact { p, .. } => {
                *p = 0;
                // Start with taps - 1 frames of silent history.
                self.buf.resize((self.taps - 1) * self.channels, 0);
                self.newest = self.taps - 1;
            }
            Step::Approx { frac, .. } => {
                *frac = 0;
                self.buf.resize((self.taps - 1) * self.channels, 0);
                self.newest = self.taps - 1;
            }
        }
    }

    /// Frames buffered beyond the history the filter needs.
    pub fn buffered(&self) -> usize {
        (self.buf.len() / self.channels).saturating_sub(self.newest)
    }

    /// Delay introduced by the filter, in input frames.
    pub fn delay_frames(&self) -> usize {
        if self.is_passthrough() { 0 } else { self.taps / 2 }
    }

    /// Input frames that must still be pushed before `out_frames` more
    /// output frames can be pulled.
    pub fn input_needed(&self, out_frames: usize) -> usize {
        if out_frames == 0 {
            return 0;
        }
        let have = self.buf.len() / self.channels;
        let last = match self.step {
            Step::Copy => self.newest + out_frames - 1,
            Step::Exact { l, m, p } => {
                self.newest + ((p as u64 + (out_frames as u64 - 1) * m as u64) / l as u64) as usize
            }
            Step::Approx { step, frac } => {
                self.newest + ((frac as u128 + (out_frames as u128 - 1) * step as u128) >> 32) as usize
            }
        };
        (last + 1).saturating_sub(have)
    }

    /// Appends interleaved input frames.
    pub fn push(&mut self, input: &[i16]) {
        let n = input.len() - input.len() % self.channels;
        self.buf.extend_from_slice(&input[..n]);
    }

    /// Produces up to `out.len() / channels` frames; returns the number of
    /// frames written.
    pub fn pull(&mut self, out: &mut [i16]) -> usize {
        let ch = self.channels;
        let want = out.len() / ch;
        let have = self.buf.len() / ch;
        let mut produced = 0;
        if let Step::Copy = self.step {
            let n = want.min(have - self.newest);
            out[..n * ch].copy_from_slice(&self.buf[self.newest * ch..(self.newest + n) * ch]);
            self.newest += n;
            produced = n;
        } else {
            let taps = self.taps;
            while produced < want && self.newest < have {
                let phase = match self.step {
                    Step::Exact { p, .. } => p as usize,
                    Step::Approx { frac, .. } => ((frac >> 22) as usize).min(self.phases - 1),
                    Step::Copy => 0,
                };
                let c = &self.coeffs[phase * taps..phase * taps + taps];
                let start = (self.newest + 1 - taps) * ch;
                let x = &self.buf[start..start + taps * ch];
                let o = &mut out[produced * ch..produced * ch + ch];
                if ch == 2 {
                    let (mut l, mut r) = (0i32, 0i32);
                    for (k, f) in c.iter().zip(x.as_chunks::<2>().0) {
                        l += *k as i32 * f[0] as i32;
                        r += *k as i32 * f[1] as i32;
                    }
                    o[0] = ((l + (1 << 13)) >> 14).clamp(-32768, 32767) as i16;
                    o[1] = ((r + (1 << 13)) >> 14).clamp(-32768, 32767) as i16;
                } else {
                    for (cc, ov) in o.iter_mut().enumerate() {
                        let mut acc = 0i32;
                        for (j, k) in c.iter().enumerate() {
                            acc += *k as i32 * x[j * ch + cc] as i32;
                        }
                        *ov = ((acc + (1 << 13)) >> 14).clamp(-32768, 32767) as i16;
                    }
                }
                produced += 1;
                match &mut self.step {
                    Step::Exact { l, m, p } => {
                        *p += *m;
                        while *p >= *l {
                            *p -= *l;
                            self.newest += 1;
                        }
                    }
                    Step::Approx { step, frac } => {
                        *frac += *step;
                        self.newest += (*frac >> 32) as usize;
                        *frac &= 0xFFFF_FFFF;
                    }
                    Step::Copy => {}
                }
            }
        }
        // Drop input that no future output needs (keep taps - 1 frames).
        let keep_from = self.newest.saturating_sub(self.taps - 1).min(have);
        if keep_from > 0 && keep_from * ch >= 1024 {
            self.buf.drain(..keep_from * ch);
            self.newest -= keep_from;
        }
        produced
    }

    /// Converts a whole buffer in one go (convenience for offline use).
    pub fn process_all(in_rate: u32, out_rate: u32, channels: usize, input: &[i16]) -> Vec<i16> {
        let mut r = Resampler::new(in_rate, out_rate, channels);
        let frames_in = input.len() / channels.max(1);
        let frames_out = (frames_in as u64 * out_rate as u64 / in_rate.max(1) as u64) as usize;
        r.push(input);
        // Flush the filter delay with silence.
        r.push(&vec![0i16; r.delay_frames() * channels.max(1)]);
        let mut out = vec![0i16; frames_out * channels.max(1)];
        // Skip the filter delay so that output is aligned with the input.
        let skip = (r.delay_frames() as u64 * out_rate as u64 / in_rate.max(1) as u64) as usize;
        let mut scratch = vec![0i16; skip * channels.max(1)];
        r.pull(&mut scratch);
        let n = r.pull(&mut out);
        out.truncate(n * channels.max(1));
        out
    }
}

/// Designs the polyphase filter bank (Q14, each phase normalised to 1.0).
fn design(phases: usize, taps: usize, ratio: f64) -> Vec<i16> {
    let len = phases * taps;
    // Cut-off relative to the upsampled rate (phases x input rate).
    let fc = 0.5 * ROLLOFF * if ratio > 1.0 { 1.0 / ratio } else { 1.0 } / phases as f64;
    let center = (len as f64 - 1.0) / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let proto: Vec<f64> = (0..len)
        .map(|q| {
            let x = q as f64 - center;
            let arg = 2.0 * fc * x;
            let sinc = if arg.abs() < 1e-12 {
                1.0
            } else {
                FloatExt::sin(core::f64::consts::PI * arg) / (core::f64::consts::PI * arg)
            };
            let r = x / (center + 0.5);
            let w = bessel_i0(KAISER_BETA * FloatExt::sqrt((1.0 - r * r).max(0.0))) / i0_beta;
            sinc * w
        })
        .collect();
    let mut out = vec![0i16; len];
    for p in 0..phases {
        // Phase p uses h[m * phases + p] for input frame newest - m.
        let sum: f64 = (0..taps).map(|m| proto[m * phases + p]).sum();
        let scale = if sum.abs() > 1e-9 { 16384.0 / sum } else { 0.0 };
        let mut q: Vec<i32> = (0..taps).map(|m| FloatExt::round(proto[m * phases + p] * scale) as i32).collect();
        // Fix rounding so the phase sums to exactly 16384 (unity gain).
        let err = 16384 - q.iter().sum::<i32>();
        if let Some(mid) = q.get_mut(taps / 2) {
            *mid += err;
        }
        for m in 0..taps {
            // Reverse: tap j multiplies frame newest - (taps - 1 - j).
            out[p * taps + (taps - 1 - m)] = q[m].clamp(-32768, 32767) as i16;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, frames: usize, channels: usize) -> Vec<i16> {
        let mut v = Vec::new();
        for i in 0..frames {
            let s = (i as f64 * freq / rate as f64 * core::f64::consts::TAU).sin() * 16000.0;
            for _ in 0..channels {
                v.push(s.round() as i16);
            }
        }
        v
    }

    /// Fits a sine of known frequency to `x` and returns the residual SNR.
    fn sine_snr(x: &[i16], rate: u32, freq: f64, channels: usize) -> f64 {
        let n = x.len() / channels;
        let (mut ss, mut sc) = (0f64, 0f64);
        for i in 0..n {
            let ph = i as f64 * freq / rate as f64 * core::f64::consts::TAU;
            ss += x[i * channels] as f64 * ph.sin();
            sc += x[i * channels] as f64 * ph.cos();
        }
        let (a, b) = (2.0 * ss / n as f64, 2.0 * sc / n as f64);
        let (mut sig, mut noise) = (0f64, 0f64);
        for i in 0..n {
            let ph = i as f64 * freq / rate as f64 * core::f64::consts::TAU;
            let fit = a * ph.sin() + b * ph.cos();
            sig += fit * fit;
            noise += (x[i * channels] as f64 - fit).powi(2);
        }
        10.0 * (sig / noise.max(1e-9)).log10()
    }

    #[test]
    fn converts_44k_to_48k_cleanly() {
        let input = sine(44_100, 1000.0, 44_100, 2);
        let out = Resampler::process_all(44_100, 48_000, 2, &input);
        assert!((out.len() as i64 / 2 - 48_000).abs() <= 2, "{}", out.len());
        // Skip the edges (filter ramps).
        let mid = &out[2000..out.len() - 2000];
        let snr = sine_snr(mid, 48_000, 1000.0, 2);
        assert!(snr > 70.0, "SNR {snr:.1} dB");
        // Both channels identical.
        assert!(mid.chunks(2).all(|f| f[0] == f[1]));
    }

    #[test]
    fn high_frequencies_survive_and_images_are_removed() {
        let input = sine(44_100, 15_000.0, 22_050, 1);
        let out = Resampler::process_all(44_100, 48_000, 1, &input);
        let snr = sine_snr(&out[1000..out.len() - 1000], 48_000, 15_000.0, 1);
        assert!(snr > 50.0, "SNR {snr:.1} dB");
    }

    #[test]
    fn downsampling_and_odd_ratios() {
        for (a, b) in [(96_000u32, 48_000u32), (22_050, 48_000), (8_000, 48_000), (44_100, 47_999), (48_000, 44_100)] {
            let input = sine(a, 440.0, a as usize / 2, 1);
            let out = Resampler::process_all(a, b, 1, &input);
            let expect = b as usize / 2;
            assert!((out.len() as i64 - expect as i64).abs() <= 3, "{a}->{b}: {}", out.len());
            let snr = sine_snr(&out[500..out.len() - 500], b, 440.0, 1);
            assert!(snr > 60.0, "{a}->{b}: SNR {snr:.1} dB");
        }
    }

    #[test]
    fn streaming_matches_one_shot() {
        let input = sine(44_100, 3000.0, 10_000, 2);
        let mut r = Resampler::new(44_100, 48_000, 2);
        let mut streamed = Vec::new();
        let mut pos = 0;
        let mut chunk = 1;
        while pos < input.len() / 2 {
            // Ask for a varying number of output frames, provide just enough input.
            let want = 1 + chunk % 300;
            chunk += 37;
            let full = r.input_needed(want);
            let need = full.min(input.len() / 2 - pos);
            r.push(&input[pos * 2..(pos + need) * 2]);
            pos += need;
            let mut out = vec![0i16; want * 2];
            let n = r.pull(&mut out);
            if need == full {
                assert_eq!(n, want);
            }
            streamed.extend_from_slice(&out[..n * 2]);
        }
        let mut r2 = Resampler::new(44_100, 48_000, 2);
        r2.push(&input);
        let mut whole = vec![0i16; streamed.len()];
        let n = r2.pull(&mut whole);
        assert_eq!(n * 2, streamed.len());
        assert_eq!(whole, streamed);
    }

    #[test]
    fn input_needed_is_exact() {
        let mut r = Resampler::new(44_100, 48_000, 1);
        for want in [1usize, 2, 7, 160, 1000] {
            let need = r.input_needed(want);
            r.push(&vec![100i16; need]);
            let mut out = vec![0i16; want];
            assert_eq!(r.pull(&mut out), want);
            assert_eq!(r.input_needed(0), 0);
        }
    }

    #[test]
    fn passthrough() {
        let mut r = Resampler::new(48_000, 48_000, 2);
        assert!(r.is_passthrough());
        assert_eq!(r.input_needed(10), 10);
        r.push(&[1, 2, 3, 4]);
        let mut out = [0i16; 8];
        assert_eq!(r.pull(&mut out), 2);
        assert_eq!(&out[..4], &[1, 2, 3, 4]);
    }
}
