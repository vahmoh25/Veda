//! Texture sampling for the shader interpreter (OpenGL ES 3.0 sections
//! 3.8.10 to 3.8.16): every target, nearest and linear filtering within and
//! between mipmap levels, the wrap modes, seamless cube maps, depth
//! comparison and swizzles.
//!
//! The level of detail of an implicit lookup comes from the coordinates'
//! differences across each 2x2 quad of fragments, as GPUs compute it: the
//! four fragments of a quad share one level of detail.

use alloc::vec::Vec;

use vglsl::builtins::TexLod;
use vglsl::interp::{LANES, Lanes, Mask, Textures};
use vglsl::ir::TexOp;
use vglsl::ops::f16_to_f32;
use vglsl::types::Dim;
use vmath::f32 as m;

use crate::backend::{Filter, Func, SamplerState, Wrap};
use crate::format::{Class, Format};

pub use crate::format::srgb_to_linear;

/// One mipmap level of a sampled texture.
#[derive(Clone, Copy, Debug)]
pub struct TexLevel {
    pub ptr: *const u8,
    pub width: u32,
    pub height: u32,
    /// 3D slices, array layers or cube faces.
    pub depth: u32,
    pub row: usize,
    pub image: usize,
}

/// What texels hold, as samplers return them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Float,
    Int,
    Uint,
    Depth,
}

/// A texture bound to a sampler for one draw: levels base to max of its
/// view, and the sampler state.
#[derive(Clone, Debug)]
pub struct Sampled {
    pub levels: Vec<TexLevel>,
    pub dim: Dim,
    pub format: Format,
    pub texel: usize,
    pub kind: Kind,
    pub swizzle: [u8; 4],
    pub state: SamplerState,
}

// SAFETY: the level pointers point into resources the renderer keeps alive
// and unchanged while a draw that samples them runs (they are never written
// by it: a texture that is also rendered to is not sampled).
unsafe impl Send for Sampled {}
unsafe impl Sync for Sampled {}

/// Texel values before swizzling: floats, or integers as their bits.
type Texel4 = [f32; 4];

impl Sampled {
    /// The kind of values a format's texels are.
    pub fn kind_of(f: Format) -> Kind {
        match f.class() {
            Class::Sint => Kind::Int,
            Class::Uint => Kind::Uint,
            Class::Depth | Class::DepthStencil => Kind::Depth,
            _ => Kind::Float,
        }
    }

    /// Texel `(i, j, k)` of level `l` (coordinates within the level).
    #[inline(always)]
    fn fetch(&self, l: &TexLevel, i: u32, j: u32, k: u32) -> Texel4 {
        debug_assert!(i < l.width && j < l.height && k < l.depth);
        let o = k as usize * l.image + j as usize * l.row + i as usize * self.texel;
        // SAFETY: the coordinates are within the level (every caller wraps
        // or clamps them first), so the texel is inside the resource.
        let p = unsafe { l.ptr.add(o) };
        match self.format {
            // The common formats, as one 32-bit read.
            Format::Rgba8Unorm | Format::Rgbx8Unorm | Format::Rgba8Srgb | Format::Rgbx8Srgb => {
                // SAFETY: as above; these texels are 4 bytes.
                let w = u32::from_le(unsafe { p.cast::<u32>().read_unaligned() });
                let b = w.to_le_bytes();
                let u = |x: u8| f32::from(x) * (1.0 / 255.0);
                match self.format {
                    Format::Rgba8Unorm => [u(b[0]), u(b[1]), u(b[2]), u(b[3])],
                    Format::Rgbx8Unorm => [u(b[0]), u(b[1]), u(b[2]), 1.0],
                    Format::Rgba8Srgb => [srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), u(b[3])],
                    _ => [srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), 1.0],
                }
            }
            _ => {
                // SAFETY: as above.
                let b = unsafe { core::slice::from_raw_parts(p, self.texel) };
                decode(self.format, b)
            }
        }
    }
}

