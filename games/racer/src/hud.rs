//! Heads-up display and menus: speedometer, position, laps and times,
//! standings, mini-map, countdown lights, title, pause and results screens.
//!
//! Static parts (panels, labels, the speedometer dial, the mini-map
//! outline) are rendered once into cached bitmaps; only numbers, needles and
//! markers are drawn every frame.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use v3d::app::{Hud, HudFont};
use vgfx::{Canvas, Color, LineCap, Path, Rect, StrokeStyle, Text};
use vmath::{FloatExt, Vec2};

pub const ACCENT: u32 = 0xFFFF5A36;
pub const PANEL: u32 = 0xA8101418;
pub const TEXT: u32 = 0xFFF4F6F8;
pub const DIM: u32 = 0xFFA8B0BC;

/// `m:ss.mmm`.
pub fn fmt_time(t: f32) -> String {
    let ms = (t.max(0.0) * 1000.0) as u32;
    format!("{}:{:02}.{:03}", ms / 60000, (ms / 1000) % 60, ms % 1000)
}

fn ordinal(n: usize) -> &'static str {
    match n {
        1 => "ST",
        2 => "ND",
        3 => "RD",
        _ => "TH",
    }
}

/// Cache keys of the static HUD layers.
mod keys {
    pub const POSITION: u64 = 1;
    pub const LAP: u64 = 2;
    pub const TIMES: u64 = 3;
    pub const STANDINGS: u64 = 4;
    pub const DIAL: u64 = 5;
    pub const MAP: u64 = 6;
    pub const LIGHTS: u64 = 7;
}

/// Text in a cached layer.
fn label(c: &mut Canvas, text: &mut Text, fonts: [usize; 4], x: f32, y: f32, size: f32, color: u32, s: &str) {
    let f = Hud::font_index(fonts, HudFont::Bold);
    text.draw(c, f, size, x, y, s, Color(color));
}

/// Aligned text in a cached layer (`align` as in [`Hud::text`]).
fn label_aligned(
    c: &mut Canvas,
    text: &mut Text,
    fonts: [usize; 4],
    x: f32,
    y: f32,
    size: f32,
    font: HudFont,
    color: u32,
    align: u8,
    s: &str,
) {
    let f = Hud::font_index(fonts, font);
    let w = text.measure(f, size, s);
    let x = match align {
        1 => x - w / 2.0,
        2 => x - w,
        _ => x,
    };
    text.draw(c, f, size, (x + 0.5) as i32 as f32, y, s, Color(color));
}

/// The track outline fitted into a unit square.
pub struct MiniMap {
    pub points: Vec<Vec2>,
    min: Vec2,
    scale: f32,
    /// Identifies the track for the cached outline.
    pub id: u64,
}

impl MiniMap {
    pub fn new(outline: &[Vec2], id: u64) -> MiniMap {
        let mut min = Vec2::splat(f32::MAX);
        let mut max = Vec2::splat(f32::MIN);
        for p in outline {
            min = min.min(*p);
            max = max.max(*p);
        }
        let size = max - min;
        let scale = 1.0 / size.x.max(size.y).max(1.0);
        let points = outline.iter().step_by(2).map(|p| (*p - min) * scale).collect();
        MiniMap { points, min, scale, id }
    }

    /// World (x, z) to unit map coordinates.
    pub fn to_map(&self, p: Vec2) -> Vec2 {
        (p - self.min) * self.scale
    }
}

/// One line of the standings.
pub struct Standing {
    pub name: &'static str,
    pub color: u32,
    pub player: bool,
    pub finished: bool,
}

/// What the race HUD shows.
pub struct RaceInfo<'a> {
    pub position: usize,
    pub cars: usize,
    pub lap: u32,
    pub laps: u32,
    pub speed_kmh: f32,
    pub rpm: f32,
    pub gear: u32,
    pub race_time: f32,
    pub lap_time: f32,
    pub best_lap: Option<f32>,
    pub standings: &'a [Standing],
    pub map: &'a MiniMap,
    /// World (x, z), colour, is the player.
    pub dots: &'a [(Vec2, u32, bool)],
    pub message: Option<(&'a str, f32)>,
    pub wrong_way: bool,
}

