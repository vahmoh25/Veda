//! Vertex transformation, lighting, fog and projection.
//!
//! Per vertex this is a 3x4 matrix multiply, a few 2.14 dot products for
//! the lights and two integer divisions for the projection: all integer
//! work, which is ~20x cheaper than the equivalent float code under TCG.

use super::{
    FxVertex, OC_BOTTOM, OC_FAR, OC_GUARD, OC_LEFT, OC_NEAR, OC_RIGHT, OC_TOP, TVert, XF_FOG, XF_FOG_FADE, XF_LIT,
    XF_SPECULAR, XF_TWO_SIDED, XF_VCOLOR, Xform,
};

/// Replaces the contents of `out` with the transformed and lit vertices of
/// `input`.
pub fn transform(xf: &Xform, input: &[FxVertex], out: &mut alloc::vec::Vec<TVert>) {
    let m = xf.mvp.map(|v| v as i64);
    let (t0, t1, t3) = (m[3] << 16, m[7] << 16, m[15] << 16);
    const LIM: i64 = 1 << 46;
    let row = |a: i64, b: i64, c: i64, t: i64, x: i64, y: i64, z: i64| {
        let v = a.wrapping_mul(x).wrapping_add(b.wrapping_mul(y)).wrapping_add(c.wrapping_mul(z)).wrapping_add(t);
        (v.clamp(-LIM, LIM) >> 16) as i32
    };
    out.clear();
    out.extend(input.iter().map(|v| {
        let (x, y, z) = (v.x as i64, v.y as i64, v.z as i64);
        let cx = row(m[0], m[1], m[2], t0, x, y, z);
        let cy = row(m[4], m[5], m[6], t1, x, y, z);
        let cw = row(m[12], m[13], m[14], t3, x, y, z);
        let (sx, sy, q, outcode) = projection(xf, cx, cy, cw);
        let (mul, add) = shade(xf, v, cw);
        TVert { cx, cy, cw, sx, sy, q, u: v.u, v: v.v, mul, add, outcode }
    }));
}

/// Computes the outcode and, in front of the near plane, the screen
/// position and depth of a clip-space vertex.
pub fn project(xf: &Xform, v: &mut TVert) {
    (v.sx, v.sy, v.q, v.outcode) = projection(xf, v.cx, v.cy, v.cw);
}

/// The screen position (28.4), depth and outcode of clip-space position
/// (cx, cy, w); the position and depth are 0 behind the near plane.
#[inline(always)]
fn projection(xf: &Xform, cx: i32, cy: i32, w: i32) -> (i32, i32, i32, u16) {
    let mut oc = 0;
    if w < xf.near_w {
        oc |= OC_NEAR;
    }
    if w > xf.far_w {
        oc |= OC_FAR;
    }
    if cx < w.wrapping_neg() {
        oc |= OC_LEFT;
    }
    if cx > w {
        oc |= OC_RIGHT;
    }
    if cy < w.wrapping_neg() {
        oc |= OC_BOTTOM;
    }
    if cy > w {
        oc |= OC_TOP;
    }
    let gw = (w.max(0) as i64 * xf.guard as i64) >> 16;
    let (cx, cy) = (cx as i64, cy as i64);
    if cx < -gw || cx > gw || cy < -gw || cy > gw {
        oc |= OC_GUARD;
    }
    if oc & OC_NEAR != 0 {
        return (0, 0, 0, oc);
    }
    let w = w as i64;
    let sx = xf.vp_x.wrapping_add((cx * xf.vp_sx as i64 / w) as i32);
    let sy = xf.vp_y.wrapping_sub((cy * xf.vp_sy as i64 / w) as i32);
    (sx, sy, (xf.q_scale / w).min(0x7FFF_FFFF) as i32, oc)
}

/// A 2.14 dot product.
#[inline(always)]
fn dot14(x: i32, y: i32, z: i32, d: &[i32; 4]) -> i32 {
    (x * d[0] + y * d[1] + z * d[2]) >> 14
}

/// Integer square root of a non-negative value (one hardware square root
/// is cheaper than a bitwise loop under TCG; the rounding is corrected).
fn isqrt(v: i64) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut r = vmath::f64::sqrt(v as f64) as i64;
    while r > 0 && r * r > v {
        r -= 1;
    }
    while (r + 1) * (r + 1) <= v {
        r += 1;
    }
    r
}

