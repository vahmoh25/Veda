//! Tile rasterisation.
//!
//! For every row of the tile a triangle touches, the covered span is found
//! directly from the three edge functions with one integer division per
//! edge (cheap under QEMU TCG), so the pixel loop never tests edges. Each
//! render mode (texturing, blending, alpha test, depth test and write,
//! affine mapping, flat colour) gets its own monomorphised rasteriser, so
//! the per-pixel loop contains only the work that mode needs: scalar
//! integer code, which TCG translates well.

use super::{
    FxTexture, M_ADD, M_AFFINE, M_ALPHA, M_ATEST, M_BILINEAR, M_BLEND_MASK, M_COUNT, M_FLAT, M_OPAQUE, M_TEX, M_ZTEST,
    M_ZWRITE, RasterStats, T_FRAC, Target, Tri, Z_FRAC, log2q, scalar_loop,
};

/// Fills rows [y0, y1) x [x0, x1) of the target (clipped to it): the colour
/// of each row from `row_colors` (one per target row, made opaque), depth 0.
///
/// # Safety
///
/// The target's buffers must be valid for its size, and no other thread may
/// access the rectangle meanwhile.
pub unsafe fn clear_rect(t: &Target, x0: i32, y0: i32, x1: i32, y1: i32, row_colors: &[u32]) {
    let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(t.width), y1.min(t.height));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let n = (x1 - x0) as usize;
    for y in y0..y1 {
        let at = y as usize * t.stride as usize + x0 as usize;
        // SAFETY: the row lies inside the target, which the caller lends us.
        unsafe {
            core::slice::from_raw_parts_mut(t.color.add(at), n).fill(row_colors[y as usize] | 0xFF00_0000);
            core::ptr::write_bytes(t.depth.add(at), 0, n);
        }
    }
}

/// Rasterises the triangles `tris[i]` for each `i` in `indices`, clipped
/// to the rectangle [x0, x1) x [y0, y1), and adds the work done to `stats`.
///
/// # Safety
///
/// The target's buffers must be valid for its size, no other thread may
/// access the rectangle meanwhile, and the triangles' textures must be
/// valid.
pub unsafe fn raster(
    t: &Target,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    tris: &[Tri],
    indices: &[u32],
    stats: &mut RasterStats,
) {
    let mut st = RasterStats::default();
    for &i in indices {
        let tri = &tris[i as usize];
        let m = (tri.mode & (M_COUNT - 1)) as usize;
        // SAFETY: as required of the caller.
        unsafe { RASTER[m >> 4][m & 15](t, x0, y0, x1, y1, tri, &mut st) };
    }
    stats.triangles += st.triangles;
    stats.spans += st.spans;
    stats.pixels += st.pixels;
}

type RasterFn = unsafe fn(&Target, i32, i32, i32, i32, &Tri, &mut RasterStats);

/// The rasteriser variant for each mode: bits that cannot matter in a mode
/// are cleared first, so equivalent modes share one function.
const fn variant(m: u32) -> u32 {
    if m & M_TEX != 0 { m & !M_FLAT } else { m & !(M_ATEST | M_AFFINE) }
}

/// Sixteen consecutive table entries, starting at mode `$b`.
macro_rules! row {
    ($b:expr) => {
        [
            raster_tri::<{ variant($b) }>,
            raster_tri::<{ variant($b + 1) }>,
            raster_tri::<{ variant($b + 2) }>,
            raster_tri::<{ variant($b + 3) }>,
            raster_tri::<{ variant($b + 4) }>,
            raster_tri::<{ variant($b + 5) }>,
            raster_tri::<{ variant($b + 6) }>,
            raster_tri::<{ variant($b + 7) }>,
            raster_tri::<{ variant($b + 8) }>,
            raster_tri::<{ variant($b + 9) }>,
            raster_tri::<{ variant($b + 10) }>,
            raster_tri::<{ variant($b + 11) }>,
            raster_tri::<{ variant($b + 12) }>,
            raster_tri::<{ variant($b + 13) }>,
            raster_tri::<{ variant($b + 14) }>,
            raster_tri::<{ variant($b + 15) }>,
        ]
    };
}

