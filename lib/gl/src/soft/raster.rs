//! Rendering a scene's tiles (OpenGL ES 3.0 chapters 3 and 4):
//! rasterization, the fragment shader, and the per-fragment operations.
//!
//! A tile is worked through in 4x4 blocks, which are the sixteen lanes of
//! the shader interpreter: four 2x2 quads, so that derivatives and texture
//! levels of detail can be taken across each quad. Pixels of a quad that
//! the primitive does not cover still run the shader (as helpers) but are
//! never written.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use vglsl::interp::{Env, Exec, LANES, Lanes, Mask};
use vglsl::link::Interpolation;
use vglsl::types::Scalar;

use super::program::SoftProgram;
use super::scene::{ClearCmd, Cmd, DrawRecord, LinePrim, PointPrim, SUB, Scene, TILE, Targets, Tri, sample_positions};
use super::target::{RenderTarget, blend, stencil_op, unorm8};
use super::texture::DrawTextures;
use crate::backend::Stencil;
use crate::format::{Class, Format, Texel};

/// Pixel offsets of each lane in a 4x4 block: four 2x2 quads, row-major,
/// each quad's pixels row-major (y up).
const LX: [i32; LANES] = [0, 1, 0, 1, 2, 3, 2, 3, 0, 1, 0, 1, 2, 3, 2, 3];
const LY: [i32; LANES] = [0, 0, 1, 1, 0, 0, 1, 1, 2, 2, 3, 3, 2, 2, 3, 3];

/// What a draw's fragment shaders read besides their inputs.
pub struct DrawEnv<'a> {
    pub blocks: Vec<&'a [u8]>,
    pub textures: DrawTextures<'a>,
}

/// A worker's state.
pub struct Worker {
    exec: Exec,
    /// The draw whose prologue the registers hold.
    loaded: Option<u32>,
}

/// A block of fragments after rasterization.
struct Block {
    x: i32,
    y: i32,
    /// Covered samples of each lane.
    cov: [u8; LANES],
    /// Depth of each lane's samples (window coordinates).
    z: [[f32; 4]; LANES],
    front: bool,
}

impl Worker {
    pub fn new(registers: usize) -> Worker {
        let empty = vglsl::interp::Program {
            prologue: Vec::new(),
            main: Vec::new(),
            tex: Vec::new(),
            tex_size: Vec::new(),
            registers,
            uniform_registers: 0,
            inputs: Vec::new(),
            outputs: Vec::new(),
            discards: false,
            derivatives: false,
        };
        Worker { exec: Exec::new(&empty), loaded: None }
    }

