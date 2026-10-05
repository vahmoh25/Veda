//! Music — the Veda music player.
//!
//! * A library of the tracks in `/home/user/Music` (QOA and WAV), plus any
//!   file passed on the command line, which starts playing at once.
//! * A now-playing panel with generated cover art, a real-time spectrum
//!   visualiser driven by the samples being heard, a seek bar with elapsed
//!   and remaining time, transport controls, shuffle/repeat and volume.
//! * Decoding happens on a background thread ([`engine`]) that streams to
//!   the audio service; covers are drawn on another ([`art`]), so the UI
//!   stays responsive.
//! * The library is rescanned whenever the window regains the focus, and a
//!   second `music FILE` hands the file to the running player ([`remote`]).
//! * User actions are logged concisely (`playing <title>`, `paused at m:ss`,
//!   `seek to m:ss`, `volume N%`, ...) for the GUI tests.
//! * [`agent`] lets the voice agent play, find and control music.
//!
//! Keyboard: Space play/pause, Left/Right seek 5 s, Up/Down volume, N next,
//! P previous, S shuffle, R repeat, M mute.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod art;
mod engine;
mod library;
mod remote;
mod widgets;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use vabi::{RawHandle, signals};
use vaudio::fft::Analyzer;
use vgfx::{Align, Bitmap, Color, Rect};
use vmath::FloatExt;
use vproto::input::keys;
use vrt::object::Event;
use vrt::sync::Mutex;
use vui::{App, Font, Icon, Ui, WindowSpec};

use art::ArtSpec;
use engine::{Command, PlayState, Shared, Status};
use library::{Track, format_time};

vrt::entry!(main);

/// Spectrum bands drawn.
const BANDS: usize = 40;
/// Samples per spectrum analysis.
const FFT_SIZE: usize = 2048;
/// Cover sizes.
const COVER: i32 = 232;
const THUMB: i32 = 44;
const SIDEBAR_W: i32 = 304;
const ROW_H: i32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repeat {
    Off,
    All,
    One,
}

impl Repeat {
    fn name(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }
}

/// Covers rendered by the art thread.
struct Covers {
    specs: Vec<ArtSpec>,
    large: Vec<Option<Arc<Bitmap>>>,
    thumbs: Vec<Option<Arc<Bitmap>>>,
    /// Render this cover next.
    priority: Option<usize>,
}

/// Makes the corners of a square cover transparent (anti-aliased).
fn round_corners(b: &mut Bitmap, radius: f32) {
    let (w, h) = (b.width, b.height);
    let rad = radius.min(w as f32 / 2.0);
    let band = rad.ceil() as i32;
    for y in 0..band.min(h) {
        for x in 0..band.min(w) {
            let fx = rad - (x as f32 + 0.5);
            let fy = rad - (y as f32 + 0.5);
            let d = FloatExt::sqrt(fx * fx + fy * fy);
            let cov = ((rad - d + 0.5).clamp(0.0, 1.0) * 255.0) as u32;
            if cov >= 255 {
                continue;
            }
            for (cx, cy) in [(x, y), (w - 1 - x, y), (x, h - 1 - y), (w - 1 - x, h - 1 - y)] {
                let p = &mut b.pixels[(cy * w + cx) as usize];
                *p = vgfx::color::scale(*p, cov);
            }
        }
    }
}

/// The cover-art thread: renders missing covers, most wanted first.
fn art_thread(covers: Arc<Mutex<Covers>>, wake: Event, notify: Event) {
    let mut drawn = 0;
    loop {
        let job = {
            let c = covers.lock();
            let missing = |i: &usize| c.large.get(*i).is_some_and(|x| x.is_none());
            c.priority.filter(missing).or_else(|| (0..c.large.len()).find(missing)).map(|i| (i, c.specs[i]))
        };
        let Some((i, spec)) = job else {
            if drawn > 0 {
                let ready = covers.lock().large.iter().filter(|c| c.is_some()).count();
                vrt::println!("covers: {} ready", ready);
                drawn = 0;
            }
            let _ = wake.wait(signals::SIGNALED, vabi::DEADLINE_INFINITE);
            let _ = wake.clear();
            continue;
        };
        drawn += 1;
        let mut big = art::render(&spec, COVER);
        round_corners(&mut big, 14.0);
        let mut thumb = big.resized(THUMB, THUMB);
        round_corners(&mut thumb, 6.0);
        {
            let mut c = covers.lock();
            // The library may have changed while we were drawing.
            if c.specs.get(i) != Some(&spec) {
                continue;
            }
            if let Some(slot) = c.large.get_mut(i) {
                *slot = Some(Arc::new(big));
            }
            if let Some(slot) = c.thumbs.get_mut(i) {
                *slot = Some(Arc::new(thumb));
            }
        }
        let _ = notify.signal();
    }
}

