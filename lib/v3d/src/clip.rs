//! Homogeneous clipping of triangles against the near plane and the guard
//! band, in the integer clip space of the core.
//!
//! Only triangles that cross the near plane or reach far outside the screen
//! (beyond the guard band, where the rasterizer's fixed-point edge functions
//! would overflow) are clipped; everything else is rasterised directly and
//! the edge functions do the screen-edge clipping per pixel.

use crate::ffi::{self, OC_GUARD, OC_NEAR, TVert, Xform};

/// Most vertices a triangle can have after clipping against five planes.
const MAX_POLY: usize = 8;

#[derive(Clone, Copy)]
enum Plane {
    Near,
    Left,
    Right,
    Bottom,
    Top,
}

/// Signed distance to a plane (>= 0 inside), in clip-space units (16.16).
fn distance(p: Plane, xf: &Xform, v: &TVert) -> i64 {
    let w = v.cw as i64;
    let gw = (w.max(0) * xf.guard as i64) >> 16;
    match p {
        Plane::Near => w - xf.near_w as i64,
        Plane::Left => gw + v.cx as i64,
        Plane::Right => gw - v.cx as i64,
        Plane::Bottom => gw + v.cy as i64,
        Plane::Top => gw - v.cy as i64,
    }
}

/// `a + (b - a) * t` with `t` in 0..=65536.
fn lerp(a: &TVert, b: &TVert, t: i64) -> TVert {
    let l = |x: i32, y: i32| (x as i64 + (((y as i64 - x as i64) * t) >> 16)) as i32;
    let lu = |x: u16, y: u16| (x as i64 + (((y as i64 - x as i64) * t) >> 16)).clamp(0, 0xFFFF) as u16;
    TVert {
        cx: l(a.cx, b.cx),
        cy: l(a.cy, b.cy),
        cw: l(a.cw, b.cw),
        sx: 0,
        sy: 0,
        q: 0,
        u: l(a.u, b.u),
        v: l(a.v, b.v),
        mul: [lu(a.mul[0], b.mul[0]), lu(a.mul[1], b.mul[1]), lu(a.mul[2], b.mul[2]), lu(a.mul[3], b.mul[3])],
        add: [lu(a.add[0], b.add[0]), lu(a.add[1], b.add[1]), lu(a.add[2], b.add[2])],
        outcode: 0,
    }
}

/// Clips one polygon against a plane (Sutherland-Hodgman).
fn clip_poly(p: Plane, xf: &Xform, input: &[TVert], out: &mut [TVert; MAX_POLY]) -> usize {
    let mut n = 0;
    let count = input.len();
    for i in 0..count {
        let (a, b) = (&input[i], &input[(i + 1) % count]);
        let (da, db) = (distance(p, xf, a), distance(p, xf, b));
        if da >= 0 && n < MAX_POLY {
            out[n] = *a;
            n += 1;
        }
        if (da >= 0) != (db >= 0) && n < MAX_POLY {
            let t = ((da << 16) / (da - db)).clamp(0, 65536);
            let mut v = lerp(a, b, t);
            // `t` has 16 bits: along edges thousands of viewports long the
            // new vertex can miss the plane by a few pixels. Put it on the
            // plane so that the guard band is a hard bound.
            let gw = ((v.cw.max(0) as i64 * xf.guard as i64) >> 16).min(i32::MAX as i64) as i32;
            match p {
                Plane::Near => v.cw = v.cw.max(xf.near_w),
                Plane::Left => v.cx = v.cx.max(-gw),
                Plane::Right => v.cx = v.cx.min(gw),
                Plane::Bottom => v.cy = v.cy.max(-gw),
                Plane::Top => v.cy = v.cy.min(gw),
            }
            out[n] = v;
            n += 1;
        }
    }
    n
}

/// Clips triangle `a b c` and calls `emit` with each resulting (projected)
/// triangle, preserving the winding.
pub(crate) fn clip_triangle(xf: &Xform, a: &TVert, b: &TVert, c: &TVert, mut emit: impl FnMut(&TVert, &TVert, &TVert)) {
    let or = a.outcode | b.outcode | c.outcode;
    let mut buf_a = [TVert::default(); MAX_POLY];
    let mut buf_b = [TVert::default(); MAX_POLY];
    buf_a[0] = *a;
    buf_a[1] = *b;
    buf_a[2] = *c;
    let mut n = 3;
    let mut cur = &mut buf_a;
    let mut next = &mut buf_b;
    let planes: &[Plane] = match (or & OC_NEAR != 0, or & OC_GUARD != 0) {
        (true, true) => &[Plane::Near, Plane::Left, Plane::Right, Plane::Bottom, Plane::Top],
        (true, false) => &[Plane::Near],
        (false, true) => &[Plane::Left, Plane::Right, Plane::Bottom, Plane::Top],
        (false, false) => &[],
    };
    for &p in planes {
        n = clip_poly(p, xf, &cur[..n], next);
        if n < 3 {
            return;
        }
        core::mem::swap(&mut cur, &mut next);
    }
    for v in cur[..n].iter_mut() {
        // SAFETY: `v` is a valid vertex and `xf` valid parameters.
        unsafe { ffi::v3d_project(xf, v) };
    }
    for i in 1..n - 1 {
        emit(&cur[0], &cur[i], &cur[i + 1]);
    }
}
