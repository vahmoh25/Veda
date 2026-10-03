//! `f32` math functions: the floating-point methods that `std` has but `core` lacks.
//!
//! Every function mirrors the `std` method of the same name, with the receiver
//! as the first argument: `vmath::f32::sin(x)` is `x.sin()`, `atan2(y, x)` is
//! `y.atan2(x)`, `powf(x, n)` is `x.powf(n)`, `mul_add(x, a, b)` is
//! `x.mul_add(a, b)` and `log(x, base)` is `x.log(base)`. Special values
//! (NaN, ±∞, ±0, subnormals) behave like `std`. Extras: [`fmod`] (C `fmodf`,
//! identical to the `%` operator) and [`lerp`].
//!
//! See the [`f64` module](crate::f64) for how to import these functions; most
//! code should simply use the [`FloatExt`](crate::FloatExt) trait (`x.sin()`).
//!
//! # Algorithms and accuracy
//!
//! The functions follow musl's single-precision algorithms, but evaluate the
//! polynomials and argument reductions in `f64` and round once at the end.
//! That is both faster and more accurate than pure `f32` evaluation: nearly
//! all results are correctly rounded and every tested function stays within
//! 1 ulp of the exact result. The hyperbolic functions and `exp_m1` call the
//! `f64` implementations.
//!
//! [`sqrt`] compiles to `sqrtss` when SSE2 is enabled and to an exact integer
//! algorithm on soft-float targets. [`mul_add`] is computed in `f64` (the
//! product is exact there), which matches a fused multiply-add except in rare
//! double-rounding cases.
//!
//! Most functions are `const fn` (useful for compile-time tables); functions
//! that need `sqrt` (`hypot`, `asin`, `acos`, `asinh`, `acosh`) are not.

pub use core::f32::consts;

use crate::f64 as m64;
use core::f64::consts::{FRAC_PI_2, FRAC_PI_4, LN_2, LOG2_E, LOG10_2, LOG10_E, PI};

const SIGN_MASK: u32 = 1 << 31;
const MANT_MASK: u32 = (1 << 23) - 1;

// ---------------------------------------------------------------------------
// Basic operations
// ---------------------------------------------------------------------------

/// Absolute value (clears the sign bit, also for NaN).
#[inline]
pub const fn abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & !SIGN_MASK)
}

/// `x` with the sign of `sign` (also for NaN and ±0).
#[inline]
pub const fn copysign(x: f32, sign: f32) -> f32 {
    f32::from_bits((x.to_bits() & !SIGN_MASK) | (sign.to_bits() & SIGN_MASK))
}

/// `1.0` for positive values (including `+0.0` and `+∞`), `-1.0` for negative
/// values (including `-0.0`), NaN for NaN.
#[inline]
pub const fn signum(x: f32) -> f32 {
    if x.is_nan() { f32::NAN } else { copysign(1.0, x) }
}

/// The smaller of two numbers; if one argument is NaN the other is returned.
#[inline]
pub const fn min(x: f32, y: f32) -> f32 {
    x.min(y)
}

/// The larger of two numbers; if one argument is NaN the other is returned.
#[inline]
pub const fn max(x: f32, y: f32) -> f32 {
    x.max(y)
}

/// Restricts `x` to `[min, max]`; NaN stays NaN.
///
/// # Panics
///
/// Like `std`, panics if `min > max` or either bound is NaN.
#[inline]
pub const fn clamp(x: f32, min: f32, max: f32) -> f32 {
    x.clamp(min, max)
}

/// `x * a + b`, computed in `f64` with a single final rounding to `f32` (the
/// product is exact in `f64`), so it matches a fused multiply-add except in
/// rare double-rounding cases.
#[inline]
pub const fn mul_add(x: f32, a: f32, b: f32) -> f32 {
    (x as f64 * a as f64 + b as f64) as f32
}

/// Converts degrees to radians.
#[inline]
pub const fn to_radians(x: f32) -> f32 {
    x.to_radians()
}

/// Converts radians to degrees.
#[inline]
pub const fn to_degrees(x: f32) -> f32 {
    x.to_degrees()
}

/// `1 / x`.
#[inline]
pub const fn recip(x: f32) -> f32 {
    1.0 / x
}

/// Linear interpolation `a + (b - a) * t` (exact at `t = 0`).
#[inline]
pub const fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ---------------------------------------------------------------------------
// Rounding
// ---------------------------------------------------------------------------

/// The unbiased exponent (128 for ∞/NaN, -127 for zero/subnormals).
#[inline(always)]
const fn exponent(bits: u32) -> i32 {
    ((bits >> 23) & 0xff) as i32 - 0x7f
}

/// Rounds toward zero.
#[inline]
pub const fn trunc(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 23 {
        return x;
    }
    if e < 0 {
        return f32::from_bits(bits & SIGN_MASK);
    }
    f32::from_bits(bits & !(MANT_MASK >> e))
}

/// Rounds toward −∞.
#[inline]
pub const fn floor(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 23 {
        return x;
    }
    if e < 0 {
        return if bits >> 31 == 0 {
            0.0
        } else if bits << 1 == 0 {
            x
        } else {
            -1.0
        };
    }
    let m = MANT_MASK >> e;
    if bits & m == 0 {
        return x;
    }
    let bits = if bits >> 31 != 0 { bits + m } else { bits };
    f32::from_bits(bits & !m)
}

