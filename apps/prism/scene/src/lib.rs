//! Prism's scene, separate from its window so that it can be tested on the
//! host: everything here talks only to an OpenGL ES 3.0 [`vgl::Context`].

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod mesh;
pub mod scene;
pub mod shaders;

#[cfg(test)]
mod tests;
