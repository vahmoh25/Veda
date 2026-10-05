//! Music for the voice agent: what it shows (the song playing and where it
//! is, what plays next, the library) and what it does (playing a song,
//! album or artist by name, pausing, skipping, seeking, shuffle, repeat and
//! its own volume), through the same player operations as the buttons and
//! keys.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use vui::agent::{
    self, Action, AppAgentInfo, Value, arg_bool, arg_f64, arg_opt_int, arg_opt_str, arg_str, object, show_path,
};

use crate::engine::PlayState;
use crate::library::{Track, format_time};
use crate::{Player, Repeat};

/// The most songs the agent gets in one list.
const MAX_SONGS: usize = 60;
/// The most album names in the state.
const MAX_ALBUMS: usize = 30;
/// How many of the songs that play next the state lists.
const UP_NEXT: i32 = 5;
/// How long an action waits for the library when the player has just
/// started (it scans the Music folder first).
const LIBRARY_WAIT_NS: u64 = 2_000_000_000;
/// The least [`score`] that counts as a match.
const MIN_SCORE: u32 = 30;
/// Words of a request that say what kind of music is wanted, not which.
const FILLER: &[&str] =
    &["the", "a", "an", "song", "songs", "track", "tracks", "album", "music", "by", "some", "please", "play", "called"];
const EMPTY_LIBRARY: &str = "the library is empty: there are no .qoa or .wav files in ~/Music";

pub fn info() -> AppAgentInfo {
    agent::info(
        "A music player for the songs in ~/Music: it plays songs, albums and artists by name, pauses, skips, seeks, \
         shuffles and repeats, and has its own volume.",
        vec![
            Action::new(
                "play",
                "Plays music: the best match for a song, album or artist when a name is given, otherwise resumes \
                 (or starts) playback",
            )
            .param(
                "query",
                "string",
                "A song title, album, artist or genre as the user said it (leave out to resume)",
                false,
            )
            .choice("kind", "What the query names (any of them if not given)", false, &["song", "album", "artist"])
            .build(),
            Action::new("pause", "Pauses playback (play resumes it at the same place)").build(),
            Action::new("stop", "Stops playback and goes back to the start of the song").build(),
            Action::new("next", "Skips to the next song (in shuffled order when shuffle is on)").build(),
            Action::new("previous", "Goes back to the previous song").build(),
            Action::new("seek", "Jumps to a position in the current song")
                .param(
                    "seconds",
                    "number",
                    "Seconds from the start of the song; with relative, how far to move (negative goes back)",
                    true,
                )
                .param("relative", "boolean", "Move from the current position (false by default)", false)
                .build(),
            Action::new("shuffle", "Turns shuffle on or off")
                .param("on", "boolean", "true to play in random order, false for library order", true)
                .build(),
            Action::new("repeat", "Sets what repeats")
                .choice(
                    "mode",
                    "off (stop after the last song), all (start the library again) or one (the current song)",
                    true,
                    &["off", "all", "one"],
                )
                .build(),
            Action::new("set_volume", "Changes the player's own volume (the system volume is separate)")
                .param("level", "integer", "Volume in percent, 0 to 100", false)
                .param("change", "integer", "Change by this many percentage points, such as 10 or -10", false)
                .param("mute", "boolean", "Mute (true) or unmute (false) the player", false)
                .build(),
            Action::new(
                "list_songs",
                "Lists the songs in the library, or those whose title, album, artist or genre matches",
            )
            .param("query", "string", "What to look for (every song if not given)", false)
            .build(),
        ],
    )
}

/// The lower-case words of `s`.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// The words of a request that name the music (all of them if every word is
/// a filler word, as in a song called "The Song").
fn query_words(query: &str) -> Vec<String> {
    let all = words(query);
    let named: Vec<String> = all.iter().filter(|w| !FILLER.contains(&w.as_str())).cloned().collect();
    if named.is_empty() { all } else { named }
}

