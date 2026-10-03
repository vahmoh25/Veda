// transform.cpp - fixed-point vertex transformation, lighting, fog and
// projection.
//
// Per vertex this is a 3x4 matrix multiply, a few 2.14 dot products for the
// lights and two integer divisions for the projection: all integer work,
// which is ~20x cheaper than the equivalent float code under QEMU TCG.

#include "v3d_core.h"
#include "v3d_simd.h"

static_assert(sizeof(V3dVertex) == 32, "V3dVertex layout");
static_assert(sizeof(V3dTVert) == 48, "V3dTVert layout");
static_assert(sizeof(V3dPointLight) == 32, "V3dPointLight layout");

static inline i32 clamp_i32(i64 v, i32 lo, i32 hi) {
    return v < lo ? lo : (v > hi ? hi : (i32)v);
}

static inline i32 min_i32(i32 a, i32 b) {
    return a < b ? a : b;
}

static inline i32 max_i32(i32 a, i32 b) {
    return a > b ? a : b;
}

static inline i32 dot14(i32 x, i32 y, i32 z, const i32* d) {
    return (x * d[0] + y * d[1] + z * d[2]) >> 14;
}

/// Integer square root of a non-negative 64-bit value (via the FPU: one
/// conversion and one sqrtsd are cheaper than a bitwise loop under TCG).
static inline i64 isqrt64(i64 v) {
    if (v <= 0) return 0;
    double d = (double)v;
    i64 r = (i64)_mm_cvtsd_f64(_mm_sqrt_sd(_mm_setzero_pd(), _mm_set_sd(d)));
    // Correct the rounding of the floating-point result.
    while (r > 0 && r * r > v) r--;
    while ((r + 1) * (r + 1) <= v) r++;
    return r;
}

/// Computes the outcode and (when in front of the near plane) the screen
/// position and depth of a clip-space vertex.
static inline void project(const V3dXform* xf, V3dTVert* v) {
    u32 oc = 0;
    const i32 w = v->cw;
    if (w < xf->near_w) oc |= OC_NEAR;
    if (w > xf->far_w) oc |= OC_FAR;
    if (v->cx < -w) oc |= OC_LEFT;
    if (v->cx > w) oc |= OC_RIGHT;
    if (v->cy < -w) oc |= OC_BOTTOM;
    if (v->cy > w) oc |= OC_TOP;
    const i64 gw = ((i64)(w > 0 ? w : 0) * xf->guard) >> 16;
    if (v->cx < -gw || v->cx > gw || v->cy < -gw || v->cy > gw) oc |= OC_GUARD;
    if (!(oc & OC_NEAR)) {
        v->sx = xf->vp_x + (i32)(((i64)v->cx * xf->vp_sx) / w);
        v->sy = xf->vp_y - (i32)(((i64)v->cy * xf->vp_sy) / w);
        const i64 q = xf->q_scale / w;
        v->q = q > 0x7FFFFFFF ? 0x7FFFFFFF : (i32)q;
    } else {
        v->sx = 0;
        v->sy = 0;
        v->q = 0;
    }
    v->outcode = (u16)oc;
}

V3D_EXPORT void v3d_project(const V3dXform* xf, V3dTVert* v) {
    project(xf, v);
}

