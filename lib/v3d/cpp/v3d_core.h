// v3d_core.h - the C++ core of the Vindows software renderer.
//
// Freestanding MSVC C++20: no CRT, no STL, no exceptions, no RTTI. Every
// structure here is mirrored by a #[repr(C)] struct in lib/v3d/src/ffi.rs;
// keep the two in sync.
//
// The whole pipeline after the per-draw matrix setup is integer/fixed point:
// Vindows usually runs under QEMU's TCG emulator, where scalar integer
// operations cost ~0.5 ns, integer division ~1 ns, but every floating-point
// instruction ~6-30 ns (and packed float conversions far more). Fixed-point
// formats used throughout:
//
//   * positions, clip coordinates, texture coordinates: 16.16
//   * unit normals and directions: 2.14 (1.0 = 16384)
//   * light colours / multipliers: 8.8 (1.0 = 256)
//   * additive colours: output units 0..255 scaled by 256
//   * screen coordinates: 28.4 (1/16 pixel)
//   * depth: q = near / w scaled to 2^30 at the near plane (bigger = closer)

#pragma once

typedef signed char i8;
typedef unsigned char u8;
typedef short i16;
typedef unsigned short u16;
typedef int i32;
typedef unsigned int u32;
typedef long long i64;
typedef unsigned long long u64;

#define V3D_EXPORT extern "C"

// ---------------------------------------------------------------------------
// Vertices

/// A mesh vertex in fixed point (32 bytes).
struct V3dVertex {
    i32 x, y, z;       // object-space position, 16.16
    i16 nx, ny, nz;    // unit normal, 2.14
    i16 pad;
    i32 u, v;          // texture coordinates, 16.16 (1.0 = one repeat)
    u32 color;         // straight-alpha 0xAARRGGBB
};

/// A transformed (clip-space, lit) vertex (48 bytes).
struct V3dTVert {
    i32 cx, cy, cw;    // clip space, 16.16 (cw = view depth)
    i32 sx, sy;        // screen position, 28.4 (valid without OC_NEAR)
    i32 q;             // near / w * 2^30 (valid without OC_NEAR)
    i32 u, v;          // texture coordinates, 16.16
    u16 mul[4];        // r, g, b multipliers 8.8; a = alpha 0..256
    u16 add[3];        // additive r, g, b in output units * 256
    u16 outcode;       // OC_* bits
};

enum : u32 {
    OC_NEAR = 1,       // w < near
    OC_FAR = 2,        // w > far
    OC_LEFT = 4,       // x < -w
    OC_RIGHT = 8,      // x > w
    OC_BOTTOM = 16,    // y < -w
    OC_TOP = 32,       // y > w
    OC_GUARD = 64,     // outside the guard band (needs clipping)
    OC_REJECT = OC_NEAR | OC_FAR | OC_LEFT | OC_RIGHT | OC_BOTTOM | OC_TOP,
    OC_CLIP = OC_NEAR | OC_GUARD,
};

// ---------------------------------------------------------------------------
// Transform and lighting

struct V3dPointLight {
    i32 pos[3];        // object space, 16.16
    i32 radius;        // 16.16
    i32 rgb[3];        // colour x material diffuse, 8.8
    i32 pad;
};

enum : u32 {
    XF_LIT = 1,         // compute lighting (otherwise mul = base colour)
    XF_VCOLOR = 2,      // multiply by the vertex colour
    XF_FOG = 4,         // apply distance fog
    XF_FOG_FADE = 8,    // fog only fades the colour (additive materials)
    XF_SPECULAR = 16,   // Blinn-Phong specular highlight
    XF_TWO_SIDED = 32,  // light both sides (|n.l|)
};

enum { V3D_MAX_POINT_LIGHTS = 4 };

struct V3dXform {
    i32 mvp[16];        // object -> clip, row major, 16.16 (row 2 unused)
    i32 light_dir[4];   // towards the sun (object space), 2.14
    i32 up_dir[4];      // world up (object space), 2.14
    i32 half_dir[4];    // Blinn-Phong half vector (object space), 2.14
    i32 sun_rgb[4];     // sun colour x material, 8.8
    i32 sky_rgb[4];     // ambient light from above x material, 8.8
    i32 ground_rgb[4];  // ambient light from below x material, 8.8
    i32 spec_rgb[4];    // specular colour, output units * 256
    i32 emit_rgb[4];    // emissive colour, output units * 256
    i32 base_rgba[4];   // unlit colour multiplier, 8.8 (alpha 0..256)
    i32 fog_rgb[4];     // fog colour, output units * 256
    i32 fog_start;      // view depth where fog starts, 16.16
    i32 fog_mul;        // fog = (w - start) * fog_mul >> 16 (0..256)
    i32 fog_max;        // maximum fog, 0..256
    i32 spec_power;     // shininess = 2^spec_power
    u32 flags;          // XF_*
    i32 num_points;     // point lights used
    V3dPointLight points[V3D_MAX_POINT_LIGHTS];
    i32 near_w;         // near plane, 16.16
    i32 far_w;          // far plane, 16.16
    i32 guard;          // guard band as a multiple of w, 16.16
    i32 vp_x, vp_y;     // viewport centre, 28.4
    i32 vp_sx, vp_sy;   // viewport half size, 28.4
    i32 pad;
    i64 q_scale;        // near (16.16) << 30
};

// ---------------------------------------------------------------------------
// Textures

struct V3dMip {
    const u32* pixels;  // premultiplied 0xAARRGGBB
    i32 wlog2, hlog2;
};

enum { V3D_MAX_MIPS = 13 };