/// A texel as four values: floats for normalized and float formats,
/// integer bits for integer formats, the depth for depth formats.
#[inline(always)]
fn decode(f: Format, b: &[u8]) -> Texel4 {
    let u8n = |x: u8| f32::from(x) * (1.0 / 255.0);
    match f {
        Format::Rgba8Unorm => [u8n(b[0]), u8n(b[1]), u8n(b[2]), u8n(b[3])],
        Format::Rgbx8Unorm => [u8n(b[0]), u8n(b[1]), u8n(b[2]), 1.0],
        Format::Rgba8Srgb => [srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), u8n(b[3])],
        Format::Rgbx8Srgb => [srgb_to_linear(b[0]), srgb_to_linear(b[1]), srgb_to_linear(b[2]), 1.0],
        Format::R8Unorm => [u8n(b[0]), 0.0, 0.0, 1.0],
        Format::Rg8Unorm => [u8n(b[0]), u8n(b[1]), 0.0, 1.0],
        Format::L8Unorm => {
            let l = u8n(b[0]);
            [l, l, l, 1.0]
        }
        Format::A8Unorm => [0.0, 0.0, 0.0, u8n(b[0])],
        Format::L8A8Unorm => {
            let l = u8n(b[0]);
            [l, l, l, u8n(b[1])]
        }
        Format::Rgba16Float => {
            let h = |i: usize| f16_to_f32(u16::from_le_bytes([b[2 * i], b[2 * i + 1]]));
            [h(0), h(1), h(2), h(3)]
        }
        Format::Rgba32Float => {
            let f = |i: usize| f32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
            [f(0), f(1), f(2), f(3)]
        }
        Format::R32Float => [f32::from_le_bytes([b[0], b[1], b[2], b[3]]), 0.0, 0.0, 1.0],
        Format::D24UnormS8Uint | Format::D24Unorm => {
            let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8;
            [v as f32 * (1.0 / 16_777_215.0), 0.0, 0.0, 1.0]
        }
        Format::D16Unorm => [f32::from(u16::from_le_bytes([b[0], b[1]])) * (1.0 / 65535.0), 0.0, 0.0, 1.0],
        Format::D32Float | Format::D32FloatS8Uint => [f32::from_le_bytes([b[0], b[1], b[2], b[3]]), 0.0, 0.0, 1.0],
        _ => match f.decode(b) {
            crate::format::Texel::Float(v) => v,
            crate::format::Texel::Int(v) => v.map(|x| f32::from_bits(x as u32)),
            crate::format::Texel::Uint(v) => v.map(f32::from_bits),
            crate::format::Texel::Depth(d, _) => [d, 0.0, 0.0, 1.0],
        },
    }
}

/// `floor` as an integer (saturating).
#[inline(always)]
fn ifloor(x: f32) -> i32 {
    let i = x as i32;
    if (i as f32) > x { i - 1 } else { i }
}

/// Wraps an integer texel coordinate into `0..size`.
#[inline(always)]
fn wrap(i: i32, size: u32, mode: Wrap) -> u32 {
    // Inside already: every mode leaves it (no division).
    if (i as u32) < size {
        return i as u32;
    }
    wrap_outside(i, size, mode)
}

#[inline(never)]
fn wrap_outside(i: i32, size: u32, mode: Wrap) -> u32 {
    let s = size as i32;
    match mode {
        Wrap::Repeat if size.is_power_of_two() => (i & (s - 1)) as u32,
        Wrap::ClampToEdge => i.clamp(0, s - 1) as u32,
        Wrap::Repeat => i.rem_euclid(s) as u32,
        Wrap::MirroredRepeat => {
            let m = i.rem_euclid(2 * s);
            (if m < s { m } else { 2 * s - 1 - m }) as u32
        }
    }
}

/// A cube map direction's face and coordinates in [0, 1] (table 3.21).
fn cube_face(x: f32, y: f32, z: f32) -> (u32, f32, f32) {
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, sc, tc, ma) = if ax >= ay && ax >= az {
        if x >= 0.0 { (0, -z, -y, ax) } else { (1, z, -y, ax) }
    } else if ay >= az {
        if y >= 0.0 { (2, x, z, ay) } else { (3, x, -z, ay) }
    } else if z >= 0.0 {
        (4, x, -y, az)
    } else {
        (5, -x, -y, az)
    };
    let inv = if ma > 0.0 { 1.0 / ma } else { 0.0 };
    (face, (sc * inv + 1.0) * 0.5, (tc * inv + 1.0) * 0.5)
}

/// The direction through the centre of texel `(i, j)` (possibly outside
/// the face) of face `face` of a cube of `size` texels.
fn cube_direction(face: u32, i: i32, j: i32, size: u32) -> (f32, f32, f32) {
    let sc = 2.0 * (i as f32 + 0.5) / size as f32 - 1.0;
    let tc = 2.0 * (j as f32 + 0.5) / size as f32 - 1.0;
    match face {
        0 => (1.0, -tc, -sc),
        1 => (-1.0, -tc, sc),
        2 => (sc, 1.0, tc),
        3 => (sc, -1.0, -tc),
        4 => (sc, -tc, 1.0),
        _ => (-sc, -tc, -1.0),
    }
}

