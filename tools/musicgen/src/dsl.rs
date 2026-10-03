//! A tiny composition language on top of `vaudio::synth`: drum patterns
//! as strings, melodies as `note/beats` tokens, chord progressions,
//! arpeggios, basslines and humanisation.

use vaudio::synth::Part;
use vaudio::synth::theory::{self, note};

/// Beats per bar (everything here is in 4/4).
pub const BAR: f64 = 4.0;

/// Drum keys used by the kits.
pub mod key {
    pub const KICK: f32 = 36.0;
    pub const SNARE: f32 = 38.0;
    pub const CLAP: f32 = 39.0;
    pub const RIM: f32 = 37.0;
    pub const HAT: f32 = 42.0;
    pub const OPEN_HAT: f32 = 46.0;
    pub const SHAKER: f32 = 70.0;
    pub const CRASH: f32 = 49.0;
    pub const RIDE: f32 = 51.0;
    pub const TOM_LO: f32 = 45.0;
    pub const TOM_HI: f32 = 48.0;
    pub const FX: f32 = 60.0;
}

/// A small deterministic random generator for humanisation.
#[derive(Debug, Clone)]
pub struct Humanize {
    state: u64,
    /// Timing jitter in beats.
    pub timing: f64,
    /// Velocity jitter (fraction).
    pub velocity: f32,
}

impl Humanize {
    pub fn new(seed: u64, timing: f64, velocity: f32) -> Humanize {
        Humanize { state: seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407), timing, velocity }
    }

    pub fn none() -> Humanize {
        Humanize::new(1, 0.0, 0.0)
    }

    /// Uniform in -1..1.
    pub fn next(&mut self) -> f64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.state >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
    }

    fn apply(&mut self, start: f64, vel: f32) -> (f64, f32) {
        let t = start + self.next() * self.timing;
        let v = vel * (1.0 + self.next() as f32 * self.velocity);
        (t.max(0.0), v.clamp(0.05, 1.0))
    }
}

/// Places one hit per 16th-note step of `pattern` (`X` accent, `x` normal,
/// `o` ghost, anything else rest; `|` and spaces are ignored), repeated
/// for `bars` bars from bar `bar`. `swing` delays every second 16th by that
/// fraction of a 16th.
pub fn drums(part: &mut Part, key: f32, pattern: &str, bar: f64, bars: usize, swing: f64, h: &mut Humanize) {
    let steps: Vec<char> = pattern.chars().filter(|c| *c != '|' && *c != ' ').collect();
    let n = steps.len().max(1);
    let step_beats = (BAR * if n > 16 { (n / 16) as f64 } else { 1.0 }) / n as f64;
    let pattern_bars = if n > 16 { n / 16 } else { 1 };
    let mut b = 0;
    while b < bars {
        for (i, c) in steps.iter().enumerate() {
            let vel = match c {
                'X' => 1.0,
                'x' => 0.82,
                'o' => 0.45,
                _ => continue,
            };
            let mut t = (bar + b as f64) * BAR + i as f64 * step_beats;
            if i % 2 == 1 {
                t += swing * step_beats;
            }
            let (t, v) = h.apply(t, vel);
            part.note(t, step_beats, key, v);
        }
        b += pattern_bars;
    }
}

/// Parses a melody: tokens `NOTE/BEATS` (`C5/1`, `F#4/0.5`), `-/BEATS`
/// for rests, optional `!` suffix for an accent and `~` for a softer note.
/// Places it from beat `start`; returns the beat after the last token.
pub fn melody(part: &mut Part, start: f64, text: &str, vel: f32, h: &mut Humanize) -> f64 {
    let mut t = start;
    for tok in text.split_whitespace() {
        let (body, mut v) = if let Some(b) = tok.strip_suffix('!') {
            (b, (vel * 1.18).min(1.0))
        } else if let Some(b) = tok.strip_suffix('~') {
            (b, vel * 0.7)
        } else {
            (tok, vel)
        };
        let Some((name, len)) = body.split_once('/') else { continue };
        let len: f64 = len.parse().unwrap_or(1.0);
        if name != "-"
            && let Some(p) = note(name)
        {
            let (tt, vv) = h.apply(t, v);
            v = vv;
            // A hair shorter than the slot keeps repeated notes distinct.
            part.note(tt, len * 0.94, p, v);
        }
        t += len;
    }
    t
}

