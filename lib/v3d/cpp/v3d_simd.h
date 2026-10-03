// v3d_simd.h - compiler intrinsics for the freestanding build.
#pragma once

// MSVC's intrinsic headers include the CRT's <malloc.h> (for _mm_malloc),
// which is not on the include path of a CRT-free build: pretend it was
// already included. Nothing here uses _mm_malloc.
#ifndef _INC_MALLOC
#define _INC_MALLOC
#endif

#include <intrin.h>
#include <emmintrin.h>

#include "v3d_core.h"

/// Number of significant bits of `x` (0 for 0).
static __forceinline i32 bitlen64(u64 x) {
    unsigned long idx;
    return _BitScanReverse64(&idx, x) ? (i32)idx + 1 : 0;
}

/// log2(x) in quarter steps (x >= 1): 4 * floor(log2 x) plus the next two
/// mantissa bits as a linear fraction.
static __forceinline i32 log2q(u64 x) {
    unsigned long idx;
    if (!_BitScanReverse64(&idx, x | 1)) return 0;
    const i32 frac = idx >= 2 ? (i32)((x >> (idx - 2)) & 3) : (i32)((x << (2 - idx)) & 3);
    return (i32)idx * 4 + frac;
}