/// Lighting, vertex colour, specular, emission and fog for one vertex.
static inline void shade(const V3dXform* xf, const V3dVertex& v, i32 w, V3dTVert& o) {
    const u32 flags = xf->flags;
    i32 r, g, b;
    i32 ar = xf->emit_rgb[0], ag = xf->emit_rgb[1], ab = xf->emit_rgb[2];
    if (flags & XF_LIT) {
        const i32 nx = v.nx, ny = v.ny, nz = v.nz;
        i32 ndl = dot14(nx, ny, nz, xf->light_dir);
        if (flags & XF_TWO_SIDED) {
            ndl = ndl < 0 ? -ndl : ndl;
        } else {
            ndl = max_i32(ndl, 0);
        }
        const i32 t = (dot14(nx, ny, nz, xf->up_dir) + 16384) >> 1;  // 0..16384
        r = xf->ground_rgb[0] + (((xf->sky_rgb[0] - xf->ground_rgb[0]) * t) >> 14) + ((xf->sun_rgb[0] * ndl) >> 14);
        g = xf->ground_rgb[1] + (((xf->sky_rgb[1] - xf->ground_rgb[1]) * t) >> 14) + ((xf->sun_rgb[1] * ndl) >> 14);
        b = xf->ground_rgb[2] + (((xf->sky_rgb[2] - xf->ground_rgb[2]) * t) >> 14) + ((xf->sun_rgb[2] * ndl) >> 14);
        for (i32 i = 0; i < xf->num_points; i++) {
            const V3dPointLight& p = xf->points[i];
            // Work in 8.8 units so that squares fit comfortably in 64 bits.
            const i64 dx = ((i64)p.pos[0] - v.x) >> 8;
            const i64 dy = ((i64)p.pos[1] - v.y) >> 8;
            const i64 dz = ((i64)p.pos[2] - v.z) >> 8;
            const i64 d2 = dx * dx + dy * dy + dz * dz;
            const i64 rr = (i64)(p.radius >> 8);
            const i64 r2 = rr * rr;
            if (d2 >= r2 || r2 <= 0) continue;
            i64 ndd = nx * dx + ny * dy + nz * dz;  // 2.14 x 8.8
            if (flags & XF_TWO_SIDED) {
                ndd = ndd < 0 ? -ndd : ndd;
            } else if (ndd <= 0) {
                continue;
            }
            i32 att = (i32)(((r2 - d2) << 14) / r2);  // 1 - d^2/r^2, 2.14
            att = (att * att) >> 14;
            const i64 len = isqrt64(d2);  // 8.8
            const i32 lam = len > 0 ? (i32)min_i32((i32)(ndd / len), 16384) : 16384;
            const i32 k = (lam * att) >> 14;
            r += (p.rgb[0] * k) >> 14;
            g += (p.rgb[1] * k) >> 14;
            b += (p.rgb[2] * k) >> 14;
        }
        if (flags & XF_SPECULAR) {
            i32 s = dot14(nx, ny, nz, xf->half_dir);
            if (s > 0) {
                for (i32 i = 0; i < xf->spec_power; i++) s = (s * s) >> 14;
                ar += (xf->spec_rgb[0] * s) >> 14;
                ag += (xf->spec_rgb[1] * s) >> 14;
                ab += (xf->spec_rgb[2] * s) >> 14;
            }
        }
    } else {
        r = xf->base_rgba[0];
        g = xf->base_rgba[1];
        b = xf->base_rgba[2];
    }
    i32 a = xf->base_rgba[3];
    if (flags & XF_VCOLOR) {
        const u32 c = v.color;
        const i32 vr = (c >> 16) & 255, vg = (c >> 8) & 255, vb = c & 255, va = c >> 24;
        r = (r * (vr + (vr >> 7))) >> 8;
        g = (g * (vg + (vg >> 7))) >> 8;
        b = (b * (vb + (vb >> 7))) >> 8;
        a = (a * (va + (va >> 7))) >> 8;
    }
    if (flags & XF_FOG) {
        const i64 f64 = ((i64)(w - xf->fog_start) * xf->fog_mul) >> 32;
        const i32 f = (i32)(f64 < 0 ? 0 : (f64 > xf->fog_max ? xf->fog_max : f64));
        const i32 k = 256 - f;
        if (flags & XF_FOG_FADE) {
            // Blended materials (glows, smoke) vanish into the fog: fading
            // the alpha fades their premultiplied contribution.
            a = (a * k) >> 8;
        } else {
            r = (r * k) >> 8;
            g = (g * k) >> 8;
            b = (b * k) >> 8;
            ar = ((ar * k) >> 8) + ((xf->fog_rgb[0] * f) >> 8);
            ag = ((ag * k) >> 8) + ((xf->fog_rgb[1] * f) >> 8);
            ab = ((ab * k) >> 8) + ((xf->fog_rgb[2] * f) >> 8);
        }
    }
    o.mul[0] = (u16)clamp_i32(r, 0, 0xFFFF);
    o.mul[1] = (u16)clamp_i32(g, 0, 0xFFFF);
    o.mul[2] = (u16)clamp_i32(b, 0, 0xFFFF);
    o.mul[3] = (u16)clamp_i32(a, 0, 256);
    o.add[0] = (u16)clamp_i32(ar, 0, 0xFFFF);
    o.add[1] = (u16)clamp_i32(ag, 0, 0xFFFF);
    o.add[2] = (u16)clamp_i32(ab, 0, 0xFFFF);
}

V3D_EXPORT void v3d_transform(const V3dXform* xf, const V3dVertex* in, i32 count, V3dTVert* out) {
    const i32* m = xf->mvp;
    const i64 m0 = m[0], m1 = m[1], m2 = m[2], m4 = m[4], m5 = m[5], m6 = m[6], m12 = m[12], m13 = m[13], m14 = m[14];
    const i64 t0 = (i64)m[3] << 16, t1 = (i64)m[7] << 16, t3 = (i64)m[15] << 16;
    const i64 lim = (i64)1 << 46;
    for (i32 i = 0; i < count; i++) {
        const V3dVertex& v = in[i];
        V3dTVert& o = out[i];
        const i64 x = v.x, y = v.y, z = v.z;
        i64 cx = m0 * x + m1 * y + m2 * z + t0;
        i64 cy = m4 * x + m5 * y + m6 * z + t1;
        i64 cw = m12 * x + m13 * y + m14 * z + t3;
        cx = cx < -lim ? -lim : (cx > lim ? lim : cx);
        cy = cy < -lim ? -lim : (cy > lim ? lim : cy);
        cw = cw < -lim ? -lim : (cw > lim ? lim : cw);
        o.cx = (i32)(cx >> 16);
        o.cy = (i32)(cy >> 16);
        o.cw = (i32)(cw >> 16);
        project(xf, &o);
        o.u = v.u;
        o.v = v.v;
        shade(xf, v, o.cw, o);
    }
}