/// The rasterisers, indexed by mode (high nibble, low nibble).
static RASTER: [[RasterFn; 16]; 16] = [
    row!(0),
    row!(16),
    row!(32),
    row!(48),
    row!(64),
    row!(80),
    row!(96),
    row!(112),
    row!(128),
    row!(144),
    row!(160),
    row!(176),
    row!(192),
    row!(208),
    row!(224),
    row!(240),
];

/// Rasterises one triangle in mode `M`, clipped to the rectangle.
///
/// # Safety
///
/// As for [`raster`].
unsafe fn raster_tri<const M: u32>(tg: &Target, cx0: i32, cy0: i32, cx1: i32, cy1: i32, t: &Tri, st: &mut RasterStats) {
    let (xs, xe) = (t.x0.max(cx0), t.x1.min(cx1));
    let (ys, ye) = (t.y0.max(cy0), t.y1.min(cy1));
    if xs >= xe || ys >= ye {
        return;
    }
    st.triangles += 1;
    let width = xe - xs;
    let (dxs, dys) = ((xs - t.rx) as i64, (ys - t.ry) as i64);
    let mut e: [i64; 3] = core::array::from_fn(|k| t.e[k] + t.ex[k] as i64 * dxs + t.ey[k] as i64 * dys);
    let (mut spans, mut pixels) = (0u32, 0u32);
    for y in ys..ye {
        let (mut xa, mut xb) = (0, width);
        if clip_span(e[0], t.ex[0] as i64, &mut xa, &mut xb)
            && clip_span(e[1], t.ex[1] as i64, &mut xa, &mut xb)
            && clip_span(e[2], t.ex[2] as i64, &mut xa, &mut xb)
            && xa < xb
        {
            spans += 1;
            pixels += (xb - xa) as u32;
            // SAFETY: the span lies inside the triangle's bounding box and
            // the rectangle, so inside the target (as required of the caller).
            unsafe { shade_span::<M>(tg, t, xs + xa, xs + xb, y) };
        }
        for (e, &ey) in e.iter_mut().zip(&t.ey) {
            *e += ey as i64;
        }
    }
    st.spans += spans as u64;
    st.pixels += pixels as u64;
}

/// Narrows the covered span [xa, xb) of a row with one edge function `e`
/// (its value at the span origin), which changes by `ex` per pixel.
/// Returns false when the row is empty.
#[inline(always)]
fn clip_span(e: i64, ex: i64, xa: &mut i32, xb: &mut i32) -> bool {
    if ex > 0 {
        if e < 0 {
            let n = (-e + ex - 1) / ex; // first pixel with e + n * ex >= 0
            if n >= *xb as i64 {
                return false;
            }
            if n > *xa as i64 {
                *xa = n as i32;
            }
        }
    } else if ex < 0 {
        if e < 0 {
            return false;
        }
        let n = e / -ex + 1; // one past the last pixel with e + n * ex >= 0
        if n < *xb as i64 {
            *xb = n as i32;
        }
    } else if e < 0 {
        return false;
    }
    true
}

#[inline(always)]
fn clamp255(v: i32) -> u32 {
    v.clamp(0, 255) as u32
}

/// x * a / 255 for the channels at bits 0-7 and 16-23.
#[inline(always)]
fn mul_div255_rb(x: u32, a: u32) -> u32 {
    let t = (x & 0x00FF_00FF).wrapping_mul(a).wrapping_add(0x0080_0080);
    (t.wrapping_add((t >> 8) & 0x00FF_00FF) >> 8) & 0x00FF_00FF
}

/// Scales the colour channels of a pixel by a / 255.
#[inline(always)]
fn scale_rgb(px: u32, a: u32) -> u32 {
    mul_div255_rb(px, a) | (mul_div255_rb(px >> 8, a) << 8)
}