/// Shifts a melody text by `semis` semitones (for harmonies and octaves).
pub fn transpose(text: &str, semis: i32) -> String {
    let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    text.split_whitespace()
        .map(|tok| {
            let (body, suffix) = match tok.char_indices().last() {
                Some((i, c)) if c == '!' || c == '~' => (&tok[..i], &tok[i..]),
                _ => (tok, ""),
            };
            match body.split_once('/') {
                Some((n, len)) if n != "-" => match note(n) {
                    Some(p) => {
                        let q = p as i32 + semis;
                        format!("{}{}/{}{}", names[q.rem_euclid(12) as usize], q.div_euclid(12) - 1, len, suffix)
                    }
                    None => tok.to_string(),
                },
                _ => tok.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sustained chords: every symbol of `chords` lasts `beats`, voiced near
/// `center` (MIDI).
pub fn pads(part: &mut Part, start: f64, chords: &[&str], beats: f64, center: f32, vel: f32) {
    for (i, sym) in chords.iter().enumerate() {
        for p in theory::voiced(sym, center) {
            part.note(start + i as f64 * beats, beats, p, vel);
        }
    }
}

/// Chords played with a rhythm: `pattern` has one character per 16th
/// (`x` hit, `-` holds the previous hit, anything else rest), repeated over
/// each chord's `beats`.
pub fn comp(
    part: &mut Part,
    start: f64,
    chords: &[&str],
    beats: f64,
    pattern: &str,
    center: f32,
    vel: f32,
    swing: f64,
    h: &mut Humanize,
) {
    let steps: Vec<char> = pattern.chars().filter(|c| *c != '|' && *c != ' ').collect();
    let step = 0.25;
    for (ci, sym) in chords.iter().enumerate() {
        let tones = theory::voiced(sym, center);
        let base = start + ci as f64 * beats;
        let nsteps = (beats / step) as usize;
        let mut i = 0;
        while i < nsteps {
            let c = steps[i % steps.len()];
            if c == 'x' || c == 'X' {
                let mut len = 1;
                while i + len < nsteps && steps[(i + len) % steps.len()] == '-' {
                    len += 1;
                }
                let mut t = base + i as f64 * step;
                if i % 2 == 1 {
                    t += swing * step;
                }
                let accent = if c == 'X' { 1.1 } else { 1.0 };
                for (k, &p) in tones.iter().enumerate() {
                    // A slight strum spreads the notes.
                    let (tt, vv) = h.apply(t + k as f64 * 0.006, vel * accent);
                    part.note(tt, len as f64 * step * 0.95, p, vv);
                }
                i += len;
            } else {
                i += 1;
            }
        }
    }
}

/// An arpeggio over each chord: `pattern` lists chord-tone indices
/// (0 = lowest; indices past the chord climb by octaves), one per `step`
/// beats.
pub fn arp(
    part: &mut Part,
    start: f64,
    chords: &[&str],
    beats: f64,
    step: f64,
    pattern: &[usize],
    center: f32,
    vel: f32,
    h: &mut Humanize,
) {
    for (ci, sym) in chords.iter().enumerate() {
        let tones = theory::voiced(sym, center);
        if tones.is_empty() {
            continue;
        }
        let n = (beats / step).round() as usize;
        for i in 0..n {
            let idx = pattern[i % pattern.len()];
            let p = tones[idx % tones.len()] + 12.0 * (idx / tones.len()) as f32;
            let accent = if i % 4 == 0 { 1.0 } else { 0.85 };
            let (t, v) = h.apply(start + ci as f64 * beats + i as f64 * step, vel * accent);
            part.note(t, step * 0.9, p, v);
        }
    }
}

/// A bassline: per 16th step, `R` root, `r` root an octave up, `5` fifth,
/// `8` octave, `b` flat seventh, `3` third (minor or major per chord),
/// `-` holds, anything else rest. Repeats over each chord.
pub fn bass(
    part: &mut Part,
    start: f64,
    chords: &[&str],
    beats: f64,
    pattern: &str,
    octave: i32,
    vel: f32,
    swing: f64,
    h: &mut Humanize,
) {
    let steps: Vec<char> = pattern.chars().filter(|c| *c != '|' && *c != ' ').collect();
    for (ci, sym) in chords.iter().enumerate() {
        let Some((root_pc, quality)) = theory::parse_chord(sym) else { continue };
        let root = (12 * (octave + 1) + root_pc) as f32;
        let minor = quality.starts_with('m') && !quality.starts_with("maj");
        let n = (beats / 0.25) as usize;
        let mut i = 0;
        while i < n {
            let c = steps[i % steps.len()];
            let interval = match c {
                'R' => Some(0.0),
                'r' | '8' => Some(12.0),
                '5' => Some(7.0),
                'b' => Some(10.0),
                '3' => Some(if minor { 3.0 } else { 4.0 }),
                _ => None,
            };
            if let Some(iv) = interval {
                let mut len = 1;
                while i + len < n && steps[(i + len) % steps.len()] == '-' {
                    len += 1;
                }
                let mut t = start + ci as f64 * beats + i as f64 * 0.25;
                if i % 2 == 1 {
                    t += swing * 0.25;
                }
                let accent = if i % 4 == 0 { 1.0 } else { 0.88 };
                let (tt, vv) = h.apply(t, vel * accent);
                part.note(tt, len as f64 * 0.25 * 0.92, root + iv, vv);
                i += len;
            } else {
                i += 1;
            }
        }
    }
}

/// Repeats a chord list `times` times.
pub fn repeat<'a>(chords: &[&'a str], times: usize) -> Vec<&'a str> {
    let mut v = Vec::new();
    for _ in 0..times {
        v.extend_from_slice(chords);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaudio::synth::{Kit, Part};

    #[test]
    fn patterns_and_melodies() {
        let mut p = Part::new("t", Box::new(Kit::default()));
        let mut h = Humanize::none();
        drums(&mut p, key::KICK, "x...x...x...x...", 0.0, 2, 0.0, &mut h);
        assert_eq!(p.notes.len(), 8);
        assert_eq!(p.notes[1].start, 1.0);
        let end = melody(&mut p, 0.0, "C4/1 -/0.5 E4/0.5! G4/2~", 0.8, &mut h);
        assert_eq!(end, 4.0);
        assert_eq!(transpose("C4/1 F#4/0.5! -/1", 12), "C5/1 F#5/0.5! -/1");
        assert_eq!(transpose("B4/1", 1), "C5/1");
    }
}