struct Player {
    shared: Arc<Shared>,
    tracks: Vec<Track>,
    covers: Arc<Mutex<Covers>>,
    art_wake: Option<Event>,
    current: Option<usize>,
    selected: Option<usize>,
    shuffle: bool,
    repeat: Repeat,
    order: Vec<usize>,
    /// Volume slider position (0..1, perceptual).
    volume: f32,
    muted: bool,
    status: Status,
    last_ended: u32,
    analyzer: Analyzer,
    analyzer_rate: u32,
    window_buf: Vec<f32>,
    levels: Vec<f32>,
    bars: Vec<f32>,
    peaks: Vec<f32>,
    last_frame_ns: u64,
    seek_drag: Option<f32>,
    /// The library generation shown.
    library_gen: u32,
    /// The window had the focus last frame (regaining it rescans).
    was_focused: bool,
    /// The `music` service (absent in a second, independent player).
    remote: Option<remote::Server>,
    /// When to log the volume (debounced while the slider moves).
    volume_log_at: Option<u64>,
    volume_logged: i32,
    /// A track we asked the engine to load that it has not confirmed yet.
    requested: Option<u32>,
    load_seq: u32,
    /// A seek we asked the engine for that it has not done yet.
    seeking: Option<u32>,
    seek_seq: u32,
    rng: vmath::Rng,
    /// Another Music process handed us a file: come to the front.
    activate: bool,
    perf_frames: u32,
    perf_ns: u64,
    perf_start: u64,
    cover_shadow: vgfx::ShadowTemplate,
    glow: vgfx::ShadowTemplate,
}

impl Player {
    fn new(shared: Arc<Shared>, remote: Option<remote::Server>) -> Player {
        let seed = vrt::time::now_ns();
        Player {
            shared,
            tracks: Vec::new(),
            covers: Arc::new(Mutex::new(Covers {
                specs: Vec::new(),
                large: Vec::new(),
                thumbs: Vec::new(),
                priority: None,
            })),
            art_wake: None,
            current: None,
            selected: None,
            shuffle: false,
            repeat: Repeat::All,
            order: Vec::new(),
            volume: 0.85,
            muted: false,
            status: Status {
                state: PlayState::Idle,
                path: String::new(),
                rate: 0,
                frames: 0,
                pos_frames: 0,
                pos_ns: 0,
                ended: 0,
                error: String::new(),
                device: String::new(),
                load_id: 0,
                seek_id: 0,
            },
            last_ended: 0,
            analyzer: Analyzer::new(FFT_SIZE, 44_100, BANDS, 40.0, 16_000.0),
            analyzer_rate: 44_100,
            window_buf: vec![0.0; FFT_SIZE],
            levels: vec![0.0; BANDS],
            bars: vec![0.0; BANDS],
            peaks: vec![0.0; BANDS],
            last_frame_ns: 0,
            seek_drag: None,
            library_gen: 0,
            was_focused: true,
            remote,
            volume_log_at: None,
            volume_logged: -1,
            requested: None,
            load_seq: 0,
            seeking: None,
            seek_seq: 0,
            rng: vmath::Rng::new(seed),
            activate: false,
            perf_frames: 0,
            perf_ns: 0,
            perf_start: 0,
            cover_shadow: vgfx::ShadowTemplate::new(14, 26),
            glow: vgfx::ShadowTemplate::new(14, 46),
        }
    }

    fn gain(&self) -> f32 {
        if self.muted { 0.0 } else { vaudio::mix::slider_to_gain(self.volume) }
    }

    /// Picks up library changes from the engine (scans, added files and
    /// requests to play a file).
    fn sync_library(&mut self) {
        let (tracks, play) = {
            let mut lib = self.shared.library.lock();
            let changed = lib.generation != self.library_gen;
            self.library_gen = lib.generation;
            (changed.then(|| lib.tracks.clone()), lib.play_request.take())
        };
        if let Some(tracks) = tracks {
            self.set_tracks(tracks);
        }
        if let Some(p) = play
            && let Some(i) = self.tracks.iter().position(|t| t.path == p)
        {
            self.play_index(i);
        }
    }