/// How a lookup samples: the level(s) and filter.
#[derive(Clone, Copy)]
struct Choice {
    level: usize,
    /// A second level and its weight (linear mipmapping).
    next: Option<(usize, f32)>,
    filter: Filter,
}

impl Sampled {
    /// The levels and filter for level of detail `lambda` (section
    /// 3.8.10.4; `c` is 0).
    fn choose(&self, lambda: f32) -> Choice {
        let st = &self.state;
        let lambda = if lambda.is_nan() { 0.0 } else { lambda.clamp(st.min_lod, st.max_lod) };
        let q = self.levels.len() - 1;
        if lambda <= 0.0 {
            return Choice { level: 0, next: None, filter: st.mag };
        }
        // lambda > 0 from here: truncation is floor.
        match st.mip {
            None => Choice { level: 0, next: None, filter: st.min },
            Some(Filter::Nearest) => {
                // ceil(lambda + 0.5) - 1, rounding half down.
                let x = lambda + 0.5;
                let c = x as usize + usize::from(x > (x as usize) as f32);
                let d = if lambda <= 0.5 { 0 } else { c.saturating_sub(1).min(q) };
                Choice { level: d, next: None, filter: st.min }
            }
            Some(Filter::Linear) => {
                let fl = (lambda.min(1e6) as usize) as f32;
                let d1 = (fl as usize).min(q);
                if d1 >= q {
                    Choice { level: q, next: None, filter: st.min }
                } else {
                    Choice { level: d1, next: Some((d1 + 1, lambda - fl)), filter: st.min }
                }
            }
        }
    }

    /// Filters level `l` at normalized coordinates `(s, t, r)` (`r` is
    /// the layer for arrays) with an integer texel offset.
    fn filter2d(
        &self,
        l: usize,
        s: f32,
        t: f32,
        layer: u32,
        filter: Filter,
        off: [i8; 3],
        cmp: Option<(Func, f32)>,
    ) -> Texel4 {
        let lv = &self.levels[l];
        let w = self.state.wrap;
        let (u, v) = (s * lv.width as f32, t * lv.height as f32);
        match filter {
            Filter::Nearest => {
                let i = wrap(ifloor(u).saturating_add(i32::from(off[0])), lv.width, w[0]);
                let j = wrap(ifloor(v).saturating_add(i32::from(off[1])), lv.height, w[1]);
                self.compare(self.fetch(lv, i, j, layer), cmp)
            }
            Filter::Linear => {
                let (u, v) = (u - 0.5, v - 0.5);
                let (fu, fv) = (ifloor(u), ifloor(v));
                let (a, b) = (u - fu as f32, v - fv as f32);
                let i0 = fu.saturating_add(i32::from(off[0]));
                let j0 = fv.saturating_add(i32::from(off[1]));
                let (x0, x1) = (wrap(i0, lv.width, w[0]), wrap(i0.saturating_add(1), lv.width, w[0]));
                let (y0, y1) = (wrap(j0, lv.height, w[1]), wrap(j0.saturating_add(1), lv.height, w[1]));
                let t00 = self.compare(self.fetch(lv, x0, y0, layer), cmp);
                let t10 = self.compare(self.fetch(lv, x1, y0, layer), cmp);
                let t01 = self.compare(self.fetch(lv, x0, y1, layer), cmp);
                let t11 = self.compare(self.fetch(lv, x1, y1, layer), cmp);
                bilerp(t00, t10, t01, t11, a, b)
            }
        }
    }

    /// Filters a 3D level.
    fn filter3d(&self, l: usize, s: f32, t: f32, r: f32, filter: Filter, off: [i8; 3]) -> Texel4 {
        let lv = &self.levels[l];
        let wm = self.state.wrap;
        let (u, v, w) = (s * lv.width as f32, t * lv.height as f32, r * lv.depth as f32);
        match filter {
            Filter::Nearest => {
                let i = wrap(ifloor(u).saturating_add(i32::from(off[0])), lv.width, wm[0]);
                let j = wrap(ifloor(v).saturating_add(i32::from(off[1])), lv.height, wm[1]);
                let k = wrap(ifloor(w).saturating_add(i32::from(off[2])), lv.depth, wm[2]);
                self.fetch(lv, i, j, k)
            }
            Filter::Linear => {
                let (u, v, w) = (u - 0.5, v - 0.5, w - 0.5);
                let (fu, fv, fw) = (ifloor(u), ifloor(v), ifloor(w));
                let (a, b, c) = (u - fu as f32, v - fv as f32, w - fw as f32);
                let i0 = fu.saturating_add(i32::from(off[0]));
                let j0 = fv.saturating_add(i32::from(off[1]));
                let k0 = fw.saturating_add(i32::from(off[2]));
                let xs = [wrap(i0, lv.width, wm[0]), wrap(i0.saturating_add(1), lv.width, wm[0])];
                let ys = [wrap(j0, lv.height, wm[1]), wrap(j0.saturating_add(1), lv.height, wm[1])];
                let zs = [wrap(k0, lv.depth, wm[2]), wrap(k0.saturating_add(1), lv.depth, wm[2])];
                let slice = |k: u32| {
                    bilerp(
                        self.fetch(lv, xs[0], ys[0], k),
                        self.fetch(lv, xs[1], ys[0], k),
                        self.fetch(lv, xs[0], ys[1], k),
                        self.fetch(lv, xs[1], ys[1], k),
                        a,
                        b,
                    )
                };
                lerp4(slice(zs[0]), slice(zs[1]), c)
            }
        }
    }