/// Rounds toward +∞.
#[inline]
pub const fn ceil(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 23 {
        return x;
    }
    if e < 0 {
        return if bits >> 31 != 0 {
            -0.0
        } else if bits == 0 {
            x
        } else {
            1.0
        };
    }
    let m = MANT_MASK >> e;
    if bits & m == 0 {
        return x;
    }
    let bits = if bits >> 31 == 0 { bits + m } else { bits };
    f32::from_bits(bits & !m)
}

/// Rounds to the nearest integer, ties away from zero.
#[inline]
pub const fn round(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 23 {
        return x;
    }
    if e < -1 {
        return f32::from_bits(bits & SIGN_MASK);
    }
    if e == -1 {
        return f32::from_bits((bits & SIGN_MASK) | 0x3f80_0000);
    }
    let m = MANT_MASK >> e;
    let half = 1u32 << (22 - e);
    f32::from_bits((bits + half) & !m)
}

/// Rounds to the nearest integer, ties to even.
#[inline]
pub const fn round_ties_even(x: f32) -> f32 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 23 {
        return x;
    }
    let sign = bits & SIGN_MASK;
    if e < -1 {
        return f32::from_bits(sign);
    }
    if e == -1 {
        return if bits & !SIGN_MASK == 0x3f00_0000 {
            f32::from_bits(sign)
        } else {
            f32::from_bits(sign | 0x3f80_0000)
        };
    }
    let m = MANT_MASK >> e;
    let frac = bits & m;
    if frac == 0 {
        return x;
    }
    let half = 1u32 << (22 - e);
    // Weight of the lowest integer bit (for e == 0 the exponent's LSB, which is
    // set for the biased exponent 0x7f, matching the odd integer part 1).
    let one = 1u32 << (23 - e);
    let mut r = bits & !m;
    if frac > half || (frac == half && r & one != 0) {
        r += one;
    }
    f32::from_bits(r)
}

/// The fractional part `x - trunc(x)` (NaN for ±∞).
#[inline]
pub const fn fract(x: f32) -> f32 {
    x - trunc(x)
}

// ---------------------------------------------------------------------------
// Remainders
// ---------------------------------------------------------------------------

/// C `fmodf`: the remainder of `x / y` rounded toward zero, with the sign of
/// `x`. Exact. This is what Rust's `%` operator computes for floats.
pub const fn fmod(x: f32, y: f32) -> f32 {
    let mut uxi = x.to_bits();
    let mut uyi = y.to_bits();
    let mut ex = ((uxi >> 23) & 0xff) as i32;
    let mut ey = ((uyi >> 23) & 0xff) as i32;
    let sx = uxi & SIGN_MASK;

    if uyi << 1 == 0 || y.is_nan() || ex == 0xff {
        return f32::NAN;
    }
    if uxi << 1 <= uyi << 1 {
        if uxi << 1 == uyi << 1 {
            return 0.0 * x;
        }
        return x;
    }

    if ex == 0 {
        let mut i = uxi << 9;
        while i >> 31 == 0 {
            ex -= 1;
            i <<= 1;
        }
        uxi <<= (1 - ex) as u32;
    } else {
        uxi &= MANT_MASK;
        uxi |= 1 << 23;
    }
    if ey == 0 {
        let mut i = uyi << 9;
        while i >> 31 == 0 {
            ey -= 1;
            i <<= 1;
        }
        uyi <<= (1 - ey) as u32;
    } else {
        uyi &= MANT_MASK;
        uyi |= 1 << 23;
    }

    while ex > ey {
        let i = uxi.wrapping_sub(uyi);
        if i >> 31 == 0 {
            if i == 0 {
                return 0.0 * x;
            }
            uxi = i;
        }
        uxi <<= 1;
        ex -= 1;
    }
    let i = uxi.wrapping_sub(uyi);
    if i >> 31 == 0 {
        if i == 0 {
            return 0.0 * x;
        }
        uxi = i;
    }
    while uxi >> 23 == 0 {
        uxi <<= 1;
        ex -= 1;
    }

    if ex > 0 {
        uxi -= 1 << 23;
        uxi |= (ex as u32) << 23;
    } else {
        uxi >>= (1 - ex) as u32;
    }
    f32::from_bits(uxi | sx)
}

/// Euclidean remainder: the least non-negative `r` with `x = n * rhs + r`
/// (computed like `std`, so it can round up to `|rhs|`).
#[inline]
pub const fn rem_euclid(x: f32, rhs: f32) -> f32 {
    let r = fmod(x, rhs);
    if r < 0.0 { r + abs(rhs) } else { r }
}

/// Euclidean division: the integer `n` such that `x = n * rhs + rem_euclid(x, rhs)`.
#[inline]
pub const fn div_euclid(x: f32, rhs: f32) -> f32 {
    let q = trunc(x / rhs);
    if fmod(x, rhs) < 0.0 {
        return if rhs > 0.0 { q - 1.0 } else { q + 1.0 };
    }
    q
}

// ---------------------------------------------------------------------------
// Roots
// ---------------------------------------------------------------------------