    /// Renders tile `index` of a scene.
    pub fn tile(&mut self, scene: &Scene, envs: &[DrawEnv<'_>], index: usize) {
        let tx = (index as u32 % scene.tiles_x) as i32;
        let ty = (index as u32 / scene.tiles_x) as i32;
        let t = &scene.targets;
        let rect =
            [tx * TILE, ty * TILE, ((tx + 1) * TILE).min(t.width as i32), ((ty + 1) * TILE).min(t.height as i32)];
        for &cmd in &scene.bins[index] {
            match cmd {
                Cmd::Clear(i) => clear(t, &scene.clears[i as usize], rect),
                Cmd::Tri(i) => {
                    let tri = &scene.tris[i as usize];
                    self.primitive(scene, envs, tri.draw, intersect(rect, tri.bbox), |w, d, bx, by, r| {
                        w.tri_block(scene, tri, d, bx, by, r)
                    });
                }
                Cmd::Line(i) => {
                    let l = &scene.lines[i as usize];
                    self.primitive(scene, envs, l.draw, intersect(rect, l.bbox), |w, d, bx, by, r| {
                        w.line_block(scene, l, d, bx, by, r)
                    });
                }
                Cmd::Point(i) => {
                    let p = &scene.points[i as usize];
                    self.primitive(scene, envs, p.draw, intersect(rect, p.bbox), |w, d, bx, by, r| {
                        w.point_block(scene, p, d, bx, by, r)
                    });
                }
            }
        }
    }

    /// Runs `block` over the 4x4 blocks of `rect`, then the fragment
    /// stage on each block it covers.
    fn primitive(
        &mut self,
        scene: &Scene,
        envs: &[DrawEnv<'_>],
        draw: u32,
        rect: [i32; 4],
        mut block: impl FnMut(&mut Worker, &DrawRecord, i32, i32, [i32; 4]) -> Option<Block>,
    ) {
        if rect[0] >= rect[2] || rect[1] >= rect[3] {
            return;
        }
        let d = &scene.draws[draw as usize];
        let e = &envs[draw as usize];
        if self.loaded != Some(draw) {
            let p = &d.program.fs;
            if self.exec.regs.len() < d.program.registers {
                self.exec.regs.resize(d.program.registers, Lanes::ZERO);
            }
            self.exec.regs[..d.prologue.len()].copy_from_slice(&d.prologue);
            let _ = p;
            self.loaded = Some(draw);
        }
        let env = Env { uniforms: &d.uniforms, blocks: &e.blocks, textures: &e.textures };
        let mut by = rect[1] & !3;
        while by < rect[3] {
            let mut bx = rect[0] & !3;
            while bx < rect[2] {
                if let Some(mut b) = block(self, d, bx, by, rect) {
                    self.fragment(&scene.targets, d, &env, &mut b);
                }
                bx += 4;
            }
            by += 4;
        }
    }

    // ---- Rasterization ------------------------------------------------------------

    fn tri_block(&mut self, scene: &Scene, t: &Tri, d: &DrawRecord, bx: i32, by: i32, rect: [i32; 4]) -> Option<Block> {
        let samples = scene.targets.samples.max(1);
        let pos = sample_positions(samples);
        let all = ((1u32 << samples) - 1) as u8;
        // The edges at the corners of the block's samples: a block wholly
        // outside an edge is skipped, one inside all edges is covered.
        let (smin, smax) = if samples > 1 { (32, 224) } else { (128, 128) };
        let (x0, x1) = (i64::from(bx) * SUB + smin, i64::from(bx + 3) * SUB + smax);
        let (y0, y1) = (i64::from(by) * SUB + smin, i64::from(by + 3) * SUB + smax);
        let mut inside = true;
        for e in &t.edges {
            let (ex0, ex1, ey0, ey1) = (e[0] * x0, e[0] * x1, e[1] * y0, e[1] * y1);
            if ex0.max(ex1) + ey0.max(ey1) + e[2] < 0 {
                return None;
            }
            inside &= ex0.min(ex1) + ey0.min(ey1) + e[2] >= 0;
        }
        let whole = bx >= rect[0] && by >= rect[1] && bx + 4 <= rect[2] && by + 4 <= rect[3];
        let mut cov = [0u8; LANES];
        let mut any = 0u8;
        if inside && whole {
            cov = [all; LANES];
            any = all;
        } else {
            for lane in 0..LANES {
                let (x, y) = (bx + LX[lane], by + LY[lane]);
                if x < rect[0] || x >= rect[2] || y < rect[1] || y >= rect[3] {
                    continue;
                }
                let mut m = 0u8;
                for (s, &(sx, sy)) in pos.iter().enumerate() {
                    let (px, py) = (i64::from(x) * SUB + sx, i64::from(y) * SUB + sy);
                    if t.edges.iter().all(|e| e[0] * px + e[1] * py + e[2] >= 0) {
                        m |= 1 << s;
                    }
                }
                cov[lane] = m;
                any |= m;
            }
        }
        if any == 0 {
            return None;
        }
        let p = &d.program;
        let a = &scene.attrs[t.attrs as usize..];
        let iw = [f32::from_bits(a[0]), f32::from_bits(a[1]), f32::from_bits(a[2])];
        // Screen-space barycentrics of vertices 1 and 2, 1/w and depth at
        // each lane's centre: planes, stepped from the block's first centre.
        let (cx, cy) = (bx as f32 + 0.5 - t.origin[0], by as f32 + 0.5 - t.origin[1]);
        let l1_0 = t.l1[0] * cx + t.l1[1] * cy;
        let l2_0 = t.l2[0] * cx + t.l2[1] * cy;
        let z_0 = t.z[2] + t.z[0] * cx + t.z[1] * cy;
        let mut l0 = [0.0f32; LANES];
        let mut l1 = [0.0f32; LANES];
        let mut l2 = [0.0f32; LANES];
        let mut fw = [0.0f32; LANES];
        let mut inv = [0.0f32; LANES];
        let mut fz = [0.0f32; LANES];
        for lane in 0..LANES {
            let (dx, dy) = (LX[lane] as f32, LY[lane] as f32);
            l1[lane] = l1_0 + t.l1[0] * dx + t.l1[1] * dy;
            l2[lane] = l2_0 + t.l2[0] * dx + t.l2[1] * dy;
            l0[lane] = 1.0 - l1[lane] - l2[lane];
            fw[lane] = l0[lane] * iw[0] + l1[lane] * iw[1] + l2[lane] * iw[2];
            inv[lane] = 1.0 / fw[lane];
            fz[lane] = z_0 + t.z[0] * dx + t.z[1] * dy;
        }
        // Depth at each sample (the centre for single sampling).
        let mut z = [[0.0f32; 4]; LANES];
        if samples > 1 {
            for (lane, zl) in z.iter_mut().enumerate() {
                for (s, &(sx, sy)) in pos.iter().enumerate() {
                    let (ox, oy) = ((sx - 128) as f32 / SUB as f32, (sy - 128) as f32 / SUB as f32);
                    zl[s] = fz[lane] + t.z[0] * ox + t.z[1] * oy;
                }
            }
        } else {
            for (zl, &f) in z.iter_mut().zip(&fz) {
                zl[0] = f;
            }
        }
        // Centroid varyings of partly covered pixels are taken at a covered
        // sample.
        let centroid = samples > 1 && p.centroid;
        let (mut c0, mut c1, mut c2, mut cinv) = (l0, l1, l2, inv);
        if centroid {
            for lane in 0..LANES {
                let c = cov[lane];
                if c == 0 || c == all {
                    continue;
                }
                let (sx, sy) = pos[c.trailing_zeros() as usize];
                let (ox, oy) = ((sx - 128) as f32 / SUB as f32, (sy - 128) as f32 / SUB as f32);
                c1[lane] = l1[lane] + t.l1[0] * ox + t.l1[1] * oy;
                c2[lane] = l2[lane] + t.l2[0] * ox + t.l2[1] * oy;
                c0[lane] = 1.0 - c1[lane] - c2[lane];
                cinv[lane] = 1.0 / (c0[lane] * iw[0] + c1[lane] * iw[1] + c2[lane] * iw[2]);
            }
        }
        // Varyings: (sum of weights times value/w) times w.
        let regs = &mut self.exec.regs;
        let mut o = 3;
        for v in &p.varyings {
            let r = &mut regs[v.fs as usize].0;
            match v.interpolation {
                Interpolation::Flat => {
                    *r = [a[o]; LANES];
                    o += 1;
                }
                kind => {
                    let (p0, p1, p2) = (f32::from_bits(a[o]), f32::from_bits(a[o + 1]), f32::from_bits(a[o + 2]));
                    let (w0, w1, w2, wi) =
                        if kind == Interpolation::Centroid { (&c0, &c1, &c2, &cinv) } else { (&l0, &l1, &l2, &inv) };
                    for lane in 0..LANES {
                        r[lane] = ((w0[lane] * p0 + w1[lane] * p1 + w2[lane] * p2) * wi[lane]).to_bits();
                    }
                    o += 3;
                }
            }
        }
        self.frag_coord(p, bx, by, &fz, &fw);
        Some(Block { x: bx, y: by, cov, z, front: t.front })
    }

    fn line_block(
        &mut self,
        scene: &Scene,
        l: &LinePrim,
        d: &DrawRecord,
        bx: i32,
        by: i32,
        rect: [i32; 4],
    ) -> Option<Block> {
        let samples = scene.targets.samples.max(1);
        let pos = sample_positions(samples);
        let (ax, ay, bxx, byy) = (l.a[0], l.a[1], l.b[0], l.b[1]);
        let (dx, dy) = (bxx - ax, byy - ay);
        let covered = |px: f32, py: f32| {
            // The parallelogram of the segment: a span of `width` across
            // its minor axis; the last pixel is left to the next segment.
            if l.x_major {
                let t = (px - ax) / dx;
                let ly = ay + t * dy;
                (0.0..1.0).contains(&t) && py - ly >= -l.half_width && py - ly < l.half_width
            } else {
                let t = (py - ay) / dy;
                let lx = ax + t * dx;
                (0.0..1.0).contains(&t) && px - lx >= -l.half_width && px - lx < l.half_width
            }
        };
        let mut cov = [0u8; LANES];
        let mut any = 0;
        for lane in 0..LANES {
            let (x, y) = (bx + LX[lane], by + LY[lane]);
            if x < rect[0] || x >= rect[2] || y < rect[1] || y >= rect[3] {
                continue;
            }
            let mut m = 0u8;
            for (s, &(sx, sy)) in pos.iter().enumerate() {
                if covered(x as f32 + sx as f32 / SUB as f32, y as f32 + sy as f32 / SUB as f32) {
                    m |= 1 << s;
                }
            }
            cov[lane] = m;
            any |= m;
        }
        if any == 0 {
            return None;
        }
        let p = &d.program;
        let a = &scene.attrs[l.attrs as usize..];
        let iw = [f32::from_bits(a[0]), f32::from_bits(a[1])];
        let len2 = dx * dx + dy * dy;
        let mut ts = [0.0f32; LANES];
        let mut inv = [0.0f32; LANES];
        let mut fz = [0.0f32; LANES];
        let mut fw = [0.0f32; LANES];
        let mut z = [[0.0f32; 4]; LANES];
        for lane in 0..LANES {
            let (cx, cy) = ((bx + LX[lane]) as f32 + 0.5, (by + LY[lane]) as f32 + 0.5);
            let t = (((cx - ax) * dx + (cy - ay) * dy) / len2).clamp(0.0, 1.0);
            let w = (1.0 - t) * iw[0] + t * iw[1];
            ts[lane] = t;
            inv[lane] = 1.0 / w;
            fw[lane] = w;
            fz[lane] = l.a[2] + t * (l.b[2] - l.a[2]);
            z[lane] = [fz[lane]; 4];
        }
        let regs = &mut self.exec.regs;
        let mut o = 2;
        for v in &p.varyings {
            let r = &mut regs[v.fs as usize].0;
            if v.interpolation == Interpolation::Flat {
                *r = [a[o]; LANES];
                o += 1;
            } else {
                let (pa, pb) = (f32::from_bits(a[o]), f32::from_bits(a[o + 1]));
                for lane in 0..LANES {
                    let t = ts[lane];
                    r[lane] = (((1.0 - t) * pa + t * pb) * inv[lane]).to_bits();
                }
                o += 2;
            }
        }
        self.frag_coord(p, bx, by, &fz, &fw);
        Some(Block { x: bx, y: by, cov, z, front: true })
    }

    fn point_block(
        &mut self,
        scene: &Scene,
        pt: &PointPrim,
        d: &DrawRecord,
        bx: i32,
        by: i32,
        rect: [i32; 4],
    ) -> Option<Block> {
        let samples = scene.targets.samples.max(1);
        let pos = sample_positions(samples);
        let h = pt.size * 0.5;
        let (x0, x1) = (pt.center[0] - h, pt.center[0] + h);
        let (y0, y1) = (pt.center[1] - h, pt.center[1] + h);
        let mut cov = [0u8; LANES];
        let mut any = 0;
        for lane in 0..LANES {
            let (x, y) = (bx + LX[lane], by + LY[lane]);
            if x < rect[0] || x >= rect[2] || y < rect[1] || y >= rect[3] {
                continue;
            }
            let mut m = 0u8;
            for (s, &(sx, sy)) in pos.iter().enumerate() {
                let (px, py) = (x as f32 + sx as f32 / SUB as f32, y as f32 + sy as f32 / SUB as f32);
                if px >= x0 && px < x1 && py >= y0 && py < y1 {
                    m |= 1 << s;
                }
            }
            cov[lane] = m;
            any |= m;
        }
        if any == 0 {
            return None;
        }
        let p = &d.program;
        let a = &scene.attrs[pt.attrs as usize..];
        let regs = &mut self.exec.regs;
        for (k, v) in p.varyings.iter().enumerate() {
            regs[v.fs as usize] = Lanes::splat(a[k]);
        }
        // gl_PointCoord: (0, 0) at the upper left.
        for (c, r) in p.point_coord.iter().enumerate() {
            let Some(r) = r else { continue };
            let reg = &mut regs[*r as usize].0;
            for lane in 0..LANES {
                let (cx, cy) = ((bx + LX[lane]) as f32 + 0.5, (by + LY[lane]) as f32 + 0.5);
                let v = if c == 0 { 0.5 + (cx - pt.center[0]) / pt.size } else { 0.5 - (cy - pt.center[1]) / pt.size };
                reg[lane] = v.to_bits();
            }
        }
        let fz = [pt.z; LANES];
        let fw = [pt.inv_w; LANES];
        self.frag_coord(p, bx, by, &fz, &fw);
        Some(Block { x: bx, y: by, cov, z: [[pt.z; 4]; LANES], front: true })
    }

    /// Fills `gl_FragCoord`.
    fn frag_coord(&mut self, p: &SoftProgram, bx: i32, by: i32, z: &[f32; LANES], w: &[f32; LANES]) {
        for (c, r) in p.frag_coord.iter().enumerate() {
            let Some(r) = r else { continue };
            let reg = &mut self.exec.regs[*r as usize].0;
            for lane in 0..LANES {
                let v = match c {
                    0 => (bx + LX[lane]) as f32 + 0.5,
                    1 => (by + LY[lane]) as f32 + 0.5,
                    2 => z[lane],
                    _ => w[lane],
                };
                reg[lane] = v.to_bits();
            }
        }
    }

    // ---- Fragments ----------------------------------------------------------------

    fn fragment(&mut self, targets: &Targets, d: &DrawRecord, env: &Env<'_>, b: &mut Block) {
        let p = &d.program;
        let samples = targets.samples.max(1);
        let all_samples: u8 = ((1u32 << samples) - 1) as u8;
        if let Some(r) = p.front_facing {
            self.exec.regs[r as usize] = Lanes::splat(if b.front { !0 } else { 0 });
        }
        let s = &d.state;
        if s.early {
            let n = depth_stencil(targets, d, b, None);
            if let Some(q) = &d.query
                && n > 0
            {
                q.fetch_add(n, Ordering::Relaxed);
            }
        }
        let mask = quads(&b.cov);
        if mask == 0 {
            return;
        }
        let alive = self.exec.run(&p.fs, mask, env);
        for (lane, c) in b.cov.iter_mut().enumerate() {
            if alive & (1 << lane) == 0 {
                *c = 0;
            }
        }
        // Multisample coverage operations.
        if samples > 1 {
            if s.alpha_to_coverage
                && let Some(o) = &p.outputs[0]
                && let Some(r) = o.regs[3]
            {
                let alpha = self.exec.regs[r as usize];
                for lane in 0..LANES {
                    let a = alpha.f32(lane).clamp(0.0, 1.0);
                    let n = (a * samples as f32 + 0.5) as u32;
                    b.cov[lane] &= ((1u32 << n.min(samples)) - 1) as u8;
                }
            }
            if let Some((value, invert)) = s.sample_coverage {
                let n = (value.clamp(0.0, 1.0) * samples as f32 + 0.5) as u32;
                let mut m = ((1u32 << n.min(samples)) - 1) as u8;
                if invert {
                    m = !m & all_samples;
                }
                for c in &mut b.cov {
                    *c &= m;
                }
            }
        }
        if !s.early {
            let frag_depth = p.frag_depth.map(|r| self.exec.regs[r as usize]);
            let n = depth_stencil(targets, d, b, frag_depth.as_ref());
            if let Some(q) = &d.query
                && n > 0
            {
                q.fetch_add(n, Ordering::Relaxed);
            }
        }
        if b.cov.iter().all(|&c| c == 0) {
            return;
        }
        self.write_colors(targets, d, b);
    }

    fn write_colors(&self, targets: &Targets, d: &DrawRecord, b: &Block) {
        let p = &d.program;
        let s = &d.state;
        let samples = targets.samples.max(1);
        for (i, buffer) in s.draw_buffers.iter().enumerate() {
            let Some(att) = *buffer else { continue };
            let Some(t) = targets.colors.get(att as usize).copied().flatten() else { continue };
            let Some(out) = p.outputs.get(i).copied().flatten() else { continue };
            let regs: [Option<Lanes>; 4] = out.regs.map(|r| r.map(|r| self.exec.regs[r as usize]));
            let class = t.format.class();
            let integer = matches!(class, Class::Uint | Class::Sint);
            let one = if out.scalar == Scalar::Float { 1.0f32.to_bits() } else { 1 };
            let blending = s.blend.enabled && !integer;
            let clamp = matches!(class, Class::Unorm | Class::Snorm);
            let mask = s.blend.write_mask;
            // The common case: single-sampled RGBA8, every component written.
            if samples == 1 && mask == [true; 4] && matches!(t.format, Format::Rgba8Unorm | Format::Rgbx8Unorm) {
                write_rgba8(&t, b, &regs, &s.blend, blending);
                continue;
            }
            for lane in 0..LANES {
                let c = b.cov[lane];
                if c == 0 {
                    continue;
                }
                let src = [0, 1, 2, 3]
                    .map(|k| f32::from_bits(regs[k].as_ref().map_or(if k == 3 { one } else { 0 }, |r| r.0[lane])));
                let (x, y) = ((b.x + LX[lane]) as u32, (b.y + LY[lane]) as u32);
                for sm in 0..samples {
                    if c & (1 << sm) == 0 {
                        continue;
                    }
                    // SAFETY: covered pixels are inside this worker's tile
                    // and the framebuffer.
                    unsafe {
                        if blending {
                            let dst = t.load(x, y, sm);
                            t.store(x, y, sm, blend(&s.blend, src, dst, clamp), mask);
                        } else {
                            t.store(x, y, sm, src, mask);
                        }
                    }
                }
            }
        }
    }
}

/// The lanes of every quad with a covered sample.
fn quads(cov: &[u8; LANES]) -> Mask {
    let mut mask = 0;
    for q in 0..LANES / 4 {
        if cov[q * 4..q * 4 + 4].iter().any(|&c| c != 0) {
            mask |= 0xF << (q * 4);
        }
    }
    mask
}

fn intersect(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])]
}

