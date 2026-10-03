//! `v3d` — Vindows 3D: a multi-threaded fixed-point software renderer.
//!
//! There is no GPU: everything runs on the CPU, usually under QEMU's TCG
//! emulator, where integer instructions are cheap and floating point is
//! very expensive. So floating point is used only once per draw call (to
//! build matrices and light parameters); vertex transformation and
//! lighting, clipping, triangle setup and rasterisation are fixed-point
//! integer code (the [`pipeline`] module).
//!
//! # Pipeline
//!
//! * [`Mesh`]es (built with [`MeshBuilder`] and the [`shapes`] module) are
//!   drawn with a model matrix and a [`Material`] into a [`Frame`] obtained
//!   from [`Renderer::frame`], under a [`Camera`] and an [`Environment`]
//!   (sun, hemisphere ambient, point lights, fog, sky gradient).
//! * Per-object frustum culling, back-face culling, near-plane and
//!   guard-band clipping, depth buffering, perspective-correct mip-mapped
//!   texturing, Gouraud (per-vertex) Lambert / Blinn-Phong lighting,
//!   distance fog, alpha / additive / multiplicative blending, alpha
//!   testing, [`Billboard`]s and [`ParticleSystem`]s.
//! * Triangles are binned into 64x32 screen tiles; geometry and tiles are
//!   processed by a [`ThreadPool`] sized to the CPU count.
//! * The image is rendered at an internal resolution and scaled to the
//!   window with [`Renderer::present`].
//!
//! ```ignore
//! let mut r = v3d::Renderer::new(512, 320, v3d::ThreadPool::for_system());
//! let cube = v3d::shapes::cuboid(vmath::Vec3::ONE, 0xFFFF8000).build();
//! let cam = v3d::Camera::look_at(vmath::Vec3::new(2.0, 2.0, 3.0), vmath::Vec3::ZERO, vmath::Vec3::Y);
//! let mut f = r.frame(&cam, &v3d::Environment::default());
//! f.draw(&cube, &vmath::Mat4::IDENTITY, &v3d::Material::lambert(0xFFFFFFFF));
//! f.finish();
//! r.present(window_pixels, stride, width, height);
//! ```

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

#[cfg(feature = "app")]
pub mod app;
pub mod camera;
mod clip;
pub mod env;
mod fixed;
pub mod material;
pub mod mesh;
pub mod particles;
pub mod pipeline;
pub mod pool;
pub mod renderer;
pub mod shapes;
pub mod texture;

#[cfg(test)]
mod tests;

pub use camera::{Camera, Frustum};
pub use env::{Background, Environment, Fog, PointLight};
pub use fixed::{alpha_of, pack_rgb, pack_rgba, rgb_of};
pub use material::{Blend, Cull, Material, Shading};
pub use mesh::{Bounds, Mesh, MeshBuilder, Vertex};
pub use particles::{Burst, Particle, ParticleSystem};
pub use pool::ThreadPool;
pub use renderer::{
    BILLBOARD_FADE_END, BILLBOARD_FADE_START, Beam, Billboard, Frame, Renderer, Stats, TILE_H, TILE_W, Upscale2x,
};
pub use texture::{Texture, TextureId, lerp_color};
