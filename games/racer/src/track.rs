//! The procedurally generated circuit: a closed centripetal Catmull-Rom
//! spline through randomly placed control points, resampled at even
//! spacing, with elevation, banking in corners, a racing line for the AI
//! and the meshes of the road, curbs, verges and start/finish line.

use alloc::vec::Vec;

use v3d::{Mesh, MeshBuilder, Vertex, shapes};
use vmath::{FloatExt, Mat4, Quat, Rng, Vec2, Vec3};

/// Distance between samples along the centre line (metres).
pub const SPACING: f32 = 5.0;
/// Half the road width.
pub const HALF_WIDTH: f32 = 7.0;
/// Width of the curbs outside the road edges.
pub const CURB: f32 = 1.6;
/// Width of the grass verge strip that joins the road to the terrain.
pub const VERGE: f32 = 12.0;
/// Distance of corner barriers from the centre line.
pub const BARRIER: f32 = HALF_WIDTH + CURB + 3.6;
/// Road texture repeat length.
const ROAD_REPEAT: f32 = 16.0;
/// Samples per road mesh segment (for culling).
const SEGMENT: usize = 20;

/// One point of the centre line with its local frame.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub pos: Vec3,
    /// Unit forward direction (follows the slope).
    pub dir: Vec3,
    /// Unit vector to the left across the (banked) road.
    pub left: Vec3,
    /// Road surface normal.
    pub up: Vec3,
    /// Distance from the start line.
    pub s: f32,
    /// Signed curvature (1/m, positive = turning left), smoothed.
    pub curv: f32,
    /// 0..1: how strongly the corner is marked with curbs.
    pub curb: f32,
    /// A barrier wall on this side of the road (-1 right, 1 left, 0 none).
    pub barrier: i8,
}

/// Where a point is relative to the track.
#[derive(Clone, Copy, Debug, Default)]
pub struct Locate {
    /// Index of the sample at the start of the nearest segment.
    pub index: usize,
    /// Distance along the track (0..length).
    pub s: f32,
    /// Signed distance to the centre line (positive = left).
    pub lateral: f32,
    /// Height of the road surface (extended sideways) at this point.
    pub height: f32,
}

/// Meshes of one stretch of road.
pub struct Segment {
    pub road: Mesh,
    pub curbs: Option<Mesh>,
    pub verge: Mesh,
}

/// The circuit.
pub struct Track {
    pub samples: Vec<Sample>,
    pub length: f32,
    /// Racing line: preferred lateral offset per sample.
    pub line: Vec<f32>,
    /// Flat 2D outline for the mini-map.
    pub outline: Vec<Vec2>,
    pub name: &'static str,
}

pub const TRACK_NAMES: [&str; 3] = ["Valley Ring", "Highland Loop", "Lakeside Sprint"];

fn wrap_angle(a: f32) -> f32 {
    let mut a = a;
    while a > vmath::PI {
        a -= vmath::TAU;
    }
    while a < -vmath::PI {
        a += vmath::TAU;
    }
    a
}

/// Centripetal Catmull-Rom between p1 and p2 (t in 0..1).
fn catmull_rom(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, t: f32) -> Vec3 {
    let knot = |a: Vec3, b: Vec3| a.distance(b).max(1e-3).sqrt();
    let t0 = 0.0;
    let t1 = t0 + knot(p0, p1);
    let t2 = t1 + knot(p1, p2);
    let t3 = t2 + knot(p2, p3);
    let t = t1 + (t2 - t1) * t;
    let a1 = p0 * ((t1 - t) / (t1 - t0)) + p1 * ((t - t0) / (t1 - t0));
    let a2 = p1 * ((t2 - t) / (t2 - t1)) + p2 * ((t - t1) / (t2 - t1));
    let a3 = p2 * ((t3 - t) / (t3 - t2)) + p3 * ((t - t2) / (t3 - t2));
    let b1 = a1 * ((t2 - t) / (t2 - t0)) + a2 * ((t - t0) / (t2 - t0));
    let b2 = a2 * ((t3 - t) / (t3 - t1)) + a3 * ((t - t1) / (t3 - t1));
    b1 * ((t2 - t) / (t2 - t1)) + b2 * ((t - t1) / (t2 - t1))
}

