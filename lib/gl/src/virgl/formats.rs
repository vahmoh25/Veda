//! How each storage format is kept on the host.
//!
//! Most formats have a virgl format with the same memory layout. Where the
//! host lacks one, a wider format stands in: the luminance and alpha
//! formats become red and red-green ones read through a swizzle, RGB
//! formats without an `X` variant become RGBA ones whose alpha reads as
//! one, and packed formats the host cannot sample (5-5-5-1 on Direct3D
//! hosts, for one) are converted to 8 or 16 bits per component on the way
//! in and out.

use super::caps::HostCaps;
use super::protocol::format as vf;
use crate::format::{Class, Format};

/// `PIPE_SWIZZLE_0` and `PIPE_SWIZZLE_1`.
const Z: u8 = 4;
const O: u8 = 5;
const ID: [u8; 4] = [0, 1, 2, 3];

/// A format as the host keeps it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostFormat {
    /// The virgl format.
    pub virgl: u32,
    /// The layout of the host's texels, if it differs from the storage
    /// format's (texels are converted on transfers).
    pub layout: Option<Format>,
    /// How the host's components map to what samplers return.
    pub swizzle: [u8; 4],
    /// Whether the host can render to it.
    pub renderable: bool,
}

impl HostFormat {
    /// The format whose layout transfers use.
    pub fn transfer_format(&self, f: Format) -> Format {
        self.layout.unwrap_or(f)
    }
}

