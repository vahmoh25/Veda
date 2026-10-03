//! 3x3 and 4x4 `f32` matrices ([`Mat3`], [`Mat4`]).
//!
//! Conventions (the same as `glam`):
//!
//! * **Column-major storage.** A matrix is its columns `x_axis`, `y_axis`,
//!   `z_axis` (and `w_axis`), so [`Mat4::to_cols_array`] returns
//!   `[m00, m10, m20, m30, m01, ...]` (`mRC` = row R, column C), the layout
//!   OpenGL-style APIs expect. For an affine transform the columns are the
//!   images of the basis vectors and `w_axis` holds the translation.
//! * **Column vectors.** Points transform as `v' = M * v`, and `A * B` applies
//!   `B` first, then `A` (so `projection * view * model`).
//! * **Right-handed** coordinates; rotations follow the right-hand rule
//!   (a positive angle turns counter-clockwise when looking down the axis
//!   toward the origin). Angles are in radians.
//! * **Cameras** ([`Mat4::look_at_rh`]) look down `-Z` with `+Y` up.
//!   [`Mat4::perspective_rh`] / [`Mat4::orthographic_rh`] map view-space depth
//!   to NDC `z` in `[-1, 1]` (OpenGL); the `_zo` variants map to `[0, 1]`
//!   (Direct3D/Vulkan style, convenient for a `[0, 1]` depth buffer). In both,
//!   the near plane maps to the low end.
//! * **Inverses.** `try_inverse` returns `None` for singular (or non-finite)
//!   matrices; `inverse` returns [`Mat4::IDENTITY`] in that case.

use core::ops::{Add, AddAssign, Mul, MulAssign, Neg, Sub, SubAssign};

use crate::f32 as m;
use crate::{Quat, Vec2, Vec3, Vec4};

// ---------------------------------------------------------------------------
// Mat3
// ---------------------------------------------------------------------------

/// A 3x3 column-major matrix: 3D rotations/scales, or 2D affine transforms
/// (see [`from_translation`](Self::from_translation),
/// [`transform_point2`](Self::transform_point2)).
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
pub struct Mat3 {
    pub x_axis: Vec3,
    pub y_axis: Vec3,
    pub z_axis: Vec3,
}