    /// Filters a cube map level along direction `(x, y, z)`, across face
    /// edges (cube maps are seamless in OpenGL ES 3.0).
    fn filter_cube(&self, l: usize, x: f32, y: f32, z: f32, filter: Filter, cmp: Option<(Func, f32)>) -> Texel4 {
        let lv = &self.levels[l];
        let size = lv.width;
        let (face, s, t) = cube_face(x, y, z);
        let (u, v) = (s * size as f32, t * size as f32);
        let texel = |i: i32, j: i32| -> Texel4 {
            let n = size as i32;
            let (f, i, j) = if (0..n).contains(&i) && (0..n).contains(&j) {
                (face, i as u32, j as u32)
            } else {
                // Off the face: the texel of the face the direction through
                // its centre meets.
                let (dx, dy, dz) = cube_direction(face, i, j, size);
                let (f, s2, t2) = cube_face(dx, dy, dz);
                let clampi = |c: f32| (ifloor(c * size as f32)).clamp(0, n - 1) as u32;
                (f, clampi(s2), clampi(t2))
            };
            self.compare(self.fetch(lv, i, j, f), cmp)
        };
        match filter {
            Filter::Nearest => {
                let n = size as i32 - 1;
                texel(ifloor(u).clamp(0, n), ifloor(v).clamp(0, n))
            }
            Filter::Linear => {
                let (u, v) = (u - 0.5, v - 0.5);
                let (i0, j0) = (ifloor(u), ifloor(v));
                let (a, b) = (u - i0 as f32, v - j0 as f32);
                bilerp(texel(i0, j0), texel(i0 + 1, j0), texel(i0, j0 + 1), texel(i0 + 1, j0 + 1), a, b)
            }
        }
    }

    /// A depth comparison's result (1 or 0) in place of the texel, if
    /// comparing.
    #[inline(always)]
    fn compare(&self, t: Texel4, cmp: Option<(Func, f32)>) -> Texel4 {
        match cmp {
            Some((func, reference)) => {
                let r = if func.test(reference, t[0]) { 1.0 } else { 0.0 };
                [r, 0.0, 0.0, 1.0]
            }
            None => t,
        }
    }

    /// Samples at `coords` with level choice `c`.
    fn lookup(&self, coords: [f32; 4], c: Choice, off: [i8; 3], cmp: Option<(Func, f32)>) -> Texel4 {
        let at = |l: usize, filter: Filter| -> Texel4 {
            match self.dim {
                Dim::D2 => self.filter2d(l, coords[0], coords[1], 0, filter, off, cmp),
                Dim::D2Array => {
                    let layers = self.levels[l].depth as i32;
                    let layer = ifloor(coords[2] + 0.5).clamp(0, layers - 1) as u32;
                    self.filter2d(l, coords[0], coords[1], layer, filter, off, cmp)
                }
                Dim::D3 => self.filter3d(l, coords[0], coords[1], coords[2], filter, off),
                Dim::Cube => self.filter_cube(l, coords[0], coords[1], coords[2], filter, cmp),
            }
        };
        let a = at(c.level, c.filter);
        match c.next {
            Some((l2, f)) => lerp4(a, at(l2, c.filter), f),
            None => a,
        }
    }

