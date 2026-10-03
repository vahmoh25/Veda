//! Notes, chords and scales.

use alloc::vec::Vec;

use vmath::FloatExt;

/// Frequency of a MIDI note (A4 = 69 = 440 Hz; fractional notes allowed).
pub fn mtof(note: f32) -> f32 {
    440.0 * FloatExt::exp2((note - 69.0) / 12.0)
}

/// Parses a note name such as `C4`, `F#3`, `Bb2` or `E-1` (C4 = 60).
pub fn note(name: &str) -> Option<f32> {
    let b = name.as_bytes();
    let base = match b.first()?.to_ascii_uppercase() {
        b'C' => 0,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };
    let mut i = 1;
    let mut acc = 0i32;
    while i < b.len() && (b[i] == b'#' || b[i] == b'b') {
        acc += if b[i] == b'#' { 1 } else { -1 };
        i += 1;
    }
    let octave: i32 = name.get(i..)?.parse().ok()?;
    Some((12 * (octave + 1) + base + acc) as f32)
}

/// Semitone intervals of a chord quality (`""` major, `"m"`, `"7"`,
/// `"maj7"`, `"m7"`, `"m9"`, `"maj9"`, `"9"`, `"add9"`, `"sus2"`, `"sus4"`,
/// `"6"`, `"m6"`, `"dim"`, `"aug"`, `"m7b5"`, `"5"`).
pub fn intervals(quality: &str) -> &'static [i32] {
    match quality {
        "" | "maj" => &[0, 4, 7],
        "m" | "min" => &[0, 3, 7],
        "7" => &[0, 4, 7, 10],
        "maj7" => &[0, 4, 7, 11],
        "m7" => &[0, 3, 7, 10],
        "m9" => &[0, 3, 7, 10, 14],
        "maj9" => &[0, 4, 7, 11, 14],
        "9" => &[0, 4, 7, 10, 14],
        "add9" => &[0, 4, 7, 14],
        "madd9" => &[0, 3, 7, 14],
        "sus2" => &[0, 2, 7],
        "sus4" => &[0, 5, 7],
        "6" => &[0, 4, 7, 9],
        "m6" => &[0, 3, 7, 9],
        "dim" => &[0, 3, 6],
        "aug" => &[0, 4, 8],
        "m7b5" => &[0, 3, 6, 10],
        "5" => &[0, 7],
        _ => &[0, 4, 7],
    }
}

/// Parses a chord symbol (`Am7`, `F#maj7`, `Bbadd9`) into its root
/// pitch class (0..12) and quality.
pub fn parse_chord(sym: &str) -> Option<(i32, &str)> {
    let b = sym.as_bytes();
    let base = match b.first()?.to_ascii_uppercase() {
        b'C' => 0,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };
    let mut i = 1;
    let mut acc = 0;
    if i < b.len() && b[i] == b'#' {
        acc = 1;
        i += 1;
    } else if i < b.len() && b[i] == b'b' {
        acc = -1;
        i += 1;
    }
    Some(((base + acc + 12) % 12, &sym[i..]))
}

/// The tones of a chord symbol with its root in octave `octave`
/// (`chord("Am7", 3)` = A3 C4 E4 G4).
pub fn chord(sym: &str, octave: i32) -> Vec<f32> {
    let Some((root, q)) = parse_chord(sym) else { return Vec::new() };
    let r = 12 * (octave + 1) + root;
    intervals(q).iter().map(|i| (r + i) as f32).collect()
}

/// A chord voiced close to `center` (MIDI): each tone moved by octaves to
/// lie within about half an octave of it — smooth voice leading.
pub fn voiced(sym: &str, center: f32) -> Vec<f32> {
    let mut v: Vec<f32> = chord(sym, 4)
        .into_iter()
        .map(|mut n| {
            while n > center + 6.0 {
                n -= 12.0;
            }
            while n < center - 6.0 {
                n += 12.0;
            }
            n
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    v
}

/// The root of a chord symbol in octave `octave`.
pub fn root(sym: &str, octave: i32) -> f32 {
    parse_chord(sym).map(|(r, _)| (12 * (octave + 1) + r) as f32).unwrap_or(48.0)
}

/// Scale degrees (semitones) of common scales.
pub fn scale(name: &str) -> &'static [i32] {
    match name {
        "minor" | "aeolian" => &[0, 2, 3, 5, 7, 8, 10],
        "dorian" => &[0, 2, 3, 5, 7, 9, 10],
        "pentatonic" => &[0, 2, 4, 7, 9],
        "minor_pentatonic" => &[0, 3, 5, 7, 10],
        "lydian" => &[0, 2, 4, 6, 7, 9, 11],
        "mixolydian" => &[0, 2, 4, 5, 7, 9, 10],
        _ => &[0, 2, 4, 5, 7, 9, 11],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_chords() {
        assert_eq!(note("C4"), Some(60.0));
        assert_eq!(note("A4"), Some(69.0));
        assert_eq!(note("F#3"), Some(54.0));
        assert_eq!(note("Bb2"), Some(46.0));
        assert_eq!(note("C-1"), Some(0.0));
        assert!((mtof(69.0) - 440.0).abs() < 1e-3);
        assert!((mtof(57.0) - 220.0).abs() < 1e-3);
        assert_eq!(chord("Am7", 3), alloc::vec![57.0, 60.0, 64.0, 67.0]);
        assert_eq!(chord("F#m", 3), alloc::vec![54.0, 57.0, 61.0]);
        assert_eq!(root("Bbmaj7", 2), 46.0);
        let v = voiced("C", 62.0);
        assert!(v.iter().all(|&n| (56.0..=68.0).contains(&n)), "{v:?}");
    }
}
