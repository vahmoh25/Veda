//! Rotation quaternions ([`Quat`]).
//!
//! A `Quat` is `x i + y j + z k + w`; rotations use unit quaternions.
//! Conventions match [`Mat3`]/[`Mat4`]: right-handed, positive angles rotate
//! counter-clockwise around the axis (right-hand rule), and `a * b` applies
//! `b` first, then `a` (like matrices). `q * v` rotates the vector `v`.
//!
//! Euler angles use the **YXZ** order common for cameras and characters:
//! [`Quat::from_euler(yaw, pitch, roll)`](Quat::from_euler) is
//! `Ry(yaw) * Rx(pitch) * Rz(roll)`, i.e. roll around the local Z axis first,
//! then pitch around X, then yaw around the world Y axis.
//!
//! "Forward" is `-Z` (as for the view matrices in [`Mat4`]), so
//! [`Quat::look_rotation`] rotates `-Z` onto the given direction.

use core::ops::{Add, Mul, MulAssign, Neg, Sub};

use crate::f32 as m;
use crate::{Mat3, Mat4, Vec3, Vec4};

/// A quaternion; unit quaternions represent 3D rotations.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Default for Quat {
    /// The identity rotation.
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Quat {
    /// The identity rotation.
    pub const IDENTITY: Self = Self::from_xyzw(0.0, 0.0, 0.0, 1.0);

    /// Creates a quaternion from its components (not normalized).
    #[inline]
    pub const fn from_xyzw(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// Creates a quaternion from `[x, y, z, w]`.
    #[inline]
    pub const fn from_array(a: [f32; 4]) -> Self {
        Self::from_xyzw(a[0], a[1], a[2], a[3])
    }

    /// The components as `[x, y, z, w]`.
    #[inline]
    pub const fn to_array(self) -> [f32; 4] {
        [self.x, self.y, self.z, self.w]
    }

    /// Creates a quaternion from a `Vec4` `(x, y, z, w)`.
    #[inline]
    pub const fn from_vec4(v: Vec4) -> Self {
        Self::from_xyzw(v.x, v.y, v.z, v.w)
    }

    /// The components as a `Vec4` `(x, y, z, w)`.
    #[inline]
    pub const fn to_vec4(self) -> Vec4 {
        Vec4::new(self.x, self.y, self.z, self.w)
    }

    /// The vector part `(x, y, z)`.
    #[inline]
    pub const fn xyz(self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }

    /// A rotation of `angle` radians around the unit vector `axis`.
    #[inline]
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle * 0.5);
        let v = axis * s;
        Self::from_xyzw(v.x, v.y, v.z, c)
    }

    /// A rotation around `v.normalize()` by `v.length()` radians (a "rotation vector").
    #[inline]
    pub fn from_scaled_axis(v: Vec3) -> Self {
        let len = v.length();
        if len == 0.0 { Self::IDENTITY } else { Self::from_axis_angle(v / len, len) }
    }

    /// A rotation around the X axis.
    #[inline]
    pub fn from_rotation_x(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle * 0.5);
        Self::from_xyzw(s, 0.0, 0.0, c)
    }

    /// A rotation around the Y axis.
    #[inline]
    pub fn from_rotation_y(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle * 0.5);
        Self::from_xyzw(0.0, s, 0.0, c)
    }

    /// A rotation around the Z axis.
    #[inline]
    pub fn from_rotation_z(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle * 0.5);
        Self::from_xyzw(0.0, 0.0, s, c)
    }

    /// Euler angles in YXZ order: `Ry(yaw) * Rx(pitch) * Rz(roll)` (radians).
    #[inline]
    pub fn from_euler(yaw: f32, pitch: f32, roll: f32) -> Self {
        Self::from_rotation_y(yaw) * Self::from_rotation_x(pitch) * Self::from_rotation_z(roll)
    }

    /// The YXZ Euler angles `(yaw, pitch, roll)` of a unit quaternion, inverse of
    /// [`from_euler`](Self::from_euler). Pitch is in `[-π/2, π/2]`; at the poles
    /// (gimbal lock) the roll is reported as 0.
    pub fn to_euler(self) -> (f32, f32, f32) {
        let Self { x, y, z, w } = self;
        // Rotation matrix elements (row, column) of Ry * Rx * Rz.
        let r12 = 2.0 * (y * z - w * x); // -sin(pitch)
        let sp = m::clamp(-r12, -1.0, 1.0);
        let pitch = m::asin(sp);
        if m::abs(sp) < 0.999_999 {
            let yaw = m::atan2(2.0 * (x * z + w * y), 1.0 - 2.0 * (x * x + y * y));
            let roll = m::atan2(2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z));
            (yaw, pitch, roll)
        } else {
            let yaw = m::atan2(-2.0 * (x * z - w * y), 1.0 - 2.0 * (y * y + z * z));
            (yaw, pitch, 0.0)
        }
    }

    /// The rotation of a pure rotation matrix (orthonormal, determinant 1).
    pub fn from_mat3(mat: &Mat3) -> Self {
        // mCR = column C, row R.
        let (m00, m01, m02) = (mat.x_axis.x, mat.x_axis.y, mat.x_axis.z);
        let (m10, m11, m12) = (mat.y_axis.x, mat.y_axis.y, mat.y_axis.z);
        let (m20, m21, m22) = (mat.z_axis.x, mat.z_axis.y, mat.z_axis.z);
        // Pick the largest of |x|, |y|, |z|, |w| to divide by, for stability.
        if m22 <= 0.0 {
            let dif10 = m11 - m00;
            let omm22 = 1.0 - m22;
            if dif10 <= 0.0 {
                let four_xsq = omm22 - dif10;
                let inv4x = 0.5 / m::sqrt(four_xsq);
                Self::from_xyzw(four_xsq * inv4x, (m01 + m10) * inv4x, (m02 + m20) * inv4x, (m12 - m21) * inv4x)
            } else {
                let four_ysq = omm22 + dif10;
                let inv4y = 0.5 / m::sqrt(four_ysq);
                Self::from_xyzw((m01 + m10) * inv4y, four_ysq * inv4y, (m12 + m21) * inv4y, (m20 - m02) * inv4y)
            }
        } else {
            let sum10 = m11 + m00;
            let opm22 = 1.0 + m22;
            if sum10 <= 0.0 {
                let four_zsq = opm22 - sum10;
                let inv4z = 0.5 / m::sqrt(four_zsq);
                Self::from_xyzw((m02 + m20) * inv4z, (m12 + m21) * inv4z, four_zsq * inv4z, (m01 - m10) * inv4z)
            } else {
                let four_wsq = opm22 + sum10;
                let inv4w = 0.5 / m::sqrt(four_wsq);
                Self::from_xyzw((m12 - m21) * inv4w, (m20 - m02) * inv4w, (m01 - m10) * inv4w, four_wsq * inv4w)
            }
        }
    }

    /// The rotation part of a 4x4 matrix without scale or shear.
    #[inline]
    pub fn from_mat4(mat: &Mat4) -> Self {
        Self::from_mat3(&Mat3::from_mat4(mat))
    }

    /// The shortest rotation taking the unit vector `from` to the unit vector `to`.
    pub fn from_rotation_arc(from: Vec3, to: Vec3) -> Self {
        const ONE_MINUS_EPS: f32 = 1.0 - 2.0 * f32::EPSILON;
        let dot = from.dot(to);
        if dot > ONE_MINUS_EPS {
            Self::IDENTITY
        } else if dot < -ONE_MINUS_EPS {
            // Opposite vectors: half a turn around any perpendicular axis.
            Self::from_axis_angle(from.any_orthonormal_vector(), core::f32::consts::PI)
        } else {
            let c = from.cross(to);
            Self::from_xyzw(c.x, c.y, c.z, 1.0 + dot).normalize()
        }
    }

    /// The orientation whose forward axis (`-Z`) points along `forward` and
    /// whose `+Y` axis is as close to `up` as possible. Falls back to some
    /// perpendicular up vector if `up` is parallel to `forward`; a zero
    /// `forward` means `-Z`.
    pub fn look_rotation(forward: Vec3, up: Vec3) -> Self {
        let back = -forward.normalize_or(Vec3::NEG_Z); // local +Z
        let right = match up.cross(back).try_normalize() {
            Some(r) => r,
            None => back.any_orthonormal_vector(),
        };
        let up = back.cross(right);
        Self::from_mat3(&Mat3::from_cols(right, up, back))
    }

    /// The rotation axis (unit) and angle in `[0, π]`; the X axis and 0 for the identity.
    pub fn to_axis_angle(self) -> (Vec3, f32) {
        let q = if self.w < 0.0 { -self } else { self };
        let s = q.xyz().length();
        if s < 1e-7 {
            return (Vec3::X, 0.0);
        }
        (q.xyz() / s, 2.0 * m::atan2(s, q.w))
    }

    /// The rotation as a scaled axis (axis times angle).
    #[inline]
    pub fn to_scaled_axis(self) -> Vec3 {
        let (axis, angle) = self.to_axis_angle();
        axis * angle
    }

    /// The conjugate `(-x, -y, -z, w)`; the inverse of a unit quaternion.
    #[inline]
    pub const fn conjugate(self) -> Self {
        Self::from_xyzw(-self.x, -self.y, -self.z, self.w)
    }

    /// The inverse (`conjugate / length²`).
    #[inline]
    pub fn inverse(self) -> Self {
        self.conjugate() * (1.0 / self.length_squared())
    }

    /// The 4D dot product.
    #[inline]
    pub fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z + self.w * rhs.w
    }

    /// The squared length.
    #[inline]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    /// The length.
    #[inline]
    pub fn length(self) -> f32 {
        m::sqrt(self.dot(self))
    }

    /// The quaternion scaled to unit length (non-finite for zero input).
    #[inline]
    pub fn normalize(self) -> Self {
        self * (1.0 / self.length())
    }

    /// Whether the length is 1 within a small tolerance.
    #[inline]
    pub fn is_normalized(self) -> bool {
        m::abs(self.length_squared() - 1.0) <= 2e-4
    }

    /// Whether every component is finite.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.w.is_finite()
    }

    /// Whether every component differs from `rhs` by at most `max_abs_diff`
    /// (note that `q` and `-q` are the same rotation but not equal here).
    #[inline]
    pub fn abs_diff_eq(self, rhs: Self, max_abs_diff: f32) -> bool {
        self.to_vec4().abs_diff_eq(rhs.to_vec4(), max_abs_diff)
    }

    /// The angle in radians (in `[0, π]`) of the rotation between two unit quaternions.
    #[inline]
    pub fn angle_between(self, rhs: Self) -> f32 {
        2.0 * m::acos(m::min(m::abs(self.dot(rhs)), 1.0))
    }

    /// The Hamilton product `self * rhs` (applies `rhs` first).
    #[inline]
    pub fn mul_quat(self, rhs: Self) -> Self {
        let (x0, y0, z0, w0) = (self.x, self.y, self.z, self.w);
        let (x1, y1, z1, w1) = (rhs.x, rhs.y, rhs.z, rhs.w);
        Self::from_xyzw(
            w0 * x1 + x0 * w1 + y0 * z1 - z0 * y1,
            w0 * y1 - x0 * z1 + y0 * w1 + z0 * x1,
            w0 * z1 + x0 * y1 - y0 * x1 + z0 * w1,
            w0 * w1 - x0 * x1 - y0 * y1 - z0 * z1,
        )
    }

    /// Rotates `v` by this unit quaternion (same as `self * v`).
    #[inline]
    pub fn mul_vec3(self, v: Vec3) -> Vec3 {
        let u = self.xyz();
        let t = u.cross(v) * 2.0;
        v + t * self.w + u.cross(t)
    }

    /// Rotates `v` by this unit quaternion.
    #[inline]
    pub fn rotate(self, v: Vec3) -> Vec3 {
        self.mul_vec3(v)
    }

    /// Normalized linear interpolation along the shorter arc (fast, slightly
    /// non-uniform speed).
    #[inline]
    pub fn nlerp(self, end: Self, t: f32) -> Self {
        let end = if self.dot(end) < 0.0 { -end } else { end };
        (self + (end - self) * t).normalize()
    }

    /// Spherical linear interpolation along the shorter arc (constant angular speed).
    pub fn slerp(self, end: Self, t: f32) -> Self {
        let mut end = end;
        let mut dot = self.dot(end);
        if dot < 0.0 {
            end = -end;
            dot = -dot;
        }
        if dot > 0.9995 {
            // Nearly parallel: nlerp is accurate and avoids dividing by sin(θ) ~ 0.
            return (self + (end - self) * t).normalize();
        }
        let theta = m::acos(dot);
        let inv_sin = 1.0 / m::sin(theta);
        let s0 = m::sin((1.0 - t) * theta) * inv_sin;
        let s1 = m::sin(t * theta) * inv_sin;
        self * s0 + end * s1
    }

    /// Rotates toward `target` by at most `max_angle` radians (never overshoots).
    #[inline]
    pub fn rotate_towards(self, target: Self, max_angle: f32) -> Self {
        let angle = self.angle_between(target);
        if angle <= max_angle || angle == 0.0 {
            return target;
        }
        self.slerp(target, max_angle / angle)
    }

    /// The rotation as a 3x3 matrix.
    #[inline]
    pub fn to_mat3(self) -> Mat3 {
        Mat3::from_quat(self)
    }

    /// The rotation as a 4x4 matrix.
    #[inline]
    pub fn to_mat4(self) -> Mat4 {
        Mat4::from_quat(self)
    }
}