    /// The texel `texelFetch` reads (integer coordinates and level), or
    /// `None` if they are outside the texture.
    fn texel_fetch(&self, coords: [i32; 3], level: i32, off: [i8; 3]) -> Option<Texel4> {
        let l = usize::try_from(level).ok().filter(|&l| l < self.levels.len())?;
        let lv = &self.levels[l];
        let i = coords[0].checked_add(i32::from(off[0]))?;
        let j = coords[1].checked_add(i32::from(off[1]))?;
        let k = match self.dim {
            Dim::D3 => coords[2].checked_add(i32::from(off[2]))?,
            Dim::D2Array => coords[2],
            _ => 0,
        };
        if i < 0 || j < 0 || k < 0 || i as u32 >= lv.width || j as u32 >= lv.height || k as u32 >= lv.depth {
            return None;
        }
        Some(self.fetch(lv, i as u32, j as u32, k as u32))
    }

    /// `log2(rho)`: the level of detail for coordinate derivatives along x
    /// and y (normalized coordinates).
    fn lambda(&self, dx: [f32; 3], dy: [f32; 3]) -> f32 {
        let b = &self.levels[0];
        let (w, h) = (b.width as f32, b.height as f32);
        let d = if self.dim == Dim::D3 { b.depth as f32 } else { 0.0 };
        let px = (dx[0] * w) * (dx[0] * w) + (dx[1] * h) * (dx[1] * h) + (dx[2] * d) * (dx[2] * d);
        let py = (dy[0] * w) * (dy[0] * w) + (dy[1] * h) * (dy[1] * h) + (dy[2] * d) * (dy[2] * d);
        // log2(sqrt(x)) = log2(x) / 2.
        0.5 * m::log2(px.max(py))
    }

    /// Applies the swizzle to a result.
    #[inline(always)]
    fn swizzled(&self, v: Texel4) -> [u32; 4] {
        let one = if matches!(self.kind, Kind::Int | Kind::Uint) { 1u32 } else { 1.0f32.to_bits() };
        self.swizzle.map(|s| match s {
            0..=3 => v[s as usize].to_bits(),
            4 => 0,
            _ => one,
        })
    }
}

#[inline(always)]
fn lerp4(a: Texel4, b: Texel4, t: f32) -> Texel4 {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t]
}

#[inline(always)]
fn bilerp(t00: Texel4, t10: Texel4, t01: Texel4, t11: Texel4, a: f32, b: f32) -> Texel4 {
    lerp4(lerp4(t00, t10, a), lerp4(t01, t11, a), b)
}

/// Interpolates two RGBA8 texels with an 8-bit weight (`f` in 0..=256), two
/// channels at a time in each word.
#[inline(always)]
fn lerp_u8(a: u32, b: u32, f: u32) -> u32 {
    let g = 256 - f;
    let rb = ((a & 0x00FF_00FF) * g + (b & 0x00FF_00FF) * f + 0x0080_0080) >> 8;
    let ga = ((a >> 8) & 0x00FF_00FF) * g + ((b >> 8) & 0x00FF_00FF) * f + 0x0080_0080;
    (rb & 0x00FF_00FF) | (ga & 0xFF00_FF00)
}

/// Bilinear interpolation of four RGBA8 texels with 8-bit weights.
#[inline(always)]
fn bilerp_u8(t00: u32, t10: u32, t01: u32, t11: u32, fu: u32, fv: u32) -> u32 {
    lerp_u8(lerp_u8(t00, t10, fu), lerp_u8(t01, t11, fu), fv)
}

/// A weight in [0, 1] as 0..=256.
#[inline(always)]
fn weight(f: f32) -> u32 {
    (f * 256.0 + 0.5) as u32
}

/// The textures one draw's shaders sample, by sampler index (`None`:
/// incomplete or unsuitable, sampled as (0, 0, 0, 1)).
pub struct DrawTextures<'a> {
    pub bound: &'a [Option<Sampled>],
}

/// Coordinates of a cube lookup as face coordinates on lane `p`'s face
/// (for derivatives within a quad).
fn cube_on_face(face: u32, x: f32, y: f32, z: f32) -> (f32, f32) {
    let (sc, tc, ma) = match face {
        0 => (-z, -y, x),
        1 => (z, -y, -x),
        2 => (x, z, y),
        3 => (x, -z, -y),
        4 => (x, -y, z),
        _ => (-x, -y, -z),
    };
    let inv = if ma.abs() > 0.0 { 1.0 / ma.abs() } else { 0.0 };
    ((sc * inv + 1.0) * 0.5, (tc * inv + 1.0) * 0.5)
}