pub fn race(hud: &mut Hud, info: &RaceInfo) {
    let (w, h) = (hud.width as f32, hud.height as f32);
    // Position (top left).
    hud.cached(keys::POSITION, 18, 16, 176, 92, |c, t, f| {
        c.fill_rounded_rect(Rect::new(0, 0, 176, 92), 12.0, Color(PANEL));
        c.fill_rect(Rect::new(0, 0, 6, 92), Color(ACCENT));
        label(c, t, f, 22.0, 34.0, 13.0, DIM, "POSITION");
    });
    let pw = hud.text(38.0, 96.0, 46.0, HudFont::Bold, TEXT, 0, &format!("{}", info.position));
    hud.text(40.0 + pw, 72.0, 16.0, HudFont::Bold, TEXT, 0, ordinal(info.position));
    hud.text(40.0 + pw + 28.0, 96.0, 20.0, HudFont::Bold, DIM, 0, &format!("/ {}", info.cars));
    // Lap.
    hud.cached(keys::LAP, 204, 16, 128, 92, |c, t, f| {
        c.fill_rounded_rect(Rect::new(0, 0, 128, 92), 12.0, Color(PANEL));
        label(c, t, f, 16.0, 34.0, 13.0, DIM, "LAP");
    });
    let lap = info.lap.clamp(1, info.laps);
    let lw = hud.text(218.0, 96.0, 40.0, HudFont::Bold, TEXT, 0, &format!("{}", lap));
    hud.text(222.0 + lw, 96.0, 20.0, HudFont::Bold, DIM, 0, &format!("/ {}", info.laps));

    // Times (top right).
    let tx = w as i32 - 250;
    hud.cached(keys::TIMES, tx, 16, 232, 104, |c, t, f| {
        c.fill_rounded_rect(Rect::new(0, 0, 232, 104), 12.0, Color(PANEL));
        for (i, l) in ["TIME", "LAP", "BEST"].iter().enumerate() {
            label(c, t, f, 14.0, 30.0 + i as f32 * 28.0, 13.0, DIM, l);
        }
    });
    let x1 = w - 30.0;
    let best = info.best_lap.map(fmt_time).unwrap_or_else(|| String::from("-:--.---"));
    hud.text(x1, 46.0, 19.0, HudFont::MonoBold, TEXT, 2, &fmt_time(info.race_time));
    hud.text(x1, 74.0, 19.0, HudFont::MonoBold, TEXT, 2, &fmt_time(info.lap_time));
    hud.text(x1, 102.0, 19.0, HudFont::MonoBold, 0xFF7CE08A, 2, &best);

    // Standings (left, under the position).
    let n = info.standings.len() as i32;
    let sh = 14 + 24 * n;
    hud.cached(keys::STANDINGS + ((n as u64) << 8), 18, 118, 176, sh, |c, _, _| {
        c.fill_rounded_rect(Rect::new(0, 0, 176, sh), 10.0, Color(0x90101418));
    });
    for (i, s) in info.standings.iter().enumerate() {
        let y = 140.0 + i as f32 * 24.0;
        if s.player {
            hud.panel(Rect::new(22, y as i32 - 17, 168, 22), 6.0, 0x50FFFFFF);
        }
        hud.text(30.0, y, 14.0, HudFont::Bold, DIM, 0, &format!("{}", i + 1));
        hud.fill(Rect::new(51, y as i32 - 10, 10, 10), s.color);
        let name_color = if s.player { 0xFFFFFFFF } else { 0xFFDCE2EA };
        hud.text(68.0, y, 14.0, if s.player { HudFont::Bold } else { HudFont::Regular }, name_color, 0, s.name);
        if s.finished {
            hud.text(182.0, y, 12.0, HudFont::Bold, 0xFF7CE08A, 2, "FIN");
        }
    }

    speedometer(hud, w - 112.0, h - 108.0, info);
    minimap(hud, Rect::new(18, h as i32 - 200, 182, 182), info);

    if let Some((msg, alpha)) = info.message {
        banner(hud, msg, alpha);
    }
    if info.wrong_way {
        hud.panel(Rect::new((w / 2.0) as i32 - 150, (h * 0.3) as i32 - 34, 300, 52), 10.0, 0xC8A01818);
        hud.text(w / 2.0, h * 0.3, 30.0, HudFont::Bold, 0xFFFFFFFF, 1, "WRONG WAY");
    }
}

