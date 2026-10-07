//! State queries (OpenGL ES 3.0 section 6.1): `Get*`, strings and the
//! implementation's limits.

use super::buffers::UNIFORM_BUFFER_OFFSET_ALIGNMENT;
use super::{Context, DRAW_BUFFERS, FEEDBACK_BINDINGS, TEXTURE_UNITS, UNIFORM_BUFFER_BINDINGS, VERTEX_ATTRIBS};
use crate::gl;

/// A state value as the specification types it.
enum Value {
    Bools(usize, [bool; 4]),
    /// Integers and enumerants.
    Ints(usize, [i64; 16]),
    /// Floats; `normalized` ones convert to integers as color components
    /// do (table 4.5).
    Floats(usize, [f32; 4], bool),
}

fn ints(v: &[i64]) -> Value {
    let mut a = [0; 16];
    let n = v.len().min(16);
    a[..n].copy_from_slice(&v[..n]);
    Value::Ints(n, a)
}

fn int(v: i64) -> Value {
    ints(&[v])
}

fn enm(v: u32) -> Value {
    int(i64::from(v))
}

fn boolean(b: bool) -> Value {
    Value::Bools(1, [b, false, false, false])
}

fn floats(v: &[f32], normalized: bool) -> Value {
    let mut a = [0.0; 4];
    let n = v.len().min(4);
    a[..n].copy_from_slice(&v[..n]);
    Value::Floats(n, a, normalized)
}

/// A normalized float as an integer (table 4.5's signed conversion).
fn norm_to_int(f: f32) -> i64 {
    let v = (f64::from(f.clamp(-1.0, 1.0)) * 4_294_967_295.0 - 1.0) / 2.0;
    vmath::f64::round(v) as i64
}

/// The extensions this implementation offers (`GL_EXTENSIONS`).
fn extensions(ctx: &Context) -> alloc::vec::Vec<&'static str> {
    let caps = ctx.backend.caps();
    let mut v = alloc::vec![
        "GL_OES_element_index_uint",
        "GL_OES_standard_derivatives",
        "GL_OES_texture_3D",
        "GL_EXT_shader_texture_lod",
        "GL_EXT_frag_depth",
        "GL_EXT_draw_buffers",
        "GL_EXT_shadow_samplers",
        "GL_OES_texture_half_float",
        "GL_OES_texture_float",
        "GL_OES_depth_texture",
        "GL_OES_packed_depth_stencil",
        "GL_OES_rgb8_rgba8",
        "GL_OES_vertex_array_object",
    ];
    if caps.color_buffer_float {
        v.push("GL_EXT_color_buffer_float");
        v.push("GL_EXT_color_buffer_half_float");
    }
    if caps.float_linear {
        v.push("GL_OES_texture_float_linear");
        v.push("GL_OES_texture_half_float_linear");
    }
    if caps.max_anisotropy > 1.0 {
        v.push("GL_EXT_texture_filter_anisotropic");
    }
    v
}

