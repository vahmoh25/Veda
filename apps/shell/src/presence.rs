//! How the agent looks: OS1's ring, from Spike Jonze's "Her".
//!
//! * **The ring**: white, a quarter of its radius wide (OS1's proportions).
//!   Drawn as the band between two outlines, so that it can tremble
//!   ([`ring`]); a light can run around it ([`ring_with_light`]).
//! * **The coil** ([`coil`]): the film's loading figure, which Geoff
//!   McFetridge called a triple helix — a white ribbon wound three times
//!   around a long loop that spins about its length, its far side fading
//!   into what lies behind it. Turned to face the viewer and brought closer,
//!   it becomes the ring: OS1 starting up.
//!
//! Only shapes and numbers here; what the agent is doing decides which
//! (see `agent::draw_presence`).

use alloc::vec::Vec;
use core::f32::consts::{PI, TAU};

use vgfx::{Canvas, Color, FillRule, Path};
use vmath::FloatExt;

/// The ring's width relative to its radius.
pub const RING_WIDTH: f32 = 0.25;

/// The ring's outline, trembling by `amount` (0 is round) at time `t`:
/// the offset from the radius `r` at angle `a`.
fn tremble(a: f32, r: f32, amount: f32, t: f32) -> f32 {
    if amount <= 0.0 {
        return 0.0;
    }
    let wave = (5.0 * a + 3.1 * t).sin() + 0.6 * (8.0 * a - 4.3 * t).sin() + 0.4 * (3.0 * a + 2.2 * t).sin();
    r * amount * 0.022 * wave
}

/// Points around a circle for a radius (finer when larger).
fn steps(r: f32) -> usize {
    ((r * 1.6) as usize).clamp(32, 160)
}

/// A ring of mid radius `r` and width `w` around (`cx`, `cy`), its outline
/// trembling by `amount` at time `t`.
#[allow(clippy::too_many_arguments)]
pub fn ring(c: &mut Canvas, cx: f32, cy: f32, r: f32, w: f32, amount: f32, t: f32, color: Color) {
    let n = steps(r);
    let mut p = Path::new();
    for half in [w / 2.0, -w / 2.0] {
        for i in 0..n {
            let a = i as f32 / n as f32 * TAU;
            let rr = (r + half + tremble(a, r, amount, t)).max(0.0);
            let (x, y) = (cx + rr * a.cos(), cy + rr * a.sin());
            if i == 0 {
                p.move_to(x, y);
            } else {
                p.line_to(x, y);
            }
        }
        p.close();
    }
    c.fill_path(&p, color, FillRule::EvenOdd);
}

/// A ring with a light running around it (the agent thinking): `base`
/// alpha, brightening to `peak` at the angle `at`. The light is arcs laid
/// over each other, each narrower than the last, so that it has no seams.
#[allow(clippy::too_many_arguments)]
pub fn ring_with_light(c: &mut Canvas, cx: f32, cy: f32, r: f32, w: f32, at: f32, base: u8, peak: u8) {
    const ARCS: usize = 24;
    ring(c, cx, cy, r, w, 0.0, 0.0, Color::rgba(255, 255, 255, base));
    // Each arc lets this much of what is under it show through, so that
    // all of them together reach `peak`.
    let rest = (1.0 - peak as f32 / 255.0).max(0.004) / (1.0 - base as f32 / 255.0).max(0.004);
    let alpha = 1.0 - rest.powf(1.0 / ARCS as f32);
    let color = Color::rgba(255, 255, 255, (255.0 * alpha) as u8);
    let (ri, ro) = (r - w / 2.0, r + w / 2.0);
    for k in 1..=ARCS {
        // Up to a third of a turn either side.
        let half = TAU / 3.0 * k as f32 / ARCS as f32;
        let n = ((half * r / 2.0) as usize).max(4);
        let mut p = Path::new();
        for i in 0..=n {
            let a = at - half + 2.0 * half * i as f32 / n as f32;
            let (x, y) = (cx + ro * a.cos(), cy + ro * a.sin());
            if i == 0 {
                p.move_to(x, y);
            } else {
                p.line_to(x, y);
            }
        }
        for i in (0..=n).rev() {
            let a = at - half + 2.0 * half * i as f32 / n as f32;
            p.line_to(cx + ri * a.cos(), cy + ri * a.sin());
        }
        p.close();
        c.fill_path(&p, color, FillRule::NonZero);
    }
}

