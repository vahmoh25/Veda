//! Operations outside the pipeline: copies between images, copies from
//! the framebuffer, blits (OpenGL ES 3.0 section 4.3.3), mipmap generation
//! and transform feedback's writes.

use alloc::vec;
use alloc::vec::Vec;

use super::SoftBackend;
use super::program::SoftProgram;
use super::resource::{Level, Resource};
use crate::backend::{Blit, BufferRange, Filter, Framebuffer, Rect, Region, ResourceId, Surface};
use crate::format::{Class, Format, Geometry, Texel, downsample};
use crate::pixels;
use vmath::f32 as m;

/// Copies a box between images of the same format.
pub fn copy_region(
    b: &mut SoftBackend,
    src: ResourceId,
    src_level: u32,
    region: Region,
    dst: ResourceId,
    dst_level: u32,
    at: [u32; 3],
) {
    let Some(s) = b.res(src) else { return };
    let Some((_, r)) = s.clip(src_level, region) else { return };
    let f = s.desc.format;
    let row = r.w as usize * f.bytes();
    let image = row * r.h as usize;
    let Some(mut tmp) = pixels::try_zeroed(image * r.d as usize) else { return };
    s.read(src_level, r, &mut tmp, row, image);
    if let Some(d) = b.res_mut(dst)
        && d.desc.format == f
    {
        d.write(dst_level, Region::new(at[0], at[1], at[2], r.w, r.h, r.d), &tmp, row, image);
    }
}

/// Reads a rectangle of a framebuffer's color attachment `index`
/// (multisampled pixels resolved) as its format's texels.
fn read_rect(b: &SoftBackend, fb: &Framebuffer, index: u8, rect: Rect) -> Option<(Format, Vec<u8>)> {
    let s = fb.colors.get(index as usize).copied().flatten()?;
    let r = b.res(s.resource)?;
    let f = r.desc.format;
    let row = rect.w as usize * f.bytes();
    let mut out = pixels::try_zeroed(row * rect.h as usize)?;
    r.read(
        s.level,
        Region::new(rect.x as u32, rect.y as u32, s.layer, rect.w as u32, rect.h as u32, 1),
        &mut out,
        row,
        row * rect.h as usize,
    );
    Some((f, out))
}

/// `CopyTexSubImage`: a rectangle of the read buffer to a texture image.
pub fn copy_to_texture(b: &mut SoftBackend, src: &Framebuffer, read: u8, rect: Rect, dst: Surface, x: u32, y: u32) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    let Some((sf, texels)) = read_rect(b, src, read, rect) else { return };
    let Some(d) = b.res_mut(dst.resource) else { return };
    let df = d.desc.format;
    let (w, h) = (rect.w as u32, rect.h as u32);
    let data = if sf == df {
        texels
    } else {
        // Convert, values moving as they are stored (sRGB stays encoded).
        let mut out = vec![0u8; (w * h) as usize * df.bytes()];
        for (o, t) in out.chunks_exact_mut(df.bytes()).zip(texels.chunks_exact(sf.bytes())) {
            pixels::raw(df).encode(&pixels::decode_raw(sf, t), o);
        }
        out
    };
    let row = w as usize * df.bytes();
    d.write(dst.level, Region::new(x, y, dst.layer, w, h, 1), &data, row, row * h as usize);
}

/// The texel of a resource level at `(x, y)` of layer `z`, resolved and
/// decoded (sRGB linearized).
fn texel(r: &Resource, level: u32, x: u32, y: u32, z: u32) -> Texel {
    let f = r.desc.format;
    let lv = r.levels[level as usize];
    let o = lv.at(x, y, z);
    let bytes = &r.data[o..o + lv.texel];
    if r.samples > 1 {
        let mut one = [0u8; 16];
        super::resource::resolve(f, bytes, &mut one[..f.bytes()]);
        f.decode(&one[..f.bytes()])
    } else {
        f.decode(&bytes[..f.bytes()])
    }
}