    /// Replaces the track list, keeping drawn covers and the current and
    /// selected tracks (matched by path).
    fn set_tracks(&mut self, tracks: Vec<Track>) {
        let path_of = |i: Option<usize>| i.and_then(|i| self.tracks.get(i)).map(|t| t.path.clone());
        let (cur, sel) = (path_of(self.current), path_of(self.selected));
        {
            let mut c = self.covers.lock();
            let (mut specs, mut large, mut thumbs) = (Vec::new(), Vec::new(), Vec::new());
            for t in &tracks {
                let spec = ArtSpec::new(t.art.as_deref(), t.seed);
                let old = self.tracks.iter().position(|o| o.path == t.path).filter(|&i| c.specs.get(i) == Some(&spec));
                large.push(old.and_then(|i| c.large.get(i).cloned().flatten()));
                thumbs.push(old.and_then(|i| c.thumbs.get(i).cloned().flatten()));
                specs.push(spec);
            }
            c.specs = specs;
            c.large = large;
            c.thumbs = thumbs;
            c.priority = None;
        }
        let find = |p: Option<String>| p.and_then(|p| tracks.iter().position(|t| t.path == p));
        self.current = find(cur);
        self.selected = find(sel).or(if tracks.is_empty() { None } else { Some(0) });
        self.tracks = tracks;
        self.rebuild_order();
        // Start (or wake) the cover renderer.
        match &self.art_wake {
            Some(w) => {
                let _ = w.signal();
            }
            None => {
                if let Ok(wake) = Event::create()
                    && let (Ok(w2), Ok(n2)) = (wake.0.duplicate(None), self.shared.notify.0.duplicate(None))
                {
                    let covers = self.covers.clone();
                    let (w2, n2) = (Event::from_handle(w2), Event::from_handle(n2));
                    let ok = vrt::thread::Builder::new()
                        .name("covers")
                        .priority(vabi::priority::NORMAL - 2)
                        .spawn(move || art_thread(covers, w2, n2))
                        .is_ok();
                    if ok {
                        self.art_wake = Some(wake);
                    }
                }
            }
        }
    }

    fn rebuild_order(&mut self) {
        let n = self.tracks.len();
        self.order = (0..n).collect();
        if self.shuffle && n > 1 {
            self.rng.shuffle(&mut self.order);
            // Keep the current track first so "next" moves on from it.
            if let Some(c) = self.current
                && let Some(p) = self.order.iter().position(|&i| i == c)
            {
                self.order.swap(0, p);
            }
        }
    }

    fn play_index(&mut self, i: usize) {
        let Some(t) = self.tracks.get(i) else { return };
        self.current = Some(i);
        self.selected = Some(i);
        self.covers.lock().priority = Some(i);
        if let Some(w) = &self.art_wake {
            let _ = w.signal();
        }
        self.shared.send(Command::Volume(self.gain()));
        self.load_seq = self.load_seq.wrapping_add(1).max(1);
        self.shared.send(Command::Load { path: t.path.clone(), play: true, start_ms: 0, id: self.load_seq });
        // Show the new track at once (the engine confirms shortly).
        self.status.path = t.path.clone();
        self.requested = Some(self.load_seq);
        self.status.state = PlayState::Playing;
        self.status.pos_frames = 0;
        self.status.frames = t.duration_ms * t.rate as u64 / 1000;
        self.status.rate = t.rate;
    }

    /// The track `step` positions away in play order (wrapping if `wrap`).
    fn neighbour(&self, step: i32, wrap: bool) -> Option<usize> {
        let n = self.order.len() as i32;
        if n == 0 {
            return None;
        }
        let cur = self.current.and_then(|c| self.order.iter().position(|&i| i == c)).map(|p| p as i32).unwrap_or(-1);
        let mut p = cur + step;
        if p < 0 || p >= n {
            if !wrap {
                return None;
            }
            p = p.rem_euclid(n);
        }
        Some(self.order[p as usize])
    }

    fn next(&mut self, wrap: bool) {
        match self.neighbour(1, wrap) {
            Some(i) => self.play_index(i),
            None => self.shared.send(Command::Pause),
        }
    }

    fn previous(&mut self) {
        let now = vrt::time::now_ns();
        if self.status.position_ms(now) > 3000 || self.order.len() <= 1 {
            self.seek_to(0);
        } else if let Some(i) = self.neighbour(-1, true) {
            self.play_index(i);
        }
    }

    fn toggle_play(&mut self) {
        match self.status.state {
            PlayState::Playing => {
                self.shared.send(Command::Pause);
                self.status.state = PlayState::Paused;
            }
            PlayState::Paused if self.current.is_some() => {
                self.shared.send(Command::Play);
                self.status.state = PlayState::Playing;
            }
            _ => {
                let i = self.selected.or(self.current).unwrap_or(0);
                self.play_index(i);
            }
        }
    }

    fn seek_by(&mut self, delta_ms: i64) {
        if self.current.is_none() {
            return;
        }
        let now = vrt::time::now_ns();
        let pos = self.status.position_ms(now) as i64 + delta_ms;
        let ms = pos.clamp(0, self.status.duration_ms().saturating_sub(500) as i64) as u64;
        self.seek_to(ms);
    }

    /// Jumps to `ms` in the current track. The new position shows at once
    /// and stays until the engine has done the seek: its reports until then
    /// still have the old one.
    fn seek_to(&mut self, ms: u64) {
        self.seek_seq = self.seek_seq.wrapping_add(1).max(1);
        self.shared.send(Command::Seek { ms, id: self.seek_seq });
        self.seeking = Some(self.seek_seq);
        self.status.pos_frames = ms * self.status.rate as u64 / 1000;
        self.status.pos_ns = vrt::time::now_ns();
    }

    fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if self.volume > 0.0 {
            self.muted = false;
        }
        self.shared.send(Command::Volume(self.gain()));
        // Log once the value settles (a slider drag is one change).
        self.volume_log_at = Some(vrt::time::now_ns() + 300_000_000);
    }

    /// Logs the volume if a change has settled.
    fn log_volume(&mut self, ui: &mut Ui) {
        let Some(at) = self.volume_log_at else { return };
        if ui.now() < at {
            ui.repaint_at(at);
            return;
        }
        self.volume_log_at = None;
        let percent = (self.volume * 100.0 + 0.5) as i32;
        if percent != self.volume_logged {
            self.volume_logged = percent;
            vrt::println!("volume {}%", percent);
        }
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.shared.send(Command::Volume(self.gain()));
        vrt::println!("{}", if self.muted { "muted" } else { "unmuted" });
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.rebuild_order();
        vrt::println!("shuffle {}", if self.shuffle { "on" } else { "off" });
    }

    fn handle_keys(&mut self, ui: &Ui, list_focused: bool) {
        for k in &ui.input.keys {
            if k.modifiers & (vproto::display::modifiers::CTRL | vproto::display::modifiers::ALT) != 0 {
                continue;
            }
            match k.code {
                keys::SPACE if !k.repeat => self.toggle_play(),
                keys::LEFT => self.seek_by(-5000),
                keys::RIGHT => self.seek_by(5000),
                keys::UP if !list_focused => self.set_volume(self.volume + 0.05),
                keys::DOWN if !list_focused => self.set_volume(self.volume - 0.05),
                keys::N if !k.repeat => self.next(true),
                keys::P if !k.repeat => self.previous(),
                keys::S if !k.repeat => self.toggle_shuffle(),
                keys::R if !k.repeat => self.cycle_repeat(),
                keys::M if !k.repeat => self.toggle_mute(),
                _ => {}
            }
        }
    }

    fn cycle_repeat(&mut self) {
        self.set_repeat(match self.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        });
    }

    fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
        vrt::println!("repeat {}", repeat.name());
    }

    /// Reacts to engine status changes (a track ended, errors).
    fn sync_status(&mut self) {
        let mut s = self.shared.status();
        // Until the engine has loaded the track we asked for, keep the
        // optimistic local state (and ignore the end of the old track).
        if let Some(req) = self.requested {
            if s.load_id != req {
                self.last_ended = s.ended;
                return;
            }
            self.requested = None;
        }
        // Until it has done the seek we asked for, it reports the old
        // position: keep ours.
        if let Some(id) = self.seeking {
            if s.seek_id == id {
                self.seeking = None;
            } else {
                s.pos_frames = self.status.pos_frames;
                s.pos_ns = self.status.pos_ns;
            }
        }
        if s.ended != self.last_ended {
            self.last_ended = s.ended;
            match self.repeat {
                Repeat::One => {
                    if let Some(c) = self.current {
                        self.play_index(c);
                    }
                }
                Repeat::All => self.next(true),
                Repeat::Off => self.next(false),
            }
            return;
        }
        if s.state == PlayState::Error && self.status.state != PlayState::Error {
            vrt::println!("playback error: {}", s.error);
        }
        self.status = s;
        if let Some(i) = self.tracks.iter().position(|t| t.path == self.status.path) {
            self.current = Some(i);
        }
    }

    fn update_spectrum(&mut self, now: u64) {
        let dt = if self.last_frame_ns == 0 { 0.033 } else { (now.saturating_sub(self.last_frame_ns)) as f32 / 1e9 };
        let dt = dt.clamp(0.0, 0.1);
        self.last_frame_ns = now;
        let playing = self.status.state == PlayState::Playing;
        if playing && self.status.rate > 0 {
            if self.analyzer_rate != self.status.rate {
                self.analyzer = Analyzer::new(FFT_SIZE, self.status.rate, BANDS, 40.0, 16_000.0);
                self.analyzer_rate = self.status.rate;
            }
            self.analyzer.set_range(-68.0, -10.0);
            let pos = self.status.position_at(now);
            let ok = self.shared.viz.lock().window(pos, &mut self.window_buf);
            if ok {
                self.analyzer.analyze(&self.window_buf, &mut self.levels);
            } else {
                self.levels.fill(0.0);
            }
        } else {
            self.levels.fill(0.0);
        }
        for i in 0..BANDS {
            let target = self.levels[i];
            let b = &mut self.bars[i];
            *b = if target > *b { *b + (target - *b) * (dt * 30.0).min(1.0) } else { (*b - dt * 1.6).max(target) };
            let p = &mut self.peaks[i];
            *p = if *b > *p { *b } else { (*p - dt * 0.45).max(0.0) };
        }
    }

    fn animating(&self) -> bool {
        self.status.state == PlayState::Playing || self.bars.iter().chain(&self.peaks).any(|&b| b > 0.002)
    }

    fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.tracks.get(i))
    }

    fn accent(&self) -> (Color, Color) {
        let spec = self.current.and_then(|i| self.covers.lock().specs.get(i).copied());
        match spec {
            Some(s) => {
                let (deep, bright) = art::palette(&s);
                (bright, deep)
            }
            None => (Color::hex(0x5B8CFF), Color::hex(0x8B5CF6)),
        }
    }

    // ---- drawing ----------------------------------------------------------

    fn draw_sidebar(&mut self, ui: &mut Ui, r: Rect, accent: Color) {
        let t = ui.theme().clone();
        ui.canvas.fill_rect(r, Color::hex(0x16161B));
        ui.canvas.fill_rect(Rect::new(r.right() - 1, r.y, 1, r.h), t.border);
        let head = Rect::new(r.x + 20, r.y + 18, r.w - 40, 28);
        ui.icon(Rect::new(head.x, head.y, 24, 28), Icon::Music, 20.0, accent);
        ui.label(Rect::new(head.x + 32, head.y, head.w - 32, 28), "Library", Font::Bold, 20.0, t.text, Align::Left);
        let total: u64 = self.tracks.iter().map(|t| t.duration_ms).sum();
        let sub = if self.tracks.is_empty() {
            String::from("Looking for music…")
        } else {
            let n = self.tracks.len();
            format!("{} track{} · {}", n, if n == 1 { "" } else { "s" }, format_time(total))
        };
        ui.label(
            Rect::new(head.x, head.bottom() + 2, head.w, 20),
            &sub,
            Font::Regular,
            t.small_size,
            t.text_dim,
            Align::Left,
        );
        let list = Rect::new(r.x, r.y + 80, r.w - 1, r.h - 80);
        ui.canvas.fill_rect(Rect::new(r.x + 16, list.y - 1, r.w - 33, 1), t.border);
        if self.tracks.is_empty() {
            let msg = Rect::new(r.x + 20, list.y + 20, r.w - 40, 60);
            ui.paragraph(msg, "Put .qoa or .wav files in Music in your home folder.", t.small_size + 1.0, t.text_faint);
            return;
        }
        let thumbs: Vec<Option<Arc<Bitmap>>> = self.covers.lock().thumbs.clone();
        let tracks = &self.tracks;
        let current = self.current;
        let playing = self.status.state == PlayState::Playing;
        let now_s = (ui.now() / 1_000_000) as f32 / 1000.0;
        let mut selected = self.selected;
        let resp = ui.list(list, "library", tracks.len(), ROW_H, &mut selected, |ui, i, rr, st| {
            let tr = &tracks[i];
            let is_current = current == Some(i);
            let inner = rr.inset(8, 3, 8, 3);
            if is_current {
                ui.canvas.fill_rounded_rect(inner, 10.0, accent.with_alpha(34));
            } else if st.selected {
                ui.canvas.fill_rounded_rect(inner, 10.0, Color::rgba(255, 255, 255, 18));
            } else if st.hovered {
                ui.canvas.fill_rounded_rect(inner, 10.0, Color::rgba(255, 255, 255, 10));
            }
            let th = Rect::new(inner.x + 8, inner.y + (inner.h - THUMB) / 2, THUMB, THUMB);
            match thumbs.get(i).and_then(|b| b.clone()) {
                Some(b) => ui.canvas.draw_bitmap(&b, th.x, th.y, 255),
                None => ui.canvas.fill_rounded_rect(th, 6.0, Color::rgba(255, 255, 255, 20)),
            }
            let tx = th.right() + 12;
            let right_w = 44;
            let tw = inner.right() - tx - right_w;
            let title_color = if is_current { accent.lerp(Color::WHITE, 0.25) } else { Color::hex(0xECECF1) };
            ui.label(Rect::new(tx, inner.y + 9, tw, 20), &tr.title, Font::Bold, 14.0, title_color, Align::Left);
            ui.label(
                Rect::new(tx, inner.y + 29, tw, 18),
                &tr.artist,
                Font::Regular,
                12.0,
                Color::hex(0xA6A6B2),
                Align::Left,
            );
            let dur = Rect::new(inner.right() - right_w - 6, inner.y, right_w, inner.h);
            if is_current {
                widgets::eq_glyph(ui, dur, now_s, playing, accent);
            } else {
                ui.label(dur, &format_time(tr.duration_ms), Font::Regular, 12.0, Color::hex(0x8A8A96), Align::Right);
            }
        });
        self.selected = selected;
        if let Some(i) = resp.activated {
            self.play_index(i);
        }
    }

    fn draw_now_playing(&mut self, ui: &mut Ui, r: Rect, accent: Color, accent2: Color) {
        let t = ui.theme().clone();
        let now = ui.now();
        // Backdrop: the cover's colours fading into the background.
        let split = r.y + r.h * 11 / 20;
        ui.canvas.fill_vertical_gradient(Rect::new(r.x, r.y, r.w, split - r.y), accent2.lerp(t.bg, 0.55), t.bg);
        ui.canvas.fill_rect(Rect::new(r.x, split, r.w, r.bottom() - split), t.bg);
        let pad = 36;
        let area = r.inset(pad, 32, pad, 24);

        // Cover and titles.
        let cover_r = Rect::new(area.x, area.y, COVER, COVER);
        // A coloured glow that breathes with the bass, then the drop shadow.
        let bass = (self.bars[..6].iter().sum::<f32>() / 6.0).clamp(0.0, 1.0);
        if bass > 0.01 {
            let grow = (bass * 12.0) as i32;
            self.glow.draw(&mut ui.canvas, cover_r.inflate(grow), accent.with_alpha((25.0 + bass * 95.0) as u8), false);
        }
        self.cover_shadow.draw(&mut ui.canvas, cover_r.translate(0, 10), Color::rgba(0, 0, 0, 150), true);
        let cover = self.current.and_then(|i| self.covers.lock().large.get(i).and_then(|b| b.clone()));
        match cover {
            Some(b) => ui.canvas.draw_bitmap(&b, cover_r.x, cover_r.y, 255),
            None => {
                ui.canvas.fill_rounded_rect_gradient(cover_r, 14.0, Color::hex(0x2C2C36), Color::hex(0x1E1E26));
                ui.icon(cover_r, Icon::Music, 72.0, Color::rgba(255, 255, 255, 60));
            }
        }
        let info = Rect::new(cover_r.right() + 32, area.y + 8, area.right() - cover_r.right() - 32, COVER - 8);
        let track = self.current_track().cloned();
        let state_label = match self.status.state {
            PlayState::Playing => "NOW PLAYING",
            PlayState::Paused => "PAUSED",
            PlayState::Error => "CANNOT PLAY",
            PlayState::Idle => {
                if track.is_some() {
                    "STOPPED"
                } else {
                    "MUSIC"
                }
            }
        };
        ui.label(
            Rect::new(info.x, info.y + 26, info.w, 18),
            state_label,
            Font::Bold,
            12.0,
            accent.lerp(Color::WHITE, 0.2),
            Align::Left,
        );
        match &track {
            Some(tr) => {
                ui.label(Rect::new(info.x, info.y + 48, info.w, 40), &tr.title, Font::Bold, 30.0, t.text, Align::Left);
                ui.label(
                    Rect::new(info.x, info.y + 92, info.w, 24),
                    &tr.artist,
                    Font::Regular,
                    17.0,
                    t.text.lerp(t.text_dim, 0.4),
                    Align::Left,
                );
                let mut meta = String::new();
                if !tr.album.is_empty() {
                    meta.push_str(&tr.album);
                }
                if tr.year != 0 {
                    meta.push_str(&format!("{}{}", if meta.is_empty() { "" } else { " · " }, tr.year));
                }
                ui.label(
                    Rect::new(info.x, info.y + 118, info.w, 20),
                    &meta,
                    Font::Regular,
                    13.0,
                    t.text_dim,
                    Align::Left,
                );
                // Chips: genre, tempo, format.
                let mut x = info.x;
                let mut chips: Vec<String> = Vec::new();
                if !tr.genre.is_empty() {
                    chips.push(tr.genre.clone());
                }
                if tr.bpm != 0 {
                    chips.push(format!("{} BPM", tr.bpm));
                }
                chips.push(format!("{} · {} kHz", tr.format, tr.rate as f64 / 1000.0));
                for c in &chips {
                    let w = ui.measure(c, Font::Regular, 12.0) as i32 + 20;
                    if x + w > info.right() {
                        break;
                    }
                    let cr = Rect::new(x, info.y + 150, w, 24);
                    ui.canvas.fill_rounded_rect(cr, 12.0, Color::rgba(255, 255, 255, 16));
                    ui.canvas.stroke_rounded_rect(cr, 12.0, 1.0, Color::rgba(255, 255, 255, 26));
                    ui.label(cr, c, Font::Regular, 12.0, t.text_dim, Align::Center);
                    x += w + 8;
                }
                if self.status.state == PlayState::Error {
                    ui.label(
                        Rect::new(info.x, info.y + 184, info.w, 20),
                        &self.status.error,
                        Font::Regular,
                        12.0,
                        t.danger,
                        Align::Left,
                    );
                }
            }
            None => {
                ui.label(
                    Rect::new(info.x, info.y + 48, info.w, 40),
                    "Nothing playing",
                    Font::Bold,
                    30.0,
                    t.text,
                    Align::Left,
                );
                ui.label(
                    Rect::new(info.x, info.y + 92, info.w, 24),
                    "Pick a track from the library or press Space.",
                    Font::Regular,
                    15.0,
                    t.text_dim,
                    Align::Left,
                );
            }
        }

        // Spectrum visualiser.
        let viz_top = cover_r.bottom() + 28;
        let controls_h = 128;
        let viz_h = (area.bottom() - controls_h - viz_top).clamp(40, 220);
        let viz = Rect::new(area.x, viz_top, area.w, viz_h);
        self.draw_spectrum(ui, viz, accent, accent2);

        // Seek bar with times.
        let dur_ms = self.status.duration_ms();
        let pos_ms = self.status.position_ms(now);
        let frac = if dur_ms > 0 { pos_ms as f32 / dur_ms as f32 } else { 0.0 };
        let bar_y = viz.bottom() + 22;
        let bar = Rect::new(area.x + 56, bar_y, area.w - 112, 18);
        let shown_ms = match self.seek_drag {
            Some(f) => (f * dur_ms as f32) as u64,
            None => pos_ms,
        };
        if let Some(f) = widgets::seek_bar(ui, bar, frac, accent, &mut self.seek_drag)
            && self.current.is_some()
            && dur_ms > 0
        {
            let ms = ((f * dur_ms as f32) as u64).min(dur_ms.saturating_sub(250));
            self.seek_to(ms);
        }
        let time_color = t.text_dim;
        ui.label(
            Rect::new(area.x, bar_y, 48, 18),
            &format_time(shown_ms),
            Font::Regular,
            12.0,
            time_color,
            Align::Left,
        );
        let remain = format!("-{}", format_time(dur_ms.saturating_sub(shown_ms)));
        ui.label(Rect::new(area.right() - 48, bar_y, 48, 18), &remain, Font::Regular, 12.0, time_color, Align::Right);

        // Transport.
        let cy = bar_y + 62;
        // Centred, but leave room for the volume control on the right.
        let cx = (area.x + area.w / 2).min(area.right() - (154 + 24 + 40 + 96)).max(area.x + 154);
        let playing = self.status.state == PlayState::Playing;
        if widgets::play_button(ui, (cx, cy), 28, playing, accent, accent2) {
            self.toggle_play();
        }
        let bs = 40;
        let prev_r = Rect::new(cx - 28 - 24 - bs, cy - bs / 2, bs, bs);
        let next_r = Rect::new(cx + 28 + 24, cy - bs / 2, bs, bs);
        if ui.icon_button(prev_r, Icon::Previous, "Previous (P)") {
            self.previous();
        }
        if ui.icon_button(next_r, Icon::Next, "Next (N)") {
            self.next(true);
        }
        let shuffle_r = prev_r.translate(-bs - 22, 0);
        if widgets::toggle_icon(ui, shuffle_r, Icon::Shuffle, self.shuffle, None, "Shuffle (S)", accent) {
            self.toggle_shuffle();
        }
        let repeat_r = next_r.translate(bs + 22, 0);
        let (on, badge, tip) = match self.repeat {
            Repeat::Off => (false, None, "Repeat: off (R)"),
            Repeat::All => (true, None, "Repeat: all (R)"),
            Repeat::One => (true, Some("1"), "Repeat: one (R)"),
        };
        if widgets::toggle_icon(ui, repeat_r, Icon::Repeat, on, badge, tip, accent) {
            self.cycle_repeat();
        }

        // Volume: right-aligned, never overlapping the transport.
        let vol_w = (area.right() - (repeat_r.right() + 24) - 40).clamp(56, 132);
        let vol = Rect::new(area.right() - vol_w, cy - 10, vol_w, 20);
        let mute_r = Rect::new(vol.x - 40, cy - 16, 32, 32);
        let vol_icon = if self.muted || self.volume <= 0.0 { Icon::Mute } else { Icon::Volume };
        if ui.icon_button(mute_r, vol_icon, if self.muted { "Unmute (M)" } else { "Mute (M)" }) {
            self.toggle_mute();
        }
        let mut v = self.volume;
        let saved = ui.ctx.theme.accent;
        ui.ctx.theme.accent = accent;
        if ui.slider(vol, "volume", &mut v, 0.0, 1.0) {
            self.set_volume(v);
        }
        ui.ctx.theme.accent = saved;

        // Output device.
        let dev = match self.status.device.as_str() {
            "" => String::new(),
            "none" => String::from("No sound device: playing silently"),
            d => format!("Output: {}", d),
        };
        let label_w = (cx - 154 - 6 - area.x).min(220);
        if label_w > 90 {
            ui.label(Rect::new(area.x, cy - 10, label_w, 20), &dev, Font::Regular, 11.0, t.text_faint, Align::Left);
        }
    }

    fn draw_spectrum(&mut self, ui: &mut Ui, r: Rect, accent: Color, accent2: Color) {
        let n = BANDS as i32;
        let gap = 4;
        let bw = ((r.w - gap * (n - 1)) / n).max(2);
        let total = bw * n + gap * (n - 1);
        let x0 = r.x + (r.w - total) / 2;
        let base = r.y + r.h * 4 / 5;
        let max_h = base - r.y;
        let refl_h = r.bottom() - base;
        // Baseline.
        ui.canvas.fill_rect(Rect::new(x0, base, total, 1), Color::rgba(255, 255, 255, 22));
        for i in 0..BANDS {
            let x = x0 + i as i32 * (bw + gap);
            let v = self.bars[i];
            let h = ((v * max_h as f32) as i32).max(2);
            let col_top = accent.lerp(Color::WHITE, 0.15 + 0.25 * v);
            let col_bottom = accent2.lerp(accent, 0.3);
            let bar = Rect::new(x, base - h, bw, h);
            ui.canvas.fill_rounded_rect_gradient(bar, (bw as f32 / 2.0).min(3.0), col_top, col_bottom);
            // Reflection.
            let rh = ((h as f32 * 0.35) as i32).min(refl_h - 2);
            if rh > 1 {
                ui.canvas.fill_vertical_gradient(
                    Rect::new(x, base + 2, bw, rh),
                    col_bottom.with_alpha(70),
                    col_bottom.with_alpha(0),
                );
            }
            // Falling peak cap.
            let p = self.peaks[i];
            if p > 0.01 {
                let py = base - (p * max_h as f32) as i32 - 4;
                ui.canvas.fill_rounded_rect(Rect::new(x, py, bw, 2), 1.0, Color::rgba(255, 255, 255, 170));
            }
        }
    }
}

