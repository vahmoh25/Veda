//! The [`FloatExt`] trait: `std`'s floating-point methods for `no_std` code.

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// `std`'s floating-point methods on `f32` and `f64`, for `no_std` crates.
///
/// With `use vmath::FloatExt;` in scope, code such as `x.sqrt()`,
/// `y.atan2(x)` or `a.mul_add(b, c)` compiles unchanged in `no_std` crates and
/// behaves like `std`. Every method has the same name and signature as the
/// inherent `std` method and forwards to the free function of the same name in
/// [`crate::f32`] / [`crate::f64`] (see there for algorithms and accuracy).
/// [`lerp`](FloatExt::lerp) is an addition.
///
/// Inherent methods take precedence over trait methods. Where `std` is linked
/// (host tests, tools), `x.sin()` therefore calls `std`, and the few methods
/// that `core` itself provides (`abs`, `min`, `max`, `clamp`, `signum`,
/// `copysign`, `recip`, `to_degrees`, `to_radians`) always resolve to `core`.
/// The results agree; call `FloatExt::sin(x)` to force this implementation.
///
/// Notable behaviour (as in `std`): `min`/`max` ignore NaN arguments, `clamp`
/// panics if `min > max` or a bound is NaN, `round` rounds ties away from zero,
/// `powf(x, 0) == 1` for every `x`. `mul_add` is not fused for `f64` (two
/// roundings) and computed via `f64` for `f32`.
///
/// ```
/// use vmath::FloatExt;
///
/// let angle = FloatExt::atan2(1.0f32, 1.0);
/// assert!((FloatExt::to_degrees(angle) - 45.0).abs() < 1e-5);
/// assert_eq!(FloatExt::sqrt(2.25f64), 1.5);
/// ```
pub trait FloatExt: sealed::Sealed + Copy {
    /// Largest integer less than or equal to `self`.
    fn floor(self) -> Self;
    /// Smallest integer greater than or equal to `self`.
    fn ceil(self) -> Self;
    /// Nearest integer, rounding half-way cases away from zero.
    fn round(self) -> Self;
    /// Nearest integer, rounding half-way cases to the even integer.
    fn round_ties_even(self) -> Self;
    /// Integer part of `self` (rounds toward zero).
    fn trunc(self) -> Self;
    /// Fractional part, `self - self.trunc()`.
    fn fract(self) -> Self;
    /// Absolute value.
    fn abs(self) -> Self;
    /// `1.0` or `-1.0` with the sign of `self` (NaN for NaN).
    fn signum(self) -> Self;
    /// `self` with the sign of `sign`.
    fn copysign(self, sign: Self) -> Self;
    /// `self * a + b` (see the trait docs about fusing).
    fn mul_add(self, a: Self, b: Self) -> Self;
    /// Euclidean division (the quotient rounded so that the remainder is non-negative).
    fn div_euclid(self, rhs: Self) -> Self;
    /// Least non-negative remainder of `self (mod rhs)`.
    fn rem_euclid(self, rhs: Self) -> Self;
    /// `self` raised to an integer power.
    fn powi(self, n: i32) -> Self;
    /// `self` raised to a floating-point power.
    fn powf(self, n: Self) -> Self;
    /// Square root (NaN for negative numbers other than `-0.0`).
    fn sqrt(self) -> Self;
    /// `e^self`.
    fn exp(self) -> Self;
    /// `2^self`.
    fn exp2(self) -> Self;
    /// `e^self - 1`, accurate near zero.
    fn exp_m1(self) -> Self;
    /// Natural logarithm.
    fn ln(self) -> Self;
    /// Logarithm with respect to an arbitrary base (`self.ln() / base.ln()`).
    fn log(self, base: Self) -> Self;
    /// Base-2 logarithm.
    fn log2(self) -> Self;
    /// Base-10 logarithm.
    fn log10(self) -> Self;
    /// `ln(1 + self)`, accurate near zero.
    fn ln_1p(self) -> Self;
    /// Cube root.
    fn cbrt(self) -> Self;
    /// Length of the hypotenuse, `sqrt(self² + other²)`, without undue overflow.
    fn hypot(self, other: Self) -> Self;
    /// Sine (radians).
    fn sin(self) -> Self;
    /// Cosine (radians).
    fn cos(self) -> Self;
    /// Tangent (radians).
    fn tan(self) -> Self;
    /// Arcsine in radians, in [-π/2, π/2].
    fn asin(self) -> Self;
    /// Arccosine in radians, in [0, π].
    fn acos(self) -> Self;
    /// Arctangent in radians, in [-π/2, π/2].
    fn atan(self) -> Self;
    /// Four-quadrant arctangent of `self / other` (`self` is y), in [-π, π].
    fn atan2(self, other: Self) -> Self;
    /// `(sin, cos)` with a single argument reduction.
    fn sin_cos(self) -> (Self, Self);
    /// Hyperbolic sine.
    fn sinh(self) -> Self;
    /// Hyperbolic cosine.
    fn cosh(self) -> Self;
    /// Hyperbolic tangent.
    fn tanh(self) -> Self;
    /// Inverse hyperbolic sine.
    fn asinh(self) -> Self;
    /// Inverse hyperbolic cosine.
    fn acosh(self) -> Self;
    /// Inverse hyperbolic tangent.
    fn atanh(self) -> Self;
    /// Minimum of two numbers, ignoring NaN.
    fn min(self, other: Self) -> Self;
    /// Maximum of two numbers, ignoring NaN.
    fn max(self, other: Self) -> Self;
    /// Restricts `self` to `[min, max]` (panics if `min > max` or a bound is NaN).
    fn clamp(self, min: Self, max: Self) -> Self;
    /// Converts radians to degrees.
    fn to_degrees(self) -> Self;
    /// Converts degrees to radians.
    fn to_radians(self) -> Self;
    /// `1 / self`.
    fn recip(self) -> Self;
    /// Linear interpolation `self + (b - self) * t` (not a `std` method).
    fn lerp(self, b: Self, t: Self) -> Self;
}

