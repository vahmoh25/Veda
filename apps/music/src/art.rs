//! Procedurally generated cover art.
//!
//! Every track gets a deterministic cover from a style and a seed. Tracks
//! made by `musicgen` carry an `art` tag (`style:hue:seed`) chosen to fit
//! the music; other files get a style derived from a hash of their title.
//! Covers are drawn with ordinary canvas operations (gradients, circles,
//! paths), which are cheap even under emulation, on a background thread.

use alloc::vec::Vec;
use core::f32::consts::TAU;

use vgfx::{Bitmap, Canvas, Color, FillRule, Path, Rect, StrokeStyle};
use vmath::{FloatExt, Rng};

/// Cover art styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Synthwave,
    Lofi,
    Aurora,
    Pixel,
    Pulse,
    Waves,
}

impl Style {
    fn from_name(s: &str) -> Option<Style> {
        Some(match s {
            "synthwave" => Style::Synthwave,
            "lofi" => Style::Lofi,
            "aurora" => Style::Aurora,
            "pixel" => Style::Pixel,
            "pulse" => Style::Pulse,
            "waves" => Style::Waves,
            _ => return None,
        })
    }
}

/// What to draw for a track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArtSpec {
    pub style: Style,
    /// Base hue in turns (0..1).
    pub hue: f32,
    pub seed: u64,
}

impl ArtSpec {
    /// From an `art` tag (`style:hue_degrees:seed`) or, failing that, a hash.
    pub fn new(tag: Option<&str>, hash: u64) -> ArtSpec {
        if let Some(tag) = tag {
            let mut it = tag.split(':');
            if let Some(style) = it.next().and_then(Style::from_name) {
                let hue = it.next().and_then(|h| h.parse::<f32>().ok()).unwrap_or(0.0) / 360.0;
                let seed = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(hash);
                return ArtSpec { style, hue, seed };
            }
        }
        let styles = [Style::Synthwave, Style::Lofi, Style::Aurora, Style::Pixel, Style::Pulse, Style::Waves];
        ArtSpec { style: styles[(hash % 6) as usize], hue: ((hash >> 8) % 360) as f32 / 360.0, seed: hash }
    }
}