/// How well `query` (its words) names `field`: 100 for the same words, less
/// for its first words, some words in a row or some words anywhere, 0 for
/// too little in common.
fn score(query: &[String], field: &str) -> u32 {
    let fw = words(field);
    if query.is_empty() || fw.is_empty() {
        return 0;
    }
    let (q, f) = (query.join(" "), fw.join(" "));
    if q == f {
        return 100;
    }
    if q.replace(' ', "") == f.replace(' ', "") {
        return 95;
    }
    if format!("{f} ").starts_with(&format!("{q} ")) {
        return 85;
    }
    if format!(" {f} ").contains(&format!(" {q} ")) {
        return 75;
    }
    // Words in common; a longer word may be the start of the other ("window"
    // and "windows"), as speech recognition hears endings loosely.
    let same = |a: &str, b: &str| a == b || (a.len().min(b.len()) >= 4 && (a.starts_with(b) || b.starts_with(a)));
    let found = query.iter().filter(|w| fw.iter().any(|x| same(w, x))).count();
    if found * 2 < query.len() {
        return 0;
    }
    (found * 60 / query.len().max(fw.len())) as u32
}

/// The text of a song's field that a query may name.
fn field<'a>(t: &'a Track, kind: &str) -> &'a str {
    match kind {
        "song" => &t.title,
        "album" => &t.album,
        "artist" => &t.artist,
        _ => &t.genre,
    }
}

/// Whether `query` names any of a song's title, album, artist or genre.
fn matches(t: &Track, query: &[String]) -> bool {
    ["song", "album", "artist", "genre"].iter().any(|k| score(query, field(t, k)) >= MIN_SCORE)
}

/// One song as the agent sees it.
fn song(t: &Track) -> Value {
    let mut v = object! {
        "title" => t.title.as_str(),
        "artist" => t.artist.as_str(),
        "duration" => format_time(t.duration_ms),
    };
    if !t.album.is_empty() {
        v.set("album", t.album.as_str());
    }
    v
}

/// "playing", "paused", ... as the agent says it.
fn playback(state: PlayState, loaded: bool) -> &'static str {
    match state {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Error => "cannot play",
        PlayState::Idle if loaded => "stopped",
        PlayState::Idle => "nothing playing",
    }
}

/// What a request found: the song to start, and the song, album, artist or
/// genre it named.
struct Found {
    index: usize,
    kind: &'static str,
    name: String,
}

pub fn state(p: &Player) -> Value {
    let track = p.current_track();
    let mut v = object! { "playback" => playback(p.status.state, track.is_some()) };
    match track {
        Some(t) => {
            let pos = p.position_ms();
            let mut now = song(t);
            now.set("position", format_time(pos));
            now.set("position_seconds", pos / 1000);
            now.set("duration_seconds", t.duration_ms / 1000);
            if !t.genre.is_empty() {
                now.set("genre", t.genre.as_str());
            }
            if t.year != 0 {
                now.set("year", t.year);
            }
            now.set("file", show_path(&t.path));
            v.set("now_playing", now);
        }
        None => {
            v.set("now_playing", Value::Null);
        }
    }
    if p.status.state == PlayState::Error {
        v.set("error", p.status.error.as_str());
    }
    v.set("shuffle", p.shuffle);
    v.set("repeat", p.repeat.name());
    v.set("volume", (p.volume * 100.0 + 0.5) as u32);
    v.set("muted", p.muted);
    v.set("up_next", p.up_next());
    let mut albums: Vec<&str> = p.tracks.iter().map(|t| t.album.as_str()).filter(|a| !a.is_empty()).collect();
    albums.sort_unstable();
    albums.dedup();
    let mut artists: Vec<&str> = p.tracks.iter().map(|t| t.artist.as_str()).collect();
    artists.sort_unstable();
    artists.dedup();
    let total: u64 = p.tracks.iter().map(|t| t.duration_ms).sum();
    v.set(
        "library",
        object! {
            "songs" => p.tracks.len(),
            "albums" => albums.len(),
            "artists" => artists.len(),
            "total_time" => format_time(total),
            "album_names" => albums.iter().take(MAX_ALBUMS).copied().collect::<Vec<_>>(),
        },
    );
    v.set("songs", p.tracks.iter().take(MAX_SONGS).map(song).collect::<Vec<_>>());
    if p.tracks.len() > MAX_SONGS {
        v.set("songs_cut_short", true);
    }
    match p.status.device.as_str() {
        "" => {}
        "none" => {
            v.set("sound_output", "none (no sound device: playing silently)");
        }
        d => {
            v.set("sound_output", d);
        }
    }
    v
}

