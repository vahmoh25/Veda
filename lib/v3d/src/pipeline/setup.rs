//! Triangle setup: culling, the bounding box, edge functions with the
//! top-left fill rule, and fixed-point plane equations for depth, colours
//! and perspective-correct texture coordinates.
//!
//! Screen positions are 28.4 fixed point. A plane a(x, y) is stored as its
//! value at the centre of the reference pixel (rx, ry) plus per-pixel
//! gradients, all with extra fraction bits; the rasteriser evaluates it at
//! the first covered pixel of each span and then steps by the x gradient.

use alloc::vec::Vec;

use super::{
    C_FRAC, CULL_BACK, CULL_FRONT, FxTexture, M_ADD, M_AFFINE, M_ALPHA, M_ATEST, M_BLEND_MASK, M_FLAT, M_TEX, OC_CLIP,
    OC_REJECT, Setup, T_FRAC, TVert, Tri, Z_FRAC, bitlen64, log2q,
};

/// The triangle's geometry, relative to vertex 0.
struct Geo {
    /// Edge vectors from vertex 0 (28.4).
    dx1: i64,
    dy1: i64,
    dx2: i64,
    dy2: i64,
    /// Twice the signed area (> 0).
    area: i64,
    /// Reference pixel centre minus vertex 0 (28.4).
    ox: i64,
    oy: i64,
}

impl Geo {
    /// The plane through the vertex values `a`, with `frac` extra fraction
    /// bits: its value at the reference pixel centre and its per-pixel
    /// gradients.
    fn plane(&self, a: [i64; 3], frac: u32) -> (i64, i64, i64) {
        let d1 = a[1].wrapping_sub(a[0]);
        let d2 = a[2].wrapping_sub(a[0]);
        let nx = d1.wrapping_mul(self.dy2).wrapping_sub(d2.wrapping_mul(self.dy1)); // da/dX * area
        let ny = d2.wrapping_mul(self.dx1).wrapping_sub(d1.wrapping_mul(self.dx2)); // da/dY * area
        let gx = (nx << (4 + frac)) / self.area;
        let gy = (ny << (4 + frac)) / self.area;
        let at_ref = (nx.wrapping_mul(self.ox).wrapping_add(ny.wrapping_mul(self.oy)) << frac) / self.area;
        ((a[0] << frac).wrapping_add(at_ref), gx, gy)
    }

    /// A colour plane, saturated to 32 bits.
    fn plane32(&self, a: [i64; 3]) -> (i32, i32, i32) {
        let (base, gx, gy) = self.plane(a, C_FRAC);
        (sat32(base), sat32(gx), sat32(gy))
    }
}

fn min3(a: [i64; 3]) -> i64 {
    a[0].min(a[1]).min(a[2])
}

fn max3(a: [i64; 3]) -> i64 {
    a[0].max(a[1]).max(a[2])
}

/// a / b rounded towards negative infinity (b > 0).
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && a < 0 { q - 1 } else { q }
}

fn sat32(v: i64) -> i32 {
    v.clamp(-0x3FFF_FFFF, 0x3FFF_FFFF) as i32
}

/// log2 of a positive number in quarter steps (from its exponent and the
/// top two mantissa bits); 0 for values up to 1.
fn log2q_f64(v: f64) -> i32 {
    if v.is_nan() || v <= 1.0 {
        return 0;
    }
    let bits = v.to_bits();
    let e = ((bits >> 52) & 0x7FF) as i32 - 1023;
    let m = ((bits >> 50) & 3) as i32;
    e * 4 + m
}

fn abs_f64(v: f64) -> f64 {
    if v < 0.0 { -v } else { v }
}

/// Sets up the triangles of `indices` (three per triangle, indexing
/// `verts`): visible ones are appended to `out`, and the numbers of those
/// that need clipping to `deferred`. Returns how many were appended to
/// `out`.
///
/// # Safety
///
/// `s.tex`, when not null, must point to a valid texture whose levels
/// point to valid pixels, for as long as the triangles are used.
pub unsafe fn setup_batch(
    s: &Setup,
    verts: &[TVert],
    indices: &[u32],
    out: &mut Vec<Tri>,
    deferred: &mut Vec<u32>,
) -> usize {
    let before = out.len();
    out.reserve(indices.len() / 3);
    for (i, tri) in indices.as_chunks::<3>().0.iter().enumerate() {
        let (a, b, c) = (&verts[tri[0] as usize], &verts[tri[1] as usize], &verts[tri[2] as usize]);
        let (oa, ob, oc) = (a.outcode, b.outcode, c.outcode);
        if oa & ob & oc & OC_REJECT != 0 {
            continue;
        }
        if (oa | ob | oc) & OC_CLIP != 0 {
            deferred.push(i as u32);
            continue;
        }
        // SAFETY: as required of the caller.
        unsafe { setup_tri(s, a, b, c, out) };
    }
    out.len() - before
}

