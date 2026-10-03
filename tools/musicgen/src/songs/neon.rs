//! "Neon Horizon" — synthwave in A minor, 100 BPM.
//!
//! Intro (8) · verse (8) · chorus (8) · breakdown (4) · chorus (8) · outro (8)

use vaudio::synth::{Part, Song};

use super::Track;
use crate::dsl::{self, BAR, Humanize, key};
use crate::patches::{self, SR};

const VERSE: &[&str] = &["Am", "F", "C", "G"];
const CHORUS: &[&str] = &["F", "G", "Am", "Am", "F", "G", "C", "E"];
const BREAK: &[&str] = &["Dm", "Am", "F", "G"];

const MELODY_VERSE: &str = "A4/0.5 C5/0.5 E5/1! D5/0.5 C5/0.5 D5/1 \
    C5/1.5 A4/0.5 F4/2 \
    G4/0.5 A4/0.5 C5/1 E5/1 G5/1! \
    F5/1 E5/0.5 D5/0.5 B4/2 \
    A4/0.5 C5/0.5 E5/1! D5/0.5 C5/0.5 D5/1 \
    C5/1 A4/1 C5/1 F5/1! \
    E5/1.5 D5/0.5 C5/1 G4/1 \
    B4/1 D5/1 G4/2";

const HOOK: &str = "C5/0.5 F5/1 G5/0.5 A5/2! \
    G5/1.5 F5/0.5 D5/2 \
    E5/1 A5/1 C6/1! B5/0.5 A5/0.5 \
    E5/3.5 -/0.5 \
    C5/0.5 F5/1 G5/0.5 A5/2! \
    B5/1 D6/1! C6/1 B5/1 \
    C6/1.5 B5/0.5 G5/1 E5/1 \
    G#5/2! B5/1 E5/1";

const BELLS_BREAK: &str = "A5/1 F5/1 D5/2 E5/1 C5/1 A4/2 C5/1 F5/1 A5/2 B5/1 D6/1 G5/2";