/// Square root, correctly rounded. Negative inputs (other than `-0.0`) give NaN.
#[inline]
pub fn sqrt(x: f32) -> f32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_cvtss_f32, _mm_set_ss, _mm_sqrt_ss};
        // SAFETY: the intrinsics only require SSE, which is implied by the SSE2
        // target feature this cfg guarantees for the whole compilation target.
        unsafe { _mm_cvtss_f32(_mm_sqrt_ss(_mm_set_ss(x))) }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse2")))]
    {
        sqrt_soft(x)
    }
}

/// Correctly rounded square root without floating-point hardware (rounding the
/// exact `f64` square root to `f32` is correctly rounded: 53 >= 2·24 + 2).
#[cfg_attr(all(target_arch = "x86_64", target_feature = "sse2"), allow(dead_code))]
pub(crate) const fn sqrt_soft(x: f32) -> f32 {
    m64::sqrt_soft(x as f64) as f32
}

/// Cube root (defined for negative values: `cbrt(-8) = -2`).
pub const fn cbrt(x: f32) -> f32 {
    const B1: u32 = 709_958_130; // (127 - 127/3 - 0.03306235651) * 2^23
    const B2: u32 = 642_849_266; // (127 - 127/3 - 24/3 - 0.03306235651) * 2^23
    const X1P24: f32 = 16_777_216.0;

    let mut ui = x.to_bits();
    let mut hx = ui & 0x7fff_ffff;
    if hx >= 0x7f80_0000 {
        return x + x; // NaN, ±∞
    }
    // Rough cbrt to 5 bits.
    if hx < 0x0080_0000 {
        if hx == 0 {
            return x; // ±0
        }
        ui = (x * X1P24).to_bits();
        hx = ui & 0x7fff_ffff;
        hx = hx / 3 + B2;
    } else {
        hx = hx / 3 + B1;
    }
    ui &= SIGN_MASK;
    ui |= hx;
    // Two Halley iterations in double precision: 16 bits, then 47 bits.
    let xd = x as f64;
    let mut t = f32::from_bits(ui) as f64;
    let mut r = t * t * t;
    t = t * (xd + xd + r) / (xd + r + r);
    r = t * t * t;
    t = t * (xd + xd + r) / (xd + r + r);
    t as f32
}

/// `sqrt(x² + y²)` without undue overflow or underflow. `hypot(±∞, NaN) = +∞`.
pub fn hypot(x: f32, y: f32) -> f32 {
    let ax = x.to_bits() & 0x7fff_ffff;
    let ay = y.to_bits() & 0x7fff_ffff;
    if ax == 0x7f80_0000 || ay == 0x7f80_0000 {
        return f32::INFINITY;
    }
    if ax > 0x7f80_0000 || ay > 0x7f80_0000 {
        return f32::NAN;
    }
    let (xd, yd) = (x as f64, y as f64);
    m64::sqrt(xd * xd + yd * yd) as f32
}

// ---------------------------------------------------------------------------
// Trigonometric functions
// ---------------------------------------------------------------------------

/// `m · 2^-e` (exact): musl's hexadecimal float constants `0x<m>.0p-<e>`.
const fn hexf(m: i64, e: u32) -> f64 {
    m as f64 / (1u128 << e) as f64
}

/// sin(x) for |x| <= π/4; |sin(x)/x - s(x)| < 2^-37.5.
#[inline(always)]
const fn sindf(x: f64) -> f64 {
    const S1: f64 = hexf(-0x15_5555_54cb_ac77, 55); // -0.166666666416265235595
    const S2: f64 = hexf(0x11_1110_896e_fbb2, 59); // 0.0083333293858894631756
    const S3: f64 = hexf(-0x1a_00f9_e2ca_e774, 65); // -0.000198393348360966317347
    const S4: f64 = hexf(0x16_cd87_8c3b_46a7, 71); // 0.0000027183114939898219064
    let z = x * x;
    let w = z * z;
    let r = S3 + z * S4;
    let s = z * x;
    (x + s * (S1 + z * S2)) + s * w * r
}

/// cos(x) for |x| <= π/4; |cos(x) - c(x)| < 2^-34.1.
#[inline(always)]
const fn cosdf(x: f64) -> f64 {
    const C0: f64 = hexf(-0x1f_ffff_fd0c_5e81, 54); // -0.499999997251031003120
    const C1: f64 = hexf(0x15_5553_e105_3a42, 57); // 0.0416666233237390631894
    const C2: f64 = hexf(-0x16_c087_e80f_1e27, 62); // -0.00138867637746099294692
    const C3: f64 = hexf(0x19_9342_e0ee_5069, 68); // 0.0000243904487962774090654
    let z = x * x;
    let w = z * z;
    let r = C2 + z * C3;
    ((1.0 + z * C0) + w * C1) + (w * z) * r
}

