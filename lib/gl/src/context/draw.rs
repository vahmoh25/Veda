//! Drawing (OpenGL ES 3.0 sections 2.8.3 and 2.9): the checks a draw call
//! makes, and the complete state it hands the renderer.

use alloc::vec::Vec;

use vglsl::types::{Basic, Dim, Scalar};

use super::{Context, TEXTURE_UNITS, VERTEX_ATTRIBS};
use crate::backend::{
    Attrib, AttribType, Blend, BufferRange, DepthStencil, DrawInfo, DrawState, Framebuffer, IndexType, Mode, Raster,
    Rect, Stencil, TextureBinding, Viewport,
};
use crate::format::Class;
use crate::gl;

/// Space reused from draw to draw.
#[derive(Default)]
pub(crate) struct Scratch {
    attribs: Vec<Attrib>,
    textures: Vec<TextureBinding>,
    blocks: Vec<Option<BufferRange>>,
    feedback: Vec<BufferRange>,
}

pub(crate) fn mode(m: u32) -> Option<Mode> {
    Some(match m {
        gl::POINTS => Mode::Points,
        gl::LINES => Mode::Lines,
        gl::LINE_LOOP => Mode::LineLoop,
        gl::LINE_STRIP => Mode::LineStrip,
        gl::TRIANGLES => Mode::Triangles,
        gl::TRIANGLE_STRIP => Mode::TriangleStrip,
        gl::TRIANGLE_FAN => Mode::TriangleFan,
        _ => return None,
    })
}

fn stencil(s: &super::StencilFace) -> Stencil {
    Stencil {
        func: s.func,
        reference: s.reference,
        value_mask: s.value_mask,
        write_mask: s.write_mask,
        fail: s.fail,
        depth_fail: s.depth_fail,
        pass: s.pass,
    }
}

/// The texture target (index into the unit's bindings) a sampler type
/// samples.
fn sampler_target(dim: Dim) -> usize {
    match dim {
        Dim::D2 => 0,
        Dim::Cube => 1,
        Dim::D3 => 2,
        Dim::D2Array => 3,
    }
}

/// The vertices and primitives that whole primitives of `count` vertices
/// make (transform feedback records only whole ones).
pub(crate) fn whole_primitives(mode: u32, count: u64) -> (u64, u64) {
    match mode {
        gl::POINTS => (count, count),
        gl::LINES => (count / 2 * 2, count / 2),
        _ => (count / 3 * 3, count / 3),
    }
}

impl Context {
    /// `glDrawArrays`.
    pub fn draw_arrays(&mut self, mode: u32, first: i32, count: i32) {
        self.draw(mode, first, count, None, 1);
    }

    /// `glDrawArraysInstanced`.
    pub fn draw_arrays_instanced(&mut self, mode: u32, first: i32, count: i32, instances: i32) {
        self.draw(mode, first, count, None, instances);
    }

    /// `glDrawElements` (`offset` into the bound `ELEMENT_ARRAY_BUFFER`).
    pub fn draw_elements(&mut self, mode: u32, count: i32, ty: u32, offset: usize) {
        self.draw(mode, 0, count, Some((ty, offset)), 1);
    }

    /// `glDrawElementsInstanced`.
    pub fn draw_elements_instanced(&mut self, mode: u32, count: i32, ty: u32, offset: usize, instances: i32) {
        self.draw(mode, 0, count, Some((ty, offset)), instances);
    }

    /// `glDrawRangeElements` (the range is a hint).
    pub fn draw_range_elements(&mut self, mode: u32, start: u32, end: u32, count: i32, ty: u32, offset: usize) {
        if end < start {
            return self.err(gl::INVALID_VALUE);
        }
        self.draw(mode, 0, count, Some((ty, offset)), 1);
    }

