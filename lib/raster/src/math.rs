//! Minimal floating-point helpers.
//!
//! `core` on stable Rust does not provide `floor`, `sqrt`, `sin`, `powf`, ... for `f32`. The
//! rasterizer and the font engine (which builds on this crate and re-uses these helpers) need only
//! a handful of them, so they live here instead of in a dependency. Everything is deterministic,
//! never panics, and is accurate to about `f32` precision over the ranges used by 2D geometry
//! (internally most functions evaluate in `f64`).
//!
//! General-purpose code should prefer `vmath`; these helpers exist for crates that must not
//! depend on it.

/// 2^23: every `f32` with a larger magnitude is already an integer.
const INTEGRAL: f32 = 8_388_608.0;

/// Pi as `f32`.
pub const PI: f32 = core::f32::consts::PI;

/// Returns the largest integer less than or equal to `x`. NaN and infinities are returned unchanged.
#[inline]
pub fn floor(x: f32) -> f32 {
    if x.abs() < INTEGRAL {
        let t = x as i32 as f32;
        if t > x { t - 1.0 } else { t }
    } else {
        x
    }
}

/// Returns the smallest integer greater than or equal to `x`. NaN and infinities are returned
/// unchanged.
#[inline]
pub fn ceil(x: f32) -> f32 {
    if x.abs() < INTEGRAL {
        let t = x as i32 as f32;
        if t < x { t + 1.0 } else { t }
    } else {
        x
    }
}

/// Rounds `x` towards zero.
#[inline]
pub fn trunc(x: f32) -> f32 {
    if x.abs() < INTEGRAL { x as i32 as f32 } else { x }
}

/// Rounds `x` to the nearest integer, ties away from zero.
#[inline]
pub fn round(x: f32) -> f32 {
    if x.abs() < INTEGRAL {
        let t = x as i32 as f32;
        let f = x - t; // exact for |x| < 2^23
        if f >= 0.5 {
            t + 1.0
        } else if f <= -0.5 {
            t - 1.0
        } else {
            t
        }
    } else {
        x
    }
}

/// Returns `x - floor(x)`, in `[0, 1)` for finite inputs.
#[inline]
pub fn fract(x: f32) -> f32 {
    x - floor(x)
}

/// Square root. Returns 0 for negative inputs and NaN, and infinity for infinity.
#[inline]
pub fn sqrt(x: f32) -> f32 {
    sqrt64(x as f64) as f32
}

/// `f64` square root. Returns 0 for negative inputs and NaN.
///
/// Uses the SSE2 `sqrtsd` instruction when the target has SSE2 enabled (Vindows user space) and
/// Newton-Raphson iteration otherwise (soft-float targets such as UEFI).
#[inline]
pub fn sqrt64(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        return 0.0;
    }
    sqrt64_positive(x)
}

#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[inline]
fn sqrt64_positive(x: f64) -> f64 {
    use core::arch::x86_64::{_mm_cvtsd_f64, _mm_set_sd, _mm_sqrt_pd};
    // SAFETY: these intrinsics only require SSE2, which this cfg guarantees is statically enabled
    // for the whole compilation; they operate purely on register values.
    unsafe { _mm_cvtsd_f64(_mm_sqrt_pd(_mm_set_sd(x))) }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "sse2")))]
#[inline]
fn sqrt64_positive(x: f64) -> f64 {
    sqrt64_newton(x)
}

/// Software square root for positive inputs (also unit-tested on hosts with SSE2).
#[cfg_attr(all(target_arch = "x86_64", target_feature = "sse2"), allow(dead_code))]
fn sqrt64_newton(x: f64) -> f64 {
    if x == f64::INFINITY {
        return x;
    }
    // Halving the exponent gives an estimate within ~6%; Newton converges quadratically from
    // there (4 iterations reach full precision, 6 also cover subnormal inputs reasonably).
    let bits = x.to_bits();
    let mut y = f64::from_bits((bits >> 1) + 0x1FF8_0000_0000_0000);
    let mut i = 0;
    while i < 6 {
        y = 0.5 * (y + x / y);
        i += 1;
    }
    y
}

/// Euclidean length `sqrt(x^2 + y^2)`.
#[inline]
pub fn hypot(x: f32, y: f32) -> f32 {
    let (x, y) = (x as f64, y as f64);
    sqrt64(x * x + y * y) as f32
}

/// Returns `(sin(x), cos(x))` for an angle in radians. Non-finite inputs give `(NaN, NaN)`.
pub fn sin_cos(x: f32) -> (f32, f32) {
    let (s, c) = sin_cos64(x as f64);
    (s as f32, c as f32)
}

/// Sine of an angle in radians.
#[inline]
pub fn sin(x: f32) -> f32 {
    sin_cos(x).0
}

/// Cosine of an angle in radians.
#[inline]
pub fn cos(x: f32) -> f32 {
    sin_cos(x).1
}

/// Tangent of an angle in radians.
#[inline]
pub fn tan(x: f32) -> f32 {
    let (s, c) = sin_cos64(x as f64);
    (s / c) as f32
}