/// tan(x) (or -1/tan(x) when `odd`) for |x| <= π/4; |tan(x)/x - t(x)| < 2^-25.5.
#[inline(always)]
const fn tandf(x: f64, odd: bool) -> f64 {
    const T0: f64 = hexf(0x15_554d_3418_c99f, 54); // 0.333331395030791399758
    const T1: f64 = hexf(0x11_12fd_3899_9f72, 55); // 0.133392002712976742718
    const T2: f64 = hexf(0x1b_54c9_1d86_5afe, 57); // 0.0533812378445670393523
    const T3: f64 = hexf(0x19_1df3_908c_33ce, 58); // 0.0245283181166547278873
    const T4: f64 = hexf(0x18_5dad_fcec_f44e, 61); // 0.00297435743359967304927
    const T5: f64 = hexf(0x13_62b9_bf97_1bcd, 59); // 0.00946564784943673166728
    let z = x * x;
    let r = T4 + z * T5;
    let t = T2 + z * T3;
    let w = z * z;
    let s = z * x;
    let u = T0 + z * T1;
    let r = (x + s * u) + (s * w) * (t + w * r);
    if odd { -1.0 / r } else { r }
}

/// Multiples of π/2 rounded to double precision.
const S1PIO2: f64 = FRAC_PI_2;
const S2PIO2: f64 = PI;
const S3PIO2: f64 = 3.0 * FRAC_PI_2;
const S4PIO2: f64 = 2.0 * PI;

/// Reduces finite `x` with |x| > π/4: returns `(n mod 4, y)` with x = n·π/2 + y.
const fn rem_pio2f(x: f32) -> (i32, f64) {
    const INVPIO2: f64 = f64::from_bits(0x3fe4_5f30_6dc9_c883);
    const PIO2_1: f64 = f64::from_bits(0x3ff9_21fb_5000_0000); // first 25 bits of π/2
    const PIO2_1T: f64 = f64::from_bits(0x3e51_10b4_611a_6263); // π/2 - PIO2_1
    let ix = x.to_bits() & 0x7fff_ffff;
    if ix < 0x4dc9_0fdb {
        // |x| ~< 2^28 * (π/2): 25 + 53 bits of π/2 are enough.
        let xd = x as f64;
        let fnum = xd * INVPIO2 + m64::TOINT - m64::TOINT;
        let n = fnum as i32;
        return (n & 3, xd - fnum * PIO2_1 - fnum * PIO2_1T);
    }
    let (n, y0, y1) = m64::rem_pio2_large(x as f64);
    (n, y0 + y1)
}

/// Sine (radians).
pub const fn sin(x: f32) -> f32 {
    let bits = x.to_bits();
    let sign = bits >> 31 != 0;
    let ix = bits & 0x7fff_ffff;
    let xd = x as f64;
    if ix <= 0x3f49_0fda {
        // |x| ~<= π/4
        if ix < 0x3980_0000 {
            return x; // |x| < 2^-12
        }
        return sindf(xd) as f32;
    }
    if ix <= 0x407b_53d1 {
        // |x| ~<= 5π/4
        if ix <= 0x4016_cbe3 {
            // |x| ~<= 3π/4
            return (if sign { -cosdf(xd + S1PIO2) } else { cosdf(xd - S1PIO2) }) as f32;
        }
        return sindf(if sign { -(xd + S2PIO2) } else { -(xd - S2PIO2) }) as f32;
    }
    if ix <= 0x40e2_31d5 {
        // |x| ~<= 9π/4
        if ix <= 0x40af_eddf {
            // |x| ~<= 7π/4
            return (if sign { cosdf(xd + S3PIO2) } else { -cosdf(xd - S3PIO2) }) as f32;
        }
        return sindf(if sign { xd + S4PIO2 } else { xd - S4PIO2 }) as f32;
    }
    if ix >= 0x7f80_0000 {
        return f32::NAN; // sin(±∞) and sin(NaN)
    }
    let (n, y) = rem_pio2f(x);
    (match n {
        0 => sindf(y),
        1 => cosdf(y),
        2 => sindf(-y),
        _ => -cosdf(y),
    }) as f32
}

/// Cosine (radians).
pub const fn cos(x: f32) -> f32 {
    let bits = x.to_bits();
    let sign = bits >> 31 != 0;
    let ix = bits & 0x7fff_ffff;
    let xd = x as f64;
    if ix <= 0x3f49_0fda {
        if ix < 0x3980_0000 {
            return 1.0;
        }
        return cosdf(xd) as f32;
    }
    if ix <= 0x407b_53d1 {
        if ix > 0x4016_cbe3 {
            // |x| ~> 3π/4
            return (-cosdf(if sign { xd + S2PIO2 } else { xd - S2PIO2 })) as f32;
        }
        return (if sign { sindf(xd + S1PIO2) } else { sindf(S1PIO2 - xd) }) as f32;
    }
    if ix <= 0x40e2_31d5 {
        if ix > 0x40af_eddf {
            // |x| ~> 7π/4
            return cosdf(if sign { xd + S4PIO2 } else { xd - S4PIO2 }) as f32;
        }
        return (if sign { sindf(-xd - S3PIO2) } else { sindf(xd - S3PIO2) }) as f32;
    }
    if ix >= 0x7f80_0000 {
        return f32::NAN;
    }
    let (n, y) = rem_pio2f(x);
    (match n {
        0 => cosdf(y),
        1 => sindf(-y),
        2 => -cosdf(y),
        _ => sindf(y),
    }) as f32
}