impl Default for Mat3 {
    /// The identity matrix.
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mat3 {
    /// All zeros.
    pub const ZERO: Self = Self::from_cols(Vec3::ZERO, Vec3::ZERO, Vec3::ZERO);
    /// The identity.
    pub const IDENTITY: Self = Self::from_cols(Vec3::X, Vec3::Y, Vec3::Z);

    /// Creates a matrix from its columns.
    #[inline]
    pub const fn from_cols(x_axis: Vec3, y_axis: Vec3, z_axis: Vec3) -> Self {
        Self { x_axis, y_axis, z_axis }
    }

    /// Creates a matrix from a column-major array.
    #[inline]
    pub const fn from_cols_array(a: &[f32; 9]) -> Self {
        Self::from_cols(Vec3::new(a[0], a[1], a[2]), Vec3::new(a[3], a[4], a[5]), Vec3::new(a[6], a[7], a[8]))
    }

    /// The elements in column-major order.
    #[inline]
    pub const fn to_cols_array(&self) -> [f32; 9] {
        let (x, y, z) = (self.x_axis, self.y_axis, self.z_axis);
        [x.x, x.y, x.z, y.x, y.y, y.z, z.x, z.y, z.z]
    }

    /// Creates a matrix from an array of columns.
    #[inline]
    pub const fn from_cols_array_2d(a: &[[f32; 3]; 3]) -> Self {
        Self::from_cols(Vec3::from_array(a[0]), Vec3::from_array(a[1]), Vec3::from_array(a[2]))
    }

    /// The columns as arrays.
    #[inline]
    pub const fn to_cols_array_2d(&self) -> [[f32; 3]; 3] {
        [self.x_axis.to_array(), self.y_axis.to_array(), self.z_axis.to_array()]
    }

    /// A diagonal (scale) matrix.
    #[inline]
    pub const fn from_diagonal(d: Vec3) -> Self {
        Self::from_cols(Vec3::new(d.x, 0.0, 0.0), Vec3::new(0.0, d.y, 0.0), Vec3::new(0.0, 0.0, d.z))
    }

    /// The upper-left 3x3 part of a 4x4 matrix.
    #[inline]
    pub const fn from_mat4(m: &Mat4) -> Self {
        Self::from_cols(m.x_axis.xyz(), m.y_axis.xyz(), m.z_axis.xyz())
    }

    /// The rotation matrix of a unit quaternion.
    #[inline]
    pub fn from_quat(q: Quat) -> Self {
        let (x2, y2, z2) = (q.x + q.x, q.y + q.y, q.z + q.z);
        let (xx, xy, xz) = (q.x * x2, q.x * y2, q.x * z2);
        let (yy, yz, zz) = (q.y * y2, q.y * z2, q.z * z2);
        let (wx, wy, wz) = (q.w * x2, q.w * y2, q.w * z2);
        Self::from_cols(
            Vec3::new(1.0 - (yy + zz), xy + wz, xz - wy),
            Vec3::new(xy - wz, 1.0 - (xx + zz), yz + wx),
            Vec3::new(xz + wy, yz - wx, 1.0 - (xx + yy)),
        )
    }

    /// A rotation of `angle` radians around the unit vector `axis`.
    #[inline]
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle);
        let (xs, ys, zs) = (axis.x * s, axis.y * s, axis.z * s);
        let t = 1.0 - c;
        let (xt, yt, zt) = (axis.x * t, axis.y * t, axis.z * t);
        Self::from_cols(
            Vec3::new(axis.x * xt + c, axis.x * yt + zs, axis.x * zt - ys),
            Vec3::new(axis.y * xt - zs, axis.y * yt + c, axis.y * zt + xs),
            Vec3::new(axis.z * xt + ys, axis.z * yt - xs, axis.z * zt + c),
        )
    }

    /// A rotation around the X axis (turns +Y toward +Z).
    #[inline]
    pub fn from_rotation_x(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle);
        Self::from_cols(Vec3::X, Vec3::new(0.0, c, s), Vec3::new(0.0, -s, c))
    }

    /// A rotation around the Y axis (turns +Z toward +X).
    #[inline]
    pub fn from_rotation_y(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle);
        Self::from_cols(Vec3::new(c, 0.0, -s), Vec3::Y, Vec3::new(s, 0.0, c))
    }

    /// A rotation around the Z axis (turns +X toward +Y).
    #[inline]
    pub fn from_rotation_z(angle: f32) -> Self {
        let (s, c) = m::sin_cos(angle);
        Self::from_cols(Vec3::new(c, s, 0.0), Vec3::new(-s, c, 0.0), Vec3::Z)
    }

    /// 2D affine: a translation (in homogeneous coordinates `(x, y, 1)`).
    #[inline]
    pub const fn from_translation(translation: Vec2) -> Self {
        Self::from_cols(Vec3::X, Vec3::Y, Vec3::new(translation.x, translation.y, 1.0))
    }

    /// 2D affine: a counter-clockwise rotation (same as [`from_rotation_z`](Self::from_rotation_z)).
    #[inline]
    pub fn from_angle(angle: f32) -> Self {
        Self::from_rotation_z(angle)
    }

    /// 2D affine: a non-uniform scale `diag(s.x, s.y, 1)`. (For a 3D scale use
    /// [`from_diagonal`](Self::from_diagonal).)
    #[inline]
    pub const fn from_scale(scale: Vec2) -> Self {
        Self::from_diagonal(Vec3::new(scale.x, scale.y, 1.0))
    }

    /// 2D affine: scale, then rotate, then translate.
    #[inline]
    pub fn from_scale_angle_translation(scale: Vec2, angle: f32, translation: Vec2) -> Self {
        let (s, c) = m::sin_cos(angle);
        Self::from_cols(
            Vec3::new(c * scale.x, s * scale.x, 0.0),
            Vec3::new(-s * scale.y, c * scale.y, 0.0),
            Vec3::new(translation.x, translation.y, 1.0),
        )
    }

    /// Column `i` (panics if `i > 2`).
    #[inline]
    pub fn col(&self, i: usize) -> Vec3 {
        match i {
            0 => self.x_axis,
            1 => self.y_axis,
            2 => self.z_axis,
            _ => panic!("Mat3 column index out of bounds"),
        }
    }

    /// Mutable column `i` (panics if `i > 2`).
    #[inline]
    pub fn col_mut(&mut self, i: usize) -> &mut Vec3 {
        match i {
            0 => &mut self.x_axis,
            1 => &mut self.y_axis,
            2 => &mut self.z_axis,
            _ => panic!("Mat3 column index out of bounds"),
        }
    }

    /// Row `i` (panics if `i > 2`).
    #[inline]
    pub fn row(&self, i: usize) -> Vec3 {
        Vec3::new(self.x_axis[i], self.y_axis[i], self.z_axis[i])
    }

    /// The transpose.
    #[inline]
    pub fn transpose(&self) -> Self {
        let (x, y, z) = (self.x_axis, self.y_axis, self.z_axis);
        Self::from_cols(Vec3::new(x.x, y.x, z.x), Vec3::new(x.y, y.y, z.y), Vec3::new(x.z, y.z, z.z))
    }

    /// The determinant.
    #[inline]
    pub fn determinant(&self) -> f32 {
        self.z_axis.dot(self.x_axis.cross(self.y_axis))
    }

    /// The inverse, or `None` if the matrix is singular or not finite.
    pub fn try_inverse(&self) -> Option<Self> {
        let tmp0 = self.y_axis.cross(self.z_axis);
        let tmp1 = self.z_axis.cross(self.x_axis);
        let tmp2 = self.x_axis.cross(self.y_axis);
        let det = self.z_axis.dot(tmp2);
        let inv_det = 1.0 / det;
        if det == 0.0 || !inv_det.is_finite() {
            return None;
        }
        let inv = Self::from_cols(tmp0 * inv_det, tmp1 * inv_det, tmp2 * inv_det).transpose();
        if inv.is_finite() { Some(inv) } else { None }
    }

    /// The inverse, or the identity if the matrix is singular (see [`try_inverse`](Self::try_inverse)).
    #[inline]
    pub fn inverse(&self) -> Self {
        self.try_inverse().unwrap_or(Self::IDENTITY)
    }

    /// `self * v`.
    #[inline]
    pub fn mul_vec3(&self, v: Vec3) -> Vec3 {
        self.x_axis * v.x + self.y_axis * v.y + self.z_axis * v.z
    }

    /// `self * rhs` (applies `rhs` first).
    #[inline]
    pub fn mul_mat3(&self, rhs: &Self) -> Self {
        Self::from_cols(self.mul_vec3(rhs.x_axis), self.mul_vec3(rhs.y_axis), self.mul_vec3(rhs.z_axis))
    }

    /// 2D affine: transforms the point `p` (applies the translation). Assumes
    /// the last row is `(0, 0, 1)`.
    #[inline]
    pub fn transform_point2(&self, p: Vec2) -> Vec2 {
        (self.x_axis * p.x + self.y_axis * p.y + self.z_axis).xy()
    }

    /// 2D affine: transforms the direction `v` (ignores the translation).
    #[inline]
    pub fn transform_vector2(&self, v: Vec2) -> Vec2 {
        (self.x_axis * v.x + self.y_axis * v.y).xy()
    }

    /// Whether every element is finite.
    #[inline]
    pub fn is_finite(&self) -> bool {
        self.x_axis.is_finite() && self.y_axis.is_finite() && self.z_axis.is_finite()
    }

    /// Whether every element differs from `rhs` by at most `max_abs_diff`.
    #[inline]
    pub fn abs_diff_eq(&self, rhs: &Self, max_abs_diff: f32) -> bool {
        self.x_axis.abs_diff_eq(rhs.x_axis, max_abs_diff)
            && self.y_axis.abs_diff_eq(rhs.y_axis, max_abs_diff)
            && self.z_axis.abs_diff_eq(rhs.z_axis, max_abs_diff)
    }
}

