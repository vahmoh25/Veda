//! "Morning Compile" — sunny acoustic pop in G major, 104 BPM.
//!
//! Intro (4) · verse (8) · chorus (8) · verse (8) · bridge (8) · chorus
//! (8) · outro (4). Fingerpicked and strummed guitars, glockenspiel,
//! whistle, bass, shaker and claps.

use vaudio::synth::{Part, Song};

use super::Track;
use crate::dsl::{self, BAR, Humanize, key};
use crate::patches::{self, SR};

const VERSE: &[&str] = &["G", "D", "Em", "C"];
const CHORUS: &[&str] = &["C", "G", "Am", "D", "C", "G", "D", "D"];
const BRIDGE: &[&str] = &["Em", "C", "G", "D"];

const GLOCK_VERSE: &str = "B5/0.5 D6/0.5 B5/0.5 G5/0.5 A5/1 B5/1! \
    A5/0.5 F#5/0.5 D5/1 E5/1 F#5/1 \
    G5/1.5 E5/0.5 B5/1! G5/1 \
    E5/1 G5/1 C6/1 B5/1 \
    B5/0.5 D6/0.5 B5/0.5 G5/0.5 A5/1 B5/1! \
    D6/1 C6/0.5 B5/0.5 A5/2 \
    G5/1 B5/1 E6/1! D6/1 \
    C6/1.5 B5/0.5 A5/1 G5/1";

const HOOK: &str = "E5/1 G5/1 C6/1.5! B5/0.5 \
    B5/1 D6/1 G5/2 \
    A5/1 C6/1 E6/1! D6/0.5 C6/0.5 \
    D6/2 F#5/1 A5/1 \
    E6/1! D6/0.5 C6/0.5 G5/2 \
    B5/1 D6/1 G6/2! \
    F#6/1 E6/1 D6/1 A5/1 \
    D6/3 -/1";

const WHISTLE: &str = "B4/2 E5/1 G5/1 \
    G5/1.5 E5/0.5 C5/2 \
    D5/1 G5/1 B5/2! \
    A5/2 F#5/1 D5/1 \
    E5/1 G5/1 B5/1 E6/1! \
    C6/2 G5/1 E5/1 \
    D5/1 G5/1 B5/1 D6/1 \
    C6/2 A5/1 F#5/1";

