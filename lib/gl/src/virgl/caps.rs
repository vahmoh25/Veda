//! The host's capabilities: virglrenderer's capability set 2
//! (`struct virgl_caps_v2`), as the device reports it.

/// What the host renderer can do.
#[derive(Clone, Debug)]
pub struct HostCaps {
    pub max_version: u32,
    /// Formats that can be sampled, rendered to and read back (bit `f` of
    /// word `f / 32` for virgl format `f`).
    pub sampler: [u32; 16],
    pub render: [u32; 16],
    pub readback: [u32; 16],
    pub glsl_level: u32,
    pub max_texture_array_layers: u32,
    pub max_render_targets: u32,
    pub max_samples: u32,
    pub max_uniform_blocks: u32,
    pub point_size: (f32, f32),
    pub line_width: (f32, f32),
    pub max_vertex_attribs: u32,
    pub uniform_buffer_offset_alignment: u32,
    pub capability_bits: u32,
    pub capability_bits_v2: u32,
    pub max_texture_2d_size: u32,
    pub max_texture_3d_size: u32,
    pub max_texture_cube_size: u32,
    pub max_anisotropy: f32,
    /// The host's `GL_RENDERER`.
    pub renderer: [u8; 64],
}

/// Offsets into `struct virgl_caps_v2` (all fields are 32 bits).
mod off {
    pub const MAX_VERSION: usize = 0;
    pub const SAMPLER: usize = 4;
    pub const RENDER: usize = 68;
    pub const GLSL_LEVEL: usize = 264;
    pub const MAX_TEXTURE_ARRAY_LAYERS: usize = 268;
    pub const MAX_RENDER_TARGETS: usize = 280;
    pub const MAX_SAMPLES: usize = 284;
    pub const MAX_UNIFORM_BLOCKS: usize = 296;
    pub const MIN_ALIASED_POINT_SIZE: usize = 308;
    pub const MAX_ALIASED_POINT_SIZE: usize = 312;
    pub const MIN_ALIASED_LINE_WIDTH: usize = 324;
    pub const MAX_ALIASED_LINE_WIDTH: usize = 328;
    pub const MAX_VERTEX_ATTRIBS: usize = 356;
    pub const UNIFORM_BUFFER_OFFSET_ALIGNMENT: usize = 384;
    pub const CAPABILITY_BITS: usize = 392;
    pub const MAX_TEXTURE_2D_SIZE: usize = 484;
    pub const MAX_TEXTURE_3D_SIZE: usize = 488;
    pub const MAX_TEXTURE_CUBE_SIZE: usize = 492;
    pub const READBACK: usize = 560;
    pub const CAPABILITY_BITS_V2: usize = 688;
    pub const RENDERER: usize = 696;
    pub const MAX_ANISOTROPY: usize = 760;
    /// The fields up to and including `max_anisotropy`.
    pub const END: usize = 764;
}

impl HostCaps {
    /// Parses capability set 2. Returns `None` if it is too short or not
    /// version 2.
    pub fn parse(b: &[u8]) -> Option<HostCaps> {
        if b.len() < off::END {
            return None;
        }
        let u = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let f = |o: usize| f32::from_bits(u(o));
        let mask = |o: usize| core::array::from_fn(|i| u(o + 4 * i));
        let max_version = u(off::MAX_VERSION);
        if max_version < 2 {
            return None;
        }
        let mut renderer = [0u8; 64];
        renderer.copy_from_slice(&b[off::RENDERER..off::RENDERER + 64]);
        Some(HostCaps {
            max_version,
            sampler: mask(off::SAMPLER),
            render: mask(off::RENDER),
            readback: mask(off::READBACK),
            glsl_level: u(off::GLSL_LEVEL),
            max_texture_array_layers: u(off::MAX_TEXTURE_ARRAY_LAYERS),
            max_render_targets: u(off::MAX_RENDER_TARGETS),
            max_samples: u(off::MAX_SAMPLES),
            max_uniform_blocks: u(off::MAX_UNIFORM_BLOCKS),
            point_size: (f(off::MIN_ALIASED_POINT_SIZE), f(off::MAX_ALIASED_POINT_SIZE)),
            line_width: (f(off::MIN_ALIASED_LINE_WIDTH), f(off::MAX_ALIASED_LINE_WIDTH)),
            max_vertex_attribs: u(off::MAX_VERTEX_ATTRIBS),
            uniform_buffer_offset_alignment: u(off::UNIFORM_BUFFER_OFFSET_ALIGNMENT),
            capability_bits: u(off::CAPABILITY_BITS),
            capability_bits_v2: u(off::CAPABILITY_BITS_V2),
            max_texture_2d_size: u(off::MAX_TEXTURE_2D_SIZE),
            max_texture_3d_size: u(off::MAX_TEXTURE_3D_SIZE),
            max_texture_cube_size: u(off::MAX_TEXTURE_CUBE_SIZE),
            max_anisotropy: f(off::MAX_ANISOTROPY),
            renderer,
        })
    }

    pub fn can_sample(&self, format: u32) -> bool {
        bit(&self.sampler, format)
    }

    pub fn can_render(&self, format: u32) -> bool {
        bit(&self.render, format)
    }

    pub fn can_read_back(&self, format: u32) -> bool {
        bit(&self.readback, format)
    }

    pub fn has(&self, bit: u32) -> bool {
        self.capability_bits & bit != 0
    }

    pub fn has2(&self, bit: u32) -> bool {
        self.capability_bits_v2 & bit != 0
    }

    /// The host renderer's name.
    pub fn renderer_name(&self) -> &str {
        let n = self.renderer.iter().position(|&c| c == 0).unwrap_or(self.renderer.len());
        core::str::from_utf8(&self.renderer[..n]).unwrap_or("")
    }
}

fn bit(mask: &[u32; 16], f: u32) -> bool {
    (f as usize) < 512 && mask[f as usize / 32] & (1 << (f % 32)) != 0
}
