//! "Rainy Window" — lo-fi hip hop in F major, 78 BPM, swung.
//!
//! Intro (4) · A (8) · B (8) · A (8) · break (2) · outro (4)

use vaudio::synth::drums;
use vaudio::synth::{Bed, Part, Song};

use super::Track;
use crate::dsl::{self, BAR, Humanize, key};
use crate::patches::{self, SR};

/// Chords in half bars.
const A: &[&str] = &["Fmaj7", "Fmaj7", "Em7", "A7", "Dm7", "Dm7", "Gm7", "C7"];
const B: &[&str] = &["Bbmaj7", "Bbmaj7", "Am7", "Am7", "Gm9", "Gm9", "C9", "C9"];

const MELODY_A: &str = "C5/0.75 A4/0.25 C5/0.5 E5/1.5! -/1 \
    D5/0.5 E5/0.5 G5/1 C#5/1 E5/1 \
    F5/1.5! E5/0.5 D5/1 A4/1 \
    Bb4/1 D5/1 E5/1 G4/1 \
    A4/0.5 C5/0.5 E5/0.5 G5/0.5 A5/2! \
    G5/1 E5/1 E5/0.5 C#5/0.5 A4/1 \
    D5/1 F5/1 A5/1.5! G5/0.5 \
    F5/2 E5/1.5 -/0.5";

const MELODY_B: &str = "D5/1 F5/1 A5/2! \
    G5/1 E5/1 C5/2 \
    Bb4/1 D5/1 F5/1 A5/1 \
    G5/2! E5/1 D5/1 \
    F5/1 D5/1 A5/2 \
    E5/1.5 G5/0.5 C6/2! \
    A5/1 G5/1 F5/1 D5/1 \
    E5/3 -/1";

/// Rain on a window: filtered noise with a slow swell plus drops.
fn rain(secs: f32) -> Vec<f32> {
    let n = (secs * SR) as usize;
    let mut out = drums::crackle(secs, 1.0, 21, SR);
    let mut s: u32 = 0x1234_5678;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    let (mut lp_l, mut lp_r, mut hp_l, mut hp_r) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut drops: Vec<(usize, f32, f32, f32)> = Vec::new();
    for i in 0..n {
        let t = i as f32 / SR;
        let swell = 0.75 + 0.25 * (t * 0.13).sin() * (t * 0.07 + 1.0).sin();
        let (wl, wr) = (rnd(), rnd());
        lp_l += 0.25 * (wl - lp_l);
        lp_r += 0.25 * (wr - lp_r);
        hp_l += 0.02 * (lp_l - hp_l);
        hp_r += 0.02 * (lp_r - hp_r);
        let mut l = (lp_l - hp_l) * 0.05 * swell;
        let mut r = (lp_r - hp_r) * 0.05 * swell;
        if rnd() > 0.9993 {
            drops.push((i, 1800.0 + rnd().abs() * 2600.0, 0.5 + 0.5 * rnd(), 0.02 + 0.03 * rnd().abs()));
        }
        drops.retain(|&(start, f, pan, amp)| {
            let dt = (i - start) as f32 / SR;
            if dt > 0.05 {
                return false;
            }
            let v = (dt * f * std::f32::consts::TAU * (1.0 + dt * 8.0)).sin() * (-dt * 90.0).exp() * amp;
            l += v * (1.0 - pan * 0.5);
            r += v * (0.5 + pan * 0.5);
            true
        });
        out[i * 2] += l;
        out[i * 2 + 1] += r;
    }
    out
}

