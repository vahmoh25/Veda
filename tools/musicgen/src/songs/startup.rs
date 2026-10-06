//! The startup sound, which the shell plays as the boot splash gives way
//! to the desktop: under four seconds, quiet, and soft at its start (the
//! speaker amplifiers of some laptops power up as it begins). A warm
//! Dmaj9 swells in while bells climb the chord to a held E6, in a large
//! room with a faint echo. Shipped as `sounds/startup.wav` (48 kHz
//! stereo, the rate sound devices run at, so it is played as it is).

use vaudio::synth::{Adsr, Analog, FilterSpec, Mode, OscSpec, Part, Song, Wave};

use crate::dsl::{self, Humanize};
use crate::patches;

pub const RATE: u32 = 48_000;

/// The pad: a sine and triangle blend that swells in over half a second
/// and lets go slowly.
fn swell() -> Analog {
    let mut a = Analog::new(Wave::Triangle, Adsr::new(0.5, 1.2, 0.8, 1.6));
    a.oscs = vec![OscSpec::new(Wave::Triangle, 0.8, 0.0), OscSpec::new(Wave::Sine, 0.5, 12.0)];
    a.unison = 3;
    a.detune = 12.0;
    a.width = 1.0;
    let env = Adsr::new(0.8, 1.5, 0.6, 1.5);
    a.filter = Some(FilterSpec {
        mode: Mode::LowPass,
        cutoff: 1800.0,
        q: 0.6,
        env_octaves: 0.7,
        env,
        keytrack: 0.2,
        vel_octaves: 0.3,
    });
    a.gain = 0.26;
    a.vel_sens = 0.2;
    a
}

pub fn song() -> Song {
    // At 120 beats a minute a beat is half a second: a bar of sound (two
    // seconds), then the room's tail.
    let mut song = Song::new(120.0, RATE as f32, 4.0);
    song.tail = 1.8;
    song.fade_out = 1.2;
    song.reverb = (0.86, 0.45, 1.0, 0.03);
    song.reverb_return = 1.0;
    song.delay_beats = 0.75;
    song.delay_feedback = 0.3;
    song.delay_return = 0.6;
    song.master.target_rms_db = -21.0;
    song.master.ceiling_db = -4.0;
    let mut h = Humanize::new(31, 0.0, 0.04);
    let mut bells = Part::new("bells", Box::new(patches::bell(3.0, 1.9)));
    dsl::melody(&mut bells, 0.0, "-/0.5 D5/0.25~ F#5/0.25~ A5/0.25 C#6/0.25 E6/2.5!", 0.62, &mut h);
    bells.reverb = 0.55;
    bells.delay = 0.25;
    bells.gain = 1.1;
    let mut pad = Part::new("pad", Box::new(swell()));
    dsl::pads(&mut pad, 0.0, &["Dmaj9"], 3.0, 62.0, 0.6);
    pad.reverb = 0.45;
    pad.gain = 0.9;
    song.parts = vec![bells, pad];
    song
}
