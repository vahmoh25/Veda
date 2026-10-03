//! 2D, 3D and 4D `f32` vectors ([`Vec2`], [`Vec3`], [`Vec4`]) and a small
//! integer vector ([`IVec2`]).
//!
//! Conventions follow `glam`: plain `#[repr(C)]` structs with public fields
//! (`Vec3` is 12 bytes, no SIMD padding), component-wise arithmetic operators
//! (`a + b`, `a * b`, `v * 2.0`, `2.0 * v`, `-v`, and the `*Assign` forms),
//! `v[i]` indexing, and methods such as [`dot`](Vec3::dot),
//! [`cross`](Vec3::cross), [`length`](Vec3::length) and
//! [`normalize`](Vec3::normalize). Vectors are column vectors when multiplied
//! by matrices (`m * v`); the coordinate system is right-handed.
//!
//! `normalize` returns non-finite components for zero-length input (like
//! `glam`); use [`try_normalize`](Vec3::try_normalize) or
//! [`normalize_or_zero`](Vec3::normalize_or_zero) when that can happen.

use core::fmt;
use core::iter::{Product, Sum};
use core::ops::{Add, AddAssign, Div, DivAssign, Index, IndexMut, Mul, MulAssign, Neg, Sub, SubAssign};

use crate::f32 as m;

/// Implements the component-wise operators shared by all float vectors.
macro_rules! impl_vec_ops {
    ($V:ident, $($f:ident),+) => {
        impl Add for $V {
            type Output = Self;
            #[inline]
            fn add(self, rhs: Self) -> Self {
                Self { $($f: self.$f + rhs.$f),+ }
            }
        }
        impl Sub for $V {
            type Output = Self;
            #[inline]
            fn sub(self, rhs: Self) -> Self {
                Self { $($f: self.$f - rhs.$f),+ }
            }
        }
        impl Mul for $V {
            type Output = Self;
            #[inline]
            fn mul(self, rhs: Self) -> Self {
                Self { $($f: self.$f * rhs.$f),+ }
            }
        }
        impl Div for $V {
            type Output = Self;
            #[inline]
            fn div(self, rhs: Self) -> Self {
                Self { $($f: self.$f / rhs.$f),+ }
            }
        }
        impl Add<f32> for $V {
            type Output = Self;
            #[inline]
            fn add(self, rhs: f32) -> Self {
                Self { $($f: self.$f + rhs),+ }
            }
        }
        impl Sub<f32> for $V {
            type Output = Self;
            #[inline]
            fn sub(self, rhs: f32) -> Self {
                Self { $($f: self.$f - rhs),+ }
            }
        }
        impl Mul<f32> for $V {
            type Output = Self;
            #[inline]
            fn mul(self, rhs: f32) -> Self {
                Self { $($f: self.$f * rhs),+ }
            }
        }
        impl Div<f32> for $V {
            type Output = Self;
            #[inline]
            fn div(self, rhs: f32) -> Self {
                Self { $($f: self.$f / rhs),+ }
            }
        }
        impl Add<$V> for f32 {
            type Output = $V;
            #[inline]
            fn add(self, rhs: $V) -> $V {
                $V { $($f: self + rhs.$f),+ }
            }
        }
        impl Sub<$V> for f32 {
            type Output = $V;
            #[inline]
            fn sub(self, rhs: $V) -> $V {
                $V { $($f: self - rhs.$f),+ }
            }
        }
        impl Mul<$V> for f32 {
            type Output = $V;
            #[inline]
            fn mul(self, rhs: $V) -> $V {
                $V { $($f: self * rhs.$f),+ }
            }
        }
        impl Div<$V> for f32 {
            type Output = $V;
            #[inline]
            fn div(self, rhs: $V) -> $V {
                $V { $($f: self / rhs.$f),+ }
            }
        }
        impl Neg for $V {
            type Output = Self;
            #[inline]
            fn neg(self) -> Self {
                Self { $($f: -self.$f),+ }
            }
        }
        impl AddAssign for $V {
            #[inline]
            fn add_assign(&mut self, rhs: Self) {
                $(self.$f += rhs.$f;)+
            }
        }
        impl SubAssign for $V {
            #[inline]
            fn sub_assign(&mut self, rhs: Self) {
                $(self.$f -= rhs.$f;)+
            }
        }
        impl MulAssign for $V {
            #[inline]
            fn mul_assign(&mut self, rhs: Self) {
                $(self.$f *= rhs.$f;)+
            }
        }
        impl DivAssign for $V {
            #[inline]
            fn div_assign(&mut self, rhs: Self) {
                $(self.$f /= rhs.$f;)+
            }
        }
        impl AddAssign<f32> for $V {
            #[inline]
            fn add_assign(&mut self, rhs: f32) {
                $(self.$f += rhs;)+
            }
        }
        impl SubAssign<f32> for $V {
            #[inline]
            fn sub_assign(&mut self, rhs: f32) {
                $(self.$f -= rhs;)+
            }
        }
        impl MulAssign<f32> for $V {
            #[inline]
            fn mul_assign(&mut self, rhs: f32) {
                $(self.$f *= rhs;)+
            }
        }
        impl DivAssign<f32> for $V {
            #[inline]
            fn div_assign(&mut self, rhs: f32) {
                $(self.$f /= rhs;)+
            }
        }
        impl Sum for $V {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, |a, b| a + b)
            }
        }
        impl<'a> Sum<&'a $V> for $V {
            fn sum<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, |a, b| a + *b)
            }
        }
        impl Product for $V {
            fn product<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ONE, |a, b| a * b)
            }
        }
        impl<'a> Product<&'a $V> for $V {
            fn product<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
                iter.fold(Self::ONE, |a, b| a * *b)
            }
        }
    };
}

