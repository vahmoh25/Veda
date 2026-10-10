//! Clears, blits, mipmaps, presentation, and the resources and shaders
//! they need; and how 3D textures are read, copied and drawn into on hosts
//! that cannot bind their slices to a framebuffer.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use vglsl::tgsi;
use vglsl::types::{Dim, Sampler, Scalar};

use super::draw::{blend_words, dsa_words};
use super::protocol::*;
use super::{Res, VirglBackend};
use crate::backend::*;
use crate::format::{Class, Format, Geometry, downsample};
use crate::pixels::try_zeroed;

/// What the internal operations keep between calls.
#[derive(Default)]
pub(super) struct Internal {
    /// The buffer current attribute values are read from.
    current: Option<u32>,
    /// The vertex buffer of [`COVER`].
    cover: Option<u32>,
    /// The clear program (vertex and fragment shader).
    clear: Option<(u32, u32)>,
    /// The programs that copy a 3D texture's texels into a 2D image, by
    /// the kind of values they move (float, signed, unsigned).
    fetch: [Option<Fetch>; 3],
    /// Textures standing in for missing ones, by sampler kind: the
    /// resource, its view and the sampler state.
    missing: BTreeMap<(u8, bool, u8), (u32, u32, u32)>,
    /// The image presentation reads back, and its size.
    present: Option<(u32, u32, u32)>,
    /// A single-sampled copy of a multisampled color buffer.
    resolve: Option<(u32, u32, u32, Format)>,
}

/// A program copying texels of a 3D texture's slice, unconverted, to the
/// pixels of a 2D image: the program, its uniform slots and the slot of
/// `at` (where the copied box starts: x, y, slice).
#[derive(Clone, Copy)]
struct Fetch {
    program: ProgramId,
    slots: u32,
    at: u32,
}

/// A triangle covering the whole viewport (x, y, z, w per vertex).
const COVER: [f32; 12] = [-1.0, -1.0, 0.0, 1.0, 3.0, -1.0, 0.0, 1.0, -1.0, 3.0, 0.0, 1.0];

const CLEAR_VS: &str = "VERT\nDCL IN[0]\nDCL OUT[0], POSITION\n  MOV OUT[0], IN[0]\nEND\n";

const FETCH_VS: &str = "#version 300 es\nin vec4 pos;\nvoid main() { gl_Position = pos; }\n";

/// The fetch program's fragment shader for samplers of a kind (`""`,
/// `"i"` or `"u"`): pixel `(x, y)` gets texel `at + (x, y, 0)`.
fn fetch_fs(kind: &str) -> String {
    format!(
        "#version 300 es\nprecision highp float;\nprecision highp int;\n\
         uniform highp {kind}sampler3D t;\nuniform ivec4 at;\nout highp {kind}vec4 o;\n\
         void main() {{ o = texelFetch(t, at.xyz + ivec3(gl_FragCoord.xy, 0), 0); }}\n"
    )
}

/// The state an internal draw over a whole image has: no culling, tests,
/// blending or masks.
pub(super) const PLAIN_RASTER: Raster = Raster {
    cull: None,
    front_ccw: true,
    polygon_offset: None,
    discard: false,
    line_width: 1.0,
    alpha_to_coverage: false,
    sample_coverage: None,
};
const ALWAYS: Stencil = Stencil {
    func: Func::Always,
    reference: 0,
    value_mask: 0xFF,
    write_mask: 0,
    fail: StencilOp::Keep,
    depth_fail: StencilOp::Keep,
    pass: StencilOp::Keep,
};
pub(super) const NO_TESTS: DepthStencil = DepthStencil {
    depth_test: false,
    depth_func: Func::Always,
    depth_write: false,
    stencil_test: false,
    front: ALWAYS,
    back: ALWAYS,
};
pub(super) const NO_BLEND: Blend = Blend {
    enabled: false,
    eq_rgb: BlendEq::Add,
    eq_alpha: BlendEq::Add,
    src_rgb: BlendFactor::One,
    dst_rgb: BlendFactor::Zero,
    src_alpha: BlendFactor::One,
    dst_alpha: BlendFactor::Zero,
    color: [0.0; 4],
    write_mask: [true; 4],
    dither: false,
};
const NEAREST: SamplerState = SamplerState {
    min: Filter::Nearest,
    mag: Filter::Nearest,
    mip: None,
    wrap: [Wrap::ClampToEdge; 3],
    min_lod: -1000.0,
    max_lod: 1000.0,
    compare: None,
    max_anisotropy: 1.0,
};

/// 2D textures standing in for slices of 3D textures in a framebuffer,
/// each with the slice it stands in for.
pub(super) type Proxies = Vec<(Surface, u32)>;

/// Writes `CONST[i]` to every color buffer `i` and `CONST[8].x` to depth.
fn clear_fs() -> alloc::string::String {
    use core::fmt::Write;
    let mut t = alloc::string::String::from("FRAG\n");
    for i in 0..8 {
        let _ = writeln!(t, "DCL OUT[{i}], COLOR[{i}]");
    }
    t.push_str("DCL OUT[8], POSITION\nDCL CONST[0..8]\n");
    for i in 0..8 {
        let _ = writeln!(t, "  MOV OUT[{i}], CONST[{i}]");
    }
    t.push_str("  MOV OUT[8].z, CONST[8].xxxx\nEND\n");
    t
}

