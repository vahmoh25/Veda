//! `vgl` — OpenGL ES 3.0 for Veda.
//!
//! * [`Context`] is the API: every OpenGL ES 3.0 entry point, as a method
//!   named after the C function in snake case (`glDrawArrays` is
//!   [`Context::draw_arrays`]), with the enumerants of [`gl`]. Every call is
//!   checked as the specification requires, and a failing call changes
//!   nothing and records its error for [`Context::get_error`]. Shaders are
//!   compiled by [`vglsl`].
//! * A [`backend::Backend`] renders: [`soft`], a multi-threaded software
//!   renderer that runs the shaders on a SIMD interpreter, or [`virgl`],
//!   which sends the work to the GPU through Veda's renderer.
//! * [`format`] knows OpenGL ES's pixel formats and converts between them.
//!
//! Differences from the C API, as in WebGL 2: vertex and index data always
//! come from buffer objects (there are no client-side arrays), pixel data
//! is a slice whose length is checked, and the "pointer" arguments that
//! are offsets into bound buffers are `usize` offsets.

#![no_std]

extern crate alloc;
#[cfg(any(test, feature = "host-virgl"))]
extern crate std;

pub mod backend;
pub mod context;
pub mod etc;
pub mod format;
pub mod gl;
pub mod pixels;
pub mod present;
pub mod soft;
#[cfg(feature = "veda")]
pub mod veda;
pub mod virgl;

#[cfg(test)]
mod tests;

pub use context::{Config, Context};
pub use pixels::{Pixels, PixelsMut};
