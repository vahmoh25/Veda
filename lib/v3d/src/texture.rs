//! Textures: power-of-two images with a full mip chain, and procedural
//! generators (checkerboards, tileable noise, gradients, soft sprites).
//!
//! Pixels are given as straight-alpha `0xAARRGGBB` and stored premultiplied
//! (the vgfx convention), which keeps mipmaps and blending free of dark
//! fringes. Textures always repeat; sampling is nearest-neighbour from the
//! mip level chosen per pixel by the rasterizer.

use alloc::vec;
use alloc::vec::Vec;

use crate::pipeline::{FxTexture, MAX_MIPS, Mip};

/// Handle of a texture registered with a [`crate::Renderer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextureId(pub(crate) u32);

/// A mip-mapped texture.
pub struct Texture {
    width: u32,
    height: u32,
    levels: Vec<Vec<u32>>,
    pub(crate) fx: FxTexture,
}

// SAFETY: the raw pointers in `fx` point into `levels`, which is never
// mutated after construction; sharing read-only access is sound.
unsafe impl Send for Texture {}
// SAFETY: as above.
unsafe impl Sync for Texture {}

impl Clone for Texture {
    fn clone(&self) -> Texture {
        Texture::from_levels(self.width, self.height, self.levels.clone())
    }
}

impl core::fmt::Debug for Texture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Texture({}x{}, {} levels)", self.width, self.height, self.levels.len())
    }
}

fn premultiply(px: u32) -> u32 {
    let a = px >> 24;
    match a {
        255 => px,
        0 => 0,
        _ => {
            let ch = |s: u32| ((((px >> s) & 255) * a + 127) / 255) << s;
            (a << 24) | ch(16) | ch(8) | ch(0)
        }
    }
}

/// Averages four pixels per channel.
fn average4(a: u32, b: u32, c: u32, d: u32) -> u32 {
    let lo = (a & 0x00FF_00FF) + (b & 0x00FF_00FF) + (c & 0x00FF_00FF) + (d & 0x00FF_00FF) + 0x0002_0002;
    let hi = ((a >> 8) & 0x00FF_00FF)
        + ((b >> 8) & 0x00FF_00FF)
        + ((c >> 8) & 0x00FF_00FF)
        + ((d >> 8) & 0x00FF_00FF)
        + 0x0002_0002;
    ((lo >> 2) & 0x00FF_00FF) | (((hi >> 2) & 0x00FF_00FF) << 8)
}

impl Texture {
    /// Creates a texture from straight-alpha pixels. The size is rounded up
    /// to powers of two (resampling nearest-neighbour) and capped at 4096.
    pub fn new(width: u32, height: u32, pixels: &[u32]) -> Texture {
        let (w, h) = (width.max(1), height.max(1));
        let (pw, ph) = (w.next_power_of_two().min(4096), h.next_power_of_two().min(4096));
        let mut base = vec![0u32; (pw * ph) as usize];
        for y in 0..ph {
            let sy = (y as u64 * h as u64 / ph as u64) as u32;
            for x in 0..pw {
                let sx = (x as u64 * w as u64 / pw as u64) as u32;
                let px = pixels.get((sy * width + sx) as usize).copied().unwrap_or(0xFFFF_00FF);
                base[(y * pw + x) as usize] = premultiply(px);
            }
        }
        Texture::from_base(pw, ph, base)
    }

    /// Creates a texture by evaluating `f(x, y)` (straight alpha) for every
    /// texel; `width` and `height` must be powers of two.
    pub fn from_fn(width: u32, height: u32, mut f: impl FnMut(u32, u32) -> u32) -> Texture {
        let (pw, ph) = (width.max(1).next_power_of_two().min(4096), height.max(1).next_power_of_two().min(4096));
        let mut base = Vec::with_capacity((pw * ph) as usize);
        for y in 0..ph {
            for x in 0..pw {
                base.push(premultiply(f(x, y)));
            }
        }
        Texture::from_base(pw, ph, base)
    }

    /// A single-colour 1x1 texture.
    pub fn solid(color: u32) -> Texture {
        Texture::from_fn(1, 1, |_, _| color)
    }

    fn from_base(w: u32, h: u32, base: Vec<u32>) -> Texture {
        let mut levels = vec![base];
        let (mut lw, mut lh) = (w, h);
        while (lw > 1 || lh > 1) && levels.len() < MAX_MIPS {
            let (nw, nh) = ((lw / 2).max(1), (lh / 2).max(1));
            let prev = levels.last().unwrap();
            let mut next = Vec::with_capacity((nw * nh) as usize);
            for y in 0..nh {
                let (y0, y1) = ((y * 2).min(lh - 1), (y * 2 + 1).min(lh - 1));
                for x in 0..nw {
                    let (x0, x1) = ((x * 2).min(lw - 1), (x * 2 + 1).min(lw - 1));
                    next.push(average4(
                        prev[(y0 * lw + x0) as usize],
                        prev[(y0 * lw + x1) as usize],
                        prev[(y1 * lw + x0) as usize],
                        prev[(y1 * lw + x1) as usize],
                    ));
                }
            }
            levels.push(next);
            lw = nw;
            lh = nh;
        }
        Texture::from_levels(w, h, levels)
    }

    fn from_levels(width: u32, height: u32, levels: Vec<Vec<u32>>) -> Texture {
        let empty = Mip { pixels: core::ptr::null(), wlog2: 0, hlog2: 0 };
        let mut fx = FxTexture { levels: [empty; MAX_MIPS], count: levels.len() as i32 };
        let (mut w, mut h) = (width, height);
        for (i, l) in levels.iter().enumerate() {
            fx.levels[i] =
                Mip { pixels: l.as_ptr(), wlog2: w.trailing_zeros() as i32, hlog2: h.trailing_zeros() as i32 };
            w = (w / 2).max(1);
            h = (h / 2).max(1);
        }
        Texture { width, height, levels, fx }
    }