impl VirglBackend {
    fn internal_shader(&mut self, stage: u32, text: &str) -> u32 {
        let h = self.new_handle();
        let s = tgsi::Shader {
            text: text.into(),
            tokens: text.len() as u32 + 16,
            position: None,
            point_size: None,
            varyings: Vec::new(),
            attributes: Vec::new(),
            samplers: Vec::new(),
            blocks: Vec::new(),
        };
        self.create_shader(h, stage, &s, &[0]);
        h
    }

    fn internal_buffer(&mut self, data: &[u8]) -> u32 {
        let desc = ResourceDesc {
            target: Target::Buffer,
            format: Format::R8Unorm,
            width: data.len() as u32,
            height: 1,
            depth: 1,
            levels: 1,
            samples: 0,
        };
        let Ok(b) = self.create(&desc) else { return 0 };
        self.write_buffer(b, 0, data);
        b
    }

    /// The buffer of current attribute values (16 attributes).
    pub(super) fn current_buffer(&mut self) -> u32 {
        if let Some(b) = self.internal.current {
            return b;
        }
        let b = self.internal_buffer(&[0u8; 16 * 16]);
        self.internal.current = Some(b);
        b
    }

    /// The vertex buffer of [`COVER`].
    fn cover_buffer(&mut self) -> u32 {
        if let Some(b) = self.internal.cover {
            return b;
        }
        let bytes: Vec<u8> = COVER.iter().flat_map(|v| v.to_le_bytes()).collect();
        let b = self.internal_buffer(&bytes);
        self.internal.cover = Some(b);
        b
    }

    /// A view and sampler state for a sampler whose texture is missing
    /// (or incomplete): it reads (0, 0, 0, 1), and a shadow lookup 0.
    pub(super) fn missing_texture(&mut self, s: Sampler) -> (u32, u32) {
        let kind = match s.ty {
            Scalar::Int => 1,
            Scalar::Uint => 2,
            _ => 0,
        };
        let dim = match s.dim {
            Dim::D2 => 0,
            Dim::D3 => 1,
            Dim::Cube => 2,
            Dim::D2Array => 3,
        };
        let key = (dim, s.shadow, kind);
        if let Some(&(_, v, st)) = self.internal.missing.get(&key) {
            return (v, st);
        }
        let (format, texel): (Format, &[u8]) = match (s.shadow, kind) {
            (true, _) => (Format::D16Unorm, &[0, 0]),
            (_, 1) => (Format::Rgba8Sint, &[0, 0, 0, 1]),
            (_, 2) => (Format::Rgba8Uint, &[0, 0, 0, 1]),
            _ => (Format::Rgba8Unorm, &[0, 0, 0, 255]),
        };
        let (target, layers) = match s.dim {
            Dim::D2 => (Target::Texture2D, 1),
            Dim::D3 => (Target::Texture3D, 1),
            Dim::Cube => (Target::TextureCube, 6),
            Dim::D2Array => (Target::Texture2DArray, 1),
        };
        let desc = ResourceDesc { target, format, width: 1, height: 1, depth: layers, levels: 1, samples: 0 };
        let Ok(r) = self.create(&desc) else { return (0, 0) };
        let data: Vec<u8> = (0..layers).flat_map(|_| texel.iter().copied()).collect();
        let tb = texel.len();
        self.write_texture(r, 0, Region::new(0, 0, 0, 1, 1, layers), &data, tb, tb);
        let view = self.view(&View { resource: r, target, base_level: 0, max_level: 0, swizzle: [0, 1, 2, 3] });
        let sampler = SamplerState {
            min: Filter::Nearest,
            mag: Filter::Nearest,
            mip: None,
            wrap: [Wrap::ClampToEdge; 3],
            min_lod: -1000.0,
            max_lod: 1000.0,
            // A comparison that always fails gives 0.
            compare: s.shadow.then_some(Func::Never),
            max_anisotropy: 1.0,
        };
        let st = self.sampler_state(&sampler);
        self.internal.missing.insert(key, (r, view, st));
        (view, st)
    }

    // ---- Clears -------------------------------------------------------------------

    pub(super) fn clear_buffers(&mut self, fb: &Framebuffer, c: &Clear) {
        match self.flatten(fb) {
            Some((flat, proxies)) => {
                self.clear_framebuffer(&flat, c);
                self.unflatten(proxies);
            }
            None => self.clear_framebuffer(fb, c),
        }
    }

    /// Clears a new render target to zero (color, depth and stencil), as
    /// the software renderer's start: the host's memory may still hold what
    /// another context drew.
    pub(super) fn zero_render_target(&mut self, id: u32, d: &ResourceDesc) {
        let s = Surface { resource: id, level: 0, layer: 0 };
        let mut fb = Framebuffer { width: d.width, height: d.height, samples: d.samples, ..Framebuffer::default() };
        let mut c = Clear { color_mask: [true; 4], stencil_mask: 0xFF, ..Clear::default() };
        if matches!(d.format.class(), Class::Depth | Class::DepthStencil | Class::Stencil) {
            if d.format.has_depth() {
                fb.depth = Some(s);
                c.depth = Some(0.0);
            }
            if d.format.has_stencil() {
                fb.stencil = Some(s);
                c.stencil = Some(0);
            }
        } else {
            fb.colors[0] = Some(s);
            fb.draw_buffers[0] = Some(0);
            c.colors[0] = Some([0; 4]);
        }
        self.clear_buffers(&fb, &c);
    }