impl Player {
    /// Takes the library and the playback status from the engine, as a
    /// frame does (waiting a moment for the first scan of the Music folder
    /// when the player has just started).
    fn agent_sync(&mut self) {
        let end = vrt::time::now_ns() + LIBRARY_WAIT_NS;
        while self.tracks.is_empty() && self.shared.library.lock().generation == 0 && vrt::time::now_ns() < end {
            vrt::time::sleep(vrt::time::Duration::from_millis(20));
        }
        self.sync_library();
        self.sync_status();
    }

    /// Where the current song is (ms), as of now: the engine's position
    /// (the window's copy is updated once a frame), unless a song the
    /// player asked for is still loading or a seek is not done yet.
    fn position_ms(&self) -> u64 {
        let now = vrt::time::now_ns();
        let engine = self.shared.status();
        let seeked = self.seeking.is_none_or(|id| engine.seek_id == id);
        if self.requested.is_none() && seeked && engine.path == self.status.path {
            engine.position_ms(now)
        } else {
            self.status.position_ms(now)
        }
    }

    /// The songs that play next on their own (after the current one).
    fn up_next(&self) -> Vec<Value> {
        let brief = |t: &Track| object! { "title" => t.title.as_str(), "artist" => t.artist.as_str() };
        if self.repeat == Repeat::One {
            return self.current_track().map(brief).into_iter().collect();
        }
        let mut out = Vec::new();
        for step in 1..=UP_NEXT {
            match self.neighbour(step, self.repeat == Repeat::All) {
                Some(i) if Some(i) != self.current => out.push(brief(&self.tracks[i])),
                _ => break,
            }
        }
        out
    }

    /// The song playing (or paused) and where it is, for results.
    fn now_playing(&self) -> Value {
        match self.current_track() {
            Some(t) => {
                let mut v = song(t);
                v.set("playback", playback(self.status.state, true));
                v.set("position", format_time(self.position_ms()));
                v
            }
            None => object! { "playback" => playback(self.status.state, false) },
        }
    }

