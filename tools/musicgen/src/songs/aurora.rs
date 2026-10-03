//! "Aurora Drift" — ambient in D major, 66 BPM.
//!
//! Intro (6) · A (8) · B (8) · A' (8): slow pads, glassy bells, harp
//! arpeggios, a soft pulse and wind.

use vaudio::synth::{Adsr, Analog, Bed, Part, Song, Wave};

use super::Track;
use crate::dsl::{self, BAR, Humanize, key};
use crate::patches::{self, SR};

/// Two bars per chord.
const A: &[&str] = &["Dmaj9", "Gmaj7", "Bm9", "Asus4"];
const B: &[&str] = &["Em9", "Gmaj9", "Dmaj9", "Asus2"];

const BELLS_A: &str = "F#5/2 A5/1 B5/1 -/4 \
    A5/1.5 E5/0.5 D5/2 -/4 \
    F#5/1 A5/1 D6/2 -/4 \
    C#6/2 B5/1 A5/1 -/4";

const BELLS_B: &str = "B5/1 G5/1 E5/2 -/4 \
    D6/2 B5/1 F#5/1 -/4 \
    E5/1 F#5/1 A5/2 -/4 \
    E6/3 -/5";

const BELLS_END: &str = "A5/2 F#5/2 -/4 E5/2 B5/2 -/4 D6/2 A5/2 -/4 F#5/4 -/4";

/// Wind: noise through a slowly wandering band-pass.
fn wind(secs: f32) -> Vec<f32> {
    let n = (secs * SR) as usize;
    let mut out = Vec::with_capacity(n * 2);
    let mut s: u32 = 0xC0FF_EE11;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    let mut f = [
        vaudio::synth::filter::Svf::new(vaudio::synth::Mode::BandPass),
        vaudio::synth::filter::Svf::new(vaudio::synth::Mode::BandPass),
    ];
    for i in 0..n {
        let t = i as f32 / SR;
        if i % 64 == 0 {
            for (c, flt) in f.iter_mut().enumerate() {
                let centre = 500.0 + 380.0 * (t * 0.05 + c as f32 * 1.7).sin() + 200.0 * (t * 0.13 + c as f32).sin();
                flt.set(centre, 2.2, SR);
            }
        }
        let gust = 0.6 + 0.4 * (t * 0.09).sin() * (t * 0.031 + 2.0).sin();
        out.push(f[0].process(rnd()) * 0.25 * gust);
        out.push(f[1].process(rnd()) * 0.25 * gust);
    }
    out
}