    fn clear_framebuffer(&mut self, fb: &Framebuffer, c: &Clear) {
        let whole = c.scissor.is_none_or(|s| {
            s.x <= 0
                && s.y <= 0
                && s.x.saturating_add(s.w) >= fb.width as i32
                && s.y.saturating_add(s.h) >= fb.height as i32
        });
        let colors = c.colors.iter().any(Option::is_some);
        let masked = (colors && c.color_mask != [true; 4]) || (c.stencil.is_some() && c.stencil_mask & 0xFF != 0xFF);
        // virglrenderer clears integer buffers with a float clear when
        // the clear covers every bound buffer, which leaves them
        // undefined.
        let integer = (0..8).any(|i| {
            c.colors[i].is_some()
                && fb.colors[i]
                    .and_then(|s| self.resources.get(&s.resource))
                    .is_some_and(|r| r.desc.format.is_integer())
        });
        if !whole || masked || integer {
            return self.clear_with_quad(fb, c);
        }
        // The host's clear: color buffers with the same value together.
        let zs = fb.depth.or(fb.stencil);
        let mut first = true;
        let mut done = [false; 8];
        for i in 0..8 {
            let Some(value) = c.colors[i].filter(|_| !done[i]) else { continue };
            let mut bits = 0;
            let mut surfaces = [None; 8];
            for (j, d) in done.iter_mut().enumerate().skip(i) {
                if c.colors[j] == Some(value) {
                    *d = true;
                    surfaces[j] = fb.colors[j];
                    bits |= CLEAR_COLOR0 << j;
                }
            }
            let n = surfaces.iter().rposition(Option::is_some).map_or(0, |k| k + 1);
            self.bind_surfaces(&surfaces[..n], zs);
            if first {
                bits |= self.depth_stencil_bits(c);
            }
            self.emit_clear(bits, value, c);
            first = false;
        }
        if first {
            let bits = self.depth_stencil_bits(c);
            if bits != 0 {
                self.bind_surfaces(&[], zs);
                self.emit_clear(bits, [0; 4], c);
            }
        }
    }

    fn depth_stencil_bits(&self, c: &Clear) -> u32 {
        (if c.depth.is_some() { CLEAR_DEPTH } else { 0 }) | (if c.stencil.is_some() { CLEAR_STENCIL } else { 0 })
    }

    fn emit_clear(&mut self, bits: u32, color: [u32; 4], c: &Clear) {
        let d = f64::from(c.depth.unwrap_or(1.0).clamp(0.0, 1.0)).to_bits();
        let s = c.stencil.unwrap_or(0) as u32 & 0xFF;
        self.emit(CMD_CLEAR, 0, &[bits, color[0], color[1], color[2], color[3], d as u32, (d >> 32) as u32, s]);
    }