/// The candidates for a format, best first: (virgl format, layout if it
/// differs, swizzle).
fn candidates(f: Format) -> &'static [(u32, Option<Format>, [u8; 4])] {
    use Format::*;
    const RGB1: [u8; 4] = [0, 1, 2, O];
    match f {
        R8Unorm => &[(vf::R8_UNORM, None, ID)],
        R8Snorm => &[(vf::R8_SNORM, None, ID)],
        R8Uint => &[(vf::R8_UINT, None, ID)],
        R8Sint => &[(vf::R8_SINT, None, ID)],
        Rg8Unorm => &[(vf::R8G8_UNORM, None, ID)],
        Rg8Snorm => &[(vf::R8G8_SNORM, None, ID)],
        Rg8Uint => &[(vf::R8G8_UINT, None, ID)],
        Rg8Sint => &[(vf::R8G8_SINT, None, ID)],
        Rgbx8Unorm => &[(vf::R8G8B8X8_UNORM, None, ID), (vf::R8G8B8A8_UNORM, None, RGB1)],
        Rgbx8Snorm => &[(vf::R8G8B8X8_SNORM, None, ID), (vf::R8G8B8A8_SNORM, None, RGB1)],
        Rgbx8Srgb => &[(vf::R8G8B8X8_SRGB, None, ID), (vf::R8G8B8A8_SRGB, None, RGB1)],
        Rgbx8Uint => &[(vf::R8G8B8X8_UINT, None, ID), (vf::R8G8B8A8_UINT, None, RGB1)],
        Rgbx8Sint => &[(vf::R8G8B8X8_SINT, None, ID), (vf::R8G8B8A8_SINT, None, RGB1)],
        Rgba8Unorm => &[(vf::R8G8B8A8_UNORM, None, ID)],
        Rgba8Snorm => &[(vf::R8G8B8A8_SNORM, None, ID)],
        Rgba8Srgb => &[(vf::R8G8B8A8_SRGB, None, ID)],
        Rgba8Uint => &[(vf::R8G8B8A8_UINT, None, ID)],
        Rgba8Sint => &[(vf::R8G8B8A8_SINT, None, ID)],
        R16Float => &[(vf::R16_FLOAT, None, ID)],
        R16Uint => &[(vf::R16_UINT, None, ID)],
        R16Sint => &[(vf::R16_SINT, None, ID)],
        Rg16Float => &[(vf::R16G16_FLOAT, None, ID)],
        Rg16Uint => &[(vf::R16G16_UINT, None, ID)],
        Rg16Sint => &[(vf::R16G16_SINT, None, ID)],
        Rgbx16Float => &[(vf::R16G16B16X16_FLOAT, None, ID), (vf::R16G16B16A16_FLOAT, None, RGB1)],
        Rgbx16Uint => &[(vf::R16G16B16X16_UINT, None, ID), (vf::R16G16B16A16_UINT, None, RGB1)],
        Rgbx16Sint => &[(vf::R16G16B16X16_SINT, None, ID), (vf::R16G16B16A16_SINT, None, RGB1)],
        Rgba16Float => &[(vf::R16G16B16A16_FLOAT, None, ID)],
        Rgba16Uint => &[(vf::R16G16B16A16_UINT, None, ID)],
        Rgba16Sint => &[(vf::R16G16B16A16_SINT, None, ID)],
        R32Float => &[(vf::R32_FLOAT, None, ID)],
        R32Uint => &[(vf::R32_UINT, None, ID)],
        R32Sint => &[(vf::R32_SINT, None, ID)],
        Rg32Float => &[(vf::R32G32_FLOAT, None, ID)],
        Rg32Uint => &[(vf::R32G32_UINT, None, ID)],
        Rg32Sint => &[(vf::R32G32_SINT, None, ID)],
        Rgbx32Float => &[(vf::R32G32B32A32_FLOAT, None, RGB1)],
        Rgbx32Uint => &[(vf::R32G32B32A32_UINT, None, RGB1)],
        Rgbx32Sint => &[(vf::R32G32B32A32_SINT, None, RGB1)],
        Rgba32Float => &[(vf::R32G32B32A32_FLOAT, None, ID)],
        Rgba32Uint => &[(vf::R32G32B32A32_UINT, None, ID)],
        Rgba32Sint => &[(vf::R32G32B32A32_SINT, None, ID)],
        B5G6R5Unorm => &[(vf::B5G6R5_UNORM, None, ID), (vf::R8G8B8X8_UNORM, Some(Rgbx8Unorm), ID)],
        Rgba4Unorm => &[(vf::A4B4G4R4_UNORM, None, ID), (vf::R8G8B8A8_UNORM, Some(Rgba8Unorm), ID)],
        Rgb5A1Unorm => &[(vf::A1B5G5R5_UNORM, None, ID), (vf::R8G8B8A8_UNORM, Some(Rgba8Unorm), ID)],
        Rgb10A2Unorm => &[(vf::R10G10B10A2_UNORM, None, ID)],
        Rgb10A2Uint => &[(vf::R10G10B10A2_UINT, None, ID)],
        R11G11B10Float => &[(vf::R11G11B10_FLOAT, None, ID), (vf::R16G16B16A16_FLOAT, Some(Rgba16Float), RGB1)],
        Rgb9E5Float => &[(vf::R9G9B9E5_FLOAT, None, ID), (vf::R16G16B16A16_FLOAT, Some(Rgba16Float), RGB1)],
        D16Unorm => &[(vf::Z16_UNORM, None, ID)],
        // The host reads this as a 32-bit normalised depth, whose top 24
        // bits are the stored ones.
        D24Unorm => &[(vf::Z24X8_UNORM, None, ID)],
        D24UnormS8Uint => &[(vf::S8_UINT_Z24_UNORM, None, ID)],
        D32Float => &[(vf::Z32_FLOAT, None, ID)],
        D32FloatS8Uint => &[(vf::Z32_FLOAT_S8X24_UINT, None, ID)],
        // Stencil alone in a depth-stencil image: hosts bind 8-bit stencil
        // formats as stencil-only attachments, which some (ANGLE, Intel's
        // OpenGL) find incomplete. Draws ignore the depth it brings (see
        // `draw_with`).
        S8Uint => &[(vf::S8_UINT_Z24_UNORM, Some(D24UnormS8Uint), ID)],
        L8Unorm => &[(vf::R8_UNORM, None, [0, 0, 0, O])],
        A8Unorm => &[(vf::R8_UNORM, None, [Z, Z, Z, 0])],
        L8A8Unorm => &[(vf::R8G8_UNORM, None, [0, 0, 0, 1])],
        L16Float => &[(vf::R16_FLOAT, None, [0, 0, 0, O])],
        A16Float => &[(vf::R16_FLOAT, None, [Z, Z, Z, 0])],
        L16A16Float => &[(vf::R16G16_FLOAT, None, [0, 0, 0, 1])],
        L32Float => &[(vf::R32_FLOAT, None, [0, 0, 0, O])],
        A32Float => &[(vf::R32_FLOAT, None, [Z, Z, Z, 0])],
        L32A32Float => &[(vf::R32G32_FLOAT, None, [0, 0, 0, 1])],
    }
}

