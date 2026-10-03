//! Basic geometry types: points/vectors and axis-aligned rectangles.

use core::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

use crate::math;

/// A point or vector in 2D space (pixels or path units; `y` grows downwards on screens).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Point {
    /// The origin.
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    /// Creates a point.
    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }

    /// Dot product.
    #[inline]
    pub fn dot(self, o: Point) -> f32 {
        self.x * o.x + self.y * o.y
    }

    /// 2D cross product (z component of the 3D cross product).
    #[inline]
    pub fn cross(self, o: Point) -> f32 {
        self.x * o.y - self.y * o.x
    }

    /// Squared length.
    #[inline]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    /// Length.
    #[inline]
    pub fn length(self) -> f32 {
        math::hypot(self.x, self.y)
    }

    /// Returns the vector scaled to unit length, or `None` if it is (almost) zero or not finite.
    #[inline]
    pub fn normalize(self) -> Option<Point> {
        let len = self.length();
        if len > 1e-12 && len.is_finite() { Some(self * (1.0 / len)) } else { None }
    }

    /// The vector rotated by 90 degrees: `(-y, x)`. On a y-down screen this turns clockwise.
    #[inline]
    pub fn perp(self) -> Point {
        Point::new(-self.y, self.x)
    }

    /// Linear interpolation: `self + (o - self) * t`.
    #[inline]
    pub fn lerp(self, o: Point, t: f32) -> Point {
        Point::new(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }

    /// Midpoint of `self` and `o`.
    #[inline]
    pub fn midpoint(self, o: Point) -> Point {
        Point::new((self.x + o.x) * 0.5, (self.y + o.y) * 0.5)
    }

    /// Returns `true` if both coordinates are finite.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Add for Point {
    type Output = Point;
    #[inline]
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Point {
    type Output = Point;
    #[inline]
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Point {
    type Output = Point;
    #[inline]
    fn mul(self, s: f32) -> Point {
        Point::new(self.x * s, self.y * s)
    }
}

impl Neg for Point {
    type Output = Point;
    #[inline]
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

impl AddAssign for Point {
    #[inline]
    fn add_assign(&mut self, o: Point) {
        self.x += o.x;
        self.y += o.y;
    }
}

impl SubAssign for Point {
    #[inline]
    fn sub_assign(&mut self, o: Point) {
        self.x -= o.x;
        self.y -= o.y;
    }
}

/// An axis-aligned rectangle given by its minimum (`x0`, `y0`) and maximum (`x1`, `y1`) corners.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x0: f32,
    /// Top edge.
    pub y0: f32,
    /// Right edge.
    pub x1: f32,
    /// Bottom edge.
    pub y1: f32,
}

impl Rect {
    /// Creates a rectangle from its corners.
    #[inline]
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Rect { x0, y0, x1, y1 }
    }

    /// Creates a rectangle from its top-left corner and size.
    #[inline]
    pub fn from_xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect::new(x, y, x + w, y + h)
    }

    /// A degenerate rectangle containing just `p`.
    #[inline]
    pub fn from_point(p: Point) -> Self {
        Rect::new(p.x, p.y, p.x, p.y)
    }

    /// Width (`x1 - x0`).
    #[inline]
    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }

    /// Height (`y1 - y0`).
    #[inline]
    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }

    /// Returns `true` if the rectangle has no area.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    /// Grows the rectangle to include `p`.
    #[inline]
    pub fn include(&mut self, p: Point) {
        self.x0 = self.x0.min(p.x);
        self.y0 = self.y0.min(p.y);
        self.x1 = self.x1.max(p.x);
        self.y1 = self.y1.max(p.y);
    }

    /// The smallest rectangle containing both rectangles.
    #[inline]
    pub fn union(&self, o: &Rect) -> Rect {
        Rect::new(self.x0.min(o.x0), self.y0.min(o.y0), self.x1.max(o.x1), self.y1.max(o.y1))
    }

    /// The overlap of both rectangles (may be empty).
    #[inline]
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect::new(self.x0.max(o.x0), self.y0.max(o.y0), self.x1.min(o.x1), self.y1.min(o.y1))
    }

    /// Returns `true` if `p` lies inside or on the border of the rectangle.
    #[inline]
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x0 && p.x <= self.x1 && p.y >= self.y0 && p.y <= self.y1
    }
}