    /// The best match for `query`: a song, or the first song (in library
    /// order) of an album, artist or genre. `kind` limits what it may name.
    fn find_music(&self, query: &str, kind: Option<&str>) -> Result<Found, String> {
        let kinds: &[&'static str] = match kind {
            None | Some("any") => &["song", "album", "artist", "genre"],
            Some("song") => &["song"],
            Some("album") => &["album"],
            Some("artist") => &["artist"],
            Some(other) => return Err(format!("'kind' cannot be {other}")),
        };
        let q = query_words(query);
        // Scores times ten, less the kind's rank: a title wins over an album
        // or artist that matches as well.
        let mut best: Option<(u32, usize, &'static str)> = None;
        for (i, t) in self.tracks.iter().enumerate() {
            for (rank, k) in kinds.iter().enumerate() {
                let s = score(&q, field(t, k));
                if s < MIN_SCORE {
                    continue;
                }
                let s = s * 10 - rank as u32;
                if best.is_none_or(|(b, _, _)| s > b) {
                    best = Some((s, i, k));
                }
            }
        }
        let Some((_, i, kind)) = best else {
            let some: Vec<&str> = self.tracks.iter().take(6).map(|t| t.title.as_str()).collect();
            return Err(format!(
                "nothing in the library matches \u{201c}{query}\u{201d}; it has {} songs, such as {}",
                self.tracks.len(),
                some.join(", ")
            ));
        };
        let name = field(&self.tracks[i], kind).to_string();
        let index =
            if kind == "song" { i } else { self.tracks.iter().position(|t| field(t, kind) == name).unwrap_or(i) };
        Ok(Found { index, kind, name })
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        self.agent_sync();
        match action {
            "play" => {
                if self.tracks.is_empty() {
                    return Err(EMPTY_LIBRARY.into());
                }
                match arg_opt_str(args, "query") {
                    Some(query) => {
                        let found = self.find_music(query, arg_opt_str(args, "kind"))?;
                        self.play_index(found.index);
                        let mut v = self.now_playing();
                        if found.kind != "song" {
                            v.set("from", object! { found.kind => found.name });
                        }
                        Ok(v)
                    }
                    None => {
                        if self.status.state != PlayState::Playing {
                            self.toggle_play();
                        }
                        Ok(self.now_playing())
                    }
                }
            }
            "pause" => match self.status.state {
                PlayState::Playing => {
                    self.toggle_play();
                    Ok(self.now_playing())
                }
                PlayState::Paused => Ok(self.now_playing()),
                _ => Err("nothing is playing".into()),
            },
            "stop" => {
                if self.current.is_none() {
                    return Err("nothing is playing".into());
                }
                if self.status.state == PlayState::Playing {
                    self.toggle_play();
                }
                self.seek_to(0);
                // The engine is told; the song is at its start.
                let mut v = self.now_playing();
                v.set("position", format_time(0));
                Ok(v)
            }
            "next" => {
                if self.tracks.is_empty() {
                    return Err(EMPTY_LIBRARY.into());
                }
                self.next(true);
                Ok(self.now_playing())
            }
            "previous" => {
                if self.current.is_none() {
                    return Err("nothing is playing: say which song to play".into());
                }
                let i = self.neighbour(-1, true).ok_or(EMPTY_LIBRARY)?;
                self.play_index(i);
                Ok(self.now_playing())
            }
            "seek" => {
                let t = self.current_track().ok_or("nothing is playing: play a song first")?;
                let (title, duration) = (t.title.clone(), t.duration_ms);
                let secs = arg_f64(args, "seconds")?;
                let relative = arg_bool(args, "relative").unwrap_or(false);
                if !relative && secs < 0.0 {
                    return Err("'seconds' counts from the start of the song and cannot be negative".into());
                }
                let target = if relative { self.position_ms() as f64 + secs * 1000.0 } else { secs * 1000.0 };
                if target >= duration as f64 {
                    return Err(format!("{title} is only {} long", format_time(duration)));
                }
                let ms = (target.max(0.0) as u64).min(duration.saturating_sub(500));
                self.seek_to(ms);
                Ok(object! {
                    "title" => title,
                    "position" => format_time(ms),
                    "duration" => format_time(duration),
                })
            }
            "shuffle" => {
                let on = arg_bool(args, "on").ok_or("say whether shuffle should be on (true) or off (false)")?;
                if self.shuffle != on {
                    self.toggle_shuffle();
                }
                Ok(object! { "shuffle" => self.shuffle })
            }
            "repeat" => {
                let mode = match arg_str(args, "mode")? {
                    "off" => Repeat::Off,
                    "all" => Repeat::All,
                    "one" => Repeat::One,
                    other => return Err(format!("'mode' cannot be {other}: it is off, all or one")),
                };
                if self.repeat != mode {
                    self.set_repeat(mode);
                }
                Ok(object! { "repeat" => mode.name() })
            }
            "set_volume" => {
                let mut level = (self.volume * 100.0 + 0.5) as i64;
                let mut changed = false;
                if let Some(l) = arg_opt_int(args, "level")? {
                    level = l;
                    changed = true;
                }
                if let Some(c) = arg_opt_int(args, "change")? {
                    level += c;
                    changed = true;
                }
                if changed {
                    self.set_volume(level.clamp(0, 100) as f32 / 100.0);
                }
                if let Some(mute) = arg_bool(args, "mute")
                    && mute != self.muted
                {
                    self.toggle_mute();
                }
                Ok(object! { "volume" => (self.volume * 100.0 + 0.5) as u32, "muted" => self.muted })
            }
            "list_songs" => {
                let query = arg_opt_str(args, "query");
                let q = query.map(query_words).unwrap_or_default();
                let found: Vec<usize> =
                    (0..self.tracks.len()).filter(|&i| query.is_none() || matches(&self.tracks[i], &q)).collect();
                if found.is_empty() {
                    return Err(match query {
                        Some(query) => format!("no song in the library matches \u{201c}{query}\u{201d}"),
                        None => EMPTY_LIBRARY.into(),
                    });
                }
                let songs: Vec<Value> = found
                    .iter()
                    .take(MAX_SONGS)
                    .map(|&i| {
                        let mut v = song(&self.tracks[i]);
                        if self.current == Some(i) {
                            v.set("now_playing", true);
                        }
                        v
                    })
                    .collect();
                Ok(object! { "songs" => songs, "total" => found.len(), "cut_short" => found.len() > MAX_SONGS })
            }
            other => Err(format!("Music has no action called {other}")),
        }
    }
}
