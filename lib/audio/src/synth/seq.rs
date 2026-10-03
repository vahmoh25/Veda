//! Songs: parts (an instrument, its notes and mixing settings) rendered
//! offline into a mastered stereo mix.
//!
//! Signal flow: every note is rendered by its part's instrument into the
//! part's buffer; the part then gets its insert chain (high-pass, EQ,
//! automated low-pass, drive, chorus), volume automation, side-chain
//! ducking, pan and gain, and is summed into the dry bus with sends to a
//! shared delay and reverb. The master chain is high-pass, shelving EQ, a
//! glue compressor, loudness normalisation and a look-ahead limiter, so the
//! result never clips.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use vmath::FloatExt;

use super::filter::{Biquad, BiquadKind, Mode, Svf};
use super::fx::{Chorus, Compressor, Delay, Limiter, Reverb, soft_clip};
use super::voice::{Instrument, pan_gains};

/// A note: start and length in beats, MIDI pitch, velocity 0..1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    pub start: f64,
    pub len: f64,
    pub pitch: f32,
    pub vel: f32,
}

impl Note {
    pub const fn new(start: f64, len: f64, pitch: f32, vel: f32) -> Note {
        Note { start, len, pitch, vel }
    }
}

/// One instrument track.
pub struct Part {
    pub name: String,
    pub inst: Box<dyn Instrument>,
    pub notes: Vec<Note>,
    /// Linear gain.
    pub gain: f32,
    /// -1 (left) .. 1 (right).
    pub pan: f32,
    /// Send levels.
    pub reverb: f32,
    pub delay: f32,
    /// Side-chain ducking depth (0..1) driven by [`Song::duck_times`].
    pub duck: f32,
    /// (rate Hz, depth ms, mix).
    pub chorus: Option<(f32, f32, f32)>,
    /// High-pass cut-off in Hz (0 = off).
    pub highpass: f32,
    /// Low-pass automation: (beat, Hz) points (empty = off).
    pub lowpass: Vec<(f64, f32)>,
    /// Volume automation: (beat, gain) points (empty = constant 1).
    pub volume: Vec<(f64, f32)>,
    /// Static EQ bands: (kind, frequency, Q).
    pub eq: Vec<(BiquadKind, f32, f32)>,
    /// Saturation drive (1 = clean).
    pub drive: f32,
}

impl Part {
    pub fn new(name: &str, inst: Box<dyn Instrument>) -> Part {
        Part {
            name: name.into(),
            inst,
            notes: Vec::new(),
            gain: 1.0,
            pan: 0.0,
            reverb: 0.0,
            delay: 0.0,
            duck: 0.0,
            chorus: None,
            highpass: 0.0,
            lowpass: Vec::new(),
            volume: Vec::new(),
            eq: Vec::new(),
            drive: 1.0,
        }
    }

    pub fn note(&mut self, start: f64, len: f64, pitch: f32, vel: f32) {
        self.notes.push(Note::new(start, len, pitch, vel));
    }
}

/// Master-bus settings.
#[derive(Debug, Clone, Copy)]
pub struct Master {
    pub highpass: f32,
    pub low_shelf: (f32, f32),
    pub high_shelf: (f32, f32),
    pub comp_threshold: f32,
    pub comp_ratio: f32,
    /// Loudness target (RMS of the louder parts, dBFS).
    pub target_rms_db: f32,
    /// Peak ceiling (dBFS).
    pub ceiling_db: f32,
}

impl Default for Master {
    fn default() -> Master {
        Master {
            highpass: 28.0,
            low_shelf: (120.0, 0.0),
            high_shelf: (9000.0, 0.0),
            comp_threshold: -16.0,
            comp_ratio: 2.0,
            target_rms_db: -15.5,
            ceiling_db: -1.0,
        }
    }
}

/// A complete arrangement.
pub struct Song {
    pub bpm: f64,
    pub sr: f32,
    /// Length in beats (the reverb tail is added after it).
    pub length: f64,
    pub tail: f32,
    pub parts: Vec<Part>,
    /// Reverb: (size, damping, width, pre-delay seconds) and return level.
    pub reverb: (f32, f32, f32, f32),
    pub reverb_return: f32,
    /// Delay time in beats, feedback and return level.
    pub delay_beats: f64,
    pub delay_feedback: f32,
    pub delay_return: f32,
    /// Beats at which ducking parts dip (usually the kick).
    pub duck_times: Vec<f64>,
    /// Ducking recovery time in seconds.
    pub duck_release: f32,
    pub master: Master,
    /// Fade in/out at the very start and end (seconds).
    pub fade_in: f32,
    pub fade_out: f32,
}

