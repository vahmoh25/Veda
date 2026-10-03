//! Vector paths: contours made of lines and quadratic/cubic Bézier curves, plus builders for common
//! shapes (rectangles, rounded rectangles, ellipses, circles, arcs and pies).
//!
//! Paths follow canvas semantics: `line_to`/`quad_to`/`cubic_to` on an empty path first move to the
//! (first) given point, and drawing after `close` starts a new contour at the closed contour's start
//! point. Filling closes every contour implicitly; stroking distinguishes open and closed contours.

use alloc::vec::Vec;

use crate::flatten;
use crate::geom::{Point, Rect};
use crate::math;
use crate::transform::Transform;

/// Handle length of a cubic Bézier approximating a quarter circle of radius 1: 4/3 * (sqrt(2) - 1).
pub(crate) const KAPPA: f32 = 0.552_284_8;

/// A path command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Starts a new contour (1 point).
    MoveTo,
    /// A straight line (1 point).
    LineTo,
    /// A quadratic Bézier curve (control point, end point).
    QuadTo,
    /// A cubic Bézier curve (two control points, end point).
    CubicTo,
    /// Closes the current contour (no points).
    Close,
}

impl Verb {
    /// The number of points this verb consumes.
    #[inline]
    pub const fn num_points(self) -> usize {
        match self {
            Verb::MoveTo | Verb::LineTo => 1,
            Verb::QuadTo => 2,
            Verb::CubicTo => 3,
            Verb::Close => 0,
        }
    }
}

/// A path element with its points, as produced by [`Path::iter`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    /// Start a new contour at the point.
    MoveTo(Point),
    /// Line to the point.
    LineTo(Point),
    /// Quadratic curve: control point, end point.
    QuadTo(Point, Point),
    /// Cubic curve: first control point, second control point, end point.
    CubicTo(Point, Point, Point),
    /// Close the current contour.
    Close,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// No current point.
    #[default]
    Empty,
    /// A contour is open; the current point is the last point.
    Open,
    /// The last contour was closed; the current point is its start.
    Closed,
}

/// A vector path made of contours of lines and Bézier curves.
#[derive(Clone, Debug, Default)]
pub struct Path {
    verbs: Vec<Verb>,
    points: Vec<Point>,
    start: Point,
    state: State,
}

impl Path {
    /// Creates an empty path.
    pub fn new() -> Self {
        Path::default()
    }

    /// Creates an empty path with preallocated storage.
    pub fn with_capacity(verbs: usize, points: usize) -> Self {
        Path { verbs: Vec::with_capacity(verbs), points: Vec::with_capacity(points), ..Path::default() }
    }

    /// Removes all contours (keeps the allocated storage).
    pub fn clear(&mut self) {
        self.verbs.clear();
        self.points.clear();
        self.start = Point::ZERO;
        self.state = State::Empty;
    }

    /// Returns `true` if the path has no commands.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// The path commands.
    #[inline]
    pub fn verbs(&self) -> &[Verb] {
        &self.verbs
    }

    /// The points of all commands, in order (see [`Verb::num_points`]).
    #[inline]
    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// Mutable access to the points (the structure of the path cannot be changed this way).
    #[inline]
    pub fn points_mut(&mut self) -> &mut [Point] {
        &mut self.points
    }

    /// The current point, if any.
    pub fn current_point(&self) -> Option<Point> {
        match self.state {
            State::Empty => None,
            State::Open => self.points.last().copied(),
            State::Closed => Some(self.start),
        }
    }

    /// Starts a new contour at `(x, y)`. Consecutive `move_to`s collapse into one.
    pub fn move_to(&mut self, x: f32, y: f32) {
        let p = Point::new(x, y);
        if self.verbs.last() == Some(&Verb::MoveTo) {
            if let Some(last) = self.points.last_mut() {
                *last = p;
            }
        } else {
            self.verbs.push(Verb::MoveTo);
            self.points.push(p);
        }
        self.start = p;
        self.state = State::Open;
    }

    /// Re-opens a contour at the start of the last closed one, if needed.
    #[inline]
    fn reopen(&mut self) {
        if self.state == State::Closed {
            let s = self.start;
            self.verbs.push(Verb::MoveTo);
            self.points.push(s);
            self.state = State::Open;
        }
    }

