// post.cpp - presenting the internal render target: scaling it to the
// window with bilinear filtering.

#include "v3d_core.h"
#include "v3d_simd.h"

namespace {

/// Linear interpolation of two opaque pixels, t in 0..256.
inline u32 lerp_px(u32 a, u32 b, u32 t) {
    const u32 it = 256 - t;
    const u32 rb = ((((a & 0x00FF00FF) * it) + ((b & 0x00FF00FF) * t)) >> 8) & 0x00FF00FF;
    const u32 ag = ((((a >> 8) & 0x00FF00FF) * it) + (((b >> 8) & 0x00FF00FF) * t)) & 0xFF00FF00;
    return rb | ag;
}

/// Rounding-up average of two pixels per byte (like pavgb).
inline u32 avg_px(u32 a, u32 b) {
    return (a | b) - (((a ^ b) & 0xFEFEFEFEu) >> 1);
}

/// 3/4 a + 1/4 b per byte.
inline u32 mix31(u32 a, u32 b) {
    return avg_px(a, avg_px(a, b));
}

inline __m128i mix31_sse(__m128i a, __m128i b) {
    return _mm_avg_epu8(a, _mm_avg_epu8(a, b));
}

/// Horizontally doubles one source row into `out` (2 * sw pixels) with
/// 3/4 - 1/4 weights (pixel centres of the doubled row fall at 1/4 and 3/4).
void double_row(const u32* s, i32 sw, u32* out) {
    if (sw <= 0) return;
    // Edges (and short rows) in scalar code.
    auto scalar = [&](i32 x) {
        const u32 c = s[x], l = s[x > 0 ? x - 1 : 0], r = s[x + 1 < sw ? x + 1 : sw - 1];
        out[2 * x] = mix31(c, l);
        out[2 * x + 1] = mix31(c, r);
    };
    if (sw < 8) {
        for (i32 x = 0; x < sw; x++) scalar(x);
        return;
    }
    scalar(0);
    i32 x = 1;
    for (; x + 4 < sw; x += 4) {
        const __m128i c = _mm_loadu_si128((const __m128i*)(s + x));
        const __m128i l = _mm_loadu_si128((const __m128i*)(s + x - 1));
        const __m128i r = _mm_loadu_si128((const __m128i*)(s + x + 1));
        const __m128i even = mix31_sse(c, l);
        const __m128i odd = mix31_sse(c, r);
        _mm_storeu_si128((__m128i*)(out + 2 * x), _mm_unpacklo_epi32(even, odd));
        _mm_storeu_si128((__m128i*)(out + 2 * x + 4), _mm_unpackhi_epi32(even, odd));
    }
    for (; x < sw; x++) scalar(x);
}

}  // namespace

namespace {

/// Horizontally scales source row `s` (sw pixels) to `dw` pixels.
void scale_row(const u32* s, i32 sw, i32 dw, i64 stepx, u32* out) {
    const i64 maxx = (i64)(sw - 1) << 16;
    i64 fx = (stepx >> 1) - 32768;
    // Scalar on purpose: MSVC's auto-vectorised SSE version of these loops
    // is several times slower under QEMU TCG (SSE multiplies are helper calls).
#pragma loop(no_vector)
    for (i32 x = 0; x < dw; x++, fx += stepx) {
        const i64 cx = fx < 0 ? 0 : (fx > maxx ? maxx : fx);
        const i32 sx = (i32)(cx >> 16);
        const i32 sx1 = sx + 1 < sw ? sx + 1 : sx;
        out[x] = lerp_px(s[sx], s[sx1], (u32)(cx >> 8) & 255);
    }
}

}  // namespace