impl Song {
    pub fn new(bpm: f64, sr: f32, length: f64) -> Song {
        Song {
            bpm,
            sr,
            length,
            tail: 3.0,
            parts: Vec::new(),
            reverb: (0.6, 0.5, 1.0, 0.02),
            reverb_return: 1.0,
            delay_beats: 0.75,
            delay_feedback: 0.35,
            delay_return: 1.0,
            duck_times: Vec::new(),
            duck_release: 0.25,
            master: Master::default(),
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }

    pub fn seconds_per_beat(&self) -> f64 {
        60.0 / self.bpm
    }

    /// Total length in seconds (including the tail).
    pub fn duration(&self) -> f64 {
        self.length * self.seconds_per_beat() + self.tail as f64
    }
}

/// Linear interpolation through automation points.
pub fn automation(points: &[(f64, f32)], beat: f64) -> f32 {
    match points.iter().position(|p| p.0 > beat) {
        None => points.last().map(|p| p.1).unwrap_or(1.0),
        Some(0) => points[0].1,
        Some(i) => {
            let (a, b) = (points[i - 1], points[i]);
            let t = ((beat - a.0) / (b.0 - a.0).max(1e-9)) as f32;
            a.1 + (b.1 - a.1) * t
        }
    }
}

/// Side-chain gain at time `t` seconds (1 = no ducking).
fn duck_gain(times: &[f64], spb: f64, t: f64, depth: f32, release: f32) -> f32 {
    // Most recent trigger at or before t.
    let beat = t / spb;
    let i = times.partition_point(|&b| b <= beat);
    if i == 0 {
        return 1.0;
    }
    let dt = (t - times[i - 1] * spb) as f32;
    let attack = 0.004;
    let d = if dt < attack {
        dt / attack
    } else {
        let x = ((dt - attack) / release).min(1.0);
        (1.0 - x) * (1.0 - x)
    };
    1.0 - depth * d
}

/// Renders a part (notes, inserts, volume, ducking) into a stereo buffer.
fn render_part(song: &Song, part: &Part, index: usize, frames: usize) -> Vec<f32> {
    let sr = song.sr;
    let spb = song.seconds_per_beat();
    let mut buf = vec![0.0f32; frames * 2];
    for (k, n) in part.notes.iter().enumerate() {
        let start = (n.start * spb * sr as f64).round() as i64;
        if start < 0 || start as usize >= frames {
            continue;
        }
        let secs = (n.len * spb) as f32;
        let seed = (index as u32).wrapping_mul(0x9E37_79B9) ^ (k as u32).wrapping_mul(0x85EB_CA6B);
        let v = part.inst.render(n.pitch, n.vel, secs, sr, seed);
        let s = start as usize * 2;
        let end = (s + v.len()).min(buf.len());
        for (d, x) in buf[s..end].iter_mut().zip(&v) {
            *d += *x;
        }
    }
    // Inserts.
    if part.highpass > 0.0 || !part.eq.is_empty() {
        let mut chain: Vec<[Biquad; 2]> = Vec::new();
        if part.highpass > 0.0 {
            let b = Biquad::new(BiquadKind::HighPass, part.highpass, 0.707, sr);
            chain.push([b.clone(), b]);
        }
        for &(kind, f, q) in &part.eq {
            let b = Biquad::new(kind, f, q, sr);
            chain.push([b.clone(), b]);
        }
        for fr in buf.as_chunks_mut::<2>().0 {
            for [bl, br] in chain.iter_mut() {
                fr[0] = bl.process(fr[0]);
                fr[1] = br.process(fr[1]);
            }
        }
    }
    if !part.lowpass.is_empty() {
        let mut f = [Svf::new(Mode::LowPass), Svf::new(Mode::LowPass)];
        for (i, fr) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            if i % 32 == 0 {
                let beat = i as f64 / sr as f64 / spb;
                let fc = automation(&part.lowpass, beat);
                f[0].set(fc, 0.8, sr);
                f[1].set(fc, 0.8, sr);
            }
            fr[0] = f[0].process(fr[0]);
            fr[1] = f[1].process(fr[1]);
        }
    }
    if part.drive > 1.0 {
        let norm = 1.0 / soft_clip(part.drive);
        for v in buf.iter_mut() {
            *v = soft_clip(*v * part.drive) * norm;
        }
    }
    if let Some((rate, depth, mix)) = part.chorus {
        let mut c = Chorus::new(rate, depth, mix, sr);
        for fr in buf.as_chunks_mut::<2>().0 {
            let (l, r) = c.process(fr[0], fr[1]);
            fr[0] = l;
            fr[1] = r;
        }
    }
    let (pl, pr) = pan_gains(part.pan);
    let (pl, pr) = (pl * core::f32::consts::SQRT_2, pr * core::f32::consts::SQRT_2);
    for (i, fr) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let t = i as f64 / sr as f64;
        let mut g = part.gain;
        if !part.volume.is_empty() {
            g *= automation(&part.volume, t / spb);
        }
        if part.duck > 0.0 {
            g *= duck_gain(&song.duck_times, spb, t, part.duck, song.duck_release);
        }
        fr[0] *= g * pl;
        fr[1] *= g * pr;
    }
    buf
}

