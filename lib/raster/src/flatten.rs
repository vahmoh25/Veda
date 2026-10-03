//! Curve flattening: approximating quadratic and cubic Bézier curves by line segments.
//!
//! The segment count comes from Wang's formula: for a degree-`d` curve whose control points have
//! second differences of at most `M`, `n = ceil(sqrt(d(d-1)/8 * M / tolerance))` uniform parameter
//! steps keep every point of the polyline within `tolerance` of the curve. This is robust (no
//! recursion on degenerate input, a hard cap on the count) and cheap; cubics whose required count is
//! large are first split in half so that unevenly curved cubics (e.g. near cusps) get more segments
//! only where needed. Points are evaluated with forward differencing.

use crate::geom::Point;
use crate::math;

/// Default flattening tolerance in pixels.
pub const DEFAULT_TOLERANCE: f32 = 0.2;

/// Upper bound for the number of segments produced for a single curve.
pub const MAX_SEGMENTS: u32 = 1024;

/// `ceil(sqrt(m / tolerance))`, clamped to `1..=MAX_SEGMENTS` (1 for NaN or degenerate input).
#[inline]
fn segments_for(m: f32, tolerance: f32) -> u32 {
    let tol = if tolerance > 1e-4 { tolerance } else { 1e-4 };
    let v = m / tol;
    if v.is_nan() || v <= 1.0 {
        return 1;
    }
    let n = math::ceil(math::sqrt(v));
    if n >= MAX_SEGMENTS as f32 { MAX_SEGMENTS } else { (n as u32).max(1) }
}

/// The number of line segments needed to approximate a quadratic curve within `tolerance`.
pub fn quad_segments(p0: Point, p1: Point, p2: Point, tolerance: f32) -> u32 {
    let dd = p0 - p1 * 2.0 + p2;
    segments_for(0.25 * dd.length(), tolerance)
}

/// The number of line segments needed to approximate a cubic curve within `tolerance`.
pub fn cubic_segments(p0: Point, p1: Point, p2: Point, p3: Point, tolerance: f32) -> u32 {
    let d1 = (p0 - p1 * 2.0 + p2).length_squared();
    let d2 = (p1 - p2 * 2.0 + p3).length_squared();
    segments_for(0.75 * math::sqrt(d1.max(d2)), tolerance)
}

/// Flattens the quadratic curve `p0, p1, p2`, calling `f` with every polyline vertex after `p0`
/// (the last call is exactly `p2`).
pub fn flatten_quad(p0: Point, p1: Point, p2: Point, tolerance: f32, f: &mut impl FnMut(Point)) {
    let n = quad_segments(p0, p1, p2, tolerance);
    if n <= 1 {
        f(p2);
        return;
    }
    // P(t) = p0 + b t + a t^2
    let h = 1.0 / n as f32;
    let a = p0 - p1 * 2.0 + p2;
    let b = (p1 - p0) * 2.0;
    let mut d1 = a * (h * h) + b * h;
    let d2 = a * (2.0 * h * h);
    let mut p = p0;
    for _ in 1..n {
        p += d1;
        d1 += d2;
        f(p);
    }
    f(p2);
}

/// Flattens the cubic curve `p0, p1, p2, p3`, calling `f` with every polyline vertex after `p0`
/// (the last call is exactly `p3`).
pub fn flatten_cubic(p0: Point, p1: Point, p2: Point, p3: Point, tolerance: f32, f: &mut impl FnMut(Point)) {
    flatten_cubic_rec(p0, p1, p2, p3, tolerance, f, 0);
}

fn flatten_cubic_rec(
    p0: Point,
    p1: Point,
    p2: Point,
    p3: Point,
    tolerance: f32,
    f: &mut impl FnMut(Point),
    depth: u32,
) {
    let n = cubic_segments(p0, p1, p2, p3, tolerance);
    if n > 16 && depth < 3 {
        // De Casteljau split at t = 1/2.
        let p01 = p0.midpoint(p1);
        let p12 = p1.midpoint(p2);
        let p23 = p2.midpoint(p3);
        let p012 = p01.midpoint(p12);
        let p123 = p12.midpoint(p23);
        let m = p012.midpoint(p123);
        flatten_cubic_rec(p0, p01, p012, m, tolerance, f, depth + 1);
        flatten_cubic_rec(m, p123, p23, p3, tolerance, f, depth + 1);
        return;
    }
    if n <= 1 {
        f(p3);
        return;
    }
    // P(t) = a t^3 + b t^2 + c t + p0
    let a = p3 - p0 + (p1 - p2) * 3.0;
    let b = (p0 - p1 * 2.0 + p2) * 3.0;
    let c = (p1 - p0) * 3.0;
    let h = 1.0 / n as f32;
    let h2 = h * h;
    let h3 = h2 * h;
    let mut d1 = a * h3 + b * h2 + c * h;
    let mut d2 = a * (6.0 * h3) + b * (2.0 * h2);
    let d3 = a * (6.0 * h3);
    let mut p = p0;
    for _ in 1..n {
        p += d1;
        d1 += d2;
        d2 += d3;
        f(p);
    }
    f(p3);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn dist_to_polyline(p: Point, poly: &[Point]) -> f32 {
        let mut best = f32::MAX;
        for w in poly.windows(2) {
            let (a, b) = (w[0], w[1]);
            let ab = b - a;
            let t =
                if ab.length_squared() > 0.0 { ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
            best = best.min((a + ab * t - p).length());
        }
        best
    }

    #[test]
    fn cubic_within_tolerance() {
        let (p0, p1, p2, p3) =
            (Point::new(0.0, 0.0), Point::new(30.0, 100.0), Point::new(170.0, -60.0), Point::new(200.0, 40.0));
        for &tol in &[0.05f32, 0.2, 1.0] {
            let mut poly = Vec::from([p0]);
            flatten_cubic(p0, p1, p2, p3, tol, &mut |p| poly.push(p));
            assert_eq!(*poly.last().unwrap(), p3);
            for i in 0..=1000 {
                let t = i as f32 / 1000.0;
                let mt = 1.0 - t;
                let q = p0 * (mt * mt * mt) + p1 * (3.0 * mt * mt * t) + p2 * (3.0 * mt * t * t) + p3 * (t * t * t);
                assert!(dist_to_polyline(q, &poly) <= tol * 1.01 + 1e-3, "tol {tol} t {t}");
            }
        }
    }

    #[test]
    fn quad_within_tolerance_and_degenerate() {
        let (p0, p1, p2) = (Point::new(0.0, 0.0), Point::new(50.0, 80.0), Point::new(100.0, 0.0));
        let mut poly = Vec::from([p0]);
        flatten_quad(p0, p1, p2, 0.2, &mut |p| poly.push(p));
        for i in 0..=500 {
            let t = i as f32 / 500.0;
            let mt = 1.0 - t;
            let q = p0 * (mt * mt) + p1 * (2.0 * mt * t) + p2 * (t * t);
            assert!(dist_to_polyline(q, &poly) <= 0.21);
        }
        // A straight "curve" is a single segment; NaN does not explode.
        assert_eq!(quad_segments(p0, Point::new(50.0, 0.0), p2, 0.2), 1);
        assert_eq!(cubic_segments(p0, Point::new(f32::NAN, 0.0), p2, p2, 0.2), 1);
        assert_eq!(cubic_segments(p0, Point::new(1e30, 0.0), p2, p2, 0.2), MAX_SEGMENTS);
    }
}