/// A big centred message with a fading panel.
pub fn banner(hud: &mut Hud, msg: &str, alpha: f32) {
    let (w, h) = (hud.width as f32, hud.height as f32);
    let a = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
    let tw = hud.measure(HudFont::Bold, 40.0, msg);
    let r = Rect::new(((w - tw) / 2.0 - 28.0) as i32, (h * 0.22) as i32 - 46, (tw + 56.0) as i32, 66);
    hud.panel(r, 12.0, ((a * 0xA8 / 255) << 24) | 0x101418);
    hud.fill(Rect::new(r.x, r.y + r.h - 4, r.w, 4), (a << 24) | (ACCENT & 0xFFFFFF));
    hud.text_shadow(w / 2.0, h * 0.22, 40.0, HudFont::Bold, (a << 24) | 0xFFFFFF, 1, msg);
}

const DIAL_R: f32 = 82.0;
const DIAL_START: f32 = 0.75 * vmath::PI;
const DIAL_SWEEP: f32 = 1.5 * vmath::PI;
const DIAL_MAX: f32 = 260.0;

fn speedometer(hud: &mut Hud, cx: f32, cy: f32, info: &RaceInfo) {
    let half = 100;
    let (ox, oy) = (cx as i32 - half, cy as i32 - half);
    hud.cached(keys::DIAL, ox, oy, 2 * half, 2 * half, |c, t, f| {
        let (lx, ly) = (half as f32, half as f32);
        c.fill_circle(lx, ly, DIAL_R + 14.0, Color(0xB0101418));
        let mut bg = Path::new();
        bg.arc(lx, ly, DIAL_R, DIAL_START, DIAL_SWEEP);
        c.stroke_path(&bg, &StrokeStyle::new(9.0).with_cap(LineCap::Round), Color(0x40FFFFFF));
        for k in 0..=13 {
            let a = DIAL_START + DIAL_SWEEP * (k as f32 * 20.0 / DIAL_MAX);
            let (s, co) = a.sin_cos();
            let (r0, r1) = if k % 2 == 0 { (DIAL_R - 22.0, DIAL_R - 12.0) } else { (DIAL_R - 18.0, DIAL_R - 12.0) };
            c.draw_line(lx + co * r0, ly + s * r0, lx + co * r1, ly + s * r1, 2.0, Color(0xC0FFFFFF));
        }
        let bold = Hud::font_index(f, HudFont::Bold);
        let kw = t.measure(bold, 11.0, "KM/H");
        t.draw(c, bold, 11.0, lx - kw / 2.0, ly + 54.0, "KM/H", Color(DIM));
        c.fill_rect(Rect::new(half - 34, half + 62, 68, 5), Color(0x40FFFFFF));
    });
    let t = (info.speed_kmh / DIAL_MAX).clamp(0.0, 1.0);
    if t > 0.005 {
        let mut fg = Path::new();
        fg.arc(cx, cy, DIAL_R, DIAL_START, DIAL_SWEEP * t);
        let col = v3d::lerp_color(0xFF3AD8FF, ACCENT, (t * 256.0) as u32);
        hud.canvas.stroke_path(&fg, &StrokeStyle::new(9.0).with_cap(LineCap::Round), Color(col));
    }
    // Needle.
    let a = DIAL_START + DIAL_SWEEP * t;
    let (s, c) = a.sin_cos();
    hud.line(cx - c * 10.0, cy - s * 10.0, cx + c * (DIAL_R - 16.0), cy + s * (DIAL_R - 16.0), 3.0, ACCENT);
    hud.fill(Rect::new(cx as i32 - 5, cy as i32 - 5, 10, 10), 0xFF20242A);
    hud.text(cx, cy + 36.0, 30.0, HudFont::MonoBold, TEXT, 1, &format!("{}", info.speed_kmh as u32));
    hud.text(cx + 52.0, cy - 30.0, 22.0, HudFont::Bold, TEXT, 1, &format!("{}", info.gear));
    let fill = (68.0 * info.rpm.clamp(0.0, 1.0)) as i32;
    hud.fill(Rect::new(cx as i32 - 34, cy as i32 + 62, fill, 5), if info.rpm > 0.9 { 0xFFFF4040 } else { 0xFF3AD8FF });
}