/// The stencil and depth tests of a block's covered samples (sections
/// 4.1.4 and 4.1.5): updates the buffers and the coverage; returns how many
/// samples passed.
fn depth_stencil(t: &Targets, d: &DrawRecord, b: &mut Block, frag_depth: Option<&Lanes>) -> u64 {
    let ds = &d.state.depth_stencil;
    let samples = t.samples.max(1);
    let depth = t.depth.filter(|_| ds.depth_test);
    let stencil = t.stencil.filter(|_| ds.stencil_test);
    let face: &Stencil = if b.front { &ds.front } else { &ds.back };
    let reference = face.reference.clamp(0, 255) as u8;
    let (vm, wm) = (face.value_mask as u8, face.write_mask as u8);
    if let (1, None, Some(dt)) = (samples, stencil, depth) {
        return depth_only(&dt, ds.depth_func, ds.depth_write, b, frag_depth);
    }
    let mut passed = 0u64;
    if depth.is_none() && stencil.is_none() {
        for &c in &b.cov {
            passed += u64::from(c.count_ones());
        }
        return passed;
    }
    for lane in 0..LANES {
        let c = b.cov[lane];
        if c == 0 {
            continue;
        }
        let (x, y) = ((b.x + LX[lane]) as u32, (b.y + LY[lane]) as u32);
        let mut kept = 0u8;
        for sm in 0..samples {
            if c & (1 << sm) == 0 {
                continue;
            }
            // SAFETY: covered pixels are inside this worker's tile.
            unsafe {
                let sval = stencil.map(|st| st.load_stencil(x, y, sm));
                let write_stencil = |st: &RenderTarget, v: u8| {
                    let old = st.load_stencil(x, y, sm);
                    st.store_stencil(x, y, sm, (old & !wm) | (v & wm));
                };
                if let (Some(st), Some(sv)) = (stencil, sval)
                    && !face.func.test(reference & vm, sv & vm)
                {
                    write_stencil(&st, stencil_op(face.fail, sv, reference));
                    continue;
                }
                if let Some(dt) = depth {
                    let z = frag_depth.map_or(b.z[lane][sm as usize], |f| f.f32(lane));
                    let q = dt.quantize(z);
                    if !q.test(ds.depth_func, dt.load_depth(x, y, sm)) {
                        if let (Some(st), Some(sv)) = (stencil, sval) {
                            write_stencil(&st, stencil_op(face.depth_fail, sv, reference));
                        }
                        continue;
                    }
                    if ds.depth_write {
                        dt.store_depth(x, y, sm, q);
                    }
                }
                if let (Some(st), Some(sv)) = (stencil, sval) {
                    write_stencil(&st, stencil_op(face.pass, sv, reference));
                }
            }
            kept |= 1 << sm;
        }
        b.cov[lane] = kept;
        passed += u64::from(kept.count_ones());
    }
    passed
}