    fn draw(&mut self, gl_mode: u32, first: i32, count: i32, elements: Option<(u32, usize)>, instances: i32) {
        let Some(m) = mode(gl_mode) else { return self.err(gl::INVALID_ENUM) };
        if first < 0 || count < 0 || instances < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let index_type = match elements.map(|(t, _)| t) {
            None => None,
            Some(gl::UNSIGNED_BYTE) => Some(IndexType::U8),
            Some(gl::UNSIGNED_SHORT) => Some(IndexType::U16),
            Some(gl::UNSIGNED_INT) => Some(IndexType::U32),
            Some(_) => return self.err(gl::INVALID_ENUM),
        };
        // Transform feedback restricts what may be drawn (section 2.15.2).
        let feedback = self.feedback_running();
        let mut feedback_vertices = 0;
        if feedback {
            let t = self.tf();
            if elements.is_some() || gl_mode != t.primitive_mode {
                return self.err(gl::INVALID_OPERATION);
            }
            let (vertices, _) = whole_primitives(gl_mode, count as u64);
            feedback_vertices = vertices * instances as u64;
            let written = t.vertices;
            if written + feedback_vertices > self.feedback_capacity() {
                return self.err(gl::INVALID_OPERATION);
            }
        }
        let fb = match self.draw_state_framebuffer() {
            Ok(fb) => fb,
            Err(_) => return self.err(gl::INVALID_FRAMEBUFFER_OPERATION),
        };
        // No program: nothing is drawn (OpenGL ES 3.0 leaves it undefined).
        let Some(exe) = self.programs.current_exe else { return };
        // The arrays the program reads come from buffers, which must not be
        // mapped (there are no client-side arrays).
        let used = self.used_attribs(exe);
        for (i, a) in self.vao().attribs.iter().enumerate() {
            if a.enabled && used[i] {
                match a.buffer {
                    None => return self.err(gl::INVALID_OPERATION),
                    Some(k) if self.buffers.get(k).map.is_some() => return self.err(gl::INVALID_OPERATION),
                    _ => {}
                }
            }
        }
        let element_buffer = match elements {
            None => None,
            Some(_) => match self.vao().element_buffer {
                None => return self.err(gl::INVALID_OPERATION),
                Some(k) if self.buffers.get(k).map.is_some() => return self.err(gl::INVALID_OPERATION),
                Some(k) => Some(k),
            },
        };
        if self.sampler_conflict(exe).is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        if count == 0 || instances == 0 {
            return;
        }
        let index = match (element_buffer, index_type) {
            (Some(k), Some(t)) => match self.buffers.get(k).resource {
                Some(r) => Some((r, t)),
                // An empty element buffer has no indices to draw.
                None => return,
            },
            _ => None,
        };
        self.fill_builtin_uniforms(exe);
        let mut scratch = core::mem::take(&mut self.scratch);
        self.resolve_attribs(&mut scratch.attribs);
        self.resolve_textures(exe, &fb, &mut scratch.textures);
        self.resolve_blocks(exe, &mut scratch.blocks);
        let has_feedback = feedback && self.resolve_feedback(exe, &mut scratch.feedback);
        let s = &self.state;
        let v = s.viewport;
        let x = self.programs.exe(exe);
        let state = DrawState {
            framebuffer: &fb,
            viewport: Viewport {
                x: v[0] as f32,
                y: v[1] as f32,
                w: v[2] as f32,
                h: v[3] as f32,
                near: s.depth_range[0],
                far: s.depth_range[1],
            },
            scissor: s.scissor_test.then(|| Rect {
                x: s.scissor[0],
                y: s.scissor[1],
                w: s.scissor[2],
                h: s.scissor[3],
            }),
            raster: Raster {
                cull: s.cull_face.then_some(s.cull_mode),
                front_ccw: s.front_ccw,
                polygon_offset: s.polygon_offset_fill.then_some(s.polygon_offset),
                discard: s.rasterizer_discard,
                line_width: s.line_width,
                alpha_to_coverage: s.sample_alpha_to_coverage,
                sample_coverage: s.sample_coverage.then_some((s.sample_coverage_value, s.sample_coverage_invert)),
            },
            depth_stencil: DepthStencil {
                depth_test: s.depth_test,
                depth_func: s.depth_func,
                depth_write: s.depth_write,
                stencil_test: s.stencil_test,
                front: stencil(&s.stencil_front),
                back: stencil(&s.stencil_back),
            },
            blend: Blend {
                enabled: s.blend,
                eq_rgb: s.blend_eq.0,
                eq_alpha: s.blend_eq.1,
                src_rgb: s.blend_src.0,
                dst_rgb: s.blend_dst.0,
                src_alpha: s.blend_src.1,
                dst_alpha: s.blend_dst.1,
                color: s.blend_color,
                write_mask: s.color_mask,
                dither: s.dither,
            },
            program: x.backend,
            uniforms: &x.uniforms,
            blocks: &scratch.blocks,
            textures: &scratch.textures,
            attribs: &scratch.attribs,
            index,
            primitive_restart: s.primitive_restart,
            feedback: has_feedback.then_some(&scratch.feedback[..]),
        };
        let info = DrawInfo {
            mode: m,
            first: match elements {
                Some((_, offset)) => offset,
                None => first as usize,
            },
            count: count as u32,
            indexed: index.is_some(),
            instances: instances as u32,
        };
        self.backend.draw(&state, &info);
        self.scratch = scratch;
        if feedback {
            let (_, primitives) = whole_primitives(gl_mode, count as u64);
            self.tf_mut().vertices += feedback_vertices;
            self.primitives_written += primitives * instances as u64;
        }
    }

