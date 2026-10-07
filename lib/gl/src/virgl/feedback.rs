//! Transform feedback where the host records only zeros.
//!
//! Some hosts' OpenGL drivers capture nothing but zeros from a vertex
//! shader that writes `gl_ClipDistance` (Intel's on Windows, desktop and
//! OpenGL ES alike), and virglrenderer's vertex shaders all do there, for
//! user clip planes. The first draw that captures checks the host with a
//! point whose values are known. Where they do not come back, the vertices
//! a draw captures are shaded in guest memory instead, by the software
//! renderer's vertex stage, from the buffers' contents as this renderer
//! keeps them (read back where the GPU wrote them); the host still draws
//! what the application sees.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use vglsl::interp::{Env, Exec, Lanes};

use super::VirglBackend;
use super::ops::{NO_BLEND, NO_TESTS, PLAIN_RASTER};
use crate::backend::*;
use crate::format::Format;
use crate::pixels::try_zeroed;
use crate::soft::program::SoftProgram;
use crate::soft::texture::{DrawTextures, Sampled, TexLevel};
use crate::soft::vertex::{self, Fetch};

const CHECK_VS: &str = "#version 300 es\nout vec4 v;\nvoid main() { v = vec4(1.0, 2.0, 3.0, 4.0); gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
const CHECK_FS: &str = "#version 300 es\nprecision mediump float;\nout vec4 o;\nvoid main() { o = vec4(1.0); }\n";

/// A texture's levels read back for the vertex shader: how they are
/// sampled, and the texels, which `sampled` points into (moving a `Vec`
/// leaves its buffer where it is).
struct ReadBack {
    sampled: Sampled,
    _texels: Vec<Vec<u8>>,
}

impl VirglBackend {
    /// Whether the host records transform feedback, found out at the first
    /// draw that captures.
    pub(super) fn feedback_works(&mut self) -> bool {
        if let Some(ok) = self.feedback_on_host {
            return ok;
        }
        // The check itself captures on the host.
        self.feedback_on_host = Some(true);
        let ok = self.unqueried(|b| b.check_feedback()).unwrap_or(false);
        self.feedback_on_host = Some(ok);
        ok
    }

