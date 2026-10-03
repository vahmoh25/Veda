//! Stroking: converts the outline of a stroked path into a path that can be filled.
//!
//! Each contour is flattened into a polyline. Open polylines become one closed outline (left side
//! forwards, end cap, right side backwards, start cap); closed polylines become two contours (the
//! left side and the reversed right side). Outer corners get the requested join (miter with
//! limit, round or bevel); inner corners are connected through the vertex itself, which leaves
//! small self-overlapping loops that are harmless (and exact) under [`FillRule::NonZero`], the rule
//! the result must be filled with.
//!
//! [`FillRule::NonZero`]: crate::FillRule::NonZero

use alloc::vec::Vec;

use crate::flatten;
use crate::geom::Point;
use crate::math;
use crate::path::{KAPPA, Path, PathEl};

/// The shape at the ends of open contours.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineCap {
    /// The stroke ends exactly at the end point.
    #[default]
    Butt,
    /// A half circle around the end point.
    Round,
    /// A half square extending the stroke by half its width.
    Square,
}

/// The shape at the corners of contours.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineJoin {
    /// Sharp corners, beveled when the miter would exceed `miter_limit`.
    #[default]
    Miter,
    /// Circular arcs around the corner.
    Round,
    /// Corners cut off by a straight line.
    Bevel,
}

/// Stroke parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrokeStyle {
    /// Stroke width in path units.
    pub width: f32,
    /// End cap of open contours.
    pub cap: LineCap,
    /// Corner join.
    pub join: LineJoin,
    /// Maximum ratio of miter length to stroke width (SVG semantics, default 4).
    pub miter_limit: f32,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        StrokeStyle { width: 1.0, cap: LineCap::Butt, join: LineJoin::Miter, miter_limit: 4.0 }
    }
}

impl StrokeStyle {
    /// A style with the given width and default caps/joins.
    pub fn new(width: f32) -> Self {
        StrokeStyle { width, ..StrokeStyle::default() }
    }

    /// Returns the style with a different cap.
    pub fn with_cap(mut self, cap: LineCap) -> Self {
        self.cap = cap;
        self
    }

    /// Returns the style with a different join.
    pub fn with_join(mut self, join: LineJoin) -> Self {
        self.join = join;
        self
    }

    /// Returns the style with a different miter limit.
    pub fn with_miter_limit(mut self, limit: f32) -> Self {
        self.miter_limit = limit;
        self
    }
}

/// Converts `path` stroked with `style` into a fillable outline (fill it with the non-zero rule).
/// `tolerance` is the curve flattening tolerance in path units.
pub fn stroke(path: &Path, style: &StrokeStyle, tolerance: f32) -> Path {
    let mut out = Path::new();
    Stroker::new().stroke_into(path, style, tolerance, &mut out);
    out
}

/// A reusable stroker (keeps its buffers between calls).
#[derive(Default)]
pub struct Stroker {
    pts: Vec<Point>,
    dirs: Vec<Point>,
    out: Path,
}

/// Per-call stroking parameters.
struct Ctx<'o> {
    hw: f32,
    join: LineJoin,
    cap: LineCap,
    /// Minimum `1 + cos(turn angle)` for which a miter is allowed.
    miter_min: f32,
    tol: f32,
    out: &'o mut Path,
    first: bool,
}

