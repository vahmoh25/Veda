//! "Pixel Quest" — chiptune in C major, 150 BPM.
//!
//! Intro (4) · theme (16) · bridge (8) · cave (8) · theme with harmony
//! (16) · fanfare (8). Two pulse channels, a triangle bass and noise drums.

use vaudio::synth::{Part, Song};

use super::Track;
use crate::dsl::{self, BAR, Humanize, key};
use crate::patches::{self, SR};

const THEME_CHORDS: &[&str] = &["C", "G", "Am", "F", "C", "G", "Am", "F"];
const THEME_CHORDS_2: &[&str] = &["C", "G", "Am", "F", "C", "G", "F", "G"];
const BRIDGE: &[&str] = &["Dm", "G", "Em", "Am", "Dm", "G", "F", "G"];
const CAVE: &[&str] = &["Am", "F", "G", "Em", "Am", "F", "G", "E"];
const CODA: &[&str] = &["F", "G", "C", "Am", "F", "G", "C", "C"];

const THEME: &str = "E5/0.5 G5/0.5 C6/1! B5/0.5 G5/0.5 E5/1 \
    D5/0.5 G5/0.5 B5/1! A5/0.5 G5/0.5 D5/1 \
    C5/0.5 E5/0.5 A5/1! G5/0.5 E5/0.5 C5/0.5 E5/0.5 \
    F5/1.5 E5/0.5 D5/1 C5/1 \
    E5/0.5 G5/0.5 C6/1! D6/0.5 C6/0.5 G5/1 \
    B5/0.5 A5/0.5 G5/1 D6/1! B5/1";
const THEME_END_1: &str = "C6/1 B5/0.5 A5/0.5 E5/1 A5/1 F5/1 A5/1 G5/2";
const THEME_END_2: &str = "A5/1 C6/1 F6/1! E6/1 D6/2 B5/1 G5/1";

const BRIDGE_CALL: &str = "F5/0.5 A5/0.5 D6/1! -/2 G5/0.5 B5/0.5 D6/1! -/2 E5/0.5 G5/0.5 B5/1! -/2 A5/1 C6/1 E6/2! \
    F5/0.5 A5/0.5 D6/1! -/2 G5/0.5 B5/0.5 D6/1! -/2 A5/1 C6/1 F6/1! E6/1 D6/1 B5/1 G5/1 D5/1";
const BRIDGE_ANSWER: &str = "-/2 A5/0.5 F5/0.5 D5/1 -/2 B5/0.5 G5/0.5 D5/1 -/2 G5/0.5 E5/0.5 B4/1 -/4 \
    -/2 A5/0.5 F5/0.5 D5/1 -/2 D6/0.5 B5/0.5 G5/1";

const CAVE_MELODY: &str = "A4/1 E5/1 A5/1! G5/0.5 E5/0.5 \
    F5/1.5 E5/0.5 C5/1 A4/1 \
    B4/0.5 D5/0.5 G5/1 F5/0.5 D5/0.5 B4/1 \
    E5/3! -/1 \
    A5/1 C6/1 E6/1! D6/0.5 C6/0.5 \
    C6/1.5 A5/0.5 F5/1 A5/1 \
    B5/0.5 C6/0.5 D6/1 B5/0.5 G5/0.5 D5/1 \
    G#5/2! B5/1 E6/1";

const FANFARE: &str = "A5/1 C6/1 A5/1 F5/1 B5/1 D6/1 B5/1 G5/1 C6/1 E6/1 G6/2! E6/1 C6/1 A5/2 \
    F5/0.5 A5/0.5 C6/1 F6/2! G5/0.5 B5/0.5 D6/1 G6/2! \
    C6/0.25 E6/0.25 G6/0.25 C7/0.25 -/0.5 G6/0.5 C7/2! -/4";