/// Implements the methods shared by all float vectors.
macro_rules! impl_vec_common {
    ($V:ident, $n:literal, $f0:ident $(, $f:ident)*) => {
        impl $V {
            /// All components zero.
            pub const ZERO: Self = Self::splat(0.0);
            /// All components one.
            pub const ONE: Self = Self::splat(1.0);
            /// All components minus one.
            pub const NEG_ONE: Self = Self::splat(-1.0);
            /// All components `f32::MIN`.
            pub const MIN: Self = Self::splat(f32::MIN);
            /// All components `f32::MAX`.
            pub const MAX: Self = Self::splat(f32::MAX);
            /// All components +∞.
            pub const INFINITY: Self = Self::splat(f32::INFINITY);
            /// All components NaN.
            pub const NAN: Self = Self::splat(f32::NAN);

            /// A vector with every component set to `v`.
            #[inline]
            pub const fn splat(v: f32) -> Self {
                Self { $f0: v $(, $f: v)* }
            }

            /// The components as an array.
            #[inline]
            pub const fn to_array(self) -> [f32; $n] {
                [self.$f0 $(, self.$f)*]
            }

            /// Applies `f` to every component.
            #[inline]
            pub fn map(self, f: impl Fn(f32) -> f32) -> Self {
                Self { $f0: f(self.$f0) $(, $f: f(self.$f))* }
            }

            /// Dot product.
            #[inline]
            pub fn dot(self, rhs: Self) -> f32 {
                self.$f0 * rhs.$f0 $(+ self.$f * rhs.$f)*
            }

            /// Squared length (cheaper than [`length`](Self::length)).
            #[inline]
            pub fn length_squared(self) -> f32 {
                self.dot(self)
            }

            /// Euclidean length.
            #[inline]
            pub fn length(self) -> f32 {
                m::sqrt(self.dot(self))
            }

            /// `1 / length` (∞ for the zero vector).
            #[inline]
            pub fn length_recip(self) -> f32 {
                1.0 / self.length()
            }

            /// Distance between two points.
            #[inline]
            pub fn distance(self, rhs: Self) -> f32 {
                (self - rhs).length()
            }

            /// Squared distance between two points.
            #[inline]
            pub fn distance_squared(self, rhs: Self) -> f32 {
                (self - rhs).length_squared()
            }

            /// The vector scaled to length 1. The result is non-finite for zero
            /// or non-finite input; see [`try_normalize`](Self::try_normalize).
            #[inline]
            pub fn normalize(self) -> Self {
                self * self.length_recip()
            }

            /// The normalized vector, or `None` if the length is zero or not finite.
            #[inline]
            pub fn try_normalize(self) -> Option<Self> {
                let rcp = self.length_recip();
                if rcp.is_finite() && rcp > 0.0 { Some(self * rcp) } else { None }
            }

            /// The normalized vector, or zero if it cannot be normalized.
            #[inline]
            pub fn normalize_or_zero(self) -> Self {
                self.normalize_or(Self::ZERO)
            }

            /// The normalized vector, or `fallback` if it cannot be normalized.
            #[inline]
            pub fn normalize_or(self, fallback: Self) -> Self {
                match self.try_normalize() {
                    Some(v) => v,
                    None => fallback,
                }
            }

            /// Whether the length is 1 within a small tolerance (|len² - 1| <= 2e-4).
            #[inline]
            pub fn is_normalized(self) -> bool {
                m::abs(self.length_squared() - 1.0) <= 2e-4
            }

            /// Linear interpolation `self + (rhs - self) * t`.
            #[inline]
            pub fn lerp(self, rhs: Self, t: f32) -> Self {
                self + (rhs - self) * t
            }

            /// `self * a + b`, component-wise.
            #[inline]
            pub fn mul_add(self, a: Self, b: Self) -> Self {
                Self { $f0: self.$f0 * a.$f0 + b.$f0 $(, $f: self.$f * a.$f + b.$f)* }
            }

            /// Component-wise minimum (NaN components are ignored, like `f32::min`).
            #[inline]
            pub fn min(self, rhs: Self) -> Self {
                Self { $f0: m::min(self.$f0, rhs.$f0) $(, $f: m::min(self.$f, rhs.$f))* }
            }

            /// Component-wise maximum (NaN components are ignored, like `f32::max`).
            #[inline]
            pub fn max(self, rhs: Self) -> Self {
                Self { $f0: m::max(self.$f0, rhs.$f0) $(, $f: m::max(self.$f, rhs.$f))* }
            }

            /// Component-wise clamp, `self.max(min).min(max)` (never panics).
            #[inline]
            pub fn clamp(self, min: Self, max: Self) -> Self {
                self.max(min).min(max)
            }

            /// Component-wise absolute value.
            #[inline]
            pub fn abs(self) -> Self {
                Self { $f0: m::abs(self.$f0) $(, $f: m::abs(self.$f))* }
            }

            /// Component-wise `floor`.
            #[inline]
            pub fn floor(self) -> Self {
                Self { $f0: m::floor(self.$f0) $(, $f: m::floor(self.$f))* }
            }

            /// Component-wise `ceil`.
            #[inline]
            pub fn ceil(self) -> Self {
                Self { $f0: m::ceil(self.$f0) $(, $f: m::ceil(self.$f))* }
            }

            /// Component-wise `round` (ties away from zero).
            #[inline]
            pub fn round(self) -> Self {
                Self { $f0: m::round(self.$f0) $(, $f: m::round(self.$f))* }
            }

            /// Component-wise `trunc`.
            #[inline]
            pub fn trunc(self) -> Self {
                Self { $f0: m::trunc(self.$f0) $(, $f: m::trunc(self.$f))* }
            }

            /// Component-wise `fract` (`x - trunc(x)`).
            #[inline]
            pub fn fract(self) -> Self {
                Self { $f0: m::fract(self.$f0) $(, $f: m::fract(self.$f))* }
            }

            /// Component-wise `signum`.
            #[inline]
            pub fn signum(self) -> Self {
                Self { $f0: m::signum(self.$f0) $(, $f: m::signum(self.$f))* }
            }

            /// Component-wise `1 / x`.
            #[inline]
            pub fn recip(self) -> Self {
                Self { $f0: 1.0 / self.$f0 $(, $f: 1.0 / self.$f)* }
            }

            /// The smallest component.
            #[inline]
            pub fn min_element(self) -> f32 {
                let v = self.$f0;
                $(let v = m::min(v, self.$f);)*
                v
            }

            /// The largest component.
            #[inline]
            pub fn max_element(self) -> f32 {
                let v = self.$f0;
                $(let v = m::max(v, self.$f);)*
                v
            }

            /// Sum of the components.
            #[inline]
            pub fn element_sum(self) -> f32 {
                self.$f0 $(+ self.$f)*
            }

            /// Product of the components.
            #[inline]
            pub fn element_product(self) -> f32 {
                self.$f0 $(* self.$f)*
            }

            /// Whether every component is finite.
            #[inline]
            pub fn is_finite(self) -> bool {
                self.$f0.is_finite() $(&& self.$f.is_finite())*
            }

            /// Whether any component is NaN.
            #[inline]
            pub fn is_nan(self) -> bool {
                self.$f0.is_nan() $(|| self.$f.is_nan())*
            }

            /// Whether every component differs from `rhs` by at most `max_abs_diff`.
            #[inline]
            pub fn abs_diff_eq(self, rhs: Self, max_abs_diff: f32) -> bool {
                m::abs(self.$f0 - rhs.$f0) <= max_abs_diff $(&& m::abs(self.$f - rhs.$f) <= max_abs_diff)*
            }

            /// The vector with its length clamped to `[min, max]` (zero stays zero).
            #[inline]
            pub fn clamp_length(self, min: f32, max: f32) -> Self {
                let len_sq = self.length_squared();
                if len_sq < min * min {
                    self * (min / m::sqrt(len_sq))
                } else if len_sq > max * max {
                    self * (max / m::sqrt(len_sq))
                } else {
                    self
                }
            }

            /// The vector with its length clamped to at most `max`.
            #[inline]
            pub fn clamp_length_max(self, max: f32) -> Self {
                let len_sq = self.length_squared();
                if len_sq > max * max { self * (max / m::sqrt(len_sq)) } else { self }
            }

            /// The projection of `self` onto `rhs` (`rhs` must be non-zero).
            #[inline]
            pub fn project_onto(self, rhs: Self) -> Self {
                rhs * (self.dot(rhs) / rhs.dot(rhs))
            }

            /// The component of `self` perpendicular to `rhs` (`rhs` must be non-zero).
            #[inline]
            pub fn reject_from(self, rhs: Self) -> Self {
                self - self.project_onto(rhs)
            }

            /// Reflects `self` off a surface with the given unit `normal`.
            #[inline]
            pub fn reflect(self, normal: Self) -> Self {
                self - normal * (2.0 * self.dot(normal))
            }

            /// Refracts the unit vector `self` through a surface with the unit
            /// `normal` and ratio of refraction indices `eta`; zero on total
            /// internal reflection.
            #[inline]
            pub fn refract(self, normal: Self, eta: f32) -> Self {
                let n_dot_i = normal.dot(self);
                let k = 1.0 - eta * eta * (1.0 - n_dot_i * n_dot_i);
                if k >= 0.0 { self * eta - normal * (eta * n_dot_i + m::sqrt(k)) } else { Self::ZERO }
            }

            /// Moves from `self` toward `target` by at most `max_delta` (never overshoots).
            #[inline]
            pub fn move_towards(self, target: Self, max_delta: f32) -> Self {
                let delta = target - self;
                let dist = delta.length();
                if dist <= max_delta || dist <= 1e-12 { target } else { self + delta * (max_delta / dist) }
            }

            /// The point halfway between `self` and `rhs`.
            #[inline]
            pub fn midpoint(self, rhs: Self) -> Self {
                (self + rhs) * 0.5
            }
        }

        impl From<[f32; $n]> for $V {
            #[inline]
            fn from(a: [f32; $n]) -> Self {
                Self::from_array(a)
            }
        }

        impl From<$V> for [f32; $n] {
            #[inline]
            fn from(v: $V) -> Self {
                v.to_array()
            }
        }

        impl fmt::Display for $V {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("[")?;
                fmt::Display::fmt(&self.$f0, f)?;
                $(
                    f.write_str(", ")?;
                    fmt::Display::fmt(&self.$f, f)?;
                )*
                f.write_str("]")
            }
        }

        impl_vec_ops!($V, $f0 $(, $f)*);
    };
}