macro_rules! impl_float_ext {
    ($t:ident) => {
        impl FloatExt for $t {
            #[inline]
            fn floor(self) -> Self {
                crate::$t::floor(self)
            }
            #[inline]
            fn ceil(self) -> Self {
                crate::$t::ceil(self)
            }
            #[inline]
            fn round(self) -> Self {
                crate::$t::round(self)
            }
            #[inline]
            fn round_ties_even(self) -> Self {
                crate::$t::round_ties_even(self)
            }
            #[inline]
            fn trunc(self) -> Self {
                crate::$t::trunc(self)
            }
            #[inline]
            fn fract(self) -> Self {
                crate::$t::fract(self)
            }
            #[inline]
            fn abs(self) -> Self {
                crate::$t::abs(self)
            }
            #[inline]
            fn signum(self) -> Self {
                crate::$t::signum(self)
            }
            #[inline]
            fn copysign(self, sign: Self) -> Self {
                crate::$t::copysign(self, sign)
            }
            #[inline]
            fn mul_add(self, a: Self, b: Self) -> Self {
                crate::$t::mul_add(self, a, b)
            }
            #[inline]
            fn div_euclid(self, rhs: Self) -> Self {
                crate::$t::div_euclid(self, rhs)
            }
            #[inline]
            fn rem_euclid(self, rhs: Self) -> Self {
                crate::$t::rem_euclid(self, rhs)
            }
            #[inline]
            fn powi(self, n: i32) -> Self {
                crate::$t::powi(self, n)
            }
            #[inline]
            fn powf(self, n: Self) -> Self {
                crate::$t::powf(self, n)
            }
            #[inline]
            fn sqrt(self) -> Self {
                crate::$t::sqrt(self)
            }
            #[inline]
            fn exp(self) -> Self {
                crate::$t::exp(self)
            }
            #[inline]
            fn exp2(self) -> Self {
                crate::$t::exp2(self)
            }
            #[inline]
            fn exp_m1(self) -> Self {
                crate::$t::exp_m1(self)
            }
            #[inline]
            fn ln(self) -> Self {
                crate::$t::ln(self)
            }
            #[inline]
            fn log(self, base: Self) -> Self {
                crate::$t::log(self, base)
            }
            #[inline]
            fn log2(self) -> Self {
                crate::$t::log2(self)
            }
            #[inline]
            fn log10(self) -> Self {
                crate::$t::log10(self)
            }
            #[inline]
            fn ln_1p(self) -> Self {
                crate::$t::ln_1p(self)
            }
            #[inline]
            fn cbrt(self) -> Self {
                crate::$t::cbrt(self)
            }
            #[inline]
            fn hypot(self, other: Self) -> Self {
                crate::$t::hypot(self, other)
            }
            #[inline]
            fn sin(self) -> Self {
                crate::$t::sin(self)
            }
            #[inline]
            fn cos(self) -> Self {
                crate::$t::cos(self)
            }
            #[inline]
            fn tan(self) -> Self {
                crate::$t::tan(self)
            }
            #[inline]
            fn asin(self) -> Self {
                crate::$t::asin(self)
            }
            #[inline]
            fn acos(self) -> Self {
                crate::$t::acos(self)
            }
            #[inline]
            fn atan(self) -> Self {
                crate::$t::atan(self)
            }
            #[inline]
            fn atan2(self, other: Self) -> Self {
                crate::$t::atan2(self, other)
            }
            #[inline]
            fn sin_cos(self) -> (Self, Self) {
                crate::$t::sin_cos(self)
            }
            #[inline]
            fn sinh(self) -> Self {
                crate::$t::sinh(self)
            }
            #[inline]
            fn cosh(self) -> Self {
                crate::$t::cosh(self)
            }
            #[inline]
            fn tanh(self) -> Self {
                crate::$t::tanh(self)
            }
            #[inline]
            fn asinh(self) -> Self {
                crate::$t::asinh(self)
            }
            #[inline]
            fn acosh(self) -> Self {
                crate::$t::acosh(self)
            }
            #[inline]
            fn atanh(self) -> Self {
                crate::$t::atanh(self)
            }
            #[inline]
            fn min(self, other: Self) -> Self {
                crate::$t::min(self, other)
            }
            #[inline]
            fn max(self, other: Self) -> Self {
                crate::$t::max(self, other)
            }
            #[inline]
            fn clamp(self, min: Self, max: Self) -> Self {
                crate::$t::clamp(self, min, max)
            }
            #[inline]
            fn to_degrees(self) -> Self {
                crate::$t::to_degrees(self)
            }
            #[inline]
            fn to_radians(self) -> Self {
                crate::$t::to_radians(self)
            }
            #[inline]
            fn recip(self) -> Self {
                crate::$t::recip(self)
            }
            #[inline]
            fn lerp(self, b: Self, t: Self) -> Self {
                crate::$t::lerp(self, b, t)
            }
        }
    };
}

impl_float_ext!(f32);
impl_float_ext!(f64);