V3D_EXPORT void v3d_upscale(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dw, i32 dh, i32 dstride, i32 y0,
                            i32 y1) {
    enum { MAXW = 4096 };
    if (sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 || dw > MAXW) return;
    // Separable filter: source rows are scaled horizontally once (two are
    // kept, as consecutive destination rows mostly share them), then each
    // destination row blends two of them vertically.
    u32 rows[2][MAXW];
    i32 row_src[2] = {-1, -1};
    const i64 stepx = ((i64)sw << 16) / dw, stepy = ((i64)sh << 16) / dh;
    const i64 maxy = (i64)(sh - 1) << 16;
    auto get = [&](i32 sy) -> const u32* {
        for (i32 k = 0; k < 2; k++) {
            if (row_src[k] == sy) return rows[k];
        }
        // Replace the row that is not the other one needed (the lower one).
        const i32 k = row_src[0] < row_src[1] ? 0 : 1;
        scale_row(src + (i64)sy * sstride, sw, dw, stepx, rows[k]);
        row_src[k] = sy;
        return rows[k];
    };
    for (i32 y = y0; y < y1; y++) {
        i64 fy = (i64)y * stepy + (stepy >> 1) - 32768;
        fy = fy < 0 ? 0 : (fy > maxy ? maxy : fy);
        const i32 sy = (i32)(fy >> 16);
        const u32 wy = (u32)(fy >> 8) & 255;
        const i32 sy1 = sy + 1 < sh ? sy + 1 : sy;
        u32* d = dst + (i64)y * dstride;
        if (wy == 0 || sy1 == sy) {
            const u32* a = get(sy);
#pragma loop(no_vector)
            for (i32 x = 0; x < dw; x++) d[x] = a[x] | 0xFF000000u;
        } else {
            // Rows only increase within a band, so fetching sy1 evicts an
            // older row, never sy.
            const u32* a = get(sy);
            const u32* b = get(sy1);
#pragma loop(no_vector)
            for (i32 x = 0; x < dw; x++) d[x] = lerp_px(a[x], b[x], wy) | 0xFF000000u;
        }
    }
}

// ---------------------------------------------------------------------------
// Fast 2x upscaling: each source pixel s(x, y) becomes the 2x2 block
//   s           avg(s, right)
//   avg(s, down) avg of all four
// i.e. bilinear sampling at the source pixel corners (the image moves by
// half a source pixel, which is invisible). One SSE2 average per output
// pixel, or a few scalar operations in the SWAR variant (which is faster
// under QEMU TCG, where SSE instructions are emulated by helpers).

V3D_EXPORT void v3d_upscale2x_fast(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dstride, i32 y0, i32 y1) {
    if (sw <= 0 || sh <= 0) return;
    const __m128i opaque = _mm_set1_epi32((int)0xFF000000u);
    i32 sy0 = y0 >> 1, sy1 = (y1 + 1) >> 1;
    if (sy1 > sh) sy1 = sh;
    for (i32 sy = sy0; sy < sy1; sy++) {
        const u32* r0 = src + (i64)sy * sstride;
        const u32* r1 = sy + 1 < sh ? r0 + sstride : r0;
        const i32 ya = 2 * sy, yb = ya + 1;
        u32* da = (ya >= y0 && ya < y1) ? dst + (i64)ya * dstride : nullptr;
        u32* db = (yb >= y0 && yb < y1) ? dst + (i64)yb * dstride : nullptr;
        i32 x = 0;
        for (; x + 5 <= sw; x += 4) {
            const __m128i a = _mm_loadu_si128((const __m128i*)(r0 + x));
            const __m128i b = _mm_loadu_si128((const __m128i*)(r0 + x + 1));
            const __m128i ab = _mm_avg_epu8(a, b);
            if (da) {
                _mm_storeu_si128((__m128i*)(da + 2 * x), _mm_or_si128(_mm_unpacklo_epi32(a, ab), opaque));
                _mm_storeu_si128((__m128i*)(da + 2 * x + 4), _mm_or_si128(_mm_unpackhi_epi32(a, ab), opaque));
            }
            if (db) {
                const __m128i c = _mm_loadu_si128((const __m128i*)(r1 + x));
                const __m128i d = _mm_loadu_si128((const __m128i*)(r1 + x + 1));
                const __m128i ac = _mm_avg_epu8(a, c);
                const __m128i abcd = _mm_avg_epu8(ab, _mm_avg_epu8(c, d));
                _mm_storeu_si128((__m128i*)(db + 2 * x), _mm_or_si128(_mm_unpacklo_epi32(ac, abcd), opaque));
                _mm_storeu_si128((__m128i*)(db + 2 * x + 4), _mm_or_si128(_mm_unpackhi_epi32(ac, abcd), opaque));
            }
        }
        for (; x < sw; x++) {
            const i32 xr = x + 1 < sw ? x + 1 : x;
            const u32 a = r0[x], ab = avg_px(a, r0[xr]);
            if (da) {
                da[2 * x] = a | 0xFF000000u;
                da[2 * x + 1] = ab | 0xFF000000u;
            }
            if (db) {
                const u32 c = r1[x];
                db[2 * x] = avg_px(a, c) | 0xFF000000u;
                db[2 * x + 1] = avg_px(ab, avg_px(c, r1[xr])) | 0xFF000000u;
            }
        }
    }
}

