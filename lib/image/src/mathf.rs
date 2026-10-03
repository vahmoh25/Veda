//! Minimal floating-point helpers.
//!
//! `core` on stable Rust has no `floor` or `sin`, and this crate must not depend on `vmath`. Only
//! the resampler uses floating point, and only to compute filter weights once per output row or
//! column, so these straightforward `f64` implementations are plenty.

/// Absolute value.
#[inline]
pub(crate) fn abs(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
}

/// Largest integer `<= x` (for `|x| < 2^52`, which covers every coordinate this crate uses).
#[inline]
pub(crate) fn floor(x: f64) -> f64 {
    if abs(x) >= 4_503_599_627_370_496.0 || x.is_nan() {
        return x;
    }
    let t = x as i64 as f64;
    if t > x { t - 1.0 } else { t }
}

/// Smallest integer `>= x`.
#[inline]
pub(crate) fn ceil(x: f64) -> f64 {
    -floor(-x)
}

/// `sin(pi * x)`, accurate to about 1e-9 for any finite `x` of moderate size.
pub(crate) fn sin_pi(x: f64) -> f64 {
    // Reduce to t in [-1, 1): sin(pi x) has period 2.
    let mut t = x - 2.0 * floor(x * 0.5 + 0.5);
    // Fold into [-0.5, 0.5] using sin(pi t) = sin(pi (1 - t)) = sin(pi (-1 - t)).
    if t > 0.5 {
        t = 1.0 - t;
    } else if t < -0.5 {
        t = -1.0 - t;
    }
    let u = core::f64::consts::PI * t;
    let u2 = u * u;
    // Taylor series up to u^13; the truncation error for |u| <= pi/2 is below 1e-9.
    u * (1.0
        + u2 * (-1.0 / 6.0
            + u2 * (1.0 / 120.0
                + u2 * (-1.0 / 5040.0
                    + u2 * (1.0 / 362_880.0 + u2 * (-1.0 / 39_916_800.0 + u2 * (1.0 / 6_227_020_800.0)))))))
}

/// The normalized sinc function `sin(pi x) / (pi x)`.
pub(crate) fn sinc(x: f64) -> f64 {
    if abs(x) < 1e-9 { 1.0 } else { sin_pi(x) / (core::f64::consts::PI * x) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(floor(1.5), 1.0);
        assert_eq!(floor(-1.5), -2.0);
        assert_eq!(floor(-2.0), -2.0);
        assert_eq!(ceil(1.2), 2.0);
        assert_eq!(ceil(-1.2), -1.0);
        assert_eq!(ceil(3.0), 3.0);
        // Compare against reference values of sin(pi x).
        let cases = [
            (0.0, 0.0),
            (0.5, 1.0),
            (1.0, 0.0),
            (1.5, -1.0),
            (0.25, core::f64::consts::FRAC_1_SQRT_2),
            (-0.25, -core::f64::consts::FRAC_1_SQRT_2),
            (2.75, core::f64::consts::FRAC_1_SQRT_2),
            (-2.9, -0.309_016_994_374_947_4),
            (1.0 / 6.0, 0.5),
        ];
        for (x, want) in cases {
            assert!(abs(sin_pi(x) - want) < 1e-9, "sin_pi({x}) = {} want {want}", sin_pi(x));
        }
        assert!(abs(sinc(0.0) - 1.0) < 1e-12);
        assert!(abs(sinc(1.0)) < 1e-9);
        assert!(abs(sinc(0.5) - 2.0 / core::f64::consts::PI) < 1e-9);
    }
}
