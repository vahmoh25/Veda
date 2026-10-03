//! Anti-aliased vector path rasterizer shared by the font engine and 2D graphics.
//!
//! * [`Path`] builds contours of lines and quadratic/cubic Bézier curves, with helpers for
//!   rectangles, per-corner rounded rectangles, circles, ellipses, arcs and pies.
//! * [`Transform`] is a 2x3 affine matrix applied while rasterizing.
//! * [`Rasterizer`] converts paths into exact-area anti-aliased coverage (FreeType "gray"-style
//!   cell accumulation in 24.8 fixed point, non-zero and even-odd fill rules) and hands it out as
//!   [`Span`]s that distinguish solid runs from per-pixel coverage, or renders into a [`Mask`].
//! * [`Stroker`] / [`stroke`] turn stroked paths (width, caps, joins, miter limit) into fillable
//!   outlines.
//! * [`flatten`] approximates curves by lines (Wang's formula, default tolerance 0.2 px).
//! * [`math`] provides the few float functions (`floor`, `sqrt`, `sin_cos`, ...) missing from `core`.
//!
//! ```ignore
//! let mut path = Path::new();
//! path.rounded_rect(10.0, 10.0, 200.0, 80.0, [8.0; 4]);
//! let mut raster = Rasterizer::new();
//! raster.fill(&path, &Transform::IDENTITY, FillRule::NonZero, (width, height), |span| match span.coverage {
//!     Coverage::Solid(a) => fill_run(span.y, span.x, span.len, a),
//!     Coverage::Mask(m) => blend_run(span.y, span.x, m),
//! });
//! ```

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod flatten;
mod geom;
pub mod math;
mod path;
mod raster;
mod stroke;
mod transform;

pub use geom::{Point, Rect};
pub use path::{Path, PathEl, PathIter, Verb};
pub use raster::{Coverage, FillRule, MAX_DIMENSION, Mask, Rasterizer, Span};
pub use stroke::{LineCap, LineJoin, StrokeStyle, Stroker, stroke};
pub use transform::Transform;

#[cfg(test)]
mod visual_tests;