impl Ctx<'_> {
    #[inline]
    fn emit(&mut self, p: Point) {
        if self.first {
            self.out.move_to(p.x, p.y);
            self.first = false;
        } else {
            self.out.line_to(p.x, p.y);
        }
    }

    /// Circular arc around `c` from radius vector `a` to `b` (both of length `hw`), taking the
    /// short way, or through `via` when `a` and `b` are (nearly) opposite.
    fn arc(&mut self, c: Point, a: Point, b: Point, via: Point) {
        let r2 = self.hw * self.hw;
        let cos = a.dot(b) / r2;
        if cos < -0.9999 {
            self.arc_short(c, a, via);
            self.arc_short(c, via, b);
        } else if cos < 0.0 {
            let m = match (a + b).normalize() {
                Some(m) => m * self.hw,
                None => via,
            };
            self.arc_short(c, a, m);
            self.arc_short(c, m, b);
        } else {
            self.arc_short(c, a, b);
        }
    }

    /// An arc of at most 90 degrees as one cubic (or a line if it is flatter than the tolerance).
    fn arc_short(&mut self, c: Point, a: Point, b: Point) {
        let r = self.hw;
        let r2 = r * r;
        let cos = (a.dot(b) / r2).clamp(-1.0, 1.0);
        let sin = a.cross(b) / r2;
        // Sagitta of the arc: r * (1 - cos(angle / 2)).
        if r * (1.0 - math::sqrt((1.0 + cos) * 0.5)) <= self.tol {
            let e = c + b;
            self.out.line_to(e.x, e.y);
            return;
        }
        let t = sin.abs() / (1.0 + cos); // tan(angle / 2)
        let k = 4.0 / 3.0 * t / (1.0 + math::sqrt(1.0 + t * t)); // 4/3 tan(angle / 4)
        let s = if sin >= 0.0 { 1.0 } else { -1.0 };
        let ta = a.perp() * s;
        let tb = b.perp() * s;
        let p1 = c + a + ta * k;
        let p2 = c + b - tb * k;
        let e = c + b;
        self.out.cubic_to(p1.x, p1.y, p2.x, p2.y, e.x, e.y);
    }

    /// Emits the corner at `p` between the incoming direction `u0` and the outgoing direction
    /// `u1` on the left (+perp) side, ending at `p + perp(u1) * hw`.
    fn join(&mut self, p: Point, u0: Point, u1: Point) {
        let n0 = u0.perp() * self.hw;
        let n1 = u1.perp() * self.hw;
        let cross = u0.cross(u1);
        let dot = u0.dot(u1);
        self.emit(p + n0);
        if cross.abs() < 1e-6 && dot > 0.0 {
            // Straight continuation.
            return;
        }
        if cross > 0.0 {
            // Inner side of the turn: pivot through the vertex.
            self.out.line_to(p.x, p.y);
            let e = p + n1;
            self.out.line_to(e.x, e.y);
            return;
        }
        let e = p + n1;
        match self.join {
            LineJoin::Bevel => self.out.line_to(e.x, e.y),
            LineJoin::Miter => {
                let d = 1.0 + dot;
                if d >= self.miter_min && d > 1e-6 {
                    let m = p + (n0 + n1) * (1.0 / d);
                    self.out.line_to(m.x, m.y);
                }
                self.out.line_to(e.x, e.y);
            }
            LineJoin::Round => self.arc(p, n0, n1, u0 * self.hw),
        }
    }

    /// Emits the cap at end point `p` with outward direction `u`, from `p + perp(-u)...`: the
    /// current point is `p + n` and the cap ends at `p - n`.
    fn cap(&mut self, p: Point, u: Point, n: Point) {
        match self.cap {
            LineCap::Butt => {
                let e = p - n;
                self.out.line_to(e.x, e.y);
            }
            LineCap::Square => {
                let ext = u * self.hw;
                let a = p + n + ext;
                let b = p - n + ext;
                let e = p - n;
                self.out.line_to(a.x, a.y);
                self.out.line_to(b.x, b.y);
                self.out.line_to(e.x, e.y);
            }
            LineCap::Round => {
                let mid = u * self.hw;
                let k = KAPPA;
                // Two quarter circles: n -> mid -> -n.
                let (a1, a2, a3) = (p + n + mid * k, p + mid + n * k, p + mid);
                self.out.cubic_to(a1.x, a1.y, a2.x, a2.y, a3.x, a3.y);
                let (b1, b2, b3) = (p + mid - n * k, p - n + mid * k, p - n);
                self.out.cubic_to(b1.x, b1.y, b2.x, b2.y, b3.x, b3.y);
            }
        }
    }
}

/// Squared distance below which consecutive points are merged.
const MERGE_EPS2: f32 = 1e-10;

impl Stroker {
    /// Creates a stroker.
    pub fn new() -> Self {
        Stroker::default()
    }

    /// Strokes `path` and returns the outline (valid until the next call). Fill it with the
    /// non-zero rule. `tolerance` is the curve flattening tolerance in path units.
    pub fn stroke(&mut self, path: &Path, style: &StrokeStyle, tolerance: f32) -> &Path {
        let mut out = core::mem::take(&mut self.out);
        out.clear();
        self.stroke_into(path, style, tolerance, &mut out);
        self.out = out;
        &self.out
    }

    /// Strokes `path` and appends the outline contours to `out`.
    pub fn stroke_into(&mut self, path: &Path, style: &StrokeStyle, tolerance: f32, out: &mut Path) {
        let hw = style.width * 0.5;
        if !(hw > 0.0 && hw.is_finite()) {
            return;
        }
        let limit = if style.miter_limit >= 1.0 { style.miter_limit } else { 1.0 };
        let tol = if tolerance > 0.0 && tolerance.is_finite() { tolerance } else { flatten::DEFAULT_TOLERANCE };
        let mut ctx = Ctx {
            hw,
            join: style.join,
            cap: style.cap,
            miter_min: 2.0 / (limit * limit),
            tol,
            out,
            first: true,
        };
        self.pts.clear();
        let mut had_segment = false;
        let mut last = Point::ZERO;
        for el in path.iter() {
            match el {
                PathEl::MoveTo(p) => {
                    self.finish(&mut ctx, false, had_segment);
                    self.pts.clear();
                    push_point(&mut self.pts, p);
                    had_segment = false;
                    last = p;
                }
                PathEl::LineTo(p) => {
                    push_point(&mut self.pts, p);
                    had_segment = true;
                    last = p;
                }
                PathEl::QuadTo(c, p) => {
                    let pts = &mut self.pts;
                    flatten::flatten_quad(last, c, p, tol, &mut |q| push_point(pts, q));
                    had_segment = true;
                    last = p;
                }
                PathEl::CubicTo(c1, c2, p) => {
                    let pts = &mut self.pts;
                    flatten::flatten_cubic(last, c1, c2, p, tol, &mut |q| push_point(pts, q));
                    had_segment = true;
                    last = p;
                }
                PathEl::Close => {
                    self.finish(&mut ctx, true, had_segment);
                    self.pts.clear();
                    had_segment = false;
                }
            }
        }
        self.finish(&mut ctx, false, had_segment);
        self.pts.clear();
    }