impl Context {
    /// The value of a state variable, or `None` (and `INVALID_ENUM`).
    fn state_value(&mut self, pname: u32) -> Option<Value> {
        let s = &self.state;
        let caps = self.backend.caps().clone();
        let limits = self.shading.limits;
        let name_of =
            |k: Option<super::Key>, store: &super::Store<super::Buffer>| i64::from(k.map_or(0, |k| store.name(k)));
        Some(match pname {
            // Vertex arrays and buffers.
            gl::VERTEX_ARRAY_BINDING => int(i64::from(self.vertex_array.map_or(0, |k| self.vertex_arrays.name(k)))),
            gl::ARRAY_BUFFER_BINDING => int(name_of(self.bound.array, &self.buffers)),
            gl::ELEMENT_ARRAY_BUFFER_BINDING => int(name_of(self.vao().element_buffer, &self.buffers)),
            gl::COPY_READ_BUFFER_BINDING => int(name_of(self.bound.copy_read, &self.buffers)),
            gl::COPY_WRITE_BUFFER_BINDING => int(name_of(self.bound.copy_write, &self.buffers)),
            gl::PIXEL_PACK_BUFFER_BINDING => int(name_of(self.bound.pixel_pack, &self.buffers)),
            gl::PIXEL_UNPACK_BUFFER_BINDING => int(name_of(self.bound.pixel_unpack, &self.buffers)),
            gl::UNIFORM_BUFFER_BINDING => int(name_of(self.bound.uniform, &self.buffers)),
            gl::TRANSFORM_FEEDBACK_BUFFER_BINDING => int(name_of(self.tf().generic, &self.buffers)),
            gl::TRANSFORM_FEEDBACK_BINDING => int(i64::from(self.feedback.map_or(0, |k| self.feedbacks.name(k)))),
            gl::TRANSFORM_FEEDBACK_ACTIVE => boolean(self.tf().active),
            gl::TRANSFORM_FEEDBACK_PAUSED => boolean(self.tf().paused),
            // Transformation and rasterization.
            gl::VIEWPORT => ints(&s.viewport.map(i64::from)),
            gl::DEPTH_RANGE => floats(&s.depth_range, true),
            gl::LINE_WIDTH => floats(&[s.line_width], false),
            gl::CULL_FACE => boolean(s.cull_face),
            gl::CULL_FACE_MODE => enm(match s.cull_mode {
                crate::backend::Cull::Front => gl::FRONT,
                crate::backend::Cull::Back => gl::BACK,
                crate::backend::Cull::Both => gl::FRONT_AND_BACK,
            }),
            gl::FRONT_FACE => enm(if s.front_ccw { gl::CCW } else { gl::CW }),
            gl::POLYGON_OFFSET_FACTOR => floats(&[s.polygon_offset.0], false),
            gl::POLYGON_OFFSET_UNITS => floats(&[s.polygon_offset.1], false),
            gl::POLYGON_OFFSET_FILL => boolean(s.polygon_offset_fill),
            gl::RASTERIZER_DISCARD => boolean(s.rasterizer_discard),
            gl::SAMPLE_ALPHA_TO_COVERAGE => boolean(s.sample_alpha_to_coverage),
            gl::SAMPLE_COVERAGE => boolean(s.sample_coverage),
            gl::SAMPLE_COVERAGE_VALUE => floats(&[s.sample_coverage_value], true),
            gl::SAMPLE_COVERAGE_INVERT => boolean(s.sample_coverage_invert),
            gl::PRIMITIVE_RESTART_FIXED_INDEX => boolean(s.primitive_restart),
            // Textures.
            gl::ACTIVE_TEXTURE => enm(gl::TEXTURE0 + self.active_texture as u32),
            gl::TEXTURE_BINDING_2D => int(i64::from(self.unit_binding(gl::TEXTURE_2D))),
            gl::TEXTURE_BINDING_CUBE_MAP => int(i64::from(self.unit_binding(gl::TEXTURE_CUBE_MAP))),
            gl::TEXTURE_BINDING_3D => int(i64::from(self.unit_binding(gl::TEXTURE_3D))),
            gl::TEXTURE_BINDING_2D_ARRAY => int(i64::from(self.unit_binding(gl::TEXTURE_2D_ARRAY))),
            gl::SAMPLER_BINDING => {
                int(i64::from(self.sampler_units[self.active_texture].map_or(0, |k| self.samplers.name(k))))
            }
            // Per-fragment operations.
            gl::SCISSOR_TEST => boolean(s.scissor_test),
            gl::SCISSOR_BOX => ints(&s.scissor.map(i64::from)),
            gl::STENCIL_TEST => boolean(s.stencil_test),
            gl::STENCIL_FUNC => enm(super::func_gl(s.stencil_front.func)),
            gl::STENCIL_VALUE_MASK => int(i64::from(s.stencil_front.value_mask as i32)),
            gl::STENCIL_REF => int(i64::from(s.stencil_front.reference)),
            gl::STENCIL_FAIL => enm(super::stencil_op_gl(s.stencil_front.fail)),
            gl::STENCIL_PASS_DEPTH_FAIL => enm(super::stencil_op_gl(s.stencil_front.depth_fail)),
            gl::STENCIL_PASS_DEPTH_PASS => enm(super::stencil_op_gl(s.stencil_front.pass)),
            gl::STENCIL_BACK_FUNC => enm(super::func_gl(s.stencil_back.func)),
            gl::STENCIL_BACK_VALUE_MASK => int(i64::from(s.stencil_back.value_mask as i32)),
            gl::STENCIL_BACK_REF => int(i64::from(s.stencil_back.reference)),
            gl::STENCIL_BACK_FAIL => enm(super::stencil_op_gl(s.stencil_back.fail)),
            gl::STENCIL_BACK_PASS_DEPTH_FAIL => enm(super::stencil_op_gl(s.stencil_back.depth_fail)),
            gl::STENCIL_BACK_PASS_DEPTH_PASS => enm(super::stencil_op_gl(s.stencil_back.pass)),
            gl::DEPTH_TEST => boolean(s.depth_test),
            gl::DEPTH_FUNC => enm(super::func_gl(s.depth_func)),
            gl::BLEND => boolean(s.blend),
            gl::BLEND_SRC_RGB => enm(super::blend_factor_gl(s.blend_src.0)),
            gl::BLEND_SRC_ALPHA => enm(super::blend_factor_gl(s.blend_src.1)),
            gl::BLEND_DST_RGB => enm(super::blend_factor_gl(s.blend_dst.0)),
            gl::BLEND_DST_ALPHA => enm(super::blend_factor_gl(s.blend_dst.1)),
            gl::BLEND_EQUATION_RGB => enm(super::blend_eq_gl(s.blend_eq.0)),
            gl::BLEND_EQUATION_ALPHA => enm(super::blend_eq_gl(s.blend_eq.1)),
            gl::BLEND_COLOR => floats(&s.blend_color, true),
            gl::DITHER => boolean(s.dither),
            // Framebuffer control.
            gl::COLOR_WRITEMASK => Value::Bools(4, s.color_mask),
            gl::DEPTH_WRITEMASK => boolean(s.depth_write),
            gl::STENCIL_WRITEMASK => int(i64::from(s.stencil_front.write_mask as i32)),
            gl::STENCIL_BACK_WRITEMASK => int(i64::from(s.stencil_back.write_mask as i32)),
            gl::COLOR_CLEAR_VALUE => floats(&s.clear_color, true),
            gl::DEPTH_CLEAR_VALUE => floats(&[s.clear_depth], true),
            gl::STENCIL_CLEAR_VALUE => int(i64::from(s.clear_stencil)),
            gl::DRAW_FRAMEBUFFER_BINDING => {
                int(i64::from(self.draw_framebuffer.map_or(0, |k| self.framebuffers.name(k))))
            }
            gl::READ_FRAMEBUFFER_BINDING => {
                int(i64::from(self.read_framebuffer.map_or(0, |k| self.framebuffers.name(k))))
            }
            gl::RENDERBUFFER_BINDING => int(i64::from(self.renderbuffer.map_or(0, |k| self.renderbuffers.name(k)))),
            gl::READ_BUFFER => enm(match self.read_framebuffer {
                None => self.default_fb.read_buffer,
                Some(k) => self.framebuffers.get(k).read_buffer,
            }),
            p if (gl::DRAW_BUFFER0..gl::DRAW_BUFFER0 + DRAW_BUFFERS as u32).contains(&p) => {
                let i = (p - gl::DRAW_BUFFER0) as usize;
                enm(match self.draw_framebuffer {
                    None if i == 0 => self.default_fb.draw_buffer,
                    None => gl::NONE,
                    Some(k) => self.framebuffers.get(k).draw_buffers[i],
                })
            }
            // Pixel storage.
            gl::UNPACK_ALIGNMENT => int(i64::from(s.unpack.alignment)),
            gl::UNPACK_ROW_LENGTH => int(i64::from(s.unpack.row_length)),
            gl::UNPACK_IMAGE_HEIGHT => int(i64::from(s.unpack.image_height)),
            gl::UNPACK_SKIP_PIXELS => int(i64::from(s.unpack.skip_pixels)),
            gl::UNPACK_SKIP_ROWS => int(i64::from(s.unpack.skip_rows)),
            gl::UNPACK_SKIP_IMAGES => int(i64::from(s.unpack.skip_images)),
            gl::PACK_ALIGNMENT => int(i64::from(s.pack.alignment)),
            gl::PACK_ROW_LENGTH => int(i64::from(s.pack.row_length)),
            gl::PACK_SKIP_PIXELS => int(i64::from(s.pack.skip_pixels)),
            gl::PACK_SKIP_ROWS => int(i64::from(s.pack.skip_rows)),
            // Programs and hints.
            gl::CURRENT_PROGRAM => int(i64::from(self.current_program())),
            gl::GENERATE_MIPMAP_HINT => enm(s.generate_mipmap_hint),
            gl::FRAGMENT_SHADER_DERIVATIVE_HINT => enm(s.derivative_hint),
            // Implementation-dependent values.
            gl::MAX_ELEMENT_INDEX => int(i64::from(u32::MAX)),
            gl::SUBPIXEL_BITS => int(8),
            gl::MAX_3D_TEXTURE_SIZE => int(i64::from(caps.max_3d_texture_size)),
            gl::MAX_TEXTURE_SIZE => int(i64::from(caps.max_texture_size)),
            gl::MAX_ARRAY_TEXTURE_LAYERS => int(i64::from(caps.max_array_layers)),
            gl::MAX_TEXTURE_LOD_BIAS => floats(&[16.0], false),
            gl::MAX_CUBE_MAP_TEXTURE_SIZE => int(i64::from(caps.max_cube_map_size)),
            gl::MAX_RENDERBUFFER_SIZE => int(i64::from(caps.max_renderbuffer_size)),
            gl::MAX_DRAW_BUFFERS | gl::MAX_COLOR_ATTACHMENTS => int(DRAW_BUFFERS as i64),
            gl::MAX_VIEWPORT_DIMS => ints(&[i64::from(caps.max_viewport), i64::from(caps.max_viewport)]),
            gl::ALIASED_POINT_SIZE_RANGE => floats(&[caps.point_size.0, caps.point_size.1], false),
            gl::ALIASED_LINE_WIDTH_RANGE => floats(&[caps.line_width.0, caps.line_width.1], false),
            gl::MAX_ELEMENTS_INDICES | gl::MAX_ELEMENTS_VERTICES => int(1 << 20),
            gl::NUM_COMPRESSED_TEXTURE_FORMATS => int(crate::etc::FORMATS.len() as i64),
            gl::COMPRESSED_TEXTURE_FORMATS => {
                let v: alloc::vec::Vec<i64> = crate::etc::FORMATS.iter().map(|f| i64::from(f.gl)).collect();
                ints(&v)
            }
            gl::NUM_PROGRAM_BINARY_FORMATS | gl::NUM_SHADER_BINARY_FORMATS => int(0),
            gl::PROGRAM_BINARY_FORMATS | gl::SHADER_BINARY_FORMATS => Value::Ints(0, [0; 16]),
            gl::SHADER_COMPILER => boolean(true),
            gl::MAX_SERVER_WAIT_TIMEOUT => int(0),
            gl::NUM_EXTENSIONS => int(extensions(self).len() as i64),
            gl::MAJOR_VERSION => int(3),
            gl::MINOR_VERSION => int(0),
            gl::MAX_VERTEX_ATTRIBS => int(VERTEX_ATTRIBS as i64),
            gl::MAX_VERTEX_UNIFORM_COMPONENTS => int(i64::from(limits.max_vertex_uniform_vectors) * 4),
            gl::MAX_VERTEX_UNIFORM_VECTORS => int(i64::from(limits.max_vertex_uniform_vectors)),
            gl::MAX_VERTEX_UNIFORM_BLOCKS | gl::MAX_FRAGMENT_UNIFORM_BLOCKS => {
                int(i64::from(limits.max_uniform_blocks))
            }
            gl::MAX_COMBINED_UNIFORM_BLOCKS => int(i64::from(limits.max_uniform_blocks) * 2),
            gl::MAX_VERTEX_OUTPUT_COMPONENTS => int(i64::from(limits.max_vertex_output_vectors) * 4),
            gl::MAX_VERTEX_TEXTURE_IMAGE_UNITS => int(i64::from(limits.max_vertex_texture_image_units)),
            gl::MAX_FRAGMENT_UNIFORM_COMPONENTS => int(i64::from(limits.max_fragment_uniform_vectors) * 4),
            gl::MAX_FRAGMENT_UNIFORM_VECTORS => int(i64::from(limits.max_fragment_uniform_vectors)),
            gl::MAX_FRAGMENT_INPUT_COMPONENTS => int(i64::from(limits.max_fragment_input_vectors) * 4),
            gl::MAX_TEXTURE_IMAGE_UNITS => int(i64::from(limits.max_texture_image_units)),
            gl::MIN_PROGRAM_TEXEL_OFFSET => int(i64::from(limits.min_program_texel_offset)),
            gl::MAX_PROGRAM_TEXEL_OFFSET => int(i64::from(limits.max_program_texel_offset)),
            gl::MAX_UNIFORM_BUFFER_BINDINGS => int(UNIFORM_BUFFER_BINDINGS as i64),
            gl::MAX_UNIFORM_BLOCK_SIZE => int(i64::from(limits.max_uniform_block_size)),
            gl::UNIFORM_BUFFER_OFFSET_ALIGNMENT => int(UNIFORM_BUFFER_OFFSET_ALIGNMENT as i64),
            gl::MAX_COMBINED_VERTEX_UNIFORM_COMPONENTS => {
                int(i64::from(limits.max_uniform_blocks) * i64::from(limits.max_uniform_block_size) / 4
                    + i64::from(limits.max_vertex_uniform_vectors) * 4)
            }
            gl::MAX_COMBINED_FRAGMENT_UNIFORM_COMPONENTS => {
                int(i64::from(limits.max_uniform_blocks) * i64::from(limits.max_uniform_block_size) / 4
                    + i64::from(limits.max_fragment_uniform_vectors) * 4)
            }
            gl::MAX_VARYING_COMPONENTS => int(i64::from(limits.max_varying_vectors) * 4),
            gl::MAX_VARYING_VECTORS => int(i64::from(limits.max_varying_vectors)),
            gl::MAX_COMBINED_TEXTURE_IMAGE_UNITS => int(TEXTURE_UNITS as i64),
            gl::MAX_TRANSFORM_FEEDBACK_INTERLEAVED_COMPONENTS => {
                int(i64::from(limits.max_transform_feedback_interleaved_components))
            }
            gl::MAX_TRANSFORM_FEEDBACK_SEPARATE_ATTRIBS => int(FEEDBACK_BINDINGS as i64),
            gl::MAX_TRANSFORM_FEEDBACK_SEPARATE_COMPONENTS => {
                int(i64::from(limits.max_transform_feedback_separate_components))
            }
            gl::MAX_SAMPLES => int(i64::from(caps.max_samples)),
            gl::MAX_TEXTURE_MAX_ANISOTROPY_EXT => floats(&[caps.max_anisotropy], false),
            // Framebuffer-dependent values.
            gl::SAMPLE_BUFFERS | gl::SAMPLES => {
                let samples = self.draw_state_framebuffer().map_or(0, |f| f.samples);
                int(if pname == gl::SAMPLES { i64::from(samples) } else { i64::from(samples > 0) })
            }
            gl::RED_BITS | gl::GREEN_BITS | gl::BLUE_BITS | gl::ALPHA_BITS | gl::DEPTH_BITS | gl::STENCIL_BITS => {
                let (c, d, st) = self.draw_formats();
                let v = match pname {
                    gl::RED_BITS => c.map_or(0, |i| i.bits[0]),
                    gl::GREEN_BITS => c.map_or(0, |i| i.bits[1]),
                    gl::BLUE_BITS => c.map_or(0, |i| i.bits[2]),
                    gl::ALPHA_BITS => c.map_or(0, |i| i.bits[3]),
                    gl::DEPTH_BITS => d.map_or(0, |i| i.bits[4]),
                    _ => st.map_or(0, |i| i.bits[5]),
                };
                int(i64::from(v))
            }
            gl::IMPLEMENTATION_COLOR_READ_FORMAT | gl::IMPLEMENTATION_COLOR_READ_TYPE => {
                let pair = self.read_color().ok().and_then(|_| self.color_read_pair());
                let Some((f, t)) = pair else {
                    self.err(gl::INVALID_OPERATION);
                    return None;
                };
                enm(if pname == gl::IMPLEMENTATION_COLOR_READ_FORMAT { f } else { t })
            }
            _ => {
                if self.capability_state(pname).is_some() {
                    return self.capability_state(pname).map(boolean);
                }
                self.err(gl::INVALID_ENUM);
                return None;
            }
        })
    }

