// raster.cpp - tile rasterisation.
//
// For every row of the tile a triangle touches, the covered span is found
// directly from the three edge functions with one integer division per edge
// (cheap under QEMU TCG), so the pixel loop never tests edges. Each render
// mode (texturing, blending, alpha test, depth test/write, affine mapping)
// gets its own template instantiation; the per-pixel work is scalar integer
// code: under TCG, scalar integer instructions are several times cheaper
// than SSE ones, while on real hardware this loop is still memory bound.

#include "v3d_core.h"
#include "v3d_simd.h"

namespace {

inline u32 clamp255(i32 v) {
    return v < 0 ? 0u : (v > 255 ? 255u : (u32)v);
}

/// x * a / 255 for the channels at bits 0-7 and 16-23.
inline u32 mul_div255_rb(u32 x, u32 a) {
    const u32 t = (x & 0x00FF00FF) * a + 0x00800080;
    return ((t + ((t >> 8) & 0x00FF00FF)) >> 8) & 0x00FF00FF;
}

/// Scales the colour channels of a pixel by a / 255.
inline u32 scale_rgb(u32 px, u32 a) {
    return mul_div255_rb(px, a) | (mul_div255_rb(px >> 8, a) << 8);
}

/// Linear interpolation of two pixels (all four channels), t in 0..256.
__forceinline u32 lerp_px(u32 a, u32 b, u32 t) {
    const u32 it = 256 - t;
    const u32 rb = ((((a & 0x00FF00FF) * it) + ((b & 0x00FF00FF) * t)) >> 8) & 0x00FF00FF;
    const u32 ag = ((((a >> 8) & 0x00FF00FF) * it) + (((b >> 8) & 0x00FF00FF) * t)) & 0xFF00FF00;
    return rb | ag;
}

/// Bilinear texel lookup at (u, v) in texels * 256 of a wrapping level.
__forceinline u32 sample_bilinear(const u32* pix, u32 wl, u32 wmask, u32 hmask, u32 u, u32 v) {
    u -= 128;  // texel centres
    v -= 128;
    const u32 x0 = (u >> 8) & wmask, x1 = (x0 + 1) & wmask;
    const u32 y0 = (v >> 8) & hmask, y1 = (y0 + 1) & hmask;
    const u32* r0 = pix + (y0 << wl);
    const u32* r1 = pix + (y1 << wl);
    const u32 fx = u & 255, fy = v & 255;
    return lerp_px(lerp_px(r0[x0], r0[x1], fx), lerp_px(r1[x0], r1[x1], fx), fy);
}

/// Narrows the covered span [xa, xb) of a row with one edge function `e`
/// (value at the span origin) that changes by `ex` per pixel. Returns false
/// when the row is empty.
__forceinline bool clip_span(i64 e, i64 ex, i32& xa, i32& xb) {
    if (ex > 0) {
        if (e < 0) {
            const i64 n = (-e + ex - 1) / ex;  // first pixel with e + n * ex >= 0
            if (n >= xb) return false;
            if (n > xa) xa = (i32)n;
        }
    } else if (ex < 0) {
        if (e < 0) return false;
        const i64 n = e / -ex + 1;  // one past the last pixel with e + n * ex >= 0
        if (n < xb) xb = (i32)n;
    } else if (e < 0) {
        return false;
    }
    return true;
}

template <u32 M>
__forceinline void shade_span(const V3dTarget* tg, const V3dTri& t, i32 xa, i32 xb, i32 y) {
    constexpr bool TEX = (M & M_TEX) != 0;
    constexpr u32 BLEND = M & M_BLEND_MASK;
    constexpr bool ATEST = TEX && (M & M_ATEST) != 0;
    constexpr bool ZTEST = (M & M_ZTEST) != 0;
    constexpr bool ZWRITE = (M & M_ZWRITE) != 0;
    constexpr bool AFFINE = TEX && (M & M_AFFINE) != 0;
    constexpr bool FLAT = !TEX && (M & M_FLAT) != 0;
    constexpr i32 NC = TEX ? 7 : (FLAT ? 1 : 4);

    const i64 dx = xa - t.rx, dy = y - t.ry;
    const i64 row = (i64)y * tg->stride + xa;
    u32* cp = tg->color + row;
    i32* zp = tg->depth + row;
    i64 z = t.z + t.zx * dx + t.zy * dy;
    const i64 zx = t.zx;
    i32 c[NC], cx[NC];
    // Flat triangles: the colour is computed once (c[0] is unused).
    u32 fr = 0, fg = 0, fb = 0, fa = 255;
    if constexpr (FLAT) {
        c[0] = 0;
        cx[0] = 0;
        fr = clamp255(t.c[0] >> 16);
        fg = clamp255(t.c[1] >> 16);
        fb = clamp255(t.c[2] >> 16);
        const u32 a256 = (u32)(t.c[3] > 0 ? t.c[3] : 0) >> 16;
        fa = a256 > 255 ? 255 : a256;
    } else {
        for (i32 k = 0; k < NC; k++) {
            c[k] = (i32)(t.c[k] + (i64)t.cx[k] * dx + (i64)t.cy[k] * dy);
            cx[k] = t.cx[k];
        }
    }
    i64 uq = 0, vq = 0, qt = 0, uqx = 0, vqx = 0, qtx = 0;
    const V3dTexture* tex = t.tex;
    i32 maxl = 0, lod_base = t.lod_base;
    // Current mip level: affine mapping uses one level for the whole
    // triangle, perspective mapping one per sub-span.
    const u32* mpix = nullptr;
    u32 mwl = 0, mwmask = 0, mhmask = 0, mshift = 0;
    bool bil = false;
    const bool want_bil = (t.mode & M_BILINEAR) != 0;
    // Perspective mapping: exact texture coordinates (texels * 256) every
    // SUB pixels, linear in between.
    constexpr i32 SUB = 16;
    i64 pu = 0, pv = 0, pdu = 0, pdv = 0;
    i32 block = 0;
    if constexpr (TEX) {
        uq = t.uq + t.uqx * dx + t.uqy * dy;
        vq = t.vq + t.vqx * dx + t.vqy * dy;
        uqx = t.uqx;
        vqx = t.vqx;
        maxl = tex->count - 1;
        if constexpr (AFFINE) {
            i32 l = (lod_base + 2) >> 2;
            l = l < 0 ? 0 : (l > maxl ? maxl : l);
            const V3dMip& m = tex->levels[l];
            mpix = m.pixels;
            mwl = (u32)m.wlog2;
            mwmask = (1u << m.wlog2) - 1;
            mhmask = (1u << m.hlog2) - 1;
            mshift = 8 + (u32)l;
            bil = want_bil && l == 0;
        } else {
            qt = t.qt + t.qtx * dx + t.qty * dy;
            qtx = t.qtx;
            const i64 q0 = qt > 0 ? qt : 1;
            pu = uq / q0;
            pv = vq / q0;
        }
    }

    for (i32 n = xb - xa; n > 0; n--) {
        if constexpr (TEX && !AFFINE) {
            if (block == 0) {
                // Start a sub-span: exact coordinates at its end, the step,
                // and the mip level from its middle.
                const i32 len = n < SUB ? n : SUB;
                const i64 qe = qt + qtx * len;
                const i64 q1 = qe > 0 ? qe : 1;
                const i64 u1 = (uq + uqx * len) / q1, v1 = (vq + vqx * len) / q1;
                if (len == SUB) {
                    pdu = (u1 - pu) >> 4;
                    pdv = (v1 - pv) >> 4;
                } else {
                    pdu = (u1 - pu) / len;
                    pdv = (v1 - pv) / len;
                }
                const i64 qm = qt + qtx * (len >> 1);
                i32 l = (lod_base - log2q((u64)(qm > 1 ? qm : 1)) + 2) >> 2;
                l = l < 0 ? 0 : (l > maxl ? maxl : l);
                const V3dMip& m = tex->levels[l];
                mpix = m.pixels;
                mwl = (u32)m.wlog2;
                mwmask = (1u << m.wlog2) - 1;
                mhmask = (1u << m.hlog2) - 1;
                mshift = 8 + (u32)l;
                bil = want_bil && l == 0;
                uq += uqx * len;
                vq += vqx * len;
                qt = qe;
                block = len;
            }
        }
        i32 zz = 0;
        if constexpr (ZTEST || ZWRITE) zz = (i32)(z >> Z_FRAC);
        bool keep = !ZTEST || zz > *zp;
        u32 r = 0, g = 0, b = 0, a = 255;
        if (keep) {
            if constexpr (TEX) {
                u32 texel;
                if constexpr (AFFINE) {
                    const u32 u = (u32)(uq >> T_FRAC), v = (u32)(vq >> T_FRAC);
                    texel = bil ? sample_bilinear(mpix, mwl, mwmask, mhmask, u, v)
                                : mpix[(((v >> mshift) & mhmask) << mwl) | ((u >> mshift) & mwmask)];
                } else {
                    const u32 u = (u32)pu, v = (u32)pv;
                    texel = bil ? sample_bilinear(mpix, mwl, mwmask, mhmask, u, v)
                                : mpix[(((v >> mshift) & mhmask) << mwl) | ((u >> mshift) & mwmask)];
                }
                const u32 ta = texel >> 24;
                const u32 ma = (u32)(c[3] > 0 ? c[3] : 0) >> 16;  // 0..256
                a = (ta * ma) >> 8;
                if constexpr (ATEST) keep = a >= 128;
                const u32 mr = (u32)(c[0] > 0 ? c[0] : 0) >> 8, mg = (u32)(c[1] > 0 ? c[1] : 0) >> 8,
                          mb = (u32)(c[2] > 0 ? c[2] : 0) >> 8;
                i32 ar = c[4] >> 16, ag = c[5] >> 16, ab = c[6] >> 16;
                if constexpr (BLEND == M_ALPHA) {
                    ar = (ar * (i32)ta) >> 8;
                    ag = (ag * (i32)ta) >> 8;
                    ab = (ab * (i32)ta) >> 8;
                }
                r = clamp255((i32)((((texel >> 16) & 255) * mr) >> 8) + ar);
                g = clamp255((i32)((((texel >> 8) & 255) * mg) >> 8) + ag);
                b = clamp255((i32)(((texel & 255) * mb) >> 8) + ab);
            } else if constexpr (FLAT) {
                r = fr;
                g = fg;
                b = fb;
                a = fa;
            } else {
                r = clamp255(c[0] >> 16);
                g = clamp255(c[1] >> 16);
                b = clamp255(c[2] >> 16);
                const u32 a256 = (u32)(c[3] > 0 ? c[3] : 0) >> 16;
                a = a256 > 255 ? 255 : a256;
            }
        }
        if (keep) {
            if constexpr (BLEND == M_OPAQUE) {
                *cp = 0xFF000000u | (r << 16) | (g << 8) | b;
            } else if constexpr (BLEND == M_ALPHA) {
                const u32 d = scale_rgb(*cp, 255 - (a > 255 ? 255 : a));
                const u32 rr = ((d >> 16) & 255) + r, gg = ((d >> 8) & 255) + g, bb = (d & 255) + b;
                *cp = 0xFF000000u | ((rr > 255 ? 255 : rr) << 16) | ((gg > 255 ? 255 : gg) << 8) | (bb > 255 ? 255 : bb);
            } else if constexpr (BLEND == M_ADD) {
                const u32 d = *cp;
                const u32 rr = ((d >> 16) & 255) + r, gg = ((d >> 8) & 255) + g, bb = (d & 255) + b;
                *cp = 0xFF000000u | ((rr > 255 ? 255 : rr) << 16) | ((gg > 255 ? 255 : gg) << 8) | (bb > 255 ? 255 : bb);
            } else {
                // Multiply, faded towards white by the alpha.
                const u32 ia = 255 - (a > 255 ? 255 : a);
                const u32 sr = r + ((255 - r) * ia) / 255, sg = g + ((255 - g) * ia) / 255,
                          sb = b + ((255 - b) * ia) / 255;
                const u32 d = *cp;
                *cp = 0xFF000000u | ((((d >> 16) & 255) * sr / 255) << 16) | ((((d >> 8) & 255) * sg / 255) << 8) |
                      ((d & 255) * sb / 255);
            }
            if constexpr (ZWRITE) *zp = zz;
        }
        z += zx;
        if constexpr (!FLAT) {
            // Kept scalar: the auto-vectorised form spills the colours to
            // memory and uses SSE adds, both slow under QEMU TCG.
#pragma loop(no_vector)
            for (i32 k = 0; k < NC; k++) c[k] += cx[k];
        }
        if constexpr (TEX) {
            if constexpr (AFFINE) {
                uq += uqx;
                vq += vqx;
            } else {
                pu += pdu;
                pv += pdv;
                block--;
            }
        }
        cp++;
        zp++;
    }
}

template <u32 M>
void raster_tri(const V3dTarget* tg, i32 cx0, i32 cy0, i32 cx1, i32 cy1, const V3dTri& t, V3dRasterStats& st) {
    const i32 xs = t.x0 > cx0 ? t.x0 : cx0;
    const i32 xe = t.x1 < cx1 ? t.x1 : cx1;
    const i32 ys = t.y0 > cy0 ? t.y0 : cy0;
    const i32 ye = t.y1 < cy1 ? t.y1 : cy1;
    if (xs >= xe || ys >= ye) return;
    st.triangles++;
    const i32 width = xe - xs;
    const i64 dxs = xs - t.rx, dys = ys - t.ry;
    i64 e0 = t.e[0] + (i64)t.ex[0] * dxs + (i64)t.ey[0] * dys;
    i64 e1 = t.e[1] + (i64)t.ex[1] * dxs + (i64)t.ey[1] * dys;
    i64 e2 = t.e[2] + (i64)t.ex[2] * dxs + (i64)t.ey[2] * dys;
    const i64 ex0 = t.ex[0], ex1 = t.ex[1], ex2 = t.ex[2];
    const i64 ey0 = t.ey[0], ey1 = t.ey[1], ey2 = t.ey[2];
    u32 spans = 0, pixels = 0;
    for (i32 y = ys; y < ye; y++, e0 += ey0, e1 += ey1, e2 += ey2) {
        i32 xa = 0, xb = width;
        if (!clip_span(e0, ex0, xa, xb) || !clip_span(e1, ex1, xa, xb) || !clip_span(e2, ex2, xa, xb)) continue;
        if (xa >= xb) continue;
        spans++;
        pixels += (u32)(xb - xa);
        shade_span<M>(tg, t, xs + xa, xs + xb, y);
    }
    st.spans += spans;
    st.pixels += pixels;
}

typedef void (*RasterFn)(const V3dTarget*, i32, i32, i32, i32, const V3dTri&, V3dRasterStats&);

#define R1(n) raster_tri<(n)>
#define R4(n) R1(n), R1(n + 1), R1(n + 2), R1(n + 3)
#define R16(n) R4(n), R4(n + 4), R4(n + 8), R4(n + 12)
#define R64(n) R16(n), R16(n + 16), R16(n + 32), R16(n + 48)

const RasterFn RASTER[M_COUNT] = {R64(0), R64(64), R64(128), R64(192)};

}  // namespace

