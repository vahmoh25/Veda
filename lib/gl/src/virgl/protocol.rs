//! The virgl command stream's numbers: virglrenderer's
//! `virgl_protocol.h` and `virgl_hw.h`, and the Gallium enumerations the
//! commands carry (`p_defines.h`).

#![allow(dead_code)]

/// A command's first word: the command, an object type and the length of
/// what follows, in words.
pub const fn cmd0(cmd: u32, obj: u32, len: u32) -> u32 {
    cmd | (obj << 8) | (len << 16)
}

/// The longest command (its length field is 16 bits).
pub const MAX_COMMAND_WORDS: usize = 0xFFFF;

// Commands (`enum virgl_context_cmd`).
pub const CMD_NOP: u32 = 0;
pub const CMD_CREATE_OBJECT: u32 = 1;
pub const CMD_BIND_OBJECT: u32 = 2;
pub const CMD_DESTROY_OBJECT: u32 = 3;
pub const CMD_SET_VIEWPORT_STATE: u32 = 4;
pub const CMD_SET_FRAMEBUFFER_STATE: u32 = 5;
pub const CMD_SET_VERTEX_BUFFERS: u32 = 6;
pub const CMD_CLEAR: u32 = 7;
pub const CMD_DRAW_VBO: u32 = 8;
pub const CMD_RESOURCE_INLINE_WRITE: u32 = 9;
pub const CMD_SET_SAMPLER_VIEWS: u32 = 10;
pub const CMD_SET_INDEX_BUFFER: u32 = 11;
pub const CMD_SET_CONSTANT_BUFFER: u32 = 12;
pub const CMD_SET_STENCIL_REF: u32 = 13;
pub const CMD_SET_BLEND_COLOR: u32 = 14;
pub const CMD_SET_SCISSOR_STATE: u32 = 15;
pub const CMD_BLIT: u32 = 16;
pub const CMD_RESOURCE_COPY_REGION: u32 = 17;
pub const CMD_BIND_SAMPLER_STATES: u32 = 18;
pub const CMD_BEGIN_QUERY: u32 = 19;
pub const CMD_END_QUERY: u32 = 20;
pub const CMD_GET_QUERY_RESULT: u32 = 21;
pub const CMD_SET_POLYGON_STIPPLE: u32 = 22;
pub const CMD_SET_CLIP_STATE: u32 = 23;
pub const CMD_SET_SAMPLE_MASK: u32 = 24;
pub const CMD_SET_STREAMOUT_TARGETS: u32 = 25;
pub const CMD_SET_RENDER_CONDITION: u32 = 26;
pub const CMD_SET_UNIFORM_BUFFER: u32 = 27;
pub const CMD_SET_SUB_CTX: u32 = 28;
pub const CMD_CREATE_SUB_CTX: u32 = 29;
pub const CMD_DESTROY_SUB_CTX: u32 = 30;
pub const CMD_BIND_SHADER: u32 = 31;
pub const CMD_SET_TESS_STATE: u32 = 32;
pub const CMD_SET_MIN_SAMPLES: u32 = 33;
pub const CMD_SET_SHADER_BUFFERS: u32 = 34;
pub const CMD_SET_SHADER_IMAGES: u32 = 35;
pub const CMD_MEMORY_BARRIER: u32 = 36;
pub const CMD_LAUNCH_GRID: u32 = 37;
pub const CMD_SET_FRAMEBUFFER_STATE_NO_ATTACH: u32 = 38;
pub const CMD_TEXTURE_BARRIER: u32 = 39;
pub const CMD_SET_ATOMIC_BUFFERS: u32 = 40;
pub const CMD_SET_DEBUG_FLAGS: u32 = 41;
pub const CMD_GET_QUERY_RESULT_QBO: u32 = 42;
pub const CMD_TRANSFER3D: u32 = 43;
pub const CMD_END_TRANSFERS: u32 = 44;
pub const CMD_COPY_TRANSFER3D: u32 = 45;

