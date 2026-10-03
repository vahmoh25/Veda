//! "Skyline Run" — uplifting house in G minor / B-flat major, 124 BPM.
//!
//! Intro (8) · groove (8) · drop (16) · breakdown (8) · drop (16) · outro (4)

use vaudio::synth::{Adsr, Fm, Part, Song};

use super::Track;
use crate::dsl::{self, BAR, Humanize, key};
use crate::patches::{self, SR};

const PROG: &[&str] = &["Gm", "Eb", "Bb", "F"];

const HOOK: &str = "G5/0.75 F5/0.75 D5/0.5 F5/0.75 G5/0.75 Bb5/0.5! \
    G5/1.5 F5/0.5 Eb5/1 D5/1 \
    F5/0.75 D5/0.75 Bb4/0.5 D5/0.75 F5/0.75 Bb5/0.5! \
    A5/1.5 G5/0.5 F5/1 C5/1";
const HOOK_TURN: &str = "G5/0.75 F5/0.75 D5/0.5 F5/0.75 G5/0.75 Bb5/0.5! \
    G5/1.5 F5/0.5 Eb5/1 D5/1 \
    F5/0.75 D5/0.75 Bb4/0.5 D5/0.75 F5/0.75 Bb5/0.5! \
    A5/1 C6/1! A5/1 F5/1";

/// A bright house piano.
fn piano() -> Fm {
    Fm {
        ratio: 1.0,
        index: 2.4,
        index_decay: 0.35,
        index_sustain: 0.3,
        tine_ratio: 4.0,
        tine: 0.6,
        amp: Adsr::new(0.002, 1.2, 0.0, 0.2),
        vel_index: 0.6,
        detune: 7.0,
        tremolo: (0.0, 0.0),
        gain: 0.2,
    }
}