impl App for Player {
    fn agent_info(&self) -> Option<vui::agent::AppAgentInfo> {
        Some(agent::info())
    }

    fn agent_state(&self) -> vui::agent::Value {
        agent::state(self)
    }

    fn agent_invoke(&mut self, action: &str, args: &vui::agent::Value) -> Result<vui::agent::Value, String> {
        Player::agent_invoke(self, action, args)
    }

    fn wait_handles(&self) -> Vec<(RawHandle, u32)> {
        let mut v = vec![(self.shared.notify.raw(), signals::SIGNALED)];
        if let Some(r) = &self.remote {
            r.wait_handles(&mut v);
        }
        v
    }

    fn handle_signaled(&mut self, index: usize, _observed: u32) {
        if index > 0 {
            // Another Music process handed us files to play.
            let files = self.remote.as_mut().map(|r| r.poll()).unwrap_or_default();
            for path in files {
                vrt::println!("opening {}", path);
                self.shared.send(Command::AddFile { path, play: true });
                self.activate = true;
            }
            return;
        }
        let _ = self.shared.notify.clear();
        self.sync_library();
        self.sync_status();
    }

    fn update(&mut self, ui: &mut Ui) {
        let started = vrt::time::now_ns();
        let now = ui.now();
        if core::mem::take(&mut self.activate) {
            ui.activate();
        }
        // Coming back to the window picks up files added in the meantime.
        if ui.input.focused && !self.was_focused {
            self.shared.send(Command::Rescan);
        }
        self.was_focused = ui.input.focused;
        self.sync_library();
        self.sync_status();
        let list_id = ui.id("library") ^ 0x1157;
        let list_focused = ui.focused(list_id);
        self.handle_keys(ui, list_focused);
        self.update_spectrum(now);
        let (accent, accent2) = self.accent();
        let full = ui.rect();
        let (side, main) = full.split_left(SIDEBAR_W.min(full.w / 2));
        self.draw_sidebar(ui, side, accent);
        self.draw_now_playing(ui, main, accent, accent2);

        // Window title follows the track.
        let title = match self.current_track() {
            Some(t) => format!("{} — {}", t.title, t.artist),
            None => String::from("Music"),
        };
        ui.set_title(&title);
        if self.animating() {
            ui.repaint_at(now + 33_000_000);
        }
        self.log_volume(ui);
        // Occasional frame-time report in the system log.
        let spent = vrt::time::now_ns() - started;
        self.perf_frames += 1;
        self.perf_ns += spent;
        if self.perf_start == 0 {
            self.perf_start = now;
        }
        if now - self.perf_start > 10_000_000_000 {
            if self.perf_frames > 30 {
                vrt::println!(
                    "ui: {} frames in {} s, {} us per frame",
                    self.perf_frames,
                    (now - self.perf_start) / 1_000_000_000,
                    self.perf_ns / 1000 / self.perf_frames as u64
                );
            }
            self.perf_frames = 0;
            self.perf_ns = 0;
            self.perf_start = now;
        }
    }
}

fn main() -> i32 {
    let Some(shared) = Shared::new() else {
        vrt::println!("cannot create events");
        return 1;
    };
    let args = vrt::env::args();
    let file = args.get(1).filter(|a| !a.starts_with('-')).cloned();
    // One player at a time: a file goes to the player that is running.
    let mut server = remote::Server::register();
    if server.is_none()
        && let Some(p) = &file
    {
        if remote::forward(p) {
            vrt::println!("handed {} to the running player", p);
            return 0;
        }
        // The other player is gone (or hung): take over.
        server = remote::Server::register();
    }
    let player = Player::new(shared.clone(), server);
    if !engine::start(shared.clone(), player.gain()) {
        vrt::println!("cannot start the decoder thread");
        return 1;
    }
    if let Some(path) = file {
        shared.send(Command::AddFile { path, play: true });
    }
    let mut spec = WindowSpec::new("Music", 1060, 680);
    spec.app_id = "music".into();
    spec.min_width = 900;
    spec.min_height = 600;
    vui::run(spec, player)
}