    /// A scissored or masked clear: a triangle over the framebuffer that
    /// writes the values, through the write masks and the scissor.
    fn clear_with_quad(&mut self, fb: &Framebuffer, c: &Clear) {
        let (vs, fs) = match self.internal.clear {
            Some(p) => p,
            None => {
                let vs = self.internal_shader(SHADER_VERTEX, CLEAR_VS);
                let fs = self.internal_shader(SHADER_FRAGMENT, &clear_fs());
                self.internal.clear = Some((vs, fs));
                (vs, fs)
            }
        };
        let vb = self.cover_buffer();
        let mut surfaces = [None; 8];
        for (i, s) in surfaces.iter_mut().enumerate() {
            if c.colors[i].is_some() {
                *s = fb.colors[i];
            }
        }
        let n = surfaces.iter().rposition(Option::is_some).map_or(0, |k| k + 1);
        let zs = if c.depth.is_some() || c.stencil.is_some() { fb.depth.or(fb.stencil) } else { None };
        self.bind_surfaces(&surfaces[..n], zs);
        let full = Viewport { x: 0.0, y: 0.0, w: fb.width as f32, h: fb.height as f32, near: 0.0, far: 1.0 };
        self.set_viewport(&full);
        if let Some(r) = c.scissor {
            self.set_scissor(r);
        }
        let raster = Raster {
            cull: None,
            front_ccw: true,
            polygon_offset: None,
            discard: false,
            line_width: 1.0,
            alpha_to_coverage: false,
            sample_coverage: None,
        };
        let rs = self.rasterizer_words(&raster, c.scissor.is_some(), false, fb.samples > 0);
        self.bind_rasterizer(rs);
        let stencil = Stencil {
            func: Func::Always,
            reference: c.stencil.unwrap_or(0),
            value_mask: 0xFF,
            write_mask: c.stencil_mask,
            fail: StencilOp::Replace,
            depth_fail: StencilOp::Replace,
            pass: StencilOp::Replace,
        };
        let ds = DepthStencil {
            depth_test: c.depth.is_some(),
            depth_func: Func::Always,
            depth_write: true,
            stencil_test: c.stencil.is_some(),
            front: stencil,
            back: stencil,
        };
        self.bind_dsa(dsa_words(&ds));
        if c.stencil.is_some() {
            let r = c.stencil.unwrap_or(0);
            self.set_stencil_ref(r, r);
        }
        let blend = Blend {
            enabled: false,
            eq_rgb: BlendEq::Add,
            eq_alpha: BlendEq::Add,
            src_rgb: BlendFactor::One,
            dst_rgb: BlendFactor::Zero,
            src_alpha: BlendFactor::One,
            dst_alpha: BlendFactor::Zero,
            color: [0.0; 4],
            write_mask: c.color_mask,
            dither: false,
        };
        self.bind_blend(blend_words(&blend, false));
        self.set_sample_mask(!0);
        self.bind_shaders(vs, fs);
        let mut w = vec![SHADER_FRAGMENT, 0];
        for i in 0..8 {
            w.extend_from_slice(&c.colors[i].unwrap_or([0; 4]));
        }
        w.extend_from_slice(&[c.depth.unwrap_or(1.0).clamp(0.0, 1.0).to_bits(), 0, 0, 0]);
        self.emit(CMD_SET_CONSTANT_BUFFER, 0, &w);
        self.state.bound.constants_invalidate(1);
        self.bind_elements(vec![0, 0, 0, format::R32G32B32A32_FLOAT]);
        let vbw = vec![16, 0, vb];
        self.emit(CMD_SET_VERTEX_BUFFERS, 0, &vbw);
        self.state.bound.vertex_buffers_set(vbw);
        self.emit(CMD_DRAW_VBO, 0, &[0, 3, PRIM_TRIANGLES, 0, 1, 0, 0, 0, 0, 0, !0, 0]);
    }

    // ---- Blits ----------------------------------------------------------------------