pub fn track() -> Track {
    let bars = 60.0;
    let mut song = Song::new(124.0, SR, bars * BAR);
    song.reverb = (0.7, 0.4, 1.0, 0.02);
    song.reverb_return = 0.75;
    song.delay_beats = 0.75;
    song.delay_feedback = 0.4;
    song.duck_release = 0.3;
    song.tail = 3.0;
    song.fade_out = 3.0;
    song.master.target_rms_db = -14.5;
    song.master.low_shelf = (80.0, 1.0);
    song.master.high_shelf = (11_000.0, 0.5);
    let mut h = Humanize::new(55, 0.0, 0.05);

    // Drums.
    let mut dr = Part::new("drums", Box::new(patches::kit_house()));
    let four = "X...x...X...x...";
    dsl::drums(&mut dr, key::KICK, four, 0.0, 32, 0.0, &mut h);
    dsl::drums(&mut dr, key::KICK, four, 40.0, 18, 0.0, &mut h);
    dsl::drums(&mut dr, key::HAT, "..x...x...x...x.", 0.0, 4, 0.0, &mut h);
    dsl::drums(&mut dr, key::OPEN_HAT, "..x...x...x...x.", 4.0, 28, 0.0, &mut h);
    dsl::drums(&mut dr, key::OPEN_HAT, "..x...x...x...x.", 40.0, 20, 0.0, &mut h);
    dsl::drums(&mut dr, key::CLAP, "....X.......X...", 8.0, 24, 0.0, &mut h);
    dsl::drums(&mut dr, key::CLAP, "....X.......X...", 40.0, 16, 0.0, &mut h);
    dsl::drums(&mut dr, key::SHAKER, "xoxoxoxoxoxoxoxo", 8.0, 24, 0.0, &mut h);
    dsl::drums(&mut dr, key::SHAKER, "xoxoxoxoxoxoxoxo", 40.0, 16, 0.0, &mut h);
    dsl::drums(&mut dr, key::HAT, "x.o.x.o.x.o.x.o.", 16.0, 16, 0.0, &mut h);
    dsl::drums(&mut dr, key::RIDE, "x...x...x...x...", 48.0, 8, 0.0, &mut h);
    for b in [8.0, 16.0, 24.0, 40.0, 48.0] {
        dr.note(b * BAR, 4.0, key::CRASH, 0.9);
    }
    // Breakdown: riser and snare roll.
    dr.note(36.0 * BAR, 16.0, key::FX, 1.0);
    dsl::drums(&mut dr, key::SNARE, "x...x...x...x...", 36.0, 2, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "x.x.x.x.x.x.x.x.", 38.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "xxxxxxxxXXXXXXXX", 39.0, 1, 0.0, &mut h);
    dsl::drums(&mut dr, key::SNARE, "............x.xX", 31.0, 1, 0.0, &mut h);
    dr.gain = 1.0;
    song.duck_times = super::hits_of(&dr, key::KICK);

    // Bass on the off-beats.
    let mut bass = Part::new("bass", Box::new(patches::synth_bass(700.0)));
    let off = "..R-..R-..R-..R-";
    dsl::bass(&mut bass, 4.0 * BAR, &dsl::repeat(PROG, 7), BAR, off, 2, 0.9, 0.0, &mut h);
    dsl::bass(&mut bass, 40.0 * BAR, &dsl::repeat(PROG, 4), BAR, off, 2, 0.95, 0.0, &mut h);
    bass.duck = 0.55;
    bass.gain = 1.05;

    // Piano stabs, filtered open over the intro.
    let mut pno = Part::new("piano", Box::new(piano()));
    let stabs = "x..x..x...x..x..";
    dsl::comp(&mut pno, 0.0, &dsl::repeat(PROG, 8), BAR, stabs, 65.0, 0.8, 0.0, &mut h);
    dsl::comp(&mut pno, 32.0 * BAR, &dsl::repeat(PROG, 2), BAR, "x-------x---x---", 65.0, 0.7, 0.0, &mut h);
    dsl::comp(&mut pno, 40.0 * BAR, &dsl::repeat(PROG, 5), BAR, stabs, 65.0, 0.85, 0.0, &mut h);
    pno.lowpass = vec![(0.0, 500.0), (8.0 * BAR, 9000.0), (56.0 * BAR, 9000.0), (60.0 * BAR, 600.0)];
    pno.duck = 0.35;
    pno.reverb = 0.22;
    pno.delay = 0.1;
    pno.gain = 0.9;

    // Pumping pad.
    let mut pad = Part::new("pad", Box::new(patches::supersaw_pad(2600.0)));
    dsl::pads(&mut pad, 8.0 * BAR, &dsl::repeat(PROG, 6), BAR, 62.0, 0.7);
    dsl::pads(&mut pad, 32.0 * BAR, &dsl::repeat(PROG, 7), BAR, 62.0, 0.8);
    pad.duck = 0.65;
    pad.reverb = 0.3;
    pad.chorus = Some((0.5, 2.5, 0.35));
    pad.gain = 0.7;
    pad.lowpass = vec![(8.0 * BAR, 1500.0), (16.0 * BAR, 4000.0), (32.0 * BAR, 2000.0), (39.5 * BAR, 6000.0)];

    // The hook.
    let mut lead = Part::new("lead", Box::new(patches::supersaw_lead(4200.0)));
    for (start, last) in [
        (16.0, false),
        (20.0, false),
        (24.0, false),
        (28.0, true),
        (40.0, false),
        (44.0, false),
        (48.0, false),
        (52.0, true),
    ] {
        dsl::melody(&mut lead, start * BAR, if last { HOOK_TURN } else { HOOK }, 0.85, &mut h);
    }
    lead.delay = 0.22;
    lead.reverb = 0.22;
    lead.duck = 0.2;
    lead.gain = 2.6;
    // An octave-up double in the second drop and a soft version in the
    // breakdown.
    let mut sparkle = Part::new("pluck", Box::new(patches::pluck_synth(2500.0, 0.25)));
    dsl::melody(&mut sparkle, 32.0 * BAR, HOOK, 0.6, &mut h);
    dsl::melody(&mut sparkle, 36.0 * BAR, HOOK, 0.7, &mut h);
    for start in [48.0, 52.0] {
        dsl::melody(&mut sparkle, start * BAR, &dsl::transpose(HOOK, 12), 0.5, &mut h);
    }
    sparkle.delay = 0.35;
    sparkle.reverb = 0.3;
    sparkle.pan = 0.25;
    sparkle.gain = 1.2;

    song.parts = vec![dr, bass, pno, pad, lead, sparkle];
    Track { number: 5, title: "Skyline Run", genre: "House", art: "pulse:280:9", bpm: 124, song }
}
