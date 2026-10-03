//! Materials: how a surface is shaded and blended.

use vmath::Vec3;

use crate::texture::TextureId;

/// Lighting model (evaluated per vertex).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shading {
    /// The base colour (and texture) as is, plus emission.
    Unlit,
    /// Diffuse lighting: sun, hemisphere ambient and point lights.
    Lambert,
    /// Lambert plus a Blinn-Phong highlight. `shininess` is rounded to a
    /// power of two (2..=256).
    Phong {
        /// Specular exponent.
        shininess: f32,
        /// Highlight colour, 0..1 per channel (times the sun colour).
        specular: Vec3,
    },
}

/// How the shaded colour is combined with the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// Replaces the frame (depth tested and written).
    Opaque,
    /// Source-over with the material/texture/vertex alpha (sorted back to
    /// front, no depth writes).
    Alpha,
    /// Adds light (glows, lasers, sparks); order independent.
    Additive,
    /// Multiplies the frame (white = no change), e.g. soft shadows.
    Multiply,
}

/// Which faces are discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cull {
    /// Faces whose vertices run clockwise on screen (seen from behind).
    Back,
    /// Faces whose vertices run counter-clockwise on screen.
    Front,
    /// Draw both sides.
    None,
}

/// A surface description.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Straight-alpha `0xAARRGGBB` base colour (multiplies texture and
    /// vertex colours).
    pub color: u32,
    /// Texture (registered with [`crate::Renderer::add_texture`]).
    pub texture: Option<TextureId>,
    /// Lighting model.
    pub shading: Shading,
    /// Emitted light, 0..1 per channel (added after lighting).
    pub emissive: Vec3,
    /// How the result is combined with the frame.
    pub blend: Blend,
    /// Discard texels whose alpha is below one half (foliage cut-outs).
    pub alpha_test: bool,
    /// Which faces are discarded.
    pub cull: Cull,
    /// Hidden behind nearer surfaces.
    pub depth_test: bool,
    /// Hides farther surfaces drawn later.
    pub depth_write: bool,
    /// Affected by distance fog.
    pub fog: bool,
    /// Multiply by the mesh's vertex colours.
    pub vertex_colors: bool,
    /// Light both sides of thin surfaces (|n.l|).
    pub two_sided_lighting: bool,
    /// Mipmap bias in quarter levels (negative = sharper).
    pub lod_bias: i32,
    /// Affine (not perspective-correct) texturing: cheaper, exact for
    /// screen-aligned quads such as billboards.
    pub affine: bool,
    /// Bilinear filtering where the texture is magnified (smoother close
    /// up, costs about twice as much per pixel there).
    pub bilinear: bool,
    /// Added to the object's distance when ordering draws: opaque objects
    /// are drawn front to back (so hidden pixels are rejected by the depth
    /// test early), and a positive offset makes large background meshes
    /// (terrain) go after the nearer objects that cover them.
    pub sort_offset: f32,
}

impl Default for Material {
    fn default() -> Material {
        Material::lambert(0xFFFF_FFFF)
    }
}

impl Material {
    /// Diffuse-lit, vertex-coloured, opaque.
    pub const fn lambert(color: u32) -> Material {
        Material {
            color,
            texture: None,
            shading: Shading::Lambert,
            emissive: Vec3::ZERO,
            blend: Blend::Opaque,
            alpha_test: false,
            cull: Cull::Back,
            depth_test: true,
            depth_write: true,
            fog: true,
            vertex_colors: true,
            two_sided_lighting: false,
            lod_bias: 0,
            affine: false,
            bilinear: false,
            sort_offset: 0.0,
        }
    }

    /// Unlit (the colour as is).
    pub const fn unlit(color: u32) -> Material {
        let mut m = Material::lambert(color);
        m.shading = Shading::Unlit;
        m
    }

    /// Diffuse plus specular highlights.
    pub const fn phong(color: u32, shininess: f32, specular: Vec3) -> Material {
        let mut m = Material::lambert(color);
        m.shading = Shading::Phong { shininess, specular };
        m
    }

    /// Unlit additive glow (no depth writes, fades in fog).
    pub const fn glow(color: u32) -> Material {
        let mut m = Material::unlit(color);
        m.blend = Blend::Additive;
        m.depth_write = false;
        m.cull = Cull::None;
        m
    }

    /// Uses texture `t` (multiplied by the base colour).
    pub const fn with_texture(mut self, t: TextureId) -> Material {
        self.texture = Some(t);
        self
    }

    /// Sets the blend mode; blended materials stop writing depth.
    pub const fn with_blend(mut self, b: Blend) -> Material {
        self.blend = b;
        if !matches!(b, Blend::Opaque) {
            self.depth_write = false;
        }
        self
    }

    /// Sets the emitted light (0..1 per channel).
    pub const fn with_emissive(mut self, e: Vec3) -> Material {
        self.emissive = e;
        self
    }

    /// Sets which faces are discarded.
    pub const fn with_cull(mut self, c: Cull) -> Material {
        self.cull = c;
        self
    }

    /// Draws and lights both sides (leaves, flags, thin panels).
    pub const fn double_sided(mut self) -> Material {
        self.cull = Cull::None;
        self.two_sided_lighting = true;
        self
    }

    /// Ignores distance fog (sky decorations, HUD-like objects).
    pub const fn without_fog(mut self) -> Material {
        self.fog = false;
        self
    }

    /// Discards texels with alpha below one half.
    pub const fn with_alpha_test(mut self) -> Material {
        self.alpha_test = true;
        self
    }

    /// Shifts mip selection by quarter levels (positive = blurrier and
    /// cheaper, negative = sharper).
    pub const fn with_lod_bias(mut self, quarter_levels: i32) -> Material {
        self.lod_bias = quarter_levels;
        self
    }

    /// Neither tests nor writes depth (drawn over everything before it).
    pub const fn without_depth(mut self) -> Material {
        self.depth_test = false;
        self.depth_write = false;
        self
    }

    /// Moves the draw in the depth sort by `d` world units (positive =
    /// treated as farther) without changing the image.
    pub const fn with_sort_offset(mut self, d: f32) -> Material {
        self.sort_offset = d;
        self
    }

    /// Filters the texture bilinearly where it is magnified.
    pub const fn with_bilinear(mut self) -> Material {
        self.bilinear = true;
        self
    }

    /// Interpolates texture coordinates linearly in screen space: cheaper,
    /// and fine for surfaces facing the camera or covering little depth.
    pub const fn affine(mut self) -> Material {
        self.affine = true;
        self
    }

    /// True if the material is drawn in the sorted transparent pass.
    pub fn is_blended(&self) -> bool {
        !matches!(self.blend, Blend::Opaque)
    }
}