/// Lighting, vertex colour, specular, emission and fog for one vertex at
/// view depth `w`: the colour multipliers and the additive colour.
#[inline(always)]
fn shade(xf: &Xform, v: &FxVertex, w: i32) -> ([u16; 4], [u16; 3]) {
    let flags = xf.flags;
    let (mut r, mut g, mut b);
    let [mut ar, mut ag, mut ab, _] = xf.emit_rgb;
    if flags & XF_LIT != 0 {
        let (nx, ny, nz) = (v.nx as i32, v.ny as i32, v.nz as i32);
        let ndl = dot14(nx, ny, nz, &xf.light_dir);
        let ndl = if flags & XF_TWO_SIDED != 0 { ndl.wrapping_abs() } else { ndl.max(0) };
        // How much the normal points up: 0..16384.
        let t = (dot14(nx, ny, nz, &xf.up_dir) + 16384) >> 1;
        let ambient = |k: usize| {
            xf.ground_rgb[k]
                .wrapping_add(xf.sky_rgb[k].wrapping_sub(xf.ground_rgb[k]).wrapping_mul(t) >> 14)
                .wrapping_add(xf.sun_rgb[k].wrapping_mul(ndl) >> 14)
        };
        (r, g, b) = (ambient(0), ambient(1), ambient(2));
        for p in xf.points.iter().take(xf.num_points.max(0) as usize) {
            // Work in 8.8 units so that squares fit comfortably in 64 bits.
            let dx = (p.pos[0] as i64 - v.x as i64) >> 8;
            let dy = (p.pos[1] as i64 - v.y as i64) >> 8;
            let dz = (p.pos[2] as i64 - v.z as i64) >> 8;
            let d2 = dx * dx + dy * dy + dz * dz;
            let rr = (p.radius >> 8) as i64;
            let r2 = rr * rr;
            if d2 >= r2 || r2 <= 0 {
                continue;
            }
            let mut ndd = nx as i64 * dx + ny as i64 * dy + nz as i64 * dz; // 2.14 x 8.8
            if flags & XF_TWO_SIDED != 0 {
                ndd = ndd.wrapping_abs();
            } else if ndd <= 0 {
                continue;
            }
            let att = (((r2 - d2) << 14) / r2) as i32; // 1 - d^2/r^2, 2.14
            let att = (att * att) >> 14;
            let len = isqrt(d2); // 8.8
            let lam = if len > 0 { ((ndd / len) as i32).min(16384) } else { 16384 };
            let k = (lam * att) >> 14;
            r = r.wrapping_add(p.rgb[0].wrapping_mul(k) >> 14);
            g = g.wrapping_add(p.rgb[1].wrapping_mul(k) >> 14);
            b = b.wrapping_add(p.rgb[2].wrapping_mul(k) >> 14);
        }
        if flags & XF_SPECULAR != 0 {
            let mut s = dot14(nx, ny, nz, &xf.half_dir);
            if s > 0 {
                for _ in 0..xf.spec_power {
                    s = (s * s) >> 14;
                }
                ar = ar.wrapping_add(xf.spec_rgb[0].wrapping_mul(s) >> 14);
                ag = ag.wrapping_add(xf.spec_rgb[1].wrapping_mul(s) >> 14);
                ab = ab.wrapping_add(xf.spec_rgb[2].wrapping_mul(s) >> 14);
            }
        }
    } else {
        (r, g, b) = (xf.base_rgba[0], xf.base_rgba[1], xf.base_rgba[2]);
    }
    let mut a = xf.base_rgba[3];
    if flags & XF_VCOLOR != 0 {
        let c = v.color;
        // 0..255 -> 0..256, so that 255 multiplies by exactly 1.
        let unit = |shift: u32| {
            let x = ((c >> shift) & 255) as i32;
            x + (x >> 7)
        };
        r = r.wrapping_mul(unit(16)) >> 8;
        g = g.wrapping_mul(unit(8)) >> 8;
        b = b.wrapping_mul(unit(0)) >> 8;
        a = a.wrapping_mul(unit(24)) >> 8;
    }
    if flags & XF_FOG != 0 {
        let f = (w.wrapping_sub(xf.fog_start) as i64 * xf.fog_mul as i64) >> 32;
        let f = if f < 0 {
            0
        } else if f > xf.fog_max as i64 {
            xf.fog_max
        } else {
            f as i32
        };
        let k = 256 - f;
        if flags & XF_FOG_FADE != 0 {
            // Blended materials (glows, smoke) vanish into the fog: fading
            // the alpha fades their premultiplied contribution.
            a = a.wrapping_mul(k) >> 8;
        } else {
            r = r.wrapping_mul(k) >> 8;
            g = g.wrapping_mul(k) >> 8;
            b = b.wrapping_mul(k) >> 8;
            ar = (ar.wrapping_mul(k) >> 8).wrapping_add(xf.fog_rgb[0].wrapping_mul(f) >> 8);
            ag = (ag.wrapping_mul(k) >> 8).wrapping_add(xf.fog_rgb[1].wrapping_mul(f) >> 8);
            ab = (ab.wrapping_mul(k) >> 8).wrapping_add(xf.fog_rgb[2].wrapping_mul(f) >> 8);
        }
    }
    let channel = |v: i32| v.clamp(0, 0xFFFF) as u16;
    ([channel(r), channel(g), channel(b), a.clamp(0, 256) as u16], [channel(ar), channel(ag), channel(ab)])
}