/// Sine and cosine at once (one argument reduction). Returns `(sin, cos)`,
/// bit-identical to [`sin`] and [`cos`].
pub const fn sin_cos(x: f32) -> (f32, f32) {
    let bits = x.to_bits();
    let sign = bits >> 31 != 0;
    let ix = bits & 0x7fff_ffff;
    let xd = x as f64;
    let (s, c) = if ix <= 0x3f49_0fda {
        if ix < 0x3980_0000 {
            return (x, 1.0);
        }
        (sindf(xd), cosdf(xd))
    } else if ix <= 0x407b_53d1 {
        if ix <= 0x4016_cbe3 {
            if sign { (-cosdf(xd + S1PIO2), sindf(xd + S1PIO2)) } else { (cosdf(xd - S1PIO2), sindf(S1PIO2 - xd)) }
        } else {
            let y = if sign { xd + S2PIO2 } else { xd - S2PIO2 };
            (sindf(-y), -cosdf(y))
        }
    } else if ix <= 0x40e2_31d5 {
        if ix <= 0x40af_eddf {
            if sign { (cosdf(xd + S3PIO2), sindf(-xd - S3PIO2)) } else { (-cosdf(xd - S3PIO2), sindf(xd - S3PIO2)) }
        } else {
            let y = if sign { xd + S4PIO2 } else { xd - S4PIO2 };
            (sindf(y), cosdf(y))
        }
    } else if ix >= 0x7f80_0000 {
        let nan = f32::NAN;
        return (nan, nan);
    } else {
        let (n, y) = rem_pio2f(x);
        match n {
            0 => (sindf(y), cosdf(y)),
            1 => (cosdf(y), sindf(-y)),
            2 => (sindf(-y), -cosdf(y)),
            _ => (-cosdf(y), sindf(y)),
        }
    };
    (s as f32, c as f32)
}

/// Tangent (radians).
pub const fn tan(x: f32) -> f32 {
    let bits = x.to_bits();
    let sign = bits >> 31 != 0;
    let ix = bits & 0x7fff_ffff;
    let xd = x as f64;
    if ix <= 0x3f49_0fda {
        if ix < 0x3980_0000 {
            return x;
        }
        return tandf(xd, false) as f32;
    }
    if ix <= 0x407b_53d1 {
        if ix <= 0x4016_cbe3 {
            return tandf(if sign { xd + S1PIO2 } else { xd - S1PIO2 }, true) as f32;
        }
        return tandf(if sign { xd + S2PIO2 } else { xd - S2PIO2 }, false) as f32;
    }
    if ix <= 0x40e2_31d5 {
        if ix <= 0x40af_eddf {
            return tandf(if sign { xd + S3PIO2 } else { xd - S3PIO2 }, true) as f32;
        }
        return tandf(if sign { xd + S4PIO2 } else { xd - S4PIO2 }, false) as f32;
    }
    if ix >= 0x7f80_0000 {
        return f32::NAN;
    }
    let (n, y) = rem_pio2f(x);
    tandf(y, n & 1 != 0) as f32
}

// ---------------------------------------------------------------------------
// Inverse trigonometric functions
// ---------------------------------------------------------------------------

/// Rational approximation of (asin(x) - x) / x^3 in terms of z = x^2.
#[inline(always)]
const fn asin_r(z: f64) -> f64 {
    const PS0: f64 = 1.666_658_669_7e-01;
    const PS1: f64 = -4.274_342_209_1e-02;
    const PS2: f64 = -8.656_363_003_0e-03;
    const QS1: f64 = -7.066_296_339_0e-01;
    let p = z * (PS0 + z * (PS1 + z * PS2));
    let q = 1.0 + z * QS1;
    p / q
}

/// Arcsine in radians, in [-π/2, π/2]; NaN outside [-1, 1].
pub fn asin(x: f32) -> f32 {
    let bits = x.to_bits();
    let ix = bits & 0x7fff_ffff;
    let xd = x as f64;
    if ix >= 0x3f80_0000 {
        if ix == 0x3f80_0000 {
            return (xd * FRAC_PI_2) as f32; // ±π/2
        }
        return f32::NAN; // |x| > 1 or NaN
    }
    if ix < 0x3f00_0000 {
        // |x| < 0.5
        if ix < 0x3980_0000 {
            return x; // |x| < 2^-12
        }
        return (xd + xd * asin_r(xd * xd)) as f32;
    }
    // 0.5 <= |x| < 1: asin(x) = π/2 - 2 asin(sqrt((1 - |x|) / 2))
    let z = (1.0 - m64::abs(xd)) * 0.5;
    let s = m64::sqrt(z);
    let r = FRAC_PI_2 - 2.0 * (s + s * asin_r(z));
    (if bits >> 31 != 0 { -r } else { r }) as f32
}

/// Arccosine in radians, in [0, π]; NaN outside [-1, 1].
pub fn acos(x: f32) -> f32 {
    let bits = x.to_bits();
    let ix = bits & 0x7fff_ffff;
    let xd = x as f64;
    if ix >= 0x3f80_0000 {
        if ix == 0x3f80_0000 {
            return if bits >> 31 != 0 { PI as f32 } else { 0.0 };
        }
        return f32::NAN;
    }
    if ix < 0x3f00_0000 {
        // |x| < 0.5
        if ix <= 0x3280_0000 {
            return FRAC_PI_2 as f32; // |x| < 2^-26
        }
        return (FRAC_PI_2 - (xd + xd * asin_r(xd * xd))) as f32;
    }
    if bits >> 31 != 0 {
        // x <= -0.5
        let z = (1.0 + xd) * 0.5;
        let s = m64::sqrt(z);
        return (PI - 2.0 * (s + s * asin_r(z))) as f32;
    }
    // x >= 0.5
    let z = (1.0 - xd) * 0.5;
    let s = m64::sqrt(z);
    (2.0 * (s + s * asin_r(z))) as f32
}