// ---------------------------------------------------------------------------
// Vec2
// ---------------------------------------------------------------------------

/// A 2D vector of `f32`.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
#[repr(C)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl_vec_common!(Vec2, 2, x, y);

impl Vec2 {
    /// The unit X axis.
    pub const X: Self = Self::new(1.0, 0.0);
    /// The unit Y axis.
    pub const Y: Self = Self::new(0.0, 1.0);
    /// The negative X axis.
    pub const NEG_X: Self = Self::new(-1.0, 0.0);
    /// The negative Y axis.
    pub const NEG_Y: Self = Self::new(0.0, -1.0);

    /// Creates a vector.
    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Creates a vector from an array.
    #[inline]
    pub const fn from_array(a: [f32; 2]) -> Self {
        Self::new(a[0], a[1])
    }

    /// A 3D vector `(x, y, z)`.
    #[inline]
    pub const fn extend(self, z: f32) -> Vec3 {
        Vec3::new(self.x, self.y, z)
    }

    /// The swizzle `(y, x)`.
    #[inline]
    pub const fn yx(self) -> Self {
        Self::new(self.y, self.x)
    }

    /// The vector rotated by +90° (counter-clockwise): `(-y, x)`.
    #[inline]
    pub const fn perp(self) -> Self {
        Self::new(-self.y, self.x)
    }