impl Textures for DrawTextures<'_> {
    fn sample(&self, op: &TexOp, index: &[u32; LANES], args: &[Lanes], mask: Mask, out: &mut [Lanes]) {
        if let Some(t) = self.fast_2d(op, index) {
            return self.sample_2d_u8(t, op, args, mask, out);
        }
        let nc = op.coords as usize;
        let shadow = op.sampler.shadow;
        let fetch = op.lod == TexLod::Fetch;
        // The coordinates proper (without layer or reference).
        let spatial = match op.sampler.dim {
            Dim::D2 | Dim::D2Array => 2,
            Dim::D3 | Dim::Cube => 3,
        };
        let coord = |c: usize, lane: usize| -> f32 { args.get(c).map_or(0.0, |a| a.f32(lane)) };
        // Per-quad derivatives for implicit lookups.
        let mut quad_lambda = [0.0f32; LANES / 4];
        let implicit = op.lod == TexLod::Implicit;
        if implicit {
            for (q, ql) in quad_lambda.iter_mut().enumerate() {
                let b = q * 4;
                if mask & (0xF << b) == 0 {
                    continue;
                }
                let Some(t) = self.bound.get(index[b] as usize).and_then(Option::as_ref) else { continue };
                let mut c = [[0.0f32; 3]; 4];
                for (k, cl) in c.iter_mut().enumerate() {
                    for (i, v) in cl.iter_mut().enumerate().take(spatial) {
                        *v = coord(i, b + k);
                    }
                }
                if op.sampler.dim == Dim::Cube {
                    let (face, _, _) = cube_face(c[0][0], c[0][1], c[0][2]);
                    for cl in &mut c {
                        let (s, t) = cube_on_face(face, cl[0], cl[1], cl[2]);
                        *cl = [s, t, 0.0];
                    }
                }
                let dx = [c[1][0] - c[0][0], c[1][1] - c[0][1], c[1][2] - c[0][2]];
                let dy = [c[2][0] - c[0][0], c[2][1] - c[0][1], c[2][2] - c[0][2]];
                *ql = t.lambda(dx, dy);
            }
        }
        // Where the level-of-detail argument is.
        let extra = nc;
        for lane in 0..LANES {
            if mask & (1 << lane) == 0 {
                continue;
            }
            let Some(t) = self.bound.get(index[lane] as usize).and_then(Option::as_ref) else {
                let one = if matches!(op.sampler.ty, vglsl::types::Scalar::Int | vglsl::types::Scalar::Uint) {
                    1
                } else {
                    1.0f32.to_bits()
                };
                let v = [0, 0, 0, one];
                for (c, o) in out.iter_mut().enumerate() {
                    o.0[lane] = v[c];
                }
                continue;
            };
            if fetch {
                let ci = |c: usize| args.get(c).map_or(0, |a| a.0[lane] as i32);
                let level = ci(extra);
                let v = t.texel_fetch([ci(0), ci(1), ci(2)], level, op.offset).unwrap_or([0.0; 4]);
                let s = t.swizzled(v);
                for (c, o) in out.iter_mut().enumerate() {
                    o.0[lane] = s[c];
                }
                continue;
            }
            let mut coords = [0.0f32; 4];
            for (c, v) in coords.iter_mut().enumerate().take(nc.min(4)) {
                *v = coord(c, lane);
            }
            let lambda = match op.lod {
                TexLod::Implicit => {
                    let bias = if op.bias { coord(extra, lane) } else { 0.0 };
                    quad_lambda[lane / 4] + bias
                }
                TexLod::Lod => coord(extra, lane),
                TexLod::Grad => {
                    let gs = op.grad_size();
                    let mut dx = [0.0; 3];
                    let mut dy = [0.0; 3];
                    for i in 0..gs {
                        dx[i] = coord(extra + i, lane);
                        dy[i] = coord(extra + gs + i, lane);
                    }
                    if op.sampler.dim == Dim::Cube {
                        // Gradients of the direction, projected onto the
                        // face it points at.
                        let (face, s0, t0) = cube_face(coords[0], coords[1], coords[2]);
                        let (sx, tx) = cube_on_face(face, coords[0] + dx[0], coords[1] + dx[1], coords[2] + dx[2]);
                        let (sy, ty) = cube_on_face(face, coords[0] + dy[0], coords[1] + dy[1], coords[2] + dy[2]);
                        t.lambda([sx - s0, tx - t0, 0.0], [sy - s0, ty - t0, 0.0])
                    } else {
                        t.lambda(dx, dy)
                    }
                }
                TexLod::Fetch => 0.0,
            };
            let choice = t.choose(lambda);
            let cmp = if shadow {
                let reference = coords[nc - 1];
                // Fixed-point depth formats compare against a clamped
                // reference.
                let r = if matches!(t.format.class(), Class::Depth | Class::DepthStencil)
                    && t.format != Format::D32Float
                    && t.format != Format::D32FloatS8Uint
                {
                    reference.clamp(0.0, 1.0)
                } else {
                    reference
                };
                Some((t.state.compare.unwrap_or(Func::LessEqual), r))
            } else {
                None
            };
            let v = t.lookup(coords, choice, op.offset, cmp);
            if shadow {
                out[0].0[lane] = v[0].to_bits();
            } else {
                let s = t.swizzled(v);
                for (c, o) in out.iter_mut().enumerate() {
                    o.0[lane] = s[c];
                }
            }
        }
    }

    fn size(&self, sampler: u32, lod: i32) -> [i32; 3] {
        let Some(t) = self.bound.get(sampler as usize).and_then(Option::as_ref) else { return [0; 3] };
        let Some(l) = usize::try_from(lod).ok().and_then(|l| t.levels.get(l)) else { return [0; 3] };
        match t.dim {
            Dim::D2 | Dim::Cube => [l.width as i32, l.height as i32, 0],
            Dim::D3 | Dim::D2Array => [l.width as i32, l.height as i32, l.depth as i32],
        }
    }
}
impl DrawTextures<'_> {
    /// The texture of a lookup the RGBA8 fast path can do: a 2D float
    /// lookup with an implicit or explicit level of a single, constant
    /// sampler, of an 8-bit unsigned normalized texture without comparison.
    #[inline]
    fn fast_2d(&self, op: &TexOp, index: &[u32; LANES]) -> Option<&Sampled> {
        if op.sampler.shadow
            || op.sampler.dim != Dim::D2
            || !matches!(op.lod, TexLod::Implicit | TexLod::Lod)
            || op.dynamic
        {
            return None;
        }
        let t = self.bound.get(index[0] as usize)?.as_ref()?;
        let ok = t.dim == Dim::D2
            && matches!(t.format, Format::Rgba8Unorm | Format::Rgbx8Unorm)
            && t.state.compare.is_none();
        ok.then_some(t)
    }

    /// The fast path: the four lanes of each quad share their level of
    /// detail (for implicit lookups), and texels are filtered as packed
    /// 8-bit values.
    fn sample_2d_u8(&self, t: &Sampled, op: &TexOp, args: &[Lanes], mask: Mask, out: &mut [Lanes]) {
        let (s, tc) = (&args[0], &args[1]);
        let extra = args.get(2);
        let identity = t.swizzle == [0, 1, 2, 3];
        let opaque = t.format == Format::Rgbx8Unorm;
        for q in 0..LANES / 4 {
            let qm = (mask >> (q * 4)) & 0xF;
            if qm == 0 {
                continue;
            }
            let b = q * 4;
            let quad = if op.lod == TexLod::Implicit {
                let dx = [s.f32(b + 1) - s.f32(b), tc.f32(b + 1) - tc.f32(b), 0.0];
                let dy = [s.f32(b + 2) - s.f32(b), tc.f32(b + 2) - tc.f32(b), 0.0];
                t.lambda(dx, dy)
            } else {
                0.0
            };
            for k in 0..4 {
                if qm & (1 << k) == 0 {
                    continue;
                }
                let lane = b + k;
                let lambda = match (op.lod, extra) {
                    (TexLod::Lod, Some(e)) => e.f32(lane),
                    (_, Some(e)) if op.bias => quad + e.f32(lane),
                    _ => quad,
                };
                let c = t.choose(lambda);
                let (u, v) = (s.f32(lane), tc.f32(lane));
                let mut texel = t.filter_u8(c.level, c.filter, u, v, op.offset);
                if let Some((l2, f)) = c.next {
                    texel = lerp_u8(texel, t.filter_u8(l2, c.filter, u, v, op.offset), weight(f));
                }
                let bytes = texel.to_le_bytes();
                let n = |x: u8| f32::from(x) * (1.0 / 255.0);
                let rgba = [n(bytes[0]), n(bytes[1]), n(bytes[2]), if opaque { 1.0 } else { n(bytes[3]) }];
                let bits = if identity { rgba.map(f32::to_bits) } else { t.swizzled(rgba) };
                for (c, o) in out.iter_mut().enumerate() {
                    o.0[lane] = bits[c];
                }
            }
        }
    }
}