    /// Adds a line to `(x, y)`.
    pub fn line_to(&mut self, x: f32, y: f32) {
        if self.state == State::Empty {
            self.move_to(x, y);
            return;
        }
        self.reopen();
        self.verbs.push(Verb::LineTo);
        self.points.push(Point::new(x, y));
    }

    /// Adds a quadratic Bézier curve with control point `(x1, y1)` ending at `(x, y)`.
    pub fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        if self.state == State::Empty {
            self.move_to(x1, y1);
        }
        self.reopen();
        self.verbs.push(Verb::QuadTo);
        self.points.push(Point::new(x1, y1));
        self.points.push(Point::new(x, y));
    }

    /// Adds a cubic Bézier curve with control points `(x1, y1)`, `(x2, y2)` ending at `(x, y)`.
    pub fn cubic_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        if self.state == State::Empty {
            self.move_to(x1, y1);
        }
        self.reopen();
        self.verbs.push(Verb::CubicTo);
        self.points.push(Point::new(x1, y1));
        self.points.push(Point::new(x2, y2));
        self.points.push(Point::new(x, y));
    }

    /// Closes the current contour (a straight line back to its start is implied).
    pub fn close(&mut self) {
        if self.state == State::Open {
            self.verbs.push(Verb::Close);
            self.state = State::Closed;
        }
    }

    /// Adds a closed rectangle contour (clockwise on screen).
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.close();
    }

    /// Adds a closed rounded rectangle contour with per-corner radii
    /// `[top_left, top_right, bottom_right, bottom_left]`.
    ///
    /// Radii are clamped like CSS `border-radius`: if adjacent radii do not fit along a side, all
    /// radii are scaled down by the same factor. Negative or non-finite radii count as 0.
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radii: [f32; 4]) {
        let (x, w) = if w < 0.0 { (x + w, -w) } else { (x, w) };
        let (y, h) = if h < 0.0 { (y + h, -h) } else { (y, h) };
        let mut r = radii.map(|r| if r > 0.0 && r.is_finite() { r } else { 0.0 });
        let mut f = 1.0f32;
        for (sum, side) in [(r[0] + r[1], w), (r[3] + r[2], w), (r[0] + r[3], h), (r[1] + r[2], h)] {
            if sum > side && sum > 0.0 {
                f = f.min(side / sum);
            }
        }
        if f < 1.0 {
            for v in &mut r {
                *v *= f;
            }
        }
        let [tl, tr, br, bl] = r;
        let k = 1.0 - KAPPA;
        self.move_to(x + tl, y);
        self.line_to(x + w - tr, y);
        if tr > 0.0 {
            self.cubic_to(x + w - tr * k, y, x + w, y + tr * k, x + w, y + tr);
        }
        self.line_to(x + w, y + h - br);
        if br > 0.0 {
            self.cubic_to(x + w, y + h - br * k, x + w - br * k, y + h, x + w - br, y + h);
        }
        self.line_to(x + bl, y + h);
        if bl > 0.0 {
            self.cubic_to(x + bl * k, y + h, x, y + h - bl * k, x, y + h - bl);
        }
        self.line_to(x, y + tl);
        if tl > 0.0 {
            self.cubic_to(x, y + tl * k, x + tl * k, y, x + tl, y);
        }
        self.close();
    }

    /// Adds a closed axis-aligned ellipse contour (four cubic arcs, clockwise on screen).
    pub fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32) {
        let (kx, ky) = (rx * KAPPA, ry * KAPPA);
        self.move_to(cx + rx, cy);
        self.cubic_to(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry);
        self.cubic_to(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy);
        self.cubic_to(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry);
        self.cubic_to(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy);
        self.close();
    }

    /// Adds a closed circle contour.
    pub fn circle(&mut self, cx: f32, cy: f32, r: f32) {
        self.ellipse(cx, cy, r, r);
    }

    /// Adds a circular arc around `(cx, cy)` starting at `start_angle` and sweeping `sweep_angle`
    /// radians (positive = from +x towards +y, clockwise on screen; clamped to one full turn).
    ///
    /// Like the canvas `arc()`, the arc is connected to the current contour with a straight line,
    /// or starts a new contour if there is no current point. The contour is left open.
    pub fn arc(&mut self, cx: f32, cy: f32, r: f32, start_angle: f32, sweep_angle: f32) {
        self.ellipse_arc(cx, cy, r, r, start_angle, sweep_angle);
    }

    /// Adds an arc of the axis-aligned ellipse with radii `(rx, ry)`; see [`Path::arc`].
    pub fn ellipse_arc(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, start_angle: f32, sweep_angle: f32) {
        if !(start_angle.is_finite() && sweep_angle.is_finite()) {
            return;
        }
        const TAU: f32 = core::f32::consts::TAU;
        let sweep = sweep_angle.clamp(-TAU, TAU);
        let (mut s, mut c) = math::sin_cos(start_angle);
        let (x0, y0) = (cx + rx * c, cy + ry * s);
        if self.state == State::Empty {
            self.move_to(x0, y0);
        } else {
            self.line_to(x0, y0);
        }
        let quarter = core::f32::consts::FRAC_PI_2;
        let n = (math::ceil(sweep.abs() / quarter - 1e-4) as usize).clamp(1, 4);
        let step = sweep / n as f32;
        let k = 4.0 / 3.0 * math::tan(step * 0.25);
        for i in 1..=n {
            let (s1, c1) = math::sin_cos(start_angle + step * i as f32);
            self.cubic_to(
                cx + rx * (c - k * s),
                cy + ry * (s + k * c),
                cx + rx * (c1 + k * s1),
                cy + ry * (s1 - k * c1),
                cx + rx * c1,
                cy + ry * s1,
            );
            s = s1;
            c = c1;
        }
    }

    /// Adds a closed pie slice (circular sector) contour: center, arc, back to the center.
    pub fn pie(&mut self, cx: f32, cy: f32, r: f32, start_angle: f32, sweep_angle: f32) {
        self.move_to(cx, cy);
        self.arc(cx, cy, r, start_angle, sweep_angle);
        self.close();
    }

    /// Adds a closed polygon contour through `points` (nothing if `points` is empty).
    pub fn polygon(&mut self, points: &[Point]) {
        self.polyline(points);
        if !points.is_empty() {
            self.close();
        }
    }

    /// Adds an open polyline contour through `points` (nothing if `points` is empty).
    pub fn polyline(&mut self, points: &[Point]) {
        if let Some((first, rest)) = points.split_first() {
            self.move_to(first.x, first.y);
            for p in rest {
                self.line_to(p.x, p.y);
            }
        }
    }

    /// Adds an open contour consisting of a single line (useful for stroking).
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.move_to(x0, y0);
        self.line_to(x1, y1);
    }

    /// Appends all contours of `other`.
    pub fn append(&mut self, other: &Path) {
        if other.is_empty() {
            return;
        }
        // `other` always starts with a MoveTo, so the contour structure is preserved.
        self.verbs.extend_from_slice(&other.verbs);
        self.points.extend_from_slice(&other.points);
        self.start = other.start;
        self.state = other.state;
    }

    /// Appends all contours of `other`, transformed by `t`.
    pub fn append_transformed(&mut self, other: &Path, t: &Transform) {
        if other.is_empty() {
            return;
        }
        self.verbs.extend_from_slice(&other.verbs);
        self.points.extend(other.points.iter().map(|&p| t.apply(p)));
        self.start = t.apply(other.start);
        self.state = other.state;
    }

    /// Transforms every point of the path in place.
    pub fn transform(&mut self, t: &Transform) {
        for p in &mut self.points {
            *p = t.apply(*p);
        }
        self.start = t.apply(self.start);
    }

    /// The bounding box of all points, including curve control points (a conservative bound of the
    /// curves). `None` for an empty path.
    pub fn bounds(&self) -> Option<Rect> {
        let (first, rest) = self.points.split_first()?;
        let mut r = Rect::from_point(*first);
        for p in rest {
            r.include(*p);
        }
        Some(r)
    }

    /// Iterates over the path elements.
    #[inline]
    pub fn iter(&self) -> PathIter<'_> {
        PathIter { verbs: self.verbs.iter(), points: &self.points, index: 0 }
    }

    /// Calls `f` with a flattened version of the path: only `MoveTo`, `LineTo` and `Close` elements,
    /// with curves approximated by lines within `tolerance`.
    pub fn flatten(&self, tolerance: f32, mut f: impl FnMut(PathEl)) {
        let mut last = Point::ZERO;
        for el in self.iter() {
            match el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => {
                    f(el);
                    last = p;
                }
                PathEl::QuadTo(c, p) => {
                    flatten::flatten_quad(last, c, p, tolerance, &mut |q| f(PathEl::LineTo(q)));
                    last = p;
                }
                PathEl::CubicTo(c1, c2, p) => {
                    flatten::flatten_cubic(last, c1, c2, p, tolerance, &mut |q| f(PathEl::LineTo(q)));
                    last = p;
                }
                PathEl::Close => f(el),
            }
        }
    }

    /// Emboldens the outline by moving every point outwards along the bisector of its neighbouring
    /// edges (the approach of FreeType's `FT_Outline_EmboldenXY`). Vertical stems grow by
    /// `strength_x` in total (half on each side), horizontal stems by `strength_y`; the outline is not
    /// re-centred. Works on outlines whose outer contours share one orientation (fonts, shapes built
    /// by this type); counters (holes) shrink accordingly.
    pub fn embolden(&mut self, strength_x: f32, strength_y: f32) {
        let mut scratch = Vec::new();
        self.embolden_with(strength_x, strength_y, &mut scratch);
    }

    /// Like [`Path::embolden`], using `scratch` as temporary storage to avoid allocations.
    pub fn embolden_with(&mut self, strength_x: f32, strength_y: f32, scratch: &mut Vec<Point>) {
        if !(strength_x.is_finite() && strength_y.is_finite()) || (strength_x == 0.0 && strength_y == 0.0) {
            return;
        }
        // Orientation of the outer contours from the signed area of all control polygons.
        let mut area = 0.0f64;
        self.for_each_contour(|pts| {
            let n = pts.len();
            for i in 0..n {
                let (p, q) = (pts[i], pts[(i + 1) % n]);
                area += p.x as f64 * q.y as f64 - q.x as f64 * p.y as f64;
            }
        });
        if area == 0.0 || !area.is_finite() {
            return;
        }
        let sign = if area > 0.0 { 1.0 } else { -1.0 };
        let (hx, hy) = (strength_x * 0.5, strength_y * 0.5);
        scratch.clear();
        scratch.resize(self.points.len(), Point::ZERO);
        let mut ranges = ContourRanges { verbs: &self.verbs, vi: 0, pi: 0 };
        while let Some((s, e)) = ranges.next_range() {
            let Some(pts) = self.points.get(s..e) else {
                break;
            };
            let n = pts.len();
            if n < 3 {
                continue;
            }
            for i in 0..n {
                let p = pts[i];
                let mut j = (i + n - 1) % n;
                let mut steps = 0;
                while pts[j] == p && steps < n {
                    j = (j + n - 1) % n;
                    steps += 1;
                }
                let mut k = (i + 1) % n;
                steps = 0;
                while pts[k] == p && steps < n {
                    k = (k + 1) % n;
                    steps += 1;
                }
                let (Some(ui), Some(uo)) = ((p - pts[j]).normalize(), (pts[k] - p).normalize()) else {
                    continue;
                };
                let ni = Point::new(ui.y, -ui.x) * sign;
                let no = Point::new(uo.y, -uo.x) * sign;
                let mut shift = ni + no;
                let d = 1.0 + ui.dot(uo);
                if d > 1e-3 {
                    shift = shift * (1.0 / d);
                }
                // Limit the miter at sharp corners to twice the strength.
                let len2 = shift.length_squared();
                if len2 > 4.0 {
                    shift = shift * (2.0 / math::sqrt(len2));
                }
                scratch[s + i] = Point::new(shift.x * hx, shift.y * hy);
            }
        }
        for (p, d) in self.points.iter_mut().zip(scratch.iter()) {
            *p += *d;
        }
    }

    /// Calls `f` with the points of each contour (the MoveTo point and all following points).
    fn for_each_contour(&self, mut f: impl FnMut(&[Point])) {
        let mut ranges = ContourRanges { verbs: &self.verbs, vi: 0, pi: 0 };
        while let Some((s, e)) = ranges.next_range() {
            if let Some(pts) = self.points.get(s..e) {
                f(pts);
            }
        }
    }
}