    /// An `Enable`/`Disable` capability's state.
    fn capability_state(&self, cap: u32) -> Option<bool> {
        let s = &self.state;
        Some(match cap {
            gl::BLEND => s.blend,
            gl::CULL_FACE => s.cull_face,
            gl::DEPTH_TEST => s.depth_test,
            gl::DITHER => s.dither,
            gl::POLYGON_OFFSET_FILL => s.polygon_offset_fill,
            gl::PRIMITIVE_RESTART_FIXED_INDEX => s.primitive_restart,
            gl::RASTERIZER_DISCARD => s.rasterizer_discard,
            gl::SAMPLE_ALPHA_TO_COVERAGE => s.sample_alpha_to_coverage,
            gl::SAMPLE_COVERAGE => s.sample_coverage,
            gl::SCISSOR_TEST => s.scissor_test,
            gl::STENCIL_TEST => s.stencil_test,
            _ => return None,
        })
    }

    /// `glGetBooleanv`.
    pub fn get_booleanv(&mut self, pname: u32, out: &mut [bool]) {
        let Some(v) = self.state_value(pname) else { return };
        let mut tmp = [false; 16];
        let n = match v {
            Value::Bools(n, b) => {
                tmp[..4].copy_from_slice(&b);
                n
            }
            Value::Ints(n, i) => {
                for k in 0..n {
                    tmp[k] = i[k] != 0;
                }
                n
            }
            Value::Floats(n, f, _) => {
                for k in 0..n {
                    tmp[k] = f[k] != 0.0;
                }
                n
            }
        };
        super::vertex::fill(out, &tmp[..n]);
    }