V3D_EXPORT void v3d_raster(const V3dTarget* t, i32 x0, i32 y0, i32 x1, i32 y1, const V3dTri* tris, const u32* indices,
                           i32 count, V3dRasterStats* stats) {
    V3dRasterStats st = {0, 0, 0};
    for (i32 i = 0; i < count; i++) {
        const V3dTri& tri = tris[indices[i]];
        RASTER[tri.mode & (M_COUNT - 1)](t, x0, y0, x1, y1, tri, st);
    }
    if (stats) {
        stats->triangles += st.triangles;
        stats->spans += st.spans;
        stats->pixels += st.pixels;
    }
}

V3D_EXPORT void v3d_clear_rect(const V3dTarget* t, i32 x0, i32 y0, i32 x1, i32 y1, const u32* row_colors) {
    if (x0 < 0) x0 = 0;
    if (y0 < 0) y0 = 0;
    if (x1 > t->width) x1 = t->width;
    if (y1 > t->height) y1 = t->height;
    if (x0 >= x1 || y0 >= y1) return;
    const i32 n = x1 - x0;
    for (i32 y = y0; y < y1; y++) {
        u32* c = t->color + (i64)y * t->stride + x0;
        i32* z = t->depth + (i64)y * t->stride + x0;
        const __m128i cv = _mm_set1_epi32((int)(row_colors[y] | 0xFF000000u));
        const __m128i zv = _mm_setzero_si128();
        i32 i = 0;
        for (; i + 4 <= n; i += 4) {
            _mm_storeu_si128((__m128i*)(c + i), cv);
            _mm_storeu_si128((__m128i*)(z + i), zv);
        }
        for (; i < n; i++) {
            c[i] = row_colors[y] | 0xFF000000u;
            z[i] = 0;
        }
    }
}