impl Sampled {
    /// A packed RGBA8 texel.
    #[inline(always)]
    fn fetch_u32(&self, l: &TexLevel, i: u32, j: u32) -> u32 {
        debug_assert!(i < l.width && j < l.height);
        // SAFETY: the coordinates are wrapped into the level by the caller,
        // and the format's texels are 4 bytes.
        u32::from_le(unsafe { l.ptr.add(j as usize * l.row + i as usize * 4).cast::<u32>().read_unaligned() })
    }

    /// [`Sampled::filter2d`] for RGBA8 texels, packed.
    #[inline(always)]
    fn filter_u8(&self, l: usize, filter: Filter, s: f32, t: f32, off: [i8; 3]) -> u32 {
        let lv = &self.levels[l];
        let w = self.state.wrap;
        let (u, v) = (s * lv.width as f32, t * lv.height as f32);
        match filter {
            Filter::Nearest => {
                let i = wrap(ifloor(u).saturating_add(i32::from(off[0])), lv.width, w[0]);
                let j = wrap(ifloor(v).saturating_add(i32::from(off[1])), lv.height, w[1]);
                self.fetch_u32(lv, i, j)
            }
            Filter::Linear => {
                let (u, v) = (u - 0.5, v - 0.5);
                let (fu, fv) = (ifloor(u), ifloor(v));
                let (a, b) = (weight(u - fu as f32), weight(v - fv as f32));
                let i0 = fu.saturating_add(i32::from(off[0]));
                let j0 = fv.saturating_add(i32::from(off[1]));
                let (x0, x1) = (wrap(i0, lv.width, w[0]), wrap(i0.saturating_add(1), lv.width, w[0]));
                let (y0, y1) = (wrap(j0, lv.height, w[1]), wrap(j0.saturating_add(1), lv.height, w[1]));
                bilerp_u8(
                    self.fetch_u32(lv, x0, y0),
                    self.fetch_u32(lv, x1, y0),
                    self.fetch_u32(lv, x0, y1),
                    self.fetch_u32(lv, x1, y1),
                    a,
                    b,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vglsl::types::{Sampler, Scalar};

    fn rgba8(w: u32, h: u32, levels: u32) -> (Vec<u8>, Vec<TexLevel>) {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for l in 0..levels {
            let (lw, lh) = ((w >> l).max(1), (h >> l).max(1));
            offsets.push((data.len(), lw, lh));
            data.extend((0..lw * lh * 4).map(|i| (i * 7 % 251) as u8));
        }
        let levels = offsets
            .iter()
            .map(|&(o, lw, lh)| TexLevel {
                ptr: data.as_ptr().wrapping_add(o),
                width: lw,
                height: lh,
                depth: 1,
                row: lw as usize * 4,
                image: (lw * lh * 4) as usize,
            })
            .collect();
        (data, levels)
    }

    #[test]
    #[ignore]
    fn bench_sampling() {
        let (data, levels) = rgba8(256, 256, 9);
        let st = SamplerState {
            min: Filter::Linear,
            mag: Filter::Linear,
            mip: Some(Filter::Linear),
            wrap: [Wrap::Repeat; 3],
            min_lod: -1000.0,
            max_lod: 1000.0,
            compare: None,
            max_anisotropy: 1.0,
        };
        let s = Sampled {
            levels,
            dim: Dim::D2,
            format: Format::Rgba8Unorm,
            texel: 4,
            kind: Kind::Float,
            swizzle: [0, 1, 2, 3],
            state: st,
        };
        let bound = [Some(s)];
        let tex = DrawTextures { bound: &bound };
        let op = TexOp {
            sampler: Sampler::new(Dim::D2, false, Scalar::Float),
            index: 0,
            dynamic: false,
            count: 1,
            lod: TexLod::Implicit,
            bias: false,
            offset: [0; 3],
            coords: 2,
        };
        let mut out = [Lanes::ZERO; 4];
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            for k in 0..20_000u32 {
                // Quads 0.3 texels apart: level of detail about 0.6.
                let base = (k % 977) as f32 / 977.0;
                let mut u = [0.0f32; LANES];
                let mut v = [0.0f32; LANES];
                for lane in 0..LANES {
                    let (x, y) = ((lane & 1) + 2 * ((lane >> 2) & 1), ((lane >> 1) & 1) + 2 * (lane >> 3));
                    u[lane] = base + x as f32 * 0.3 / 256.0;
                    v[lane] = base * 0.5 + y as f32 * 0.3 / 256.0;
                }
                let args = [Lanes::from_f32(u), Lanes::from_f32(v)];
                tex.sample(&op, &[0; LANES], &args, vglsl::interp::ALL, &mut out);
            }
            best = best.min(t0.elapsed().as_secs_f64());
        }
        std::println!("trilinear RGBA8: {:.1} ns per lookup", best * 1e9 / (20_000.0 * LANES as f64));
        drop(data);
    }
}
