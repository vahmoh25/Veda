//! The perspective camera and view-frustum tests.

#[allow(unused_imports)] // host tests link std, whose inherent float methods win
use vmath::{FloatExt, Mat4, Vec3, Vec4};

/// A perspective camera looking along `forward` (right-handed, +Y up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// Eye position in world space.
    pub position: Vec3,
    /// Unit view direction.
    pub forward: Vec3,
    /// Approximate up direction (orthogonalised against `forward`).
    pub up: Vec3,
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Distance of the near clipping plane (geometry closer is clipped).
    pub near: f32,
    /// Distance of the far plane (objects beyond it are culled).
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Camera {
        Camera {
            position: Vec3::new(0.0, 0.0, 5.0),
            forward: Vec3::NEG_Z,
            up: Vec3::Y,
            fov_y: 1.0,
            near: 0.1,
            far: 1000.0,
        }
    }
}

impl Camera {
    /// A camera at `eye` looking at `target`.
    pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Camera {
        Camera { position: eye, forward: (target - eye).normalize_or(Vec3::NEG_Z), up, ..Camera::default() }
    }

    /// Unit right vector.
    pub fn right(&self) -> Vec3 {
        let r = self.forward.cross(self.up);
        r.try_normalize().unwrap_or_else(|| self.forward.any_orthonormal_vector())
    }

    /// Unit up vector, orthogonal to `forward`.
    pub fn true_up(&self) -> Vec3 {
        self.right().cross(self.forward).normalize_or(Vec3::Y)
    }

    /// World to view space.
    pub fn view(&self) -> Mat4 {
        Mat4::look_to_rh(self.position, self.forward, self.true_up())
    }

    /// View to clip space for a viewport with the given aspect ratio.
    pub fn projection(&self, aspect: f32) -> Mat4 {
        Mat4::perspective_rh_zo(self.fov_y, aspect.max(1e-3), self.near.max(1e-4), self.far.max(self.near + 1e-3))
    }

    /// The view frustum in world space.
    pub fn frustum(&self, aspect: f32) -> Frustum {
        Frustum::from_matrix(&(self.projection(aspect) * self.view()))
    }

    /// Projects a world point to normalised device coordinates (x, y in
    /// -1..1, y up); `None` behind the camera.
    pub fn project(&self, p: Vec3, aspect: f32) -> Option<Vec3> {
        let clip = (self.projection(aspect) * self.view()) * p.extend(1.0);
        if clip.w <= self.near * 0.5 {
            return None;
        }
        Some(clip.xyz() / clip.w)
    }

    /// The direction of the view ray through normalised device coordinates.
    pub fn ray(&self, ndc_x: f32, ndc_y: f32, aspect: f32) -> Vec3 {
        let t = (self.fov_y * 0.5).tan();
        (self.forward + self.right() * (ndc_x * t * aspect) + self.true_up() * (ndc_y * t)).normalize_or(self.forward)
    }
}

/// Six inward-facing planes (`n.x * x + n.y * y + n.z * z + d >= 0` inside).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frustum {
    /// Left, right, bottom, top, near and far planes as `(n, d)`.
    pub planes: [Vec4; 6],
}

impl Frustum {
    /// Extracts the planes of a view-projection matrix with `[0, 1]` depth.
    pub fn from_matrix(m: &Mat4) -> Frustum {
        let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
        let mut planes = [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2];
        for p in &mut planes {
            let len = p.xyz().length();
            if len > 0.0 {
                *p /= len;
            }
        }
        Frustum { planes }
    }

    /// True if the sphere is at least partly inside.
    pub fn intersects_sphere(&self, center: Vec3, radius: f32) -> bool {
        self.planes.iter().all(|p| p.xyz().dot(center) + p.w >= -radius)
    }
}