    /// Captures one point's (1, 2, 3, 4) on the host and reads it back.
    fn check_feedback(&mut self) -> Option<bool> {
        let options = vglsl::Options::default();
        let vs = vglsl::compile(vglsl::Stage::Vertex, &[CHECK_VS], &options).shader?;
        let fs = vglsl::compile(vglsl::Stage::Fragment, &[CHECK_FS], &options).shader?;
        let bindings =
            vglsl::link::Bindings { attributes: Vec::new(), feedback: vec!["v".into()], feedback_separate: false };
        let p = vglsl::program::link(&vs, &fs, &bindings, &options.limits).program?;
        let program = self.program(Arc::new(p));
        let one =
            |target, format, width| ResourceDesc { target, format, width, height: 1, depth: 1, levels: 1, samples: 0 };
        let buffer = self.create(&one(Target::Buffer, Format::R8Unorm, 16)).ok();
        let target = self.create(&one(Target::Renderbuffer, Format::Rgba8Unorm, 1)).ok();
        let mut ok = None;
        if let (Some(buffer), Some(target)) = (buffer, target) {
            let mut fb = Framebuffer { width: 1, height: 1, ..Framebuffer::default() };
            fb.colors[0] = Some(Surface { resource: target, level: 0, layer: 0 });
            fb.draw_buffers[0] = Some(0);
            let ranges = [BufferRange { buffer, offset: 0, size: 16 }];
            let state = DrawState {
                framebuffer: &fb,
                viewport: Viewport { x: 0.0, y: 0.0, w: 1.0, h: 1.0, near: 0.0, far: 1.0 },
                scissor: None,
                raster: Raster { discard: true, ..PLAIN_RASTER },
                depth_stencil: NO_TESTS,
                blend: NO_BLEND,
                program,
                uniforms: &[],
                blocks: &[],
                textures: &[],
                attribs: &[],
                index: None,
                primitive_restart: false,
                feedback: Some(&ranges),
            };
            self.draw_call(&state, &DrawInfo { mode: Mode::Points, first: 0, count: 1, indexed: false, instances: 1 });
            let mut got = [0u8; 16];
            self.read_buffer(buffer, 0, &mut got);
            let want = [1.0f32, 2.0, 3.0, 4.0].map(f32::to_bits);
            ok = Some(got.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).eq(want));
        }
        for r in [buffer, target].into_iter().flatten() {
            self.destroy(r);
        }
        self.drop_program(program);
        ok
    }

    /// Records what a draw's vertices capture by shading them in guest
    /// memory (`samplers`: the sampler indices its vertex shader uses).
    pub(super) fn feedback_in_guest(
        &mut self,
        source: &Arc<vglsl::program::Program>,
        samplers: &[u32],
        s: &DrawState<'_>,
        info: &DrawInfo,
        ranges: &[BufferRange],
    ) {
        let sp = self.guest_programs.entry(s.program).or_insert_with(|| Arc::new(SoftProgram::new(source.clone())));
        let sp = sp.clone();
        let ncap = sp.feedback.len();
        if ncap == 0 {
            return;
        }
        // What the vertices are made of, as it is now.
        let mut buffers: Vec<u32> = s.attribs.iter().filter_map(|a| a.buffer).collect();
        buffers.extend(s.blocks.iter().flatten().map(|r| r.buffer));
        buffers.extend(s.index.map(|(b, _)| b));
        for b in buffers {
            self.current_shadow(b);
        }
        let textures: Vec<Option<ReadBack>> = (0..s.textures.len() as u32)
            .map(|i| if samplers.contains(&i) { self.read_back_texture(&s.textures[i as usize]) } else { None })
            .collect();
        let Some(recorded) = self.shade_captured(&sp, s, info, &textures) else { return };
        // Each buffer's vertices, as far as its range holds them.
        let vertices = recorded.len() / ncap;
        for (bi, range) in ranges.iter().enumerate() {
            let stride = sp.feedback_strides.get(bi).copied().unwrap_or(0);
            if stride == 0 {
                continue;
            }
            let comps: Vec<usize> =
                sp.feedback.iter().enumerate().filter(|(_, c)| c.buffer as usize == bi).map(|(k, _)| k).collect();
            let n = vertices.min(range.size / stride);
            let Some(mut data) = try_zeroed(n * stride) else { continue };
            for v in 0..n {
                for (j, &k) in comps.iter().enumerate() {
                    data[v * stride + j * 4..][..4].copy_from_slice(&recorded[v * ncap + k].to_le_bytes());
                }
            }
            self.write_buffer(range.buffer, range.offset, &data);
        }
    }

    /// The captured components of a draw's whole primitives, in order.
    fn shade_captured(
        &self,
        sp: &SoftProgram,
        s: &DrawState<'_>,
        info: &DrawInfo,
        textures: &[Option<ReadBack>],
    ) -> Option<Vec<u32>> {
        let bytes = |id: u32| self.resources.get(&id).and_then(|r| r.shadow.as_ref()).map(|s| s.data.as_slice());
        let fetch: Vec<Fetch<'_>> = s
            .attribs
            .iter()
            .map(|a| Fetch {
                data: a.buffer.and_then(bytes),
                offset: a.offset,
                stride: a.stride as usize,
                size: a.size,
                ty: a.ty,
                normalized: a.normalized,
                integer: a.integer,
                divisor: a.divisor,
                current: a.current,
            })
            .collect();
        // Each block as the program declares it, zero past its buffer.
        let blocks: Vec<Vec<u8>> = sp
            .program
            .linked
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let mut v = vec![0u8; b.size as usize];
                if let Some(Some(r)) = s.blocks.get(i)
                    && let Some(d) = bytes(r.buffer)
                {
                    let start = r.offset.min(d.len());
                    let end = start.saturating_add(r.size.min(b.size as usize)).min(d.len());
                    v[..end - start].copy_from_slice(&d[start..end]);
                }
                v
            })
            .collect();
        let block_refs: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
        let sampled: Vec<Option<Sampled>> = textures.iter().map(|t| t.as_ref().map(|t| t.sampled.clone())).collect();
        let tex = DrawTextures { bound: &sampled };
        let env = Env { uniforms: s.uniforms, blocks: &block_refs, textures: &tex };
        let mut exec = Exec::new(&sp.vs);
        if exec.regs.len() < sp.registers {
            exec.regs.resize(sp.registers, Lanes::ZERO);
        }
        exec.prologue(&sp.vs, &env);
        // The vertex stream: indices, or consecutive vertices.
        let ids: Vec<u32> = match s.index {
            Some((buffer, ty)) => {
                let restart = ty.restart();
                crate::soft::read_indices(bytes(buffer).unwrap_or(&[]), info.first, info.count, ty)
                    .into_iter()
                    .filter(|&i| !(s.primitive_restart && i == restart))
                    .collect()
            }
            None => (0..info.count).map(|k| (info.first as u32).wrapping_add(k)).collect(),
        };
        let per = match info.mode {
            Mode::Points => 1,
            Mode::Lines => 2,
            _ => 3,
        };
        let whole = ids.len() / per * per;
        let ncap = sp.feedback.len();
        let words = vertex::vertex_words(sp);
        let mut out = try_u32s(ids.len() * words)?;
        let mut captured = try_u32s(ids.len() * ncap)?;
        let mut recorded = Vec::new();
        recorded.try_reserve_exact(whole * ncap * info.instances.max(1) as usize).ok()?;
        for instance in 0..info.instances.max(1) {
            vertex::shade(sp, &fetch, &env, &mut exec, &ids, instance, &mut out, Some(&mut captured));
            recorded.extend_from_slice(&captured[..whole * ncap]);
        }
        Some(recorded)
    }

    /// A texture's levels, read back as a sampler of the vertex shader sees
    /// them.
    fn read_back_texture(&mut self, b: &TextureBinding) -> Option<ReadBack> {
        let v = b.view?;
        let d = self.resources.get(&v.resource)?.desc;
        let format = d.format;
        let tb = format.bytes();
        let (mut texels, mut levels) = (Vec::new(), Vec::new());
        for l in v.base_level..=v.max_level.min(d.levels.saturating_sub(1)) {
            let (w, h) = ((d.width >> l).max(1), (d.height >> l).max(1));
            let depth = match d.target {
                Target::Texture3D => (d.depth >> l).max(1),
                Target::TextureCube => 6,
                Target::Texture2DArray => d.depth,
                _ => 1,
            };
            let (row, image) = (w as usize * tb, w as usize * h as usize * tb);
            let mut data = try_zeroed(image * depth as usize)?;
            self.read_texture(v.resource, l, Region::new(0, 0, 0, w, h, depth), &mut data, row, image);
            levels.push(TexLevel { ptr: data.as_ptr(), width: w, height: h, depth, row, image });
            texels.push(data);
        }
        if levels.is_empty() {
            return None;
        }
        let dim = match v.target {
            Target::Texture3D => vglsl::types::Dim::D3,
            Target::TextureCube => vglsl::types::Dim::Cube,
            Target::Texture2DArray => vglsl::types::Dim::D2Array,
            _ => vglsl::types::Dim::D2,
        };
        let sampled = Sampled {
            levels,
            dim,
            format,
            texel: tb,
            kind: Sampled::kind_of(format),
            swizzle: v.swizzle,
            state: b.sampler,
        };
        Some(ReadBack { sampled, _texels: texels })
    }
}

/// `n` zeroed words, if there is memory for them.
fn try_u32s(n: usize) -> Option<Vec<u32>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.resize(n, 0);
    Some(v)
}
