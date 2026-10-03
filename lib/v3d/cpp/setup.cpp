// setup.cpp - triangle setup: culling, bounding box, edge functions with the
// top-left fill rule, and fixed-point plane equations for depth, colours and
// perspective-correct texture coordinates.
//
// Screen positions are 28.4 fixed point. A plane a(x, y) is stored as its
// value at the centre of the reference pixel (rx, ry) plus per-pixel
// gradients, all with extra fraction bits; the rasteriser evaluates it at
// the first covered pixel of each span and then steps by the x gradient.

#include "v3d_core.h"
#include "v3d_simd.h"

static_assert(sizeof(V3dTri) == 272, "V3dTri layout");
static_assert(sizeof(V3dXform) == 416, "V3dXform layout");
static_assert(sizeof(V3dSetup) == 32, "V3dSetup layout");
static_assert(sizeof(V3dTexture) == 13 * 16 + 8, "V3dTexture layout");

namespace {

struct Geo {
    i64 dx1, dy1, dx2, dy2;  // edge vectors from vertex 0 (28.4)
    i64 area;                // twice the signed area (> 0)
    i64 ox, oy;              // reference pixel centre minus vertex 0 (28.4)
};

inline i64 min3(i64 a, i64 b, i64 c) {
    i64 m = a < b ? a : b;
    return m < c ? m : c;
}

inline i64 max3(i64 a, i64 b, i64 c) {
    i64 m = a > b ? a : b;
    return m > c ? m : c;
}

inline i64 floor_div(i64 a, i64 b) {  // b > 0
    i64 q = a / b;
    return (a % b != 0 && a < 0) ? q - 1 : q;
}

inline i32 sat32(i64 v) {
    return v < -0x3FFFFFFF ? -0x3FFFFFFF : (v > 0x3FFFFFFF ? 0x3FFFFFFF : (i32)v);
}

/// Plane through (vertex value a0, a1, a2) with `frac` extra fraction bits:
/// value at the reference pixel centre and per-pixel gradients.
inline void plane(const Geo& g, i64 a0, i64 a1, i64 a2, i32 frac, i64* base, i64* gx, i64* gy) {
    const i64 d1 = a1 - a0, d2 = a2 - a0;
    const i64 nx = d1 * g.dy2 - d2 * g.dy1;  // d a / d X * area
    const i64 ny = d2 * g.dx1 - d1 * g.dx2;  // d a / d Y * area
    *gx = (nx << (4 + frac)) / g.area;
    *gy = (ny << (4 + frac)) / g.area;
    *base = (a0 << frac) + ((nx * g.ox + ny * g.oy) << frac) / g.area;
}

inline void plane32(const Geo& g, i64 a0, i64 a1, i64 a2, i32* base, i32* gx, i32* gy) {
    i64 b, x, y;
    plane(g, a0, a1, a2, C_FRAC, &b, &x, &y);
    *base = sat32(b);
    *gx = sat32(x);
    *gy = sat32(y);
}

/// log2 of a positive double in quarter steps (from its exponent and the
/// top two mantissa bits).
inline i32 log2q_f64(double v) {
    if (!(v > 1.0)) return 0;
    union {
        double d;
        u64 u;
    } c;
    c.d = v;
    const i32 e = (i32)((c.u >> 52) & 0x7FF) - 1023;
    const i32 m = (i32)((c.u >> 50) & 3);
    return e * 4 + m;
}

}  // namespace

