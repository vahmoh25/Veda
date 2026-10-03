//! `f64` math functions: the floating-point methods that `std` has but `core` lacks.
//!
//! Every function mirrors the `std` method of the same name, with the receiver
//! as the first argument: `vmath::f64::sin(x)` is `x.sin()`, `atan2(y, x)` is
//! `y.atan2(x)`, `powf(x, n)` is `x.powf(n)`, `mul_add(x, a, b)` is
//! `x.mul_add(a, b)` and `log(x, base)` is `x.log(base)`. Special values
//! (NaN, ±∞, ±0, subnormals) behave like `std`. Extras: [`fmod`] (C `fmod`,
//! identical to the `%` operator), [`scalbn`] (`x · 2^n`) and [`lerp`].
//!
//! Usually you want the [`FloatExt`](crate::FloatExt) trait instead, which
//! exposes all of these as methods (`x.sin()`). To call the free functions,
//! write full paths or import the module under another name
//! (`use vmath::f64 as m64;`). Importing it as `use vmath::f64;` also works:
//! the module re-exports [`consts`], and names it does not define fall back to
//! the primitive type, so `f64::MAX` and `f64::from_bits` keep working while
//! `f64::sqrt(x)` calls this module.
//!
//! # Algorithms and accuracy
//!
//! The transcendental functions are ports of the FreeBSD/musl versions of Sun's
//! `fdlibm` (same polynomials, splitting tricks and special-case handling).
//! Measured against the host C library, `sin`, `cos`, `tan`, `asin`, `acos`,
//! `atan`, `exp`, `exp2`, `exp_m1`, `ln`, `log2`, `log10`, `ln_1p`, `cbrt`,
//! `hypot` and `powf` stay within 1 ulp; `atan2`, `sinh`, `cosh`, `tanh` and
//! the inverse hyperbolic functions within 2 ulp. `sqrt`, the rounding
//! functions, `fmod`, `rem_euclid`, `div_euclid`, `abs`, `copysign` and
//! `signum` are exact.
//!
//! Trigonometric argument reduction is accurate for every finite argument:
//! a three-part Cody–Waite reduction below 2^20·π/2 and an integer
//! Payne–Hanek reduction against a 1216-bit table of 2/π above it.
//!
//! [`sqrt`] compiles to `sqrtsd` when SSE2 is enabled (all user-space targets)
//! and to an exact integer algorithm on soft-float targets such as
//! `x86_64-unknown-uefi`.
//!
//! [`mul_add`] is **not** fused: it computes `x * a + b` with two roundings.
//!
//! Most functions are `const fn` and can be used to build tables at compile
//! time; functions that need `sqrt` (`hypot`, `asin`, `acos`, `asinh`,
//! `acosh`, `powf`) are not.

pub use core::f64::consts;

use core::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

// ---------------------------------------------------------------------------
// Bit helpers
// ---------------------------------------------------------------------------

const SIGN_MASK: u64 = 1 << 63;
const MANT_MASK: u64 = (1 << 52) - 1;

/// 2^1023.
const X1P1023: f64 = f64::from_bits(0x7fe0_0000_0000_0000);
/// 2^-1022 (smallest normal).
const X1P_1022: f64 = f64::from_bits(0x0010_0000_0000_0000);
/// 2^53.
const X1P53: f64 = f64::from_bits(0x4340_0000_0000_0000);
/// 2^54.
const X1P54: f64 = f64::from_bits(0x4350_0000_0000_0000);
/// 2^700.
const X1P700: f64 = f64::from_bits(0x6bb0_0000_0000_0000);
/// 2^-700.
const X1P_700: f64 = f64::from_bits(0x1430_0000_0000_0000);
/// Adding and subtracting this rounds a double with |x| < 2^51 to an integer.
pub(crate) const TOINT: f64 = 1.5 / f64::EPSILON;

/// The high 32 bits of `x`.
#[inline(always)]
const fn high_word(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

/// The low 32 bits of `x`.
#[inline(always)]
const fn low_word(x: f64) -> u32 {
    x.to_bits() as u32
}

/// Builds a double from its high and low 32-bit words.
#[inline(always)]
const fn from_words(hi: u32, lo: u32) -> f64 {
    f64::from_bits(((hi as u64) << 32) | lo as u64)
}

/// `x` with its low 32 bits cleared (a value with at most 21 significant bits).
#[inline(always)]
const fn clear_low_word(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0xffff_ffff_0000_0000)
}

/// `x` with its high word replaced.
#[inline(always)]
const fn with_high_word(x: f64, hi: u32) -> f64 {
    f64::from_bits(((hi as u64) << 32) | (x.to_bits() & 0xffff_ffff))
}

/// 2^e for -1022 <= e <= 1023.
#[inline(always)]
pub(crate) const fn pow2i(e: i32) -> f64 {
    f64::from_bits(((e + 0x3ff) as u64) << 52)
}

// ---------------------------------------------------------------------------
// Basic operations
// ---------------------------------------------------------------------------

/// Absolute value (clears the sign bit, also for NaN).
#[inline]
pub const fn abs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & !SIGN_MASK)
}

/// `x` with the sign of `sign` (also for NaN and ±0).
#[inline]
pub const fn copysign(x: f64, sign: f64) -> f64 {
    f64::from_bits((x.to_bits() & !SIGN_MASK) | (sign.to_bits() & SIGN_MASK))
}

/// `1.0` for positive values (including `+0.0` and `+∞`), `-1.0` for negative
/// values (including `-0.0`), NaN for NaN.
#[inline]
pub const fn signum(x: f64) -> f64 {
    if x.is_nan() { f64::NAN } else { copysign(1.0, x) }
}

/// The smaller of two numbers; if one argument is NaN the other is returned.
#[inline]
pub const fn min(x: f64, y: f64) -> f64 {
    x.min(y)
}

/// The larger of two numbers; if one argument is NaN the other is returned.
#[inline]
pub const fn max(x: f64, y: f64) -> f64 {
    x.max(y)
}

/// Restricts `x` to `[min, max]`; NaN stays NaN.
///
/// # Panics
///
/// Like `std`, panics if `min > max` or either bound is NaN.
#[inline]
pub const fn clamp(x: f64, min: f64, max: f64) -> f64 {
    x.clamp(min, max)
}

/// `x * a + b`. **Not fused**: the product is rounded before the addition.
#[inline]
pub const fn mul_add(x: f64, a: f64, b: f64) -> f64 {
    x * a + b
}

/// Converts degrees to radians.
#[inline]
pub const fn to_radians(x: f64) -> f64 {
    x.to_radians()
}

/// Converts radians to degrees.
#[inline]
pub const fn to_degrees(x: f64) -> f64 {
    x.to_degrees()
}

/// `1 / x`.
#[inline]
pub const fn recip(x: f64) -> f64 {
    1.0 / x
}

/// Linear interpolation `a + (b - a) * t` (exact at `t = 0`).
#[inline]
pub const fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

// ---------------------------------------------------------------------------
// Rounding
// ---------------------------------------------------------------------------

/// The unbiased exponent of `x` (1024 for ∞/NaN, -1023 for zero/subnormals).
#[inline(always)]
const fn exponent(bits: u64) -> i32 {
    ((bits >> 52) & 0x7ff) as i32 - 0x3ff
}

/// Rounds toward zero.
#[inline]
pub const fn trunc(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 52 {
        return x; // integral already, ∞ or NaN
    }
    if e < 0 {
        return f64::from_bits(bits & SIGN_MASK); // |x| < 1 -> ±0
    }
    f64::from_bits(bits & !(MANT_MASK >> e))
}