/// atan(x) in double precision with single-precision accuracy, for finite x >= 0.
#[inline(always)]
const fn atan_pos(x: f64) -> f64 {
    const AT: [f64; 5] =
        [3.333_332_836_6e-01, -1.999_915_838_2e-01, 1.425_363_570_5e-01, -1.064_801_737_7e-01, 6.168_760_731_8e-02];
    let (id, t) = if x < 0.4375 {
        (4, x)
    } else if x < 0.6875 {
        (0, (2.0 * x - 1.0) / (2.0 + x))
    } else if x < 1.1875 {
        (1, (x - 1.0) / (x + 1.0))
    } else if x < 2.4375 {
        (2, (x - 1.5) / (1.0 + 1.5 * x))
    } else {
        (3, -1.0 / x)
    };
    let z = t * t;
    let w = z * z;
    let s1 = z * (AT[0] + w * (AT[2] + w * AT[4]));
    let s2 = w * (AT[1] + w * AT[3]);
    if id == 4 { t - t * (s1 + s2) } else { m64::ATANHI[id] - ((t * (s1 + s2) - m64::ATANLO[id]) - t) }
}

/// Arctangent in radians, in [-π/2, π/2].
pub const fn atan(x: f32) -> f32 {
    let bits = x.to_bits();
    let ix = bits & 0x7fff_ffff;
    let sign = bits >> 31 != 0;
    if ix >= 0x4c80_0000 {
        // |x| >= 2^26 or NaN
        if ix > 0x7f80_0000 {
            return x;
        }
        let z = FRAC_PI_2 as f32;
        return if sign { -z } else { z };
    }
    if ix < 0x3980_0000 {
        return x; // |x| < 2^-12
    }
    let r = atan_pos(m64::abs(x as f64));
    (if sign { -r } else { r }) as f32
}

/// Four-quadrant arctangent of `y / x` in radians, in [-π, π].
pub const fn atan2(y: f32, x: f32) -> f32 {
    const PI_F: f32 = PI as f32;
    const PIO2_F: f32 = FRAC_PI_2 as f32;
    const PIO4_F: f32 = FRAC_PI_4 as f32;
    const PI3O4_F: f32 = (3.0 * FRAC_PI_4) as f32;
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    let ix = x.to_bits();
    let iy = y.to_bits();
    if ix == 0x3f80_0000 {
        return atan(y); // x = 1.0
    }
    let m = ((iy >> 31) & 1) | ((ix >> 30) & 2); // 2 * sign(x) + sign(y)
    let ix = ix & 0x7fff_ffff;
    let iy = iy & 0x7fff_ffff;
    if iy == 0 {
        return match m {
            0 | 1 => y,
            2 => PI_F,
            _ => -PI_F,
        };
    }
    if ix == 0 {
        return if m & 1 != 0 { -PIO2_F } else { PIO2_F };
    }
    if ix == 0x7f80_0000 {
        if iy == 0x7f80_0000 {
            return match m {
                0 => PIO4_F,
                1 => -PIO4_F,
                2 => PI3O4_F,
                _ => -PI3O4_F,
            };
        }
        return match m {
            0 => 0.0,
            1 => -0.0,
            2 => PI_F,
            _ => -PI_F,
        };
    }
    // |y/x| > 2^26
    if ix + (26 << 23) < iy || iy == 0x7f80_0000 {
        return if m & 1 != 0 { -PIO2_F } else { PIO2_F };
    }
    // z = atan(|y/x|) with correct underflow
    let z = if m & 2 != 0 && iy + (26 << 23) < ix { 0.0 } else { atan_pos(m64::abs(y as f64 / x as f64)) };
    (match m {
        0 => z,
        1 => -z,
        2 => PI - z,
        _ => z - PI,
    }) as f32
}

// ---------------------------------------------------------------------------
// Exponentials
// ---------------------------------------------------------------------------

/// 2^k · e^r for |r| <= ~0.35 (single-precision rational approximation of
/// fdlibm's `expf`, |x (e^x + 1)/(e^x - 1) - p(x)| < 2^-27.74), in double precision.
#[inline(always)]
const fn exp_core(r: f64, k: i32) -> f64 {
    const P1: f64 = 0x00aa_aa8f as f64 / (1u64 << 26) as f64; // 1.6666625440e-1
    const P2: f64 = -(0x00b5_5215 as f64) / (1u64 << 32) as f64; // -2.7667332906e-3
    let rr = r * r;
    let c = r - rr * (P1 + rr * P2);
    (1.0 + (r * c / (2.0 - c) + r)) * m64::pow2i(k)
}