pub fn track() -> Track {
    let bars = 44.0;
    let mut song = Song::new(100.0, SR, bars * BAR);
    song.reverb = (0.78, 0.45, 1.0, 0.025);
    song.reverb_return = 0.9;
    song.delay_beats = 0.75;
    song.delay_feedback = 0.38;
    song.duck_release = 0.22;
    song.tail = 4.0;
    song.fade_out = 7.0;
    song.master.low_shelf = (90.0, 1.0);
    song.master.high_shelf = (10_000.0, 0.0);
    let mut h = Humanize::new(11, 0.0, 0.04);

    // Drums.
    let mut dr = Part::new("drums", Box::new(patches::kit_80s()));
    dsl::drums(&mut dr, key::HAT, "x.o.x.o.x.o.x.o.", 4.0, 4, 0.0, &mut h);
    dr.note(6.0 * BAR, 8.0, key::FX, 0.9);
    for (start, n) in [(8.0, 7usize), (36.0, 4)] {
        dsl::drums(&mut dr, key::KICK, "X.......x.....x.", start, n, 0.0, &mut h);
        dsl::drums(&mut dr, key::SNARE, "....X.......X...", start, n, 0.0, &mut h);
        dsl::drums(&mut dr, key::HAT, "x.x.x.x.x.x.x.x.", start, n, 0.0, &mut h);
    }
    // Fill into the chorus.
    dsl::drums(&mut dr, key::KICK, "X.......x.......", 15.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "....X.......xxXX", 15.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::HAT, "x.x.x.x.x.x.....", 15.0, 1, 0.0, &mut h);
    for start in [16.0, 28.0] {
        dsl::drums(&mut dr, key::KICK, "X...x...X...x...", start, 8, 0.0, &mut h);
        dsl::drums(&mut dr, key::SNARE, "....X.......X...", start, 7, 0.0, &mut h);
        dsl::drums(&mut dr, key::HAT, "xoxoxoxoxoxoxoxo", start, 8, 0.0, &mut h);
        dsl::drums(&mut dr, key::OPEN_HAT, "..x...x...x...x.", start, 8, 0.0, &mut h);
        dsl::drums(&mut dr, key::SNARE, "....X.......X.xx", start + 7.0, 1, 0.0, &mut h);
        dr.note(start * BAR, 4.0, key::CRASH, 1.0);
    }
    // Tom fill closing the first chorus, riser and snare roll in the break.
    dsl::drums(&mut dr, key::TOM_HI, "........x.x.....", 23.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::TOM_LO, "............x.x.", 23.0, 1, 0.0, &mut h);
    dr.note(26.0 * BAR, 8.0, key::FX, 1.0);
    dsl::drums(&mut dr, key::SNARE, "o.o.o.o.x.x.x.x.", 26.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "xxxxxxxxXXXXXXXX", 27.0, 1, 0.0, &mut h);
    dr.note(8.0 * BAR, 4.0, key::CRASH, 0.9);
    dr.note(36.0 * BAR, 4.0, key::CRASH, 0.9);
    dsl::drums(&mut dr, key::KICK, "X.......X.......", 40.0, 3, 0.0, &mut h);
    dsl::drums(&mut dr, key::HAT, "x.o.x.o.x.o.x.o.", 40.0, 3, 0.0, &mut h);
    dr.note(43.0 * BAR, 4.0, key::CRASH, 0.8);
    dr.reverb = 0.22;
    dr.gain = 1.0;
    song.duck_times = super::hits_of(&dr, key::KICK);

    // Bass.
    let mut bass = Part::new("bass", Box::new(patches::synth_bass(520.0)));
    let groove = "R-r-R-r-R-r-R-r-";
    dsl::bass(&mut bass, 8.0 * BAR, &dsl::repeat(VERSE, 2), BAR, groove, 2, 0.9, 0.0, &mut h);
    dsl::bass(&mut bass, 16.0 * BAR, CHORUS, BAR, groove, 2, 0.95, 0.0, &mut h);
    dsl::bass(&mut bass, 24.0 * BAR, BREAK, BAR, "R---------------", 2, 0.7, 0.0, &mut h);
    dsl::bass(&mut bass, 28.0 * BAR, CHORUS, BAR, groove, 2, 0.95, 0.0, &mut h);
    dsl::bass(&mut bass, 36.0 * BAR, &dsl::repeat(VERSE, 2), BAR, groove, 2, 0.85, 0.0, &mut h);
    bass.duck = 0.3;
    bass.gain = 1.0;

    // Arpeggio: 16ths, filter opening through the intro.
    let mut arp = Part::new("arp", Box::new(patches::pluck_synth(950.0, 0.2)));
    let pat = [0usize, 1, 2, 3, 2, 1, 2, 3];
    dsl::arp(&mut arp, 2.0 * BAR, &dsl::repeat(VERSE, 2)[2..], BAR, 0.25, &pat, 64.0, 0.75, &mut h);
    dsl::arp(&mut arp, 8.0 * BAR, &dsl::repeat(VERSE, 2), BAR, 0.25, &pat, 64.0, 0.8, &mut h);
    dsl::arp(&mut arp, 16.0 * BAR, CHORUS, BAR, 0.25, &pat, 66.0, 0.8, &mut h);
    dsl::arp(&mut arp, 24.0 * BAR, BREAK, BAR, 0.5, &[0, 1, 2, 3], 64.0, 0.6, &mut h);
    dsl::arp(&mut arp, 28.0 * BAR, CHORUS, BAR, 0.25, &pat, 66.0, 0.8, &mut h);
    dsl::arp(&mut arp, 36.0 * BAR, &dsl::repeat(VERSE, 2), BAR, 0.25, &pat, 64.0, 0.75, &mut h);
    arp.lowpass = vec![
        (2.0 * BAR, 350.0),
        (8.0 * BAR, 7000.0),
        (24.0 * BAR, 7000.0),
        (26.0 * BAR, 1200.0),
        (28.0 * BAR, 8000.0),
        (40.0 * BAR, 6000.0),
        (44.0 * BAR, 500.0),
    ];
    arp.pan = 0.3;
    arp.delay = 0.3;
    arp.reverb = 0.18;
    arp.duck = 0.2;
    arp.gain = 1.25;

    // Pads.
    let mut pad = Part::new("pad", Box::new(patches::supersaw_pad(2200.0)));
    dsl::pads(&mut pad, 0.0, &dsl::repeat(VERSE, 4), BAR, 62.0, 0.8);
    dsl::pads(&mut pad, 16.0 * BAR, CHORUS, BAR, 64.0, 0.85);
    dsl::pads(&mut pad, 24.0 * BAR, BREAK, BAR, 62.0, 0.8);
    dsl::pads(&mut pad, 28.0 * BAR, CHORUS, BAR, 64.0, 0.85);
    dsl::pads(&mut pad, 36.0 * BAR, &dsl::repeat(VERSE, 2), BAR, 62.0, 0.75);
    pad.lowpass = vec![(0.0, 300.0), (8.0 * BAR, 5000.0), (40.0 * BAR, 5000.0), (44.0 * BAR, 400.0)];
    pad.chorus = Some((0.4, 3.0, 0.4));
    pad.reverb = 0.35;
    pad.duck = 0.4;
    pad.gain = 0.75;

    // Lead.
    let mut lead = Part::new("lead", Box::new(patches::saw_lead(3400.0)));
    dsl::melody(&mut lead, 8.0 * BAR, MELODY_VERSE, 0.8, &mut h);
    dsl::melody(&mut lead, 16.0 * BAR, HOOK, 0.85, &mut h);
    dsl::melody(&mut lead, 28.0 * BAR, HOOK, 0.88, &mut h);
    dsl::melody(&mut lead, 36.0 * BAR, "A4/0.5 C5/0.5 E5/1 D5/0.5 C5/0.5 D5/1 C5/1.5 A4/0.5 F4/2", 0.7, &mut h);
    lead.delay = 0.28;
    lead.reverb = 0.28;
    lead.gain = 1.6;

    // Bells: a motif in the break and a shimmering double of the last hook.
    let mut bells = Part::new("bells", Box::new(patches::bell(3.5, 1.6)));
    dsl::melody(&mut bells, 24.0 * BAR, BELLS_BREAK, 0.7, &mut h);
    dsl::melody(&mut bells, 28.0 * BAR, HOOK, 0.55, &mut h);
    bells.pan = -0.25;
    bells.delay = 0.35;
    bells.reverb = 0.45;
    bells.gain = 0.7;

    song.parts = vec![dr, bass, arp, pad, lead, bells];
    Track { number: 1, title: "Neon Horizon", genre: "Synthwave", art: "synthwave:300:7", bpm: 100, song }
}
