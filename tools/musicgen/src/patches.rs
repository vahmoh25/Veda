//! Instrument presets shared by the songs.

use vaudio::synth::drums::{self, Kick};
use vaudio::synth::{Adsr, Analog, Chip, FilterSpec, Fm, Kit, Mode, OscSpec, Pluck, Wave};

use crate::dsl::key;

pub const SR: f32 = 44_100.0;

fn lp(cutoff: f32, q: f32, env_octaves: f32, env: Adsr, keytrack: f32) -> Option<FilterSpec> {
    Some(FilterSpec { mode: Mode::LowPass, cutoff, q, env_octaves, env, keytrack, vel_octaves: 0.5 })
}

/// A wide, warm supersaw pad.
pub fn supersaw_pad(cutoff: f32) -> Analog {
    let mut a = Analog::new(Wave::Saw, Adsr::new(0.7, 1.0, 0.85, 1.4));
    a.oscs = vec![OscSpec::new(Wave::Saw, 0.8, 0.0), OscSpec::new(Wave::Saw, 0.35, 12.0)];
    a.unison = 5;
    a.detune = 28.0;
    a.width = 0.95;
    a.filter = lp(cutoff, 0.7, 0.6, Adsr::new(1.2, 1.5, 0.4, 1.0), 0.3);
    a.gain = 0.22;
    a.vel_sens = 0.3;
    a
}

/// A soft, slowly evolving pad for ambient music.
pub fn soft_pad(cutoff: f32) -> Analog {
    let mut a = Analog::new(Wave::Triangle, Adsr::new(2.2, 2.0, 0.9, 3.0));
    a.oscs = vec![
        OscSpec::new(Wave::Triangle, 0.9, 0.0),
        OscSpec::new(Wave::Saw, 0.25, 0.0),
        OscSpec::new(Wave::Sine, 0.4, 12.0),
    ];
    a.unison = 3;
    a.detune = 16.0;
    a.width = 1.0;
    a.filter = lp(cutoff, 0.6, 0.8, Adsr::new(3.0, 3.0, 0.5, 2.0), 0.2);
    a.vibrato = (0.25, 0.04, 1.0);
    a.gain = 0.28;
    a.vel_sens = 0.2;
    a
}

/// A plucky arpeggio synth.
pub fn pluck_synth(cutoff: f32, decay: f32) -> Analog {
    let mut a = Analog::new(Wave::Square, Adsr::new(0.002, decay, 0.0, 0.12));
    a.oscs = vec![OscSpec::new(Wave::Square, 0.6, 0.0), OscSpec::new(Wave::Saw, 0.5, 0.0)];
    a.unison = 2;
    a.detune = 10.0;
    a.width = 0.5;
    a.filter = lp(cutoff, 1.4, 2.6, Adsr::new(0.001, decay * 0.7, 0.0, 0.1), 0.5);
    a.gain = 0.32;
    a
}

/// A punchy synth bass (saw + square, resonant low-pass with a quick
/// envelope, sub sine).
pub fn synth_bass(cutoff: f32) -> Analog {
    let mut a = Analog::new(Wave::Saw, Adsr::new(0.003, 0.25, 0.65, 0.06));
    a.oscs = vec![OscSpec::new(Wave::Saw, 0.6, 0.0), OscSpec::new(Wave::Square, 0.45, 0.0)];
    a.sub = 0.7;
    a.filter = lp(cutoff, 1.1, 2.0, Adsr::new(0.001, 0.14, 0.15, 0.05), 0.4);
    a.drive = 1.6;
    a.gain = 0.42;
    a.vel_sens = 0.4;
    a
}

/// A round, soft bass for mellow music.
pub fn warm_bass() -> Analog {
    let mut a = Analog::new(Wave::Triangle, Adsr::new(0.006, 0.6, 0.55, 0.1));
    a.oscs = vec![OscSpec::new(Wave::Triangle, 0.7, 0.0), OscSpec::new(Wave::Sine, 0.8, 0.0)];
    a.filter = lp(420.0, 0.7, 1.2, Adsr::new(0.002, 0.2, 0.2, 0.1), 0.3);
    a.gain = 0.55;
    a.vel_sens = 0.5;
    a
}

/// A singing saw lead with vibrato.
pub fn saw_lead(cutoff: f32) -> Analog {
    let mut a = Analog::new(Wave::Saw, Adsr::new(0.01, 0.4, 0.75, 0.25));
    a.oscs = vec![OscSpec::new(Wave::Saw, 0.6, 0.0), OscSpec::new(Wave::Pulse(0.35), 0.4, 0.0)];
    a.unison = 2;
    a.detune = 14.0;
    a.width = 0.35;
    a.filter = lp(cutoff, 1.0, 1.2, Adsr::new(0.01, 0.5, 0.5, 0.3), 0.4);
    a.vibrato = (5.4, 0.16, 0.35);
    a.gain = 0.26;
    a
}