impl Mul for Quat {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        self.mul_quat(rhs)
    }
}

impl MulAssign for Quat {
    #[inline]
    fn mul_assign(&mut self, rhs: Self) {
        *self = self.mul_quat(rhs);
    }
}

impl Mul<Vec3> for Quat {
    type Output = Vec3;
    #[inline]
    fn mul(self, rhs: Vec3) -> Vec3 {
        self.mul_vec3(rhs)
    }
}

impl Mul<f32> for Quat {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f32) -> Self {
        Self::from_xyzw(self.x * rhs, self.y * rhs, self.z * rhs, self.w * rhs)
    }
}

impl Add for Quat {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self::from_xyzw(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z, self.w + rhs.w)
    }
}

impl Sub for Quat {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self::from_xyzw(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z, self.w - rhs.w)
    }
}

impl Neg for Quat {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self::from_xyzw(-self.x, -self.y, -self.z, -self.w)
    }
}

impl From<Quat> for Vec4 {
    #[inline]
    fn from(q: Quat) -> Vec4 {
        q.to_vec4()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rng;
    use core::f32::consts::{FRAC_PI_2, PI};

    fn random_quat(rng: &mut Rng) -> Quat {
        Quat::from_axis_angle(rng.unit_vec3(), rng.range_f32(-PI, PI))
    }

    /// Same rotation (q and -q are equivalent).
    fn same_rotation(a: Quat, b: Quat, eps: f32) -> bool {
        a.dot(b).abs() > 1.0 - eps
    }

    #[test]
    fn basics() {
        let q = Quat::from_rotation_z(FRAC_PI_2);
        assert!((q * Vec3::X).abs_diff_eq(Vec3::Y, 1e-6));
        assert!(Quat::from_rotation_x(FRAC_PI_2).rotate(Vec3::Y).abs_diff_eq(Vec3::Z, 1e-6));
        assert!((Quat::from_rotation_y(FRAC_PI_2) * Vec3::Z).abs_diff_eq(Vec3::X, 1e-6));
        assert_eq!(Quat::IDENTITY * Vec3::new(1.0, 2.0, 3.0), Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(Quat::default(), Quat::IDENTITY);
        assert!(q.is_normalized());
        assert!((q * q.inverse()).abs_diff_eq(Quat::IDENTITY, 1e-6));
        assert!((q * q.conjugate()).abs_diff_eq(Quat::IDENTITY, 1e-6));
        let doubled = Quat::from_xyzw(0.0, 0.0, 2.0, 0.0);
        assert!((doubled * doubled.inverse()).abs_diff_eq(Quat::IDENTITY, 1e-6));
        assert_eq!(Quat::from_array([1.0, 2.0, 3.0, 4.0]).to_array(), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(Vec4::from(Quat::IDENTITY), Vec4::W);
        assert!((Quat::from_xyzw(1.0, 2.0, 3.0, 4.0).normalize().length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn composition_matches_matrices() {
        let mut rng = Rng::new(20);
        for _ in 0..1000 {
            let (a, b) = (random_quat(&mut rng), random_quat(&mut rng));
            let v = rng.unit_vec3() * 3.0;
            // a * b applies b first, like matrices.
            assert!(((a * b) * v).abs_diff_eq(a * (b * v), 1e-4));
            assert!(((a * b).to_mat3() * v).abs_diff_eq(a.to_mat3() * (b.to_mat3() * v), 1e-4));
            assert!((a.to_mat4().transform_vector3(v)).abs_diff_eq(a * v, 1e-4));
            let mut c = a;
            c *= b;
            assert_eq!(c, a * b);
        }
    }

    #[test]
    fn matrix_round_trip() {
        let mut rng = Rng::new(21);
        for _ in 0..5000 {
            let q = random_quat(&mut rng);
            let back = Quat::from_mat3(&q.to_mat3());
            assert!(same_rotation(q, back, 1e-5), "{q:?} {back:?}");
            assert!(same_rotation(q, Quat::from_mat4(&q.to_mat4()), 1e-5));
        }
        // All four branches of from_mat3, including half turns.
        for (axis, angle) in
            [(Vec3::X, PI), (Vec3::Y, PI), (Vec3::Z, PI), (Vec3::X, 0.1), (Vec3::new(1.0, 1.0, 0.0).normalize(), 3.0)]
        {
            let q = Quat::from_axis_angle(axis, angle);
            assert!(same_rotation(q, Quat::from_mat3(&Mat3::from_axis_angle(axis, angle)), 1e-6));
        }
    }

    #[test]
    fn axis_angle_round_trip() {
        let mut rng = Rng::new(22);
        for _ in 0..1000 {
            let axis = rng.unit_vec3();
            let angle = rng.range_f32(0.01, PI - 0.01);
            let (axis2, angle2) = Quat::from_axis_angle(axis, angle).to_axis_angle();
            assert!(axis2.abs_diff_eq(axis, 1e-4), "{axis:?} {axis2:?}");
            assert!((angle2 - angle).abs() < 1e-4);
            let v = axis * angle;
            assert!(Quat::from_scaled_axis(v).to_scaled_axis().abs_diff_eq(v, 1e-4));
        }
        assert_eq!(Quat::IDENTITY.to_axis_angle(), (Vec3::X, 0.0));
        assert_eq!(Quat::from_scaled_axis(Vec3::ZERO), Quat::IDENTITY);
    }

    #[test]
    fn euler_yxz() {
        let (yaw, pitch, roll) = (0.3, -0.4, 0.5);
        let q = Quat::from_euler(yaw, pitch, roll);
        let m = Mat3::from_rotation_y(yaw) * Mat3::from_rotation_x(pitch) * Mat3::from_rotation_z(roll);
        assert!(q.to_mat3().abs_diff_eq(&m, 1e-6));
        let mut rng = Rng::new(23);
        for _ in 0..2000 {
            let yaw = rng.range_f32(-PI + 0.01, PI - 0.01);
            let pitch = rng.range_f32(-FRAC_PI_2 + 0.01, FRAC_PI_2 - 0.01);
            let roll = rng.range_f32(-PI + 0.01, PI - 0.01);
            let (y2, p2, r2) = Quat::from_euler(yaw, pitch, roll).to_euler();
            assert!(
                (y2 - yaw).abs() < 1e-3 && (p2 - pitch).abs() < 1e-3 && (r2 - roll).abs() < 1e-3,
                "{yaw} {pitch} {roll} -> {y2} {p2} {r2}"
            );
        }
        // Gimbal lock: the rotation is still reproduced.
        let q = Quat::from_euler(0.7, FRAC_PI_2, 0.2);
        let (y2, p2, r2) = q.to_euler();
        assert!((p2 - FRAC_PI_2).abs() < 1e-3 && r2 == 0.0);
        assert!(same_rotation(Quat::from_euler(y2, p2, r2), q, 1e-5));
        // Yaw turns the forward axis (-Z) toward -X for positive angles.
        assert!((Quat::from_euler(FRAC_PI_2, 0.0, 0.0) * Vec3::NEG_Z).abs_diff_eq(Vec3::NEG_X, 1e-6));
        // Positive pitch lifts the forward axis.
        assert!((Quat::from_euler(0.0, 0.3, 0.0) * Vec3::NEG_Z).y > 0.0);
    }

    #[test]
    fn interpolation() {
        let a = Quat::from_rotation_z(0.0);
        let b = Quat::from_rotation_z(2.0);
        let mid = a.slerp(b, 0.5);
        assert!(same_rotation(mid, Quat::from_rotation_z(1.0), 1e-6));
        assert!(same_rotation(a.slerp(b, 0.25), Quat::from_rotation_z(0.5), 1e-6));
        assert!(same_rotation(a.slerp(b, 0.0), a, 1e-6));
        assert!(same_rotation(a.slerp(b, 1.0), b, 1e-6));
        // Takes the short way around even when the inputs are on opposite hemispheres.
        assert!(same_rotation(a.slerp(-b, 0.5), Quat::from_rotation_z(1.0), 1e-6));
        let n = a.nlerp(b, 0.5);
        assert!(n.is_normalized() && same_rotation(n, Quat::from_rotation_z(1.0), 1e-6));
        // Nearly identical inputs.
        let c = Quat::from_rotation_z(1e-4);
        assert!(a.slerp(c, 0.5).is_normalized());
        assert!((a.angle_between(b) - 2.0).abs() < 1e-5);
        let step = a.rotate_towards(b, 0.5);
        assert!(same_rotation(step, Quat::from_rotation_z(0.5), 1e-6));
        assert_eq!(a.rotate_towards(b, 5.0), b);
    }

    #[test]
    fn rotation_arc_and_look_rotation() {
        let mut rng = Rng::new(24);
        for _ in 0..2000 {
            let (from, to) = (rng.unit_vec3(), rng.unit_vec3());
            let q = Quat::from_rotation_arc(from, to);
            assert!(q.is_normalized());
            assert!((q * from).abs_diff_eq(to, 1e-4), "{from:?} -> {to:?}: {:?}", q * from);
        }
        assert_eq!(Quat::from_rotation_arc(Vec3::X, Vec3::X), Quat::IDENTITY);
        let flip = Quat::from_rotation_arc(Vec3::Y, Vec3::NEG_Y);
        assert!((flip * Vec3::Y).abs_diff_eq(Vec3::NEG_Y, 1e-6));

        assert!(same_rotation(Quat::look_rotation(Vec3::NEG_Z, Vec3::Y), Quat::IDENTITY, 1e-6));
        for _ in 0..2000 {
            let fwd = rng.unit_vec3();
            if fwd.dot(Vec3::Y).abs() > 0.999 {
                continue;
            }
            let q = Quat::look_rotation(fwd * 4.0, Vec3::Y);
            assert!(q.is_normalized());
            assert!((q * Vec3::NEG_Z).abs_diff_eq(fwd, 1e-4));
            let up = q * Vec3::Y;
            assert!(up.y > 0.0 && up.dot(fwd).abs() < 1e-4);
            // Consistent with the view matrix: view = inverse of the camera transform.
            let view = Mat4::look_to_rh(Vec3::ZERO, fwd, Vec3::Y);
            assert!(Mat3::from_mat4(&view).abs_diff_eq(&q.conjugate().to_mat3(), 1e-4));
        }
        // Degenerate up vector: still a valid rotation with the right forward axis.
        let q = Quat::look_rotation(Vec3::Y, Vec3::Y);
        assert!(q.is_normalized() && (q * Vec3::NEG_Z).abs_diff_eq(Vec3::Y, 1e-5));
        assert!(Quat::look_rotation(Vec3::ZERO, Vec3::Y).is_normalized());
    }
}