/// `e^x`.
pub const fn exp(x: f32) -> f32 {
    let bits = x.to_bits();
    let ix = bits & 0x7fff_ffff;
    let sign = bits >> 31 != 0;
    if ix >= 0x42ae_ac50 {
        // |x| >= 87.33655 or NaN
        if ix > 0x7f80_0000 {
            return x;
        }
        if !sign && ix >= 0x42b1_7218 {
            return f32::INFINITY; // x >= 88.7228394: overflow
        }
        if sign && ix >= 0x42cf_f1b5 {
            return 0.0; // x <= -103.972084: underflow
        }
    }
    let xd = x as f64;
    let kf = xd * LOG2_E + m64::TOINT - m64::TOINT;
    exp_core(xd - kf * LN_2, kf as i32) as f32
}

/// `2^x`.
pub const fn exp2(x: f32) -> f32 {
    let bits = x.to_bits();
    let ix = bits & 0x7fff_ffff;
    if ix > 0x7f80_0000 {
        return x; // NaN
    }
    if ix >= 0x4300_0000 {
        // |x| >= 128
        if bits >> 31 == 0 {
            return f32::INFINITY;
        }
        if ix >= 0x4316_0000 {
            return 0.0; // x <= -150
        }
    }
    let xd = x as f64;
    let kf = xd + m64::TOINT - m64::TOINT;
    exp_core((xd - kf) * LN_2, kf as i32) as f32
}

/// `e^x - 1`, accurate even for `x` near zero.
#[inline]
pub const fn exp_m1(x: f32) -> f32 {
    m64::exp_m1(x as f64) as f32
}

// ---------------------------------------------------------------------------
// Hyperbolic functions (evaluated in double precision)
// ---------------------------------------------------------------------------

/// Hyperbolic sine.
#[inline]
pub const fn sinh(x: f32) -> f32 {
    m64::sinh(x as f64) as f32
}

/// Hyperbolic cosine.
#[inline]
pub const fn cosh(x: f32) -> f32 {
    m64::cosh(x as f64) as f32
}

/// Hyperbolic tangent.
#[inline]
pub const fn tanh(x: f32) -> f32 {
    m64::tanh(x as f64) as f32
}

/// Inverse hyperbolic sine.
#[inline]
pub fn asinh(x: f32) -> f32 {
    m64::asinh(x as f64) as f32
}

/// Inverse hyperbolic cosine (NaN for `x < 1`).
#[inline]
pub fn acosh(x: f32) -> f32 {
    m64::acosh(x as f64) as f32
}

/// Inverse hyperbolic tangent (±∞ at ±1, NaN outside [-1, 1]).
#[inline]
pub const fn atanh(x: f32) -> f32 {
    m64::atanh(x as f64) as f32
}

// ---------------------------------------------------------------------------
// Logarithms
// ---------------------------------------------------------------------------

/// Splits a positive, finite, normal double `u` into `(k, ln(1 + f))` with
/// u = 2^k (1 + f), 1 + f in [√2/2, √2). Uses musl's single-precision
/// polynomial, |(log(1+s) - log(1-s))/s - Lg(s)| < 2^-34.24.
#[inline(always)]
const fn log_parts(u: f64) -> (f64, f64) {
    const LG1: f64 = 0x00aa_aaaa as f64 / (1u64 << 24) as f64; // 0.66666662693
    const LG2: f64 = 0x00cc_ce13 as f64 / (1u64 << 25) as f64; // 0.40000972152
    const LG3: f64 = 0x0091_e9ee as f64 / (1u64 << 25) as f64; // 0.28498786688
    const LG4: f64 = 0x00f8_9e26 as f64 / (1u64 << 26) as f64; // 0.24279078841
    let bits = u.to_bits();
    let mut hx = (bits >> 32) as u32;
    hx += 0x3ff0_0000 - 0x3fe6_a09e;
    let k = (hx >> 20) as i32 - 0x3ff;
    hx = (hx & 0x000f_ffff) + 0x3fe6_a09e;
    let f = f64::from_bits(((hx as u64) << 32) | (bits & 0xffff_ffff)) - 1.0;
    let s = f / (2.0 + f);
    let z = s * s;
    let w = z * z;
    let r = z * (LG1 + w * LG3) + w * (LG2 + w * LG4);
    // ln(1 + f) = ln((1 + s)/(1 - s)) = 2s + s·R(s²)
    (k as f64, 2.0 * s + s * r)
}

/// Handles the special cases shared by the logarithms: `Err(result)` for ±0,
/// negative values, +∞ and NaN.
#[inline(always)]
const fn log_special(x: f32) -> Result<f64, f32> {
    let bits = x.to_bits();
    if bits << 1 == 0 {
        return Err(f32::NEG_INFINITY);
    }
    if bits >> 31 != 0 {
        return Err(f32::NAN);
    }
    if bits >= 0x7f80_0000 {
        return Err(x);
    }
    Ok(x as f64)
}

/// Natural logarithm (−∞ at ±0, NaN for negative values).
pub const fn ln(x: f32) -> f32 {
    match log_special(x) {
        Err(v) => v,
        Ok(u) => {
            let (k, l) = log_parts(u);
            (k * LN_2 + l) as f32
        }
    }
}

/// Base-2 logarithm (exact for powers of two).
pub const fn log2(x: f32) -> f32 {
    match log_special(x) {
        Err(v) => v,
        Ok(u) => {
            let (k, l) = log_parts(u);
            (k + l * LOG2_E) as f32
        }
    }
}