// Object types (`enum virgl_object_type`).
pub const OBJ_BLEND: u32 = 1;
pub const OBJ_RASTERIZER: u32 = 2;
pub const OBJ_DSA: u32 = 3;
pub const OBJ_SHADER: u32 = 4;
pub const OBJ_VERTEX_ELEMENTS: u32 = 5;
pub const OBJ_SAMPLER_VIEW: u32 = 6;
pub const OBJ_SAMPLER_STATE: u32 = 7;
pub const OBJ_SURFACE: u32 = 8;
pub const OBJ_QUERY: u32 = 9;
pub const OBJ_STREAMOUT_TARGET: u32 = 10;

/// `VIRGL_COPY_TRANSFER3D_FLAGS`.
pub const COPY_TRANSFER_SYNCHRONIZED: u32 = 1;
pub const COPY_TRANSFER_FROM_HOST: u32 = 2;

// Shader stages (`enum pipe_shader_type`).
pub const SHADER_VERTEX: u32 = 0;
pub const SHADER_FRAGMENT: u32 = 1;

// Resource targets (`enum pipe_texture_target`).
pub const TARGET_BUFFER: u32 = 0;
pub const TARGET_2D: u32 = 2;
pub const TARGET_3D: u32 = 3;
pub const TARGET_CUBE: u32 = 4;
pub const TARGET_2D_ARRAY: u32 = 7;

// Bind flags (`VIRGL_BIND_*`).
pub const BIND_DEPTH_STENCIL: u32 = 1 << 0;
pub const BIND_RENDER_TARGET: u32 = 1 << 1;
pub const BIND_SAMPLER_VIEW: u32 = 1 << 3;
pub const BIND_VERTEX_BUFFER: u32 = 1 << 4;
pub const BIND_INDEX_BUFFER: u32 = 1 << 5;
pub const BIND_CONSTANT_BUFFER: u32 = 1 << 6;
pub const BIND_STREAM_OUTPUT: u32 = 1 << 11;
pub const BIND_CUSTOM: u32 = 1 << 17;
pub const BIND_STAGING: u32 = 1 << 19;

// Clear bits (`PIPE_CLEAR_*`).
pub const CLEAR_DEPTH: u32 = 1 << 0;
pub const CLEAR_STENCIL: u32 = 1 << 1;
/// Color buffer `i` is bit `2 + i`.
pub const CLEAR_COLOR0: u32 = 1 << 2;

// Blit masks (`PIPE_MASK_*`).
pub const MASK_RGBA: u32 = 0xF;
pub const MASK_Z: u32 = 0x10;
pub const MASK_S: u32 = 0x20;

// Primitives (`enum pipe_prim_type`).
pub const PRIM_POINTS: u32 = 0;
pub const PRIM_LINES: u32 = 1;
pub const PRIM_LINE_LOOP: u32 = 2;
pub const PRIM_LINE_STRIP: u32 = 3;
pub const PRIM_TRIANGLES: u32 = 4;
pub const PRIM_TRIANGLE_STRIP: u32 = 5;
pub const PRIM_TRIANGLE_FAN: u32 = 6;

// Blend functions and factors.
pub const BLEND_ADD: u32 = 0;
pub const BLEND_SUBTRACT: u32 = 1;
pub const BLEND_REVERSE_SUBTRACT: u32 = 2;
pub const BLEND_MIN: u32 = 3;
pub const BLEND_MAX: u32 = 4;
pub const FACTOR_ONE: u32 = 0x01;
pub const FACTOR_SRC_COLOR: u32 = 0x02;
pub const FACTOR_SRC_ALPHA: u32 = 0x03;
pub const FACTOR_DST_ALPHA: u32 = 0x04;
pub const FACTOR_DST_COLOR: u32 = 0x05;
pub const FACTOR_SRC_ALPHA_SATURATE: u32 = 0x06;
pub const FACTOR_CONST_COLOR: u32 = 0x07;
pub const FACTOR_CONST_ALPHA: u32 = 0x08;
pub const FACTOR_ZERO: u32 = 0x11;
pub const FACTOR_INV_SRC_COLOR: u32 = 0x12;
pub const FACTOR_INV_SRC_ALPHA: u32 = 0x13;
pub const FACTOR_INV_DST_ALPHA: u32 = 0x14;
pub const FACTOR_INV_DST_COLOR: u32 = 0x15;
pub const FACTOR_INV_CONST_COLOR: u32 = 0x17;
pub const FACTOR_INV_CONST_ALPHA: u32 = 0x18;