impl Track {
    /// Generates circuit number `variant` (a valid layout is guaranteed by
    /// retrying with derived seeds) with enough real corners to be fun.
    pub fn generate(variant: usize) -> Track {
        let base_seed = [7u64, 1234, 98765][variant % 3];
        let mut fallback = None;
        for attempt in 0..60 {
            if let Some(t) = Track::try_generate(base_seed + attempt * 7919, variant) {
                if t.corners() >= 5 {
                    return t;
                }
                if fallback.is_none() {
                    fallback = Some(t);
                }
            }
        }
        fallback.unwrap_or_else(Track::oval)
    }

    /// Number of distinct corners (curvature peaks tighter than ~90 m).
    pub fn corners(&self) -> usize {
        let mut count = 0;
        let mut inside = false;
        for s in &self.samples {
            let tight = s.curv.abs() > 0.011;
            if tight && !inside {
                count += 1;
            }
            inside = tight || (inside && s.curv.abs() > 0.007);
        }
        count
    }

    fn try_generate(seed: u64, variant: usize) -> Option<Track> {
        let mut rng = Rng::new(seed);
        let n = 12 + rng.below(5) as usize;
        let base_r = [330.0, 280.0, 340.0][variant % 3];
        let (sx, sz) = [(1.25, 0.85), (1.05, 1.0), (1.4, 0.75)][variant % 3];
        let hills = [14.0, 26.0, 8.0][variant % 3];
        let mut radii: Vec<f32> = (0..n)
            .map(|_| {
                let r = base_r * rng.range_f32(0.5, 1.25);
                // Occasional deep dents make hairpins and chicanes.
                if rng.chance(0.22) { r * 0.55 } else { r }
            })
            .collect();
        let r = radii.clone();
        for i in 0..n {
            radii[i] = 0.15 * r[(i + n - 1) % n] + 0.7 * r[i] + 0.15 * r[(i + 1) % n];
        }
        let (ph1, ph2) = (rng.range_f32(0.0, vmath::TAU), rng.range_f32(0.0, vmath::TAU));
        let ctrl: Vec<Vec3> = (0..n)
            .map(|i| {
                let a = (i as f32 + rng.range_f32(-0.25, 0.25)) / n as f32 * vmath::TAU;
                let h = hills * (0.6 * (a * 2.0 + ph1).sin() + 0.4 * (a * 3.0 + ph2).sin());
                Vec3::new(a.cos() * radii[i] * sx, h, a.sin() * radii[i] * sz)
            })
            .collect();
        // Dense polyline along the spline.
        let mut dense = Vec::with_capacity(n * 64);
        for i in 0..n {
            let (p0, p1, p2, p3) = (ctrl[(i + n - 1) % n], ctrl[i], ctrl[(i + 1) % n], ctrl[(i + 2) % n]);
            for k in 0..64 {
                dense.push(catmull_rom(p0, p1, p2, p3, k as f32 / 64.0));
            }
        }
        let mut cum = Vec::with_capacity(dense.len() + 1);
        let mut total = 0.0;
        for i in 0..dense.len() {
            cum.push(total);
            total += dense[i].distance(dense[(i + 1) % dense.len()]);
        }
        let count = (total / SPACING).round().max(16.0) as usize;
        let spacing = total / count as f32;
        // Resample evenly.
        let mut pos = Vec::with_capacity(count);
        let mut j = 0;
        for k in 0..count {
            let target = k as f32 * spacing;
            while j + 1 < dense.len() && cum[j + 1] < target {
                j += 1;
            }
            let (a, b) = (dense[j], dense[(j + 1) % dense.len()]);
            let seg = (if j + 1 < dense.len() { cum[j + 1] } else { total }) - cum[j];
            let t = ((target - cum[j]) / seg.max(1e-4)).clamp(0.0, 1.0);
            pos.push(a.lerp(b, t));
        }
        let samples = Track::frames(&pos, spacing);
        let t = Track::finish(samples, count as f32 * spacing, variant);
        if t.valid() { Some(t) } else { None }
    }

    /// A plain oval (fallback that is always valid).
    fn oval() -> Track {
        let count = 300;
        let pos: Vec<Vec3> = (0..count)
            .map(|i| {
                let a = i as f32 / count as f32 * vmath::TAU;
                Vec3::new(a.cos() * 320.0, 0.0, a.sin() * 200.0)
            })
            .collect();
        let mut len = 0.0;
        for i in 0..count {
            len += pos[i].distance(pos[(i + 1) % count]);
        }
        Track::finish(Track::frames(&pos, len / count as f32), len, 0)
    }