struct V3dTexture {
    V3dMip levels[V3D_MAX_MIPS];
    i32 count;          // number of levels
    u32 flags;
};

// ---------------------------------------------------------------------------
// Triangle setup

enum : u32 {
    M_TEX = 1,          // textured (modulate + add)
    M_BLEND_MASK = 6,   // blend mode
    M_OPAQUE = 0,
    M_ALPHA = 2,        // premultiplied source-over
    M_ADD = 4,          // additive
    M_MUL = 6,          // multiply (darken)
    M_ATEST = 8,        // alpha test (discard alpha < 128)
    M_ZTEST = 16,       // depth test
    M_ZWRITE = 32,      // depth write
    M_AFFINE = 64,      // affine texture mapping (screen-aligned quads)
    M_FLAT = 128,       // constant colour (untextured, set by the setup)
    M_COUNT = 256,      // rasteriser variants (bits below this select one)
    M_BILINEAR = 256,   // bilinear filtering when magnifying (mip level 0)
};

/// Work counters filled by v3d_raster (for profiling).
struct V3dRasterStats {
    u64 triangles;      // triangle/tile pairs visited
    u64 spans;          // non-empty rows
    u64 pixels;         // pixels covered (before the depth test)
};

enum : u32 {
    CULL_NONE = 0,
    CULL_BACK = 1,
    CULL_FRONT = 2,
};

struct V3dSetup {
    i32 width, height;  // render target size in pixels
    u32 mode;           // M_* bits
    u32 cull;           // CULL_*
    const V3dTexture* tex;  // texture when M_TEX
    i32 lod_bias;       // mip bias in quarter levels
    i32 pad;
};

/// A set-up triangle ready for rasterisation. All planes are evaluated
/// relative to the reference pixel (rx, ry).
struct V3dTri {
    i32 x0, y0, x1, y1;     // pixel bounding box [x0, x1) x [y0, y1)
    i32 rx, ry;             // reference pixel
    u32 mode;               // M_* bits
    i32 lod_base;           // per-pixel lod = lod_base - log2(qt), quarter levels
    i64 e[3];               // edge functions at the reference pixel centre
    i32 ex[3], ey[3];       // edge steps per pixel
    i64 z, zx, zy;          // depth q with Z_FRAC extra fraction bits
    i32 c[7], cx[7], cy[7]; // colours 8.16: mul r,g,b,a (or final rgba) + add r,g,b
    i32 pad;
    i64 uq, uqx, uqy;       // u * qt (u in texels * 256, rebased)
    i64 vq, vqx, vqy;
    i64 qt, qtx, qty;       // per-triangle normalised q for texturing
    const V3dTexture* tex;
};

enum { Z_FRAC = 8, T_FRAC = 4, C_FRAC = 8 };

// ---------------------------------------------------------------------------
// Rasterisation

struct V3dTarget {
    u32* color;
    i32* depth;
    i32 stride;             // pixels per row (both buffers)
    i32 width, height;
};

// ---------------------------------------------------------------------------
// Exported functions

/// Transforms and lights `count` vertices.
V3D_EXPORT void v3d_transform(const V3dXform* xf, const V3dVertex* in, i32 count, V3dTVert* out);

/// Computes the screen position, depth and outcode of a clip-space vertex
/// (after clipping created it).
V3D_EXPORT void v3d_project(const V3dXform* xf, V3dTVert* v);

/// Sets up triangles. `indices` index `verts` (three per triangle). Writes
/// accepted triangles to `out` (room for `count` needed) and returns how
/// many; triangles that need clipping are listed in `deferred` (their
/// triangle numbers) and counted in `*deferred_count`.
V3D_EXPORT i32 v3d_setup_batch(const V3dSetup* s, const V3dTVert* verts, const u32* indices, i32 count, V3dTri* out,
                               u32* deferred, i32* deferred_count);

/// Sets up one triangle; returns 1 if it is visible (written to `out`).
V3D_EXPORT i32 v3d_setup_tri(const V3dSetup* s, const V3dTVert* a, const V3dTVert* b, const V3dTVert* c, V3dTri* out);

/// Fills rows [y0, y1) x [x0, x1) of the target: colour from `row_colors`
/// (one per target row), depth 0.
V3D_EXPORT void v3d_clear_rect(const V3dTarget* t, i32 x0, i32 y0, i32 x1, i32 y1, const u32* row_colors);

/// Rasterises triangles `tris[indices[i]]` clipped to the rectangle and
/// adds to `stats` (may be null).
V3D_EXPORT void v3d_raster(const V3dTarget* t, i32 x0, i32 y0, i32 x1, i32 y1, const V3dTri* tris, const u32* indices,
                           i32 count, V3dRasterStats* stats);

/// Bilinear scaling of the opaque source into rows [y0, y1) of `dst`.
V3D_EXPORT void v3d_upscale(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dw, i32 dh, i32 dstride, i32 y0,
                            i32 y1);

/// Exactly 2x upscaling with bilinear filtering (3:1 weights, SSE2), rows
/// [y0, y1).
V3D_EXPORT void v3d_upscale2x(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dstride, i32 y0, i32 y1);

/// Faster 2x upscaling (bilinear at the source pixel corners), SSE2.
V3D_EXPORT void v3d_upscale2x_fast(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dstride, i32 y0, i32 y1);

/// The same filter as v3d_upscale2x_fast in scalar SWAR code.
V3D_EXPORT void v3d_upscale2x_swar(const u32* src, i32 sw, i32 sh, i32 sstride, u32* dst, i32 dstride, i32 y0, i32 y1);