/// The level of every part before the master chain: (name, RMS of the
/// louder half of its 100 ms blocks in dBFS, peak dBFS). A mixing aid.
pub fn part_levels(song: &Song) -> Vec<(String, f32, f32)> {
    let frames = (song.duration() * song.sr as f64) as usize;
    let block = (song.sr * 0.1) as usize * 2;
    song.parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let buf = render_part(song, p, i, frames);
            let mut levels: Vec<f32> = buf
                .chunks(block)
                .map(|b| b.iter().map(|v| v * v).sum::<f32>() / b.len() as f32)
                .filter(|&l| l > 1e-10)
                .collect();
            levels.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));
            let loud = &levels[..(levels.len() / 2).max(1).min(levels.len())];
            let rms = if loud.is_empty() { 0.0 } else { FloatExt::sqrt(loud.iter().sum::<f32>() / loud.len() as f32) };
            let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            (p.name.clone(), 20.0 * FloatExt::log10(rms.max(1e-9)), 20.0 * FloatExt::log10(peak.max(1e-9)))
        })
        .collect()
}

/// Statistics of a rendered mix.
#[derive(Debug, Clone, Copy, Default)]
pub struct MixStats {
    pub peak_db: f32,
    pub rms_db: f32,
    pub gain_db: f32,
    /// Largest gain reduction applied by the limiter (dB, <= 0).
    pub limit_db: f32,
}