    /// Blits a rectangle (x, y, w, h; negative sizes mirror) of one surface
    /// to another.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn blit_surface(
        &mut self,
        src: Surface,
        from: [i32; 4],
        dst: Surface,
        to: [i32; 4],
        mask: u32,
        linear: bool,
        scissor: Option<Rect>,
    ) {
        let fmt = |b: &Self, id: u32| b.resources.get(&id).and_then(|r: &Res| r.host).map_or(0, |h| h.virgl);
        let (sf, df) = (fmt(self, src.resource), fmt(self, dst.resource));
        // The callers stand 2D images in for 3D slices the host cannot bind.
        if sf == 0 || df == 0 || self.is_flat_3d(src.resource) || self.is_flat_3d(dst.resource) {
            return;
        }
        // A blit without a scissor that moves texels unchanged becomes
        // glCopyImageSubData where the host has it, which some drivers
        // (Intel's on Windows) do not order after draws into the source: a
        // scissor around the destination, which clips nothing, keeps it a
        // blit.
        let scissor = scissor.or_else(|| {
            self.host.has(CAP_COPY_IMAGE).then(|| {
                let (x0, x1) = (to[0].min(to[0] + to[2]), to[0].max(to[0] + to[2]));
                let (y0, y1) = (to[1].min(to[1] + to[3]), to[1].max(to[1] + to[3]));
                Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
            })
        });
        let c = |v: i32| v.clamp(0, 0xFFFF) as u32;
        let (s0, smin, smax) = match scissor {
            Some(r) => {
                (1 << 10, c(r.x) | (c(r.y) << 16), c(r.x.saturating_add(r.w)) | (c(r.y.saturating_add(r.h)) << 16))
            }
            None => (0, 0, 0),
        };
        let filter = if linear { FILTER_LINEAR } else { FILTER_NEAREST };
        let w = [
            mask | (filter << 8) | s0,
            smin,
            smax,
            dst.resource,
            dst.level,
            df,
            to[0] as u32,
            to[1] as u32,
            dst.layer,
            to[2] as u32,
            to[3] as u32,
            1,
            src.resource,
            src.level,
            sf,
            from[0] as u32,
            from[1] as u32,
            src.layer,
            from[2] as u32,
            from[3] as u32,
            1,
        ];
        self.emit(CMD_BLIT, 0, &w);
    }

    pub(super) fn blit_framebuffers(&mut self, b: &Blit) {
        let mut b = *b;
        // From a copy of a 3D slice the host cannot bind, and into
        // stand-ins for such slices.
        let mut copy = None;
        if b.color
            && let Some(i) = b.src_color
            && let Some(s) = b.src.colors[i as usize].filter(|s| self.is_flat_3d(s.resource))
        {
            copy = self.slice_copy(s);
            b.src.colors[i as usize] = copy.map(|c| Surface { resource: c, level: 0, layer: 0 });
        }
        let proxies = self.flatten(&b.dst).map(|(fb, p)| {
            b.dst = fb;
            p
        });
        let r = |x: [i32; 4]| [x[0], x[1], x[2] - x[0], x[3] - x[1]];
        let (from, to) = (r(b.src_rect), r(b.dst_rect));
        let linear = b.filter == Filter::Linear;
        if b.color
            && let Some(src) = b.src_color.and_then(|i| b.src.colors[i as usize])
        {
            for d in b.dst.draw_buffers.iter().flatten() {
                if let Some(dst) = b.dst.colors[*d as usize] {
                    self.blit_surface(src, from, dst, to, MASK_RGBA, linear, b.scissor);
                }
            }
        }
        let mut mask = 0;
        if b.depth {
            mask |= MASK_Z;
        }
        if b.stencil {
            mask |= MASK_S;
        }
        if mask != 0
            && let (Some(src), Some(dst)) = (b.src.depth.or(b.src.stencil), b.dst.depth.or(b.dst.stencil))
        {
            self.blit_surface(src, from, dst, to, mask, false, b.scissor);
        }
        if let Some(p) = proxies {
            self.unflatten(p);
        }
        if let Some(c) = copy {
            self.destroy(c);
        }
    }

    /// Copies a framebuffer rectangle into a texture image
    /// (`CopyTexSubImage`), converting formats.
    pub(super) fn copy_framebuffer(&mut self, src: &Framebuffer, read: u8, rect: Rect, dst: Surface, x: u32, y: u32) {
        let Some(mut s) = src.colors.get(read as usize).copied().flatten() else { return };
        let mut copy = None;
        if self.is_flat_3d(s.resource) {
            let Some(c) = self.slice_copy(s) else { return };
            copy = Some(c);
            s = Surface { resource: c, level: 0, layer: 0 };
        }
        let from = [rect.x, rect.y, rect.w, rect.h];
        let to = [x as i32, y as i32, rect.w, rect.h];
        if self.is_flat_3d(dst.resource) {
            if let Some(p) = self.slice_copy(dst) {
                self.blit_surface(s, from, Surface { resource: p, level: 0, layer: 0 }, to, MASK_RGBA, false, None);
                self.unflatten(vec![(dst, p)]);
            }
        } else {
            self.blit_surface(s, from, dst, to, MASK_RGBA, false, None);
        }
        if let Some(c) = copy {
            self.destroy(c);
        }
    }

    // ---- Copies between images ------------------------------------------------------

    /// Copies a box of texels between images of the same format.
    pub(super) fn copy_box(
        &mut self,
        src: u32,
        src_level: u32,
        region: Region,
        dst: u32,
        dst_level: u32,
        at: [u32; 3],
    ) {
        let (Some(s), Some(d)) = (self.resources.get(&src), self.resources.get(&dst)) else { return };
        let (Some(sh), Some(dh)) = (s.host, d.host) else { return };
        let format = s.desc.format;
        let one = |z: u32| Region { z: region.z + z, d: 1, ..region };
        let flat = self.is_flat_3d(src) || self.is_flat_3d(dst);
        let depth = matches!(format.class(), Class::Depth | Class::DepthStencil | Class::Stencil);
        let renders = depth || (self.host.can_render(sh.virgl) && self.host.can_render(dh.virgl));
        // Images the host can draw are copied by blits, a layer at a time:
        // the host's own copies make only the first layer of a box without
        // glCopyImageSubData, and are not ordered after draws into the
        // source with it on some drivers (see `blit_surface`).
        if renders && !flat {
            let mask = match (depth, format.has_depth(), format.has_stencil()) {
                (false, ..) => MASK_RGBA,
                (true, z, s) => (if z { MASK_Z } else { 0 }) | (if s { MASK_S } else { 0 }),
            };
            let from = [region.x as i32, region.y as i32, region.w as i32, region.h as i32];
            let to = [at[0] as i32, at[1] as i32, region.w as i32, region.h as i32];
            for z in 0..region.d {
                let s = Surface { resource: src, level: src_level, layer: region.z + z };
                let d = Surface { resource: dst, level: dst_level, layer: at[2] + z };
                self.blit_surface(s, from, d, to, mask, false, None);
            }
            return;
        }
        // glCopyImageSubData takes any image (and one the host cannot draw
        // was never drawn into).
        if self.host.has(CAP_COPY_IMAGE) {
            return self.emit_copy(src, src_level, region, dst, dst_level, at);
        }
        // Otherwise through guest memory, a layer at a time: without
        // glCopyImageSubData the host copies through framebuffers, which
        // take neither formats it cannot render to (it would take their
        // texels from guest storage, which resources here lack) nor 3D
        // slices on OpenGL ES.
        let row = region.w as usize * format.bytes();
        let image = row * region.h as usize;
        let Some(mut texels) = try_zeroed(image) else { return };
        for z in 0..region.d {
            self.read_texture(src, src_level, one(z), &mut texels, row, image);
            let to = Region::new(at[0], at[1], at[2] + z, region.w, region.h, 1);
            self.write_texture(dst, dst_level, to, &texels, row, image);
        }
    }

    fn emit_copy(&mut self, src: u32, src_level: u32, r: Region, dst: u32, dst_level: u32, at: [u32; 3]) {
        let w = [dst, dst_level, at[0], at[1], at[2], src, src_level, r.x, r.y, r.z, r.w, r.h, r.d];
        self.emit(CMD_RESOURCE_COPY_REGION, 0, &w);
    }

    // ---- Mipmaps --------------------------------------------------------------------

    /// Fills levels `base + 1..=last` by halving each level into the next:
    /// each layer of a 2D image by a filtered blit, and a 3D texture in
    /// guest memory, as the software renderer does (blits cannot average
    /// slices, nor reach them on OpenGL ES hosts).
    pub(super) fn mipmap(&mut self, id: u32, base: u32, last: u32) {
        let Some(r) = self.resources.get(&id) else { return };
        let d = r.desc;
        if d.target == Target::Texture3D {
            return self.mipmap_volume(id, d, base, last);
        }
        for level in base + 1..=last {
            let size = |n: u32, l: u32| (n >> l).max(1) as i32;
            let (sw, sh) = (size(d.width, level - 1), size(d.height, level - 1));
            let (dw, dh) = (size(d.width, level), size(d.height, level));
            let layers = match d.target {
                Target::TextureCube => 6,
                Target::Texture2DArray => d.depth,
                _ => 1,
            };
            for layer in 0..layers {
                let src = Surface { resource: id, level: level - 1, layer };
                let dst = Surface { resource: id, level, layer };
                self.blit_surface(src, [0, 0, sw, sh], dst, [0, 0, dw, dh], MASK_RGBA, true, None);
            }
        }
    }

    fn mipmap_volume(&mut self, id: u32, d: ResourceDesc, base: u32, last: u32) {
        let f = d.format;
        let tb = f.bytes();
        let size = |l: u32| {
            let s = |n: u32| (n >> l).max(1);
            Geometry::packed(s(d.width), s(d.height), s(d.depth), tb)
        };
        let mut s = size(base);
        let Some(mut above) = try_zeroed(s.image * s.depth as usize) else { return };
        self.read_texture(id, base, Region::new(0, 0, 0, s.width, s.height, s.depth), &mut above, s.row, s.image);
        for level in base + 1..=last {
            let g = size(level);
            let Some(mut texels) = try_zeroed(g.image * g.depth as usize) else { return };
            downsample(f, &above, s, &mut texels, g, true);
            self.write_texture(id, level, Region::new(0, 0, 0, g.width, g.height, g.depth), &texels, g.row, g.image);
            (above, s) = (texels, g);
        }
    }

    // ---- 3D textures on OpenGL ES hosts ------------------------------------------------

    /// Whether a resource is a 3D texture whose slices the host cannot
    /// bind to a framebuffer (see `VirglBackend::flat_3d`).
    pub(super) fn is_flat_3d(&self, id: u32) -> bool {
        self.flat_3d && self.resources.get(&id).is_some_and(|r| r.desc.target == Target::Texture3D)
    }

    /// The program that fetches texels of a kind (0 float, 1 signed, 2
    /// unsigned), compiled once.
    fn fetch_program(&mut self, kind: usize) -> Option<Fetch> {
        if let Some(f) = self.internal.fetch[kind] {
            return Some(f);
        }
        let options = vglsl::Options::default();
        let compile = |stage, source: &str| vglsl::compile(stage, &[source], &options).shader;
        let vs = compile(vglsl::Stage::Vertex, FETCH_VS)?;
        let fs = compile(vglsl::Stage::Fragment, &fetch_fs(["", "i", "u"][kind]))?;
        let bindings = vglsl::link::Bindings {
            attributes: vec![("pos".into(), 0)],
            feedback: Vec::new(),
            feedback_separate: false,
        };
        let p = vglsl::program::link(&vs, &fs, &bindings, &options.limits).program?;
        let at = p.linked.uniforms.iter().find(|u| u.name == "at")?.slot;
        let slots = p.linked.slots;
        let program = self.program(Arc::new(p));
        let f = Fetch { program, slots, at };
        self.internal.fetch[kind] = Some(f);
        Some(f)
    }

    /// Draws texels `x..x + w`, `y..y + h` of a 3D texture's slice into the
    /// corner of a 2D image, as the texture's sampler returns them.
    #[allow(clippy::too_many_arguments)]
    fn fetch(&mut self, f: Fetch, src: Surface, x: u32, y: u32, w: u32, h: u32, dst: u32) {
        let cover = self.cover_buffer();
        let mut fb = Framebuffer { width: w, height: h, ..Framebuffer::default() };
        fb.colors[0] = Some(Surface { resource: dst, level: 0, layer: 0 });
        fb.draw_buffers[0] = Some(0);
        let mut uniforms = vec![[0u32; 4]; f.slots.max(f.at + 1) as usize];
        uniforms[f.at as usize] = [x, y, src.layer, 0];
        let view = View {
            resource: src.resource,
            target: Target::Texture3D,
            base_level: src.level,
            max_level: src.level,
            swizzle: [0, 1, 2, 3],
        };
        let textures = [TextureBinding { view: Some(view), sampler: NEAREST }];
        let attribs = [Attrib {
            buffer: Some(cover),
            offset: 0,
            stride: 16,
            size: 4,
            ty: AttribType::Float,
            normalized: false,
            integer: false,
            divisor: 0,
            current: [0; 4],
        }];
        let state = DrawState {
            framebuffer: &fb,
            viewport: Viewport { x: 0.0, y: 0.0, w: w as f32, h: h as f32, near: 0.0, far: 1.0 },
            scissor: None,
            raster: PLAIN_RASTER,
            depth_stencil: NO_TESTS,
            blend: NO_BLEND,
            program: f.program,
            uniforms: &uniforms,
            blocks: &[],
            textures: &textures,
            attribs: &attribs,
            index: None,
            primitive_restart: false,
            feedback: None,
        };
        self.draw_call(&state, &DrawInfo { mode: Mode::Triangles, first: 0, count: 3, indexed: false, instances: 1 });
    }

    /// Reads a box of a 3D texture the host cannot read directly (its
    /// readback binds the slices to a framebuffer): each slice is drawn,
    /// texel for texel, into an image of 32-bit components, which is read
    /// back and encoded in the texture's format; exactly, as the image
    /// holds every value a sampler returns.
    pub(super) fn read_3d(
        &mut self,
        id: u32,
        level: u32,
        region: Region,
        out: &mut [u8],
        row_pitch: usize,
        image_pitch: usize,
    ) {
        let Some(r) = self.resources.get(&id) else { return };
        let format = r.desc.format;
        let (kind, wide) = match format.class() {
            Class::Sint => (1, Format::Rgba32Sint),
            Class::Uint => (2, Format::Rgba32Uint),
            _ => (0, Format::Rgba32Float),
        };
        let Some(f) = self.fetch_program(kind) else { return };
        let (w, h) = (region.w, region.h);
        let desc = ResourceDesc {
            target: Target::Renderbuffer,
            format: wide,
            width: w,
            height: h,
            depth: 1,
            levels: 1,
            samples: 0,
        };
        let Ok(tmp) = self.create(&desc) else { return };
        let row = w as usize * 16;
        if let Some(mut texels) = try_zeroed(row * h as usize) {
            let tb = format.bytes();
            for z in 0..region.d {
                let slice = Surface { resource: id, level, layer: region.z + z };
                self.fetch(f, slice, region.x, region.y, w, h, tmp);
                self.read_texture(tmp, 0, Region::new(0, 0, 0, w, h, 1), &mut texels, row, row * h as usize);
                for y in 0..h as usize {
                    let d = &mut out[z as usize * image_pitch + y * row_pitch..][..w as usize * tb];
                    for (s, t) in texels[y * row..][..row].as_chunks::<16>().0.iter().zip(d.chunks_exact_mut(tb)) {
                        format.encode(&wide.decode(s), t);
                    }
                }
            }
        }
        self.destroy(tmp);
    }

    /// A 2D texture holding a copy of a 3D texture's slice (with the same
    /// format and size).
    pub(super) fn slice_copy(&mut self, s: Surface) -> Option<u32> {
        let d = self.resources.get(&s.resource)?.desc;
        let (w, h) = ((d.width >> s.level).max(1), (d.height >> s.level).max(1));
        let g = Geometry::packed(w, h, 1, d.format.bytes());
        let mut texels = try_zeroed(g.image)?;
        let desc = ResourceDesc {
            target: Target::Texture2D,
            format: d.format,
            width: w,
            height: h,
            depth: 1,
            levels: 1,
            samples: 0,
        };
        let copy = self.create(&desc).ok()?;
        self.read_3d(s.resource, s.level, Region::new(0, 0, s.layer, w, h, 1), &mut texels, g.row, g.image);
        self.write_texture(copy, 0, Region::new(0, 0, 0, w, h, 1), &texels, g.row, g.image);
        Some(copy)
    }

    /// A framebuffer whose 3D slices the host cannot bind are replaced by
    /// 2D copies, if it has such slices; [`Self::unflatten`] then writes
    /// the copies back. A slice there is no memory for is left out.
    pub(super) fn flatten(&mut self, fb: &Framebuffer) -> Option<(Framebuffer, Proxies)> {
        if !self.flat_3d || !fb.colors.iter().flatten().any(|s| self.is_flat_3d(s.resource)) {
            return None;
        }
        let mut flat = *fb;
        let mut proxies = Vec::new();
        for c in &mut flat.colors {
            let Some(s) = c.filter(|s| self.is_flat_3d(s.resource)) else { continue };
            *c = self.slice_copy(s).map(|p| {
                proxies.push((s, p));
                Surface { resource: p, level: 0, layer: 0 }
            });
        }
        Some((flat, proxies))
    }

    /// Writes 2D copies back to the 3D slices they stood in for, and frees
    /// them.
    pub(super) fn unflatten(&mut self, proxies: Proxies) {
        for (s, p) in proxies {
            let Some(d) = self.resources.get(&p).map(|r| r.desc) else { continue };
            let g = Geometry::packed(d.width, d.height, 1, d.format.bytes());
            if let Some(mut texels) = try_zeroed(g.image) {
                self.read_texture(p, 0, Region::new(0, 0, 0, d.width, d.height, 1), &mut texels, g.row, g.image);
                let to = Region::new(0, 0, s.layer, d.width, d.height, 1);
                self.write_texture(s.resource, s.level, to, &texels, g.row, g.image);
            }
            self.destroy(p);
        }
    }

    // ---- Presenting ------------------------------------------------------------------

    /// Copies a color buffer to a window: resolved if multisampled,
    /// flipped and scaled into a BGRA image the window's size, read back
    /// (and premultiplied by alpha for a translucent window).
    pub(super) fn present_image(&mut self, color: u32, width: u32, height: u32, dst: &mut Present<'_>) {
        if dst.width == 0 || dst.height == 0 {
            return;
        }
        let Some(r) = self.resources.get(&color) else { return };
        let (samples, format) = (r.desc.samples, r.desc.format);
        let mut src = color;
        if samples > 0 {
            let Some(r) = self.resolved(color, width, height, format) else { return };
            src = r;
        }
        let (dw, dh) = (dst.width, dst.height);
        let image = match self.internal.present {
            Some((r, w, h)) if w == dw && h == dh => r,
            other => {
                if let Some((r, _, _)) = other {
                    self.internal.present = None;
                    self.destroy(r);
                }
                let desc = ResourceDesc {
                    target: Target::Renderbuffer,
                    format: Format::Rgbx8Unorm,
                    width: dw,
                    height: dh,
                    depth: 1,
                    levels: 1,
                    samples: 0,
                };
                let Ok(r) = self.create_bgra(&desc) else { return present_by_reading(self, src, width, height, dst) };
                self.internal.present = Some((r, dw, dh));
                r
            }
        };
        let linear = (dw, dh) != (width, height);
        let from = Surface { resource: src, level: 0, layer: 0 };
        let to = Surface { resource: image, level: 0, layer: 0 };
        self.blit_surface(
            from,
            [0, 0, width as i32, height as i32],
            to,
            [0, dh as i32, dw as i32, -(dh as i32)],
            MASK_RGBA,
            linear,
            None,
        );
        // Read back the rows, in bands that fit in staging.
        let row = dw as usize * 4;
        let rows = (self.staging_size / row).max(1) as u32;
        let mut y = 0;
        while y < dh {
            let h = rows.min(dh - y);
            self.submit();
            let at = self.stage(row * h as usize);
            self.transfer(image, 0, [0, y, 0, dw, h, 1], row as u32, (row * h as usize) as u32, at, false);
            self.submit();
            let src = self.shared_mut(at, row * h as usize);
            for k in 0..h as usize {
                let line = &src[k * row..][..row];
                let out = &mut dst.pixels[(y as usize + k) * dst.stride..][..dw as usize];
                if dst.opaque {
                    for (o, p) in out.iter_mut().zip(line.as_chunks::<4>().0) {
                        *o = u32::from_le_bytes([p[0], p[1], p[2], 0xFF]);
                    }
                } else {
                    for (o, p) in out.iter_mut().zip(line.as_chunks::<4>().0) {
                        let a = u32::from(p[3]);
                        // c * a / 255, rounded.
                        let m = |c: u8| {
                            let x = u32::from(c) * a + 128;
                            (x + (x >> 8)) >> 8
                        };
                        *o = (a << 24) | (m(p[2]) << 16) | (m(p[1]) << 8) | m(p[0]);
                    }
                }
            }
            y += h;
        }
    }

    /// A BGRA image (the window's pixel order), which the generic format
    /// table does not offer.
    fn create_bgra(&mut self, desc: &ResourceDesc) -> Result<u32, OutOfMemory> {
        let virgl = format::B8G8R8A8_UNORM;
        if !self.host.can_render(virgl) || !self.host.can_read_back(virgl) {
            return Err(OutOfMemory);
        }
        let args = super::ResourceArgs {
            target: TARGET_2D,
            format: virgl,
            bind: BIND_RENDER_TARGET | BIND_SAMPLER_VIEW,
            width: desc.width,
            height: desc.height,
            depth: 1,
            array_size: 1,
            ..Default::default()
        };
        let handle = self.t.create_resource(&args, Some(self.dummy_backing()))?;
        let host = super::formats::HostFormat { virgl, layout: None, swizzle: [0, 1, 2, 3], renderable: true };
        self.resources.insert(
            handle,
            Res { desc: *desc, host: Some(host), views: Vec::new(), surfaces: Vec::new(), shadow: None },
        );
        Ok(handle)
    }

    /// A single-sampled copy of a multisampled color buffer (none if there
    /// is no memory for it).
    fn resolved(&mut self, color: u32, width: u32, height: u32, format: Format) -> Option<u32> {
        let r = match self.internal.resolve {
            Some((r, w, h, f)) if (w, h, f) == (width, height, format) => r,
            other => {
                if let Some((r, ..)) = other {
                    self.internal.resolve = None;
                    self.destroy(r);
                }
                let desc = ResourceDesc {
                    target: Target::Renderbuffer,
                    format,
                    width,
                    height,
                    depth: 1,
                    levels: 1,
                    samples: 0,
                };
                let r = self.create(&desc).ok()?;
                self.internal.resolve = Some((r, width, height, format));
                r
            }
        };
        let full = [0, 0, width as i32, height as i32];
        let s = |resource| Surface { resource, level: 0, layer: 0 };
        self.blit_surface(s(color), full, s(r), full, MASK_RGBA, false, None);
        Some(r)
    }
}