impl Mul for Mat3 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        self.mul_mat3(&rhs)
    }
}

impl MulAssign for Mat3 {
    #[inline]
    fn mul_assign(&mut self, rhs: Self) {
        *self = self.mul_mat3(&rhs);
    }
}

impl Mul<Vec3> for Mat3 {
    type Output = Vec3;
    #[inline]
    fn mul(self, rhs: Vec3) -> Vec3 {
        self.mul_vec3(rhs)
    }
}

impl Mul<f32> for Mat3 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f32) -> Self {
        Self::from_cols(self.x_axis * rhs, self.y_axis * rhs, self.z_axis * rhs)
    }
}

impl Mul<Mat3> for f32 {
    type Output = Mat3;
    #[inline]
    fn mul(self, rhs: Mat3) -> Mat3 {
        rhs * self
    }
}

impl Add for Mat3 {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self::from_cols(self.x_axis + rhs.x_axis, self.y_axis + rhs.y_axis, self.z_axis + rhs.z_axis)
    }
}

impl AddAssign for Mat3 {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Mat3 {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self::from_cols(self.x_axis - rhs.x_axis, self.y_axis - rhs.y_axis, self.z_axis - rhs.z_axis)
    }
}

impl SubAssign for Mat3 {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl Neg for Mat3 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self::from_cols(-self.x_axis, -self.y_axis, -self.z_axis)
    }
}

// ---------------------------------------------------------------------------
// Mat4
// ---------------------------------------------------------------------------

/// A 4x4 column-major matrix for 3D transforms and projections (see the
/// [module docs](self) for conventions).
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C)]
pub struct Mat4 {
    pub x_axis: Vec4,
    pub y_axis: Vec4,
    pub z_axis: Vec4,
    pub w_axis: Vec4,
}

