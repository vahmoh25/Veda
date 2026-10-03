//! Starfall's HUD and screens: score, level and wave, shield and hull
//! bars, weapon level, reticle, messages, title, pause and game over.

use alloc::format;
use alloc::string::String;

use v3d::app::{Hud, HudFont};
use vgfx::{Color, Path, Rect, StrokeStyle};
use vmath::FloatExt;

const CYAN: u32 = 0xFF4AD8FF;
const TEXT: u32 = 0xFFF0F4FA;
const DIM: u32 = 0xFF9AA6BC;
const PANEL: u32 = 0x9C0A0C16;

pub struct Info<'a> {
    pub score: u64,
    pub high: u64,
    pub multiplier: u32,
    pub level: u32,
    pub wave: u32,
    /// 0..1
    pub shield: f32,
    pub hull: f32,
    pub weapon: u32,
    /// Damage flash 0..1.
    pub hit: f32,
    pub message: Option<(&'a str, f32)>,
}

fn group(n: u64) -> String {
    let s = format!("{}", n);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn bar(h: &mut Hud, r: Rect, value: f32, color: u32, label: &str) {
    h.text(r.x as f32, r.y as f32 - 6.0, 12.0, HudFont::Bold, DIM, 0, label);
    h.panel(r, 4.0, 0x80202838);
    let w = ((r.w - 4) as f32 * value.clamp(0.0, 1.0)) as i32;
    if w > 0 {
        h.panel(Rect::new(r.x + 2, r.y + 2, w, r.h - 4), 3.0, color);
        h.fill(Rect::new(r.x + 2, r.y + 2, w, (r.h - 4) / 3), 0x40FFFFFF);
    }
    h.text(
        (r.x + r.w) as f32 - 6.0,
        (r.y + r.h) as f32 - 4.0,
        11.0,
        HudFont::Bold,
        0xE0FFFFFF,
        2,
        &format!("{}", (value * 100.0) as u32),
    );
}

pub fn playing(h: &mut Hud, i: &Info) {
    let (w, hh) = (h.width as f32, h.height as f32);
    // Damage vignette.
    if i.hit > 0.0 {
        let a = (i.hit * 120.0) as u32;
        for k in 0..6 {
            let inset = k * 10;
            let c = ((a / (k as u32 + 1)) << 24) | 0xFF2020;
            let ww = h.width;
            let hh2 = h.height;
            h.fill(Rect::new(inset, inset, ww - 2 * inset, 10), c);
            h.fill(Rect::new(inset, hh2 - inset - 10, ww - 2 * inset, 10), c);
            h.fill(Rect::new(inset, inset + 10, 10, hh2 - 2 * inset - 20), c);
            h.fill(Rect::new(ww - inset - 10, inset + 10, 10, hh2 - 2 * inset - 20), c);
        }
    }
    // Score.
    h.panel(Rect::new(16, 14, 230, 74), 12.0, PANEL);
    h.fill(Rect::new(16, 14, 5, 74), CYAN);
    h.text(34.0, 38.0, 12.0, HudFont::Bold, DIM, 0, "SCORE");
    h.text(34.0, 74.0, 32.0, HudFont::MonoBold, TEXT, 0, &group(i.score));
    if i.multiplier > 1 {
        h.panel(Rect::new(176, 22, 60, 26), 8.0, 0xE0FFB020);
        h.text(206.0, 41.0, 16.0, HudFont::Bold, 0xFF1A1206, 1, &format!("x{}", i.multiplier));
    }
    // Level / wave.
    let lw = 190;
    h.panel(Rect::new((w as i32 - lw) / 2, 14, lw, 40), 10.0, PANEL);
    h.text(w / 2.0, 40.0, 16.0, HudFont::Bold, TEXT, 1, &format!("LEVEL {}  ·  WAVE {}", i.level, i.wave));
    // High score.
    h.panel(Rect::new(w as i32 - 196, 14, 180, 52), 10.0, PANEL);
    h.text(w - 30.0, 34.0, 12.0, HudFont::Bold, DIM, 2, "HIGH SCORE");
    h.text(w - 30.0, 58.0, 20.0, HudFont::MonoBold, 0xFFFFE070, 2, &group(i.high));
    // Shield and hull.
    let by = hh as i32 - 82;
    h.panel(Rect::new(16, by - 26, 268, 92), 12.0, PANEL);
    bar(h, Rect::new(30, by, 240, 16), i.shield, CYAN, "SHIELD");
    let hull_color = v3d::lerp_color(0xFFFF4030, 0xFF50E070, (i.hull * 256.0) as u32);
    bar(h, Rect::new(30, by + 40, 240, 16), i.hull, hull_color, "HULL");
    // Weapon level.
    h.panel(Rect::new(w as i32 - 164, hh as i32 - 64, 148, 48), 10.0, PANEL);
    h.text(w - 150.0, hh - 34.0, 12.0, HudFont::Bold, DIM, 0, "GUNS");
    for k in 0..3 {
        let x = w - 96.0 + k as f32 * 24.0;
        let on = k < i.weapon;
        h.panel(Rect::new(x as i32, hh as i32 - 48, 18, 18), 4.0, if on { 0xFFFFD040 } else { 0x60FFFFFF });
    }
    if i.hull < 0.25 && i.hull > 0.0 {
        h.text_shadow(w / 2.0, hh - 40.0, 20.0, HudFont::Bold, 0xFFFF5040, 1, "HULL CRITICAL");
    }
    if let Some((msg, a)) = i.message {
        let a8 = (a.clamp(0.0, 1.0) * 255.0) as u32;
        let tw = h.measure(HudFont::Bold, 34.0, msg);
        h.panel(
            Rect::new(((w - tw) / 2.0 - 24.0) as i32, (hh * 0.24) as i32 - 40, (tw + 48.0) as i32, 56),
            12.0,
            ((a8 * 0x9C / 255) << 24) | 0x0A0C16,
        );
        h.text_shadow(w / 2.0, hh * 0.24, 34.0, HudFont::Bold, (a8 << 24) | 0xFFFFFF, 1, msg);
    }
}

pub fn reticle(h: &mut Hud, x: f32, y: f32) {
    let c = 0xB0A0F0FF;
    let mut p = Path::new();
    p.circle(x, y, 11.0);
    h.canvas.stroke_path(&p, &StrokeStyle::new(1.5), Color(c));
    for (dx, dy) in [(1.0f32, 0.0f32), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
        h.line(x + dx * 15.0, y + dy * 15.0, x + dx * 22.0, y + dy * 22.0, 2.0, c);
    }
    h.circle(x, y, 1.6, c);
}

/// The title screen (over a scene the game darkens).
pub fn title(h: &mut Hud, high: u64, t: f64) {
    let (w, hh) = (h.width as f32, h.height as f32);
    let y = hh * 0.33;
    let size = 78.0;
    h.text(w / 2.0 + 4.0, y + 4.0, size, HudFont::Bold, 0xA0000000, 1, "STARFALL");
    h.text(w / 2.0, y, size, HudFont::Bold, 0xFFFFFFFF, 1, "STARFALL");
    let tw = h.measure(HudFont::Bold, size, "STARFALL");
    h.fill(Rect::new((w / 2.0 - tw / 2.0) as i32, y as i32 + 14, tw as i32, 5), CYAN);
    h.fill(Rect::new((w / 2.0 - tw / 2.0) as i32, y as i32 + 22, (tw * 0.45) as i32, 3), 0xFFFF6AD8);
    h.text(w / 2.0, y + 52.0, 15.0, HudFont::Regular, DIM, 1, "DEFEND THE OUTER COLONIES");
    let pulse = (0.6 + 0.4 * ((t * 3.0) as f32).sin()).clamp(0.0, 1.0);
    let a = (pulse * 255.0) as u32;
    let r = Rect::new((w / 2.0) as i32 - 150, (y + 92.0) as i32, 300, 46);
    h.panel(r, 12.0, (((a * 3 / 4) & 0xFF) << 24) | 0x1A6AD0);
    h.text(w / 2.0, y + 122.0, 20.0, HudFont::Bold, 0xFFFFFFFF, 1, "PRESS ENTER TO LAUNCH");
    if high > 0 {
        h.text(w / 2.0, y + 170.0, 15.0, HudFont::Bold, 0xFFFFE070, 1, &format!("HIGH SCORE  {}", group(high)));
    }
    let hint = "Arrows/WASD fly    Space fire    Esc pause    F3 stats";
    let hw = h.measure(HudFont::Regular, 13.0, hint);
    h.panel(Rect::new(((w - hw) / 2.0 - 14.0) as i32, hh as i32 - 44, (hw + 28.0) as i32, 28), 8.0, 0x900A0C16);
    h.text(w / 2.0, hh - 25.0, 13.0, HudFont::Regular, 0xFFD0D8E8, 1, hint);
}

/// Seconds after the ship's destruction until the game over panel appears.
pub const GAME_OVER_PANEL_DELAY: f32 = 0.6;

/// The game over panel, `t` seconds after the ship was destroyed (over a
/// scene the game darkens).
pub fn game_over(h: &mut Hud, score: u64, high: u64, level: u32, t: f32) {
    let (w, hh) = (h.width as f32, h.height as f32);
    if t < GAME_OVER_PANEL_DELAY {
        return;
    }
    let (pw, ph) = (420, 250);
    let new_high = score >= high && score > 0;
    // The panel never changes while it is shown: render it once.
    let key = 0x6A09_E667_F3BC_C909u64
        ^ score.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ high.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ ((level as u64) << 1)
        ^ new_high as u64;
    h.cached(key, (w as i32 - pw) / 2, (hh * 0.26) as i32, pw, ph, |c, t, f| {
        c.fill_rounded_rect(Rect::new(0, 0, pw, ph), 16.0, Color(0xE00A0C16));
        c.fill_rect(Rect::new(0, 0, pw, 5), Color(0xFFFF4A3A));
        let mut centred = |y: f32, size: f32, font: HudFont, color: u32, s: &str| {
            let fi = Hud::font_index(f, font);
            let tw = t.measure(fi, size, s);
            t.draw(c, fi, size, ((pw as f32 - tw) / 2.0 + 0.5) as i32 as f32, y, s, Color(color));
        };
        centred(56.0, 38.0, HudFont::Bold, 0xFFFF6A5A, "GAME OVER");
        centred(100.0, 13.0, HudFont::Bold, DIM, "FINAL SCORE");
        centred(140.0, 36.0, HudFont::MonoBold, TEXT, &group(score));
        let best = if new_high { String::from("NEW HIGH SCORE!") } else { format!("High score {}", group(high)) };
        centred(172.0, 15.0, HudFont::Bold, 0xFFFFE070, &best);
        centred(196.0, 14.0, HudFont::Regular, DIM, &format!("Reached level {}", level));
        centred(230.0, 14.0, HudFont::Regular, 0xFFD0D8E8, "Enter: fly again     Esc: title");
    });
}

/// The pause menu (over a scene the game darkens).
pub fn pause(h: &mut Hud, items: &[String], selected: usize) {
    let (w, hh) = (h.width as f32, h.height as f32);
    h.text_shadow(w / 2.0, hh * 0.3, 34.0, HudFont::Bold, TEXT, 1, "PAUSED");
    for (i, item) in items.iter().enumerate() {
        let y = hh * 0.3 + 30.0 + i as f32 * 48.0;
        let r = Rect::new((w as i32 - 320) / 2, y as i32, 320, 40);
        if i == selected {
            h.panel(r, 10.0, 0xE81A6AD0);
            h.text(w / 2.0, y + 27.0, 18.0, HudFont::Bold, 0xFFFFFFFF, 1, item);
        } else {
            h.panel(r, 10.0, 0xB0101420);
            h.text(w / 2.0, y + 27.0, 18.0, HudFont::Regular, 0xFFE0E6F0, 1, item);
        }
    }
}