pub fn track() -> Track {
    let bars = 48.0;
    let mut song = Song::new(104.0, SR, bars * BAR);
    song.reverb = (0.55, 0.55, 0.9, 0.012);
    song.reverb_return = 0.7;
    song.delay_beats = 0.5;
    song.delay_feedback = 0.25;
    song.tail = 4.0;
    song.fade_out = 2.0;
    song.master.target_rms_db = -15.5;
    song.master.high_shelf = (6000.0, -2.5);
    let mut h = Humanize::new(66, 0.008, 0.08);

    // Fingerpicked guitar: bass notes plus an 8th-note pattern.
    let mut pick = Part::new("guitar", Box::new(patches::guitar(0.45)));
    let travis = [0usize, 2, 1, 3, 0, 2, 1, 3];
    let picked: Vec<(f64, Vec<&str>)> = vec![
        (0.0, dsl::repeat(VERSE, 3)),
        (12.0, CHORUS.to_vec()),
        (20.0, dsl::repeat(VERSE, 2)),
        (28.0, dsl::repeat(BRIDGE, 2)),
        (36.0, CHORUS.to_vec()),
        (44.0, VERSE.to_vec()),
    ];
    for (start, chords) in &picked {
        dsl::arp(&mut pick, start * BAR, chords, BAR, 0.5, &travis, 60.0, 0.75, &mut h);
        dsl::bass(&mut pick, start * BAR, chords, BAR, "R-------5-------", 2, 0.85, 0.0, &mut h);
    }
    pick.pan = -0.3;
    pick.reverb = 0.2;
    pick.gain = 1.8;

    // Strummed guitar in the choruses (down-up folk rhythm).
    let mut strum = Part::new("strum", Box::new(patches::guitar(0.55)));
    for start in [12.0, 36.0] {
        dsl::comp(&mut strum, start * BAR, CHORUS, BAR, "x-..x-x-..x-x-x-", 60.0, 0.6, 0.0, &mut h);
    }
    strum.pan = 0.35;
    strum.reverb = 0.2;
    strum.gain = 0.87;

    // Bass.
    let mut bass = Part::new("bass", Box::new(patches::warm_bass()));
    let walk = "R-------5-----3-";
    dsl::bass(&mut bass, 4.0 * BAR, &dsl::repeat(VERSE, 2), BAR, walk, 2, 0.85, 0.0, &mut h);
    dsl::bass(&mut bass, 12.0 * BAR, CHORUS, BAR, "R---R---5---R---", 2, 0.9, 0.0, &mut h);
    dsl::bass(&mut bass, 20.0 * BAR, &dsl::repeat(VERSE, 2), BAR, walk, 2, 0.85, 0.0, &mut h);
    dsl::bass(&mut bass, 28.0 * BAR, &dsl::repeat(BRIDGE, 2), BAR, "R-----------5---", 2, 0.8, 0.0, &mut h);
    dsl::bass(&mut bass, 36.0 * BAR, CHORUS, BAR, "R---R---5---R---", 2, 0.9, 0.0, &mut h);
    dsl::bass(&mut bass, 44.0 * BAR, &VERSE[..3], BAR, walk, 2, 0.8, 0.0, &mut h);
    bass.note(47.0 * BAR, 4.0, 43.0, 0.85);
    bass.gain = 0.8;

    // Glockenspiel melodies.
    let mut glock = Part::new("glock", Box::new(patches::glock()));
    dsl::melody(&mut glock, 4.0 * BAR, GLOCK_VERSE, 0.75, &mut h);
    dsl::melody(&mut glock, 12.0 * BAR, HOOK, 0.8, &mut h);
    dsl::melody(&mut glock, 20.0 * BAR, GLOCK_VERSE, 0.7, &mut h);
    dsl::melody(&mut glock, 36.0 * BAR, HOOK, 0.85, &mut h);
    dsl::melody(
        &mut glock,
        44.0 * BAR,
        "B5/0.5 D6/0.5 B5/0.5 G5/0.5 A5/1 B5/1 A5/0.5 F#5/0.5 D5/1 E5/1 F#5/1 G5/4 -/4 G6/4!",
        0.6,
        &mut h,
    );
    glock.pan = 0.2;
    glock.reverb = 0.3;
    glock.delay = 0.15;
    glock.gain = 1.7;

    // Whistle: doubles the hook an octave down, leads the bridge.
    let mut whistle = Part::new("whistle", Box::new(patches::soft_lead()));
    dsl::melody(&mut whistle, 12.0 * BAR, &dsl::transpose(HOOK, -12), 0.6, &mut h);
    dsl::melody(&mut whistle, 28.0 * BAR, WHISTLE, 0.8, &mut h);
    dsl::melody(&mut whistle, 36.0 * BAR, &dsl::transpose(HOOK, -12), 0.65, &mut h);
    whistle.reverb = 0.3;
    whistle.delay = 0.12;
    whistle.gain = 1.1;

    // Percussion.
    let mut dr = Part::new("drums", Box::new(patches::kit_acoustic()));
    for (start, n, chorus) in
        [(4.0, 8usize, false), (12.0, 8, true), (20.0, 8, false), (28.0, 8, false), (36.0, 8, true)]
    {
        dsl::drums(&mut dr, key::KICK, "X.......x.x.....", start, n, 0.0, &mut h);
        dsl::drums(&mut dr, key::SHAKER, "x.o.x.o.x.o.x.o.", start, n, 0.0, &mut h);
        if chorus {
            dsl::drums(&mut dr, key::SNARE, "....X.......X...", start, n, 0.0, &mut h);
            dsl::drums(&mut dr, key::CLAP, "....X.......X...", start, n, 0.0, &mut h);
            dsl::drums(&mut dr, key::HAT, "..x...x...x...x.", start, n, 0.0, &mut h);
            dr.note(start * BAR, 4.0, key::CRASH, 0.7);
        } else {
            dsl::drums(&mut dr, key::RIM, "....x.......x...", start, n, 0.0, &mut h);
        }
    }
    dsl::drums(&mut dr, key::SNARE, "........x.x.xxxx", 11.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "........x.x.xxxx", 35.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::KICK, "X.......x.......", 44.0, 3, 0.0, &mut h);
    dr.note(47.0 * BAR, 4.0, key::CRASH, 0.6);
    dr.note(47.0 * BAR, 1.0, key::KICK, 0.9);
    dr.reverb = 0.12;
    dr.gain = 0.8;

    // A final strummed G chord.
    dsl::comp(&mut strum, 47.0 * BAR, &["G"], BAR, "x---------------", 60.0, 0.7, 0.0, &mut h);

    song.parts = vec![pick, strum, bass, glock, whistle, dr];
    Track { number: 6, title: "Morning Compile", genre: "Acoustic", art: "waves:35:12", bpm: 104, song }
}