V3D_EXPORT void v3d_upscale2x_swar(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dstride, i32 y0, i32 y1) {
    if (sw <= 0 || sh <= 0) return;
    i32 sy0 = y0 >> 1, sy1 = (y1 + 1) >> 1;
    if (sy1 > sh) sy1 = sh;
    for (i32 sy = sy0; sy < sy1; sy++) {
        const u32* r0 = src + (i64)sy * sstride;
        const u32* r1 = sy + 1 < sh ? r0 + sstride : r0;
        const i32 ya = 2 * sy, yb = ya + 1;
        u32* da = (ya >= y0 && ya < y1) ? dst + (i64)ya * dstride : nullptr;
        u32* db = (yb >= y0 && yb < y1) ? dst + (i64)yb * dstride : nullptr;
        u32 a = r0[0], c = r1[0];
        // Scalar on purpose: MSVC's auto-vectorised SSE version of such
        // loops is several times slower under QEMU TCG.
#pragma loop(no_vector)
        for (i32 x = 0; x < sw; x++) {
            const i32 xr = x + 1 < sw ? x + 1 : x;
            const u32 b = r0[xr], d = r1[xr];
            const u32 ab = avg_px(a, b);
            if (da) {
                u64* p = (u64*)(da + 2 * x);
                *p = (u64)(a | 0xFF000000u) | ((u64)(ab | 0xFF000000u) << 32);
            }
            if (db) {
                u64* p = (u64*)(db + 2 * x);
                *p = (u64)(avg_px(a, c) | 0xFF000000u) | ((u64)(avg_px(ab, avg_px(c, d)) | 0xFF000000u) << 32);
            }
            a = b;
            c = d;
        }
    }
}

namespace {

/// Writes destination row `d` as 3/4 `a` + 1/4 `b` (both doubled rows).
void mix_rows(const u32* a, const u32* b, i32 dw, u32* d) {
    const __m128i opaque = _mm_set1_epi32((int)0xFF000000u);
    i32 x = 0;
    for (; x + 4 <= dw; x += 4) {
        const __m128i va = _mm_loadu_si128((const __m128i*)(a + x));
        const __m128i vb = _mm_loadu_si128((const __m128i*)(b + x));
        _mm_storeu_si128((__m128i*)(d + x), _mm_or_si128(mix31_sse(va, vb), opaque));
    }
#pragma loop(no_vector)
    for (; x < dw; x++) d[x] = mix31(a[x], b[x]) | 0xFF000000u;
}

}  // namespace

V3D_EXPORT void v3d_upscale2x(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dstride, i32 y0, i32 y1) {
    enum { MAXW = 2048 };
    if (sw <= 0 || sh <= 0 || sw > MAXW) return;
    // Rolling window of horizontally doubled source rows (above, current,
    // below). Destination row 2*sy mixes the current row 3:1 with the row
    // above, row 2*sy+1 with the row below.
    u32 bufs[3][2 * MAXW];
    u32* prev = bufs[0];
    u32* cur = bufs[1];
    u32* next = bufs[2];
    const i32 dw = 2 * sw;
    const i32 sy0 = y0 >> 1;
    i32 sy1 = (y1 + 1) >> 1;
    if (sy1 > sh) sy1 = sh;
    if (sy0 >= sy1) return;
    double_row(src + (i64)(sy0 > 0 ? sy0 - 1 : 0) * sstride, sw, prev);
    double_row(src + (i64)sy0 * sstride, sw, cur);
    for (i32 sy = sy0; sy < sy1; sy++) {
        double_row(src + (i64)(sy + 1 < sh ? sy + 1 : sy) * sstride, sw, next);
        const i32 ya = 2 * sy, yb = 2 * sy + 1;
        if (ya >= y0 && ya < y1) mix_rows(cur, prev, dw, dst + (i64)ya * dstride);
        if (yb >= y0 && yb < y1) mix_rows(cur, next, dw, dst + (i64)yb * dstride);
        u32* t = prev;
        prev = cur;
        cur = next;
        next = t;
    }
}