/// `BlitFramebuffer`.
pub fn blit(b: &mut SoftBackend, blit: &Blit) {
    let [sx0, sy0, sx1, sy1] = blit.src_rect.map(|v| v as f32);
    let [dx0, dy0, dx1, dy1] = blit.dst_rect;
    // Destination pixels: the rectangle (either corner first) clipped to
    // the framebuffer and the scissor.
    let (mut x0, mut x1) = (dx0.min(dx1), dx0.max(dx1));
    let (mut y0, mut y1) = (dy0.min(dy1), dy0.max(dy1));
    if x0 == x1 || y0 == y1 || sx0 == sx1 || sy0 == sy1 {
        return;
    }
    let dst = &blit.dst;
    x0 = x0.max(0);
    y0 = y0.max(0);
    x1 = x1.min(dst.width as i32);
    y1 = y1.min(dst.height as i32);
    if let Some(s) = blit.scissor {
        x0 = x0.max(s.x);
        y0 = y0.max(s.y);
        x1 = x1.min(s.x.saturating_add(s.w));
        y1 = y1.min(s.y.saturating_add(s.h));
    }
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    // Source position of a destination pixel centre (mirroring included).
    let sx = |x: i32| sx0 + ((x as f32 + 0.5) - dx0 as f32) * (sx1 - sx0) / (dx1 - dx0) as f32;
    let sy = |y: i32| sy0 + ((y as f32 + 0.5) - dy0 as f32) * (sy1 - sy0) / (dy1 - dy0) as f32;
    let src = &blit.src;
    let (sw, sh) = (src.width as i32, src.height as i32);
    // Read the whole source first: source and destination may share a
    // resource (other levels or layers).
    let mut writes: Vec<(Surface, u32, u32, Vec<u8>)> = Vec::new();
    if blit.color
        && let Some(ss) = blit.src_color.and_then(|i| src.colors[i as usize])
    {
        let Some(sr) = b.res(ss.resource) else { return };
        let sf = sr.desc.format;
        let integer = sf.is_integer();
        for att in dst.draw_buffers.iter().flatten() {
            let Some(ds) = dst.colors[*att as usize] else { continue };
            let Some(dr) = b.res(ds.resource) else { continue };
            let df = dr.desc.format;
            // Start from the destination: pixels whose source is outside
            // the read buffer keep their values.
            let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
            let row = w as usize * df.bytes();
            let mut buf = vec![0u8; row * h as usize];
            dr.read(ds.level, Region::new(x0 as u32, y0 as u32, ds.layer, w, h, 1), &mut buf, row, row * h as usize);
            for y in y0..y1 {
                for x in x0..x1 {
                    let (fx, fy) = (sx(x), sy(y));
                    let t = if blit.filter == Filter::Linear && !integer {
                        // Bilinear, clamped to the source's edges.
                        let (u, v) = (fx - 0.5, fy - 0.5);
                        let (i0, j0) = (m::floor(u) as i32, m::floor(v) as i32);
                        let (a, c) = (u - i0 as f32, v - j0 as f32);
                        let at = |i: i32, j: i32| {
                            let (i, j) = (i.clamp(0, sw - 1) as u32, j.clamp(0, sh - 1) as u32);
                            texel(sr, ss.level, i, j, ss.layer).as_float()
                        };
                        let l = |p: [f32; 4], q: [f32; 4], t: f32| [0, 1, 2, 3].map(|k| p[k] + (q[k] - p[k]) * t);
                        Texel::Float(l(l(at(i0, j0), at(i0 + 1, j0), a), l(at(i0, j0 + 1), at(i0 + 1, j0 + 1), a), c))
                    } else {
                        let (i, j) = (m::floor(fx) as i32, m::floor(fy) as i32);
                        if i < 0 || j < 0 || i >= sw || j >= sh {
                            // Outside the source: the destination keeps
                            // its value.
                            continue;
                        }
                        texel(sr, ss.level, i as u32, j as u32, ss.layer)
                    };
                    let o = (((y - y0) * (x1 - x0) + (x - x0)) as usize) * df.bytes();
                    df.encode(&t, &mut buf[o..o + df.bytes()]);
                }
            }
            writes.push((ds, (x1 - x0) as u32, (y1 - y0) as u32, buf));
        }
    }
    for (on, s, d) in [(blit.depth, src.depth, dst.depth), (blit.stencil, src.stencil, dst.stencil)] {
        let (true, Some(ss), Some(ds)) = (on, s, d) else { continue };
        if writes.iter().any(|w| w.0 == ds) {
            continue;
        }
        let Some(sr) = b.res(ss.resource) else { continue };
        let f = sr.desc.format;
        // Depth and stencil copy whole texels (both blits use the same
        // nearest mapping; the formats match).
        let mut buf = vec![0u8; ((x1 - x0) * (y1 - y0)) as usize * f.bytes()];
        let Some(dr) = b.res(ds.resource) else { continue };
        for y in y0..y1 {
            for x in x0..x1 {
                let (i, j) = (m::floor(sx(x)) as i32, m::floor(sy(y)) as i32);
                let o = (((y - y0) * (x1 - x0) + (x - x0)) as usize) * f.bytes();
                let (si, sj) = if i < 0 || j < 0 || i >= sw || j >= sh {
                    // Keep the destination's texel.
                    let lv = dr.levels[ds.level as usize];
                    let p = lv.at(x as u32, y as u32, ds.layer);
                    buf[o..o + f.bytes()].copy_from_slice(&dr.data[p..p + f.bytes()]);
                    continue;
                } else {
                    (i as u32, j as u32)
                };
                let lv = sr.levels[ss.level as usize];
                let p = lv.at(si, sj, ss.layer);
                buf[o..o + f.bytes()].copy_from_slice(&sr.data[p..p + f.bytes()]);
            }
        }
        // Depth and stencil share a texel: merge what the blit does not
        // copy from the destination.
        let merge = !(blit.depth && blit.stencil) && f.has_depth() && f.has_stencil();
        if merge {
            let lv = dr.levels[ds.level as usize];
            for y in y0..y1 {
                for x in x0..x1 {
                    let o = (((y - y0) * (x1 - x0) + (x - x0)) as usize) * f.bytes();
                    let p = lv.at(x as u32, y as u32, ds.layer);
                    let old = f.decode(&dr.data[p..p + f.bytes()]);
                    let new = f.decode(&buf[o..o + f.bytes()]);
                    let t = if blit.depth {
                        Texel::Depth(new.depth(), old.stencil())
                    } else {
                        Texel::Depth(old.depth(), new.stencil())
                    };
                    f.encode(&t, &mut buf[o..o + f.bytes()]);
                }
            }
        }
        writes.push((ds, (x1 - x0) as u32, (y1 - y0) as u32, buf));
    }
    for (s, w, h, buf) in writes {
        if let Some(r) = b.res_mut(s.resource) {
            let row = w as usize * r.desc.format.bytes();
            r.write(s.level, Region::new(x0 as u32, y0 as u32, s.layer, w, h, 1), &buf, row, row * h as usize);
        }
    }
}