    /// Strokes the polyline collected in `self.pts`.
    fn finish(&mut self, ctx: &mut Ctx<'_>, closed: bool, had_segment: bool) {
        let mut n = self.pts.len();
        if n == 0 {
            return;
        }
        if closed && n >= 2 && (self.pts[n - 1] - self.pts[0]).length_squared() <= MERGE_EPS2 {
            self.pts.pop();
            n -= 1;
        }
        if n == 1 {
            if had_segment {
                let p = self.pts[0];
                match ctx.cap {
                    LineCap::Butt => {}
                    LineCap::Round => ctx.out.circle(p.x, p.y, ctx.hw),
                    LineCap::Square => ctx.out.rect(p.x - ctx.hw, p.y - ctx.hw, 2.0 * ctx.hw, 2.0 * ctx.hw),
                }
            }
            return;
        }
        // Unit directions of the segments (closed contours include the closing segment).
        let segs = if closed { n } else { n - 1 };
        self.dirs.clear();
        let mut prev_dir = None;
        for i in 0..segs {
            let d = (self.pts[(i + 1) % n] - self.pts[i]).normalize();
            let d = match (d, prev_dir) {
                (Some(d), _) => d,
                (None, Some(p)) => p,
                (None, None) => Point::new(1.0, 0.0),
            };
            prev_dir = Some(d);
            self.dirs.push(d);
        }
        if closed {
            for rev in [false, true] {
                ctx.first = true;
                for i in 0..n {
                    let (p, u0, u1) = if !rev {
                        (self.pts[i], self.dirs[(i + n - 1) % n], self.dirs[i])
                    } else {
                        // Walking backwards: vertex n-1-i, directions negated.
                        let v = n - 1 - i;
                        (self.pts[v], -self.dirs[v], -self.dirs[(v + n - 1) % n])
                    };
                    ctx.join(p, u0, u1);
                }
                ctx.out.close();
            }
        } else {
            ctx.first = true;
            // Left side, forwards.
            let d0 = self.dirs[0];
            ctx.emit(self.pts[0] + d0.perp() * ctx.hw);
            for i in 1..n - 1 {
                ctx.join(self.pts[i], self.dirs[i - 1], self.dirs[i]);
            }
            let dl = self.dirs[segs - 1];
            let nl = dl.perp() * ctx.hw;
            let pe = self.pts[n - 1];
            ctx.emit(pe + nl);
            ctx.cap(pe, dl, nl);
            // Right side, backwards.
            for i in (1..n - 1).rev() {
                ctx.join(self.pts[i], -self.dirs[i], -self.dirs[i - 1]);
            }
            let n0 = d0.perp() * ctx.hw;
            let ps = self.pts[0];
            ctx.emit(ps - n0);
            ctx.cap(ps, -d0, -n0);
            ctx.out.close();
        }
    }
}