/// Sets up one triangle and appends it to `out`; false (and nothing
/// appended) if it is culled or covers no pixel.
///
/// # Safety
///
/// As for [`setup_batch`].
pub unsafe fn setup_tri(s: &Setup, a: &TVert, b: &TVert, c: &TVert, out: &mut Vec<Tri>) -> bool {
    let (x0, y0) = (a.sx as i64, a.sy as i64);
    let area = (b.sx as i64 - x0) * (c.sy as i64 - y0) - (c.sx as i64 - x0) * (b.sy as i64 - y0);
    // Screen y points down, so a counter-clockwise (front-facing) triangle
    // has a negative area here.
    if area == 0 || (s.cull == CULL_BACK && area > 0) || (s.cull == CULL_FRONT && area < 0) {
        return false;
    }
    // Wind the triangle so that its area is positive.
    let (b, c, area) = if area < 0 { (c, b, -area) } else { (b, c, area) };
    let v = [a, b, c];
    let vx = v.map(|v| v.sx as i64);
    let vy = v.map(|v| v.sy as i64);

    // Pixel bounding box (pixel centres at 16 * x + 8).
    let bx0 = ((min3(vx) + 7) >> 4).max(0);
    let by0 = ((min3(vy) + 7) >> 4).max(0);
    let bx1 = (((max3(vx) - 8) >> 4) + 1).min(s.width as i64);
    let by1 = (((max3(vy) - 8) >> 4) + 1).min(s.height as i64);
    if bx0 >= bx1 || by0 >= by1 {
        return false;
    }
    // The triangle is visible: build it in place at the end of `out`.
    out.reserve(1);
    let t = out.spare_capacity_mut()[0].write(Tri {
        x0: bx0 as i32,
        y0: by0 as i32,
        x1: bx1 as i32,
        y1: by1 as i32,
        ..Tri::ZERO
    });

    // Reference pixel: the one containing vertex 0, so that offsets stay tiny.
    let (rx, ry) = (x0 >> 4, y0 >> 4);
    t.rx = rx as i32;
    t.ry = ry as i32;
    let (px, py) = (rx * 16 + 8, ry * 16 + 8);

    // Edge functions E_ij(P) = (Xj - Xi)(Py - Yi) - (Yj - Yi)(Px - Xi);
    // inside when all are >= 0. Edges that are not top or left get a -1
    // bias so that pixels exactly on them are excluded (top-left fill rule).
    for k in 0..3 {
        let j = (k + 1) % 3;
        let (dx, dy) = (vx[j] - vx[k], vy[j] - vy[k]);
        let top_left = dy < 0 || (dy == 0 && dx > 0);
        t.e[k] = dx * (py - vy[k]) - dy * (px - vx[k]) - if top_left { 0 } else { 1 };
        t.ex[k] = (-dy * 16) as i32;
        t.ey[k] = (dx * 16) as i32;
    }

    let g = Geo {
        dx1: vx[1] - vx[0],
        dy1: vy[1] - vy[0],
        dx2: vx[2] - vx[0],
        dy2: vy[2] - vy[0],
        area,
        ox: px - vx[0],
        oy: py - vy[0],
    };
    (t.z, t.zx, t.zy) = g.plane(v.map(|v| v.q as i64), Z_FRAC);

    let mut mode = s.mode;
    // SAFETY: as required of the caller.
    let tex: Option<&FxTexture> = if mode & M_TEX != 0 { unsafe { s.tex.as_ref() } } else { None };
    let tex = tex.filter(|t| t.count > 0);
    if tex.is_none() {
        mode &= !(M_TEX | M_AFFINE | M_ATEST);
    }
    let blend = mode & M_BLEND_MASK;
    let premul = blend == M_ALPHA || blend == M_ADD;

    // Colours: multipliers r, g, b, alpha and the additive r, g, b, each
    // at the three vertices.
    let mut col = [[0i64; 3]; 7];
    for (i, v) in v.iter().enumerate() {
        let al = v.mul[3] as i64; // 0..256
        for k in 0..3 {
            if tex.is_some() {
                let (mut m, mut ad) = (v.mul[k] as i64, v.add[k] as i64);
                if premul {
                    m = (m * al) >> 8;
                    ad = (ad * al) >> 8;
                }
                col[k][i] = m;
                col[4 + k][i] = ad;
            } else {
                // A white "texel" (255) times the multiplier plus the
                // additive part, in output units * 256.
                let mut f = (v.mul[k] as i64 * 255 + v.add[k] as i64).min(0xFFFF);
                if premul {
                    f = (f * al) >> 8;
                }
                col[k][i] = f;
            }
        }
        col[3][i] = al << 8; // alpha in 8.8 like the colours (1.0 = 256 * 256)
    }
    let channels = if tex.is_some() { 7 } else { 4 };
    // Flat-shaded faces (all vertices within one output unit) are filled
    // with their average colour by a cheaper rasteriser.
    let flat = tex.is_none() && col[..4].iter().all(|c| max3(*c) - min3(*c) < 256);
    if flat {
        mode |= M_FLAT;
        for (c, col) in t.c.iter_mut().zip(&col[..4]) {
            *c = (((col[0] + col[1] + col[2]) / 3) << C_FRAC) as i32;
        }
    } else {
        mode &= !M_FLAT;
        for (k, col) in col[..channels].iter().enumerate() {
            (t.c[k], t.cx[k], t.cy[k]) = g.plane32(*col);
        }
    }

    if let Some(tx) = tex {
        t.tex = tx;
        let (wl, hl) = (tx.levels[0].wlog2, tx.levels[0].hlog2);
        // Texture coordinates in level-0 texels * 256, rebased by whole
        // repeats so that they are small and non-negative.
        let mut ut = v.map(|v| ((v.u as i64) << wl) >> 8);
        let mut vt = v.map(|v| ((v.v as i64) << hl) >> 8);
        let (tw, th) = (256i64 << wl, 256i64 << hl);
        let (ub, vb) = (floor_div(min3(ut), tw) * tw, floor_div(min3(vt), th) * th);
        for i in 0..3 {
            ut[i] -= ub;
            vt[i] -= vb;
        }
        let bits_uv = bitlen64(max3(ut).max(max3(vt)).max(1) as u64);
        if mode & M_AFFINE != 0 {
            (t.uq, t.uqx, t.uqy) = g.plane(ut, T_FRAC);
            (t.vq, t.vqx, t.vqy) = g.plane(vt, T_FRAC);
            // The largest step, in texels * 256 * 2^T_FRAC per pixel.
            let r = [t.uqx, t.uqy, t.vqx, t.vqy].map(i64::wrapping_abs).into_iter().fold(i64::MIN, i64::max);
            t.lod_base = log2q(r.max(1) as u64) - 4 * (8 + T_FRAC as i32) + s.lod_bias;
        } else {
            let qt_bits = (35 - bits_uv).clamp(8, 24);
            let qmax = v.iter().map(|v| v.q as i64).fold(i64::MIN, i64::max);
            let shift = qt_bits - bitlen64(qmax.max(1) as u64);
            let qt = v.map(|v| {
                let q = v.q as i64;
                let q = if shift >= 0 { q << shift } else { q >> -shift };
                q.max(1)
            });
            let times = |a: [i64; 3]| [a[0].wrapping_mul(qt[0]), a[1].wrapping_mul(qt[1]), a[2].wrapping_mul(qt[2])];
            (t.uq, t.uqx, t.uqy) = g.plane(times(ut), T_FRAC);
            (t.vq, t.vqx, t.vqy) = g.plane(times(vt), T_FRAC);
            (t.qt, t.qtx, t.qty) = g.plane(qt, T_FRAC);
            // Mip level: the texel footprint at a pixel is
            // |d(uq)/dx - u * d(qt)/dx| / qt; take the numerator at the
            // centroid and divide by the interpolated qt per pixel.
            let uc = (ut[0] + ut[1] + ut[2]) as f64 / 3.0;
            let vc = (vt[0] + vt[1] + vt[2]) as f64 / 3.0;
            let (qx, qy) = (t.qtx as f64, t.qty as f64);
            let mut n = abs_f64(t.uqx as f64 - uc * qx);
            for m in [t.uqy as f64 - uc * qy, t.vqx as f64 - vc * qx, t.vqy as f64 - vc * qy] {
                let m = abs_f64(m);
                if m > n {
                    n = m;
                }
            }
            // level = log2(n / qt_interp / 256); per pixel subtract log2(qt).
            t.lod_base = log2q_f64(n) - 4 * 8 + s.lod_bias;
        }
    }
    t.mode = mode;
    // SAFETY: the slot after the last element was initialised above.
    unsafe { out.set_len(out.len() + 1) };
    true
}