V3D_EXPORT i32 v3d_setup_tri(const V3dSetup* s, const V3dTVert* a, const V3dTVert* b, const V3dTVert* c, V3dTri* t) {
    i64 x0 = a->sx, y0 = a->sy, x1 = b->sx, y1 = b->sy, x2 = c->sx, y2 = c->sy;
    i64 area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
    // Screen y points down, so a counter-clockwise (front-facing) triangle
    // has a negative area here.
    if (area == 0) return 0;
    if (s->cull == CULL_BACK && area > 0) return 0;
    if (s->cull == CULL_FRONT && area < 0) return 0;
    if (area < 0) {
        const V3dTVert* tmp = b;
        b = c;
        c = tmp;
        x1 = b->sx;
        y1 = b->sy;
        x2 = c->sx;
        y2 = c->sy;
        area = -area;
    }

    // Pixel bounding box (pixel centres at 16 * x + 8).
    i64 bx0 = (min3(x0, x1, x2) + 7) >> 4;
    i64 by0 = (min3(y0, y1, y2) + 7) >> 4;
    i64 bx1 = ((max3(x0, x1, x2) - 8) >> 4) + 1;
    i64 by1 = ((max3(y0, y1, y2) - 8) >> 4) + 1;
    if (bx0 < 0) bx0 = 0;
    if (by0 < 0) by0 = 0;
    if (bx1 > s->width) bx1 = s->width;
    if (by1 > s->height) by1 = s->height;
    if (bx0 >= bx1 || by0 >= by1) return 0;
    t->x0 = (i32)bx0;
    t->y0 = (i32)by0;
    t->x1 = (i32)bx1;
    t->y1 = (i32)by1;

    // Reference pixel: the one containing vertex 0, so that offsets stay tiny.
    const i64 rx = x0 >> 4, ry = y0 >> 4;
    t->rx = (i32)rx;
    t->ry = (i32)ry;
    const i64 px = rx * 16 + 8, py = ry * 16 + 8;

    // Edge functions E_ij(P) = (Xj - Xi)(Py - Yi) - (Yj - Yi)(Px - Xi); inside
    // when all are >= 0. Edges that are not top or left get a -1 bias so
    // that pixels exactly on them are excluded (top-left fill rule).
    const i64 vx[3] = {x0, x1, x2}, vy[3] = {y0, y1, y2};
    for (i32 k = 0; k < 3; k++) {
        const i32 j = k == 2 ? 0 : k + 1;
        const i64 dx = vx[j] - vx[k], dy = vy[j] - vy[k];
        const bool top_left = dy < 0 || (dy == 0 && dx > 0);
        t->e[k] = dx * (py - vy[k]) - dy * (px - vx[k]) - (top_left ? 0 : 1);
        t->ex[k] = (i32)(-dy * 16);
        t->ey[k] = (i32)(dx * 16);
    }

    Geo g;
    g.dx1 = x1 - x0;
    g.dy1 = y1 - y0;
    g.dx2 = x2 - x0;
    g.dy2 = y2 - y0;
    g.area = area;
    g.ox = px - x0;
    g.oy = py - y0;

    plane(g, a->q, b->q, c->q, Z_FRAC, &t->z, &t->zx, &t->zy);

    u32 mode = s->mode;
    const V3dTexture* tex = (mode & M_TEX) ? s->tex : nullptr;
    if (!tex || tex->count <= 0) {
        mode &= ~(u32)(M_TEX | M_AFFINE | M_ATEST);
        tex = nullptr;
    }
    const u32 blend = mode & M_BLEND_MASK;
    const bool premul = blend == M_ALPHA || blend == M_ADD;

    // Colours.
    const V3dTVert* v[3] = {a, b, c};
    i64 col[7][3];
    for (i32 i = 0; i < 3; i++) {
        const i64 al = v[i]->mul[3];  // 0..256
        if (tex) {
            for (i32 k = 0; k < 3; k++) {
                i64 m = v[i]->mul[k];
                i64 ad = v[i]->add[k];
                if (premul) {
                    m = (m * al) >> 8;
                    ad = (ad * al) >> 8;
                }
                col[k][i] = m;
                col[4 + k][i] = ad;
            }
        } else {
            for (i32 k = 0; k < 3; k++) {
                // White "texel" (255) times the multiplier plus the additive
                // part, in output units * 256.
                i64 f = (i64)v[i]->mul[k] * 255 + v[i]->add[k];
                if (f > 0xFFFF) f = 0xFFFF;
                if (premul) f = (f * al) >> 8;
                col[k][i] = f;
                col[4 + k][i] = 0;
            }
        }
        col[3][i] = al << 8;  // alpha in 8.8 like the colours (1.0 = 256 * 256)
    }
    const i32 nchan = tex ? 7 : 4;
    // Flat-shaded faces (all vertices within one output unit) are filled
    // with their average colour by a cheaper rasteriser.
    bool flat = !tex;
    for (i32 k = 0; k < 4 && flat; k++) {
        if (max3(col[k][0], col[k][1], col[k][2]) - min3(col[k][0], col[k][1], col[k][2]) >= 256) flat = false;
    }
    if (flat) {
        mode |= M_FLAT;
        for (i32 k = 0; k < 4; k++) {
            t->c[k] = (i32)(((col[k][0] + col[k][1] + col[k][2]) / 3) << C_FRAC);
            t->cx[k] = t->cy[k] = 0;
        }
    } else {
        mode &= ~(u32)M_FLAT;
        for (i32 k = 0; k < nchan; k++) plane32(g, col[k][0], col[k][1], col[k][2], &t->c[k], &t->cx[k], &t->cy[k]);
    }
    for (i32 k = nchan; k < 7; k++) t->c[k] = t->cx[k] = t->cy[k] = 0;
    t->pad = 0;

    t->tex = tex;
    t->lod_base = 0;
    if (tex) {
        const i32 wl = tex->levels[0].wlog2, hl = tex->levels[0].hlog2;
        // Texture coordinates in level-0 texels * 256, rebased by whole
        // repeats so that they are small and non-negative.
        i64 ut[3], vt[3];
        for (i32 i = 0; i < 3; i++) {
            ut[i] = ((i64)v[i]->u << wl) >> 8;
            vt[i] = ((i64)v[i]->v << hl) >> 8;
        }
        const i64 tw = (i64)256 << wl, th = (i64)256 << hl;
        const i64 ub = floor_div(min3(ut[0], ut[1], ut[2]), tw) * tw;
        const i64 vb = floor_div(min3(vt[0], vt[1], vt[2]), th) * th;
        for (i32 i = 0; i < 3; i++) {
            ut[i] -= ub;
            vt[i] -= vb;
        }
        const i32 bits_uv = bitlen64((u64)max3(max3(ut[0], ut[1], ut[2]), max3(vt[0], vt[1], vt[2]), 1));
        if (mode & M_AFFINE) {
            plane(g, ut[0], ut[1], ut[2], T_FRAC, &t->uq, &t->uqx, &t->uqy);
            plane(g, vt[0], vt[1], vt[2], T_FRAC, &t->vq, &t->vqx, &t->vqy);
            t->qt = t->qtx = t->qty = 0;
            i64 r = t->uqx < 0 ? -t->uqx : t->uqx;
            const i64 cand[3] = {t->uqy, t->vqx, t->vqy};
            for (i32 k = 0; k < 3; k++) {
                const i64 m = cand[k] < 0 ? -cand[k] : cand[k];
                if (m > r) r = m;
            }
            // r = texels * 256 * 2^T_FRAC per pixel.
            t->lod_base = log2q((u64)(r > 1 ? r : 1)) - 4 * (8 + T_FRAC) + s->lod_bias;
        } else {
            i32 qt_bits = 35 - bits_uv;
            if (qt_bits > 24) qt_bits = 24;
            if (qt_bits < 8) qt_bits = 8;
            const i64 qmax = max3(a->q, b->q, c->q);
            const i32 shift = qt_bits - bitlen64((u64)(qmax > 1 ? qmax : 1));
            i64 qt[3];
            for (i32 i = 0; i < 3; i++) {
                i64 q = v[i]->q;
                q = shift >= 0 ? q << shift : q >> -shift;
                qt[i] = q > 0 ? q : 1;
            }
            plane(g, ut[0] * qt[0], ut[1] * qt[1], ut[2] * qt[2], T_FRAC, &t->uq, &t->uqx, &t->uqy);
            plane(g, vt[0] * qt[0], vt[1] * qt[1], vt[2] * qt[2], T_FRAC, &t->vq, &t->vqx, &t->vqy);
            plane(g, qt[0], qt[1], qt[2], T_FRAC, &t->qt, &t->qtx, &t->qty);
            // Mip level: the texel footprint at a pixel is
            // |d(uq)/dx - u * d(qt)/dx| / qt; take the numerator at the
            // centroid and divide by the interpolated qt per pixel.
            const double uc = (double)(ut[0] + ut[1] + ut[2]) / 3.0;
            const double vc = (double)(vt[0] + vt[1] + vt[2]) / 3.0;
            const double qx = (double)t->qtx, qy = (double)t->qty;
            double n = (double)t->uqx - uc * qx;
            n = n < 0 ? -n : n;
            double m = (double)t->uqy - uc * qy;
            m = m < 0 ? -m : m;
            if (m > n) n = m;
            m = (double)t->vqx - vc * qx;
            m = m < 0 ? -m : m;
            if (m > n) n = m;
            m = (double)t->vqy - vc * qy;
            m = m < 0 ? -m : m;
            if (m > n) n = m;
            // level = log2(n / qt_interp / 256); per pixel subtract log2(qt).
            t->lod_base = log2q_f64(n) - 4 * 8 + s->lod_bias;
        }
    } else {
        t->uq = t->uqx = t->uqy = 0;
        t->vq = t->vqx = t->vqy = 0;
        t->qt = t->qtx = t->qty = 0;
    }
    t->mode = mode;
    return 1;
}

V3D_EXPORT i32 v3d_setup_batch(const V3dSetup* s, const V3dTVert* verts, const u32* indices, i32 count, V3dTri* out,
                               u32* deferred, i32* deferred_count) {
    i32 n = 0, nd = 0;
    for (i32 i = 0; i < count; i++) {
        const V3dTVert* a = &verts[indices[3 * i]];
        const V3dTVert* b = &verts[indices[3 * i + 1]];
        const V3dTVert* c = &verts[indices[3 * i + 2]];
        const u32 oa = a->outcode, ob = b->outcode, oc = c->outcode;
        if (oa & ob & oc & OC_REJECT) continue;
        if ((oa | ob | oc) & OC_CLIP) {
            deferred[nd++] = (u32)i;
            continue;
        }
        n += v3d_setup_tri(s, a, b, c, &out[n]);
    }
    *deferred_count = nd;
    return n;
}
