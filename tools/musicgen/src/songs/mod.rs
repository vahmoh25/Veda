//! The album "First Light" by Vindows Studio.

mod aurora;
pub mod chime;
mod coffee;
mod neon;
mod pixel;
mod rain;
mod skyline;

use vaudio::synth::{Part, Song};

/// A finished arrangement plus its metadata.
pub struct Track {
    pub number: u32,
    pub title: &'static str,
    pub genre: &'static str,
    /// Cover art for the player: `style:hue_degrees:seed`.
    pub art: &'static str,
    pub bpm: u32,
    pub song: Song,
}

pub const ARTIST: &str = "Vindows Studio";
pub const ALBUM: &str = "First Light";
pub const YEAR: u32 = 2026;

/// Every track of the album, in order.
pub fn all() -> Vec<fn() -> Track> {
    vec![neon::track, rain::track, aurora::track, pixel::track, skyline::track, coffee::track]
}

/// The sorted start beats of a part's notes with the given key (for
/// side-chain ducking).
pub fn hits_of(part: &Part, key: f32) -> Vec<f64> {
    let mut v: Vec<f64> = part.notes.iter().filter(|n| n.pitch == key).map(|n| n.start).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}