/// Renders and masters a song. Returns interleaved stereo in -1..1 and
/// some statistics.
pub fn render(song: &Song) -> (Vec<f32>, MixStats) {
    let sr = song.sr;
    let frames = (song.duration() * sr as f64) as usize;
    let mut dry = vec![0.0f32; frames * 2];
    let mut rev_bus = vec![0.0f32; frames * 2];
    let mut del_bus = vec![0.0f32; frames * 2];
    for (i, part) in song.parts.iter().enumerate() {
        let buf = render_part(song, part, i, frames);
        for ((d, x), (rb, db)) in dry.iter_mut().zip(&buf).zip(rev_bus.iter_mut().zip(del_bus.iter_mut())) {
            *d += *x;
            *rb += *x * part.reverb;
            *db += *x * part.delay;
        }
    }
    // Delay (its output also feeds the reverb a little).
    let spb = song.seconds_per_beat();
    let mut delay = Delay::new((song.delay_beats * spb) as f32, song.delay_feedback, 5000.0, sr);
    for i in 0..frames {
        let (l, r) = delay.process(del_bus[i * 2], del_bus[i * 2 + 1]);
        let (l, r) = (l * song.delay_return, r * song.delay_return);
        dry[i * 2] += l;
        dry[i * 2 + 1] += r;
        rev_bus[i * 2] += l * 0.3;
        rev_bus[i * 2 + 1] += r * 0.3;
    }
    drop(del_bus);
    let (size, damp, width, pre) = song.reverb;
    let mut reverb = Reverb::new(size, damp, width, pre, sr);
    for i in 0..frames {
        let (l, r) = reverb.process(rev_bus[i * 2], rev_bus[i * 2 + 1]);
        dry[i * 2] += l * song.reverb_return;
        dry[i * 2 + 1] += r * song.reverb_return;
    }
    drop(rev_bus);
    // Master: high-pass, shelves, glue compression.
    let m = song.master;
    let mut eq: Vec<[Biquad; 2]> = Vec::new();
    for b in [
        Biquad::new(BiquadKind::HighPass, m.highpass, 0.707, sr),
        Biquad::new(BiquadKind::LowShelf(m.low_shelf.1), m.low_shelf.0, 0.707, sr),
        Biquad::new(BiquadKind::HighShelf(m.high_shelf.1), m.high_shelf.0, 0.707, sr),
    ] {
        eq.push([b.clone(), b]);
    }
    let mut comp = Compressor::new(m.comp_threshold, m.comp_ratio, 15.0, 180.0, sr);
    for fr in dry.as_chunks_mut::<2>().0 {
        for [bl, br] in eq.iter_mut() {
            fr[0] = bl.process(fr[0]);
            fr[1] = br.process(fr[1]);
        }
        let g = comp.gain(fr[0].abs().max(fr[1].abs()));
        fr[0] *= g;
        fr[1] *= g;
    }
    // Loudness: RMS over the louder half of 100 ms blocks.
    let block = (sr * 0.1) as usize * 2;
    let mut levels: Vec<f32> =
        dry.chunks(block).map(|b| b.iter().map(|v| v * v).sum::<f32>() / b.len() as f32).collect();
    levels.sort_by(|a, b| b.partial_cmp(a).unwrap_or(core::cmp::Ordering::Equal));
    let loud = &levels[..(levels.len() / 2).max(1)];
    let rms = FloatExt::sqrt(loud.iter().sum::<f32>() / loud.len() as f32);
    let rms_db = 20.0 * FloatExt::log10(rms.max(1e-9));
    let peak = dry.iter().fold(0.0f32, |p, v| p.max(v.abs()));
    let ceiling = FloatExt::powf(10.0f32, m.ceiling_db / 20.0);
    let mut gain_db = m.target_rms_db - rms_db;
    // Gentle mastering: never ask the limiter for more than 3 dB.
    let peak_db = 20.0 * FloatExt::log10(peak.max(1e-9));
    gain_db = gain_db.min(m.ceiling_db + 3.0 - peak_db);
    let gain = FloatExt::powf(10.0f32, gain_db / 20.0);
    let mut lim = Limiter::new(m.ceiling_db, 3.0, 90.0, sr);
    let lat = lim.latency();
    let mut out = vec![0.0f32; frames * 2];
    for i in 0..frames + lat {
        let (l, r) = if i < frames { (dry[i * 2] * gain, dry[i * 2 + 1] * gain) } else { (0.0, 0.0) };
        let (ol, or) = lim.process(l, r);
        if i >= lat {
            out[(i - lat) * 2] = ol;
            out[(i - lat) * 2 + 1] = or;
        }
    }
    // Fades.
    let fi = (song.fade_in * sr) as usize;
    let fo = (song.fade_out * sr) as usize;
    for i in 0..frames {
        let mut g = 1.0f32;
        if i < fi {
            g *= i as f32 / fi as f32;
        }
        if fo > 0 && i + fo > frames {
            let x = (frames - i) as f32 / fo as f32;
            g *= x * x;
        }
        out[i * 2] = (out[i * 2] * g).clamp(-ceiling, ceiling);
        out[i * 2 + 1] = (out[i * 2 + 1] * g).clamp(-ceiling, ceiling);
    }
    let final_peak = out.iter().fold(0.0f32, |p, v| p.max(v.abs()));
    let stats = MixStats {
        peak_db: 20.0 * FloatExt::log10(final_peak.max(1e-9)),
        rms_db: rms_db + gain_db,
        gain_db,
        limit_db: 20.0 * FloatExt::log10(lim.min_gain.max(1e-9)),
    };
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::env::Adsr;
    use crate::synth::osc::Wave;
    use crate::synth::voice::Analog;

    #[test]
    fn automation_interpolates() {
        let p = [(0.0, 100.0), (4.0, 500.0)];
        assert_eq!(automation(&p, -1.0), 100.0);
        assert_eq!(automation(&p, 2.0), 300.0);
        assert_eq!(automation(&p, 9.0), 500.0);
    }

    #[test]
    fn a_tiny_song_renders_without_clipping() {
        let mut song = Song::new(120.0, 22_050.0, 8.0);
        song.tail = 0.5;
        let mut p = Part::new("lead", Box::new(Analog::new(Wave::Saw, Adsr::new(0.01, 0.2, 0.6, 0.2))));
        for i in 0..8 {
            p.note(i as f64, 0.9, 60.0 + (i % 4) as f32 * 3.0, 0.9);
        }
        p.gain = 3.0;
        p.reverb = 0.3;
        p.delay = 0.2;
        song.parts.push(p);
        let (out, stats) = render(&song);
        assert_eq!(out.len(), (song.duration() * 22_050.0) as usize * 2);
        assert!(stats.peak_db <= -0.99 && stats.peak_db > -6.0, "{stats:?}");
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