/// Whether OpenGL ES 3.0 (with `EXT_color_buffer_float`) renders to a
/// format.
pub fn gl_renderable(f: Format) -> bool {
    use Format::*;
    match f.class() {
        Class::Depth | Class::DepthStencil | Class::Stencil => true,
        _ => matches!(
            f,
            R8Unorm
                | Rg8Unorm
                | Rgbx8Unorm
                | Rgba8Unorm
                | Rgba8Srgb
                | B5G6R5Unorm
                | Rgba4Unorm
                | Rgb5A1Unorm
                | Rgb10A2Unorm
                | Rgb10A2Uint
                | R8Uint
                | R8Sint
                | Rg8Uint
                | Rg8Sint
                | Rgba8Uint
                | Rgba8Sint
                | R16Uint
                | R16Sint
                | Rg16Uint
                | Rg16Sint
                | Rgba16Uint
                | Rgba16Sint
                | R32Uint
                | R32Sint
                | Rg32Uint
                | Rg32Sint
                | Rgba32Uint
                | Rgba32Sint
                | R16Float
                | Rg16Float
                | Rgba16Float
                | R32Float
                | Rg32Float
                | Rgba32Float
                | R11G11B10Float
        ),
    }
}

/// The host format for a storage format, if the host has one.
pub fn host_format(caps: &HostCaps, f: Format) -> Option<HostFormat> {
    let depth = matches!(f.class(), Class::Depth | Class::DepthStencil | Class::Stencil);
    let render = gl_renderable(f);
    for &(virgl, layout, swizzle) in candidates(f) {
        // Depth formats are never in the render list (they are depth
        // targets, not color targets).
        let renderable = depth || caps.can_render(virgl);
        if caps.can_sample(virgl) && (!render || renderable) {
            return Some(HostFormat { virgl, layout, swizzle, renderable });
        }
    }
    None
}

/// Composes a view's swizzle with the format's: what the application's
/// swizzle selects, taken from the host's components.
pub fn compose(format: [u8; 4], view: [u8; 4]) -> [u8; 4] {
    view.map(|s| if s < 4 { format[s as usize] } else { s })
}

/// The virgl vertex format for an attribute.
pub fn vertex_format(a: &crate::backend::Attrib) -> u32 {
    use crate::backend::AttribType as T;
    let n = a.size.clamp(1, 4) as u32 - 1;
    let base = match (a.ty, a.integer, a.normalized) {
        (T::Float, _, _) => vf::R32_FLOAT,
        (T::HalfFloat, _, _) => vf::R16_FLOAT,
        (T::Fixed, _, _) => vf::R32_FIXED,
        (T::UnsignedByte, true, _) => vf::R8_UINT,
        (T::UnsignedByte, false, true) => vf::R8_UNORM,
        (T::UnsignedByte, false, false) => vf::R8_USCALED,
        (T::Byte, true, _) => vf::R8_SINT,
        (T::Byte, false, true) => vf::R8_SNORM,
        (T::Byte, false, false) => vf::R8_SSCALED,
        (T::UnsignedShort, true, _) => vf::R16_UINT,
        (T::UnsignedShort, false, true) => vf::R16_UNORM,
        (T::UnsignedShort, false, false) => vf::R16_USCALED,
        (T::Short, true, _) => vf::R16_SINT,
        (T::Short, false, true) => vf::R16_SNORM,
        (T::Short, false, false) => vf::R16_SSCALED,
        (T::UnsignedInt, true, _) => vf::R32_UINT,
        (T::UnsignedInt, false, true) => vf::R32_UNORM,
        (T::UnsignedInt, false, false) => vf::R32_USCALED,
        (T::Int, true, _) => vf::R32_SINT,
        (T::Int, false, true) => vf::R32_SNORM,
        (T::Int, false, false) => vf::R32_SSCALED,
        (T::Int2101010Rev, _, true) => return vf::R10G10B10A2_SNORM,
        (T::Int2101010Rev, _, false) => return vf::R10G10B10A2_SSCALED,
        (T::UnsignedInt2101010Rev, _, true) => return vf::R10G10B10A2_UNORM,
        (T::UnsignedInt2101010Rev, _, false) => return vf::R10G10B10A2_USCALED,
    };
    // Each family numbers its 1- to 4-component formats consecutively.
    base + n
}