/// Clears the part of `rect` a clear command covers.
pub fn clear(t: &Targets, c: &ClearCmd, rect: [i32; 4]) {
    let r = intersect(rect, c.rect);
    if r[0] >= r[2] || r[1] >= r[3] {
        return;
    }
    let samples = t.samples.max(1);
    for (i, value) in c.colors.iter().enumerate() {
        let (Some(v), Some(target)) = (value, t.colors[i]) else { continue };
        let f = target.format;
        let texel = match f.class() {
            Class::Sint => Texel::Int(v.map(|x| x as i32)),
            Class::Uint => Texel::Uint(*v),
            _ => Texel::Float(v.map(f32::from_bits)),
        };
        let n = f.bytes();
        let mut bytes = [0u8; 16];
        f.encode(&texel, &mut bytes[..n]);
        let full = c.color_mask == [true; 4];
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                for s in 0..samples {
                    // SAFETY: the pixel is inside this worker's tile.
                    unsafe {
                        if full {
                            core::ptr::copy_nonoverlapping(bytes.as_ptr(), target.sample(x as u32, y as u32, s), n);
                        } else {
                            let v4 = match texel {
                                Texel::Float(f) => f,
                                Texel::Int(i) => i.map(|x| f32::from_bits(x as u32)),
                                Texel::Uint(u) => u.map(f32::from_bits),
                                Texel::Depth(..) => [0.0; 4],
                            };
                            target.store(x as u32, y as u32, s, v4, c.color_mask);
                        }
                    }
                }
            }
        }
    }
    if let (Some(dv), Some(dt)) = (c.depth, t.depth) {
        let q = dt.quantize(dv);
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                for s in 0..samples {
                    // SAFETY: as above.
                    unsafe { dt.store_depth(x as u32, y as u32, s, q) };
                }
            }
        }
    }
    if let (Some(sv), Some(st)) = (c.stencil, t.stencil) {
        let (v, m) = (sv as u8, c.stencil_mask as u8);
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                for s in 0..samples {
                    // SAFETY: as above.
                    unsafe {
                        let old = st.load_stencil(x as u32, y as u32, s);
                        st.store_stencil(x as u32, y as u32, s, (old & !m) | (v & m));
                    }
                }
            }
        }
    }
}