// Faces (`PIPE_FACE_*`).
pub const FACE_NONE: u32 = 0;
pub const FACE_FRONT: u32 = 1;
pub const FACE_BACK: u32 = 2;
pub const FACE_FRONT_AND_BACK: u32 = 3;

// Texture wrapping and filtering.
pub const WRAP_REPEAT: u32 = 0;
pub const WRAP_CLAMP_TO_EDGE: u32 = 2;
pub const WRAP_MIRROR_REPEAT: u32 = 4;
pub const FILTER_NEAREST: u32 = 0;
pub const FILTER_LINEAR: u32 = 1;
pub const MIPFILTER_NEAREST: u32 = 0;
pub const MIPFILTER_LINEAR: u32 = 1;
pub const MIPFILTER_NONE: u32 = 2;

// Queries (`PIPE_QUERY_*`).
pub const QUERY_OCCLUSION_PREDICATE: u32 = 1;
pub const QUERY_OCCLUSION_PREDICATE_CONSERVATIVE: u32 = 2;

/// `struct virgl_host_query_state`'s `query_state` once the result is in.
pub const QUERY_STATE_DONE: u32 = 1;

// Capability bits (`virgl_caps_v2.capability_bits`).
pub const CAP_TEXTURE_VIEW: u32 = 1 << 1;
/// Copies between images with `glCopyImageSubData` (otherwise the host
/// copies through framebuffers, or from guest storage).
pub const CAP_COPY_IMAGE: u32 = 1 << 3;
pub const CAP_COPY_TRANSFER: u32 = 1 << 26;
pub const CAP_HOST_IS_GLES: u32 = 1 << 19;
pub const CAP_TRANSFORM_FEEDBACK3: u32 = 1 << 23;
// `capability_bits_v2`.
pub const CAP2_COPY_TRANSFER_BOTH_DIRECTIONS: u32 = 1 << 7;
pub const CAP2_TEXTURE_SHADOW_LOD: u32 = 1 << 10;

