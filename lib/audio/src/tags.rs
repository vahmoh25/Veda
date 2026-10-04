//! Track metadata.
//!
//! [`Tags`] holds the descriptive fields a music player shows. They come
//! from a WAV file's `LIST/INFO` chunk (see [`crate::wav`]) or from the
//! `VTAG` trailer that Veda appends to QOA files:
//!
//! ```text
//! ... QOA frames ... | "key=value\n" lines (UTF-8) | u32 LE text length | "VTAG"
//! ```
//!
//! QOA decoders stop after the number of samples announced in the file
//! header, so the trailer does not disturb other players. Because the
//! trailer sits at the very end, a library scanner only needs to read the
//! last few kilobytes of each file.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Magic bytes that end a tag trailer.
pub const TRAILER_MAGIC: [u8; 4] = *b"VTAG";
/// Largest accepted trailer text.
pub const MAX_TRAILER_TEXT: usize = 16 * 1024;

/// Descriptive metadata of a track. Empty strings and zeros mean "unknown".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: u32,
    /// Position on the album (1-based).
    pub track: u32,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Further `(key, value)` fields, e.g. `bpm` or `art` (a cover-art seed).
    pub extra: Vec<(String, String)>,
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '\n' || c == '\r' || c == '\0' { ' ' } else { c }).collect::<String>().trim().to_string()
}

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 32 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Tags {
    /// Looks up any field by its trailer key.
    pub fn get(&self, key: &str) -> Option<&str> {
        match key {
            "title" => Some(&self.title),
            "artist" => Some(&self.artist),
            "album" => Some(&self.album),
            "genre" => Some(&self.genre),
            _ => self.extra.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
        }
    }

    /// Sets a field by its trailer key (unknown keys go to `extra`).
    pub fn set(&mut self, key: &str, value: &str) {
        let v = clean(value);
        match key {
            "title" => self.title = v,
            "artist" => self.artist = v,
            "album" => self.album = v,
            "genre" => self.genre = v,
            "year" => self.year = v.parse().unwrap_or(0),
            "track" => self.track = v.parse().unwrap_or(0),
            "duration_ms" => self.duration_ms = v.parse().unwrap_or(0),
            _ if valid_key(key) => match self.extra.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = v,
                None => self.extra.push((key.to_string(), v)),
            },
            _ => {}
        }
    }

    /// True if no field is set.
    pub fn is_empty(&self) -> bool {
        *self == Tags::default()
    }

    /// Fills fields that are empty here from `other`.
    pub fn merge_missing(&mut self, other: &Tags) {
        let fill = |a: &mut String, b: &String| {
            if a.is_empty() {
                *a = b.clone();
            }
        };
        fill(&mut self.title, &other.title);
        fill(&mut self.artist, &other.artist);
        fill(&mut self.album, &other.album);
        fill(&mut self.genre, &other.genre);
        if self.year == 0 {
            self.year = other.year;
        }
        if self.track == 0 {
            self.track = other.track;
        }
        if self.duration_ms == 0 {
            self.duration_ms = other.duration_ms;
        }
        for (k, v) in &other.extra {
            if self.get(k).is_none() {
                self.extra.push((k.clone(), v.clone()));
            }
        }
    }

    /// Encodes the trailer (`key=value` lines, length, magic).
    pub fn encode_trailer(&self) -> Vec<u8> {
        let mut text = String::new();
        let mut line = |k: &str, v: &str| {
            if !v.is_empty() {
                text.push_str(k);
                text.push('=');
                text.push_str(&clean(v));
                text.push('\n');
            }
        };
        line("title", &self.title);
        line("artist", &self.artist);
        line("album", &self.album);
        line("genre", &self.genre);
        if self.year != 0 {
            line("year", &self.year.to_string());
        }
        if self.track != 0 {
            line("track", &self.track.to_string());
        }
        if self.duration_ms != 0 {
            line("duration_ms", &self.duration_ms.to_string());
        }
        for (k, v) in &self.extra {
            if valid_key(k) {
                line(k, v);
            }
        }
        let mut out = text.into_bytes();
        out.truncate(MAX_TRAILER_TEXT);
        let n = out.len() as u32;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&TRAILER_MAGIC);
        out
    }

    /// Total size of the trailer at the end of `data` (text + 8), if any.
    pub fn trailer_len(data: &[u8]) -> Option<usize> {
        if data.len() < 8 || data[data.len() - 4..] != TRAILER_MAGIC {
            return None;
        }
        let n = crate::le32(data, data.len() - 8) as usize;
        if n > MAX_TRAILER_TEXT || n + 8 > data.len() {
            return None;
        }
        Some(n + 8)
    }

    /// Parses the trailer at the end of `data` (a whole file or its tail).
    pub fn from_trailer(data: &[u8]) -> Option<Tags> {
        let total = Tags::trailer_len(data)?;
        let text = &data[data.len() - total..data.len() - 8];
        let text = core::str::from_utf8(text).ok()?;
        let mut tags = Tags::default();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                tags.set(k.trim(), v);
            }
        }
        Some(tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_roundtrip() {
        let mut t = Tags {
            title: "Neon\nHorizon".into(),
            artist: "Veda Studio".into(),
            album: "First Light".into(),
            genre: "Synthwave".into(),
            year: 2026,
            track: 1,
            duration_ms: 123_456,
            extra: Vec::new(),
        };
        t.set("bpm", "100");
        t.set("Bad Key", "x");
        let mut file = alloc::vec![1u8, 2, 3];
        file.extend(t.encode_trailer());
        let back = Tags::from_trailer(&file).unwrap();
        assert_eq!(back.title, "Neon Horizon");
        assert_eq!(back.artist, "Veda Studio");
        assert_eq!(back.duration_ms, 123_456);
        assert_eq!(back.year, 2026);
        assert_eq!(back.get("bpm"), Some("100"));
        assert_eq!(back.extra.len(), 1);
        assert_eq!(Tags::trailer_len(&file), Some(file.len() - 3));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Tags::from_trailer(b"").is_none());
        assert!(Tags::from_trailer(b"VTAG").is_none());
        let mut bad = alloc::vec![0u8; 4];
        bad.extend_from_slice(&1000u32.to_le_bytes());
        bad.extend_from_slice(b"VTAG");
        assert!(Tags::from_trailer(&bad).is_none());
    }
}