    /// The attribute locations a program reads.
    fn used_attribs(&self, exe: u32) -> [bool; VERTEX_ATTRIBS] {
        let mut used = [false; VERTEX_ATTRIBS];
        for a in &self.programs.exe(exe).program.linked.attributes {
            let cols = match a.ty.as_basic() {
                Some(Basic::Matrix(c, _)) => c as usize,
                _ => 1,
            };
            let first = a.location as usize;
            for u in used.iter_mut().take((first + cols).min(VERTEX_ATTRIBS)).skip(first) {
                *u = true;
            }
        }
        used
    }

    /// Writes the values of built-in uniforms (`gl_DepthRange`).
    fn fill_builtin_uniforms(&mut self, exe: u32) {
        let [near, far] = self.state.depth_range;
        let e = self.programs.exes.get_mut(&exe).unwrap();
        let program = e.program.clone();
        for u in program.linked.uniforms.iter().filter(|u| u.builtin) {
            let v = match u.name.rsplit('.').next() {
                Some("near") => near,
                Some("far") => far,
                _ => far - near,
            };
            if let Some(slot) = e.uniforms.get_mut(u.slot as usize) {
                slot[0] = v.to_bits();
            }
        }
    }

    fn resolve_attribs(&self, out: &mut Vec<Attrib>) {
        out.clear();
        for (i, a) in self.vao().attribs.iter().enumerate() {
            let buffer = if a.enabled { a.buffer.and_then(|k| self.buffers.get(k).resource) } else { None };
            // An enabled array of an empty buffer reads as zero.
            let current = match (a.enabled, self.current_attribs[i]) {
                (true, _) => [0; 4],
                (false, c) => c.bits(),
            };
            let integer_current = matches!(self.current_attribs[i], super::Current::Int(_) | super::Current::Uint(_));
            out.push(Attrib {
                buffer,
                offset: a.offset,
                stride: a.effective_stride(),
                size: a.size,
                ty: super::vertex::attrib_type(a.ty).unwrap_or(AttribType::Float),
                normalized: a.normalized,
                integer: if a.enabled { a.integer } else { integer_current },
                divisor: a.divisor,
                current,
            });
        }
    }