    /// The 2D cross product `self.x * rhs.y - self.y * rhs.x` (positive when
    /// `rhs` is counter-clockwise from `self`).
    #[inline]
    pub fn perp_dot(self, rhs: Self) -> f32 {
        self.x * rhs.y - self.y * rhs.x
    }

    /// The unit vector at `angle` radians from the X axis: `(cos, sin)`.
    #[inline]
    pub fn from_angle(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle);
        Self::new(c, s)
    }

    /// The angle of this vector from the X axis, in (-π, π].
    #[inline]
    pub fn to_angle(self) -> f32 {
        m::atan2(self.y, self.x)
    }

    /// The signed angle from `self` to `rhs` in [-π, π] (positive = counter-clockwise).
    #[inline]
    pub fn angle_to(self, rhs: Self) -> f32 {
        m::atan2(self.perp_dot(rhs), self.dot(rhs))
    }

    /// Rotates `self` by the rotation that `rhs` represents (complex
    /// multiplication); with `rhs = Vec2::from_angle(a)` this rotates by `a`.
    #[inline]
    pub fn rotate(self, rhs: Self) -> Self {
        Self::new(self.x * rhs.x - self.y * rhs.y, self.y * rhs.x + self.x * rhs.y)
    }

    /// The vector rotated by `angle` radians (counter-clockwise).
    #[inline]
    pub fn rotated(self, angle: f32) -> Self {
        self.rotate(Self::from_angle(angle))
    }

    /// Converts to an integer vector, truncating toward zero (saturating, NaN -> 0).
    #[inline]
    pub const fn as_ivec2(self) -> IVec2 {
        IVec2::new(self.x as i32, self.y as i32)
    }
}

impl From<(f32, f32)> for Vec2 {
    #[inline]
    fn from((x, y): (f32, f32)) -> Self {
        Self::new(x, y)
    }
}

impl Index<usize> for Vec2 {
    type Output = f32;
    #[inline]
    fn index(&self, i: usize) -> &f32 {
        match i {
            0 => &self.x,
            1 => &self.y,
            _ => panic!("Vec2 index out of bounds"),
        }
    }
}

impl IndexMut<usize> for Vec2 {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut f32 {
        match i {
            0 => &mut self.x,
            1 => &mut self.y,
            _ => panic!("Vec2 index out of bounds"),
        }
    }
}

// ---------------------------------------------------------------------------
// Vec3
// ---------------------------------------------------------------------------

/// A 3D vector of `f32` (12 bytes, `#[repr(C)]`).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
#[repr(C)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl_vec_common!(Vec3, 3, x, y, z);

impl Vec3 {
    /// The unit X axis.
    pub const X: Self = Self::new(1.0, 0.0, 0.0);
    /// The unit Y axis.
    pub const Y: Self = Self::new(0.0, 1.0, 0.0);
    /// The unit Z axis.
    pub const Z: Self = Self::new(0.0, 0.0, 1.0);
    /// The negative X axis.
    pub const NEG_X: Self = Self::new(-1.0, 0.0, 0.0);
    /// The negative Y axis.
    pub const NEG_Y: Self = Self::new(0.0, -1.0, 0.0);
    /// The negative Z axis.
    pub const NEG_Z: Self = Self::new(0.0, 0.0, -1.0);

    /// Creates a vector.
    #[inline]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// Creates a vector from an array.
    #[inline]
    pub const fn from_array(a: [f32; 3]) -> Self {
        Self::new(a[0], a[1], a[2])
    }

    /// A 4D vector `(x, y, z, w)` (use `w = 1` for points, `w = 0` for directions).
    #[inline]
    pub const fn extend(self, w: f32) -> Vec4 {
        Vec4::new(self.x, self.y, self.z, w)
    }

    /// The `(x, y)` part.
    #[inline]
    pub const fn truncate(self) -> Vec2 {
        Vec2::new(self.x, self.y)
    }

