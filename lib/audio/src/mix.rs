//! Gains, volume curves, mixing and sample conversion.
//!
//! The mixer inside the audio service runs on every block of audio, so the
//! mixing helpers use integer arithmetic: samples are `i16`, the
//! accumulator is `i32`, and gains are Q15 fixed point (`32768` = 1.0).
//! Gain changes are ramped linearly across a block, which avoids the
//! clicks of an abrupt step.

use vmath::FloatExt;

/// Unity gain in Q15.
pub const UNITY_Q15: i32 = 1 << 15;

/// Converts a float sample in [-1, 1] to `i16` (rounded, clamped).
#[inline]
pub fn f32_to_i16(x: f32) -> i16 {
    let v = x * 32768.0;
    let v = if v >= 0.0 { v + 0.5 } else { v - 0.5 };
    (v as i32).clamp(-32768, 32767) as i16
}

/// Converts an `i16` sample to a float in [-1, 1).
#[inline]
pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 * (1.0 / 32768.0)
}

/// Decibels to a linear amplitude factor.
pub fn db_to_gain(db: f32) -> f32 {
    FloatExt::powf(10.0f32, db / 20.0)
}

/// Linear amplitude factor to decibels (`-inf` is clamped to -144 dB).
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 1e-7 { -144.0 } else { 20.0 * FloatExt::log10(gain) }
}

/// Maps a volume slider position (0..=1) to an amplitude factor with a
/// perceptual (cubic) taper: half way is about -18 dB.
pub fn slider_to_gain(pos: f32) -> f32 {
    let p = pos.clamp(0.0, 1.0);
    p * p * p
}

/// Inverse of [`slider_to_gain`].
pub fn gain_to_slider(gain: f32) -> f32 {
    FloatExt::cbrt(gain.clamp(0.0, 1.0))
}

/// A linear gain (clamped to 0..=1) in Q15.
#[inline]
pub fn gain_q15(gain: f32) -> i32 {
    (gain.clamp(0.0, 1.0) * UNITY_Q15 as f32 + 0.5) as i32
}

/// Adds `src` (interleaved, same layout as `acc`) scaled by a gain that
/// ramps linearly from `g0` to `g1` (Q15, 0..=32768) across the block.
/// `channels` is the number of interleaved channels (the ramp advances per
/// frame).
pub fn mix_into(acc: &mut [i32], src: &[i16], channels: usize, g0: i32, g1: i32) {
    let channels = channels.max(1);
    let frames = acc.len().min(src.len()) / channels;
    if frames == 0 {
        return;
    }
    let (g0, g1) = (g0.clamp(0, UNITY_Q15), g1.clamp(0, UNITY_Q15));
    let acc = &mut acc[..frames * channels];
    let src = &src[..frames * channels];
    if g0 == g1 {
        if g0 == 0 {
            return;
        }
        if g0 == UNITY_Q15 {
            for (a, &s) in acc.iter_mut().zip(src) {
                *a += s as i32;
            }
        } else {
            for (a, &s) in acc.iter_mut().zip(src) {
                *a += (s as i32 * g0) >> 15;
            }
        }
        return;
    }
    // 16.16 fixed-point ramp of the Q15 gain.
    let mut g = (g0 as i64) << 16;
    let step = (((g1 - g0) as i64) << 16) / frames as i64;
    for (a, s) in acc.chunks_exact_mut(channels).zip(src.chunks_exact(channels)) {
        let gi = (g >> 16) as i32;
        for (x, &y) in a.iter_mut().zip(s) {
            *x += (y as i32 * gi) >> 15;
        }
        g += step;
    }
}

/// Converts a mix accumulator to `i16` with a final Q15 gain, clamping
/// out-of-range values.
pub fn finish_mix(acc: &[i32], out: &mut [i16], gain: i32) {
    let gain = gain.clamp(0, UNITY_Q15);
    if gain == UNITY_Q15 {
        for (o, &a) in out.iter_mut().zip(acc) {
            *o = a.clamp(-32768, 32767) as i16;
        }
    } else {
        for (o, &a) in out.iter_mut().zip(acc) {
            *o = ((a as i64 * gain as i64) >> 15).clamp(-32768, 32767) as i16;
        }
    }
}

/// Converts interleaved stereo to mono (average) or mono to stereo
/// (duplicate) as needed; `out` must hold `frames * out_channels` samples.
pub fn convert_channels(src: &[i16], in_channels: usize, out: &mut [i16], out_channels: usize) -> usize {
    let (ic, oc) = (in_channels.max(1), out_channels.max(1));
    let frames = (src.len() / ic).min(out.len() / oc);
    for f in 0..frames {
        let s = &src[f * ic..f * ic + ic];
        let o = &mut out[f * oc..f * oc + oc];
        match (ic, oc) {
            (a, b) if a == b => o.copy_from_slice(s),
            (1, _) => o.fill(s[0]),
            (_, 1) => o[0] = ((s[0] as i32 + s[1] as i32) >> 1) as i16,
            _ => {
                for (c, v) in o.iter_mut().enumerate() {
                    *v = s[c.min(ic - 1)];
                }
            }
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn conversions() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32768);
        assert_eq!(f32_to_i16(-2.0), -32768);
        assert_eq!(f32_to_i16(0.5), 16384);
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-3);
        assert!((gain_to_db(0.1) + 20.0).abs() < 1e-3);
        assert!((gain_to_slider(slider_to_gain(0.37)) - 0.37).abs() < 1e-4);
        assert_eq!(gain_q15(1.0), UNITY_Q15);
        assert_eq!(gain_q15(2.0), UNITY_Q15);
        assert_eq!(gain_q15(-1.0), 0);
    }

    #[test]
    fn mixing_sums_and_clamps() {
        let mut acc = vec![0i32; 4];
        mix_into(&mut acc, &[30_000, -30_000, 100, 0], 2, UNITY_Q15, UNITY_Q15);
        mix_into(&mut acc, &[30_000, -30_000, 100, 0], 2, UNITY_Q15 / 2, UNITY_Q15 / 2);
        assert_eq!(acc, vec![45_000, -45_000, 150, 0]);
        let mut out = vec![0i16; 4];
        finish_mix(&acc, &mut out, UNITY_Q15);
        assert_eq!(out, vec![32767, -32768, 150, 0]);
        finish_mix(&acc, &mut out, UNITY_Q15 / 2);
        assert_eq!(out, vec![22_500, -22_500, 75, 0]);
    }

    #[test]
    fn gain_ramps_are_monotonic() {
        let mut acc = vec![0i32; 200];
        let src = vec![10_000i16; 200];
        mix_into(&mut acc, &src, 2, 0, UNITY_Q15);
        assert_eq!(acc[0], 0);
        assert!(acc.windows(2).all(|w| w[1] >= w[0]));
        assert!(acc[199] > 9_800);
    }

    #[test]
    fn channel_conversion() {
        let mut out = vec![0i16; 4];
        assert_eq!(convert_channels(&[1, 3, 5, 7], 2, &mut out, 1), 2);
        assert_eq!(&out[..2], &[2, 6]);
        assert_eq!(convert_channels(&[4, 8], 1, &mut out, 2), 2);
        assert_eq!(out, vec![4, 4, 8, 8]);
    }
}