/// A soft glow around a ring of radius `r` and width `w`: `alpha` (0..=1)
/// over the ring, fading out `spread` pixels either side of it. Bands
/// laid over each other, each wider than the last.
#[allow(clippy::too_many_arguments)]
pub fn glow(c: &mut Canvas, cx: f32, cy: f32, r: f32, w: f32, spread: f32, alpha: f32) {
    const BANDS: usize = 8;
    if alpha <= 0.0 {
        return;
    }
    let each = 1.0 - (1.0 - alpha.min(0.95)).powf(1.0 / BANDS as f32);
    let color = Color::rgba(255, 255, 255, (255.0 * each) as u8);
    for k in 1..=BANDS {
        ring(c, cx, cy, r, w + 2.0 * spread * k as f32 / BANDS as f32, 0.0, 0.0, color);
    }
}

/// The coil's half length and the radius it winds around its length at
/// (in its own units), and half the ribbon's width.
const LENGTH: f32 = 30.0;
const WIND: f32 = 5.6;
const RIBBON: f32 = 1.1;
/// Points along the coil.
const SAMPLES: usize = 200;
/// The viewer's distance, and how much closer the coil comes as it turns.
const CAMERA: f32 = 150.0;
const APPROACH: f32 = 70.0;
/// The near end, turned towards the viewer, appears this many times its
/// size at the coil's centre; [`coil_scale`] sizes the coil so that it then
/// matches the ring.
const NEAR: f32 = CAMERA / (CAMERA - LENGTH - APPROACH);

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn normalize(a: V3) -> V3 {
    let l = dot(a, a).sqrt();
    if l < 1e-9 { a } else { [a[0] / l, a[1] / l, a[2] / l] }
}

/// `v` rotated by `angle` about the unit `axis` (Rodrigues).
fn rotate(v: V3, axis: V3, angle: f32) -> V3 {
    let (s, c) = angle.sin_cos();
    let k = cross(axis, v);
    let d = dot(axis, v) * (1.0 - c);
    [v[0] * c + k[0] * s + axis[0] * d, v[1] * c + k[1] * s + axis[1] * d, v[2] * c + k[2] * s + axis[2] * d]
}

/// The coil at `t` (0..1): a loop along x, wound three times around it in
/// y and twice in z, so that it closes on itself.
fn coil_point(t: f32) -> V3 {
    let x = LENGTH * (TAU * t).sin();
    let y = WIND * (TAU * 3.0 * t).cos();
    let q = t % 0.25 / 0.25;
    let mut shift = t % 0.25 - (2.0 * (1.0 - q) * q * -0.0185 + q * q * 0.25);
    let quarter = (t / 0.25).floor() as i32;
    if quarter == 0 || quarter == 2 {
        shift = -shift;
    }
    let z = WIND * (TAU * 2.0 * (t - shift)).sin();
    [x, y, z]
}