    /// The swizzle `(x, y)`.
    #[inline]
    pub const fn xy(self) -> Vec2 {
        Vec2::new(self.x, self.y)
    }

    /// The swizzle `(x, z)` (e.g. the ground plane in a Y-up world).
    #[inline]
    pub const fn xz(self) -> Vec2 {
        Vec2::new(self.x, self.z)
    }

    /// The swizzle `(y, z)`.
    #[inline]
    pub const fn yz(self) -> Vec2 {
        Vec2::new(self.y, self.z)
    }

    /// The swizzle `(z, y, x)`.
    #[inline]
    pub const fn zyx(self) -> Self {
        Self::new(self.z, self.y, self.x)
    }

    /// Cross product (right-handed: `X.cross(Y) == Z`).
    #[inline]
    pub fn cross(self, rhs: Self) -> Self {
        Self::new(self.y * rhs.z - self.z * rhs.y, self.z * rhs.x - self.x * rhs.z, self.x * rhs.y - self.y * rhs.x)
    }

    /// The unsigned angle between two (not necessarily normalized) vectors, in [0, π].
    #[inline]
    pub fn angle_between(self, rhs: Self) -> f32 {
        m::atan2(self.cross(rhs).length(), self.dot(rhs))
    }

    /// Some vector orthogonal to `self` (not normalized; `self` must be non-zero).
    #[inline]
    pub fn any_orthogonal_vector(self) -> Self {
        if m::abs(self.x) > m::abs(self.y) {
            Self::new(-self.z, 0.0, self.x)
        } else {
            Self::new(0.0, self.z, -self.y)
        }
    }

    /// Some unit vector orthogonal to the unit vector `self`.
    #[inline]
    pub fn any_orthonormal_vector(self) -> Self {
        self.any_orthonormal_pair().1
    }

    /// Two unit vectors that together with the unit vector `self` form an
    /// orthonormal basis (Duff et al., "Building an Orthonormal Basis, Revisited").
    #[inline]
    pub fn any_orthonormal_pair(self) -> (Self, Self) {
        let sign = m::copysign(1.0, self.z);
        let a = -1.0 / (sign + self.z);
        let b = self.x * self.y * a;
        (
            Self::new(1.0 + sign * self.x * self.x * a, sign * b, -sign * self.x),
            Self::new(b, sign + self.y * self.y * a, -self.y),
        )
    }
}

impl From<(f32, f32, f32)> for Vec3 {
    #[inline]
    fn from((x, y, z): (f32, f32, f32)) -> Self {
        Self::new(x, y, z)
    }
}

impl Index<usize> for Vec3 {
    type Output = f32;
    #[inline]
    fn index(&self, i: usize) -> &f32 {
        match i {
            0 => &self.x,
            1 => &self.y,
            2 => &self.z,
            _ => panic!("Vec3 index out of bounds"),
        }
    }
}

impl IndexMut<usize> for Vec3 {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut f32 {
        match i {
            0 => &mut self.x,
            1 => &mut self.y,
            2 => &mut self.z,
            _ => panic!("Vec3 index out of bounds"),
        }
    }
}

// ---------------------------------------------------------------------------
// Vec4
// ---------------------------------------------------------------------------

/// A 4D vector of `f32` (homogeneous coordinates, RGBA colors, ...).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
#[repr(C)]
pub struct Vec4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl_vec_common!(Vec4, 4, x, y, z, w);

impl Vec4 {
    /// The unit X axis.
    pub const X: Self = Self::new(1.0, 0.0, 0.0, 0.0);
    /// The unit Y axis.
    pub const Y: Self = Self::new(0.0, 1.0, 0.0, 0.0);
    /// The unit Z axis.
    pub const Z: Self = Self::new(0.0, 0.0, 1.0, 0.0);
    /// The unit W axis.
    pub const W: Self = Self::new(0.0, 0.0, 0.0, 1.0);
    /// The negative X axis.
    pub const NEG_X: Self = Self::new(-1.0, 0.0, 0.0, 0.0);
    /// The negative Y axis.
    pub const NEG_Y: Self = Self::new(0.0, -1.0, 0.0, 0.0);
    /// The negative Z axis.
    pub const NEG_Z: Self = Self::new(0.0, 0.0, -1.0, 0.0);
    /// The negative W axis.
    pub const NEG_W: Self = Self::new(0.0, 0.0, 0.0, -1.0);

    /// Creates a vector.
    #[inline]
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// Creates a vector from an array.
    #[inline]
    pub const fn from_array(a: [f32; 4]) -> Self {
        Self::new(a[0], a[1], a[2], a[3])
    }

    /// The `(x, y, z)` part.
    #[inline]
    pub const fn truncate(self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }

    /// The swizzle `(x, y, z)`.
    #[inline]
    pub const fn xyz(self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }

    /// The swizzle `(x, y)`.
    #[inline]
    pub const fn xy(self) -> Vec2 {
        Vec2::new(self.x, self.y)
    }

    /// The perspective divide `(x, y, z) / w`.
    #[inline]
    pub fn project(self) -> Vec3 {
        self.xyz() / self.w
    }
}

impl From<(f32, f32, f32, f32)> for Vec4 {
    #[inline]
    fn from((x, y, z, w): (f32, f32, f32, f32)) -> Self {
        Self::new(x, y, z, w)
    }
}