/// `f64` sine and cosine (quadrant reduction plus Taylor polynomials on `[-pi/4, pi/4]`).
pub fn sin_cos64(x: f64) -> (f64, f64) {
    if !x.is_finite() || x.abs() > 1.0e9 {
        return (f64::NAN, f64::NAN);
    }
    const FRAC_2_PI: f64 = core::f64::consts::FRAC_2_PI;
    const PI_2_HI: f64 = core::f64::consts::FRAC_PI_2;
    const PI_2_LO: f64 = 6.123_233_995_736_766e-17;
    let q = x * FRAC_2_PI;
    let qi = if q >= 0.0 { (q + 0.5) as i64 } else { (q - 0.5) as i64 };
    let qf = qi as f64;
    let r = (x - qf * PI_2_HI) - qf * PI_2_LO;
    let r2 = r * r;
    let s = r
        * (1.0
            + r2 * (-1.0 / 6.0
                + r2 * (1.0 / 120.0
                    + r2 * (-1.0 / 5040.0
                        + r2 * (1.0 / 362_880.0 + r2 * (-1.0 / 39_916_800.0 + r2 * (1.0 / 6_227_020_800.0)))))));
    let c = 1.0
        + r2 * (-0.5
            + r2 * (1.0 / 24.0
                + r2 * (-1.0 / 720.0
                    + r2 * (1.0 / 40_320.0 + r2 * (-1.0 / 3_628_800.0 + r2 * (1.0 / 479_001_600.0))))));
    match qi & 3 {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// Natural logarithm. Returns -inf for 0 and NaN for negative inputs.
pub fn ln(x: f32) -> f32 {
    ln64(x as f64) as f32
}

/// `f64` natural logarithm (exponent extraction plus an atanh series).
pub fn ln64(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x == f64::INFINITY {
        return x;
    }
    let mut x = x;
    let mut e: i64 = 0;
    // Normalize subnormals.
    if x < f64::MIN_POSITIVE {
        x *= 18_014_398_509_481_984.0; // 2^54
        e -= 54;
    }
    let bits = x.to_bits();
    e += ((bits >> 52) & 0x7FF) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
    if m > core::f64::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    let series = z
        * (2.0
            + z2 * (2.0 / 3.0
                + z2 * (2.0 / 5.0
                    + z2 * (2.0 / 7.0
                        + z2 * (2.0 / 9.0 + z2 * (2.0 / 11.0 + z2 * (2.0 / 13.0 + z2 * (2.0 / 15.0))))))));
    e as f64 * core::f64::consts::LN_2 + series
}

/// `e^x`.
pub fn exp(x: f32) -> f32 {
    exp64(x as f64) as f32
}

/// `f64` exponential (range reduction by ln 2 plus a Taylor polynomial).
pub fn exp64(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.0 {
        return f64::INFINITY;
    }
    if x < -708.0 {
        return 0.0;
    }
    let k = x / core::f64::consts::LN_2;
    let ki = if k >= 0.0 { (k + 0.5) as i64 } else { (k - 0.5) as i64 };
    let r = x - ki as f64 * core::f64::consts::LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut i = 1;
    while i <= 13 {
        term *= r / i as f64;
        sum += term;
        i += 1;
    }
    sum * f64::from_bits(((ki + 1023) as u64) << 52)
}

/// `x^y` for `x >= 0`. Returns NaN for negative `x` (unless `y` is 0).
pub fn powf(x: f32, y: f32) -> f32 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f32::INFINITY };
    }
    if x.is_nan() || x <= 0.0 {
        return f32::NAN;
    }
    exp64(y as f64 * ln64(x as f64)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn rounding() {
        assert_eq!(floor(1.5), 1.0);
        assert_eq!(floor(-1.5), -2.0);
        assert_eq!(floor(-2.0), -2.0);
        assert_eq!(floor(3.0), 3.0);
        assert_eq!(ceil(1.2), 2.0);
        assert_eq!(ceil(-1.2), -1.0);
        assert_eq!(ceil(4.0), 4.0);
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -3.0);
        assert_eq!(round(0.499_999_97), 0.0);
        assert_eq!(trunc(-3.7), -3.0);
        assert!(floor(f32::NAN).is_nan());
        assert_eq!(floor(1.0e20), 1.0e20);
        assert!(close(fract(-0.25), 0.75, 1e-7));
    }

    #[test]
    fn roots_and_trig() {
        for i in 0..2000 {
            let x = i as f32 * 0.37 + 0.001;
            let s = sqrt(x);
            assert!(close(s * s, x, x * 1e-6 + 1e-6), "sqrt({x}) = {s}");
        }
        for i in 1..2000 {
            let x = i as f64 * 1.37e-3 * i as f64;
            let s = sqrt64_newton(x);
            assert!((s * s - x).abs() <= x * 1e-14, "sqrt64_newton({x}) = {s}");
        }
        assert_eq!(sqrt(-1.0), 0.0);
        assert_eq!(sqrt(0.0), 0.0);
        assert_eq!(sqrt(f32::NAN), 0.0);
        assert!(close(sqrt(1.0e-30), 1.0e-15, 1e-21));
        for i in -400..400 {
            let a = i as f32 * 0.05;
            let (s, c) = sin_cos(a);
            let (rs, rc) = (std::primitive::f64::sin(a as f64), std::primitive::f64::cos(a as f64));
            assert!(close(s, rs as f32, 2e-7), "sin({a})");
            assert!(close(c, rc as f32, 2e-7), "cos({a})");
        }
        assert!(close(tan(0.5), 0.546_302_5, 1e-6));
    }

    #[test]
    fn exp_ln_pow() {
        for i in 1..500 {
            let x = i as f32 * 0.013;
            assert!(close(ln(x), std::primitive::f64::ln(x as f64) as f32, 1e-6), "ln({x})");
            let e = exp(x - 3.0);
            let r = std::primitive::f64::exp((x - 3.0) as f64) as f32;
            assert!(close(e, r, r * 1e-6), "exp({x})");
        }
        assert!(close(powf(0.5, 2.2), 0.217_637_64, 1e-6));
        assert!(close(powf(2.0, 10.0), 1024.0, 1e-3));
        assert_eq!(powf(0.0, 2.0), 0.0);
        assert!(powf(-1.0, 0.5).is_nan());
    }
}
