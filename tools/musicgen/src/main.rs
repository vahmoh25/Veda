//! `musicgen` — composes and renders the sample music shipped with Veda.
//!
//! ```text
//! musicgen OUT_DIR [--wav] [--only N]
//! ```
//!
//! Writes `OUT_DIR/samples/Music/<Title>.qoa` for every track of the album
//! (QOA at 44.1 kHz, with a `VTAG` trailer holding title, artist, album,
//! genre, year, track number, duration, tempo and cover-art parameters).
//! `--wav` also writes 16-bit WAV previews to `OUT_DIR/preview/`, and
//! `--only N` renders just track N. Rendering is deterministic.

mod dsl;
mod patches;
mod songs;

use std::path::PathBuf;
use std::time::Instant;

use vaudio::synth;
use vaudio::tags::Tags;

/// Converts the mix to 16-bit with triangular dither.
fn to_i16(mix: &[f32], seed: u64) -> Vec<i16> {
    let mut s = seed | 1;
    let mut rand = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 40) as f32 / (1u64 << 24) as f32
    };
    mix.iter()
        .map(|&v| {
            let d = rand() - rand();
            (v * 32767.0 + d).round().clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out =
        PathBuf::from(args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "target/generated".into()));
    let wav = args.iter().any(|a| a == "--wav");
    let only: Option<u32> =
        args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok());
    let dir = out.join("samples").join("Music");
    std::fs::create_dir_all(&dir).expect("cannot create the output directory");
    let mut written = Vec::new();
    let mut total_bytes = 0usize;
    for make in songs::all() {
        let t = make();
        if only.is_some_and(|n| n != t.number) {
            continue;
        }
        let started = Instant::now();
        let (mix, stats) = synth::render(&t.song);
        let pcm = to_i16(&mix, t.number as u64 * 7919);
        let frames = pcm.len() / 2;
        let duration_ms = frames as u64 * 1000 / patches::SR as u64;
        let clipped = pcm.iter().filter(|&&v| v == i16::MAX || v == i16::MIN).count();
        let mut tags = Tags {
            title: t.title.into(),
            artist: songs::ARTIST.into(),
            album: songs::ALBUM.into(),
            genre: t.genre.into(),
            year: songs::YEAR,
            track: t.number,
            duration_ms,
            extra: Vec::new(),
        };
        tags.set("bpm", &t.bpm.to_string());
        tags.set("art", t.art);
        let mut file = vaudio::qoa::encode(&pcm, 2, patches::SR as u32).expect("QOA encoding failed");
        file.extend(tags.encode_trailer());
        let name = format!("{}.qoa", t.title);
        std::fs::write(dir.join(&name), &file).expect("cannot write the track");
        total_bytes += file.len();
        println!(
            "{:>2}. {:<16} {:>4}:{:02}  peak {:5.1} dBFS  loudness {:5.1} dB  gain {:+5.1} dB  limiter {:4.1} dB  clipped {}  {:5.2} MB  ({:.1} s)",
            t.number,
            t.title,
            duration_ms / 60_000,
            duration_ms / 1000 % 60,
            stats.peak_db,
            stats.rms_db,
            stats.gain_db,
            stats.limit_db,
            clipped,
            file.len() as f64 / 1e6,
            started.elapsed().as_secs_f32()
        );
        if std::env::var_os("MUSICGEN_PARTS").is_some() {
            for ((name, rms, peak), p) in synth::seq::part_levels(&t.song).iter().zip(&t.song.parts) {
                println!("      {:<8} RMS {:6.1} dB  peak {:6.1} dB  ({} notes)", name, rms, peak, p.notes.len());
            }
        }
        if wav {
            let pdir = out.join("preview");
            std::fs::create_dir_all(&pdir).ok();
            let w = vaudio::wav::write(&pcm, 2, patches::SR as u32, &tags);
            std::fs::write(pdir.join(format!("{}.wav", t.title)), w).ok();
        }
        let _ = db(0.0);
        written.push(name);
    }
    // A short WAV sample (mono, 22.05 kHz) next to the album.
    if only.is_none() || only == Some(7) {
        let (mix, stats) = synth::render(&songs::chime::song());
        let mono: Vec<f32> = mix.chunks(2).map(|f| (f[0] + f[1]) * 0.5).collect();
        let pcm = to_i16(&mono, 4242);
        let frames = pcm.len();
        let tags = Tags {
            title: "Startup Chime".into(),
            artist: songs::ARTIST.into(),
            album: songs::ALBUM.into(),
            genre: "Ambient".into(),
            year: songs::YEAR,
            track: 7,
            duration_ms: frames as u64 * 1000 / songs::chime::RATE as u64,
            extra: Vec::new(),
        };
        let file = vaudio::wav::write(&pcm, 1, songs::chime::RATE, &tags);
        let name = String::from("Startup Chime.wav");
        std::fs::write(dir.join(&name), &file).expect("cannot write the chime");
        total_bytes += file.len();
        println!(
            " 7. {:<16} {:>4}:{:02}  peak {:5.1} dBFS  (mono WAV, {} Hz)  {:5.2} MB",
            "Startup Chime",
            0,
            frames / songs::chime::RATE as usize,
            stats.peak_db,
            songs::chime::RATE,
            file.len() as f64 / 1e6
        );
        written.push(name);
    }
    // Remove stale tracks from earlier runs.
    if only.is_none() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if !written.contains(&n) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        println!("{} tracks, {:.2} MB", written.len(), total_bytes as f64 / 1e6);
    }
}
