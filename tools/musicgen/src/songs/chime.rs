//! "Startup Chime" — a short bell flourish, shipped as a mono 22.05 kHz
//! WAV file (it shows that the player handles WAV, mono and resampling).

use vaudio::synth::{Part, Song};

use crate::dsl::{self, BAR, Humanize};
use crate::patches;

pub const RATE: u32 = 22_050;

pub fn song() -> Song {
    let sr = RATE as f32;
    let mut song = Song::new(84.0, sr, 3.0 * BAR);
    song.reverb = (0.85, 0.4, 1.0, 0.03);
    song.reverb_return = 1.0;
    song.delay_beats = 0.75;
    song.delay_feedback = 0.35;
    song.tail = 4.5;
    song.fade_out = 2.0;
    song.master.target_rms_db = -18.0;
    let mut h = Humanize::new(77, 0.0, 0.05);
    let mut bells = Part::new("bells", Box::new(patches::bell(3.0, 2.4)));
    dsl::melody(&mut bells, 0.0, "D5/0.5 F#5/0.5 A5/0.5 C#6/0.5 E6/2! -/2 A5/0.5 E6/0.5 F#6/3!", 0.8, &mut h);
    bells.reverb = 0.5;
    bells.delay = 0.3;
    bells.gain = 1.4;
    let mut pad = Part::new("pad", Box::new(patches::soft_pad(2400.0)));
    dsl::pads(&mut pad, 0.0, &["Dmaj9"], 2.0 * BAR, 62.0, 0.7);
    dsl::pads(&mut pad, 2.0 * BAR, &["Dmaj9"], BAR, 69.0, 0.6);
    pad.reverb = 0.5;
    pad.gain = 0.8;
    song.parts = vec![bells, pad];
    song
}
