//! The music library: tracks found in the user's Music folder (plus files
//! opened explicitly), with metadata read from the first and last few
//! kilobytes of each file.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vaudio::source;
use vproto::fs::{open_flags, vfs};

/// Where the library lives.
pub const MUSIC_DIR: &str = "/home/user/Music";
/// Bytes read from each end of a file to find its format and tags.
const PROBE_BYTES: u32 = 8192;

/// One playable file.
#[derive(Debug, Clone)]
pub struct Track {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: u32,
    pub number: u32,
    pub bpm: u32,
    pub duration_ms: u64,
    pub format: &'static str,
    pub rate: u32,
    /// Seed for the generated cover art.
    pub seed: u64,
    /// Cover art description (`style:hue:seed`) from the file's tags.
    pub art: Option<String>,
}

/// FNV-1a.
pub fn hash(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn read_range(vfs: &vfs::Client, fd: u32, offset: u64, len: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while out.len() < len as usize {
        let want = (len as usize - out.len()).min(vproto::fs::MAX_IO as usize) as u32;
        let chunk = vfs.read(fd, offset + out.len() as u64, want).ok()?.ok()?;
        if chunk.0.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk.0);
    }
    Some(out)
}

/// Reads the metadata of one file.
pub fn probe(vfs: &vfs::Client, path: &str) -> Option<Track> {
    let stat = vfs.stat(path.into()).ok()?.ok()?;
    if stat.is_dir {
        return None;
    }
    let fd = vfs.open(path.into(), open_flags::READ).ok()?.ok()?;
    let head = read_range(vfs, fd, 0, PROBE_BYTES.min(stat.size as u32));
    let tail_len = PROBE_BYTES.min(stat.size as u32);
    let tail = read_range(vfs, fd, stat.size - tail_len as u64, tail_len);
    let _ = vfs.close(fd);
    let (head, tail) = (head?, tail?);
    let p = source::probe(&head, &tail, stat.size).ok()?;
    let t = p.tags;
    let title = if t.title.is_empty() { vfiles::path::file_stem(path).to_string() } else { t.title.clone() };
    let seed = hash(&title);
    Some(Track {
        art: t.get("art").map(|s| s.to_string()),
        path: path.to_string(),
        artist: if t.artist.is_empty() { "Unknown artist".into() } else { t.artist.clone() },
        album: t.album.clone(),
        genre: t.genre.clone(),
        year: t.year,
        number: t.track,
        bpm: t.get("bpm").and_then(|b| b.parse().ok()).unwrap_or(0),
        duration_ms: t.duration_ms,
        format: p.format.name(),
        rate: p.rate,
        seed,
        title,
    })
}

/// One audio file in the Music folder, as listed by the file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub modified: u64,
}

impl Entry {
    pub fn path(&self) -> String {
        format!("{}/{}", MUSIC_DIR, self.name)
    }
}

/// The audio files in the Music folder (cheap: one directory listing).
pub fn listing(vfs: &vfs::Client) -> Vec<Entry> {
    let Ok(Ok(entries)) = vfs.read_dir(MUSIC_DIR.into()) else { return Vec::new() };
    entries
        .into_iter()
        .filter(|e| !e.is_dir && vfiles::kind::is_playable_audio(&e.name))
        .map(|e| Entry { name: e.name, size: e.size, modified: e.modified })
        .collect()
}

/// Library order: album, track number, title.
pub fn sort(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| {
        a.album
            .to_lowercase()
            .cmp(&b.album.to_lowercase())
            .then(a.number.cmp(&b.number))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
}

/// `m:ss` (or `h:mm:ss`).
pub fn format_time(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