impl Default for Mat4 {
    /// The identity matrix.
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mat4 {
    /// All zeros.
    pub const ZERO: Self = Self::from_cols(Vec4::ZERO, Vec4::ZERO, Vec4::ZERO, Vec4::ZERO);
    /// The identity.
    pub const IDENTITY: Self = Self::from_cols(Vec4::X, Vec4::Y, Vec4::Z, Vec4::W);

    /// Creates a matrix from its columns.
    #[inline]
    pub const fn from_cols(x_axis: Vec4, y_axis: Vec4, z_axis: Vec4, w_axis: Vec4) -> Self {
        Self { x_axis, y_axis, z_axis, w_axis }
    }

    /// Creates a matrix from a column-major array.
    #[inline]
    pub const fn from_cols_array(a: &[f32; 16]) -> Self {
        Self::from_cols(
            Vec4::new(a[0], a[1], a[2], a[3]),
            Vec4::new(a[4], a[5], a[6], a[7]),
            Vec4::new(a[8], a[9], a[10], a[11]),
            Vec4::new(a[12], a[13], a[14], a[15]),
        )
    }

    /// The elements in column-major order.
    #[inline]
    pub const fn to_cols_array(&self) -> [f32; 16] {
        let (x, y, z, w) = (self.x_axis, self.y_axis, self.z_axis, self.w_axis);
        [x.x, x.y, x.z, x.w, y.x, y.y, y.z, y.w, z.x, z.y, z.z, z.w, w.x, w.y, w.z, w.w]
    }

    /// Creates a matrix from an array of columns.
    #[inline]
    pub const fn from_cols_array_2d(a: &[[f32; 4]; 4]) -> Self {
        Self::from_cols(Vec4::from_array(a[0]), Vec4::from_array(a[1]), Vec4::from_array(a[2]), Vec4::from_array(a[3]))
    }

    /// The columns as arrays.
    #[inline]
    pub const fn to_cols_array_2d(&self) -> [[f32; 4]; 4] {
        [self.x_axis.to_array(), self.y_axis.to_array(), self.z_axis.to_array(), self.w_axis.to_array()]
    }

    /// A diagonal matrix.
    #[inline]
    pub const fn from_diagonal(d: Vec4) -> Self {
        Self::from_cols(
            Vec4::new(d.x, 0.0, 0.0, 0.0),
            Vec4::new(0.0, d.y, 0.0, 0.0),
            Vec4::new(0.0, 0.0, d.z, 0.0),
            Vec4::new(0.0, 0.0, 0.0, d.w),
        )
    }

    /// Embeds a 3x3 linear transform (no translation).
    #[inline]
    pub const fn from_mat3(m: &Mat3) -> Self {
        Self::from_cols(m.x_axis.extend(0.0), m.y_axis.extend(0.0), m.z_axis.extend(0.0), Vec4::W)
    }

    /// A translation.
    #[inline]
    pub const fn from_translation(translation: Vec3) -> Self {
        Self::from_cols(Vec4::X, Vec4::Y, Vec4::Z, translation.extend(1.0))
    }

    /// A non-uniform scale.
    #[inline]
    pub const fn from_scale(scale: Vec3) -> Self {
        Self::from_diagonal(scale.extend(1.0))
    }

    /// A rotation around the X axis.
    #[inline]
    pub fn from_rotation_x(angle: f32) -> Self {
        Self::from_mat3(&Mat3::from_rotation_x(angle))
    }

    /// A rotation around the Y axis.
    #[inline]
    pub fn from_rotation_y(angle: f32) -> Self {
        Self::from_mat3(&Mat3::from_rotation_y(angle))
    }

    /// A rotation around the Z axis.
    #[inline]
    pub fn from_rotation_z(angle: f32) -> Self {
        Self::from_mat3(&Mat3::from_rotation_z(angle))
    }

    /// A rotation of `angle` radians around the unit vector `axis`.
    #[inline]
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Self {
        Self::from_mat3(&Mat3::from_axis_angle(axis, angle))
    }

    /// The rotation matrix of a unit quaternion.
    #[inline]
    pub fn from_quat(rotation: Quat) -> Self {
        Self::from_mat3(&Mat3::from_quat(rotation))
    }

    /// Rotation followed by translation.
    #[inline]
    pub fn from_rotation_translation(rotation: Quat, translation: Vec3) -> Self {
        let r = Mat3::from_quat(rotation);
        Self::from_cols(r.x_axis.extend(0.0), r.y_axis.extend(0.0), r.z_axis.extend(0.0), translation.extend(1.0))
    }

    /// Scale, then rotation, then translation (the usual model matrix).
    #[inline]
    pub fn from_scale_rotation_translation(scale: Vec3, rotation: Quat, translation: Vec3) -> Self {
        let r = Mat3::from_quat(rotation);
        Self::from_cols(
            (r.x_axis * scale.x).extend(0.0),
            (r.y_axis * scale.y).extend(0.0),
            (r.z_axis * scale.z).extend(0.0),
            translation.extend(1.0),
        )
    }

    /// Decomposes an affine transform without shear into `(scale, rotation,
    /// translation)`. A negative determinant is folded into `scale.x`.
    pub fn to_scale_rotation_translation(&self) -> (Vec3, Quat, Vec3) {
        let det = self.determinant();
        let scale = Vec3::new(
            self.x_axis.xyz().length() * m::signum(det),
            self.y_axis.xyz().length(),
            self.z_axis.xyz().length(),
        );
        let inv = scale.recip();
        let rotation = Quat::from_mat3(&Mat3::from_cols(
            self.x_axis.xyz() * inv.x,
            self.y_axis.xyz() * inv.y,
            self.z_axis.xyz() * inv.z,
        ));
        (scale, rotation, self.w_axis.xyz())
    }

    /// A right-handed view matrix for a camera at `eye` looking in direction
    /// `dir` (need not be normalized) with the given `up` direction. The camera
    /// looks down `-Z` in view space.
    #[inline]
    pub fn look_to_rh(eye: Vec3, dir: Vec3, up: Vec3) -> Self {
        let f = dir.normalize();
        let s = f.cross(up).normalize();
        let u = s.cross(f);
        Self::from_cols(
            Vec4::new(s.x, u.x, -f.x, 0.0),
            Vec4::new(s.y, u.y, -f.y, 0.0),
            Vec4::new(s.z, u.z, -f.z, 0.0),
            Vec4::new(-eye.dot(s), -eye.dot(u), eye.dot(f), 1.0),
        )
    }

    /// A right-handed view matrix for a camera at `eye` looking at `center`.
    /// `up` must not be parallel to the view direction.
    #[inline]
    pub fn look_at_rh(eye: Vec3, center: Vec3, up: Vec3) -> Self {
        Self::look_to_rh(eye, center - eye, up)
    }

    /// A right-handed perspective projection with OpenGL depth: view-space
    /// `z = -z_near` maps to NDC `z = -1` and `z = -z_far` to `+1`.
    /// `fov_y` is the vertical field of view in radians, `aspect = width / height`.
    #[inline]
    pub fn perspective_rh(fov_y: f32, aspect: f32, z_near: f32, z_far: f32) -> Self {
        let (s, c) = m::sin_cos(0.5 * fov_y);
        let h = c / s;
        let w = h / aspect;
        let r = 1.0 / (z_near - z_far);
        Self::from_cols(
            Vec4::new(w, 0.0, 0.0, 0.0),
            Vec4::new(0.0, h, 0.0, 0.0),
            Vec4::new(0.0, 0.0, (z_far + z_near) * r, -1.0),
            Vec4::new(0.0, 0.0, 2.0 * z_far * z_near * r, 0.0),
        )
    }

    /// A right-handed perspective projection with `[0, 1]` depth: view-space
    /// `z = -z_near` maps to NDC `z = 0` and `z = -z_far` to `1`.
    #[inline]
    pub fn perspective_rh_zo(fov_y: f32, aspect: f32, z_near: f32, z_far: f32) -> Self {
        let (s, c) = m::sin_cos(0.5 * fov_y);
        let h = c / s;
        let w = h / aspect;
        let r = z_far / (z_near - z_far);
        Self::from_cols(
            Vec4::new(w, 0.0, 0.0, 0.0),
            Vec4::new(0.0, h, 0.0, 0.0),
            Vec4::new(0.0, 0.0, r, -1.0),
            Vec4::new(0.0, 0.0, r * z_near, 0.0),
        )
    }

    /// A right-handed orthographic projection with OpenGL depth: the box
    /// `[left, right] x [bottom, top] x [-near, -far]` maps to `[-1, 1]^3`.
    #[inline]
    pub fn orthographic_rh(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> Self {
        let rw = 1.0 / (right - left);
        let rh = 1.0 / (top - bottom);
        let rd = 1.0 / (far - near);
        Self::from_cols(
            Vec4::new(2.0 * rw, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 * rh, 0.0, 0.0),
            Vec4::new(0.0, 0.0, -2.0 * rd, 0.0),
            Vec4::new(-(right + left) * rw, -(top + bottom) * rh, -(far + near) * rd, 1.0),
        )
    }

    /// A right-handed orthographic projection with `[0, 1]` depth (`z = -near`
    /// maps to 0, `z = -far` to 1).
    #[inline]
    pub fn orthographic_rh_zo(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> Self {
        let rw = 1.0 / (right - left);
        let rh = 1.0 / (top - bottom);
        let r = 1.0 / (near - far);
        Self::from_cols(
            Vec4::new(2.0 * rw, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 * rh, 0.0, 0.0),
            Vec4::new(0.0, 0.0, r, 0.0),
            Vec4::new(-(right + left) * rw, -(top + bottom) * rh, r * near, 1.0),
        )
    }

    /// Column `i` (panics if `i > 3`).
    #[inline]
    pub fn col(&self, i: usize) -> Vec4 {
        match i {
            0 => self.x_axis,
            1 => self.y_axis,
            2 => self.z_axis,
            3 => self.w_axis,
            _ => panic!("Mat4 column index out of bounds"),
        }
    }

    /// Mutable column `i` (panics if `i > 3`).
    #[inline]
    pub fn col_mut(&mut self, i: usize) -> &mut Vec4 {
        match i {
            0 => &mut self.x_axis,
            1 => &mut self.y_axis,
            2 => &mut self.z_axis,
            3 => &mut self.w_axis,
            _ => panic!("Mat4 column index out of bounds"),
        }
    }

    /// Row `i` (panics if `i > 3`).
    #[inline]
    pub fn row(&self, i: usize) -> Vec4 {
        Vec4::new(self.x_axis[i], self.y_axis[i], self.z_axis[i], self.w_axis[i])
    }

    /// The transpose.
    #[inline]
    pub fn transpose(&self) -> Self {
        let (x, y, z, w) = (self.x_axis, self.y_axis, self.z_axis, self.w_axis);
        Self::from_cols(
            Vec4::new(x.x, y.x, z.x, w.x),
            Vec4::new(x.y, y.y, z.y, w.y),
            Vec4::new(x.z, y.z, z.z, w.z),
            Vec4::new(x.w, y.w, z.w, w.w),
        )
    }

    /// The 2x2 sub-determinants used by [`determinant`](Self::determinant) and
    /// [`try_inverse`](Self::try_inverse) (Laplace expansion by complementary minors).
    #[inline(always)]
    fn minors(&self) -> ([f32; 6], [f32; 6]) {
        let (c0, c1, c2, c3) = (self.x_axis, self.y_axis, self.z_axis, self.w_axis);
        // aRC = row R, column C.
        let (a00, a10, a20, a30) = (c0.x, c0.y, c0.z, c0.w);
        let (a01, a11, a21, a31) = (c1.x, c1.y, c1.z, c1.w);
        let (a02, a12, a22, a32) = (c2.x, c2.y, c2.z, c2.w);
        let (a03, a13, a23, a33) = (c3.x, c3.y, c3.z, c3.w);
        let s = [
            a00 * a11 - a10 * a01,
            a00 * a12 - a10 * a02,
            a00 * a13 - a10 * a03,
            a01 * a12 - a11 * a02,
            a01 * a13 - a11 * a03,
            a02 * a13 - a12 * a03,
        ];
        let c = [
            a20 * a31 - a30 * a21,
            a20 * a32 - a30 * a22,
            a20 * a33 - a30 * a23,
            a21 * a32 - a31 * a22,
            a21 * a33 - a31 * a23,
            a22 * a33 - a32 * a23,
        ];
        (s, c)
    }

    /// The determinant.
    #[inline]
    pub fn determinant(&self) -> f32 {
        let (s, c) = self.minors();
        s[0] * c[5] - s[1] * c[4] + s[2] * c[3] + s[3] * c[2] - s[4] * c[1] + s[5] * c[0]
    }

    /// The general inverse, or `None` if the matrix is singular or not finite.
    pub fn try_inverse(&self) -> Option<Self> {
        let (s, c) = self.minors();
        let det = s[0] * c[5] - s[1] * c[4] + s[2] * c[3] + s[3] * c[2] - s[4] * c[1] + s[5] * c[0];
        let inv = 1.0 / det;
        if det == 0.0 || !inv.is_finite() {
            return None;
        }
        let (c0, c1, c2, c3) = (self.x_axis, self.y_axis, self.z_axis, self.w_axis);
        let (a00, a10, a20, a30) = (c0.x, c0.y, c0.z, c0.w);
        let (a01, a11, a21, a31) = (c1.x, c1.y, c1.z, c1.w);
        let (a02, a12, a22, a32) = (c2.x, c2.y, c2.z, c2.w);
        let (a03, a13, a23, a33) = (c3.x, c3.y, c3.z, c3.w);
        // bRC = row R, column C of the inverse.
        let b00 = (a11 * c[5] - a12 * c[4] + a13 * c[3]) * inv;
        let b01 = (-a01 * c[5] + a02 * c[4] - a03 * c[3]) * inv;
        let b02 = (a31 * s[5] - a32 * s[4] + a33 * s[3]) * inv;
        let b03 = (-a21 * s[5] + a22 * s[4] - a23 * s[3]) * inv;
        let b10 = (-a10 * c[5] + a12 * c[2] - a13 * c[1]) * inv;
        let b11 = (a00 * c[5] - a02 * c[2] + a03 * c[1]) * inv;
        let b12 = (-a30 * s[5] + a32 * s[2] - a33 * s[1]) * inv;
        let b13 = (a20 * s[5] - a22 * s[2] + a23 * s[1]) * inv;
        let b20 = (a10 * c[4] - a11 * c[2] + a13 * c[0]) * inv;
        let b21 = (-a00 * c[4] + a01 * c[2] - a03 * c[0]) * inv;
        let b22 = (a30 * s[4] - a31 * s[2] + a33 * s[0]) * inv;
        let b23 = (-a20 * s[4] + a21 * s[2] - a23 * s[0]) * inv;
        let b30 = (-a10 * c[3] + a11 * c[1] - a12 * c[0]) * inv;
        let b31 = (a00 * c[3] - a01 * c[1] + a02 * c[0]) * inv;
        let b32 = (-a30 * s[3] + a31 * s[1] - a32 * s[0]) * inv;
        let b33 = (a20 * s[3] - a21 * s[1] + a22 * s[0]) * inv;
        let r = Self::from_cols(
            Vec4::new(b00, b10, b20, b30),
            Vec4::new(b01, b11, b21, b31),
            Vec4::new(b02, b12, b22, b32),
            Vec4::new(b03, b13, b23, b33),
        );
        if r.is_finite() { Some(r) } else { None }
    }

    /// The general inverse, or the identity if the matrix is singular (see
    /// [`try_inverse`](Self::try_inverse)).
    #[inline]
    pub fn inverse(&self) -> Self {
        self.try_inverse().unwrap_or(Self::IDENTITY)
    }

    /// Faster inverse for affine transforms (last row `(0, 0, 0, 1)`), e.g.
    /// view and model matrices; `None` if the linear part is singular.
    pub fn try_affine_inverse(&self) -> Option<Self> {
        let inv3 = Mat3::from_mat4(self).try_inverse()?;
        let t = -(inv3 * self.w_axis.xyz());
        Some(Self::from_cols(inv3.x_axis.extend(0.0), inv3.y_axis.extend(0.0), inv3.z_axis.extend(0.0), t.extend(1.0)))
    }

    /// `self * v`.
    #[inline]
    pub fn mul_vec4(&self, v: Vec4) -> Vec4 {
        self.x_axis * v.x + self.y_axis * v.y + self.z_axis * v.z + self.w_axis * v.w
    }

    /// `self * rhs` (applies `rhs` first).
    #[inline]
    pub fn mul_mat4(&self, rhs: &Self) -> Self {
        Self::from_cols(
            self.mul_vec4(rhs.x_axis),
            self.mul_vec4(rhs.y_axis),
            self.mul_vec4(rhs.z_axis),
            self.mul_vec4(rhs.w_axis),
        )
    }

    /// Transforms the point `p` (`w = 1`) including the perspective divide by
    /// the resulting `w` (non-finite if that `w` is 0).
    #[inline]
    pub fn transform_point3(&self, p: Vec3) -> Vec3 {
        let r = self.mul_vec4(p.extend(1.0));
        r.xyz() / r.w
    }

    /// Transforms the point `p` assuming an affine matrix (no perspective divide).
    #[inline]
    pub fn transform_point3_affine(&self, p: Vec3) -> Vec3 {
        self.x_axis.xyz() * p.x + self.y_axis.xyz() * p.y + self.z_axis.xyz() * p.z + self.w_axis.xyz()
    }

    /// Transforms the direction `v` (`w = 0`: the translation is ignored).
    #[inline]
    pub fn transform_vector3(&self, v: Vec3) -> Vec3 {
        self.x_axis.xyz() * v.x + self.y_axis.xyz() * v.y + self.z_axis.xyz() * v.z
    }

    /// Whether every element is finite.
    #[inline]
    pub fn is_finite(&self) -> bool {
        self.x_axis.is_finite() && self.y_axis.is_finite() && self.z_axis.is_finite() && self.w_axis.is_finite()
    }

    /// Whether every element differs from `rhs` by at most `max_abs_diff`.
    #[inline]
    pub fn abs_diff_eq(&self, rhs: &Self, max_abs_diff: f32) -> bool {
        self.x_axis.abs_diff_eq(rhs.x_axis, max_abs_diff)
            && self.y_axis.abs_diff_eq(rhs.y_axis, max_abs_diff)
            && self.z_axis.abs_diff_eq(rhs.z_axis, max_abs_diff)
            && self.w_axis.abs_diff_eq(rhs.w_axis, max_abs_diff)
    }
}

impl Mul for Mat4 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        self.mul_mat4(&rhs)
    }
}

impl MulAssign for Mat4 {
    #[inline]
    fn mul_assign(&mut self, rhs: Self) {
        *self = self.mul_mat4(&rhs);
    }
}

impl Mul<Vec4> for Mat4 {
    type Output = Vec4;
    #[inline]
    fn mul(self, rhs: Vec4) -> Vec4 {
        self.mul_vec4(rhs)
    }
}

impl Mul<f32> for Mat4 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f32) -> Self {
        Self::from_cols(self.x_axis * rhs, self.y_axis * rhs, self.z_axis * rhs, self.w_axis * rhs)
    }
}