/// Writes a block's colors to a single-sampled RGBA8 target (blending if
/// `blending`).
fn write_rgba8(
    t: &RenderTarget,
    b: &Block,
    regs: &[Option<Lanes>; 4],
    blend_state: &crate::backend::Blend,
    blending: bool,
) {
    let comp = |k: usize, lane: usize| regs[k].as_ref().map_or(if k == 3 { 1.0 } else { 0.0 }, |r| r.f32(lane));
    for lane in 0..LANES {
        if b.cov[lane] == 0 {
            continue;
        }
        let (x, y) = ((b.x + LX[lane]) as u32, (b.y + LY[lane]) as u32);
        let src = [comp(0, lane), comp(1, lane), comp(2, lane), comp(3, lane)];
        // SAFETY: covered pixels are inside this worker's tile and the
        // framebuffer; the texel is 4 bytes.
        unsafe {
            let p = t.sample(x, y, 0).cast::<u32>();
            let v = if blending {
                let d = u32::from_le(p.read_unaligned()).to_le_bytes();
                let dst = [d[0], d[1], d[2], d[3]].map(|c| f32::from(c) * (1.0 / 255.0));
                blend(blend_state, src, dst, true)
            } else {
                src
            };
            let bytes = [unorm8(v[0]), unorm8(v[1]), unorm8(v[2]), unorm8(v[3])];
            p.write_unaligned(u32::from_le_bytes(bytes).to_le());
        }
    }
}