/// Rounds toward −∞.
#[inline]
pub const fn floor(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 52 {
        return x;
    }
    if e < 0 {
        // |x| < 1: +0 for [+0, 1), -0 stays -0, -1 for (-1, 0).
        return if bits >> 63 == 0 {
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
    // Negative values round away from zero: adding `m` carries into the integer part.
    let bits = if bits >> 63 != 0 { bits + m } else { bits };
    f64::from_bits(bits & !m)
}

/// Rounds toward +∞.
#[inline]
pub const fn ceil(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 52 {
        return x;
    }
    if e < 0 {
        // |x| < 1: -0 for (-1, -0], +0 stays +0, 1 for (0, 1).
        return if bits >> 63 != 0 {
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
    let bits = if bits >> 63 == 0 { bits + m } else { bits };
    f64::from_bits(bits & !m)
}

/// Rounds to the nearest integer, ties away from zero.
#[inline]
pub const fn round(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 52 {
        return x;
    }
    if e < -1 {
        return f64::from_bits(bits & SIGN_MASK); // |x| < 0.5
    }
    if e == -1 {
        return f64::from_bits((bits & SIGN_MASK) | 0x3ff0_0000_0000_0000); // 0.5 <= |x| < 1
    }
    let m = MANT_MASK >> e;
    let half = 1u64 << (51 - e);
    f64::from_bits((bits + half) & !m)
}

/// Rounds to the nearest integer, ties to even.
#[inline]
pub const fn round_ties_even(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = exponent(bits);
    if e >= 52 {
        return x;
    }
    let sign = bits & SIGN_MASK;
    if e < -1 {
        return f64::from_bits(sign);
    }
    if e == -1 {
        // 0.5 rounds to 0, (0.5, 1) to 1.
        return if bits & !SIGN_MASK == 0x3fe0_0000_0000_0000 {
            f64::from_bits(sign)
        } else {
            f64::from_bits(sign | 0x3ff0_0000_0000_0000)
        };
    }
    let m = MANT_MASK >> e;
    let frac = bits & m;
    if frac == 0 {
        return x;
    }
    let half = 1u64 << (51 - e);
    // Weight of the lowest integer bit. For e == 0 this is the exponent's LSB,
    // which is set (biased exponent 0x3ff), matching the odd integer part 1.
    let one = 1u64 << (52 - e);
    let mut r = bits & !m;
    if frac > half || (frac == half && r & one != 0) {
        r += one;
    }
    f64::from_bits(r)
}

/// The fractional part `x - trunc(x)` (NaN for ±∞).
#[inline]
pub const fn fract(x: f64) -> f64 {
    x - trunc(x)
}

// ---------------------------------------------------------------------------
// Remainders
// ---------------------------------------------------------------------------

/// C `fmod`: the remainder of `x / y` rounded toward zero, with the sign of
/// `x`. Exact. This is what Rust's `%` operator computes for floats.
pub const fn fmod(x: f64, y: f64) -> f64 {
    let mut uxi = x.to_bits();
    let mut uyi = y.to_bits();
    let mut ex = ((uxi >> 52) & 0x7ff) as i32;
    let mut ey = ((uyi >> 52) & 0x7ff) as i32;
    let sx = uxi & SIGN_MASK;

    if uyi << 1 == 0 || y.is_nan() || ex == 0x7ff {
        return f64::NAN;
    }
    if uxi << 1 <= uyi << 1 {
        if uxi << 1 == uyi << 1 {
            return 0.0 * x;
        }
        return x;
    }

    // Normalize the significands (bit 52 set) and unbias subnormal exponents.
    if ex == 0 {
        let mut i = uxi << 12;
        while i >> 63 == 0 {
            ex -= 1;
            i <<= 1;
        }
        uxi <<= (1 - ex) as u32;
    } else {
        uxi &= MANT_MASK;
        uxi |= 1 << 52;
    }
    if ey == 0 {
        let mut i = uyi << 12;
        while i >> 63 == 0 {
            ey -= 1;
            i <<= 1;
        }
        uyi <<= (1 - ey) as u32;
    } else {
        uyi &= MANT_MASK;
        uyi |= 1 << 52;
    }

    // Long division, one bit per step.
    while ex > ey {
        let i = uxi.wrapping_sub(uyi);
        if i >> 63 == 0 {
            if i == 0 {
                return 0.0 * x;
            }
            uxi = i;
        }
        uxi <<= 1;
        ex -= 1;
    }
    let i = uxi.wrapping_sub(uyi);
    if i >> 63 == 0 {
        if i == 0 {
            return 0.0 * x;
        }
        uxi = i;
    }
    while uxi >> 52 == 0 {
        uxi <<= 1;
        ex -= 1;
    }

    if ex > 0 {
        uxi -= 1 << 52;
        uxi |= (ex as u64) << 52;
    } else {
        uxi >>= (1 - ex) as u32;
    }
    f64::from_bits(uxi | sx)
}

/// Euclidean remainder: the least non-negative `r` with `x = n * rhs + r`
/// (computed like `std`, so it can round up to `|rhs|`).
#[inline]
pub const fn rem_euclid(x: f64, rhs: f64) -> f64 {
    let r = fmod(x, rhs);
    if r < 0.0 { r + abs(rhs) } else { r }
}

/// Euclidean division: the integer `n` such that `x = n * rhs + rem_euclid(x, rhs)`.
#[inline]
pub const fn div_euclid(x: f64, rhs: f64) -> f64 {
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
pub fn sqrt(x: f64) -> f64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{_mm_cvtsd_f64, _mm_set_sd, _mm_sqrt_sd};
        // SAFETY: the intrinsics only require SSE2, which this cfg guarantees is
        // enabled for the whole compilation target.
        unsafe {
            let v = _mm_set_sd(x);
            _mm_cvtsd_f64(_mm_sqrt_sd(v, v))
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse2")))]
    {
        sqrt_soft(x)
    }
}

/// Correctly rounded square root using only integer arithmetic (used on
/// soft-float targets; always compiled so it can be tested on the host).
#[cfg_attr(all(target_arch = "x86_64", target_feature = "sse2"), allow(dead_code))]
pub(crate) const fn sqrt_soft(x: f64) -> f64 {
    let bits = x.to_bits();
    if bits << 1 == 0 {
        return x; // ±0
    }
    if bits >> 63 != 0 {
        return f64::NAN; // negative (including -∞ and negative NaNs)
    }
    if bits >= 0x7ff0_0000_0000_0000 {
        return x; // +∞ or NaN
    }
    let mut e = (bits >> 52) as i32;
    let mut m = bits & MANT_MASK;
    if e == 0 {
        // Subnormal: normalize so that bit 52 is set.
        let shift = m.leading_zeros() as i32 - 11;
        m <<= shift as u32;
        e = 1 - shift;
    } else {
        m |= 1 << 52;
    }
    // x = m * 2^q with an even q.
    let mut q = e - 1075;
    if q & 1 != 0 {
        m <<= 1;
        q -= 1;
    }
    // r = floor(sqrt(m * 2^56)) has 55 bits; sqrt(x) = sqrt(m * 2^56) * 2^((q - 56) / 2).
    let n = (m as u128) << 56;
    let mut rem = n;
    let mut root: u128 = 0;
    let mut bit: u128 = 1 << 108;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= root + bit {
            rem -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    // root is in [2^54, 2^55): keep 53 bits, round to nearest even.
    let mut mant = (root >> 2) as u64;
    let round_bit = (root >> 1) & 1;
    let sticky = (root & 1) | (rem != 0) as u128;
    if round_bit == 1 && (sticky != 0 || mant & 1 == 1) {
        mant += 1;
    }
    let mut exp = (q - 56) / 2 + 2 + 52 + 0x3ff;
    if mant == 1 << 53 {
        mant >>= 1;
        exp += 1;
    }
    f64::from_bits(((exp as u64) << 52) | (mant & MANT_MASK))
}

/// Cube root (defined for negative values: `cbrt(-8) = -2`).
pub const fn cbrt(x: f64) -> f64 {
    const B1: u32 = 715_094_163; // (1023 - 1023/3 - 0.03306235651) * 2^20
    const B2: u32 = 696_219_795; // (1023 - 1023/3 - 54/3 - 0.03306235651) * 2^20
    // |1/cbrt(x) - p(x)| < 2^-23.5 (~[-7.93e-8, 7.929e-8]).
    const P0: f64 = f64::from_bits(0x3ffe_03e6_0f61_e692);
    const P1: f64 = f64::from_bits(0xbffe_28e0_92f0_2420);
    const P2: f64 = f64::from_bits(0x3ff9_f160_4a49_d6c2);
    const P3: f64 = f64::from_bits(0xbfe8_44cb_bee7_51d9);
    const P4: f64 = f64::from_bits(0x3fc2_b000_d4e4_edd7);

    let mut ui = x.to_bits();
    let mut hx = high_word(x) & 0x7fff_ffff;
    if hx >= 0x7ff0_0000 {
        return x + x; // NaN, ±∞
    }
    // Rough cbrt to 5 bits: cbrt(2^e * (1 + m)) ~= 2^(e/3) * (1 + (e % 3 + m) / 3).
    if hx < 0x0010_0000 {
        // Zero or subnormal.
        ui = (x * X1P54).to_bits();
        hx = (ui >> 32) as u32 & 0x7fff_ffff;
        if hx == 0 {
            return x;
        }
        hx = hx / 3 + B2;
    } else {
        hx = hx / 3 + B1;
    }
    ui &= SIGN_MASK;
    ui |= (hx as u64) << 32;
    let mut t = f64::from_bits(ui);

    // New cbrt to 23 bits: cbrt(x) = t * cbrt(x / t^3) ~= t * P(t^3 / x).
    let r = (t * t) * (t / x);
    t *= (P0 + r * (P1 + r * P2)) + ((r * r) * r) * (P3 + r * P4);

    // Round t away from zero to 23 bits.
    ui = t.to_bits();
    ui = (ui + 0x8000_0000) & 0xffff_ffff_c000_0000;
    t = f64::from_bits(ui);

    // One Newton step to 53 bits with error < 0.667 ulp.
    let s = t * t;
    let mut r = x / s;
    let w = t + t;
    r = (r - t) / (w + r);
    t + t * r
}

/// Splits `x * x` into `hi + lo` exactly (Dekker), for [`hypot`].
#[inline(always)]
const fn sq(x: f64) -> (f64, f64) {
    const SPLIT: f64 = 134_217_729.0; // 2^27 + 1
    let xc = x * SPLIT;
    let xh = x - xc + xc;
    let xl = x - xh;
    let hi = x * x;
    let lo = xh * xh - hi + 2.0 * xh * xl + xl * xl;
    (hi, lo)
}

/// `sqrt(x² + y²)` without undue overflow or underflow. `hypot(±∞, NaN) = +∞`.
pub fn hypot(x: f64, y: f64) -> f64 {
    let mut uxi = x.to_bits() & !SIGN_MASK;
    let mut uyi = y.to_bits() & !SIGN_MASK;
    if uxi < uyi {
        core::mem::swap(&mut uxi, &mut uyi);
    }
    let ex = (uxi >> 52) as i32;
    let ey = (uyi >> 52) as i32;
    let mut x = f64::from_bits(uxi);
    let mut y = f64::from_bits(uyi);
    if ey == 0x7ff {
        return y; // y is ∞ or NaN, so is x: hypot(NaN, ∞) = ∞
    }
    if ex == 0x7ff || uyi == 0 {
        return x;
    }
    if ex - ey > 64 {
        return x + y;
    }
    // Scale so that the squares neither overflow nor underflow.
    let mut z = 1.0;
    if ex > 0x3ff + 510 {
        z = X1P700;
        x *= X1P_700;
        y *= X1P_700;
    } else if ey < 0x3ff - 450 {
        z = X1P_700;
        x *= X1P700;
        y *= X1P700;
    }
    let (hx, lx) = sq(x);
    let (hy, ly) = sq(y);
    z * sqrt(ly + lx + hy + hx)
}

// ---------------------------------------------------------------------------
// Trigonometric kernels (|x| <= ~π/4, x + y is the argument)
// ---------------------------------------------------------------------------

/// sin(x + y) for |x + y| <= ~π/4; `y` is used only when `has_tail`.
#[inline]
const fn k_sin(x: f64, y: f64, has_tail: bool) -> f64 {
    const S1: f64 = f64::from_bits(0xbfc5_5555_5555_5549);
    const S2: f64 = f64::from_bits(0x3f81_1111_1110_f8a6);
    const S3: f64 = f64::from_bits(0xbf2a_01a0_19c1_61d5);
    const S4: f64 = f64::from_bits(0x3ec7_1de3_57b1_fe7d);
    const S5: f64 = f64::from_bits(0xbe5a_e5e6_8a2b_9ceb);
    const S6: f64 = f64::from_bits(0x3de5_d93a_5acf_d57c);
    let z = x * x;
    let w = z * z;
    let r = S2 + z * (S3 + z * S4) + z * w * (S5 + z * S6);
    let v = z * x;
    if !has_tail { x + v * (S1 + z * r) } else { x - ((z * (0.5 * y - v * r) - y) - v * S1) }
}

/// cos(x + y) for |x + y| <= ~π/4.
#[inline]
const fn k_cos(x: f64, y: f64) -> f64 {
    const C1: f64 = f64::from_bits(0x3fa5_5555_5555_554c);
    const C2: f64 = f64::from_bits(0xbf56_c16c_16c1_5177);
    const C3: f64 = f64::from_bits(0x3efa_01a0_19cb_1590);
    const C4: f64 = f64::from_bits(0xbe92_7e4f_809c_52ad);
    const C5: f64 = f64::from_bits(0x3e21_ee9e_bdb4_b1c4);
    const C6: f64 = f64::from_bits(0xbda8_fae9_be88_38d4);
    let z = x * x;
    let w = z * z;
    let r = z * (C1 + z * (C2 + z * C3)) + w * w * (C4 + z * (C5 + z * C6));
    let hz = 0.5 * z;
    let w = 1.0 - hz;
    w + (((1.0 - w) - hz) + (z * r - x * y))
}

/// tan(x + y) (or -1/tan(x + y) when `odd`) for |x + y| <= ~π/4.
const fn k_tan(x: f64, y: f64, odd: bool) -> f64 {
    const T: [f64; 13] = [
        f64::from_bits(0x3fd5_5555_5555_5563),
        f64::from_bits(0x3fc1_1111_1110_fe7a),
        f64::from_bits(0x3fab_a1ba_1bb3_41fe),
        f64::from_bits(0x3f96_64f4_8406_d637),
        f64::from_bits(0x3f82_26e3_e96e_8493),
        f64::from_bits(0x3f6d_6d22_c956_0328),
        f64::from_bits(0x3f57_dbc8_fee0_8315),
        f64::from_bits(0x3f43_44d8_f2f2_6501),
        f64::from_bits(0x3f30_26f7_1a8d_1068),
        f64::from_bits(0x3f14_7e88_a037_92a6),
        f64::from_bits(0x3f12_b80f_32f0_a7e9),
        f64::from_bits(0xbef3_75cb_db60_5373),
        f64::from_bits(0x3efb_2a70_74bf_7ad4),
    ];
    const PIO4_LO: f64 = f64::from_bits(0x3c81_a626_3314_5c07);

    let hx = high_word(x);
    let big = (hx & 0x7fff_ffff) >= 0x3fe5_9428; // |x| >= 0.6744
    let mut x = x;
    let mut y = y;
    let mut sign = false;
    if big {
        sign = hx >> 31 != 0;
        if sign {
            x = -x;
            y = -y;
        }
        x = (FRAC_PI_4 - x) + (PIO4_LO - y);
        y = 0.0;
    }
    let z = x * x;
    let w = z * z;
    // Break x^5 * (T[1] + x^2 * T[2] + ...) into odd and even polynomials.
    let r = T[1] + w * (T[3] + w * (T[5] + w * (T[7] + w * (T[9] + w * T[11]))));
    let v = z * (T[2] + w * (T[4] + w * (T[6] + w * (T[8] + w * (T[10] + w * T[12])))));
    let s = z * x;
    let r = y + z * (s * (r + v) + y) + s * T[0];
    let w = x + r;
    if big {
        let s = if odd { -1.0 } else { 1.0 };
        let v = s - 2.0 * (x + (r - w * w / (w + s)));
        return if sign { -v } else { v };
    }
    if !odd {
        return w;
    }
    // -1/(x + r) has up to 2 ulp error, so compute it accurately.
    let w0 = clear_low_word(w);
    let v = r - (w0 - x); // w0 + v = r + x
    let a = -1.0 / w;
    let a0 = clear_low_word(a);
    a0 + a * (1.0 + a0 * w0 + a0 * v)
}

// ---------------------------------------------------------------------------
// Argument reduction for trigonometric functions
// ---------------------------------------------------------------------------

const INVPIO2: f64 = f64::from_bits(0x3fe4_5f30_6dc9_c883);
/// First 33 bits of π/2.
const PIO2_1: f64 = f64::from_bits(0x3ff9_21fb_5440_0000);
/// π/2 - PIO2_1.
const PIO2_1T: f64 = f64::from_bits(0x3dd0_b461_1a62_6331);
/// Second 33 bits of π/2.
const PIO2_2: f64 = f64::from_bits(0x3dd0_b461_1a60_0000);
/// π/2 - (PIO2_1 + PIO2_2).
const PIO2_2T: f64 = f64::from_bits(0x3ba3_198a_2e03_7073);
/// Third 33 bits of π/2.
const PIO2_3: f64 = f64::from_bits(0x3ba3_198a_2e00_0000);
/// π/2 - (PIO2_1 + PIO2_2 + PIO2_3).
const PIO2_3T: f64 = f64::from_bits(0x397b_839a_2520_49c1);

/// Reduces `x` (|x| > ~π/4, finite or not) to `y0 + y1` in [-π/4, π/4] with
/// `x = n * π/2 + y0 + y1`. Returns `(n mod 4, y0, y1)`; ∞ and NaN give NaN.
const fn rem_pio2(x: f64) -> (i32, f64, f64) {
    let ix = high_word(x) & 0x7fff_ffff;
    if ix < 0x4139_21fb {
        // |x| ~< 2^20 * π/2: Cody-Waite with up to three 33-bit pieces of π/2.
        let fnum = x * INVPIO2 + TOINT - TOINT;
        let n = fnum as i32;
        let mut r = x - fnum * PIO2_1;
        let mut w = fnum * PIO2_1T; // first round, good to 85 bits
        let mut y0 = r - w;
        let ex = (ix >> 20) as i32;
        let ey = ((y0.to_bits() >> 52) & 0x7ff) as i32;
        if ex - ey > 16 {
            // Second round, good to 118 bits.
            let t = r;
            w = fnum * PIO2_2;
            r = t - w;
            w = fnum * PIO2_2T - ((t - r) - w);
            y0 = r - w;
            let ey = ((y0.to_bits() >> 52) & 0x7ff) as i32;
            if ex - ey > 49 {
                // Third round, good to 151 bits, covers all cases.
                let t = r;
                w = fnum * PIO2_3;
                r = t - w;
                w = fnum * PIO2_3T - ((t - r) - w);
                y0 = r - w;
            }
        }
        let y1 = (r - y0) - w;
        return (n & 3, y0, y1);
    }
    if ix >= 0x7ff0_0000 {
        let nan = x - x;
        return (0, nan, nan);
    }
    rem_pio2_large(x)
}

/// The bits of 2/π: entry 0 is zero padding, entry `i >= 1` holds the bits
/// with weights 2^-(64(i-1)+1) ..= 2^-(64i), most significant bit first.
const TWO_OVER_PI: [u64; 20] = [
    0x0000_0000_0000_0000,
    0xa2f9_836e_4e44_1529,
    0xfc27_57d1_f534_ddc0,
    0xdb62_9599_3c43_9041,
    0xfe51_63ab_debb_c561,
    0xb724_6e3a_424d_d2e0,
    0x0649_2eea_09d1_921c,
    0xfe1d_eb1c_b129_a73e,
    0xe882_35f5_2ebb_4484,
    0xe99c_7026_b45f_7e41,
    0x3991_d639_8353_39f4,
    0x9c84_5f8b_bdf9_283b,
    0x1ff8_97ff_de05_980f,
    0xef2f_118b_5a0a_6d1f,
    0x6d36_7ecf_27cb_09b7,
    0x4f46_3f66_9e5f_ea2d,
    0x7527_bac7_ebe5_f17b,
    0x3d07_39f7_8a52_92ea,
    0x6bfb_5fb1_1f8d_5d08,
    0x5603_3046_fc7b_6bab,
];

/// round(π/2 · 2^127).
const PIO2_FIX: u128 = 0xc90f_daa2_2168_c234_c4c6_628b_80dc_1cd1;

/// 64 bits of [`TWO_OVER_PI`] starting `sh` bits into word `i`.
#[inline(always)]
const fn two_over_pi_window(i: usize, sh: u32) -> u64 {
    if sh == 0 { TWO_OVER_PI[i] } else { (TWO_OVER_PI[i] << sh) | (TWO_OVER_PI[i + 1] >> (64 - sh)) }
}

/// Payne–Hanek reduction for finite `x` with |x| >= 2^-9 (used for |x| >= 2^20·π/2):
/// returns `(n mod 4, y0, y1)` with `x = n·π/2 + y0 + y1`, |y0 + y1| <= π/4,
/// accurate to about 2^-76 relative even for the hardest cases.
pub(crate) const fn rem_pio2_large(x: f64) -> (i32, f64, f64) {
    const MASK62: u64 = (1 << 62) - 1;
    const MASK64: u128 = u64::MAX as u128;

    let bits = x.to_bits();
    let x_neg = bits >> 63 != 0;
    let e = ((bits >> 52) & 0x7ff) as i32;
    // |x| = m * 2^k with a 53-bit integer m.
    let m = (bits & MANT_MASK) | (1 << 52);
    let k = e - 1075;

    // |x|·2/π mod 4 only depends on the bits of 2/π with weight below 2^(1-k)
    // (higher bits contribute multiples of 4). Take a 192-bit window W starting
    // at weight 2^-(k-1); then |x|·2/π mod 4 ~= (m·W mod 2^192) · 2^-190.
    let g = (k + 62) as u32;
    let wi = (g / 64) as usize;
    let sh = g % 64;
    let w2 = two_over_pi_window(wi, sh);
    let w1 = two_over_pi_window(wi + 1, sh);
    let w0 = two_over_pi_window(wi + 2, sh);
    let p0 = m as u128 * w0 as u128;
    let p1 = m as u128 * w1 as u128 + (p0 >> 64);
    let p2 = m as u128 * w2 as u128 + (p1 >> 64);
    let q2 = p2 as u64;
    let mut f1 = p1 as u64;
    let mut f0 = p0 as u64;

    // Integer part (top two bits), rounded to nearest using the half bit (bit 189).
    let n = (((q2 >> 62) + ((q2 >> 61) & 1)) & 3) as i32;
    // The low 190 bits as a two's complement fraction in [-1/2, 1/2).
    let mut f2 = q2 & MASK62;
    let frac_neg = f2 >> 61 != 0;
    if frac_neg {
        f0 = (!f0).wrapping_add(1);
        let c0 = (f0 == 0) as u64;
        f1 = (!f1).wrapping_add(c0);
        let c1 = (c0 == 1 && f1 == 0) as u64;
        f2 = (!f2).wrapping_add(c1) & MASK62;
    }

    // Normalize the 192-bit magnitude: |f| ~= nn · 2^(-126 - l), nn in [2^127, 2^128).
    let a = ((f2 as u128) << 64) | f1 as u128;
    let (nn, l) = if a != 0 {
        let l = a.leading_zeros();
        let nn = if l < 64 {
            (a << l) | (f0 >> (64 - l)) as u128
        } else if l == 64 {
            (a << 64) | f0 as u128
        } else {
            (a << l) | ((f0 as u128) << (l - 64))
        };
        (nn, l)
    } else if f0 != 0 {
        let l0 = f0.leading_zeros();
        ((f0 as u128) << (64 + l0), 128 + l0)
    } else {
        return (if x_neg { (4 - n) & 3 } else { n }, 0.0, 0.0);
    };

    // r = |f| · π/2: multiply by round(π/2 · 2^127) and keep the high 128 bits.
    let (n1, n0) = (nn >> 64, nn & MASK64);
    let (c1, c0) = (PIO2_FIX >> 64, PIO2_FIX & MASK64);
    let p00 = n0 * c0;
    let p01 = n0 * c1;
    let p10 = n1 * c0;
    let p11 = n1 * c1;
    let mid = (p00 >> 64) + (p01 & MASK64) + (p10 & MASK64);
    let mut h = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    let mut ex = -125 - l as i32; // r = h · 2^ex
    if h >> 127 == 0 {
        h <<= 1;
        ex -= 1;
    }
    // y0 = top 53 bits (exact), y1 = the next bits (rounded).
    let y0 = (h >> 75) as u64 as f64 * pow2i(ex + 75);
    let y1 = (h & ((1 << 75) - 1)) as f64 * pow2i(ex);
    let neg = frac_neg != x_neg;
    let (y0, y1) = if neg { (-y0, -y1) } else { (y0, y1) };
    (if x_neg { (4 - n) & 3 } else { n }, y0, y1)
}

// ---------------------------------------------------------------------------
// Trigonometric functions
// ---------------------------------------------------------------------------

/// Sine (radians).
pub const fn sin(x: f64) -> f64 {
    let ix = high_word(x) & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        // |x| ~<= π/4
        if ix < 0x3e50_0000 {
            return x; // |x| < 2^-26
        }
        return k_sin(x, 0.0, false);
    }
    if ix >= 0x7ff0_0000 {
        return x - x; // NaN for ±∞ and NaN
    }
    let (n, y0, y1) = rem_pio2(x);
    match n {
        0 => k_sin(y0, y1, true),
        1 => k_cos(y0, y1),
        2 => -k_sin(y0, y1, true),
        _ => -k_cos(y0, y1),
    }
}

/// Cosine (radians).
pub const fn cos(x: f64) -> f64 {
    let ix = high_word(x) & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        if ix < 0x3e46_a09e {
            return 1.0; // |x| < 2^-27 * sqrt(2)
        }
        return k_cos(x, 0.0);
    }
    if ix >= 0x7ff0_0000 {
        return x - x;
    }
    let (n, y0, y1) = rem_pio2(x);
    match n {
        0 => k_cos(y0, y1),
        1 => -k_sin(y0, y1, true),
        2 => -k_cos(y0, y1),
        _ => k_sin(y0, y1, true),
    }
}

/// Sine and cosine at once (one argument reduction). Returns `(sin, cos)`,
/// bit-identical to [`sin`] and [`cos`].
pub const fn sin_cos(x: f64) -> (f64, f64) {
    let ix = high_word(x) & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        let s = if ix < 0x3e50_0000 { x } else { k_sin(x, 0.0, false) };
        let c = if ix < 0x3e46_a09e { 1.0 } else { k_cos(x, 0.0) };
        return (s, c);
    }
    if ix >= 0x7ff0_0000 {
        let nan = x - x;
        return (nan, nan);
    }
    let (n, y0, y1) = rem_pio2(x);
    let s = k_sin(y0, y1, true);
    let c = k_cos(y0, y1);
    match n {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// Tangent (radians).
pub const fn tan(x: f64) -> f64 {
    let ix = high_word(x) & 0x7fff_ffff;
    if ix <= 0x3fe9_21fb {
        if ix < 0x3e40_0000 {
            return x; // |x| < 2^-27
        }
        return k_tan(x, 0.0, false);
    }
    if ix >= 0x7ff0_0000 {
        return x - x;
    }
    let (n, y0, y1) = rem_pio2(x);
    k_tan(y0, y1, n & 1 != 0)
}

// ---------------------------------------------------------------------------
// Inverse trigonometric functions
// ---------------------------------------------------------------------------

const PIO2_HI: f64 = FRAC_PI_2;
const PIO2_LO: f64 = f64::from_bits(0x3c91_a626_3314_5c07);

/// Rational approximation of (asin(x) - x) / x^3 in terms of z = x^2.
#[inline]
const fn asin_r(z: f64) -> f64 {
    const PS0: f64 = f64::from_bits(0x3fc5_5555_5555_5555);
    const PS1: f64 = f64::from_bits(0xbfd4_d612_03eb_6f7d);
    const PS2: f64 = f64::from_bits(0x3fc9_c155_0e88_4455);
    const PS3: f64 = f64::from_bits(0xbfa4_8228_b568_8f3b);
    const PS4: f64 = f64::from_bits(0x3f49_efe0_7501_b288);
    const PS5: f64 = f64::from_bits(0x3f02_3de1_0dfd_f709);
    const QS1: f64 = f64::from_bits(0xc003_3a27_1c8a_2d4b);
    const QS2: f64 = f64::from_bits(0x4000_2ae5_9c59_8ac8);
    const QS3: f64 = f64::from_bits(0xbfe6_066c_1b8d_0159);
    const QS4: f64 = f64::from_bits(0x3fb3_b8c5_b12e_9282);
    let p = z * (PS0 + z * (PS1 + z * (PS2 + z * (PS3 + z * (PS4 + z * PS5)))));
    let q = 1.0 + z * (QS1 + z * (QS2 + z * (QS3 + z * QS4)));
    p / q
}

/// Arcsine in radians, in [-π/2, π/2]; NaN outside [-1, 1].
pub fn asin(x: f64) -> f64 {
    let hx = high_word(x);
    let ix = hx & 0x7fff_ffff;
    if ix >= 0x3ff0_0000 {
        // |x| >= 1 or NaN
        if ((ix - 0x3ff0_0000) | low_word(x)) == 0 {
            return x * PIO2_HI; // asin(±1) = ±π/2
        }
        return f64::NAN;
    }
    if ix < 0x3fe0_0000 {
        // |x| < 0.5
        if ix < 0x3e50_0000 && ix >= 0x0010_0000 {
            return x; // 2^-1022 <= |x| < 2^-26
        }
        return x + x * asin_r(x * x);
    }
    // 1 > |x| >= 0.5
    let z = (1.0 - abs(x)) * 0.5;
    let s = sqrt(z);
    let r = asin_r(z);
    let res = if ix >= 0x3fef_3333 {
        // |x| > 0.975
        PIO2_HI - (2.0 * (s + s * r) - PIO2_LO)
    } else {
        // f + c = sqrt(z)
        let f = clear_low_word(s);
        let c = (z - f * f) / (s + f);
        0.5 * PIO2_HI - (2.0 * s * r - (PIO2_LO - 2.0 * c) - (0.5 * PIO2_HI - 2.0 * f))
    };
    if hx >> 31 != 0 { -res } else { res }
}

/// Arccosine in radians, in [0, π]; NaN outside [-1, 1].
pub fn acos(x: f64) -> f64 {
    let hx = high_word(x);
    let ix = hx & 0x7fff_ffff;
    if ix >= 0x3ff0_0000 {
        if ((ix - 0x3ff0_0000) | low_word(x)) == 0 {
            return if hx >> 31 != 0 { 2.0 * PIO2_HI } else { 0.0 };
        }
        return f64::NAN;
    }
    if ix < 0x3fe0_0000 {
        // |x| < 0.5
        if ix <= 0x3c60_0000 {
            return PIO2_HI; // |x| < 2^-57
        }
        return PIO2_HI - (x - (PIO2_LO - x * asin_r(x * x)));
    }
    if hx >> 31 != 0 {
        // x < -0.5
        let z = (1.0 + x) * 0.5;
        let s = sqrt(z);
        let w = asin_r(z) * s - PIO2_LO;
        return 2.0 * (PIO2_HI - (s + w));
    }
    // x > 0.5
    let z = (1.0 - x) * 0.5;
    let s = sqrt(z);
    let df = clear_low_word(s);
    let c = (z - df * df) / (s + df);
    let w = asin_r(z) * s + c;
    2.0 * (df + w)
}

/// atan(0.5), atan(1), atan(1.5), atan(∞) as `hi + lo` pairs.
pub(crate) const ATANHI: [f64; 4] = [
    f64::from_bits(0x3fdd_ac67_0561_bb4f),
    f64::from_bits(0x3fe9_21fb_5444_2d18),
    f64::from_bits(0x3fef_730b_d281_f69b),
    f64::from_bits(0x3ff9_21fb_5444_2d18),
];
pub(crate) const ATANLO: [f64; 4] = [
    f64::from_bits(0x3c7a_2b7f_222f_65e2),
    f64::from_bits(0x3c81_a626_3314_5c07),
    f64::from_bits(0x3c70_0788_7af0_cbbd),
    f64::from_bits(0x3c91_a626_3314_5c07),
];

/// Arctangent in radians, in [-π/2, π/2].
pub const fn atan(x: f64) -> f64 {
    const AT: [f64; 11] = [
        f64::from_bits(0x3fd5_5555_5555_550d),
        f64::from_bits(0xbfc9_9999_9998_ebc4),
        f64::from_bits(0x3fc2_4924_9200_83ff),
        f64::from_bits(0xbfbc_71c6_fe23_1671),
        f64::from_bits(0x3fb7_45cd_c54c_206e),
        f64::from_bits(0xbfb3_b0f2_af74_9a6d),
        f64::from_bits(0x3fb1_0d66_a0d0_3d51),
        f64::from_bits(0xbfad_de2d_52de_fd9a),
        f64::from_bits(0x3fa9_7b4b_2476_0deb),
        f64::from_bits(0xbfa2_b444_2c6a_6c2f),
        f64::from_bits(0x3f90_ad3a_e322_da11),
    ];
    let hx = high_word(x);
    let sign = hx >> 31 != 0;
    let ix = hx & 0x7fff_ffff;
    if ix >= 0x4410_0000 {
        // |x| >= 2^66 or NaN
        if x.is_nan() {
            return x;
        }
        return if sign { -ATANHI[3] } else { ATANHI[3] };
    }
    let mut x = x;
    let id: usize;
    if ix < 0x3fdc_0000 {
        // |x| < 0.4375
        if ix < 0x3e40_0000 {
            return x; // |x| < 2^-27
        }
        id = 4;
    } else {
        x = abs(x);
        if ix < 0x3ff3_0000 {
            // |x| < 1.1875
            if ix < 0x3fe6_0000 {
                // 7/16 <= |x| < 11/16
                id = 0;
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                // 11/16 <= |x| < 19/16
                id = 1;
                x = (x - 1.0) / (x + 1.0);
            }
        } else if ix < 0x4003_8000 {
            // |x| < 2.4375
            id = 2;
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            // 2.4375 <= |x| < 2^66
            id = 3;
            x = -1.0 / x;
        }
    }
    let z = x * x;
    let w = z * z;
    // Break the sum of AT[i] z^(i+1) into odd and even polynomials.
    let s1 = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let s2 = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));
    if id == 4 {
        return x - x * (s1 + s2);
    }
    let z = ATANHI[id] - (x * (s1 + s2) - ATANLO[id] - x);
    if sign { -z } else { z }
}

/// Four-quadrant arctangent of `y / x` in radians, in [-π, π].
pub const fn atan2(y: f64, x: f64) -> f64 {
    const PI_LO: f64 = f64::from_bits(0x3ca1_a626_3314_5c07);
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    let (mut ix, lx) = (high_word(x), low_word(x));
    let (mut iy, ly) = (high_word(y), low_word(y));
    if (ix.wrapping_sub(0x3ff0_0000) | lx) == 0 {
        return atan(y); // x = 1.0
    }
    let m = ((iy >> 31) & 1) | ((ix >> 30) & 2); // 2 * sign(x) + sign(y)
    ix &= 0x7fff_ffff;
    iy &= 0x7fff_ffff;

    if (iy | ly) == 0 {
        // y = ±0
        return match m {
            0 | 1 => y,
            2 => PI,
            _ => -PI,
        };
    }
    if (ix | lx) == 0 {
        // x = ±0
        return if m & 1 != 0 { -FRAC_PI_2 } else { FRAC_PI_2 };
    }
    if ix == 0x7ff0_0000 {
        // x = ±∞
        if iy == 0x7ff0_0000 {
            return match m {
                0 => FRAC_PI_4,
                1 => -FRAC_PI_4,
                2 => 3.0 * FRAC_PI_4,
                _ => -3.0 * FRAC_PI_4,
            };
        }
        return match m {
            0 => 0.0,
            1 => -0.0,
            2 => PI,
            _ => -PI,
        };
    }
    // |y/x| > 2^64
    if ix + (64 << 20) < iy || iy == 0x7ff0_0000 {
        return if m & 1 != 0 { -FRAC_PI_2 } else { FRAC_PI_2 };
    }
    // z = atan(|y/x|) without spurious underflow
    let z = if m & 2 != 0 && iy + (64 << 20) < ix { 0.0 } else { atan(abs(y / x)) };
    match m {
        0 => z,
        1 => -z,
        2 => PI - (z - PI_LO),
        _ => (z - PI_LO) - PI,
    }
}

// ---------------------------------------------------------------------------
// Exponentials
// ---------------------------------------------------------------------------

const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
const INV_LN2: f64 = f64::from_bits(0x3ff7_1547_652b_82fe);
const EXP_P1: f64 = f64::from_bits(0x3fc5_5555_5555_553e);
const EXP_P2: f64 = f64::from_bits(0xbf66_c16c_16be_bd93);
const EXP_P3: f64 = f64::from_bits(0x3f11_566a_af25_de2c);
const EXP_P4: f64 = f64::from_bits(0xbebb_bd41_c5d2_6bf1);
const EXP_P5: f64 = f64::from_bits(0x3e66_3769_72be_a4d0);

/// `x · 2^n`, computed without intermediate overflow and with a single rounding.
pub const fn scalbn(x: f64, n: i32) -> f64 {
    let mut n = n;
    let mut y = x;
    if n > 1023 {
        y *= X1P1023;
        n -= 1023;
        if n > 1023 {
            y *= X1P1023;
            n -= 1023;
            if n > 1023 {
                n = 1023;
            }
        }
    } else if n < -1022 {
        // Keep the final n below -53 to avoid double rounding in the subnormal range.
        y *= X1P_1022 * X1P53;
        n += 1022 - 53;
        if n < -1022 {
            y *= X1P_1022 * X1P53;
            n += 1022 - 53;
            if n < -1022 {
                n = -1022;
            }
        }
    }
    y * pow2i(n)
}

/// `2^k · e^(hi - lo)` for |hi - lo| <= ~0.35 (the fdlibm `exp` kernel).
#[inline]
const fn exp_kernel(hi: f64, lo: f64, k: i32) -> f64 {
    let x = hi - lo;
    let xx = x * x;
    let c = x - xx * (EXP_P1 + xx * (EXP_P2 + xx * (EXP_P3 + xx * (EXP_P4 + xx * EXP_P5))));
    let y = 1.0 + (x * c / (2.0 - c) - lo + hi);
    if k == 0 { y } else { scalbn(y, k) }
}

/// `e^x`.
pub const fn exp(x: f64) -> f64 {
    let hx = high_word(x);
    let sign = hx >> 31 != 0;
    let hx = hx & 0x7fff_ffff;
    if hx >= 0x4086_232b {
        // |x| >= 708.39 or NaN
        if x.is_nan() {
            return x;
        }
        if x > 709.782712893383973096 {
            return f64::INFINITY; // overflow
        }
        if x < -745.13321910194110842 {
            return 0.0; // underflow
        }
    }
    let (hi, lo, k);
    if hx > 0x3fd6_2e42 {
        // |x| > 0.5 ln2
        k = if hx >= 0x3ff0_a2b2 {
            // |x| >= 1.5 ln2
            (INV_LN2 * x + if sign { -0.5 } else { 0.5 }) as i32
        } else if sign {
            -1
        } else {
            1
        };
        hi = x - k as f64 * LN2_HI; // exact
        lo = k as f64 * LN2_LO;
    } else if hx > 0x3e30_0000 {
        // |x| > 2^-28
        k = 0;
        hi = x;
        lo = 0.0;
    } else {
        return 1.0 + x;
    }
    exp_kernel(hi, lo, k)
}

/// `2^x`.
pub const fn exp2(x: f64) -> f64 {
    // ln 2 = LN2_D + LN2_TAIL, with LN2_D split in two 26-bit halves for an exact product.
    const LN2_D: f64 = f64::from_bits(0x3fe6_2e42_fefa_39ef);
    const LN2_TAIL: f64 = f64::from_bits(0x3c7a_bc9e_3b39_803f);
    const SPLIT: f64 = 134_217_729.0; // 2^27 + 1
    const LN2_H: f64 = {
        let t = LN2_D * SPLIT;
        t - (t - LN2_D)
    };
    const LN2_L: f64 = LN2_D - LN2_H;

    let hx = high_word(x) & 0x7fff_ffff;
    if hx >= 0x408f_f000 {
        // |x| >= 1023 or NaN
        if x.is_nan() {
            return x;
        }
        if x >= 1024.0 {
            return f64::INFINITY;
        }
        if x <= -1075.0 {
            return 0.0;
        }
    }
    if hx < 0x3c90_0000 {
        return 1.0 + x; // |x| < 2^-54
    }
    // x = k + f with |f| <= 1/2 (exact), then 2^f = e^(f ln 2).
    let kf = x + TOINT - TOINT;
    let k = kf as i32;
    let f = x - kf;
    // f * ln2 as an unevaluated sum p + e (Dekker's exact product).
    let p = f * LN2_D;
    let t = f * SPLIT;
    let fh = t - (t - f);
    let fl = f - fh;
    let e = ((fh * LN2_H - p) + fh * LN2_L + fl * LN2_H) + fl * LN2_L + f * LN2_TAIL;
    exp_kernel(p, -e, k)
}

/// `e^x - 1`, accurate even for `x` near zero.
pub const fn exp_m1(x: f64) -> f64 {
    const O_THRESHOLD: f64 = f64::from_bits(0x4086_2e42_fefa_39ef);
    // Scaled coefficients of the rational approximation of R(2z), z = x*x/2.
    const Q1: f64 = f64::from_bits(0xbfa1_1111_1111_10f4);
    const Q2: f64 = f64::from_bits(0x3f5a_01a0_19fe_5585);
    const Q3: f64 = f64::from_bits(0xbf14_ce19_9eaa_dbb7);
    const Q4: f64 = f64::from_bits(0x3ed0_cfca_86e6_5239);
    const Q5: f64 = f64::from_bits(0xbe8a_fdb7_6e09_c32d);

    let hx = high_word(x) & 0x7fff_ffff;
    let sign = x.to_bits() >> 63 != 0;
    let mut x = x;

    // Huge and non-finite arguments.
    if hx >= 0x4043_687a {
        // |x| >= 56 ln2
        if x.is_nan() {
            return x;
        }
        if sign {
            return -1.0;
        }
        if x > O_THRESHOLD {
            return f64::INFINITY;
        }
    }

    // Argument reduction.
    let k: i32;
    let c: f64;
    if hx > 0x3fd6_2e42 {
        // |x| > 0.5 ln2
        let (hi, lo);
        if hx < 0x3ff0_a2b2 {
            // and |x| < 1.5 ln2
            if !sign {
                hi = x - LN2_HI;
                lo = LN2_LO;
                k = 1;
            } else {
                hi = x + LN2_HI;
                lo = -LN2_LO;
                k = -1;
            }
        } else {
            k = (INV_LN2 * x + if sign { -0.5 } else { 0.5 }) as i32;
            let t = k as f64;
            hi = x - t * LN2_HI; // exact
            lo = t * LN2_LO;
        }
        x = hi - lo;
        c = (hi - x) - lo;
    } else if hx < 0x3c90_0000 {
        return x; // |x| < 2^-54
    } else {
        k = 0;
        c = 0.0;
    }

    // x is now in the primary range.
    let hfx = 0.5 * x;
    let hxs = x * hfx;
    let r1 = 1.0 + hxs * (Q1 + hxs * (Q2 + hxs * (Q3 + hxs * (Q4 + hxs * Q5))));
    let t = 3.0 - r1 * hfx;
    let mut e = hxs * ((r1 - t) / (6.0 - x * t));
    if k == 0 {
        return x - (x * e - hxs); // c is 0
    }
    e = x * (e - c) - c;
    e -= hxs;
    // exp(x) ~ 2^k (x_reduced - e + 1)
    if k == -1 {
        return 0.5 * (x - e) - 0.5;
    }
    if k == 1 {
        if x < -0.25 {
            return -2.0 * (e - (x + 0.5));
        }
        return 1.0 + 2.0 * (x - e);
    }
    if !(0..=56).contains(&k) {
        // exp(x) - 1 rounds like exp(x)
        let mut y = x - e + 1.0;
        if k == 1024 {
            y = y * 2.0 * X1P1023;
        } else {
            y *= pow2i(k);
        }
        return y - 1.0;
    }
    let twopk = pow2i(k);
    let uf = pow2i(-k);
    if k < 20 { (x - e + (1.0 - uf)) * twopk } else { (x - (e + uf) + 1.0) * twopk }
}

/// `exp(x) / 2` (times `sign`) for x >= log(f64::MAX), avoiding premature overflow.
const fn expo2(x: f64, sign: f64) -> f64 {
    const K: u32 = 2043;
    const KLN2: f64 = f64::from_bits(0x4096_2066_151a_dd8b);
    // k is odd and scale * scale overflows.
    let scale = from_words((0x3ff + K / 2) << 20, 0);
    exp(x - KLN2) * (sign * scale) * scale
}

// ---------------------------------------------------------------------------
// Hyperbolic functions
// ---------------------------------------------------------------------------

/// Hyperbolic sine.
pub const fn sinh(x: f64) -> f64 {
    let h = if x.to_bits() >> 63 != 0 { -0.5 } else { 0.5 };
    let absx = abs(x);
    let w = high_word(absx);
    if w < 0x4086_2e42 {
        // |x| < log(f64::MAX)
        let t = exp_m1(absx);
        if w < 0x3ff0_0000 {
            if w < 0x3ff0_0000 - (26 << 20) {
                return x;
            }
            return h * (2.0 * t - t * t / (t + 1.0));
        }
        return h * (t + t / (t + 1.0));
    }
    // |x| > log(f64::MAX) or NaN
    expo2(absx, 2.0 * h)
}

/// Hyperbolic cosine.
pub const fn cosh(x: f64) -> f64 {
    let x = abs(x);
    let w = high_word(x);
    if w < 0x3fe6_2e42 {
        // |x| < log(2)
        if w < 0x3ff0_0000 - (26 << 20) {
            return 1.0;
        }
        let t = exp_m1(x);
        return 1.0 + t * t / (2.0 * (1.0 + t));
    }
    if w < 0x4086_2e42 {
        // |x| < log(f64::MAX)
        let t = exp(x);
        return 0.5 * (t + 1.0 / t);
    }
    // |x| > log(f64::MAX) or NaN
    expo2(x, 1.0)
}

/// Hyperbolic tangent.
pub const fn tanh(x: f64) -> f64 {
    let sign = x.to_bits() >> 63 != 0;
    let x = abs(x);
    let w = high_word(x);
    let t = if w > 0x3fe1_93ea {
        // |x| > log(3)/2 ~= 0.5493 or NaN
        if w > 0x4034_0000 {
            // |x| > 20 or NaN
            1.0 - 0.0 / x
        } else {
            let t = exp_m1(2.0 * x);
            1.0 - 2.0 / (t + 2.0)
        }
    } else if w > 0x3fd0_58ae {
        // |x| > log(5/3)/2 ~= 0.2554
        let t = exp_m1(2.0 * x);
        t / (t + 2.0)
    } else if w >= 0x0010_0000 {
        // |x| >= 2^-1022
        let t = exp_m1(-2.0 * x);
        -t / (t + 2.0)
    } else {
        x // subnormal
    };
    if sign { -t } else { t }
}

/// Inverse hyperbolic sine.
pub fn asinh(x: f64) -> f64 {
    let e = (x.to_bits() >> 52) & 0x7ff;
    let sign = x.to_bits() >> 63 != 0;
    let x = abs(x);
    let y = if e >= 0x3ff + 26 {
        // |x| >= 2^26 or ∞ or NaN
        ln(x) + core::f64::consts::LN_2
    } else if e >= 0x3ff + 1 {
        // |x| >= 2
        ln(2.0 * x + 1.0 / (sqrt(x * x + 1.0) + x))
    } else if e >= 0x3ff - 26 {
        // |x| >= 2^-26
        ln_1p(x + x * x / (sqrt(x * x + 1.0) + 1.0))
    } else {
        x
    };
    if sign { -y } else { y }
}

/// Inverse hyperbolic cosine (NaN for `x < 1`).
pub fn acosh(x: f64) -> f64 {
    let e = (x.to_bits() >> 52) & 0x7ff;
    if x < 1.0 {
        return f64::NAN;
    }
    if e < 0x3ff + 1 {
        // 1 <= x < 2
        return ln_1p(x - 1.0 + sqrt((x - 1.0) * (x - 1.0) + 2.0 * (x - 1.0)));
    }
    if e < 0x3ff + 26 {
        // x < 2^26
        return ln(2.0 * x - 1.0 / (x + sqrt(x * x - 1.0)));
    }
    // x >= 2^26, ∞ or NaN
    ln(x) + core::f64::consts::LN_2
}

/// Inverse hyperbolic tangent (±∞ at ±1, NaN outside [-1, 1]).
pub const fn atanh(x: f64) -> f64 {
    let e = (x.to_bits() >> 52) & 0x7ff;
    let sign = x.to_bits() >> 63 != 0;
    let y = abs(x);
    let y = if e < 0x3ff - 1 {
        if e < 0x3ff - 32 {
            y // |x| < 2^-32
        } else {
            // |x| < 0.5
            0.5 * ln_1p(2.0 * y + 2.0 * y * y / (1.0 - y))
        }
    } else {
        // avoid overflow
        0.5 * ln_1p(2.0 * (y / (1.0 - y)))
    };
    if sign { -y } else { y }
}

// ---------------------------------------------------------------------------
// Logarithms
// ---------------------------------------------------------------------------

const LG1: f64 = f64::from_bits(0x3fe5_5555_5555_5593);
const LG2: f64 = f64::from_bits(0x3fd9_9999_9997_fa04);
const LG3: f64 = f64::from_bits(0x3fd2_4924_9422_9359);
const LG4: f64 = f64::from_bits(0x3fcc_71c5_1d8e_78af);
const LG5: f64 = f64::from_bits(0x3fc7_4664_96cb_03de);
const LG6: f64 = f64::from_bits(0x3fc3_9a09_d078_c69f);
const LG7: f64 = f64::from_bits(0x3fc2_f112_df3e_5244);

/// Result of [`log_reduce`]: x = 2^k · (1 + f) with 1 + f in [√2/2, √2).
struct LogParts {
    k: i32,
    f: f64,
    hfsq: f64,
    s: f64,
    r: f64,
}

/// Special-case screening and reduction shared by `ln`, `log2` and `log10`.
/// Returns `Err(result)` for zero, negative, ∞, NaN and 1.
#[inline(always)]
const fn log_reduce(x: f64) -> Result<LogParts, f64> {
    let mut x = x;
    let mut ui = x.to_bits();
    let mut hx = (ui >> 32) as u32;
    let mut k: i32 = 0;
    if hx < 0x0010_0000 || hx >> 31 != 0 {
        if ui << 1 == 0 {
            return Err(f64::NEG_INFINITY); // log(±0) = -∞
        }
        if hx >> 31 != 0 {
            return Err(f64::NAN); // log(-#) = NaN
        }
        // Subnormal: scale up.
        k -= 54;
        x *= X1P54;
        ui = x.to_bits();
        hx = (ui >> 32) as u32;
    } else if hx >= 0x7ff0_0000 {
        return Err(x);
    } else if hx == 0x3ff0_0000 && ui << 32 == 0 {
        return Err(0.0);
    }
    // Reduce x into [√2/2, √2).
    hx += 0x3ff0_0000 - 0x3fe6_a09e;
    k += (hx >> 20) as i32 - 0x3ff;
    hx = (hx & 0x000f_ffff) + 0x3fe6_a09e;
    ui = ((hx as u64) << 32) | (ui & 0xffff_ffff);
    let f = f64::from_bits(ui) - 1.0;
    let hfsq = 0.5 * f * f;
    let s = f / (2.0 + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    Ok(LogParts { k, f, hfsq, s, r: t2 + t1 })
}

/// Natural logarithm (−∞ at ±0, NaN for negative values).
pub const fn ln(x: f64) -> f64 {
    match log_reduce(x) {
        Err(v) => v,
        Ok(p) => {
            let dk = p.k as f64;
            p.s * (p.hfsq + p.r) + dk * LN2_LO - p.hfsq + p.f + dk * LN2_HI
        }
    }
}

/// Base-2 logarithm (exact for powers of two).
pub const fn log2(x: f64) -> f64 {
    const IVLN2HI: f64 = f64::from_bits(0x3ff7_1547_6520_0000);
    const IVLN2LO: f64 = f64::from_bits(0x3de7_05fc_2eef_a200);
    match log_reduce(x) {
        Err(v) => v,
        Ok(p) => {
            // hi + lo = f - hfsq + s * (hfsq + R) ~ log(1 + f)
            let hi = clear_low_word(p.f - p.hfsq);
            let lo = p.f - hi - p.hfsq + p.s * (p.hfsq + p.r);
            let val_hi = hi * IVLN2HI;
            let mut val_lo = (lo + hi) * IVLN2LO + lo * IVLN2HI;
            let y = p.k as f64;
            let w = y + val_hi;
            val_lo += (y - w) + val_hi;
            val_lo + w
        }
    }
}

/// Base-10 logarithm.
pub const fn log10(x: f64) -> f64 {
    const IVLN10HI: f64 = f64::from_bits(0x3fdb_cb7b_1520_0000);
    const IVLN10LO: f64 = f64::from_bits(0x3dbb_9438_ca9a_add5);
    const LOG10_2HI: f64 = f64::from_bits(0x3fd3_4413_509f_6000);
    const LOG10_2LO: f64 = f64::from_bits(0x3d59_fef3_11f1_2b36);
    match log_reduce(x) {
        Err(v) => v,
        Ok(p) => {
            let hi = clear_low_word(p.f - p.hfsq);
            let lo = p.f - hi - p.hfsq + p.s * (p.hfsq + p.r);
            let val_hi = hi * IVLN10HI;
            let dk = p.k as f64;
            let y = dk * LOG10_2HI;
            let mut val_lo = dk * LOG10_2LO + (lo + hi) * IVLN10LO + lo * IVLN10HI;
            let w = y + val_hi;
            val_lo += (y - w) + val_hi;
            val_lo + w
        }
    }
}

/// Logarithm of `x` in base `base`, computed like `std` as `ln(x) / ln(base)`.
#[inline]
pub const fn log(x: f64, base: f64) -> f64 {
    ln(x) / ln(base)
}

/// `ln(1 + x)`, accurate even for `x` near zero.
pub const fn ln_1p(x: f64) -> f64 {
    let ui = x.to_bits();
    let hx = (ui >> 32) as u32;
    let mut k: i32 = 1;
    let mut c = 0.0;
    let mut f = 0.0;
    if hx < 0x3fda_827a || hx >> 31 != 0 {
        // 1 + x < √2+
        if hx >= 0xbff0_0000 {
            // x <= -1.0 (or negative NaN)
            if x == -1.0 {
                return f64::NEG_INFINITY;
            }
            return f64::NAN;
        }
        if hx << 1 < 0x3ca0_0000 << 1 {
            return x; // |x| < 2^-53
        }
        if hx <= 0xbfd2_bec4 {
            // √2/2- <= 1 + x < √2+
            k = 0;
            c = 0.0;
            f = x;
        }
    } else if hx >= 0x7ff0_0000 {
        return x;
    }
    if k != 0 {
        let u = 1.0 + x;
        let mut hu = high_word(u);
        hu += 0x3ff0_0000 - 0x3fe6_a09e;
        k = (hu >> 20) as i32 - 0x3ff;
        // Correction term ~ log(1 + x) - log(u), avoiding underflow in c / u.
        if k < 54 {
            c = if k >= 2 { 1.0 - (u - x) } else { x - (u - 1.0) };
            c /= u;
        } else {
            c = 0.0;
        }
        // Reduce u into [√2/2, √2).
        hu = (hu & 0x000f_ffff) + 0x3fe6_a09e;
        f = with_high_word(u, hu) - 1.0;
    }
    let hfsq = 0.5 * f * f;
    let s = f / (2.0 + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    let r = t2 + t1;
    let dk = k as f64;
    s * (hfsq + r) + (dk * LN2_LO + c) - hfsq + f + dk * LN2_HI
}

// ---------------------------------------------------------------------------
// Powers
// ---------------------------------------------------------------------------

/// `x` raised to an integer power by repeated squaring (the same algorithm and
/// rounding as `std`'s `powi`).
#[inline]
pub const fn powi(x: f64, n: i32) -> f64 {
    let mut a = x;
    let mut b = n.unsigned_abs();
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
    if n < 0 { 1.0 / r } else { r }
}

/// `x` raised to the power `y` (C99 `pow` special cases: `powf(x, 0) = 1` and
/// `powf(1, y) = 1` even for NaN, negative bases need integral exponents).
pub fn powf(x: f64, y: f64) -> f64 {
    const BP: [f64; 2] = [1.0, 1.5];
    const DP_H: [f64; 2] = [0.0, f64::from_bits(0x3fe2_b803_4000_0000)];
    const DP_L: [f64; 2] = [0.0, f64::from_bits(0x3e4c_fdeb_43cf_d006)];
    const TWO53: f64 = 9_007_199_254_740_992.0;
    const HUGE: f64 = 1.0e300;
    const TINY: f64 = 1.0e-300;
    // Polynomial coefficients for (3/2) * (log(x) - 2s - 2/3 * s^3).
    const L1: f64 = f64::from_bits(0x3fe3_3333_3333_3303);
    const L2: f64 = f64::from_bits(0x3fdb_6db6_db6f_abff);
    const L3: f64 = f64::from_bits(0x3fd5_5555_518f_264d);
    const L4: f64 = f64::from_bits(0x3fd1_7460_a91d_4101);
    const L5: f64 = f64::from_bits(0x3fcd_864a_93c9_db65);
    const L6: f64 = f64::from_bits(0x3fca_7e28_4a45_4eef);
    const LG2: f64 = f64::from_bits(0x3fe6_2e42_fefa_39ef);
    const LG2_H: f64 = f64::from_bits(0x3fe6_2e43_0000_0000);
    const LG2_L: f64 = f64::from_bits(0xbe20_5c61_0ca8_6c39);
    const OVT: f64 = 8.0085662595372944372e-17; // -(1024 - log2(ovfl + 0.5ulp))
    const CP: f64 = f64::from_bits(0x3fee_c709_dc3a_03fd); // 2/(3 ln2)
    const CP_H: f64 = f64::from_bits(0x3fee_c709_e000_0000);
    const CP_L: f64 = f64::from_bits(0xbe3e_2fe0_145b_01f5);
    const IVLN2: f64 = f64::from_bits(0x3ff7_1547_652b_82fe);
    const IVLN2_H: f64 = f64::from_bits(0x3ff7_1547_6000_0000);
    const IVLN2_L: f64 = f64::from_bits(0x3e54_ae0b_f85d_df44);

    let hx = high_word(x) as i32;
    let lx = low_word(x);
    let hy = high_word(y) as i32;
    let ly = low_word(y);
    let mut ix = hx & 0x7fff_ffff;
    let iy = hy & 0x7fff_ffff;

    // x**0 = 1, even if x is NaN.
    if (iy as u32 | ly) == 0 {
        return 1.0;
    }
    // 1**y = 1, even if y is NaN.
    if hx == 0x3ff0_0000 && lx == 0 {
        return 1.0;
    }
    // NaN if either argument is NaN.
    if ix > 0x7ff0_0000 || (ix == 0x7ff0_0000 && lx != 0) || iy > 0x7ff0_0000 || (iy == 0x7ff0_0000 && ly != 0) {
        return x + y;
    }

    // When x < 0: yisint = 0 (y not an integer), 1 (odd) or 2 (even).
    let mut yisint = 0;
    if hx < 0 {
        if iy >= 0x4340_0000 {
            yisint = 2; // |y| >= 2^53: even
        } else if iy >= 0x3ff0_0000 {
            let k = (iy >> 20) - 0x3ff; // exponent
            if k > 20 {
                let j = ly >> (52 - k);
                if (j << (52 - k)) == ly {
                    yisint = 2 - (j & 1) as i32;
                }
            } else if ly == 0 {
                let j = iy >> (20 - k);
                if (j << (20 - k)) == iy {
                    yisint = 2 - (j & 1);
                }
            }
        }
    }

    // Special values of y.
    if ly == 0 {
        if iy == 0x7ff0_0000 {
            // y = ±∞
            if ((ix - 0x3ff0_0000) as u32 | lx) == 0 {
                return 1.0; // (-1)**±∞ = 1
            }
            if ix >= 0x3ff0_0000 {
                return if hy >= 0 { y } else { 0.0 }; // (|x| > 1)**±∞ = ∞, 0
            }
            return if hy >= 0 { 0.0 } else { -y }; // (|x| < 1)**±∞ = 0, ∞
        }
        if iy == 0x3ff0_0000 {
            // y = ±1
            return if hy >= 0 { x } else { 1.0 / x };
        }
        if hy == 0x4000_0000 {
            return x * x; // y = 2
        }
        if hy == 0x3fe0_0000 && hx >= 0 {
            return sqrt(x); // y = 0.5, x >= +0
        }
    }

    let mut ax = abs(x);
    // Special values of x: ±0, ±∞, ±1.
    if lx == 0 && (ix == 0x7ff0_0000 || ix == 0 || ix == 0x3ff0_0000) {
        let mut z = ax;
        if hy < 0 {
            z = 1.0 / z;
        }
        if hx < 0 {
            if ((ix - 0x3ff0_0000) | yisint) == 0 {
                z = f64::NAN; // (-1)**non-integer
            } else if yisint == 1 {
                z = -z; // (x < 0)**odd = -(|x|**odd)
            }
        }
        return z;
    }

    let mut s = 1.0; // sign of the result
    if hx < 0 {
        if yisint == 0 {
            return f64::NAN; // (x < 0)**non-integer
        }
        if yisint == 1 {
            s = -1.0;
        }
    }

    // t1 + t2 = log2(|x|) in extra precision.
    let t1;
    let t2;
    if iy > 0x41e0_0000 {
        // |y| > 2^31
        if iy > 0x43f0_0000 {
            // |y| > 2^64: must overflow or underflow
            if ix <= 0x3fef_ffff {
                return if hy < 0 { HUGE * HUGE } else { TINY * TINY };
            }
            if ix >= 0x3ff0_0000 {
                return if hy > 0 { HUGE * HUGE } else { TINY * TINY };
            }
        }
        // Over/underflow if x is not close to one.
        if ix < 0x3fef_ffff {
            return if hy < 0 { s * HUGE * HUGE } else { s * TINY * TINY };
        }
        if ix > 0x3ff0_0000 {
            return if hy > 0 { s * HUGE * HUGE } else { s * TINY * TINY };
        }
        // |1 - x| <= 2^-20: log(x) ~= x - x^2/2 + x^3/3 - x^4/4.
        let t = ax - 1.0;
        let w = (t * t) * (0.5 - t * (0.333_333_333_333_333_333_33 - t * 0.25));
        let u = IVLN2_H * t;
        let v = t * IVLN2_L - w * IVLN2;
        t1 = clear_low_word(u + v);
        t2 = v - (t1 - u);
    } else {
        let mut n: i32 = 0;
        // Subnormal x.
        if ix < 0x0010_0000 {
            ax *= TWO53;
            n -= 53;
            ix = high_word(ax) as i32;
        }
        n += (ix >> 20) - 0x3ff;
        let j = ix & 0x000f_ffff;
        // Determine the interval.
        ix = j | 0x3ff0_0000;
        let k: usize;
        if j <= 0x3988e {
            k = 0; // |x| < √(3/2)
        } else if j < 0xbb67a {
            k = 1; // |x| < √3
        } else {
            k = 0;
            n += 1;
            ix -= 0x0010_0000;
        }
        ax = with_high_word(ax, ix as u32);

        // ss = s_h + s_l = (x - 1)/(x + 1) or (x - 1.5)/(x + 1.5)
        let u = ax - BP[k];
        let v = 1.0 / (ax + BP[k]);
        let ss = u * v;
        let s_h = clear_low_word(ss);
        // t_h = ax + bp[k] high
        let t_h = from_words(((ix as u32 >> 1) | 0x2000_0000) + 0x0008_0000 + ((k as u32) << 18), 0);
        let t_l = ax - (t_h - BP[k]);
        let s_l = v * ((u - s_h * t_h) - s_h * t_l);
        // log(ax)
        let s2 = ss * ss;
        let mut r = s2 * s2 * (L1 + s2 * (L2 + s2 * (L3 + s2 * (L4 + s2 * (L5 + s2 * L6)))));
        r += s_l * (s_h + ss);
        let s2 = s_h * s_h;
        let t_h = clear_low_word(3.0 + s2 + r);
        let t_l = r - ((t_h - 3.0) - s2);
        // u + v = ss * (1 + ...)
        let u = s_h * t_h;
        let v = s_l * t_h + t_l * ss;
        // 2/(3 log2) * (ss + ...)
        let p_h = clear_low_word(u + v);
        let p_l = v - (p_h - u);
        let z_h = CP_H * p_h;
        let z_l = CP_L * p_h + p_l * CP + DP_L[k];
        // log2(ax) = (ss + ...) * 2/(3 log2) = n + dp_h + z_h + z_l
        let t = n as f64;
        t1 = clear_low_word(((z_h + z_l) + DP_H[k]) + t);
        t2 = z_l - (((t1 - t) - DP_H[k]) - z_h);
    }

    // Split y into y1 + y2 and compute (y1 + y2) * (t1 + t2).
    let y1 = clear_low_word(y);
    let p_l = (y - y1) * t1 + y * t2;
    let mut p_h = y1 * t1;
    let z = p_l + p_h;
    let j = high_word(z) as i32;
    let i = low_word(z);
    if j >= 0x4090_0000 {
        // z >= 1024
        if ((j - 0x4090_0000) as u32 | i) != 0 {
            return s * HUGE * HUGE; // overflow
        }
        if p_l + OVT > z - p_h {
            return s * HUGE * HUGE;
        }
    } else if (j & 0x7fff_ffff) >= 0x4090_cc00 {
        // z <= -1075
        if ((j as u32).wrapping_sub(0xc090_cc00) | i) != 0 {
            return s * TINY * TINY; // underflow
        }
        if p_l <= z - p_h {
            return s * TINY * TINY;
        }
    }

    // 2^(p_h + p_l)
    let i = j & 0x7fff_ffff;
    let mut k = (i >> 20) - 0x3ff;
    let mut n: i32 = 0;
    if i > 0x3fe0_0000 {
        // |z| > 0.5: n = [z + 0.5]
        n = j + (0x0010_0000 >> (k + 1));
        k = ((n & 0x7fff_ffff) >> 20) - 0x3ff; // new k for n
        let t = from_words((n & !(0x000f_ffff >> k)) as u32, 0);
        n = ((n & 0x000f_ffff) | 0x0010_0000) >> (20 - k);
        if j < 0 {
            n = -n;
        }
        p_h -= t;
    }
    let t = clear_low_word(p_l + p_h);
    let u = t * LG2_H;
    let v = (p_l - (t - p_h)) * LG2 + t * LG2_L;
    let mut z = u + v;
    let w = v - (z - u);
    let t = z * z;
    let t1 = z - t * (EXP_P1 + t * (EXP_P2 + t * (EXP_P3 + t * (EXP_P4 + t * EXP_P5))));
    let r = (z * t1) / (t1 - 2.0) - (w + z * w);
    z = 1.0 - (r - z);
    let j = (high_word(z) as i32).wrapping_add(n << 20);
    if (j >> 20) <= 0 {
        z = scalbn(z, n); // subnormal output
    } else {
        z = with_high_word(z, j as u32);
    }
    s * z
}