impl Mul<Mat4> for f32 {
    type Output = Mat4;
    #[inline]
    fn mul(self, rhs: Mat4) -> Mat4 {
        rhs * self
    }
}

impl Add for Mat4 {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self::from_cols(
            self.x_axis + rhs.x_axis,
            self.y_axis + rhs.y_axis,
            self.z_axis + rhs.z_axis,
            self.w_axis + rhs.w_axis,
        )
    }
}

impl AddAssign for Mat4 {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Mat4 {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self::from_cols(
            self.x_axis - rhs.x_axis,
            self.y_axis - rhs.y_axis,
            self.z_axis - rhs.z_axis,
            self.w_axis - rhs.w_axis,
        )
    }
}

impl SubAssign for Mat4 {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl Neg for Mat4 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self::from_cols(-self.x_axis, -self.y_axis, -self.z_axis, -self.w_axis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rng;
    use core::f32::consts::{FRAC_PI_2, FRAC_PI_3, PI};

    fn random_mat4(rng: &mut Rng) -> Mat4 {
        let mut a = [0.0f32; 16];
        for v in &mut a {
            *v = rng.range_f32(-2.0, 2.0);
        }
        Mat4::from_cols_array(&a)
    }

    fn random_transform(rng: &mut Rng) -> Mat4 {
        let scale = Vec3::new(rng.range_f32(0.2, 3.0), rng.range_f32(0.2, 3.0), rng.range_f32(0.2, 3.0));
        let rot = Quat::from_axis_angle(rng.unit_vec3(), rng.range_f32(-PI, PI));
        let t = Vec3::new(rng.range_f32(-10.0, 10.0), rng.range_f32(-10.0, 10.0), rng.range_f32(-10.0, 10.0));
        Mat4::from_scale_rotation_translation(scale, rot, t)
    }

    #[test]
    fn layout_and_arrays() {
        assert_eq!(core::mem::size_of::<Mat3>(), 36);
        assert_eq!(core::mem::size_of::<Mat4>(), 64);
        let a: [f32; 16] = core::array::from_fn(|i| i as f32);
        let m = Mat4::from_cols_array(&a);
        assert_eq!(m.to_cols_array(), a);
        assert_eq!(m.x_axis, Vec4::new(0.0, 1.0, 2.0, 3.0));
        assert_eq!(m.row(1), Vec4::new(1.0, 5.0, 9.0, 13.0));
        assert_eq!(m.col(3), Vec4::new(12.0, 13.0, 14.0, 15.0));
        assert_eq!(Mat4::from_cols_array_2d(&m.to_cols_array_2d()), m);
        assert_eq!(m.transpose().transpose(), m);
        assert_eq!(m.transpose().row(1), m.col(1));
        let b: [f32; 9] = core::array::from_fn(|i| i as f32);
        let m3 = Mat3::from_cols_array(&b);
        assert_eq!(m3.to_cols_array(), b);
        assert_eq!(Mat3::from_cols_array_2d(&m3.to_cols_array_2d()), m3);
        assert_eq!(m3.row(2), Vec3::new(2.0, 5.0, 8.0));
        assert_eq!(Mat4::default(), Mat4::IDENTITY);
        assert_eq!(Mat3::default(), Mat3::IDENTITY);
    }

    #[test]
    fn multiplication_order() {
        // Translate after scaling: T * S applies S first.
        let t = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let s = Mat4::from_scale(Vec3::splat(2.0));
        let p = Vec3::new(1.0, 1.0, 1.0);
        assert_eq!((t * s).transform_point3(p), Vec3::new(3.0, 4.0, 5.0));
        assert_eq!((s * t).transform_point3(p), Vec3::new(4.0, 6.0, 8.0));
        assert_eq!(t.transform_vector3(p), p);
        assert_eq!(t.transform_point3_affine(p), Vec3::new(2.0, 3.0, 4.0));
        assert_eq!(Mat4::IDENTITY * t, t);
        let mut acc = Mat4::IDENTITY;
        acc *= t;
        acc *= s;
        assert_eq!(acc, t * s);
        assert_eq!(t * Vec4::new(0.0, 0.0, 0.0, 1.0), Vec4::new(1.0, 2.0, 3.0, 1.0));
        assert_eq!((t + t - t), t);
        assert_eq!(-(-t), t);
        assert_eq!((2.0 * Mat4::IDENTITY).determinant(), 16.0);
    }

    #[test]
    fn rotations_follow_right_hand_rule() {
        let close = |a: Vec3, b: Vec3| a.abs_diff_eq(b, 1e-6);
        assert!(close(Mat3::from_rotation_x(FRAC_PI_2) * Vec3::Y, Vec3::Z));
        assert!(close(Mat3::from_rotation_y(FRAC_PI_2) * Vec3::Z, Vec3::X));
        assert!(close(Mat3::from_rotation_z(FRAC_PI_2) * Vec3::X, Vec3::Y));
        assert!(close(Mat4::from_rotation_z(FRAC_PI_2).transform_vector3(Vec3::X), Vec3::Y));
        for (axis, m) in [
            (Vec3::X, Mat3::from_rotation_x(0.7)),
            (Vec3::Y, Mat3::from_rotation_y(0.7)),
            (Vec3::Z, Mat3::from_rotation_z(0.7)),
        ] {
            assert!(Mat3::from_axis_angle(axis, 0.7).abs_diff_eq(&m, 1e-6));
            assert!(Mat3::from_quat(Quat::from_axis_angle(axis, 0.7)).abs_diff_eq(&m, 1e-6));
            assert!((m.determinant() - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn inverse_times_matrix_is_identity() {
        let mut rng = Rng::new(10);
        let mut tested = 0;
        for _ in 0..2000 {
            let m = random_mat4(&mut rng);
            // Skip badly conditioned matrices.
            if m.determinant().abs() < 0.05 {
                continue;
            }
            let inv = m.try_inverse().expect("invertible");
            assert!((inv * m).abs_diff_eq(&Mat4::IDENTITY, 2e-3), "{m:?}");
            assert!((m * inv).abs_diff_eq(&Mat4::IDENTITY, 2e-3), "{m:?}");
            assert!((inv.determinant() * m.determinant() - 1.0).abs() < 1e-3);
            tested += 1;
        }
        assert!(tested > 1000);
        for _ in 0..1000 {
            let m = random_transform(&mut rng);
            let inv = m.inverse();
            assert!((inv * m).abs_diff_eq(&Mat4::IDENTITY, 1e-4));
            let affine = m.try_affine_inverse().unwrap();
            assert!(affine.abs_diff_eq(&inv, 1e-4));
            let m3 = Mat3::from_mat4(&m);
            assert!((m3.inverse() * m3).abs_diff_eq(&Mat3::IDENTITY, 1e-5));
        }
    }

    #[test]
    fn singular_matrices() {
        assert_eq!(Mat4::ZERO.try_inverse(), None);
        assert_eq!(Mat4::ZERO.inverse(), Mat4::IDENTITY);
        let rank3 = Mat4::from_scale(Vec3::new(1.0, 0.0, 1.0));
        assert_eq!(rank3.try_inverse(), None);
        assert_eq!(rank3.try_affine_inverse(), None);
        assert_eq!(Mat3::ZERO.try_inverse(), None);
        assert_eq!(Mat3::from_diagonal(Vec3::new(1.0, 2.0, 0.0)).inverse(), Mat3::IDENTITY);
        let nan = Mat4::from_scale(Vec3::splat(f32::NAN));
        assert_eq!(nan.try_inverse(), None);
        assert_eq!(Mat4::from_scale(Vec3::splat(1e-30)).try_inverse(), None); // det underflows
    }

    #[test]
    fn decompose_round_trip() {
        let mut rng = Rng::new(11);
        for _ in 0..1000 {
            let scale = Vec3::new(rng.range_f32(0.2, 3.0), rng.range_f32(0.2, 3.0), rng.range_f32(0.2, 3.0));
            let rot = Quat::from_axis_angle(rng.unit_vec3(), rng.range_f32(-PI, PI));
            let t = Vec3::new(rng.range_f32(-10.0, 10.0), rng.range_f32(-10.0, 10.0), rng.range_f32(-10.0, 10.0));
            let m = Mat4::from_scale_rotation_translation(scale, rot, t);
            let (s2, r2, t2) = m.to_scale_rotation_translation();
            assert!(s2.abs_diff_eq(scale, 1e-4), "{s2:?} {scale:?}");
            assert!(r2.dot(rot).abs() > 1.0 - 1e-5, "{r2:?} {rot:?}");
            assert!(t2.abs_diff_eq(t, 1e-5));
            assert!(Mat4::from_scale_rotation_translation(s2, r2, t2).abs_diff_eq(&m, 1e-4));
        }
    }

    #[test]
    fn look_at_is_orthonormal_and_maps_eye_to_origin() {
        let mut rng = Rng::new(12);
        for _ in 0..1000 {
            let eye = Vec3::new(rng.range_f32(-50.0, 50.0), rng.range_f32(-50.0, 50.0), rng.range_f32(-50.0, 50.0));
            let center = eye + rng.unit_vec3() * rng.range_f32(0.5, 20.0);
            let dir = (center - eye).normalize();
            if dir.dot(Vec3::Y).abs() > 0.99 {
                continue;
            }
            let view = Mat4::look_at_rh(eye, center, Vec3::Y);
            let r = Mat3::from_mat4(&view);
            assert!((r * r.transpose()).abs_diff_eq(&Mat3::IDENTITY, 1e-5));
            assert!((r.determinant() - 1.0).abs() < 1e-5);
            assert!(view.transform_point3(eye).abs_diff_eq(Vec3::ZERO, 1e-3));
            // The view direction maps to -Z, up stays in the upper half.
            assert!(view.transform_vector3(dir).abs_diff_eq(Vec3::NEG_Z, 1e-5));
            assert!(view.transform_vector3(Vec3::Y).y > 0.0);
            let target = view.transform_point3(center);
            assert!(target.z < 0.0 && target.x.abs() < 1e-3 && target.y.abs() < 1e-3);
            assert!(Mat4::look_to_rh(eye, dir * 3.0, Vec3::Y).abs_diff_eq(&view, 1e-4));
        }
        // Camera at +Z looking at the origin: identity rotation.
        let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        assert!(view.abs_diff_eq(&Mat4::from_translation(Vec3::new(0.0, 0.0, -5.0)), 1e-6));
    }

    #[test]
    fn perspective_depth_ranges() {
        let (near, far) = (0.1, 100.0);
        let gl = Mat4::perspective_rh(FRAC_PI_3, 16.0 / 9.0, near, far);
        let zo = Mat4::perspective_rh_zo(FRAC_PI_3, 16.0 / 9.0, near, far);
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(close(gl.transform_point3(Vec3::new(0.0, 0.0, -near)).z, -1.0));
        assert!(close(gl.transform_point3(Vec3::new(0.0, 0.0, -far)).z, 1.0));
        assert!(close(zo.transform_point3(Vec3::new(0.0, 0.0, -near)).z, 0.0));
        assert!(close(zo.transform_point3(Vec3::new(0.0, 0.0, -far)).z, 1.0));
        // Depth increases monotonically with distance.
        let mut prev = -2.0;
        for i in 1..100 {
            let z = zo.transform_point3(Vec3::new(0.0, 0.0, -(near + i as f32))).z;
            assert!(z > prev && (0.0..=1.0).contains(&z));
            prev = z;
        }
        // The top edge of the frustum maps to y = 1, the right edge to x = 1.
        let d = 10.0;
        let half_h = d * (FRAC_PI_3 / 2.0).tan();
        let top = gl.transform_point3(Vec3::new(0.0, half_h, -d));
        let right = gl.transform_point3(Vec3::new(half_h * 16.0 / 9.0, 0.0, -d));
        assert!(close(top.y, 1.0) && close(right.x, 1.0));
        // Clip-space w is the view-space distance.
        assert!(close((zo * Vec4::new(1.0, 2.0, -7.0, 1.0)).w, 7.0));
    }

    #[test]
    fn orthographic_depth_ranges() {
        let gl = Mat4::orthographic_rh(-4.0, 4.0, -3.0, 3.0, 1.0, 11.0);
        let zo = Mat4::orthographic_rh_zo(-4.0, 4.0, -3.0, 3.0, 1.0, 11.0);
        let close = |a: Vec3, b: Vec3| a.abs_diff_eq(b, 1e-6);
        assert!(close(gl.transform_point3(Vec3::new(-4.0, -3.0, -1.0)), Vec3::new(-1.0, -1.0, -1.0)));
        assert!(close(gl.transform_point3(Vec3::new(4.0, 3.0, -11.0)), Vec3::new(1.0, 1.0, 1.0)));
        assert!(close(zo.transform_point3(Vec3::new(-4.0, -3.0, -1.0)), Vec3::new(-1.0, -1.0, 0.0)));
        assert!(close(zo.transform_point3(Vec3::new(4.0, 3.0, -11.0)), Vec3::new(1.0, 1.0, 1.0)));
    }

    #[test]
    fn mat3_affine_2d() {
        let m = Mat3::from_scale_angle_translation(Vec2::new(2.0, 3.0), FRAC_PI_2, Vec2::new(10.0, 20.0));
        let expected = Mat3::from_translation(Vec2::new(10.0, 20.0))
            * Mat3::from_angle(FRAC_PI_2)
            * Mat3::from_scale(Vec2::new(2.0, 3.0));
        assert!(m.abs_diff_eq(&expected, 1e-6));
        assert!(m.transform_point2(Vec2::new(1.0, 0.0)).abs_diff_eq(Vec2::new(10.0, 22.0), 1e-5));
        assert!(m.transform_vector2(Vec2::new(1.0, 0.0)).abs_diff_eq(Vec2::new(0.0, 2.0), 1e-5));
        assert!((m.inverse() * m).abs_diff_eq(&Mat3::IDENTITY, 1e-5));
        let p = m.inverse().transform_point2(m.transform_point2(Vec2::new(3.0, -4.0)));
        assert!(p.abs_diff_eq(Vec2::new(3.0, -4.0), 1e-4));
    }
}
