//! Vindows math: a `no_std` libm replacement, linear algebra, noise and random
//! numbers for graphics and games.
//!
//! `core` on stable Rust has no `sqrt`, `sin`, `floor`, `powf`, ... This crate
//! provides them (accurately and fast, without any C runtime), plus the
//! vector/matrix/quaternion types, scalar helpers, a PRNG, noise and color
//! utilities that a software renderer and games need. It needs neither `std`
//! nor `alloc` and builds for soft-float targets such as `x86_64-unknown-uefi`.
//!
//! # Modules
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`f32`](mod@f32), [`f64`](mod@f64) | free functions `sqrt`, `sin`, `powf`, ... named like the `std` methods |
//! | [`FloatExt`] | the same as methods: `use vmath::FloatExt;` makes `x.sin()` work in `no_std` |
//! | [`scalar`] | `lerp`, `inverse_lerp`, `remap`, `smoothstep`, `wrap_angle`, `damp`, ... and `f32` constants (`PI`, `TAU`, ...) |
//! | [`vector`] | [`Vec2`], [`Vec3`], [`Vec4`], [`IVec2`] |
//! | [`matrix`] | [`Mat3`], [`Mat4`] (column-major, column vectors, right-handed) |
//! | [`quat`] | [`Quat`] rotations |
//! | [`rng`] | [`Rng`], a seedable PCG32 generator |
//! | [`noise`] | [`Noise`]: Perlin and simplex noise, fBm, ridged noise |
//! | [`color`] | sRGB transfer functions and tables, HSV, packed `0xAARRGGBB` helpers |
//!
//! The most common items are re-exported at the crate root (`vmath::Vec3`,
//! `vmath::lerp`, `vmath::PI`, ...).
//!
//! # Floating-point functions
//!
//! ```
//! use vmath::FloatExt; // in a no_std crate this provides x.sqrt(), x.sin(), ...
//!
//! let (s, c) = FloatExt::sin_cos(0.5f32);
//! assert!((s * s + c * c - 1.0).abs() < 1e-6);
//! assert_eq!(vmath::f64::floor(-2.5), -3.0); // free-function form
//! ```
//!
//! [`FloatExt`] has every float method `std` has that `core` lacks, with the
//! same names, signatures and special-case behaviour (NaN, ±∞, ±0,
//! subnormals), plus `lerp`. Inherent methods win over trait methods, so where
//! `std` is linked (host-side tests) `x.sin()` calls `std`; use
//! `FloatExt::sin(x)` or `vmath::f32::sin(x)` to call this crate explicitly.
//!
//! Accuracy, measured on random and special inputs (`f32`: against the
//! correctly rounded result, and exhaustively over all 2^32 inputs for the
//! one-argument functions; `f64`: against the host C runtime, which itself is
//! not always correctly rounded):
//!
//! | Function | `f32` max error | `f64` max error |
//! |----------|-----------------|-----------------|
//! | `sqrt`, `floor`, `ceil`, `round`, `round_ties_even`, `trunc`, `fract`, `abs`, `copysign`, `signum`, `fmod`/`%`, `rem_euclid`, `div_euclid` | exact | exact |
//! | `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2` | 1 ulp | 1 ulp |
//! | `exp`, `exp2`, `ln`, `log2`, `log10`, `ln_1p`, `hypot`, `powf` | 1 ulp | 1 ulp |
//! | `exp_m1`, `cbrt` | correctly rounded | 1 ulp (2 vs. the CRT, whose result was the wrong one) |
//! | `sinh`, `cosh`, `tanh` | correctly rounded | 2 ulp |
//! | `asinh`, `acosh`, `atanh` | 1 ulp (0 observed) | 2 ulp |
//! | `powi` | 1 ulp (correctly rounded for \|n\| <= 64) | 2 ulp for \|n\| <= 4 (plain multiplication), 1 ulp otherwise |
//! | `mul_add` | fused except for rare double rounding (1 ulp) | not fused |
//! | `log(x, base)` | `ln(x) / ln(base)` like `std` | same |
//!
//! `f32` functions are evaluated in double precision internally; nearly all
//! results are correctly rounded. Trigonometric argument reduction is exact
//! for arbitrarily large arguments. `sqrt` compiles to `sqrtss`/`sqrtsd` when
//! SSE2 is enabled and to an exact integer algorithm otherwise. `f64`'s
//! `mul_add` is not fused (two roundings); `f32`'s is computed in `f64`.
//! Most functions are `const fn`.
//!
//! # Linear algebra
//!
//! ```
//! use vmath::{Mat4, Quat, Vec3};
//!
//! let model = Mat4::from_scale_rotation_translation(Vec3::ONE, Quat::from_rotation_y(0.5), Vec3::new(0.0, 0.0, -5.0));
//! let view = Mat4::look_at_rh(Vec3::new(0.0, 2.0, 3.0), Vec3::ZERO, Vec3::Y);
//! let proj = Mat4::perspective_rh_zo(1.0, 16.0 / 9.0, 0.1, 100.0); // depth in [0, 1]
//! let clip = proj * view * model; // applied right to left
//! let ndc = clip.transform_point3(Vec3::ZERO); // includes the perspective divide
//! assert!(ndc.z > 0.0 && ndc.z < 1.0);
//! ```
//!
//! Conventions follow `glam`: column-major storage, column vectors
//! (`v' = M * v`, `A * B` applies `B` first), right-handed coordinates,
//! cameras looking down `-Z`, angles in radians. See [`matrix`] for details.
//!
//! # Random numbers, noise and color
//!
//! ```
//! use vmath::{color, Noise, Rng};
//!
//! let mut rng = Rng::new(7);
//! let x = rng.range_f32(-10.0, 10.0);
//! let height = Noise::new(7).fbm2(x * 0.1, 3.0, 5, 2.0, 0.5); // about [-1, 1]
//! let t = vmath::remap(height, -1.0, 1.0, 0.0, 1.0);
//! let pixel: u32 = color::lerp_argb(0xff20_4080, 0xffff_ffff, t); // 0xAARRGGBB
//! assert_eq!(pixel >> 24, 0xff);
//! let linear = color::srgb8_to_linear(0x80);
//! assert_eq!(color::linear_to_srgb8(linear), 0x80);
//! ```
#![no_std]

#[cfg(test)]
extern crate std;

pub mod color;
pub mod f32;
pub mod f64;
mod float_ext;
pub mod matrix;
pub mod noise;
pub mod quat;
pub mod rng;
pub mod scalar;
pub mod vector;

#[cfg(test)]
mod libm_tests;

pub use float_ext::FloatExt;
pub use matrix::{Mat3, Mat4};
pub use noise::Noise;
pub use quat::Quat;
pub use rng::Rng;
pub use scalar::*;
pub use vector::{IVec2, Vec2, Vec3, Vec4};
