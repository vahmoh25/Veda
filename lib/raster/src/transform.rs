//! 2x3 affine transforms.

use crate::geom::Point;
use crate::math;

/// A 2D affine transform (the same layout as the canvas/SVG `matrix(a, b, c, d, e, f)`):
///
/// ```text
/// x' = a * x + c * y + e
/// y' = b * x + d * y + f
/// ```
///
/// Rotation angles are in radians; positive angles turn the +x axis towards +y, i.e. clockwise on
/// a y-down screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    /// x scale / rotation component.
    pub a: f32,
    /// y shear / rotation component.
    pub b: f32,
    /// x shear / rotation component.
    pub c: f32,
    /// y scale / rotation component.
    pub d: f32,
    /// x translation.
    pub e: f32,
    /// y translation.
    pub f: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    /// The identity transform.
    pub const IDENTITY: Transform = Transform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// Creates a transform from its six coefficients.
    #[inline]
    pub const fn new(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Self {
        Transform { a, b, c, d, e, f }
    }

    /// The identity transform.
    #[inline]
    pub const fn identity() -> Self {
        Transform::IDENTITY
    }

    /// A translation by `(tx, ty)`.
    #[inline]
    pub const fn translate(tx: f32, ty: f32) -> Self {
        Transform::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    /// A scale by `(sx, sy)` about the origin.
    #[inline]
    pub const fn scale(sx: f32, sy: f32) -> Self {
        Transform::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// A rotation by `angle` radians about the origin.
    #[inline]
    pub fn rotate(angle: f32) -> Self {
        let (s, c) = math::sin_cos(angle);
        Transform::new(c, s, -s, c, 0.0, 0.0)
    }

    /// A rotation by `angle` radians about the point `(cx, cy)`.
    pub fn rotate_about(angle: f32, cx: f32, cy: f32) -> Self {
        Transform::translate(-cx, -cy).then(&Transform::rotate(angle)).then(&Transform::translate(cx, cy))
    }

    /// Matrix product `self * other`: the result applies `other` first, then `self`.
    pub fn multiply(&self, o: &Transform) -> Transform {
        Transform {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }

    /// Returns a transform that applies `self` first, then `next`.
    #[inline]
    pub fn then(&self, next: &Transform) -> Transform {
        next.multiply(self)
    }

    /// Returns `self` followed by a translation.
    #[inline]
    pub fn then_translate(&self, tx: f32, ty: f32) -> Transform {
        let mut t = *self;
        t.e += tx;
        t.f += ty;
        t
    }

    /// Returns `self` followed by a scale about the origin.
    #[inline]
    pub fn then_scale(&self, sx: f32, sy: f32) -> Transform {
        self.then(&Transform::scale(sx, sy))
    }

    /// Returns `self` followed by a rotation about the origin.
    #[inline]
    pub fn then_rotate(&self, angle: f32) -> Transform {
        self.then(&Transform::rotate(angle))
    }

    /// Transforms a point.
    #[inline]
    pub fn apply(&self, p: Point) -> Point {
        Point::new(self.a * p.x + self.c * p.y + self.e, self.b * p.x + self.d * p.y + self.f)
    }

    /// Transforms a vector (ignores the translation).
    #[inline]
    pub fn apply_vector(&self, v: Point) -> Point {
        Point::new(self.a * v.x + self.c * v.y, self.b * v.x + self.d * v.y)
    }

    /// The determinant of the linear part.
    #[inline]
    pub fn determinant(&self) -> f32 {
        self.a * self.d - self.b * self.c
    }

    /// The inverse transform, or `None` if the transform is singular or not finite.
    pub fn invert(&self) -> Option<Transform> {
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = 1.0 / det;
        let a = self.d * inv;
        let b = -self.b * inv;
        let c = -self.c * inv;
        let d = self.a * inv;
        let e = -(a * self.e + c * self.f);
        let f = -(b * self.e + d * self.f);
        let t = Transform::new(a, b, c, d, e, f);
        if t.is_finite() { Some(t) } else { None }
    }

    /// Returns `true` if this is exactly the identity.
    #[inline]
    pub fn is_identity(&self) -> bool {
        *self == Transform::IDENTITY
    }

    /// Returns `true` if all coefficients are finite.
    pub fn is_finite(&self) -> bool {
        self.a.is_finite()
            && self.b.is_finite()
            && self.c.is_finite()
            && self.d.is_finite()
            && self.e.is_finite()
            && self.f.is_finite()
    }

    /// The largest factor by which the transform stretches any vector (the spectral norm of the
    /// linear part). Useful to convert a device-space tolerance into path units.
    pub fn max_scale(&self) -> f32 {
        let (a, b, c, d) = (self.a as f64, self.b as f64, self.c as f64, self.d as f64);
        let t = a * a + b * b + c * c + d * d;
        let det = a * d - b * c;
        let disc = math::sqrt64((t * t - 4.0 * det * det).max(0.0));
        math::sqrt64((t + disc) * 0.5) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(p: Point, x: f32, y: f32) -> bool {
        (p.x - x).abs() < 1e-4 && (p.y - y).abs() < 1e-4
    }

    #[test]
    fn compose_and_invert() {
        let t = Transform::scale(2.0, 3.0).then_translate(10.0, 20.0);
        assert!(near(t.apply(Point::new(1.0, 1.0)), 12.0, 23.0));
        let r = Transform::rotate(core::f32::consts::FRAC_PI_2);
        // +x turns towards +y.
        assert!(near(r.apply(Point::new(1.0, 0.0)), 0.0, 1.0));
        let m = Transform::translate(5.0, 0.0).multiply(&Transform::scale(2.0, 2.0));
        // multiply: scale first, then translate.
        assert!(near(m.apply(Point::new(1.0, 1.0)), 7.0, 2.0));
        let full = t.then(&r).then_rotate(0.3).then_scale(0.5, 4.0);
        let inv = full.invert().unwrap();
        let p = Point::new(3.5, -7.25);
        let q = inv.apply(full.apply(p));
        assert!(near(q, p.x, p.y));
        assert!(Transform::scale(0.0, 1.0).invert().is_none());
        let ra = Transform::rotate_about(core::f32::consts::PI, 10.0, 10.0);
        assert!(near(ra.apply(Point::new(11.0, 10.0)), 9.0, 10.0));
    }

    #[test]
    fn max_scale() {
        assert!((Transform::scale(2.0, 3.0).max_scale() - 3.0).abs() < 1e-5);
        assert!((Transform::rotate(0.7).then_scale(5.0, 5.0).max_scale() - 5.0).abs() < 1e-4);
        assert!((Transform::IDENTITY.max_scale() - 1.0).abs() < 1e-6);
    }
}
