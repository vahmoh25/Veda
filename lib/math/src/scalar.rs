//! Scalar helpers for games and graphics, plus the common `f32` constants.
//!
//! Everything here works on `f32` and is re-exported at the crate root
//! (`vmath::lerp`, `vmath::PI`, ...). All functions are `const fn`, never
//! panic, and handle reversed ranges (`lerp(10.0, 0.0, t)`) naturally.

pub use core::f32::consts::{
    E, FRAC_1_PI, FRAC_1_SQRT_2, FRAC_2_PI, FRAC_2_SQRT_PI, FRAC_PI_2, FRAC_PI_3, FRAC_PI_4, FRAC_PI_6, FRAC_PI_8,
    LN_2, LN_10, LOG2_10, LOG2_E, LOG10_2, LOG10_E, PI, SQRT_2, TAU,
};

use crate::f32 as m;

/// Multiply degrees by this to get radians.
pub const DEG_TO_RAD: f32 = PI / 180.0;
/// Multiply radians by this to get degrees.
pub const RAD_TO_DEG: f32 = 180.0 / PI;

/// Linear interpolation: `a` at `t = 0`, `b` at `t = 1`; `t` is not clamped.
#[inline]
pub const fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// The inverse of [`lerp`]: the `t` with `lerp(a, b, t) == v`, or 0 if `a == b`.
#[inline]
pub const fn inverse_lerp(a: f32, b: f32, v: f32) -> f32 {
    let d = b - a;
    if d == 0.0 { 0.0 } else { (v - a) / d }
}

/// Maps `v` linearly from `[in_min, in_max]` to `[out_min, out_max]` (not clamped).
#[inline]
pub const fn remap(v: f32, in_min: f32, in_max: f32, out_min: f32, out_max: f32) -> f32 {
    lerp(out_min, out_max, inverse_lerp(in_min, in_max, v))
}

/// Clamps `x` to `[0, 1]` ("saturate"); NaN becomes 0.
#[inline]
pub const fn clamp01(x: f32) -> f32 {
    if x > 0.0 { if x < 1.0 { x } else { 1.0 } } else { 0.0 }
}

/// 0 for `x < edge`, otherwise 1.
#[inline]
pub const fn step(edge: f32, x: f32) -> f32 {
    if x < edge { 0.0 } else { 1.0 }
}

/// Hermite interpolation `3t² - 2t³` of `x` between `edge0` (0) and `edge1` (1),
/// clamped outside. With `edge0 == edge1` this is [`step`].
#[inline]
pub const fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 {
        return step(edge0, x);
    }
    let t = clamp01((x - edge0) / (edge1 - edge0));
    t * t * (3.0 - 2.0 * t)
}

/// Ken Perlin's smoother variant `6t⁵ - 15t⁴ + 10t³` (zero first and second
/// derivatives at the edges).
#[inline]
pub const fn smootherstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 {
        return step(edge0, x);
    }
    let t = clamp01((x - edge0) / (edge1 - edge0));
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Whether `|a - b| <= eps` (absolute tolerance; false if either is NaN).
#[inline]
pub const fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
    m::abs(a - b) <= eps
}

/// Whether `a` and `b` are equal up to a relative tolerance:
/// `|a - b| <= rel * max(|a|, |b|)` (or exactly equal, e.g. both zero).
#[inline]
pub const fn approx_eq_rel(a: f32, b: f32, rel: f32) -> bool {
    a == b || m::abs(a - b) <= rel * m::max(m::abs(a), m::abs(b))
}

/// The sign of `x`: -1, 0 or 1 (0 for ±0, NaN for NaN). Unlike `f32::signum`,
/// zero maps to zero.
#[inline]
pub const fn sign(x: f32) -> f32 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        x * 0.0 // ±0 stays 0, NaN stays NaN
    }
}