    /// A state value as integers.
    fn integers(&mut self, pname: u32) -> Option<(usize, [i64; 16])> {
        let v = self.state_value(pname)?;
        let mut tmp = [0i64; 16];
        let n = match v {
            Value::Bools(n, b) => {
                for k in 0..n {
                    tmp[k] = i64::from(b[k]);
                }
                n
            }
            Value::Ints(n, i) => {
                tmp[..n].copy_from_slice(&i[..n]);
                n
            }
            Value::Floats(n, f, normalized) => {
                for k in 0..n {
                    tmp[k] = if normalized { norm_to_int(f[k]) } else { vmath::f32::round(f[k]) as i64 };
                }
                n
            }
        };
        Some((n, tmp))
    }

    /// `glGetInteger64v`.
    pub fn get_integer64v(&mut self, pname: u32, out: &mut [i64]) {
        if let Some((n, v)) = self.integers(pname) {
            super::vertex::fill(out, &v[..n]);
        }
    }

    /// `glGetIntegerv`.
    pub fn get_integerv(&mut self, pname: u32, out: &mut [i32]) {
        if let Some((n, v)) = self.integers(pname) {
            let v = v.map(|x| x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
            super::vertex::fill(out, &v[..n]);
        }
    }

    /// `glGetFloatv`.
    pub fn get_floatv(&mut self, pname: u32, out: &mut [f32]) {
        let Some(v) = self.state_value(pname) else { return };
        let mut tmp = [0.0f32; 16];
        let n = match v {
            Value::Bools(n, b) => {
                for k in 0..n {
                    tmp[k] = f32::from(u8::from(b[k]));
                }
                n
            }
            Value::Ints(n, i) => {
                for k in 0..n {
                    tmp[k] = i[k] as f32;
                }
                n
            }
            Value::Floats(n, f, _) => {
                tmp[..4].copy_from_slice(&f);
                n
            }
        };
        super::vertex::fill(out, &tmp[..n]);
    }

    /// One integer state value (`glGetIntegerv` for single values).
    pub fn get_integer(&mut self, pname: u32) -> i32 {
        let mut v = [0];
        self.get_integerv(pname, &mut v);
        v[0]
    }

    /// One float state value.
    pub fn get_float(&mut self, pname: u32) -> f32 {
        let mut v = [0.0];
        self.get_floatv(pname, &mut v);
        v[0]
    }

    /// One boolean state value.
    pub fn get_boolean(&mut self, pname: u32) -> bool {
        let mut v = [false];
        self.get_booleanv(pname, &mut v);
        v[0]
    }

    /// `glGetInteger64i_v` (one value).
    pub fn get_integer64i(&mut self, target: u32, index: u32) -> i64 {
        let i = index as usize;
        let indexed = |c: &Context, uniform: bool| -> Option<(Option<super::Key>, usize, usize)> {
            if uniform {
                let b = c.bound.uniform_indexed.get(i)?;
                Some((b.buffer, b.offset, b.size))
            } else {
                let b = c.tf().bindings.get(i)?;
                Some((b.buffer, b.offset, b.size))
            }
        };
        let (uniform, what) = match target {
            gl::UNIFORM_BUFFER_BINDING => (true, 0),
            gl::UNIFORM_BUFFER_START => (true, 1),
            gl::UNIFORM_BUFFER_SIZE => (true, 2),
            gl::TRANSFORM_FEEDBACK_BUFFER_BINDING => (false, 0),
            gl::TRANSFORM_FEEDBACK_BUFFER_START => (false, 1),
            gl::TRANSFORM_FEEDBACK_BUFFER_SIZE => (false, 2),
            _ => {
                self.err(gl::INVALID_ENUM);
                return 0;
            }
        };
        let Some((buffer, offset, size)) = indexed(self, uniform) else {
            self.err(gl::INVALID_VALUE);
            return 0;
        };
        match what {
            0 => i64::from(buffer.map_or(0, |k| self.buffers.name(k))),
            1 => offset as i64,
            _ => size as i64,
        }
    }

    /// `glGetIntegeri_v` (one value).
    pub fn get_integeri(&mut self, target: u32, index: u32) -> i32 {
        self.get_integer64i(target, index).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    /// `glGetString`.
    pub fn get_string(&mut self, name: u32) -> Option<alloc::string::String> {
        use alloc::string::String;
        Some(match name {
            gl::VENDOR => String::from("Veda"),
            gl::RENDERER => String::from(self.backend.caps().name),
            gl::VERSION => String::from("OpenGL ES 3.0 Veda"),
            gl::SHADING_LANGUAGE_VERSION => String::from("OpenGL ES GLSL ES 3.00 Veda"),
            gl::EXTENSIONS => extensions(self).join(" "),
            _ => {
                self.err(gl::INVALID_ENUM);
                return None;
            }
        })
    }

    /// `glGetStringi` (`EXTENSIONS`).
    pub fn get_stringi(&mut self, name: u32, index: u32) -> Option<&'static str> {
        if name != gl::EXTENSIONS {
            self.err(gl::INVALID_ENUM);
            return None;
        }
        let e = extensions(self);
        match e.get(index as usize) {
            Some(&s) => Some(s),
            None => {
                self.err(gl::INVALID_VALUE);
                None
            }
        }
    }
}