/// Appends `p` unless it duplicates the previous point or is not finite.
#[inline]
fn push_point(pts: &mut Vec<Point>, p: Point) {
    if !p.is_finite() {
        return;
    }
    if let Some(&last) = pts.last() {
        if (p - last).length_squared() <= MERGE_EPS2 {
            return;
        }
    }
    pts.push(p);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::{FillRule, Mask, Rasterizer};
    use crate::transform::Transform;

    fn area(path: &Path, w: u32, h: u32) -> f64 {
        let mut r = Rasterizer::new();
        r.set_tolerance(0.005);
        let m: Mask = r.render_mask(path, &Transform::IDENTITY, FillRule::NonZero, w, h);
        m.data.iter().map(|&v| v as f64 / 255.0).sum()
    }

    fn check(got: f64, expected: f64, what: &str) {
        assert!(((got - expected) / expected).abs() < 0.01, "{what}: {got} vs {expected}");
    }

    #[test]
    fn line_caps() {
        let mut p = Path::new();
        p.line(20.0, 30.0, 80.0, 30.0);
        let pi = core::f64::consts::PI;
        let s = StrokeStyle::new(8.0);
        check(area(&stroke(&p, &s, 0.05), 100, 60), 60.0 * 8.0, "butt");
        let sq = s.with_cap(LineCap::Square);
        check(area(&stroke(&p, &sq, 0.05), 100, 60), 68.0 * 8.0, "square");
        let rd = s.with_cap(LineCap::Round);
        check(area(&stroke(&p, &rd, 0.05), 100, 60), 60.0 * 8.0 + pi * 16.0, "round");
        // Diagonal line keeps its area.
        let mut d = Path::new();
        d.line(10.0, 10.0, 70.0, 50.0);
        let len = (60.0f64 * 60.0 + 40.0 * 40.0).sqrt();
        check(area(&stroke(&d, &s, 0.05), 100, 60), len * 8.0, "diagonal");
    }

    #[test]
    fn closed_square_joins() {
        let mut p = Path::new();
        p.rect(20.0, 20.0, 40.0, 40.0);
        let w = 6.0f64;
        let miter = StrokeStyle::new(w as f32);
        // Outer square 46x46 minus inner square 34x34.
        check(area(&stroke(&p, &miter, 0.05), 80, 80), 46.0 * 46.0 - 34.0 * 34.0, "miter");
        let bevel = miter.with_join(LineJoin::Bevel);
        let corner = 4.0 * (w / 2.0) * (w / 2.0) / 2.0;
        check(area(&stroke(&p, &bevel, 0.05), 80, 80), 46.0 * 46.0 - 34.0 * 34.0 - corner, "bevel");
        let round = miter.with_join(LineJoin::Round);
        let rounded = 4.0 * (w / 2.0) * (w / 2.0) * (1.0 - core::f64::consts::PI / 4.0);
        check(area(&stroke(&p, &round, 0.05), 80, 80), 46.0 * 46.0 - 34.0 * 34.0 - rounded, "round");
        // A wide stroke that swallows the hole: everything is covered.
        let mut q = Path::new();
        q.rect(40.0, 40.0, 40.0, 40.0);
        let fat = StrokeStyle::new(50.0);
        check(area(&stroke(&q, &fat, 0.05), 120, 120), 90.0 * 90.0, "fat");
    }

    #[test]
    fn miter_limit_and_polyline() {
        // A sharp V: with a high limit the miter tip extends far, with limit 1 it is beveled.
        let mut p = Path::new();
        p.polyline(&[Point::new(10.0, 80.0), Point::new(40.0, 10.0), Point::new(70.0, 80.0)]);
        let s = StrokeStyle::new(6.0).with_miter_limit(10.0);
        let long = stroke(&p, &s, 0.05);
        let short = stroke(&p, &s.with_miter_limit(1.0), 0.05);
        let top_long = long.bounds().unwrap().y0;
        let top_short = short.bounds().unwrap().y0;
        assert!(top_long < top_short - 3.0, "{top_long} {top_short}");
        assert!(area(&long, 100, 100) > area(&short, 100, 100));
    }

    #[test]
    fn curves_and_dots() {
        let mut c = Path::new();
        c.circle(50.0, 50.0, 30.0);
        let s = StrokeStyle::new(4.0);
        let pi = core::f64::consts::PI;
        check(area(&stroke(&c, &s, 0.02), 100, 100), pi * (32.0f64 * 32.0 - 28.0 * 28.0), "ring");
        let mut dot = Path::new();
        dot.move_to(10.0, 10.0);
        dot.line_to(10.0, 10.0);
        check(area(&stroke(&dot, &s.with_cap(LineCap::Round), 0.02), 20, 20), pi * 4.0, "round dot");
        check(area(&stroke(&dot, &s.with_cap(LineCap::Square), 0.02), 20, 20), 16.0, "square dot");
        assert!(stroke(&dot, &s, 0.02).is_empty());
        // Degenerate styles produce nothing.
        assert!(stroke(&c, &StrokeStyle::new(0.0), 0.1).is_empty());
        assert!(stroke(&c, &StrokeStyle::new(f32::NAN), 0.1).is_empty());
    }

    #[test]
    fn hairpin_turn() {
        // Going forward and straight back must not leave holes.
        let mut p = Path::new();
        p.polyline(&[Point::new(10.0, 20.0), Point::new(60.0, 20.0), Point::new(30.0, 20.0)]);
        for join in [LineJoin::Miter, LineJoin::Round, LineJoin::Bevel] {
            let s = StrokeStyle::new(6.0).with_join(join);
            let a = area(&stroke(&p, &s, 0.05), 80, 40);
            let base = 50.0 * 6.0;
            assert!(a >= base * 0.99, "{join:?}: {a}");
        }
    }
}