// Formats (`enum virgl_formats`) this renderer uses.
pub mod format {
    pub const B8G8R8A8_UNORM: u32 = 1;
    pub const B8G8R8X8_UNORM: u32 = 2;
    pub const B5G6R5_UNORM: u32 = 7;
    pub const R10G10B10A2_UNORM: u32 = 8;
    pub const Z16_UNORM: u32 = 16;
    pub const Z32_FLOAT: u32 = 18;
    pub const S8_UINT_Z24_UNORM: u32 = 20;
    pub const Z24X8_UNORM: u32 = 21;
    pub const R32_FLOAT: u32 = 28;
    pub const R32G32_FLOAT: u32 = 29;
    pub const R32G32B32_FLOAT: u32 = 30;
    pub const R32G32B32A32_FLOAT: u32 = 31;
    pub const R32_UNORM: u32 = 32;
    pub const R32G32_UNORM: u32 = 33;
    pub const R32G32B32_UNORM: u32 = 34;
    pub const R32G32B32A32_UNORM: u32 = 35;
    pub const R32_USCALED: u32 = 36;
    pub const R32G32_USCALED: u32 = 37;
    pub const R32G32B32_USCALED: u32 = 38;
    pub const R32G32B32A32_USCALED: u32 = 39;
    pub const R32_SNORM: u32 = 40;
    pub const R32G32_SNORM: u32 = 41;
    pub const R32G32B32_SNORM: u32 = 42;
    pub const R32G32B32A32_SNORM: u32 = 43;
    pub const R32_SSCALED: u32 = 44;
    pub const R32G32_SSCALED: u32 = 45;
    pub const R32G32B32_SSCALED: u32 = 46;
    pub const R32G32B32A32_SSCALED: u32 = 47;
    pub const R16_UNORM: u32 = 48;
    pub const R16G16_UNORM: u32 = 49;
    pub const R16G16B16_UNORM: u32 = 50;
    pub const R16G16B16A16_UNORM: u32 = 51;
    pub const R16_USCALED: u32 = 52;
    pub const R16G16_USCALED: u32 = 53;
    pub const R16G16B16_USCALED: u32 = 54;
    pub const R16G16B16A16_USCALED: u32 = 55;
    pub const R16_SNORM: u32 = 56;
    pub const R16G16_SNORM: u32 = 57;
    pub const R16G16B16_SNORM: u32 = 58;
    pub const R16G16B16A16_SNORM: u32 = 59;
    pub const R16_SSCALED: u32 = 60;
    pub const R16G16_SSCALED: u32 = 61;
    pub const R16G16B16_SSCALED: u32 = 62;
    pub const R16G16B16A16_SSCALED: u32 = 63;
    pub const R8_UNORM: u32 = 64;
    pub const R8G8_UNORM: u32 = 65;
    pub const R8G8B8_UNORM: u32 = 66;
    pub const R8G8B8A8_UNORM: u32 = 67;
    pub const R8_USCALED: u32 = 69;
    pub const R8G8_USCALED: u32 = 70;
    pub const R8G8B8_USCALED: u32 = 71;
    pub const R8G8B8A8_USCALED: u32 = 72;
    pub const R8_SNORM: u32 = 74;
    pub const R8G8_SNORM: u32 = 75;
    pub const R8G8B8_SNORM: u32 = 76;
    pub const R8G8B8A8_SNORM: u32 = 77;
    pub const R8_SSCALED: u32 = 82;
    pub const R8G8_SSCALED: u32 = 83;
    pub const R8G8B8_SSCALED: u32 = 84;
    pub const R8G8B8A8_SSCALED: u32 = 85;
    pub const R32_FIXED: u32 = 87;
    pub const R32G32_FIXED: u32 = 88;
    pub const R32G32B32_FIXED: u32 = 89;
    pub const R32G32B32A32_FIXED: u32 = 90;
    pub const R16_FLOAT: u32 = 91;
    pub const R16G16_FLOAT: u32 = 92;
    pub const R16G16B16_FLOAT: u32 = 93;
    pub const R16G16B16A16_FLOAT: u32 = 94;
    pub const R8G8B8A8_SRGB: u32 = 104;
    pub const R10G10B10A2_USCALED: u32 = 123;
    pub const R11G11B10_FLOAT: u32 = 124;
    pub const R9G9B9E5_FLOAT: u32 = 125;
    pub const Z32_FLOAT_S8X24_UINT: u32 = 126;
    pub const R8G8B8X8_UNORM: u32 = 134;
    pub const X24S8_UINT: u32 = 136;
    pub const R10G10B10A2_SSCALED: u32 = 172;
    pub const R10G10B10A2_SNORM: u32 = 173;
    pub const R8_UINT: u32 = 177;
    pub const R8G8_UINT: u32 = 178;
    pub const R8G8B8_UINT: u32 = 179;
    pub const R8G8B8A8_UINT: u32 = 180;
    pub const R8_SINT: u32 = 181;
    pub const R8G8_SINT: u32 = 182;
    pub const R8G8B8_SINT: u32 = 183;
    pub const R8G8B8A8_SINT: u32 = 184;
    pub const R16_UINT: u32 = 185;
    pub const R16G16_UINT: u32 = 186;
    pub const R16G16B16_UINT: u32 = 187;
    pub const R16G16B16A16_UINT: u32 = 188;
    pub const R16_SINT: u32 = 189;
    pub const R16G16_SINT: u32 = 190;
    pub const R16G16B16_SINT: u32 = 191;
    pub const R16G16B16A16_SINT: u32 = 192;
    pub const R32_UINT: u32 = 193;
    pub const R32G32_UINT: u32 = 194;
    pub const R32G32B32_UINT: u32 = 195;
    pub const R32G32B32A32_UINT: u32 = 196;
    pub const R32_SINT: u32 = 197;
    pub const R32G32_SINT: u32 = 198;
    pub const R32G32B32_SINT: u32 = 199;
    pub const R32G32B32A32_SINT: u32 = 200;
    pub const R8G8B8X8_SNORM: u32 = 229;
    pub const R8G8B8X8_SRGB: u32 = 230;
    pub const R8G8B8X8_UINT: u32 = 231;
    pub const R8G8B8X8_SINT: u32 = 232;
    pub const R16G16B16X16_FLOAT: u32 = 236;
    pub const R16G16B16X16_UINT: u32 = 237;
    pub const R16G16B16X16_SINT: u32 = 238;
    pub const R10G10B10A2_UINT: u32 = 253;
    pub const A4B4G4R4_UNORM: u32 = 311;
    pub const A1B5G5R5_UNORM: u32 = 331;
}
