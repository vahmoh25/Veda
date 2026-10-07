//! Resource storage: buffers, and images laid out level by level.
//!
//! A level is `layers` images of `height` rows of `width` texels, each
//! texel `samples` consecutive values of the format (one for single-sampled
//! resources). Rows and images are tightly packed.

use alloc::vec::Vec;

use crate::backend::{OutOfMemory, Region, ResourceDesc, Target};
use crate::format::{Class, Format, Texel};

/// Where a level is in a resource's memory.
#[derive(Clone, Copy, Debug)]
pub struct Level {
    pub offset: usize,
    pub width: u32,
    pub height: u32,
    /// 3D slices, array layers or cube faces.
    pub layers: u32,
    /// Bytes per texel (all its samples).
    pub texel: usize,
    pub row: usize,
    pub image: usize,
}

impl Level {
    pub fn bytes(&self) -> usize {
        self.image * self.layers as usize
    }

    /// The byte offset of texel `(x, y)` of image `z`, within the
    /// resource.
    #[inline(always)]
    pub fn at(&self, x: u32, y: u32, z: u32) -> usize {
        self.offset + z as usize * self.image + y as usize * self.row + x as usize * self.texel
    }
}

/// A buffer, texture or renderbuffer's memory.
pub struct Resource {
    pub desc: ResourceDesc,
    pub levels: Vec<Level>,
    /// Values per texel (1 if single-sampled).
    pub samples: u32,
    pub data: Vec<u8>,
}

impl core::fmt::Debug for Resource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Resource({:?}, {} bytes)", self.desc, self.data.len())
    }
}

/// The dimension of level `l` of a dimension `size` (never below 1).
pub fn minify(size: u32, l: u32) -> u32 {
    (size >> l.min(31)).max(1)
}

impl Resource {
    /// A zeroed resource, or `OutOfMemory`.
    pub fn new(desc: &ResourceDesc) -> Result<Resource, OutOfMemory> {
        let samples = desc.samples.max(1);
        let mut levels = Vec::new();
        let mut total = 0usize;
        if desc.target == Target::Buffer {
            levels.push(Level {
                offset: 0,
                width: desc.width,
                height: 1,
                layers: 1,
                texel: 1,
                row: desc.width as usize,
                image: desc.width as usize,
            });
            total = desc.width as usize;
        } else {
            let texel = desc.format.bytes().checked_mul(samples as usize).ok_or(OutOfMemory)?;
            for l in 0..desc.levels.max(1) {
                let (w, h) = (minify(desc.width, l), minify(desc.height, l));
                let layers = if desc.target == Target::Texture3D { minify(desc.depth, l) } else { desc.depth.max(1) };
                let row = (w as usize).checked_mul(texel).ok_or(OutOfMemory)?;
                let image = row.checked_mul(h as usize).ok_or(OutOfMemory)?;
                let size = image.checked_mul(layers as usize).ok_or(OutOfMemory)?;
                levels.push(Level { offset: total, width: w, height: h, layers, texel, row, image });
                total = total.checked_add(size).ok_or(OutOfMemory)?;
            }
        }
        let mut data = Vec::new();
        data.try_reserve_exact(total).map_err(|_| OutOfMemory)?;
        data.resize(total, 0);
        Ok(Resource { desc: *desc, levels, samples, data })
    }

    pub fn format(&self) -> Format {
        self.desc.format
    }

    /// The level, if it exists.
    pub fn level(&self, l: u32) -> Option<&Level> {
        self.levels.get(l as usize)
    }

    /// Clamps a region to a level: `None` if nothing of it is inside.
    pub fn clip(&self, l: u32, r: Region) -> Option<(Level, Region)> {
        let lv = *self.level(l)?;
        if r.x >= lv.width || r.y >= lv.height || r.z >= lv.layers {
            return None;
        }
        let w = r.w.min(lv.width - r.x);
        let h = r.h.min(lv.height - r.y);
        let d = r.d.min(lv.layers - r.z);
        if w == 0 || h == 0 || d == 0 {
            return None;
        }
        Some((lv, Region { w, h, d, ..r }))
    }

    /// Writes a region from `src` (rows `row_pitch` apart, images
    /// `image_pitch`), every sample of each texel the same.
    pub fn write(&mut self, l: u32, r: Region, src: &[u8], row_pitch: usize, image_pitch: usize) {
        if self.desc.target == Target::Buffer {
            let (start, n) = (r.x as usize, r.w as usize);
            let end = (start + n).min(self.data.len());
            if start < end {
                let n = (end - start).min(src.len());
                self.data[start..start + n].copy_from_slice(&src[..n]);
            }
            return;
        }
        let Some((lv, c)) = self.clip(l, r) else { return };
        let tb = self.desc.format.bytes();
        let samples = self.samples as usize;
        for z in 0..c.d {
            for y in 0..c.h {
                let s = z as usize * image_pitch + y as usize * row_pitch;
                let Some(srow) = src.get(s..s + c.w as usize * tb) else { continue };
                let d = lv.at(c.x, c.y + y, c.z + z);
                let drow = &mut self.data[d..d + c.w as usize * lv.texel];
                if samples == 1 {
                    drow.copy_from_slice(srow);
                } else {
                    for (dt, st) in drow.chunks_exact_mut(lv.texel).zip(srow.chunks_exact(tb)) {
                        for s in dt.chunks_exact_mut(tb) {
                            s.copy_from_slice(st);
                        }
                    }
                }
            }
        }
    }

    /// Reads a region into `out` (laid out as for [`Resource::write`]);
    /// multisampled texels are resolved.
    pub fn read(&self, l: u32, r: Region, out: &mut [u8], row_pitch: usize, image_pitch: usize) {
        if self.desc.target == Target::Buffer {
            let (start, n) = (r.x as usize, r.w as usize);
            let end = (start + n).min(self.data.len());
            if start < end {
                let n = (end - start).min(out.len());
                out[..n].copy_from_slice(&self.data[start..start + n]);
            }
            return;
        }
        let Some((lv, c)) = self.clip(l, r) else { return };
        let f = self.desc.format;
        let tb = f.bytes();
        for z in 0..c.d {
            for y in 0..c.h {
                let o = z as usize * image_pitch + y as usize * row_pitch;
                let Some(orow) = out.get_mut(o..o + c.w as usize * tb) else { continue };
                let s = lv.at(c.x, c.y + y, c.z + z);
                let srow = &self.data[s..s + c.w as usize * lv.texel];
                if self.samples == 1 {
                    orow.copy_from_slice(srow);
                } else {
                    for (ot, st) in orow.chunks_exact_mut(tb).zip(srow.chunks_exact(lv.texel)) {
                        resolve(f, st, ot);
                    }
                }
            }
        }
    }
}

/// Resolves a multisampled texel (its samples, one after another) into one
/// value: the average for color formats, the first sample for integer,
/// depth and stencil formats.
pub fn resolve(f: Format, samples: &[u8], out: &mut [u8]) {
    let tb = f.bytes();
    match f.class() {
        Class::Unorm | Class::Snorm | Class::Float => {
            let n = samples.len() / tb;
            let mut sum = [0.0f32; 4];
            for s in samples.chunks_exact(tb) {
                // sRGB samples average in linear space.
                let v = f.decode(s).as_float();
                for i in 0..4 {
                    sum[i] += v[i];
                }
            }
            let avg = sum.map(|v| v / n as f32);
            f.encode(&Texel::Float(avg), out);
        }
        _ => out.copy_from_slice(&samples[..tb]),
    }
}