fn minimap(hud: &mut Hud, r: Rect, info: &RaceInfo) {
    let map = info.map;
    let inner = 16;
    let size = (r.w - 2 * inner).min(r.h - 2 * inner) as f32;
    let points = &map.points;
    hud.cached(keys::MAP + (map.id << 8), r.x, r.y, r.w, r.h, |c, _, _| {
        c.fill_rounded_rect(Rect::new(0, 0, r.w, r.h), 12.0, Color(PANEL));
        let pt = |p: Vec2| (inner as f32 + p.x * size, inner as f32 + p.y * size);
        let mut path = Path::new();
        for (i, p) in points.iter().enumerate() {
            let (x, y) = pt(*p);
            if i == 0 {
                path.move_to(x, y);
            } else {
                path.line_to(x, y);
            }
        }
        path.close();
        c.stroke_path(&path, &StrokeStyle::new(6.0), Color(0x60000000));
        c.stroke_path(&path, &StrokeStyle::new(3.0), Color(0xE8E8ECF0));
        if let Some(first) = points.first() {
            let (x, y) = pt(*first);
            c.fill_rect(Rect::new(x as i32 - 4, y as i32 - 4, 8, 8), Color(0xFFFFFFFF));
        }
    });
    let (ox, oy) = ((r.x + inner) as f32, (r.y + inner) as f32);
    for pass in 0..2 {
        for (p, color, player) in info.dots {
            if (*player as i32) != pass {
                continue;
            }
            let m = map.to_map(*p);
            let (x, y) = (ox + m.x * size, oy + m.y * size);
            if *player {
                hud.circle(x, y, 7.0, 0xFFFFFFFF);
            }
            hud.circle(x, y, 5.0, *color);
        }
    }
}

/// Start lights: `lit` red lights (0..=3), or all green.
pub fn countdown(hud: &mut Hud, lit: u32, go: bool, alpha: f32) {
    let (w, h) = (hud.width as f32, hud.height as f32);
    let a = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
    let pw = 260;
    let r = Rect::new((w as i32 - pw) / 2, (h * 0.16) as i32, pw, 96);
    // Each state of the lights is a cached layer.
    let state = if go { 4 } else { lit.min(3) as u64 };
    hud.cached(keys::LIGHTS + (state << 8), r.x, r.y, r.w, r.h, |c, _, _| {
        c.fill_rounded_rect(Rect::new(0, 0, pw, 96), 16.0, Color(0xC80C0E12));
        for i in 0..3 {
            let cx = 60.0 + i as f32 * 70.0;
            c.fill_circle(cx, 48.0, 27.0, Color(0xFF05060A));
            let on = go || (i as u32) < lit;
            let color = if go {
                0xFF3AE86A
            } else if on {
                0xFFFF2A2A
            } else {
                0xFF3A2020
            };
            c.fill_circle(cx, 48.0, 22.0, Color(color));
            if on {
                c.fill_circle(cx - 6.0, 41.0, 6.0, Color(0x55FFFFFF));
            }
        }
    });
    if go {
        hud.text_shadow(w / 2.0, r.y as f32 + 160.0, 64.0, HudFont::Bold, (a << 24) | 0x7CF09A, 1, "GO!");
    }
}