/// Wraps `x` into `[min, max)` (for example a tile coordinate onto a torus).
/// Returns `min` if the range is empty.
#[inline]
pub const fn wrap(x: f32, min: f32, max: f32) -> f32 {
    let range = max - min;
    if range > 0.0 {
        let r = min + m::rem_euclid(x - min, range);
        // Rounding can produce `max` itself for tiny negative offsets.
        if r >= max { min } else { r }
    } else {
        min
    }
}

/// Wraps an angle in radians to `[-π, π)`.
#[inline]
pub const fn wrap_angle(angle: f32) -> f32 {
    wrap(angle, -PI, PI)
}

/// The shortest signed angular difference `to - from` in `[-π, π)` (radians).
#[inline]
pub const fn angle_diff(from: f32, to: f32) -> f32 {
    wrap_angle(to - from)
}

/// Interpolates between two angles along the shorter arc (radians, result not wrapped).
#[inline]
pub const fn lerp_angle(from: f32, to: f32, t: f32) -> f32 {
    from + angle_diff(from, to) * t
}

/// Moves `current` toward `target` by at most `max_delta` (never overshoots).
#[inline]
pub const fn move_towards(current: f32, target: f32, max_delta: f32) -> f32 {
    let d = target - current;
    if m::abs(d) <= max_delta { target } else { current + m::copysign(max_delta, d) }
}

/// Frame-rate independent exponential smoothing of `a` toward `b`:
/// `a + (b - a) * (1 - e^(-rate * dt))`. After `1 / rate` seconds about 63% of
/// the distance is covered, regardless of how `dt` is split into frames.
#[inline]
pub const fn damp(a: f32, b: f32, rate: f32, dt: f32) -> f32 {
    lerp(a, b, -m::exp_m1(-rate * dt))
}

/// Bounces `t` back and forth between 0 and `length` (a triangle wave).
#[inline]
pub const fn ping_pong(t: f32, length: f32) -> f32 {
    if length > 0.0 {
        let t = m::rem_euclid(t, 2.0 * length);
        length - m::abs(t - length)
    } else {
        0.0
    }
}