pub fn track() -> Track {
    let bars = 60.0;
    let mut song = Song::new(150.0, SR, bars * BAR);
    song.reverb = (0.45, 0.5, 0.8, 0.01);
    song.reverb_return = 0.6;
    song.delay_beats = 0.75;
    song.delay_feedback = 0.25;
    song.tail = 2.5;
    song.master.target_rms_db = -15.5;
    song.master.low_shelf = (100.0, 2.0);
    let mut h = Humanize::none();

    // Lead pulse (12.5%/25% duty) and second pulse.
    let mut p1 = Part::new("pulse1", Box::new(patches::chip_pulse(0.25, -0.15)));
    dsl::melody(&mut p1, 4.0 * BAR, THEME, 0.9, &mut h);
    dsl::melody(&mut p1, 10.0 * BAR, THEME_END_1, 0.9, &mut h);
    dsl::melody(&mut p1, 12.0 * BAR, THEME, 0.9, &mut h);
    dsl::melody(&mut p1, 18.0 * BAR, THEME_END_2, 0.9, &mut h);
    dsl::melody(&mut p1, 20.0 * BAR, BRIDGE_CALL, 0.9, &mut h);
    dsl::melody(&mut p1, 28.0 * BAR, CAVE_MELODY, 0.9, &mut h);
    dsl::melody(&mut p1, 36.0 * BAR, THEME, 0.95, &mut h);
    dsl::melody(&mut p1, 42.0 * BAR, THEME_END_1, 0.95, &mut h);
    dsl::melody(&mut p1, 44.0 * BAR, THEME, 0.95, &mut h);
    dsl::melody(&mut p1, 50.0 * BAR, THEME_END_2, 0.95, &mut h);
    dsl::melody(&mut p1, 52.0 * BAR, FANFARE, 0.95, &mut h);
    p1.delay = 0.12;
    p1.reverb = 0.12;
    p1.gain = 1.78;

    let mut p2 = Part::new("pulse2", Box::new(patches::chip_pulse(0.125, 0.2)));
    let arp = [0usize, 1, 2, 1];
    dsl::arp(&mut p2, 0.0, &THEME_CHORDS[..4], BAR, 0.25, &[0, 1, 2, 3], 67.0, 0.7, &mut h);
    dsl::arp(&mut p2, 4.0 * BAR, THEME_CHORDS, BAR, 0.5, &arp, 64.0, 0.55, &mut h);
    dsl::arp(&mut p2, 12.0 * BAR, THEME_CHORDS_2, BAR, 0.5, &arp, 64.0, 0.55, &mut h);
    dsl::melody(&mut p2, 20.0 * BAR, BRIDGE_ANSWER, 0.75, &mut h);
    dsl::arp(&mut p2, 26.0 * BAR, &BRIDGE[6..], BAR, 0.25, &arp, 64.0, 0.55, &mut h);
    dsl::arp(&mut p2, 28.0 * BAR, CAVE, BAR, 0.25, &[0, 1, 2, 1, 3, 1, 2, 1], 62.0, 0.55, &mut h);
    // The theme returns with an octave-down double.
    dsl::melody(&mut p2, 36.0 * BAR, &dsl::transpose(THEME, -12), 0.6, &mut h);
    dsl::melody(&mut p2, 42.0 * BAR, &dsl::transpose(THEME_END_1, -12), 0.6, &mut h);
    dsl::melody(&mut p2, 44.0 * BAR, &dsl::transpose(THEME, -12), 0.6, &mut h);
    dsl::melody(&mut p2, 50.0 * BAR, &dsl::transpose(THEME_END_2, -12), 0.6, &mut h);
    dsl::arp(&mut p2, 52.0 * BAR, &CODA[..6], BAR, 0.25, &[0, 1, 2, 3], 64.0, 0.55, &mut h);
    p2.reverb = 0.12;
    p2.gain = 1.2;

    // Triangle bass.
    let mut tri = Part::new("triangle", Box::new(patches::chip_triangle()));
    let bounce = "R-r-R-r-R-r-R-r-";
    dsl::bass(&mut tri, 2.0 * BAR, &THEME_CHORDS[2..4], BAR, bounce, 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 4.0 * BAR, THEME_CHORDS, BAR, bounce, 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 12.0 * BAR, THEME_CHORDS_2, BAR, bounce, 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 20.0 * BAR, BRIDGE, BAR, "R---5---r---5---", 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 28.0 * BAR, CAVE, BAR, "R-R-r-R-R-R-r-R-", 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 36.0 * BAR, THEME_CHORDS, BAR, bounce, 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 44.0 * BAR, THEME_CHORDS_2, BAR, bounce, 2, 1.0, 0.0, &mut h);
    dsl::bass(&mut tri, 52.0 * BAR, &CODA[..7], BAR, bounce, 2, 1.0, 0.0, &mut h);
    tri.note(59.0 * BAR, 3.0, 36.0, 1.0);
    tri.gain = 1.0;

    // Noise drums.
    let mut dr = Part::new("drums", Box::new(patches::kit_chip()));
    dsl::drums(&mut dr, key::SNARE, "........x.x.xxxx", 3.0, 1, 0.0, &mut h);
    for (start, n) in [(4.0, 16usize), (36.0, 16)] {
        dsl::drums(&mut dr, key::KICK, "X.......X.x.....", start, n, 0.0, &mut h);
        dsl::drums(&mut dr, key::SNARE, "....X.......X...", start, n, 0.0, &mut h);
        dsl::drums(&mut dr, key::HAT, "x.x.x.x.x.x.x.x.", start, n, 0.0, &mut h);
        dr.note(start * BAR, 1.0, key::CRASH, 0.8);
    }
    dsl::drums(&mut dr, key::KICK, "X...X...X...X...", 20.0, 8, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "....X.......X...", 20.0, 8, 0.0, &mut h);
    dsl::drums(&mut dr, key::OPEN_HAT, "..x...x...x...x.", 20.0, 8, 0.0, &mut h);
    dsl::drums(&mut dr, key::KICK, "X.x...x.X.x...x.", 28.0, 8, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "....X.......X..x", 28.0, 8, 0.0, &mut h);
    dsl::drums(&mut dr, key::HAT, "xxxxxxxxxxxxxxxx", 28.0, 8, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "xxxxxxxxXXXXXXXX", 35.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::KICK, "X...X...X...X...", 52.0, 6, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "....X.......X...", 52.0, 6, 0.0, &mut h);
    dsl::drums(&mut dr, key::HAT, "x.x.x.x.x.x.x.x.", 52.0, 6, 0.0, &mut h);
    dr.note(52.0 * BAR, 1.0, key::CRASH, 0.8);
    dr.note(58.0 * BAR, 1.0, key::CRASH, 1.0);
    dr.note(58.0 * BAR, 1.0, key::KICK, 1.0);
    dr.reverb = 0.06;
    dr.gain = 0.8;

    song.parts = vec![p1, p2, tri, dr];
    Track { number: 4, title: "Pixel Quest", genre: "Chiptune", art: "pixel:200:5", bpm: 150, song }
}