/// Linear interpolation of two pixels (all four channels), t in 0..=256.
#[inline(always)]
fn lerp_px(a: u32, b: u32, t: u32) -> u32 {
    let it = 256 - t;
    let rb = ((a & 0x00FF_00FF).wrapping_mul(it).wrapping_add((b & 0x00FF_00FF).wrapping_mul(t)) >> 8) & 0x00FF_00FF;
    let ag =
        ((a >> 8) & 0x00FF_00FF).wrapping_mul(it).wrapping_add(((b >> 8) & 0x00FF_00FF).wrapping_mul(t)) & 0xFF00_FF00;
    rb | ag
}

/// One mip level, ready for sampling.
#[derive(Clone, Copy)]
struct Level {
    pixels: *const u32,
    wlog2: u32,
    wmask: u32,
    hmask: u32,
    /// Texture coordinate bits below a texel of this level.
    shift: u32,
}

impl Level {
    const NONE: Level = Level { pixels: core::ptr::null(), wlog2: 0, wmask: 0, hmask: 0, shift: 0 };

    /// Level `l` of `tex` (clamped to the existing levels).
    ///
    /// # Safety
    ///
    /// `tex` must be valid and have at least one level.
    #[inline(always)]
    unsafe fn of(tex: *const FxTexture, l: i32) -> Level {
        // SAFETY: as required of the caller.
        let tex = unsafe { &*tex };
        let l = l.clamp(0, tex.count - 1);
        let m = &tex.levels[l as usize];
        Level {
            pixels: m.pixels,
            wlog2: m.wlog2 as u32,
            wmask: (1u32 << m.wlog2) - 1,
            hmask: (1u32 << m.hlog2) - 1,
            shift: 8 + l as u32,
        }
    }

    /// The texel at (u, v) in level-0 texels * 256, wrapping.
    ///
    /// # Safety
    ///
    /// The level's pixels must be valid.
    #[inline(always)]
    unsafe fn nearest(&self, u: u32, v: u32) -> u32 {
        let at = (((v >> self.shift) & self.hmask) << self.wlog2) | ((u >> self.shift) & self.wmask);
        // SAFETY: the masks keep the index inside the level.
        unsafe { *self.pixels.add(at as usize) }
    }

    /// Bilinear filtering at (u, v) in texels * 256 of this level (only
    /// used for level 0, where they are level-0 texels), wrapping.
    ///
    /// # Safety
    ///
    /// The level's pixels must be valid.
    #[inline(always)]
    unsafe fn bilinear(&self, u: u32, v: u32) -> u32 {
        let (u, v) = (u.wrapping_sub(128), v.wrapping_sub(128)); // texel centres
        let x0 = (u >> 8) & self.wmask;
        let x1 = (x0 + 1) & self.wmask;
        let y0 = (v >> 8) & self.hmask;
        let y1 = (y0 + 1) & self.hmask;
        let (fx, fy) = (u & 255, v & 255);
        // SAFETY: the masks keep every index inside the level.
        unsafe {
            let r0 = self.pixels.add((y0 << self.wlog2) as usize);
            let r1 = self.pixels.add((y1 << self.wlog2) as usize);
            let top = lerp_px(*r0.add(x0 as usize), *r0.add(x1 as usize), fx);
            let bottom = lerp_px(*r1.add(x0 as usize), *r1.add(x1 as usize), fx);
            lerp_px(top, bottom, fy)
        }
    }

    /// Samples at (u, v), bilinear when `bilinear`.
    ///
    /// # Safety
    ///
    /// The level's pixels must be valid.
    #[inline(always)]
    unsafe fn sample(&self, u: u32, v: u32, bilinear: bool) -> u32 {
        // SAFETY: as required of the caller.
        unsafe { if bilinear { self.bilinear(u, v) } else { self.nearest(u, v) } }
    }
}

/// Positive part of a colour channel, shifted down.
#[inline(always)]
fn positive(v: i32, shift: u32) -> u32 {
    (v.max(0) as u32) >> shift
}