    /// Directions, curvature, banking and frames for evenly spaced points.
    fn frames(pos: &[Vec3], spacing: f32) -> Vec<Sample> {
        let n = pos.len();
        let at = |i: isize| pos[i.rem_euclid(n as isize) as usize];
        let dirs: Vec<Vec3> = (0..n as isize).map(|i| (at(i + 1) - at(i - 1)).normalize()).collect();
        let heading = |d: Vec3| d.x.atan2(d.z);
        let raw: Vec<f32> = (0..n)
            .map(|i| {
                let a = heading(dirs[(i + n - 1) % n]);
                let b = heading(dirs[(i + 1) % n]);
                wrap_angle(b - a) / (2.0 * spacing)
            })
            .collect();
        let curv = smooth(&raw, 4);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let dir = dirs[i];
            let flat_left = Vec3::Y.cross(dir).normalize_or(Vec3::X);
            let up0 = dir.cross(flat_left).normalize_or(Vec3::Y);
            let bank = (curv[i] * 3.5).clamp(-0.13, 0.13);
            let left = (flat_left * bank.cos() - up0 * bank.sin()).normalize();
            let up = dir.cross(left).normalize_or(Vec3::Y);
            let curb = ((curv[i].abs() - 0.006) / 0.006).clamp(0.0, 1.0);
            // Barrier on the outside of tighter corners.
            let barrier = if curv[i].abs() > 0.012 { -curv[i].signum() as i8 } else { 0 };
            out.push(Sample { pos: pos[i], dir, left, up, s: i as f32 * spacing, curv: curv[i], curb, barrier });
        }
        // Curbs and barriers: extend the marks a little before and after
        // corners.
        let marks: Vec<f32> = out.iter().map(|s| s.curb).collect();
        let walls: Vec<i8> = out.iter().map(|s| s.barrier).collect();
        for (i, s) in out.iter_mut().enumerate() {
            let mut m = 0.0f32;
            let mut w = 0i8;
            for k in -3isize..=3 {
                let j = (i as isize + k).rem_euclid(n as isize) as usize;
                m = m.max(marks[j]);
                if w == 0 {
                    w = walls[j];
                }
            }
            s.curb = m;
            s.barrier = w;
        }
        out
    }

    fn finish(samples: Vec<Sample>, length: f32, variant: usize) -> Track {
        let n = samples.len();
        // Racing line: towards the inside of corners, eased in and out.
        let raw: Vec<f32> = samples.iter().map(|s| (s.curv * 1100.0).clamp(-1.0, 1.0) * (HALF_WIDTH - 2.4)).collect();
        let line = smooth(&smooth(&raw, 6), 6);
        let outline = samples.iter().map(|s| Vec2::new(s.pos.x, s.pos.z)).collect();
        let _ = n;
        Track { samples, length, line, outline, name: TRACK_NAMES[variant % 3] }
    }

    /// No sharp corners and no part of the track close to another.
    fn valid(&self) -> bool {
        let n = self.samples.len();
        if self.samples.iter().any(|s| s.curv.abs() > 1.0 / 28.0) {
            return false;
        }
        let min_d = 2.0 * (HALF_WIDTH + CURB + VERGE) + 8.0;
        let skip = (min_d * 2.5 / SPACING) as usize + 4;
        for i in (0..n).step_by(2) {
            for j in (0..n).step_by(2) {
                let d = (i as isize - j as isize).unsigned_abs();
                let d = d.min(n - d);
                if d <= skip {
                    continue;
                }
                let (a, b) = (self.samples[i].pos, self.samples[j].pos);
                if Vec2::new(a.x - b.x, a.z - b.z).length() < min_d {
                    return false;
                }
            }
        }
        true
    }

    /// The sample at a (wrapping) index.
    pub fn at(&self, i: isize) -> &Sample {
        &self.samples[i.rem_euclid(self.samples.len() as isize) as usize]
    }

    /// The (interpolated) frame at distance `s`: position, dir, left, up.
    pub fn frame(&self, s: f32) -> (Vec3, Vec3, Vec3, Vec3) {
        let n = self.samples.len();
        let f = s.rem_euclid(self.length) / self.length * n as f32;
        let i = (f as usize).min(n - 1);
        let t = f - i as f32;
        let (a, b) = (&self.samples[i], self.at(i as isize + 1));
        (
            a.pos.lerp(b.pos, t),
            a.dir.lerp(b.dir, t).normalize(),
            a.left.lerp(b.left, t).normalize(),
            a.up.lerp(b.up, t).normalize(),
        )
    }

    /// A point on the (extended) road surface.
    pub fn point(&self, s: f32, lateral: f32) -> Vec3 {
        let (p, _, l, _) = self.frame(s);
        p + l * lateral
    }

    /// Racing line offset at distance `s`.
    pub fn line_at(&self, s: f32) -> f32 {
        let n = self.samples.len();
        let f = s.rem_euclid(self.length) / self.length * n as f32;
        let i = (f as usize).min(n - 1);
        let t = f - i as f32;
        self.line[i] * (1.0 - t) + self.line[(i + 1) % n] * t
    }

    /// Finds the nearest centre-line segment, searching around `hint`
    /// (pass `None` to search everywhere).
    pub fn locate(&self, p: Vec3, hint: Option<usize>) -> Locate {
        let n = self.samples.len();
        let p2 = Vec2::new(p.x, p.z);
        let mut best = (f32::MAX, 0usize, 0.0f32);
        let mut test = |i: usize| {
            let a = &self.samples[i];
            let b = &self.samples[(i + 1) % n];
            let (a2, b2) = (Vec2::new(a.pos.x, a.pos.z), Vec2::new(b.pos.x, b.pos.z));
            let ab = b2 - a2;
            let t = ((p2 - a2).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
            let d = (a2 + ab * t).distance_squared(p2);
            if d < best.0 {
                best = (d, i, t);
            }
        };
        match hint {
            Some(h) => {
                for k in -12isize..=12 {
                    test((h as isize + k).rem_euclid(n as isize) as usize);
                }
            }
            None => {
                for i in 0..n {
                    test(i);
                }
            }
        }
        let (_, i, t) = best;
        let a = &self.samples[i];
        let b = &self.samples[(i + 1) % n];
        let c = a.pos.lerp(b.pos, t);
        let left = a.left.lerp(b.left, t);
        let flat_left = Vec2::new(left.x, left.z).normalize_or(Vec2::X);
        let lateral = (p2 - Vec2::new(c.x, c.z)).dot(flat_left);
        // Height across the banked surface.
        let height = c.y + left.y / Vec2::new(left.x, left.z).length().max(1e-3) * lateral;
        let s = (a.s + t * (self.length / n as f32)).rem_euclid(self.length);
        Locate { index: i, s, lateral, height }
    }

    /// Builds the road in segments of `SEGMENT` samples (for culling): the
    /// asphalt, the curbs and the grass verges are separate meshes because
    /// they use different materials.
    pub fn build_segments(&self, ground_height: &dyn Fn(f32, f32) -> f32) -> Vec<Segment> {
        let n = self.samples.len();
        let mut out = Vec::new();
        let mut start = 0;
        while start < n {
            let end = (start + SEGMENT).min(n);
            let mut road = MeshBuilder::new();
            self.road_strip(&mut road, start, end);
            let mut curbs = MeshBuilder::new();
            self.curbs(&mut curbs, start, end);
            let mut verge = MeshBuilder::new();
            self.verges(&mut verge, start, end, ground_height);
            out.push(Segment {
                road: road.build(),
                curbs: if curbs.indices.is_empty() { None } else { Some(curbs.build()) },
                verge: verge.build(),
            });
            start = end;
        }
        out
    }

    /// Position, frame and distance of sample `i` for mesh building, where
    /// `i == n` is the start sample again at distance `length`.
    fn mesh_sample(&self, i: usize) -> (Sample, f32) {
        let n = self.samples.len();
        let s = self.samples[i % n];
        (s, if i == n { self.length } else { s.s })
    }

    fn road_strip(&self, b: &mut MeshBuilder, start: usize, end: usize) {
        let base = b.vertices.len() as u32;
        for i in start..=end {
            let (smp, s) = self.mesh_sample(i);
            let v = s / ROAD_REPEAT;
            let r = smp.pos - smp.left * HALF_WIDTH;
            let l = smp.pos + smp.left * HALF_WIDTH;
            b.vertex(Vertex::new(r, smp.up, Vec2::new(0.0, v), 0xFFFFFFFF));
            b.vertex(Vertex::new(l, smp.up, Vec2::new(1.0, v), 0xFFFFFFFF));
        }
        for k in 0..(end - start) as u32 {
            let a = base + k * 2;
            // Counter-clockwise seen from above: right, next right, next left, left.
            b.quad(a, a + 2, a + 3, a + 1);
        }
    }

    fn curbs(&self, b: &mut MeshBuilder, start: usize, end: usize) {
        for i in start..end {
            let (a, _) = self.mesh_sample(i);
            let (c, _) = self.mesh_sample(i + 1);
            let k = a.curb.max(c.curb);
            if k <= 0.0 {
                continue;
            }
            for half in 0..2 {
                let (t0, t1) = (half as f32 * 0.5, half as f32 * 0.5 + 0.5);
                let red = (i * 2 + half) % 2 == 0;
                let color = if red { 0xFFD8322A } else { 0xFFF2F2F2 };
                let p0 = a.pos.lerp(c.pos, t0);
                let p1 = a.pos.lerp(c.pos, t1);
                let l0 = a.left.lerp(c.left, t0).normalize();
                let l1 = a.left.lerp(c.left, t1).normalize();
                let up = a.up.lerp(c.up, t0).normalize();
                for side in [-1.0f32, 1.0] {
                    let inner0 = p0 + l0 * (side * HALF_WIDTH) + up * 0.02;
                    let inner1 = p1 + l1 * (side * HALF_WIDTH) + up * 0.02;
                    let outer0 = p0 + l0 * (side * (HALF_WIDTH + CURB)) + up * 0.09;
                    let outer1 = p1 + l1 * (side * (HALF_WIDTH + CURB)) + up * 0.09;
                    let quad =
                        if side > 0.0 { [inner0, inner1, outer1, outer0] } else { [outer0, outer1, inner1, inner0] };
                    let base = b.vertices.len() as u32;
                    for p in quad {
                        b.vertex(Vertex::new(p, up, Vec2::ZERO, color));
                    }
                    b.quad(base, base + 1, base + 2, base + 3);
                }
            }
        }
        self.barriers(b, start, end);
    }

    /// Red and white barrier walls on the outside of corners, and white
    /// marker posts every 30 m elsewhere.
    fn barriers(&self, b: &mut MeshBuilder, start: usize, end: usize) {
        for i in start..end {
            if i % 6 != 0 {
                continue;
            }
            let (a, _) = self.mesh_sample(i);
            for side in [-1.0f32, 1.0] {
                if a.barrier as f32 == side {
                    continue;
                }
                let fl = Vec3::new(a.left.x, 0.0, a.left.z).normalize_or(Vec3::X);
                let edge = a.pos + a.left * (side * (HALF_WIDTH + CURB));
                let p = edge + fl * (side * 2.6);
                let yaw = a.dir.x.atan2(a.dir.z);
                let m = Mat4::from_rotation_translation(Quat::from_rotation_y(yaw), p + Vec3::new(0.0, 0.25, 0.0));
                b.append_transformed(&shapes::cuboid(Vec3::new(0.16, 1.0, 0.16), 0xFFF0F0F0), &m);
                let band = Mat4::from_rotation_translation(Quat::from_rotation_y(yaw), p + Vec3::new(0.0, 0.62, 0.0));
                b.append_transformed(&shapes::cuboid(Vec3::new(0.18, 0.16, 0.18), 0xFFE8501E), &band);
            }
        }
        for i in start..end {
            let (a, _) = self.mesh_sample(i);
            let (c, _) = self.mesh_sample(i + 1);
            if a.barrier == 0 || a.barrier != c.barrier {
                continue;
            }
            let side = a.barrier as f32;
            for half in 0..2 {
                let (t0, t1) = (half as f32 * 0.5, half as f32 * 0.5 + 0.5);
                let color = if (i * 2 + half) % 2 == 0 { 0xFFD8322A } else { 0xFFF0F0F0 };
                let foot = |t: f32| {
                    let p = a.pos.lerp(c.pos, t);
                    let l = a.left.lerp(c.left, t).normalize();
                    let fl = Vec3::new(l.x, 0.0, l.z).normalize_or(Vec3::X);
                    // On the road plane at the curb, then flat out to the wall.
                    let edge = p + l * (side * (HALF_WIDTH + CURB));
                    edge + fl * (side * (BARRIER - HALF_WIDTH - CURB)) - Vec3::new(0.0, 0.25, 0.0)
                };
                let (f0, f1) = (foot(t0), foot(t1));
                let h = Vec3::new(0.0, 1.15, 0.0);
                let fl = Vec3::new(a.left.x, 0.0, a.left.z).normalize_or(Vec3::X) * (side * 0.5);
                // Face towards the road, top, and the back.
                let (inner0, inner1) = (f0, f1);
                let (outer0, outer1) = (f0 + fl, f1 + fl);
                let faces = if side < 0.0 {
                    [
                        [inner1, inner0, inner0 + h, inner1 + h],
                        [inner1 + h, inner0 + h, outer0 + h, outer1 + h],
                        [outer0, outer1, outer1 + h, outer0 + h],
                    ]
                } else {
                    [
                        [inner0, inner1, inner1 + h, inner0 + h],
                        [inner0 + h, inner1 + h, outer1 + h, outer0 + h],
                        [outer1, outer0, outer0 + h, outer1 + h],
                    ]
                };
                for f in faces {
                    b.face(f, color);
                }
            }
        }
    }

    fn verges(&self, b: &mut MeshBuilder, start: usize, end: usize, ground: &dyn Fn(f32, f32) -> f32) {
        for side in [-1.0f32, 1.0] {
            let base = b.vertices.len() as u32;
            for i in start..=end {
                let (smp, s) = self.mesh_sample(i);
                let inner_w = HALF_WIDTH + if smp.curb > 0.0 { CURB } else { 0.0 };
                let inner = smp.pos + smp.left * (side * inner_w) + smp.up * if smp.curb > 0.0 { 0.09 } else { 0.0 };
                let flat_left = Vec3::new(smp.left.x, 0.0, smp.left.z).normalize_or(Vec3::X);
                let o = smp.pos + flat_left * (side * (HALF_WIDTH + CURB + VERGE));
                let outer = Vec3::new(o.x, ground(o.x, o.z), o.z);
                let n = (smp.up + Vec3::Y).normalize();
                let v = s / 8.0;
                b.vertex(Vertex::new(inner, n, Vec2::new(inner.x / 8.0, inner.z / 8.0 + 0.0 * v), 0xFF9AB060));
                b.vertex(Vertex::new(outer, n, Vec2::new(outer.x / 8.0, outer.z / 8.0), 0xFF8AA858));
            }
            for k in 0..(end - start) as u32 {
                let a = base + k * 2;
                if side > 0.0 {
                    b.quad(a, a + 2, a + 3, a + 1);
                } else {
                    b.quad(a, a + 1, a + 3, a + 2);
                }
            }
        }
    }

    /// The checkered start/finish line (a strip just above the road).
    pub fn start_line_mesh(&self) -> Mesh {
        let mut b = MeshBuilder::new();
        let (p, dir, left, up) = self.frame(0.0);
        let half = 1.2;
        let lift = up * 0.03;
        let c = [
            p - left * HALF_WIDTH - dir * half + lift,
            p - left * HALF_WIDTH + dir * half + lift,
            p + left * HALF_WIDTH + dir * half + lift,
            p + left * HALF_WIDTH - dir * half + lift,
        ];
        let uv = [Vec2::new(0.0, 0.0), Vec2::new(0.0, 1.0), Vec2::new(7.0, 1.0), Vec2::new(7.0, 0.0)];
        for i in 0..4 {
            b.vertex(Vertex::new(c[i], up, uv[i], 0xFFFFFFFF));
        }
        b.quad(0, 1, 2, 3);
        b.build()
    }
}

/// Circular moving average with radius `r`.
pub fn smooth(v: &[f32], r: isize) -> Vec<f32> {
    let n = v.len() as isize;
    (0..n)
        .map(|i| {
            let mut acc = 0.0;
            for k in -r..=r {
                acc += v[(i + k).rem_euclid(n) as usize];
            }
            acc / (2 * r + 1) as f32
        })
        .collect()
}