    /// Each sampler's texture and sampler state. Incomplete textures,
    /// textures whose type does not suit the sampler, and textures the draw
    /// also renders to sample as (0, 0, 0, 1).
    fn resolve_textures(&mut self, exe: u32, fb: &Framebuffer, out: &mut Vec<TextureBinding>) {
        out.clear();
        let samplers = self.programs.exe(exe).program.linked.samplers.clone();
        for s in samplers {
            let unit = self
                .programs
                .exe(exe)
                .uniforms
                .get(s.slot as usize)
                .map_or(0, |v| v[0] as usize)
                .min(TEXTURE_UNITS - 1);
            let key = self.texture_units[unit][sampler_target(s.sampler.dim)];
            let params = match self.sampler_units[unit] {
                Some(k) => self.samplers.get(k).params,
                None => self.textures.get(key).sampler,
            };
            let state = super::textures::sampler_state(&params);
            let mut view = self.texture_view(key, &params);
            if let Some(v) = view {
                let tex = self.textures.get(key);
                let (base, _) = Self::level_range(tex);
                let format = tex.image(0, base).map(|i| i.internal.format);
                let class = format.map(|f| f.class());
                let suits = match (s.sampler.shadow, s.sampler.ty, class) {
                    (true, _, Some(Class::Depth | Class::DepthStencil)) => state.compare.is_some(),
                    (true, ..) => false,
                    (false, Scalar::Int, Some(Class::Sint)) => true,
                    (false, Scalar::Uint, Some(Class::Uint)) => true,
                    (false, Scalar::Float, Some(c)) => {
                        !matches!(c, Class::Sint | Class::Uint) && state.compare.is_none()
                    }
                    _ => false,
                };
                // Sampling an image the draw writes is a feedback loop.
                let renders_to = fb
                    .colors
                    .iter()
                    .flatten()
                    .chain(fb.depth.iter())
                    .any(|surf| surf.resource == v.resource && (v.base_level..=v.max_level).contains(&surf.level));
                if !suits || renders_to {
                    view = None;
                }
            }
            out.push(TextureBinding { view, sampler: state });
        }
    }

    /// Each uniform block's buffer range.
    fn resolve_blocks(&self, exe: u32, out: &mut Vec<Option<BufferRange>>) {
        out.clear();
        let x = self.programs.exe(exe);
        for &binding in &x.block_bindings {
            let b = self.bound.uniform_indexed[binding as usize];
            let range = b.buffer.and_then(|k| {
                let buf = self.buffers.get(k);
                let r = buf.resource?;
                let avail = buf.size.checked_sub(b.offset)?;
                let size = if b.size == 0 { avail } else { b.size.min(avail) };
                Some(BufferRange { buffer: r, offset: b.offset, size })
            });
            out.push(range);
        }
    }

    /// Bytes per vertex each transform feedback buffer receives.
    pub(crate) fn feedback_strides(&self, exe: u32) -> Vec<usize> {
        let l = &self.programs.exe(exe).program.linked;
        let bytes = |f: &vglsl::link::Feedback| f.components as usize * 4;
        if l.feedback_separate {
            l.feedback.iter().map(bytes).collect()
        } else {
            alloc::vec![l.feedback.iter().map(bytes).sum()]
        }
    }

    /// The bytes available to each feedback buffer binding: from its
    /// start to the end of its range or buffer.
    fn feedback_available(&self, i: usize) -> Option<(crate::backend::ResourceId, usize, usize)> {
        let b = self.tf().bindings[i];
        let buf = self.buffers.get(b.buffer?);
        let avail = buf.size.checked_sub(b.offset)?;
        let size = if b.size == 0 { avail } else { b.size.min(avail) };
        Some((buf.resource?, b.offset, size))
    }

    /// How many vertices the bound feedback buffers have room for in all.
    pub(crate) fn feedback_capacity(&self) -> u64 {
        let Some(exe) = self.programs.current_exe else { return 0 };
        let strides = self.feedback_strides(exe);
        let mut cap = u64::MAX;
        for (i, &stride) in strides.iter().enumerate() {
            let avail = self.feedback_available(i).map_or(0, |(_, _, s)| s);
            if let Some(n) = avail.checked_div(stride) {
                cap = cap.min(n as u64);
            }
        }
        if cap == u64::MAX { 0 } else { cap }
    }

    /// The ranges the next vertices are recorded into (after the ones
    /// recorded so far).
    fn resolve_feedback(&self, exe: u32, out: &mut Vec<BufferRange>) -> bool {
        out.clear();
        let written = self.tf().vertices as usize;
        for (i, stride) in self.feedback_strides(exe).into_iter().enumerate() {
            let Some((buffer, offset, size)) = self.feedback_available(i) else { return false };
            let done = written * stride;
            out.push(BufferRange { buffer, offset: offset + done, size: size.saturating_sub(done) });
        }
        true
    }
}