impl Index<usize> for Vec4 {
    type Output = f32;
    #[inline]
    fn index(&self, i: usize) -> &f32 {
        match i {
            0 => &self.x,
            1 => &self.y,
            2 => &self.z,
            3 => &self.w,
            _ => panic!("Vec4 index out of bounds"),
        }
    }
}

impl IndexMut<usize> for Vec4 {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut f32 {
        match i {
            0 => &mut self.x,
            1 => &mut self.y,
            2 => &mut self.z,
            3 => &mut self.w,
            _ => panic!("Vec4 index out of bounds"),
        }
    }
}

// ---------------------------------------------------------------------------
// IVec2
// ---------------------------------------------------------------------------

/// A 2D vector of `i32` (pixel coordinates, grid cells, tile positions).
///
/// Arithmetic uses the normal `i32` operators, so overflow panics in debug
/// builds and wraps in release builds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(C)]
pub struct IVec2 {
    pub x: i32,
    pub y: i32,
}

impl IVec2 {
    /// `(0, 0)`.
    pub const ZERO: Self = Self::new(0, 0);
    /// `(1, 1)`.
    pub const ONE: Self = Self::new(1, 1);
    /// `(1, 0)`.
    pub const X: Self = Self::new(1, 0);
    /// `(0, 1)`.
    pub const Y: Self = Self::new(0, 1);
    /// `(-1, 0)`.
    pub const NEG_X: Self = Self::new(-1, 0);
    /// `(0, -1)`.
    pub const NEG_Y: Self = Self::new(0, -1);

    /// Creates a vector.
    #[inline]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// A vector with both components set to `v`.
    #[inline]
    pub const fn splat(v: i32) -> Self {
        Self { x: v, y: v }
    }

    /// The components as an array.
    #[inline]
    pub const fn to_array(self) -> [i32; 2] {
        [self.x, self.y]
    }

    /// Converts to a float vector.
    #[inline]
    pub const fn as_vec2(self) -> Vec2 {
        Vec2::new(self.x as f32, self.y as f32)
    }

    /// Dot product.
    #[inline]
    pub const fn dot(self, rhs: Self) -> i32 {
        self.x * rhs.x + self.y * rhs.y
    }

    /// Squared length.
    #[inline]
    pub const fn length_squared(self) -> i32 {
        self.dot(self)
    }

    /// Component-wise minimum.
    #[inline]
    pub const fn min(self, rhs: Self) -> Self {
        Self::new(if self.x < rhs.x { self.x } else { rhs.x }, if self.y < rhs.y { self.y } else { rhs.y })
    }

    /// Component-wise maximum.
    #[inline]
    pub const fn max(self, rhs: Self) -> Self {
        Self::new(if self.x > rhs.x { self.x } else { rhs.x }, if self.y > rhs.y { self.y } else { rhs.y })
    }

    /// Component-wise clamp, `self.max(min).min(max)`.
    #[inline]
    pub const fn clamp(self, min: Self, max: Self) -> Self {
        self.max(min).min(max)
    }

    /// Component-wise absolute value.
    #[inline]
    pub const fn abs(self) -> Self {
        Self::new(self.x.abs(), self.y.abs())
    }

    /// Manhattan length `|x| + |y|`.
    #[inline]
    pub const fn manhattan_length(self) -> i32 {
        self.x.abs() + self.y.abs()
    }

    /// The vector rotated by +90°: `(-y, x)`.
    #[inline]
    pub const fn perp(self) -> Self {
        Self::new(-self.y, self.x)
    }
}

impl Add for IVec2 {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for IVec2 {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Mul for IVec2 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        Self::new(self.x * rhs.x, self.y * rhs.y)
    }
}

impl Mul<i32> for IVec2 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: i32) -> Self {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

impl Mul<IVec2> for i32 {
    type Output = IVec2;
    #[inline]
    fn mul(self, rhs: IVec2) -> IVec2 {
        IVec2::new(self * rhs.x, self * rhs.y)
    }
}

impl Neg for IVec2 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y)
    }
}

impl AddAssign for IVec2 {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl SubAssign for IVec2 {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl MulAssign<i32> for IVec2 {
    #[inline]
    fn mul_assign(&mut self, rhs: i32) {
        *self = *self * rhs;
    }
}

impl Index<usize> for IVec2 {
    type Output = i32;
    #[inline]
    fn index(&self, i: usize) -> &i32 {
        match i {
            0 => &self.x,
            1 => &self.y,
            _ => panic!("IVec2 index out of bounds"),
        }
    }
}

impl IndexMut<usize> for IVec2 {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut i32 {
        match i {
            0 => &mut self.x,
            1 => &mut self.y,
            _ => panic!("IVec2 index out of bounds"),
        }
    }
}

impl From<[i32; 2]> for IVec2 {
    #[inline]
    fn from(a: [i32; 2]) -> Self {
        Self::new(a[0], a[1])
    }
}

impl From<(i32, i32)> for IVec2 {
    #[inline]
    fn from((x, y): (i32, i32)) -> Self {
        Self::new(x, y)
    }
}

impl fmt::Display for IVec2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {}]", self.x, self.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;

    fn close(a: Vec3, b: Vec3) -> bool {
        a.abs_diff_eq(b, 1e-5)
    }

    #[test]
    fn layout() {
        assert_eq!(core::mem::size_of::<Vec2>(), 8);
        assert_eq!(core::mem::size_of::<Vec3>(), 12);
        assert_eq!(core::mem::size_of::<Vec4>(), 16);
        assert_eq!(core::mem::size_of::<IVec2>(), 8);
        assert_eq!(core::mem::align_of::<Vec4>(), 4);
    }