pub fn track() -> Track {
    let bars = 30.0;
    let mut song = Song::new(66.0, SR, bars * BAR);
    song.reverb = (0.95, 0.35, 1.0, 0.045);
    song.reverb_return = 1.1;
    song.delay_beats = 1.5;
    song.delay_feedback = 0.48;
    song.tail = 7.0;
    song.fade_in = 2.0;
    song.fade_out = 6.0;
    song.master.target_rms_db = -19.0;
    song.master.comp_threshold = -22.0;
    song.master.comp_ratio = 1.6;
    song.master.high_shelf = (8000.0, 1.0);
    let mut h = Humanize::new(33, 0.0, 0.08);

    let total = (bars * BAR * 60.0 / 66.0) as f32 + 7.0;
    let mut air = Part::new("wind", Box::new(Bed { data: wind(total), gain: 1.0 }));
    air.note(0.0, bars * BAR + 7.0, 60.0, 1.0);
    air.volume = vec![(0.0, 1.0), (6.0 * BAR, 0.5), (22.0 * BAR, 0.5), (30.0 * BAR, 1.0)];
    air.reverb = 0.3;
    air.gain = 0.8;

    // Drone on D.
    let mut drone_inst = Analog::new(Wave::Sine, Adsr::new(4.0, 2.0, 1.0, 5.0));
    drone_inst.oscs =
        vec![vaudio::synth::OscSpec::new(Wave::Sine, 0.8, 0.0), vaudio::synth::OscSpec::new(Wave::Triangle, 0.25, 7.0)];
    drone_inst.gain = 0.42;
    let mut drone = Part::new("drone", Box::new(drone_inst));
    drone.note(0.0, 14.0 * BAR, 38.0, 0.8);
    drone.note(14.0 * BAR, 8.0 * BAR, 40.0, 0.7);
    drone.note(22.0 * BAR, 8.0 * BAR, 38.0, 0.8);
    drone.reverb = 0.2;
    drone.gain = 0.7;

    // Pads (two bars per chord).
    let mut pad = Part::new("pad", Box::new(patches::soft_pad(1600.0)));
    dsl::pads(&mut pad, 0.0, &A[1..], 2.0 * BAR, 62.0, 0.7);
    dsl::pads(&mut pad, 6.0 * BAR, A, 2.0 * BAR, 62.0, 0.8);
    dsl::pads(&mut pad, 14.0 * BAR, B, 2.0 * BAR, 64.0, 0.8);
    dsl::pads(&mut pad, 22.0 * BAR, A, 2.0 * BAR, 62.0, 0.75);
    pad.lowpass = vec![
        (0.0, 500.0),
        (6.0 * BAR, 3200.0),
        (14.0 * BAR, 2200.0),
        (18.0 * BAR, 4200.0),
        (26.0 * BAR, 3000.0),
        (30.0 * BAR, 700.0),
    ];
    pad.chorus = Some((0.2, 4.0, 0.45));
    pad.reverb = 0.55;
    pad.gain = 0.9;

    // High shimmer.
    let mut shimmer = Part::new("shimmer", Box::new(patches::soft_pad(5000.0)));
    dsl::pads(&mut shimmer, 14.0 * BAR, B, 2.0 * BAR, 80.0, 0.5);
    dsl::pads(&mut shimmer, 22.0 * BAR, A, 2.0 * BAR, 81.0, 0.45);
    shimmer.highpass = 600.0;
    shimmer.reverb = 0.8;
    shimmer.gain = 0.35;

    // Glassy bells.
    let mut bells = Part::new("bells", Box::new(patches::bell(3.0, 3.2)));
    dsl::melody(&mut bells, 6.0 * BAR, BELLS_A, 0.7, &mut h);
    dsl::melody(&mut bells, 14.0 * BAR, BELLS_B, 0.7, &mut h);
    dsl::melody(&mut bells, 22.0 * BAR, BELLS_END, 0.6, &mut h);
    bells.pan = -0.2;
    bells.delay = 0.45;
    bells.reverb = 0.6;
    bells.gain = 1.4;

    // Harp arpeggios in B and A'.
    let mut harp = Part::new("harp", Box::new(patches::harp()));
    let pat = [0usize, 1, 2, 3, 4, 3, 2, 1];
    dsl::arp(&mut harp, 14.0 * BAR, B, 2.0 * BAR, 0.5, &pat, 66.0, 0.6, &mut h);
    dsl::arp(&mut harp, 22.0 * BAR, &A[..3], 2.0 * BAR, 0.5, &pat, 66.0, 0.5, &mut h);
    harp.pan = 0.3;
    harp.reverb = 0.45;
    harp.delay = 0.15;
    harp.gain = 1.75;

    // A soft pulse, like a slow heartbeat.
    let mut pulse = Part::new("pulse", Box::new(patches::kit_lofi()));
    dsl::drums(&mut pulse, key::KICK, "x.......o.......", 14.0, 8, 0.0, &mut h);
    dsl::drums(&mut pulse, key::SHAKER, "....o.......o...", 16.0, 6, 0.0, &mut h);
    pulse.reverb = 0.35;
    pulse.eq = vec![(vaudio::synth::BiquadKind::LowPass, 2500.0, 0.7)];
    pulse.gain = 0.55;

    song.parts = vec![air, drone, pad, shimmer, bells, harp, pulse];
    Track { number: 3, title: "Aurora Drift", genre: "Ambient", art: "aurora:150:21", bpm: 66, song }
}