/// A supersaw lead for dance music.
pub fn supersaw_lead(cutoff: f32) -> Analog {
    let mut a = Analog::new(Wave::Saw, Adsr::new(0.005, 0.3, 0.7, 0.2));
    a.oscs = vec![OscSpec::new(Wave::Saw, 0.7, 0.0), OscSpec::new(Wave::Saw, 0.3, 12.0)];
    a.unison = 5;
    a.detune = 22.0;
    a.width = 0.8;
    a.filter = lp(cutoff, 0.9, 1.0, Adsr::new(0.005, 0.3, 0.6, 0.2), 0.4);
    a.vibrato = (5.0, 0.08, 0.4);
    a.gain = 0.2;
    a
}

/// A soft sine lead (lo-fi melodies, whistles).
pub fn soft_lead() -> Analog {
    let mut a = Analog::new(Wave::Sine, Adsr::new(0.03, 0.5, 0.7, 0.35));
    a.oscs = vec![OscSpec::new(Wave::Sine, 0.8, 0.0), OscSpec::new(Wave::Triangle, 0.35, 12.0)];
    a.vibrato = (5.0, 0.12, 0.3);
    a.filter = lp(2600.0, 0.7, 0.5, Adsr::new(0.05, 0.4, 0.5, 0.3), 0.3);
    a.gain = 0.34;
    a
}

/// An electric piano (two-operator FM with a tine and tremolo).
pub fn epiano() -> Fm {
    Fm {
        ratio: 1.0,
        index: 1.7,
        index_decay: 0.9,
        index_sustain: 0.18,
        tine_ratio: 14.0,
        tine: 0.9,
        amp: Adsr::new(0.002, 2.6, 0.0, 0.45),
        vel_index: 0.8,
        detune: 5.0,
        tremolo: (4.2, 0.35),
        gain: 0.22,
    }
}

/// A bright bell / mallet.
pub fn bell(ratio: f32, decay: f32) -> Fm {
    Fm {
        ratio,
        index: 2.2,
        index_decay: decay * 0.5,
        index_sustain: 0.1,
        tine_ratio: 7.0,
        tine: 0.3,
        amp: Adsr::new(0.002, decay, 0.0, decay * 0.5),
        vel_index: 0.6,
        detune: 3.0,
        tremolo: (0.0, 0.0),
        gain: 0.2,
    }
}

/// A fingerpicked steel-string guitar.
pub fn guitar(brightness: f32) -> Pluck {
    Pluck { brightness, sustain: 3.0, release: 0.25, gain: 0.5, width: 0.4 }
}

/// A harp-like long-ringing string.
pub fn harp() -> Pluck {
    Pluck { brightness: 0.55, sustain: 5.0, release: 1.5, gain: 0.42, width: 0.6 }
}

/// NES-style pulse voice.
pub fn chip_pulse(duty: f32, pan: f32) -> Chip {
    Chip {
        wave: Wave::Pulse(duty),
        amp: Adsr::new(0.002, 0.25, 0.65, 0.05),
        arp: Vec::new(),
        arp_hz: 0.0,
        vibrato: (6.0, 0.18, 0.18),
        slide: 0.0,
        slide_secs: 0.0,
        pan,
        gain: 0.15,
    }
}

/// NES-style triangle bass.
pub fn chip_triangle() -> Chip {
    Chip {
        wave: Wave::ChipTriangle,
        amp: Adsr::new(0.001, 0.1, 0.9, 0.02),
        arp: Vec::new(),
        arp_hz: 0.0,
        vibrato: (0.0, 0.0, 0.0),
        slide: 0.0,
        slide_secs: 0.0,
        pan: 0.0,
        gain: 0.32,
    }
}

/// An 80s electronic kit (punchy kick, gated snare + clap, metallic hats).
pub fn kit_80s() -> Kit {
    let mut k = Kit::default();
    k.add(key::KICK as i32, drums::kick(Kick::PUNCHY, SR), 0.0, 0.95);
    let mut snare = drums::snare(185.0, 1.1, 0.24, SR);
    for (s, c) in snare.iter_mut().zip(drums::clap(SR)) {
        *s = *s * 0.8 + c * 0.45;
    }
    k.add(key::SNARE as i32, snare, 0.0, 0.62);
    k.add(key::HAT as i32, drums::hat(0.045, 1.0, SR), 0.25, 0.2);
    k.add(key::OPEN_HAT as i32, drums::hat(0.32, 1.0, SR), 0.25, 0.17);
    k.add(key::CRASH as i32, drums::cymbal(1.8, SR), -0.3, 0.22);
    k.add(key::TOM_LO as i32, drums::tom(98.0, SR), -0.35, 0.5);
    k.add(key::TOM_HI as i32, drums::tom(146.0, SR), 0.3, 0.45);
    k.add(key::FX as i32, drums::sweep(4.8, 300.0, 9000.0, true, SR), 0.0, 0.12);
    k
}