/// Base-10 logarithm.
pub const fn log10(x: f32) -> f32 {
    match log_special(x) {
        Err(v) => v,
        Ok(u) => {
            let (k, l) = log_parts(u);
            (k * LOG10_2 + l * LOG10_E) as f32
        }
    }
}

/// Logarithm of `x` in base `base`, computed like `std` as `ln(x) / ln(base)`.
#[inline]
pub const fn log(x: f32, base: f32) -> f32 {
    ln(x) / ln(base)
}

/// `ln(1 + x)`, accurate even for `x` near zero.
pub const fn ln_1p(x: f32) -> f32 {
    let bits = x.to_bits();
    let ix = bits & 0x7fff_ffff;
    if ix > 0x7f80_0000 {
        return x; // NaN
    }
    if bits >= 0xbf80_0000 {
        // x <= -1
        return if bits == 0xbf80_0000 { f32::NEG_INFINITY } else { f32::NAN };
    }
    if ix < 0x3380_0000 || bits == 0x7f80_0000 {
        return x; // |x| < 2^-24 (ln(1 + x) rounds to x), or +∞
    }
    // 1 + x is exact in double precision for |x| >= 2^-29.
    let (k, l) = log_parts(1.0 + x as f64);
    (k * LN_2 + l) as f32
}

// ---------------------------------------------------------------------------
// Powers
// ---------------------------------------------------------------------------

/// `x` raised to an integer power, (nearly always) correctly rounded.
///
/// Uses repeated squaring in `f64` for |n| <= 64 (a few multiplications) and
/// `2^(n·log2|x|)` beyond. (`std` leaves the rounding of `powi` unspecified.)
#[inline]
pub const fn powi(x: f32, n: i32) -> f32 {
    let mut b = n.unsigned_abs();
    if b > 64 && x.is_finite() && x != 0.0 {
        let r = pow_pos(abs(x), n as f64);
        return (if x < 0.0 && b & 1 == 1 { -r } else { r }) as f32;
    }
    let mut a = x as f64;
    let mut r = 1.0;
    loop {
        if b & 1 != 0 {
            r *= a;
        }
        b >>= 1;
        if b == 0 {
            break;
        }
        a *= a;
    }
    (if n < 0 { 1.0 / r } else { r }) as f32
}

/// `2^(y·log2(ax))` in double precision for positive, finite, nonzero `ax`
/// (results beyond the `f32` range saturate to 0 or ∞).
#[inline(always)]
const fn pow_pos(ax: f32, y: f64) -> f64 {
    pow_f64(ax as f64, y)
}

/// `u^y` for a positive, finite, normal double `u`, accurate to about 2^-30
/// relative (enough to round correctly to `f32` almost always). Results
/// beyond the `f32` range saturate to 0 or ∞.
#[inline(always)]
pub(crate) const fn pow_f64(u: f64, y: f64) -> f64 {
    let (k, l) = log_parts(u);
    let t = y * (k + l * LOG2_E);
    if t >= 128.0 {
        return f64::INFINITY;
    }
    if t < -160.0 {
        return 0.0;
    }
    let kf = t + m64::TOINT - m64::TOINT;
    exp_core((t - kf) * LN_2, kf as i32)
}

/// Classifies `y` (finite, nonzero): 0 = not an integer, 1 = odd integer, 2 = even integer.
#[inline(always)]
const fn int_class(iy: u32) -> u32 {
    let e = exponent(iy);
    if e < 0 {
        return 0;
    }
    if e > 23 {
        return 2;
    }
    let shift = 23 - e;
    if iy & ((1 << shift) - 1) != 0 {
        return 0;
    }
    if e == 0 || (iy >> shift) & 1 != 0 { 1 } else { 2 }
}

/// `x` raised to the power `y` (C99 `powf` special cases: `powf(x, 0) = 1`
/// and `powf(1, y) = 1` even for NaN, negative bases need integral exponents).
/// Computed as `2^(y·log2|x|)` in double precision.
pub const fn powf(x: f32, y: f32) -> f32 {
    let ix = x.to_bits();
    let iy = y.to_bits();
    let ax = ix & 0x7fff_ffff;
    let ay = iy & 0x7fff_ffff;
    if ay == 0 || ix == 0x3f80_0000 {
        return 1.0; // x^±0 = 1 and 1^y = 1, even for NaN
    }
    if ax > 0x7f80_0000 || ay > 0x7f80_0000 {
        return x + y; // NaN
    }
    let x_neg = ix >> 31 != 0;
    let y_neg = iy >> 31 != 0;
    if ay == 0x7f80_0000 {
        // y = ±∞
        if ax == 0x3f80_0000 {
            return 1.0; // (-1)^±∞
        }
        return if (ax > 0x3f80_0000) != y_neg { f32::INFINITY } else { 0.0 };
    }
    let yint = int_class(iy);
    if ax == 0 || ax == 0x7f80_0000 {
        // x = ±0 or ±∞
        let z = if (ax == 0) == y_neg { f32::INFINITY } else { 0.0 };
        return if x_neg && yint == 1 { -z } else { z };
    }
    if x_neg && yint == 0 {
        return f32::NAN; // negative base, non-integral exponent
    }
    let r = pow_pos(f32::from_bits(ax), y as f64);
    (if x_neg && yint == 1 { -r } else { r }) as f32
}