/// A color from hue (turns), saturation and value.
pub fn hsv(h: f32, s: f32, v: f32) -> Color {
    let (r, g, b) = vmath::color::hsv_to_rgb(h, s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    Color::rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// The two colors that best represent a cover (for backdrops and accents).
pub fn palette(spec: &ArtSpec) -> (Color, Color) {
    let h = spec.hue;
    match spec.style {
        Style::Synthwave => (hsv(h, 0.75, 0.55), hsv(h + 0.12, 0.8, 0.95)),
        Style::Lofi => (hsv(h, 0.45, 0.5), hsv(h + 0.08, 0.5, 0.95)),
        Style::Aurora => (hsv(h, 0.7, 0.4), hsv(h + 0.15, 0.6, 0.95)),
        Style::Pixel => (hsv(h, 0.7, 0.6), hsv(h + 0.5, 0.8, 1.0)),
        Style::Pulse => (hsv(h, 0.8, 0.5), hsv(h + 0.3, 0.8, 1.0)),
        Style::Waves => (hsv(h, 0.6, 0.5), hsv(h + 0.1, 0.5, 0.95)),
    }
}

/// Renders a `size` x `size` cover.
pub fn render(spec: &ArtSpec, size: i32) -> Bitmap {
    let mut bmp = Bitmap::new(size, size);
    {
        let mut c = Canvas::for_bitmap(&mut bmp);
        let mut rng = Rng::new(spec.seed);
        let s = size as f32;
        match spec.style {
            Style::Synthwave => synthwave(&mut c, s, spec.hue, &mut rng),
            Style::Lofi => lofi(&mut c, s, spec.hue, &mut rng),
            Style::Aurora => aurora(&mut c, s, spec.hue, &mut rng),
            Style::Pixel => pixel(&mut c, s, spec.hue, &mut rng),
            Style::Pulse => pulse(&mut c, s, spec.hue, &mut rng),
            Style::Waves => waves(&mut c, s, spec.hue, &mut rng),
        }
        vignette(&mut c, size);
    }
    bmp
}

fn r(x: f32) -> i32 {
    x as i32
}

/// Soft radial glow: stacked translucent circles.
fn glow(c: &mut Canvas, cx: f32, cy: f32, radius: f32, color: Color, strength: f32) {
    let steps = 14;
    for i in 0..steps {
        let t = i as f32 / steps as f32;
        let rr = radius * (1.0 - t * 0.85);
        let a = (strength * 255.0 * 0.08 * (0.3 + t)).clamp(0.0, 255.0) as u8;
        c.fill_circle(cx, cy, rr, color.with_alpha(a));
    }
}

fn stars(c: &mut Canvas, s: f32, max_y: f32, count: usize, rng: &mut Rng) {
    for _ in 0..count {
        let x = rng.range_f32(0.0, s);
        let y = rng.range_f32(0.0, max_y);
        let b = rng.range_f32(0.3, 1.0);
        let a = (b * 220.0) as u8;
        if rng.chance(0.15) {
            c.fill_circle(x, y, s * 0.006 + 0.6, Color::rgba(255, 255, 255, a));
        } else {
            c.fill_rect(Rect::new(r(x), r(y), 1, 1), Color::rgba(255, 255, 255, a));
        }
    }
}

fn vignette(c: &mut Canvas, size: i32) {
    let band = size / 5;
    c.fill_vertical_gradient(Rect::new(0, 0, size, band), Color::rgba(0, 0, 0, 70), Color::rgba(0, 0, 0, 0));
    c.fill_vertical_gradient(Rect::new(0, size - band, size, band), Color::rgba(0, 0, 0, 0), Color::rgba(0, 0, 0, 90));
}

fn synthwave(c: &mut Canvas, s: f32, hue: f32, rng: &mut Rng) {
    let horizon = s * 0.64;
    let top = hsv(hue + 0.70, 0.85, 0.12);
    let mid = hsv(hue + 0.82, 0.8, 0.38);
    let low = hsv(hue + 0.95, 0.75, 0.85);
    let sky_mid = r(horizon * 0.55);
    c.fill_vertical_gradient(Rect::new(0, 0, r(s), sky_mid), top, mid);
    c.fill_vertical_gradient(Rect::new(0, sky_mid, r(s), r(horizon) - sky_mid), mid, low);
    stars(c, s, horizon * 0.6, 70, rng);
    // The sun, with stripes cut out of its lower half.
    let (cx, cy, rad) = (s * 0.5, horizon - s * 0.03, s * 0.25);
    glow(c, cx, cy, rad * 1.7, hsv(hue + 0.95, 0.8, 1.0), 1.0);
    let sun_top = hsv(hue + 0.13, 0.85, 1.0);
    let sun_bot = hsv(hue + 0.93, 0.85, 1.0);
    let sun_rect = Rect::new(r(cx - rad), r(cy - rad), r(rad * 2.0), r(rad * 2.0));
    c.save();
    c.clip_to(Rect::new(0, 0, r(s), r(horizon)));
    c.fill_rounded_rect_gradient(sun_rect, rad, sun_top, sun_bot);
    let mut y = cy - rad * 0.1;
    let mut gap = s * 0.008;
    while y < cy + rad {
        let t = ((y - sky_mid as f32) / (horizon - sky_mid as f32)).clamp(0.0, 1.0);
        c.fill_rect(Rect::new(r(cx - rad - 2.0), r(y), r(rad * 2.0 + 4.0), r(gap).max(1)), mid.lerp(low, t));
        y += gap + s * 0.03;
        gap *= 1.35;
    }
    c.restore();
    // Distant mountains.
    let mut p = Path::new();
    p.move_to(0.0, horizon);
    let mut x = 0.0;
    while x <= s {
        let peak = horizon - rng.range_f32(s * 0.02, s * 0.12);
        p.line_to(x + s * 0.06, peak);
        x += s * 0.12;
        p.line_to(x.min(s), horizon - rng.range_f32(0.0, s * 0.03));
    }
    p.line_to(s, horizon);
    p.close();
    c.fill_path(&p, hsv(hue + 0.75, 0.7, 0.18), FillRule::NonZero);
    // The floor and its neon grid.
    c.fill_vertical_gradient(
        Rect::new(0, r(horizon), r(s), r(s - horizon) + 1),
        hsv(hue + 0.78, 0.8, 0.16),
        hsv(hue + 0.72, 0.9, 0.05),
    );
    let grid = hsv(hue + 0.5, 0.75, 1.0);
    c.fill_rect(Rect::new(0, r(horizon), r(s), 2), hsv(hue + 0.95, 0.6, 1.0));
    for i in 1..9 {
        let t = i as f32 / 9.0;
        let yy = horizon + (s - horizon) * t * t;
        c.fill_rect(Rect::new(0, r(yy), r(s), 1), grid.with_alpha((60.0 + 150.0 * t) as u8));
    }
    for i in -10..=10 {
        let xb = s * 0.5 + i as f32 * s * 0.13;
        c.draw_line(s * 0.5 + i as f32 * s * 0.012, horizon, xb, s, 1.0, grid.with_alpha(150));
    }
}

fn lofi(c: &mut Canvas, s: f32, hue: f32, rng: &mut Rng) {
    let top = hsv(hue + 0.72, 0.45, 0.30);
    let bottom = hsv(hue + 0.02, 0.45, 0.85);
    c.fill_vertical_gradient(Rect::new(0, 0, r(s), r(s)), top, bottom);
    stars(c, s, s * 0.4, 25, rng);
    // Moon.
    let (mx, my, mr) = (s * 0.68, s * 0.3, s * 0.12);
    glow(c, mx, my, mr * 2.6, hsv(hue + 0.1, 0.25, 1.0), 0.8);
    c.fill_circle(mx, my, mr, hsv(hue + 0.1, 0.12, 0.98));
    c.fill_circle(mx + mr * 0.35, my - mr * 0.15, mr * 0.18, hsv(hue + 0.1, 0.15, 0.88));
    c.fill_circle(mx - mr * 0.3, my + mr * 0.35, mr * 0.12, hsv(hue + 0.1, 0.15, 0.9));
    // City skyline with lit windows.
    let base = s * 0.8;
    let mut x = -s * 0.02;
    while x < s {
        let w = rng.range_f32(s * 0.07, s * 0.15);
        let h = rng.range_f32(s * 0.12, s * 0.38);
        let col = hsv(hue + 0.72, 0.35, rng.range_f32(0.12, 0.2));
        c.fill_rect(Rect::new(r(x), r(base - h), r(w) + 1, r(h + s * 0.2)), col);
        let mut wy = base - h + s * 0.02;
        while wy < base - s * 0.02 {
            let mut wx = x + s * 0.015;
            while wx < x + w - s * 0.02 {
                if rng.chance(0.35) {
                    c.fill_rect(
                        Rect::new(r(wx), r(wy), r(s * 0.012).max(1), r(s * 0.016).max(1)),
                        hsv(0.11, 0.55, 1.0).with_alpha(200),
                    );
                }
                wx += s * 0.03;
            }
            wy += s * 0.035;
        }
        x += w + rng.range_f32(0.0, s * 0.02);
    }
    // Window sill and frame.
    let frame = hsv(hue + 0.72, 0.3, 0.08);
    c.fill_rect(Rect::new(0, r(s * 0.86), r(s), r(s * 0.14) + 1), frame);
    c.fill_rect(Rect::new(r(s * 0.49), 0, r(s * 0.025).max(2), r(s * 0.86)), frame.with_alpha(230));
    c.fill_rect(Rect::new(0, r(s * 0.47), r(s), r(s * 0.02).max(2)), frame.with_alpha(230));
    // A mug and a plant on the sill.
    c.fill_rounded_rect(
        Rect::new(r(s * 0.14), r(s * 0.79), r(s * 0.09), r(s * 0.08)),
        s * 0.015,
        hsv(hue + 0.05, 0.5, 0.75),
    );
    c.fill_rounded_rect(Rect::new(r(s * 0.75), r(s * 0.8), r(s * 0.1), r(s * 0.07)), s * 0.01, hsv(0.07, 0.5, 0.5));
    for i in 0..5 {
        let a = -1.2 + i as f32 * 0.6;
        let (sx, sy) = (s * 0.8, s * 0.8);
        c.draw_line(
            sx,
            sy,
            sx + FloatExt::sin(a) * s * 0.07,
            sy - FloatExt::cos(a).abs() * s * 0.1,
            s * 0.012,
            hsv(0.33, 0.5, 0.45),
        );
    }
    // Rain on the glass.
    for _ in 0..90 {
        let x = rng.range_f32(0.0, s);
        let y = rng.range_f32(0.0, s * 0.85);
        let len = rng.range_f32(s * 0.02, s * 0.06);
        c.draw_line(x, y, x - len * 0.15, y + len, 1.0, Color::rgba(220, 230, 255, rng.range_f32(40.0, 110.0) as u8));
    }
}

fn aurora(c: &mut Canvas, s: f32, hue: f32, rng: &mut Rng) {
    c.fill_vertical_gradient(Rect::new(0, 0, r(s), r(s)), hsv(hue + 0.62, 0.8, 0.06), hsv(hue + 0.55, 0.7, 0.22));
    stars(c, s, s * 0.8, 110, rng);
    // Curtains: vertical strips whose top follows layered sines.
    for layer in 0..3 {
        let col = hsv(hue + layer as f32 * 0.08, 0.75, 1.0);
        let base = s * (0.28 + layer as f32 * 0.1);
        let (f1, f2) = (rng.range_f32(1.5, 3.0), rng.range_f32(4.0, 7.0));
        let (p1, p2) = (rng.range_f32(0.0, TAU), rng.range_f32(0.0, TAU));
        let strip = 3;
        let mut x = 0;
        while x < r(s) {
            let u = x as f32 / s;
            let y = base + FloatExt::sin(u * f1 * TAU + p1) * s * 0.08 + FloatExt::sin(u * f2 * TAU + p2) * s * 0.025;
            let len = s * (0.18 + 0.1 * FloatExt::sin(u * 9.0 + p2).abs());
            let a = (90.0 + 60.0 * FloatExt::sin(u * 13.0 + p1)) as u8;
            c.fill_vertical_gradient(Rect::new(x, r(y), strip, r(len)), col.with_alpha(a), col.with_alpha(0));
            c.fill_vertical_gradient(
                Rect::new(x, r(y - len * 0.25), strip, r(len * 0.25)),
                col.with_alpha(0),
                col.with_alpha(a / 2),
            );
            x += strip;
        }
    }
    // Mountains and a lake reflection.
    for (k, (h0, v)) in [(0.66f32, 0.12f32), (0.74, 0.06)].iter().enumerate() {
        let mut p = Path::new();
        p.move_to(0.0, s);
        let mut x = 0.0;
        p.line_to(0.0, s * h0);
        while x < s {
            x += rng.range_f32(s * 0.05, s * 0.12);
            p.line_to(x.min(s), s * h0 - rng.range_f32(0.0, s * (0.16 - k as f32 * 0.06)));
        }
        p.line_to(s, s);
        p.close();
        c.fill_path(&p, hsv(hue + 0.6, 0.6, *v), FillRule::NonZero);
    }
}

fn pixel(c: &mut Canvas, s: f32, hue: f32, rng: &mut Rng) {
    let bg = hsv(hue, 0.55, 0.42);
    c.fill_rect(Rect::new(0, 0, r(s), r(s)), bg);
    // Checkerboard sky.
    let cell = (s / 16.0).max(2.0);
    for gy in 0..16 {
        for gx in 0..16 {
            if (gx + gy) % 2 == 0 {
                c.fill_rect(
                    Rect::new(r(gx as f32 * cell), r(gy as f32 * cell), r(cell) + 1, r(cell) + 1),
                    Color::rgba(255, 255, 255, 14),
                );
            }
        }
    }
    // A symmetric 8x8 sprite.
    let px = s / 14.0;
    let (ox, oy) = (s * 0.5 - px * 4.0, s * 0.36 - px * 4.0);
    let body = hsv(hue + 0.5, 0.8, 1.0);
    let shade = hsv(hue + 0.55, 0.85, 0.75);
    let mut cells = [[0u8; 8]; 8];
    for row in cells.iter_mut() {
        for x in 0..4 {
            let v = if rng.chance(0.55) { 1 } else { 0 };
            row[x] = v;
            row[7 - x] = v;
        }
    }
    // Eyes.
    cells[3][2] = 2;
    cells[3][5] = 2;
    for (y, row) in cells.iter().enumerate() {
        for (x, &v) in row.iter().enumerate() {
            let rr = Rect::new(r(ox + x as f32 * px), r(oy + y as f32 * px), r(px) + 1, r(px) + 1);
            match v {
                1 => c.fill_rect(rr, if y > 4 { shade } else { body }),
                2 => c.fill_rect(rr, Color::rgb(20, 20, 30)),
                _ => {}
            }
        }
    }
    // Coins and a block floor.
    let coin = hsv(0.13, 0.85, 1.0);
    for i in 0..4 {
        let x = s * (0.14 + i as f32 * 0.24);
        c.fill_rect(Rect::new(r(x), r(s * 0.66), r(px * 0.8), r(px)), coin);
        c.fill_rect(
            Rect::new(r(x + px * 0.25), r(s * 0.66 + px * 0.2), r(px * 0.25).max(1), r(px * 0.6)),
            hsv(0.1, 0.9, 0.7),
        );
    }
    let ground_y = s * 0.8;
    let brick = hsv(hue + 0.08, 0.7, 0.55);
    let mut x = 0.0;
    while x < s {
        c.fill_rect(Rect::new(r(x), r(ground_y), r(px * 1.5), r(s - ground_y) + 1), brick);
        c.fill_rect(Rect::new(r(x), r(ground_y), r(px * 1.5), r(px * 0.25).max(1)), brick.shade(0.3));
        c.fill_rect(Rect::new(r(x + px * 1.5 - 2.0), r(ground_y), 2, r(s - ground_y) + 1), brick.shade(-0.4));
        x += px * 1.5;
    }
}

fn pulse(c: &mut Canvas, s: f32, hue: f32, rng: &mut Rng) {
    c.fill_vertical_gradient(Rect::new(0, 0, r(s), r(s)), hsv(hue + 0.7, 0.8, 0.10), hsv(hue + 0.75, 0.85, 0.22));
    let (cx, cy) = (s * 0.5, s * 0.42);
    glow(c, cx, cy, s * 0.5, hsv(hue, 0.8, 1.0), 0.8);
    for i in 0..9 {
        let rad = s * (0.06 + i as f32 * 0.045);
        let col = hsv(hue + i as f32 * 0.035, 0.75, 1.0).with_alpha((230 - i * 20) as u8);
        let mut p = Path::new();
        p.circle(cx, cy, rad);
        c.stroke_path(&p, &StrokeStyle::new(s * 0.008 + (i % 3) as f32), col);
    }
    // Light beams.
    for _ in 0..6 {
        let a = rng.range_f32(0.0, TAU);
        let len = s * 0.9;
        c.draw_line(
            cx,
            cy,
            cx + FloatExt::cos(a) * len,
            cy + FloatExt::sin(a) * len,
            s * 0.01,
            hsv(hue + 0.4, 0.5, 1.0).with_alpha(50),
        );
    }
    // An equaliser along the bottom.
    let bars = 16;
    let bw = s / bars as f32;
    for i in 0..bars {
        let h = s * rng.range_f32(0.05, 0.25) * (1.0 - (i as f32 / bars as f32 - 0.5).abs());
        let col = hsv(hue + 0.3 + i as f32 * 0.02, 0.7, 1.0);
        c.fill_vertical_gradient(
            Rect::new(r(i as f32 * bw + 1.0), r(s - h), r(bw - 2.0).max(1), r(h) + 1),
            col,
            col.with_alpha(60),
        );
    }
}

fn waves(c: &mut Canvas, s: f32, hue: f32, rng: &mut Rng) {
    c.fill_vertical_gradient(Rect::new(0, 0, r(s), r(s)), hsv(hue + 0.08, 0.35, 0.95), hsv(hue, 0.5, 0.75));
    glow(c, s * 0.3, s * 0.28, s * 0.25, Color::rgb(255, 250, 235), 1.0);
    c.fill_circle(s * 0.3, s * 0.28, s * 0.09, Color::rgb(255, 252, 240));
    let layers = 6;
    for l in 0..layers {
        let t = l as f32 / (layers - 1) as f32;
        let base = s * (0.42 + t * 0.48);
        let col = hsv(hue + 0.55 + t * 0.12, 0.4 + t * 0.3, 0.85 - t * 0.6);
        let (f, ph, amp) = (rng.range_f32(1.0, 2.5), rng.range_f32(0.0, TAU), s * rng.range_f32(0.03, 0.07));
        let mut p = Path::new();
        p.move_to(0.0, s);
        let mut pts = Vec::new();
        let mut x = 0.0;
        while x <= s + 4.0 {
            let u = x / s;
            pts.push((
                x,
                base + FloatExt::sin(u * f * TAU + ph) * amp + FloatExt::sin(u * 11.0 + ph * 2.0) * amp * 0.25,
            ));
            x += 4.0;
        }
        for (x, y) in &pts {
            p.line_to(*x, *y);
        }
        p.line_to(s, s);
        p.close();
        c.fill_path(&p, col, FillRule::NonZero);
    }
}
