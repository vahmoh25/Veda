//! Vindows math: a no_std libm replacement, linear algebra, noise and random numbers.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod f32;
pub mod f64;
mod float_ext;
pub mod rng;
pub mod vector;

#[cfg(test)]
mod libm_tests;

pub use float_ext::FloatExt;
pub use rng::Rng;
pub use vector::{IVec2, Vec2, Vec3, Vec4};