pub fn track() -> Track {
    let bars = 34.0;
    let mut song = Song::new(78.0, SR, bars * BAR);
    song.reverb = (0.62, 0.6, 0.9, 0.015);
    song.reverb_return = 0.8;
    song.delay_beats = 0.75;
    song.delay_feedback = 0.3;
    song.tail = 4.0;
    song.fade_in = 1.0;
    song.fade_out = 5.0;
    song.master.high_shelf = (6500.0, -2.5);
    song.master.low_shelf = (110.0, 1.5);
    song.master.target_rms_db = -17.0;
    song.master.comp_threshold = -19.0;
    let swing = 0.25;
    let mut h = Humanize::new(22, 0.012, 0.1);

    // Rain and crackle under everything.
    let total = (bars * BAR * 60.0 / 78.0) as f32 + 3.0;
    let mut amb = Part::new("rain", Box::new(Bed { data: rain(total), gain: 1.0 }));
    amb.note(0.0, bars * BAR + 4.0, 60.0, 1.0);
    amb.gain = 1.1;
    amb.volume = vec![(0.0, 1.0), (4.0 * BAR, 0.55), (28.0 * BAR, 0.55), (30.0 * BAR, 1.0)];

    // Drums.
    let mut dr = Part::new("drums", Box::new(patches::kit_lofi()));
    let sections = [(4.0, 8usize), (12.0, 8), (20.0, 8), (30.0, 3)];
    for (start, n) in sections {
        dsl::drums(&mut dr, key::KICK, "X......x..x.....", start, n, swing, &mut h);
        dsl::drums(&mut dr, key::SNARE, "....X.......X...", start, n, swing, &mut h);
        dsl::drums(&mut dr, key::HAT, "x.o.x.oox.o.x.ox", start, n, swing, &mut h);
        dsl::drums(&mut dr, key::RIM, "...........o....", start, n, swing, &mut h);
    }
    dsl::drums(&mut dr, key::SHAKER, "x.ox.ox.x.ox.oxo", 12.0, 8, swing, &mut h);
    dsl::drums(&mut dr, key::OPEN_HAT, "..............x.", 20.0, 8, swing, &mut h);
    dr.eq = vec![(vaudio::synth::BiquadKind::HighShelf(-4.0), 5000.0, 0.7)];
    dr.reverb = 0.1;
    dr.gain = 1.1;

    // Bass.
    let mut bass = Part::new("bass", Box::new(patches::warm_bass()));
    let line = "R-----.5";
    dsl::bass(&mut bass, 4.0 * BAR, &dsl::repeat(A, 2), 2.0, line, 2, 0.9, swing, &mut h);
    dsl::bass(&mut bass, 12.0 * BAR, &dsl::repeat(B, 2), 2.0, "R-----.3", 2, 0.9, swing, &mut h);
    dsl::bass(&mut bass, 20.0 * BAR, &dsl::repeat(A, 2), 2.0, line, 2, 0.9, swing, &mut h);
    dsl::bass(&mut bass, 30.0 * BAR, A, 2.0, "R-------", 2, 0.8, swing, &mut h);
    bass.gain = 1.1;

    // Electric piano comping (muffled in the intro and the break).
    let mut ep = Part::new("epiano", Box::new(patches::epiano()));
    let comp = "x--..x-.";
    dsl::comp(&mut ep, 0.0, A, 2.0, comp, 62.0, 0.75, swing, &mut h);
    dsl::comp(&mut ep, 4.0 * BAR, &dsl::repeat(A, 2), 2.0, comp, 62.0, 0.8, swing, &mut h);
    dsl::comp(&mut ep, 12.0 * BAR, &dsl::repeat(B, 2), 2.0, comp, 62.0, 0.8, swing, &mut h);
    dsl::comp(&mut ep, 20.0 * BAR, &dsl::repeat(A, 2), 2.0, comp, 62.0, 0.8, swing, &mut h);
    dsl::comp(&mut ep, 28.0 * BAR, &A[..4], 2.0, "x-------", 62.0, 0.7, 0.0, &mut h);
    dsl::comp(&mut ep, 30.0 * BAR, A, 2.0, comp, 62.0, 0.72, swing, &mut h);
    ep.note(32.0 * BAR, 8.0, 53.0, 0.6);
    for p in [57.0, 60.0, 64.0, 67.0] {
        ep.note(32.0 * BAR, 8.0, p, 0.6);
    }
    ep.lowpass = vec![
        (0.0, 700.0),
        (3.5 * BAR, 900.0),
        (4.0 * BAR, 6000.0),
        (28.0 * BAR, 6000.0),
        (28.5 * BAR, 1100.0),
        (30.0 * BAR, 6000.0),
        (34.0 * BAR, 800.0),
    ];
    ep.chorus = Some((0.6, 2.5, 0.35));
    ep.reverb = 0.25;
    ep.gain = 0.7;

    // Melodies: a soft lead, then a plucked guitar, then both.
    let mut lead = Part::new("lead", Box::new(patches::soft_lead()));
    dsl::melody(&mut lead, 4.0 * BAR, MELODY_A, 0.75, &mut h);
    dsl::melody(&mut lead, 20.0 * BAR, MELODY_A, 0.8, &mut h);
    lead.delay = 0.22;
    lead.reverb = 0.3;
    lead.gain = 1.27;
    let mut gtr = Part::new("guitar", Box::new(patches::guitar(0.45)));
    dsl::melody(&mut gtr, 12.0 * BAR, MELODY_B, 0.85, &mut h);
    dsl::melody(&mut gtr, 20.0 * BAR, &dsl::transpose(MELODY_A, -12), 0.6, &mut h);
    gtr.pan = -0.2;
    gtr.delay = 0.18;
    gtr.reverb = 0.25;
    gtr.gain = 2.5;

    // Vibraphone sparkles in the B section.
    let mut vib = Part::new("vibes", Box::new(patches::bell(4.0, 1.2)));
    dsl::arp(&mut vib, 12.0 * BAR, &dsl::repeat(B, 2), 2.0, 1.0, &[3, 2], 76.0, 0.45, &mut h);
    vib.pan = 0.35;
    vib.reverb = 0.4;
    vib.delay = 0.2;
    vib.gain = 0.7;

    song.duck_times = Vec::new();
    song.parts = vec![amb, dr, bass, ep, lead, gtr, vib];
    Track { number: 2, title: "Rainy Window", genre: "Lo-fi", art: "lofi:20:3", bpm: 78, song }
}
