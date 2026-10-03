//! Glyph outline consumers.
//!
//! Outlines are produced in font units with y pointing up. [`OutlineSink`] receives them;
//! [`PathSink`] maps them into a [`vraster::Path`] (scale, y-flip, offset) and [`BoundsSink`]
//! computes their control box.

use vraster::{Path, Point, Rect};

/// Receives the contours of a glyph outline (font units, y up). Every contour starts with
/// `move_to` and ends with `close`.
pub trait OutlineSink {
    /// Starts a contour.
    fn move_to(&mut self, x: f32, y: f32);
    /// Straight line to `(x, y)`.
    fn line_to(&mut self, x: f32, y: f32);
    /// Quadratic Bézier (TrueType outlines).
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32);
    /// Cubic Bézier (CFF outlines).
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32);
    /// Closes the current contour.
    fn close(&mut self);
}

/// Appends outlines to a [`Path`], mapping `(x, y)` to `(x * sx + dx, y * sy + dy)`.
///
/// For top-down bitmaps use `sx = scale`, `sy = -scale` (y-flip) and put the baseline at `dy`.
pub struct PathSink<'p> {
    /// Destination path.
    pub path: &'p mut Path,
    /// Horizontal scale.
    pub sx: f32,
    /// Vertical scale (negative to flip y).
    pub sy: f32,
    /// Horizontal offset.
    pub dx: f32,
    /// Vertical offset.
    pub dy: f32,
}

impl<'p> PathSink<'p> {
    /// A sink producing pixel coordinates for a y-down raster: `scale` = px per font unit, the
    /// glyph origin (pen position on the baseline) at `origin`.
    pub fn new(path: &'p mut Path, scale: f32, origin: Point) -> Self {
        PathSink { path, sx: scale, sy: -scale, dx: origin.x, dy: origin.y }
    }

    #[inline]
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.sx + self.dx, y * self.sy + self.dy)
    }
}

impl OutlineSink for PathSink<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.map(x, y);
        self.path.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.map(x, y);
        self.path.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.map(x1, y1);
        let (x, y) = self.map(x, y);
        self.path.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.map(x1, y1);
        let (x2, y2) = self.map(x2, y2);
        let (x, y) = self.map(x, y);
        self.path.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.path.close();
    }
}

/// Appends outlines unchanged (font units, y up).
impl OutlineSink for Path {
    fn move_to(&mut self, x: f32, y: f32) {
        Path::move_to(self, x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        Path::line_to(self, x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        Path::quad_to(self, x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        Path::cubic_to(self, x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        Path::close(self);
    }
}

/// Computes the control box (bounds of all points including off-curve control points) of an
/// outline.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoundsSink {
    /// The bounds so far (`None` until the first point).
    pub bounds: Option<Rect>,
}

impl BoundsSink {
    #[inline]
    fn add(&mut self, x: f32, y: f32) {
        let p = Point::new(x, y);
        match &mut self.bounds {
            Some(r) => r.include(p),
            None => self.bounds = Some(Rect::from_point(p)),
        }
    }
}

impl OutlineSink for BoundsSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.add(x1, y1);
        self.add(x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.add(x1, y1);
        self.add(x2, y2);
        self.add(x, y);
    }

    fn close(&mut self) {}
}