/// Fills levels `base + 1..=last` of a texture with box-filtered
/// reductions of the level above (color formats; sRGB averaged in linear
/// space).
pub fn generate_mipmap(b: &mut SoftBackend, id: ResourceId, base: u32, last: u32) {
    let Some(r) = b.res_mut(id) else { return };
    let f = r.desc.format;
    if matches!(f.class(), Class::Depth | Class::DepthStencil | Class::Stencil) || f.is_integer() || r.samples > 1 {
        return;
    }
    let volume = r.desc.target == crate::backend::Target::Texture3D;
    let geometry =
        |v: &Level| Geometry { width: v.width, height: v.height, depth: v.layers, row: v.row, image: v.image };
    for l in base + 1..=last.min(r.levels.len() as u32 - 1) {
        let (src, dst) = (r.levels[l as usize - 1], r.levels[l as usize]);
        // Each level follows the one above it in memory.
        let (above, below) = r.data.split_at_mut(dst.offset);
        let to = &mut below[..dst.bytes()];
        downsample(f, &above[src.offset..], geometry(&src), to, geometry(&dst), volume);
    }
}

/// Writes the components transform feedback recorded (`recorded`: each
/// vertex's captured components in the program's order) to the buffers'
/// ranges.
pub fn write_feedback(b: &mut SoftBackend, p: &SoftProgram, ranges: &[BufferRange], recorded: &[u32]) {
    let ncap = p.feedback.len();
    if ncap == 0 {
        return;
    }
    let vertices = recorded.len() / ncap;
    for (bi, range) in ranges.iter().enumerate() {
        let stride = p.feedback_strides.get(bi).copied().unwrap_or(0);
        if stride == 0 {
            continue;
        }
        let comps: Vec<usize> =
            p.feedback.iter().enumerate().filter(|(_, c)| c.buffer as usize == bi).map(|(k, _)| k).collect();
        b.flush_if_uses(range.buffer);
        let Some(r) = b.res_mut(range.buffer) else { continue };
        for v in 0..vertices {
            let at = range.offset + v * stride;
            if (v + 1) * stride > range.size || at + stride > r.data.len() {
                break;
            }
            for (j, &k) in comps.iter().enumerate() {
                let o = at + j * 4;
                r.data[o..o + 4].copy_from_slice(&recorded[v * ncap + k].to_le_bytes());
            }
        }
    }
}