/// A soft, dusty kit for lo-fi beats.
pub fn kit_lofi() -> Kit {
    let mut k = Kit::default();
    k.add(key::KICK as i32, drums::kick(Kick::SOFT, SR), 0.0, 0.9);
    k.add(key::SNARE as i32, drums::snare(170.0, 0.8, 0.2, SR), 0.05, 0.42);
    k.add(key::RIM as i32, drums::rim(SR), 0.2, 0.25);
    k.add(key::HAT as i32, drums::hat(0.035, 0.75, SR), -0.2, 0.13);
    k.add(key::OPEN_HAT as i32, drums::hat(0.22, 0.75, SR), -0.2, 0.09);
    k.add(key::SHAKER as i32, drums::shaker(SR), 0.35, 0.12);
    k
}

/// A punchy house kit.
pub fn kit_house() -> Kit {
    let mut k = Kit::default();
    k.add(
        key::KICK as i32,
        drums::kick(Kick { start: 210.0, end: 50.0, sweep: 0.028, decay: 0.36, click: 0.5, drive: 2.4 }, SR),
        0.0,
        0.95,
    );
    k.add(key::CLAP as i32, drums::clap(SR), 0.0, 0.42);
    k.add(key::SNARE as i32, drums::snare(200.0, 1.2, 0.18, SR), 0.0, 0.38);
    k.add(key::HAT as i32, drums::hat(0.04, 1.1, SR), 0.2, 0.17);
    k.add(key::OPEN_HAT as i32, drums::hat(0.2, 1.05, SR), -0.15, 0.17);
    k.add(key::SHAKER as i32, drums::shaker(SR), 0.4, 0.13);
    k.add(key::RIDE as i32, drums::cymbal(0.9, SR), 0.35, 0.09);
    k.add(key::CRASH as i32, drums::cymbal(2.2, SR), -0.3, 0.2);
    k.add(key::FX as i32, drums::sweep(7.7, 250.0, 11_000.0, true, SR), 0.0, 0.13);
    k
}

/// Chiptune drums from the noise channel and a pulse kick.
pub fn kit_chip() -> Kit {
    let mut k = Kit::default();
    k.add(key::KICK as i32, drums::chip_kick(SR), 0.0, 0.7);
    k.add(key::SNARE as i32, drums::chip_noise(0.14, 0.32, false, SR), 0.0, 0.48);
    k.add(key::HAT as i32, drums::chip_noise(0.035, 0.9, false, SR), 0.0, 0.2);
    k.add(key::OPEN_HAT as i32, drums::chip_noise(0.12, 0.9, true, SR), 0.0, 0.14);
    k.add(key::CRASH as i32, drums::chip_noise(0.6, 0.7, false, SR), 0.0, 0.25);
    k
}

/// A light acoustic-style kit (soft kick, brushy snare, shaker, claps).
pub fn kit_acoustic() -> Kit {
    let mut k = Kit::default();
    k.add(
        key::KICK as i32,
        drums::kick(Kick { start: 110.0, end: 55.0, sweep: 0.03, decay: 0.3, click: 0.15, drive: 1.3 }, SR),
        0.0,
        0.85,
    );
    k.add(key::SNARE as i32, drums::snare(210.0, 0.7, 0.16, SR), 0.05, 0.36);
    k.add(key::RIM as i32, drums::rim(SR), 0.15, 0.22);
    k.add(key::CLAP as i32, drums::clap(SR), 0.0, 0.32);
    k.add(key::SHAKER as i32, drums::shaker(SR), 0.3, 0.16);
    k.add(key::HAT as i32, drums::hat(0.05, 0.9, SR), -0.25, 0.12);
    k.add(key::CRASH as i32, drums::cymbal(2.0, SR), -0.2, 0.15);
    k
}

/// A soft glockenspiel (gentler than [`bell`]).
pub fn glock() -> Fm {
    Fm {
        ratio: 3.0,
        index: 1.1,
        index_decay: 0.25,
        index_sustain: 0.15,
        tine_ratio: 7.0,
        tine: 0.15,
        amp: Adsr::new(0.002, 1.1, 0.0, 0.5),
        vel_index: 0.5,
        detune: 2.0,
        tremolo: (0.0, 0.0),
        gain: 0.22,
    }
}