    #[test]
    fn operators() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        assert_eq!(a + b, Vec3::new(5.0, 7.0, 9.0));
        assert_eq!(b - a, Vec3::splat(3.0));
        assert_eq!(a * b, Vec3::new(4.0, 10.0, 18.0));
        assert_eq!(b / a, Vec3::new(4.0, 2.5, 2.0));
        assert_eq!(a * 2.0, Vec3::new(2.0, 4.0, 6.0));
        assert_eq!(2.0 * a, a * 2.0);
        assert_eq!(a / 2.0, Vec3::new(0.5, 1.0, 1.5));
        assert_eq!(6.0 / a, Vec3::new(6.0, 3.0, 2.0));
        assert_eq!(a + 1.0, Vec3::new(2.0, 3.0, 4.0));
        assert_eq!(1.0 - a, Vec3::new(0.0, -1.0, -2.0));
        assert_eq!(-a, Vec3::new(-1.0, -2.0, -3.0));
        let mut c = a;
        c += b;
        c -= a;
        assert_eq!(c, b);
        c *= 2.0;
        c /= 2.0;
        c *= a;
        c /= a;
        assert_eq!(c, b);
        assert_eq!(a[0], 1.0);
        assert_eq!(a[2], 3.0);
        c[1] = 9.0;
        assert_eq!(c.y, 9.0);
        let v4 = Vec4::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(v4[3], 4.0);
        assert_eq!([a, b].iter().sum::<Vec3>(), Vec3::new(5.0, 7.0, 9.0));
        assert_eq!([a, b].into_iter().product::<Vec3>(), a * b);
        assert_eq!(Vec3::from([1.0, 2.0, 3.0]), a);
        assert_eq!(<[f32; 3]>::from(a), [1.0, 2.0, 3.0]);
        assert_eq!(Vec2::from((1.0, 2.0)), Vec2::new(1.0, 2.0));
        assert_eq!(format!("{a}"), "[1, 2, 3]");
        assert_eq!(Vec2::default(), Vec2::ZERO);
    }

    #[test]
    #[should_panic]
    fn index_out_of_bounds() {
        let _ = Vec3::ZERO[3];
    }

    #[test]
    fn products_and_lengths() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        assert_eq!(a.dot(b), 32.0);
        assert_eq!(Vec3::X.cross(Vec3::Y), Vec3::Z);
        assert_eq!(Vec3::Y.cross(Vec3::Z), Vec3::X);
        assert_eq!(Vec3::Z.cross(Vec3::X), Vec3::Y);
        let c = a.cross(b);
        assert_eq!(c.dot(a), 0.0);
        assert_eq!(c.dot(b), 0.0);
        assert_eq!(Vec3::new(2.0, 3.0, 6.0).length(), 7.0);
        assert_eq!(Vec2::new(3.0, 4.0).length_squared(), 25.0);
        assert_eq!(Vec2::new(3.0, 4.0).distance(Vec2::ZERO), 5.0);
        let n = a.normalize();
        assert!(n.is_normalized());
        assert!(close(n * a.length(), a));
        assert_eq!(Vec3::ZERO.try_normalize(), None);
        assert_eq!(Vec3::ZERO.normalize_or_zero(), Vec3::ZERO);
        assert_eq!(Vec3::INFINITY.try_normalize(), None);
        assert!(!Vec3::ZERO.normalize().is_finite());
        assert_eq!(Vec3::new(0.0, 0.0, 5.0).normalize_or(Vec3::X), Vec3::Z);
        assert_eq!(Vec3::ZERO.normalize_or(Vec3::X), Vec3::X);
    }

    #[test]
    fn component_wise() {
        let a = Vec3::new(-1.5, 2.5, 0.25);
        let b = Vec3::new(1.0, -2.0, 3.0);
        assert_eq!(a.min(b), Vec3::new(-1.5, -2.0, 0.25));
        assert_eq!(a.max(b), Vec3::new(1.0, 2.5, 3.0));
        assert_eq!(a.abs(), Vec3::new(1.5, 2.5, 0.25));
        assert_eq!(a.floor(), Vec3::new(-2.0, 2.0, 0.0));
        assert_eq!(a.ceil(), Vec3::new(-1.0, 3.0, 1.0));
        assert_eq!(a.round(), Vec3::new(-2.0, 3.0, 0.0));
        assert_eq!(a.trunc(), Vec3::new(-1.0, 2.0, 0.0));
        assert_eq!(a.fract(), Vec3::new(-0.5, 0.5, 0.25));
        assert_eq!(a.signum(), Vec3::new(-1.0, 1.0, 1.0));
        assert_eq!(a.clamp(Vec3::splat(-1.0), Vec3::splat(1.0)), Vec3::new(-1.0, 1.0, 0.25));
        assert_eq!(a.min_element(), -1.5);
        assert_eq!(a.max_element(), 2.5);
        assert_eq!(b.element_sum(), 2.0);
        assert_eq!(b.element_product(), -6.0);
        assert_eq!(Vec2::new(2.0, 4.0).recip(), Vec2::new(0.5, 0.25));
        assert_eq!(a.map(|v| v * 2.0), a * 2.0);
        assert!(Vec3::new(1.0, f32::NAN, 0.0).is_nan());
        assert!(!Vec3::new(1.0, f32::INFINITY, 0.0).is_finite());
        assert_eq!(Vec3::ZERO.lerp(Vec3::splat(2.0), 0.25), Vec3::splat(0.5));
        assert_eq!(a.mul_add(Vec3::splat(2.0), Vec3::ONE), a * 2.0 + Vec3::ONE);
        assert_eq!(Vec3::ZERO.midpoint(Vec3::splat(2.0)), Vec3::ONE);
    }

    #[test]
    fn geometry_helpers() {
        let v = Vec3::new(1.0, -1.0, 0.0);
        assert_eq!(v.reflect(Vec3::Y), Vec3::new(1.0, 1.0, 0.0));
        assert!(close(v.project_onto(Vec3::X * 3.0), Vec3::X));
        assert!(close(v.reject_from(Vec3::X), Vec3::NEG_Y));
        // Refraction with eta = 1 passes straight through.
        let d = Vec3::new(1.0, -1.0, 0.0).normalize();
        assert!(close(d.refract(Vec3::Y, 1.0), d));
        // Total internal reflection.
        assert_eq!(Vec3::new(1.0, -0.01, 0.0).normalize().refract(Vec3::Y, 1.5), Vec3::ZERO);
        assert_eq!(Vec3::ZERO.move_towards(Vec3::X * 10.0, 3.0), Vec3::X * 3.0);
        assert_eq!(Vec3::ZERO.move_towards(Vec3::X, 3.0), Vec3::X);
        assert!(close(Vec3::X.clamp_length_max(0.5), Vec3::X * 0.5));
        assert!(close((Vec3::X * 0.1).clamp_length(1.0, 2.0), Vec3::X));
        assert!((Vec3::X.angle_between(Vec3::Y) - core::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((Vec3::X.angle_between(-Vec3::X) - core::f32::consts::PI).abs() < 1e-6);
        assert_eq!(Vec3::X.angle_between(Vec3::X * 5.0), 0.0);

        let mut rng = crate::Rng::new(3);
        for _ in 0..1000 {
            let n = rng.unit_vec3();
            let (a, b) = n.any_orthonormal_pair();
            assert!(a.is_normalized() && b.is_normalized());
            assert!(a.dot(n).abs() < 1e-5 && b.dot(n).abs() < 1e-5 && a.dot(b).abs() < 1e-5);
            assert!(close(a.cross(b), n));
            assert!(n.any_orthogonal_vector().dot(n).abs() < 1e-5);
            assert!(n.any_orthonormal_vector().is_normalized());
        }
    }

    #[test]
    fn vec2_angles() {
        use core::f32::consts::{FRAC_PI_2, PI};
        assert!(Vec2::from_angle(FRAC_PI_2).abs_diff_eq(Vec2::Y, 1e-6));
        assert!((Vec2::new(-1.0, 0.0).to_angle() - PI).abs() < 1e-6);
        assert!((Vec2::X.angle_to(Vec2::Y) - FRAC_PI_2).abs() < 1e-6);
        assert!((Vec2::Y.angle_to(Vec2::X) + FRAC_PI_2).abs() < 1e-6);
        assert_eq!(Vec2::X.perp(), Vec2::Y);
        assert_eq!(Vec2::X.perp_dot(Vec2::Y), 1.0);
        assert!(Vec2::X.rotated(FRAC_PI_2).abs_diff_eq(Vec2::Y, 1e-6));
        assert!(Vec2::new(1.0, 1.0).rotate(Vec2::from_angle(PI)).abs_diff_eq(Vec2::new(-1.0, -1.0), 1e-6));
        assert_eq!(Vec2::new(1.0, 2.0).yx(), Vec2::new(2.0, 1.0));
    }

    #[test]
    fn swizzles() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(v.xy(), Vec2::new(1.0, 2.0));
        assert_eq!(v.xz(), Vec2::new(1.0, 3.0));
        assert_eq!(v.yz(), Vec2::new(2.0, 3.0));
        assert_eq!(v.zyx(), Vec3::new(3.0, 2.0, 1.0));
        assert_eq!(v.truncate(), v.xy());
        assert_eq!(v.extend(4.0), Vec4::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(v.extend(4.0).truncate(), v);
        assert_eq!(v.extend(4.0).xyz(), v);
        assert_eq!(v.extend(2.0).project(), v / 2.0);
        assert_eq!(Vec2::new(1.0, 2.0).extend(3.0), v);
    }

    #[test]
    fn ivec2() {
        let a = IVec2::new(3, -4);
        assert_eq!(a + IVec2::ONE, IVec2::new(4, -3));
        assert_eq!(a - IVec2::ONE, IVec2::new(2, -5));
        assert_eq!(a * 2, IVec2::new(6, -8));
        assert_eq!(2 * a, a * 2);
        assert_eq!(-a, IVec2::new(-3, 4));
        assert_eq!(a.abs(), IVec2::new(3, 4));
        assert_eq!(a.manhattan_length(), 7);
        assert_eq!(a.length_squared(), 25);
        assert_eq!(a.min(IVec2::ZERO), IVec2::new(0, -4));
        assert_eq!(a.max(IVec2::ZERO), IVec2::new(3, 0));
        assert_eq!(a.as_vec2(), Vec2::new(3.0, -4.0));
        assert_eq!(Vec2::new(2.9, -2.9).as_ivec2(), IVec2::new(2, -2));
        assert_eq!(a[1], -4);
        assert_eq!(format!("{a}"), "[3, -4]");
    }
}