/// The coil's points and, at each, the direction the ribbon spreads in: a
/// frame carried along the coil without twisting, as a tube drawn along it
/// would have, its small mismatch where the loop closes spread evenly.
fn ribbon() -> (Vec<V3>, Vec<V3>) {
    let n = SAMPLES;
    let points: Vec<V3> = (0..=n).map(|i| coil_point(i as f32 / n as f32)).collect();
    let tangent = |i: usize| normalize(sub(points[(i + 1) % n], points[(i + n - 1) % n]));
    let tangents: Vec<V3> = (0..=n).map(|i| tangent(i % n)).collect();
    // A first normal, across the first tangent's smallest component.
    let t0 = tangents[0];
    let axis = if t0[0].abs() <= t0[1].abs() && t0[0].abs() <= t0[2].abs() {
        [1.0, 0.0, 0.0]
    } else if t0[1].abs() <= t0[2].abs() {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let mut normals = Vec::with_capacity(n + 1);
    normals.push(cross(t0, normalize(cross(t0, axis))));
    for i in 1..=n {
        let prev = normals[i - 1];
        let v = cross(tangents[i - 1], tangents[i]);
        let l = dot(v, v).sqrt();
        normals.push(if l > 1e-6 {
            rotate(prev, [v[0] / l, v[1] / l, v[2] / l], dot(tangents[i - 1], tangents[i]).clamp(-1.0, 1.0).acos())
        } else {
            prev
        });
    }
    let mut theta = dot(normals[0], normals[n]).clamp(-1.0, 1.0).acos() / n as f32;
    if dot(tangents[0], cross(normals[0], normals[n])) > 0.0 {
        theta = -theta;
    }
    for (i, normal) in normals.iter_mut().enumerate().skip(1) {
        *normal = rotate(*normal, tangents[i], theta * i as f32);
    }
    (points, normals)
}

/// Pixels per coil unit for a figure that becomes a ring of radius `r`.
pub fn coil_scale(r: f32) -> f32 {
    r / (WIND * NEAR)
}

/// The coil around (`cx`, `cy`) at `scale` pixels per unit: spun by `spin`
/// radians about its length, turned towards the viewer by `turn` (0 side
/// on, 1 end on and close), at `alpha` (0..=1). `thin` draws only its line
/// (for small sizes).
#[allow(clippy::too_many_arguments)]
pub fn coil(c: &mut Canvas, cx: f32, cy: f32, scale: f32, spin: f32, turn: f32, alpha: f32, thin: bool) {
    let (points, normals) = ribbon();
    let (ss, cs) = spin.sin_cos();
    let (st, ct) = (-PI / 2.0 * turn).sin_cos();
    // Into view: spun, turned, brought closer and seen in perspective.
    // Also returns the depth in the coil's own frame, which decides how
    // much the far side fades.
    let view = |p: V3| -> (f32, f32, f32) {
        let y = p[1] * cs - p[2] * ss;
        let z = p[1] * ss + p[2] * cs;
        let x = p[0] * ct + z * st;
        let depth = -p[0] * st + z * ct + APPROACH * turn;
        let k = CAMERA / (CAMERA - depth).max(1.0);
        (cx + scale * x * k, cy - scale * y * k, z)
    };
    // How much of the white is left behind the film's ten faint veils (one
    // every half unit of depth, from 2.5 behind the middle to 2 in front;
    // here a smooth slope), which matter less as the coil turns to face
    // the viewer.
    let fade = |z: f32| -> f32 {
        let behind = ((2.0 - z) / 0.5).clamp(0.0, 10.0);
        let f = 0.87f32.powf(behind);
        f + (1.0 - f) * turn
    };
    const LEVELS: usize = 20;
    // Everything in view once: the centre line, the ribbon's two edges and
    // the shade of each point.
    let centre: Vec<(f32, f32, f32)> = points.iter().map(|&p| view(p)).collect();
    let edge = |s: f32| -> Vec<(f32, f32)> {
        if thin {
            return Vec::new();
        }
        points
            .iter()
            .zip(&normals)
            .map(|(p, n)| {
                let (x, y, _) = view([p[0] + s * n[0], p[1] + s * n[1], p[2] + s * n[2]]);
                (x, y)
            })
            .collect()
    };
    let (a, b) = (edge(RIBBON), edge(-RIBBON));
    let width = if thin { 1.3 } else { (RIBBON * scale * 0.7).max(1.0) };
    let mut style = vgfx::StrokeStyle::new(width);
    style.join = vgfx::LineJoin::Round;
    // Stretches of one shade, each drawn whole (its edges as one outline,
    // which may cross itself where the ribbon turns over), so that no seams
    // show inside them. The centre line keeps the ribbon whole where it is
    // seen edge on.
    let draw = |c: &mut Canvas, run: &[usize], level: usize| {
        if run.len() < 2 {
            return;
        }
        let color = Color::rgba(255, 255, 255, (255 * level / (LEVELS - 1)) as u8);
        if !thin {
            let mut p = Path::new();
            p.move_to(a[run[0]].0, a[run[0]].1);
            for &i in &run[1..] {
                p.line_to(a[i].0, a[i].1);
            }
            for &i in run.iter().rev() {
                p.line_to(b[i].0, b[i].1);
            }
            p.close();
            c.fill_path(&p, color, FillRule::NonZero);
        }
        let mut line = Path::new();
        line.move_to(centre[run[0]].0, centre[run[0]].1);
        for &i in &run[1..] {
            line.line_to(centre[i].0, centre[i].1);
        }
        c.stroke_path(&line, &style, color);
    };
    let mut run: Vec<usize> = Vec::new();
    let mut run_level = 0;
    for i in 0..SAMPLES {
        let shade = fade((centre[i].2 + centre[i + 1].2) / 2.0) * alpha;
        let level = ((shade * (LEVELS - 1) as f32).round() as usize).min(LEVELS - 1);
        if level != run_level && !run.is_empty() {
            draw(c, &run, run_level);
            run.clear();
        }
        run_level = level;
        if level == 0 {
            continue;
        }
        if run.is_empty() {
            run.push(i);
        }
        run.push(i + 1);
    }
    draw(c, &run, run_level);
}