/// Fills pixels [xa, xb) of row `y` with triangle `t` in mode `M`.
///
/// # Safety
///
/// The span must lie inside the target, which no other thread may access
/// meanwhile, and the triangle's texture must be valid.
#[inline(always)]
unsafe fn shade_span<const M: u32>(tg: &Target, t: &Tri, xa: i32, xb: i32, y: i32) {
    let tex = M & M_TEX != 0;
    let blend = M & M_BLEND_MASK;
    let atest = tex && M & M_ATEST != 0;
    let ztest = M & M_ZTEST != 0;
    let zwrite = M & M_ZWRITE != 0;
    let affine = tex && M & M_AFFINE != 0;
    let flat = !tex && M & M_FLAT != 0;
    // Interpolated colour channels.
    let channels = if tex {
        7
    } else if flat {
        0
    } else {
        4
    };

    let (dx, dy) = ((xa - t.rx) as i64, (y - t.ry) as i64);
    let row = y as usize * tg.stride as usize + xa as usize;
    // SAFETY: the span lies inside the target (as required of the caller).
    let (mut cp, mut zp) = unsafe { (tg.color.add(row), tg.depth.add(row)) };
    let mut z = t.z + t.zx * dx + t.zy * dy;
    let mut c = [0i32; 7];
    for (k, c) in c[..channels].iter_mut().enumerate() {
        *c = (t.c[k] as i64 + t.cx[k] as i64 * dx + t.cy[k] as i64 * dy) as i32;
    }
    // Flat triangles: the colour is computed once.
    let (fr, fg, fb, fa) = if flat {
        (clamp255(t.c[0] >> 16), clamp255(t.c[1] >> 16), clamp255(t.c[2] >> 16), positive(t.c[3], 16).min(255))
    } else {
        (0, 0, 0, 255)
    };

    // Texturing: affine mapping uses one mip level for the whole triangle,
    // perspective mapping one per sub-span of SUB pixels, with exact
    // coordinates (texels * 256) at the sub-span ends and linear steps in
    // between.
    const SUB: i32 = 16;
    let want_bilinear = t.mode & M_BILINEAR != 0;
    let (mut uq, mut vq, mut qt) = (0i64, 0i64, 0i64);
    let (mut pu, mut pv, mut pdu, mut pdv) = (0i64, 0i64, 0i64, 0i64);
    let mut block = 0;
    let mut level = Level::NONE;
    let mut bilinear = false;
    if tex {
        uq = t.uq + t.uqx * dx + t.uqy * dy;
        vq = t.vq + t.vqx * dx + t.vqy * dy;
        if affine {
            let l = (t.lod_base + 2) >> 2;
            // SAFETY: textured triangles have a valid texture.
            level = unsafe { Level::of(t.tex, l) };
            bilinear = want_bilinear && level.shift == 8;
        } else {
            qt = t.qt + t.qtx * dx + t.qty * dy;
            let q0 = qt.max(1);
            pu = uq / q0;
            pv = vq / q0;
        }
    }

    for n in (1..=xb - xa).rev() {
        if tex && !affine && block == 0 {
            // Start a sub-span: exact coordinates at its end, the step, and
            // the mip level from its middle.
            let len = n.min(SUB);
            let qe = qt + t.qtx * len as i64;
            let q1 = qe.max(1);
            let u1 = (uq + t.uqx * len as i64) / q1;
            let v1 = (vq + t.vqx * len as i64) / q1;
            if len == SUB {
                pdu = (u1 - pu) >> 4;
                pdv = (v1 - pv) >> 4;
            } else {
                pdu = (u1 - pu) / len as i64;
                pdv = (v1 - pv) / len as i64;
            }
            let qm = qt + t.qtx * (len >> 1) as i64;
            let l = (t.lod_base - log2q(qm.max(1) as u64) + 2) >> 2;
            // SAFETY: textured triangles have a valid texture.
            level = unsafe { Level::of(t.tex, l) };
            bilinear = want_bilinear && level.shift == 8;
            uq += t.uqx * len as i64;
            vq += t.vqx * len as i64;
            qt = qe;
            block = len;
        }
        let zz = if ztest || zwrite { (z >> Z_FRAC) as i32 } else { 0 };
        // SAFETY: zp and cp point inside the span.
        let mut keep = !ztest || zz > unsafe { *zp };
        let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 255u32);
        if keep {
            if tex {
                let (u, v) =
                    if affine { ((uq >> T_FRAC) as u32, (vq >> T_FRAC) as u32) } else { (pu as u32, pv as u32) };
                // SAFETY: the level belongs to the triangle's valid texture.
                let texel = unsafe { level.sample(u, v, bilinear) };
                let ta = texel >> 24;
                a = (ta * positive(c[3], 16)) >> 8;
                if atest {
                    keep = a >= 128;
                }
                let (mr, mg, mb) = (positive(c[0], 8), positive(c[1], 8), positive(c[2], 8));
                let (mut ar, mut ag, mut ab) = (c[4] >> 16, c[5] >> 16, c[6] >> 16);
                if blend == M_ALPHA {
                    ar = (ar * ta as i32) >> 8;
                    ag = (ag * ta as i32) >> 8;
                    ab = (ab * ta as i32) >> 8;
                }
                r = clamp255(((((texel >> 16) & 255) * mr) >> 8) as i32 + ar);
                g = clamp255(((((texel >> 8) & 255) * mg) >> 8) as i32 + ag);
                b = clamp255((((texel & 255) * mb) >> 8) as i32 + ab);
            } else if flat {
                (r, g, b, a) = (fr, fg, fb, fa);
            } else {
                r = clamp255(c[0] >> 16);
                g = clamp255(c[1] >> 16);
                b = clamp255(c[2] >> 16);
                a = positive(c[3], 16).min(255);
            }
        }
        if keep {
            // SAFETY: cp and zp point inside the span.
            unsafe {
                *cp = match blend {
                    M_OPAQUE => 0xFF00_0000 | (r << 16) | (g << 8) | b,
                    M_ALPHA => add_sat(scale_rgb(*cp, 255 - a.min(255)), r, g, b),
                    M_ADD => add_sat(*cp, r, g, b),
                    _ => multiply(*cp, r, g, b, a),
                };
                if zwrite {
                    *zp = zz;
                }
            }
        }
        z += t.zx;
        for (c, &cx) in c[..channels].iter_mut().zip(&t.cx) {
            scalar_loop();
            *c = c.wrapping_add(cx);
        }
        if tex {
            if affine {
                uq += t.uqx;
                vq += t.vqx;
            } else {
                pu += pdu;
                pv += pdv;
                block -= 1;
            }
        }
        // SAFETY: still inside the span or one past its end.
        unsafe {
            cp = cp.add(1);
            zp = zp.add(1);
        }
    }
}

/// Adds a colour to a destination pixel per channel, saturating.
#[inline(always)]
fn add_sat(d: u32, r: u32, g: u32, b: u32) -> u32 {
    let rr = (((d >> 16) & 255) + r).min(255);
    let gg = (((d >> 8) & 255) + g).min(255);
    let bb = ((d & 255) + b).min(255);
    0xFF00_0000 | (rr << 16) | (gg << 8) | bb
}

/// Multiplies a destination pixel by a colour faded towards white by its
/// alpha.
#[inline(always)]
fn multiply(d: u32, r: u32, g: u32, b: u32, a: u32) -> u32 {
    let ia = 255 - a.min(255);
    let fade = |c: u32| c + ((255 - c) * ia) / 255;
    let (sr, sg, sb) = (fade(r), fade(g), fade(b));
    0xFF00_0000 | (((d >> 16) & 255) * sr / 255) << 16 | (((d >> 8) & 255) * sg / 255) << 8 | ((d & 255) * sb / 255)
}