/// A centred menu with a title; `selected` is highlighted.
pub fn menu(hud: &mut Hud, title: &str, subtitle: &str, items: &[String], selected: usize, y: f32) {
    let w = hud.width as f32;
    if !title.is_empty() {
        hud.text_shadow(w / 2.0, y, 30.0, HudFont::Bold, TEXT, 1, title);
    }
    if !subtitle.is_empty() {
        hud.text(w / 2.0, y + 26.0, 14.0, HudFont::Regular, DIM, 1, subtitle);
    }
    let iw = 340;
    menu_items(hud, items, selected, (w as i32 - iw) / 2, y + 56.0, iw);
}

/// Menu items in a column of `width` starting at (`x`, `y`).
pub fn menu_items(hud: &mut Hud, items: &[String], selected: usize, x: i32, y: f32, width: i32) {
    let cx = x as f32 + width as f32 / 2.0;
    for (i, item) in items.iter().enumerate() {
        let iy = y + i as f32 * 46.0;
        let r = Rect::new(x, iy as i32, width, 38);
        if i == selected {
            hud.panel(r, 10.0, 0xE8FF5A36);
            hud.text(cx, iy + 26.0, 18.0, HudFont::Bold, 0xFFFFFFFF, 1, item);
        } else {
            hud.panel(r, 10.0, 0xB0141820);
            hud.text(cx, iy + 26.0, 18.0, HudFont::Regular, 0xFFE0E6EE, 1, item);
        }
    }
}

/// The game title with a shadow and accent underlines, left-aligned at `x`.
pub fn logo(hud: &mut Hud, x: f32, y: f32) {
    let size = 64.0;
    let tw = hud.measure(HudFont::Bold, size, "VELOCITY");
    hud.text(x + 4.0, y + 4.0, size, HudFont::Bold, 0x90000000, 0, "VELOCITY");
    hud.text(x, y, size, HudFont::Bold, 0xFFFFFFFF, 0, "VELOCITY");
    let bar = Rect::new(x as i32 + 2, y as i32 + 14, tw as i32, 6);
    hud.fill(bar, ACCENT);
    hud.fill(Rect::new(bar.x, bar.y + 8, (tw * 0.6) as i32, 3), 0xFF3AD8FF);
}

/// The selected circuit's outline with the cars on it (title screen).
pub fn track_preview(hud: &mut Hud, r: Rect, name: &str, length: f32, map: &MiniMap, dots: &[(Vec2, u32, bool)]) {
    let info = RaceInfo {
        position: 1,
        cars: 1,
        lap: 1,
        laps: 1,
        speed_kmh: 0.0,
        rpm: 0.0,
        gear: 1,
        race_time: 0.0,
        lap_time: 0.0,
        best_lap: None,
        standings: &[],
        map,
        dots,
        message: None,
        wrong_way: false,
    };
    minimap(hud, r, &info);
    hud.text(r.x as f32 + r.w as f32 / 2.0, (r.y + r.h) as f32 + 22.0, 15.0, HudFont::Bold, TEXT, 1, name);
    let km = format!("{:.2} km", length / 1000.0);
    hud.text(r.x as f32 + r.w as f32 / 2.0, (r.y + r.h) as f32 + 40.0, 12.0, HudFont::Regular, DIM, 1, &km);
}