/// [`depth_stencil`] for single-sampled depth buffers without a stencil
/// test.
fn depth_only(
    dt: &RenderTarget,
    func: crate::backend::Func,
    write: bool,
    b: &mut Block,
    frag_depth: Option<&Lanes>,
) -> u64 {
    let mut passed = 0;
    let z = |b: &Block, lane: usize| {
        let z = frag_depth.map_or(b.z[lane][0], |f| f.f32(lane));
        if z > 0.0 { if z < 1.0 { z } else { 1.0 } } else { 0.0 }
    };
    for lane in 0..LANES {
        if b.cov[lane] == 0 {
            continue;
        }
        let (x, y) = ((b.x + LX[lane]) as u32, (b.y + LY[lane]) as u32);
        let zl = z(b, lane);
        // SAFETY: covered pixels are inside this worker's tile and the
        // framebuffer.
        let pass = unsafe {
            let p = dt.sample(x, y, 0);
            match dt.format {
                Format::D24UnormS8Uint | Format::D24Unorm => {
                    let q = (f64::from(zl) * 16_777_215.0 + 0.5) as u32;
                    let old = u32::from_le(p.cast::<u32>().read_unaligned());
                    let ok = func.test(q, old >> 8);
                    if ok && write {
                        p.cast::<u32>().write_unaligned(((q << 8) | (old & 0xFF)).to_le());
                    }
                    ok
                }
                Format::D16Unorm => {
                    let q = (zl * 65535.0 + 0.5) as u16;
                    let ok = func.test(q, u16::from_le(p.cast::<u16>().read_unaligned()));
                    if ok && write {
                        p.cast::<u16>().write_unaligned(q.to_le());
                    }
                    ok
                }
                _ => {
                    let ok = func.test(zl, f32::from_bits(u32::from_le(p.cast::<u32>().read_unaligned())));
                    if ok && write {
                        p.cast::<u32>().write_unaligned(zl.to_bits().to_le());
                    }
                    ok
                }
            }
        };
        if pass {
            passed += 1;
        } else {
            b.cov[lane] = 0;
        }
    }
    passed
}