/// Splits a path into the point ranges of its contours.
struct ContourRanges<'a> {
    verbs: &'a [Verb],
    vi: usize,
    pi: usize,
}

impl ContourRanges<'_> {
    fn next_range(&mut self) -> Option<(usize, usize)> {
        // Skip to the next MoveTo.
        while self.vi < self.verbs.len() && self.verbs[self.vi] != Verb::MoveTo {
            self.pi += self.verbs[self.vi].num_points();
            self.vi += 1;
        }
        if self.vi >= self.verbs.len() {
            return None;
        }
        let start = self.pi;
        self.pi += 1;
        self.vi += 1;
        while self.vi < self.verbs.len() && self.verbs[self.vi] != Verb::MoveTo {
            self.pi += self.verbs[self.vi].num_points();
            self.vi += 1;
        }
        Some((start, self.pi))
    }
}

/// Iterator over the elements of a [`Path`].
#[derive(Clone)]
pub struct PathIter<'a> {
    verbs: core::slice::Iter<'a, Verb>,
    points: &'a [Point],
    index: usize,
}

impl Iterator for PathIter<'_> {
    type Item = PathEl;

    fn next(&mut self) -> Option<PathEl> {
        let verb = *self.verbs.next()?;
        let i = self.index;
        self.index += verb.num_points();
        let pts = self.points.get(i..self.index)?;
        Some(match verb {
            Verb::MoveTo => PathEl::MoveTo(pts[0]),
            Verb::LineTo => PathEl::LineTo(pts[0]),
            Verb::QuadTo => PathEl::QuadTo(pts[0], pts[1]),
            Verb::CubicTo => PathEl::CubicTo(pts[0], pts[1], pts[2]),
            Verb::Close => PathEl::Close,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn builder_semantics() {
        let mut p = Path::new();
        p.line_to(1.0, 2.0); // acts as move_to
        p.line_to(3.0, 4.0);
        p.close();
        p.line_to(5.0, 6.0); // re-opens at (1, 2)
        assert_eq!(p.verbs(), &[Verb::MoveTo, Verb::LineTo, Verb::Close, Verb::MoveTo, Verb::LineTo]);
        assert_eq!(p.points()[2], Point::new(1.0, 2.0));
        p.move_to(0.0, 0.0);
        p.move_to(7.0, 7.0); // collapses
        assert_eq!(p.verbs().iter().filter(|v| **v == Verb::MoveTo).count(), 3);
        assert_eq!(p.current_point(), Some(Point::new(7.0, 7.0)));
        let els: Vec<PathEl> = p.iter().collect();
        assert_eq!(els.len(), 6);
    }

    #[test]
    fn shapes_and_bounds() {
        let mut p = Path::new();
        p.rounded_rect(10.0, 20.0, 100.0, 50.0, [10.0, 0.0, 40.0, 30.0]);
        let b = p.bounds().unwrap();
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (10.0, 20.0, 110.0, 70.0));
        let mut c = Path::new();
        c.circle(0.0, 0.0, 5.0);
        let b = c.bounds().unwrap();
        assert!((b.x0 + 5.0).abs() < 1e-5 && (b.y1 - 5.0).abs() < 1e-5);
        let mut a = Path::new();
        a.arc(0.0, 0.0, 10.0, 0.0, core::f32::consts::PI);
        let last = a.points().last().unwrap();
        assert!((last.x + 10.0).abs() < 1e-4 && last.y.abs() < 1e-4);
        assert_eq!(a.verbs().len(), 3); // move + 2 quarter arcs
    }

    #[test]
    fn embolden_grows_square() {
        let mut p = Path::new();
        p.polygon(&[Point::new(0.0, 0.0), Point::new(10.0, 0.0), Point::new(10.0, 10.0), Point::new(0.0, 10.0)]);
        p.embolden(2.0, 2.0);
        let b = p.bounds().unwrap();
        assert!((b.x0 + 1.0).abs() < 1e-5 && (b.x1 - 11.0).abs() < 1e-5);
        assert!((b.y0 + 1.0).abs() < 1e-5 && (b.y1 - 11.0).abs() < 1e-5);
        // Reversed orientation must also grow.
        let mut q = Path::new();
        let mut pts = vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0), Point::new(10.0, 10.0), Point::new(0.0, 10.0)];
        pts.reverse();
        q.polygon(&pts);
        q.embolden(2.0, 0.0);
        let b = q.bounds().unwrap();
        assert!((b.x0 + 1.0).abs() < 1e-5 && (b.x1 - 11.0).abs() < 1e-5 && b.y0.abs() < 1e-5);
    }
}