    /// Width of mip 0 in texels (a power of two).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height of mip 0 in texels (a power of two).
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Number of mip levels (down to 1x1).
    pub fn mip_count(&self) -> usize {
        self.levels.len()
    }

    /// Premultiplied texel of mip `level` (wrapping coordinates).
    pub fn texel(&self, level: usize, x: i32, y: i32) -> u32 {
        let l = level.min(self.levels.len() - 1);
        let (w, h) = ((self.width >> l).max(1) as i32, (self.height >> l).max(1) as i32);
        self.levels[l][(y.rem_euclid(h) * w + x.rem_euclid(w)) as usize]
    }

    // -- Procedural textures -------------------------------------------------

    /// A checkerboard of `cells` x `cells` squares.
    pub fn checker(size: u32, cells: u32, a: u32, b: u32) -> Texture {
        let cell = (size / cells.max(1)).max(1);
        Texture::from_fn(size, size, |x, y| if ((x / cell) + (y / cell)).is_multiple_of(2) { a } else { b })
    }

    /// Tileable fractal value noise blended between colours `a` and `b`.
    /// `cells` is the lattice size of the first octave (a power of two that
    /// divides `size`).
    pub fn noise(size: u32, seed: u32, cells: u32, octaves: u32, a: u32, b: u32) -> Texture {
        let size = size.next_power_of_two();
        Texture::from_fn(size, size, |x, y| {
            let n = fbm_tiled(x, y, size, cells, octaves, seed);
            lerp_color(a, b, n)
        })
    }

    /// A vertical gradient from `top` to `bottom` (1 texel wide).
    pub fn gradient(height: u32, top: u32, bottom: u32) -> Texture {
        let h = height.next_power_of_two().max(2);
        Texture::from_fn(1, h, |_, y| lerp_color(top, bottom, (y * 256 / (h - 1)).min(256)))
    }

    /// A round sprite: `color` with alpha falling off from the centre
    /// (`hardness` 0 = very soft glow, 1 = nearly a hard disc).
    pub fn radial(size: u32, color: u32, hardness: f32) -> Texture {
        let size = size.next_power_of_two().max(4);
        let half = size as f32 / 2.0;
        let k = 1.0 + hardness.clamp(0.0, 1.0) * 6.0;
        Texture::from_fn(size, size, |x, y| {
            let dx = (x as f32 + 0.5 - half) / half;
            let dy = (y as f32 + 0.5 - half) / half;
            let d = vmath::FloatExt::sqrt(dx * dx + dy * dy);
            let t = (1.0 - d).clamp(0.0, 1.0);
            // Smooth falloff, sharper with hardness.
            let a = (t * k).min(1.0);
            let a = a * a * (3.0 - 2.0 * a);
            let alpha = ((color >> 24) as f32 * a) as u32;
            (alpha.min(255) << 24) | (color & 0x00FF_FFFF)
        })
    }
}

/// Blends two straight-alpha colours, `t` in 0..=256.
pub fn lerp_color(a: u32, b: u32, t: u32) -> u32 {
    let t = t.min(256);
    let it = 256 - t;
    let ch = |s: u32| ((((a >> s) & 255) * it + ((b >> s) & 255) * t) >> 8) << s;
    ch(24) | ch(16) | ch(8) | ch(0)
}

#[inline]
fn hash2(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x.wrapping_mul(0x8DA6_B343) ^ y.wrapping_mul(0xD816_3841) ^ seed.wrapping_mul(0xCB1A_B31F);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5BD1_E995);
    h ^ (h >> 15)
}

/// Smooth value noise in 0..=256 that repeats every `period` lattice cells.
/// `x`, `y` are in 1/256 of a cell.
fn value_noise(x: u32, y: u32, period: u32, seed: u32) -> u32 {
    let (cx, cy) = (x >> 8, y >> 8);
    let (fx, fy) = (x & 255, y & 255);
    // Smoothstep weights (0..=256).
    let sx = (fx * fx * (768 - 2 * fx)) >> 16;
    let sy = (fy * fy * (768 - 2 * fy)) >> 16;
    let p = period.max(1);
    let v = |i: u32, j: u32| hash2((cx + i) % p, (cy + j) % p, seed) & 255;
    let top = v(0, 0) * (256 - sx) + v(1, 0) * sx;
    let bottom = v(0, 1) * (256 - sx) + v(1, 1) * sx;
    ((top >> 8) * (256 - sy) + (bottom >> 8) * sy) >> 8
}

/// Tileable fractal noise (0..=256) at texel (x, y) of a `size` texture.
pub fn fbm_tiled(x: u32, y: u32, size: u32, cells: u32, octaves: u32, seed: u32) -> u32 {
    let mut total = 0u32;
    let mut weight = 0u32;
    let mut amp = 128u32;
    let mut c = cells.max(1);
    for o in 0..octaves.max(1) {
        // Position in 1/256 cells: x * c / size.
        let px = ((x as u64 * c as u64 * 256) / size as u64) as u32;
        let py = ((y as u64 * c as u64 * 256) / size as u64) as u32;
        total += value_noise(px, py, c, seed.wrapping_add(o * 1013)) * amp;
        weight += amp;
        amp = (amp / 2).max(1);
        c *= 2;
    }
    (total / weight.max(1)).min(256)
}