/// Bottom hint line.
pub fn hint(hud: &mut Hud, text: &str) {
    let (w, h) = (hud.width as f32, hud.height as f32);
    let tw = hud.measure(HudFont::Regular, 13.0, text);
    hud.panel(Rect::new(((w - tw) / 2.0 - 14.0) as i32, h as i32 - 44, (tw + 28.0) as i32, 28), 8.0, 0x90101418);
    hud.text(w / 2.0, h - 25.0, 13.0, HudFont::Regular, 0xFFD0D8E2, 1, text);
}

/// One row of the results table.
pub struct ResultRow {
    pub name: &'static str,
    pub color: u32,
    pub player: bool,
    pub total: Option<f32>,
    pub best: Option<f32>,
}

pub fn results(hud: &mut Hud, rows: &[ResultRow], track: &str) {
    let pw = 560;
    let ph = 120 + rows.len() as i32 * 40;
    // Centred, and clear of the top HUD panels when the window is tall
    // enough.
    let top = ((hud.height - ph) / 2).clamp(12, 160);
    // The table only changes when a car finishes: render it once per state.
    let mut key = 0xCBF2_9CE4_8422_2325u64;
    let mut mix = |v: u64| key = (key ^ v).wrapping_mul(0x0000_0100_0000_01B3);
    track.bytes().for_each(|b| mix(b as u64));
    for row in rows {
        row.name.bytes().for_each(|b| mix(b as u64));
        mix(row.color as u64);
        mix(row.player as u64);
        mix(row.total.map_or(u64::MAX, |t| t.to_bits() as u64));
        mix(row.best.map_or(u64::MAX, |t| t.to_bits() as u64));
    }
    hud.cached(key, (hud.width - pw) / 2, top, pw, ph, |c, t, f| {
        c.fill_rounded_rect(Rect::new(0, 0, pw, ph), 16.0, Color(0xE0101418));
        c.fill_rect(Rect::new(0, 0, pw, 5), Color(ACCENT));
        let mid = pw as f32 / 2.0;
        label_aligned(c, t, f, mid, 46.0, 28.0, HudFont::Bold, TEXT, 1, "RACE RESULTS");
        label_aligned(c, t, f, mid, 68.0, 13.0, HudFont::Regular, DIM, 1, track);
        let (cx0, cx1, cx2, cx3) = (36.0, 80.0, 400.0, pw as f32 - 30.0);
        label_aligned(c, t, f, cx2, 100.0, 12.0, HudFont::Bold, DIM, 2, "TOTAL");
        label_aligned(c, t, f, cx3, 100.0, 12.0, HudFont::Bold, DIM, 2, "BEST LAP");
        for (i, row) in rows.iter().enumerate() {
            let y = 136.0 + i as f32 * 40.0;
            if row.player {
                c.fill_rounded_rect(Rect::new(14, y as i32 - 26, pw - 28, 36), 8.0, Color(0x40FF5A36));
            }
            let medal = [0xFFF2C23A, 0xFFC8D0DA, 0xFFD08A4A];
            let pc = if i < 3 { medal[i] } else { TEXT };
            label_aligned(c, t, f, cx0, y, 20.0, HudFont::Bold, pc, 1, &format!("{}", i + 1));
            c.fill_rect(Rect::new(cx1 as i32 - 7, y as i32 - 13, 14, 14), Color(row.color));
            let font = if row.player { HudFont::Bold } else { HudFont::Regular };
            label_aligned(c, t, f, cx1 + 18.0, y, 18.0, font, TEXT, 0, row.name);
            let total = row.total.map(fmt_time).unwrap_or_else(|| String::from("DNF"));
            label_aligned(c, t, f, cx2, y, 17.0, HudFont::MonoBold, TEXT, 2, &total);
            let best = row.best.map(fmt_time).unwrap_or_else(|| String::from("-"));
            label_aligned(c, t, f, cx3, y, 17.0, HudFont::MonoBold, 0xFF7CE08A, 2, &best);
        }
    });
}