/// The fractional part of `x` in `[0, 1)`, i.e. `x - floor(x)` (GLSL `fract`;
/// differs from `f32::fract` for negative numbers).
#[inline]
pub const fn fract_floor(x: f32) -> f32 {
    let f = x - m::floor(x);
    // x - floor(x) rounds to 1.0 for tiny negative x.
    if f >= 1.0 { 0.0 } else { f }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation() {
        assert_eq!(lerp(2.0, 4.0, 0.5), 3.0);
        assert_eq!(lerp(2.0, 4.0, 0.0), 2.0);
        assert_eq!(lerp(2.0, 4.0, 1.0), 4.0);
        assert_eq!(lerp(2.0, 4.0, 2.0), 6.0);
        assert_eq!(inverse_lerp(2.0, 4.0, 3.0), 0.5);
        assert_eq!(inverse_lerp(4.0, 2.0, 3.5), 0.25);
        assert_eq!(inverse_lerp(1.0, 1.0, 5.0), 0.0);
        assert_eq!(remap(5.0, 0.0, 10.0, 100.0, 200.0), 150.0);
        assert_eq!(remap(0.0, -1.0, 1.0, 0.0, 255.0), 127.5);
    }

    #[test]
    fn clamping_and_steps() {
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(0.25), 0.25);
        assert_eq!(clamp01(7.0), 1.0);
        assert_eq!(clamp01(f32::NAN), 0.0);
        assert_eq!(clamp01(f32::INFINITY), 1.0);
        assert_eq!(step(1.0, 0.5), 0.0);
        assert_eq!(step(1.0, 1.0), 1.0);
        assert_eq!(smoothstep(0.0, 1.0, -1.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 0.5), 0.5);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
        assert_eq!(smoothstep(0.0, 2.0, 0.5), 0.15625);
        assert_eq!(smoothstep(1.0, 0.0, 0.25), 0.84375); // reversed edges
        assert_eq!(smoothstep(1.0, 1.0, 1.5), 1.0);
        assert_eq!(smootherstep(0.0, 1.0, 0.5), 0.5);
        assert_eq!(smootherstep(0.0, 1.0, 1.0), 1.0);
        assert!((smootherstep(0.0, 1.0, 0.25) - 0.103_515_625).abs() < 1e-7);
    }

    #[test]
    fn comparisons() {
        assert!(approx_eq(1.0, 1.05, 0.1));
        assert!(!approx_eq(1.0, 1.2, 0.1));
        assert!(!approx_eq(f32::NAN, f32::NAN, 1.0));
        assert!(approx_eq_rel(1000.0, 1000.1, 1e-3));
        assert!(!approx_eq_rel(1.0, 1.1, 1e-3));
        assert!(approx_eq_rel(0.0, 0.0, 0.0));
        assert_eq!(sign(-3.0), -1.0);
        assert_eq!(sign(2.0), 1.0);
        assert_eq!(sign(0.0), 0.0);
        assert!(sign(f32::NAN).is_nan());
    }

    #[test]
    fn angles_and_wrapping() {
        assert!((wrap_angle(3.0 * PI) - -PI).abs() < 1e-5);
        assert!((wrap_angle(TAU + 0.5) - 0.5).abs() < 1e-5);
        assert!((wrap_angle(-0.5) - -0.5).abs() < 1e-7);
        assert_eq!(wrap_angle(0.0), 0.0);
        for i in -1000..1000 {
            let a = wrap_angle(i as f32 * 0.37);
            assert!((-PI..PI).contains(&a), "{a}");
        }
        assert!((angle_diff(0.1, TAU - 0.1) - -0.2).abs() < 1e-5);
        assert!((angle_diff(-3.0, 3.0) - (6.0 - TAU)).abs() < 1e-5);
        assert!((lerp_angle(PI - 0.1, -PI + 0.1, 0.5).abs() - PI).abs() < 1e-5);
        assert_eq!(wrap(5.5, 0.0, 2.0), 1.5);
        assert_eq!(wrap(-0.5, 0.0, 2.0), 1.5);
        assert_eq!(wrap(-1e-9, 0.0, 2.0), 0.0); // would round to 2.0
        assert_eq!(wrap(3.0, 1.0, 1.0), 1.0);
        assert_eq!(ping_pong(0.5, 2.0), 0.5);
        assert_eq!(ping_pong(3.0, 2.0), 1.0);
        assert_eq!(ping_pong(-1.0, 2.0), 1.0);
        assert_eq!(fract_floor(-0.25), 0.75);
        assert_eq!(fract_floor(1.75), 0.75);
        assert_eq!(fract_floor(-1e-10), 0.0);
        assert!((DEG_TO_RAD * 180.0 - PI).abs() < 1e-6);
        assert!((RAD_TO_DEG * PI - 180.0).abs() < 1e-4);
    }

    #[test]
    fn movement() {
        assert_eq!(move_towards(0.0, 10.0, 3.0), 3.0);
        assert_eq!(move_towards(0.0, -10.0, 3.0), -3.0);
        assert_eq!(move_towards(9.0, 10.0, 3.0), 10.0);
        assert_eq!(damp(0.0, 10.0, 5.0, 0.0), 0.0);
        let one_step = damp(0.0, 10.0, 2.0, 0.5);
        assert!((one_step - 10.0 * (1.0 - (-1.0f32).exp())).abs() < 1e-5);
        // Frame-rate independence: two half steps equal one full step.
        let half = damp(0.0, 10.0, 2.0, 0.25);
        let two_halves = damp(half, 10.0, 2.0, 0.25);
        assert!((two_halves - one_step).abs() < 1e-5);
        assert!((damp(3.0, 7.0, 1e9, 1.0) - 7.0).abs() < 1e-6);
    }
}
