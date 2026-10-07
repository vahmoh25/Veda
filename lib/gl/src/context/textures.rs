//! Textures and samplers (OpenGL ES 3.0 section 3.8).
//!
//! Each image of a mutable texture can be specified separately with any
//! size and format, while a renderer stores a texture as one resource
//! holding a mipmap chain. Images that fit the texture's resource (its
//! *storage*) live there; images that do not get a resource of their own,
//! and when the texture is used with levels that are not all in its
//! storage, the storage is rebuilt from the base level and every image that
//! fits is copied in (as Mesa's state tracker does). Immutable textures
//! (`TexStorage*`) are one resource from the start.

use alloc::vec::Vec;

use super::{
    Attachment, Context, Image, Key, MAX_LEVELS, Sampler, SamplerParams, Storage, TEXTURE_UNITS, Texture, target_index,
};
use crate::backend::{Filter, Func, Region, ResourceDesc, ResourceId, SamplerState, Surface, Target, View, Wrap};
use crate::etc;
use crate::format::{self, Class, Client, ComponentType, FormatError, Internal};
use crate::gl;
use crate::pixels::{self, Pixels};

/// Which faces a texture image target names: the texture target and the
/// face (cube maps) or 0.
fn image_target(target: u32) -> Option<(u32, usize)> {
    Some(match target {
        gl::TEXTURE_2D => (gl::TEXTURE_2D, 0),
        gl::TEXTURE_3D => (gl::TEXTURE_3D, 0),
        gl::TEXTURE_2D_ARRAY => (gl::TEXTURE_2D_ARRAY, 0),
        gl::TEXTURE_CUBE_MAP_POSITIVE_X..=gl::TEXTURE_CUBE_MAP_NEGATIVE_Z => {
            (gl::TEXTURE_CUBE_MAP, (target - gl::TEXTURE_CUBE_MAP_POSITIVE_X) as usize)
        }
        _ => return None,
    })
}

/// The renderer's resource kind for a texture target.
fn resource_target(target: u32) -> Target {
    match target {
        gl::TEXTURE_3D => Target::Texture3D,
        gl::TEXTURE_CUBE_MAP => Target::TextureCube,
        gl::TEXTURE_2D_ARRAY => Target::Texture2DArray,
        _ => Target::Texture2D,
    }
}

/// `floor(log2(x)) + 1`: the levels of a full mipmap chain.
pub(crate) fn chain_levels(max_dim: u32) -> u32 {
    32 - max_dim.max(1).leading_zeros()
}

/// A dimension at `level` below `size`.
fn minify(size: u32, level: u32) -> u32 {
    (size >> level.min(31)).max(1)
}

fn filter_valid(f: u32, min: bool) -> bool {
    match f {
        gl::NEAREST | gl::LINEAR => true,
        gl::NEAREST_MIPMAP_NEAREST
        | gl::LINEAR_MIPMAP_NEAREST
        | gl::NEAREST_MIPMAP_LINEAR
        | gl::LINEAR_MIPMAP_LINEAR => min,
        _ => false,
    }
}

fn mipmapped(min_filter: u32) -> bool {
    !matches!(min_filter, gl::NEAREST | gl::LINEAR)
}

fn wrap(w: u32) -> Wrap {
    match w {
        gl::CLAMP_TO_EDGE => Wrap::ClampToEdge,
        gl::MIRRORED_REPEAT => Wrap::MirroredRepeat,
        _ => Wrap::Repeat,
    }
}

/// The backend's sampler state for GL sampler parameters.
pub(crate) fn sampler_state(p: &SamplerParams) -> SamplerState {
    let (min, mip) = match p.min_filter {
        gl::NEAREST => (Filter::Nearest, None),
        gl::LINEAR => (Filter::Linear, None),
        gl::NEAREST_MIPMAP_NEAREST => (Filter::Nearest, Some(Filter::Nearest)),
        gl::LINEAR_MIPMAP_NEAREST => (Filter::Linear, Some(Filter::Nearest)),
        gl::NEAREST_MIPMAP_LINEAR => (Filter::Nearest, Some(Filter::Linear)),
        _ => (Filter::Linear, Some(Filter::Linear)),
    };
    SamplerState {
        min,
        mag: if p.mag_filter == gl::NEAREST { Filter::Nearest } else { Filter::Linear },
        mip,
        wrap: p.wrap.map(wrap),
        min_lod: p.min_lod,
        max_lod: p.max_lod,
        compare: (p.compare_mode == gl::COMPARE_REF_TO_TEXTURE)
            .then(|| super::compare_func(p.compare_func).unwrap_or(Func::LessEqual)),
        max_anisotropy: p.max_anisotropy,
    }
}

/// How a parameter value was given.
#[derive(Clone, Copy)]
enum Param {
    I(i32),
    F(f32),
}

impl Param {
    fn int(self) -> i32 {
        match self {
            Param::I(i) => i,
            // Enums and integers given as floats.
            Param::F(f) => vmath::f32::round(f) as i32,
        }
    }

    fn float(self) -> f32 {
        match self {
            Param::I(i) => i as f32,
            Param::F(f) => f,
        }
    }

    fn enum_(self) -> u32 {
        match self {
            Param::I(i) => i as u32,
            Param::F(f) => {
                if f >= 0.0 && f == vmath::f32::round(f) && f < 4_294_967_296.0 {
                    f as u32
                } else {
                    u32::MAX
                }
            }
        }
    }
}

/// The unsized internal formats (table 3.3, and depth for
/// `OES_depth_texture`).
fn is_unsized(internalformat: u32) -> bool {
    matches!(
        internalformat,
        gl::RGBA | gl::RGB | gl::LUMINANCE_ALPHA | gl::LUMINANCE | gl::ALPHA | gl::DEPTH_COMPONENT | gl::DEPTH_STENCIL
    )
}

/// Components of a base internal format: red, green, blue, alpha,
/// luminance.
fn base_components(base: u32) -> [bool; 5] {
    match base {
        gl::RED => [true, false, false, false, false],
        gl::RG => [true, true, false, false, false],
        gl::RGB => [true, true, true, false, false],
        gl::RGBA => [true, true, true, true, false],
        gl::ALPHA => [false, false, false, true, false],
        gl::LUMINANCE => [false, false, false, false, true],
        gl::LUMINANCE_ALPHA => [false, false, false, true, true],
        _ => [false; 5],
    }
}

/// The internal format `CopyTexImage2D` creates from a source buffer of
/// format `src` (section 3.8.5), or the error.
fn copy_format(src: &Internal, internalformat: u32) -> Result<Internal, u32> {
    let src_class = src.format.class();
    if matches!(src_class, Class::Float | Class::Depth | Class::DepthStencil | Class::Stencil) {
        return Err(gl::INVALID_OPERATION);
    }
    let srgb = src.format.is_srgb();
    let candidates: Vec<Internal> =
        if matches!(internalformat, gl::RGBA | gl::RGB | gl::LUMINANCE_ALPHA | gl::LUMINANCE | gl::ALPHA) {
            // Unsized: the effective formats table 3.12 gives each.
            if src.component != ComponentType::UnsignedNormalized || srgb {
                return Err(gl::INVALID_OPERATION);
            }
            let options: &[(u32, u32)] = match internalformat {
                gl::RGBA => &[
                    (gl::RGBA, gl::UNSIGNED_BYTE),
                    (gl::RGBA, gl::UNSIGNED_SHORT_4_4_4_4),
                    (gl::RGBA, gl::UNSIGNED_SHORT_5_5_5_1),
                ],
                gl::RGB => &[(gl::RGB, gl::UNSIGNED_BYTE), (gl::RGB, gl::UNSIGNED_SHORT_5_6_5)],
                f => &[(f, gl::UNSIGNED_BYTE)],
            };
            options.iter().filter_map(|&(f, t)| format::tex_format(internalformat, f, t).ok()).collect()
        } else {
            let Some(dst) = format::sized(internalformat) else {
                return Err(if format::is_format(internalformat) { gl::INVALID_OPERATION } else { gl::INVALID_ENUM });
            };
            let compatible = matches!(
                (src.component, dst.component),
                (ComponentType::UnsignedNormalized, ComponentType::UnsignedNormalized)
                    | (ComponentType::Int, ComponentType::Int)
                    | (ComponentType::UnsignedInt, ComponentType::UnsignedInt)
            );
            if !compatible || dst.format.is_srgb() != srgb || dst.format.has_depth() || dst.format.has_stencil() {
                return Err(gl::INVALID_OPERATION);
            }
            alloc::vec![*dst]
        };
    // New components cannot be added (table 3.16); luminance takes red.
    let s = base_components(src.base);
    let ok_base = |dst: &Internal| {
        let d = base_components(dst.base);
        (!d[0] || s[0]) && (!d[1] || s[1]) && (!d[2] || s[2]) && (!d[3] || s[3]) && (!d[4] || s[0])
    };
    // Component sizes, red standing in for luminance.
    let sizes = |f: &Internal| {
        let d = base_components(f.base);
        let lum = if d[4] { f.bits[0].max(8) } else { 0 };
        [if d[4] { lum } else { f.bits[0] }, f.bits[1], f.bits[2], f.bits[3]]
    };
    let present = |f: &Internal| {
        let d = base_components(f.base);
        [d[0] || d[4], d[1], d[2], d[3]]
    };
    let src_sizes = sizes(src);
    let cmp = |f: &Internal, rule: u8| {
        let (fs, p) = (sizes(f), present(f));
        (0..4).filter(|&i| p[i]).all(|i| match rule {
            0 => fs[i] == src_sizes[i],
            1 => fs[i] >= src_sizes[i],
            _ => fs[i] <= src_sizes[i],
        })
    };
    let valid: Vec<&Internal> = candidates.iter().filter(|f| ok_base(f)).collect();
    if valid.is_empty() {
        return Err(gl::INVALID_OPERATION);
    }
    if !is_unsized(internalformat) {
        // Sized formats must match exactly.
        return if cmp(valid[0], 0) { Ok(*valid[0]) } else { Err(gl::INVALID_OPERATION) };
    }
    for rule in 0..3 {
        if let Some(f) = valid.iter().find(|f| cmp(f, rule)) {
            return Ok(**f);
        }
    }
    Err(gl::INVALID_OPERATION)
}

/// What texture completeness found: the levels to sample and where they
/// are.
pub(crate) struct Complete {
    pub base: u32,
    pub last: u32,
}

impl Context {
    // ---- Names and binding ---------------------------------------------------

    /// `glGenTextures`.
    pub fn gen_textures(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.textures.generate();
        }
    }

    /// One texture name.
    pub fn gen_texture(&mut self) -> u32 {
        self.textures.generate()
    }

    /// `glIsTexture`.
    pub fn is_texture(&self, name: u32) -> bool {
        name != 0 && self.textures.key(name).is_some()
    }

    /// `glActiveTexture`.
    pub fn active_texture(&mut self, texture: u32) {
        match texture.checked_sub(gl::TEXTURE0) {
            Some(i) if (i as usize) < TEXTURE_UNITS => self.active_texture = i as usize,
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glBindTexture`.
    pub fn bind_texture(&mut self, target: u32, name: u32) {
        let Some(t) = target_index(target) else { return self.err(gl::INVALID_ENUM) };
        let key = if name == 0 {
            self.default_textures[t]
        } else {
            match self.textures.key(name) {
                Some(k) if self.textures.get(k).target != target => return self.err(gl::INVALID_OPERATION),
                Some(k) => k,
                None => self.textures.create(name, Texture::new(target)),
            }
        };
        self.texture_units[self.active_texture][t] = key;
    }

    /// `glDeleteTextures`.
    pub fn delete_textures(&mut self, names: &[u32]) {
        for &name in names {
            if name == 0 {
                continue;
            }
            let Some(key) = self.textures.key(name) else {
                let _ = self.textures.delete(name);
                continue;
            };
            let defaults = self.default_textures;
            for unit in &mut self.texture_units {
                for (t, bound) in unit.iter_mut().enumerate() {
                    if *bound == key {
                        *bound = defaults[t];
                    }
                }
            }
            self.detach_from_bound_framebuffers(|a| matches!(a, Attachment::Texture { key: k, .. } if k == key));
            if let Some((_, Some(dead))) = self.textures.delete(name) {
                self.destroy_texture(dead);
            }
        }
    }

    pub(crate) fn destroy_texture(&mut self, t: Texture) {
        for face in &t.images {
            for img in face.iter().flatten() {
                if let Some(r) = img.own {
                    self.backend.destroy_resource(r);
                }
            }
        }
        if let Some(s) = t.storage {
            self.backend.destroy_resource(s.resource);
        }
    }

    pub(crate) fn release_texture(&mut self, key: Key) {
        if let Some(t) = self.textures.release(key) {
            self.destroy_texture(t);
        }
    }

    /// The texture bound to `target` on the active unit.
    pub(crate) fn bound_texture(&self, target: u32) -> Option<Key> {
        target_index(target).map(|t| self.texture_units[self.active_texture][t])
    }

    // ---- Storage ----------------------------------------------------------------

    /// The dimensions level `level` of `s` has.
    fn storage_size(s: &Storage, target: u32, level: u32) -> Option<(u32, u32, u32)> {
        let l = level.checked_sub(s.base)?;
        if l >= s.desc.levels {
            return None;
        }
        let d = match target {
            gl::TEXTURE_3D => minify(s.desc.depth, l),
            gl::TEXTURE_2D_ARRAY => s.desc.depth,
            _ => 1,
        };
        Some((minify(s.desc.width, l), minify(s.desc.height, l), d))
    }

    /// Whether `img` at `level` can live in the storage `s`.
    fn fits(s: &Storage, target: u32, img: &Image, level: u32) -> bool {
        s.desc.format == img.internal.format
            && Self::storage_size(s, target, level) == Some((img.width, img.height, img.depth))
    }

    /// Where the texels of an image are: resource, level and first layer.
    pub(crate) fn image_location(&self, tex: &Texture, face: usize, level: u32) -> Option<(ResourceId, u32, u32)> {
        let img = tex.image(face, level)?;
        if let Some(r) = img.own {
            return Some((r, 0, 0));
        }
        let s = tex.storage?;
        let layer = if tex.target == gl::TEXTURE_CUBE_MAP { face as u32 } else { 0 };
        Some((s.resource, level - s.base, layer))
    }

    /// A resource for an image of its own.
    fn create_image_resource(&mut self, target: u32, img: &Image) -> Option<ResourceId> {
        let target = match target {
            gl::TEXTURE_CUBE_MAP => Target::Texture2D,
            t => resource_target(t),
        };
        let desc = ResourceDesc {
            target,
            format: img.internal.format,
            width: img.width,
            height: img.height,
            depth: img.depth,
            levels: 1,
            samples: 0,
        };
        self.backend.create_resource(&desc).ok()
    }

    /// Creates storage for a texture from an image at `level`, with the
    /// whole mipmap chain below it if `chain`.
    fn create_storage(&mut self, target: u32, img: &Image, level: u32, chain: bool) -> Option<Storage> {
        let depth = match target {
            gl::TEXTURE_CUBE_MAP => 6,
            _ => img.depth,
        };
        let max_dim =
            if target == gl::TEXTURE_3D { img.width.max(img.height).max(img.depth) } else { img.width.max(img.height) };
        let desc = ResourceDesc {
            target: resource_target(target),
            format: img.internal.format,
            width: img.width,
            height: img.height,
            depth,
            levels: if chain { chain_levels(max_dim).min(MAX_LEVELS as u32 - level) } else { 1 },
            samples: 0,
        };
        let resource = self.backend.create_resource(&desc).ok()?;
        Some(Storage { resource, desc, base: level })
    }

    /// Defines image `(face, level)` of a texture, with its texels
    /// (tightly packed) if given; returns `false` if memory ran out.
    pub(crate) fn define_image(
        &mut self,
        key: Key,
        face: usize,
        level: u32,
        mut img: Image,
        data: Option<&[u8]>,
    ) -> bool {
        let tex = self.textures.get(key);
        let target = tex.target;
        let old_own = tex.image(face, level).and_then(|i| i.own);
        let chain = mipmapped(tex.sampler.min_filter);
        let mut storage = tex.storage;
        if storage.is_none() && img.width > 0 && img.height > 0 && img.depth > 0 {
            storage = self.create_storage(target, &img, level, chain);
            if storage.is_none() {
                return false;
            }
            self.textures.get_mut(key).storage = storage;
        }
        let (resource, l, layer) = match storage {
            Some(s) if Self::fits(&s, target, &img, level) => {
                img.own = None;
                let layer = if target == gl::TEXTURE_CUBE_MAP { face as u32 } else { 0 };
                (Some(s.resource), level - s.base, layer)
            }
            _ if img.width == 0 || img.height == 0 || img.depth == 0 => {
                img.own = None;
                (None, 0, 0)
            }
            _ => {
                let Some(r) = self.create_image_resource(target, &img) else { return false };
                img.own = Some(r);
                (Some(r), 0, 0)
            }
        };
        if let Some(r) = old_own {
            self.backend.destroy_resource(r);
        }
        if let Some(r) = resource {
            let region = Region::new(0, 0, layer, img.width, img.height, img.depth);
            let row = img.width as usize * img.internal.format.bytes();
            let image = row * img.height as usize;
            match data {
                Some(d) => self.backend.write(r, l, region, d, row, image),
                None => {
                    // Undefined contents are zero, never another
                    // application's old data.
                    let Some(zero) = pixels::try_zeroed(image * img.depth as usize) else { return false };
                    self.backend.write(r, l, region, &zero, row, image);
                }
            }
        }
        self.textures.get_mut(key).images[face][level as usize] = Some(img);
        true
    }

    /// Makes the storage of a texture hold levels `base..=last` (of every
    /// face), rebuilding it if needed; returns the storage.
    pub(crate) fn finalize(&mut self, key: Key, base: u32, last: u32) -> Option<Storage> {
        let tex = self.textures.get(key);
        let faces = tex.images.len();
        let target = tex.target;
        let in_storage = |l: u32| (0..faces).all(|f| tex.image(f, l).is_some_and(|i| i.own.is_none()));
        if tex.storage.is_some() && (base..=last).all(in_storage) {
            return tex.storage;
        }
        let b = *tex.image(0, base)?;
        let new = self.create_storage(target, &b, base, true)?;
        let tex = self.textures.get(key);
        let old = tex.storage;
        let images: Vec<(usize, u32, Image)> = (0..faces)
            .flat_map(|f| (0..MAX_LEVELS as u32).map(move |l| (f, l)))
            .filter_map(|(f, l)| tex.image(f, l).map(|i| (f, l, *i)))
            .collect();
        for (f, l, mut img) in images {
            let (src, src_level, src_layer) = match img.own {
                Some(r) => (r, 0, 0),
                None => match old {
                    Some(o) => (o.resource, l - o.base, if target == gl::TEXTURE_CUBE_MAP { f as u32 } else { 0 }),
                    None => continue,
                },
            };
            let region = Region::new(0, 0, src_layer, img.width, img.height, img.depth);
            if Self::fits(&new, target, &img, l) {
                let layer = if target == gl::TEXTURE_CUBE_MAP { f as u32 } else { 0 };
                self.backend.copy_region(src, src_level, region, new.resource, l - new.base, 0, 0, layer);
                if let Some(r) = img.own.take() {
                    self.backend.destroy_resource(r);
                }
            } else if img.own.is_none() {
                // It leaves the old storage for a resource of its own.
                let Some(r) = self.create_image_resource(target, &img) else { continue };
                self.backend.copy_region(src, src_level, region, r, 0, 0, 0, 0);
                img.own = Some(r);
            }
            self.textures.get_mut(key).images[f][l as usize] = Some(img);
        }
        if let Some(o) = old {
            self.backend.destroy_resource(o.resource);
        }
        self.textures.get_mut(key).storage = Some(new);
        Some(new)
    }

    // ---- Completeness --------------------------------------------------------

    /// The effective base and maximum levels of a texture
    /// (section 3.8.10.4): `(base, q)` for mipmapped filtering.
    pub(crate) fn level_range(tex: &Texture) -> (u32, u32) {
        match tex.immutable_levels {
            Some(n) => {
                let base = tex.base_level.min(n - 1);
                let max = tex.max_level.clamp(base, n - 1);
                (base, max)
            }
            None => {
                let base = tex.base_level.min(MAX_LEVELS as u32 - 1);
                let Some(b) = tex.image(0, base) else { return (base, base) };
                let dim = if tex.target == gl::TEXTURE_3D {
                    b.width.max(b.height).max(b.depth)
                } else {
                    b.width.max(b.height)
                };
                let p = base + chain_levels(dim) - 1;
                (base, p.min(tex.max_level).min(MAX_LEVELS as u32 - 1).max(base))
            }
        }
    }

    /// Whether levels `base..=q` form a mipmap chain (and, for cube maps,
    /// whether the faces agree).
    pub(crate) fn mipmap_complete(tex: &Texture, base: u32, q: u32) -> bool {
        let Some(b) = tex.image(0, base) else { return false };
        for f in 0..tex.images.len() {
            for l in base..=q {
                let Some(i) = tex.image(f, l) else { return false };
                let k = l - base;
                let depth = if tex.target == gl::TEXTURE_3D { minify(b.depth, k) } else { b.depth };
                if i.internal != b.internal
                    || (i.width, i.height, i.depth) != (minify(b.width, k), minify(b.height, k), depth)
                {
                    return false;
                }
            }
        }
        true
    }

    /// Cube completeness: the faces' base images are square and agree.
    fn cube_complete(tex: &Texture, base: u32) -> bool {
        let Some(b) = tex.image(0, base) else { return false };
        b.width == b.height
            && (0..6).all(|f| {
                tex.image(f, base)
                    .is_some_and(|i| i.internal == b.internal && i.width == b.width && i.height == b.height)
            })
    }

    /// Whether a texture is complete when sampled with `p` (section
    /// 3.8.13), and the levels it samples.
    pub(crate) fn completeness(&self, key: Key, p: &SamplerParams) -> Option<Complete> {
        let tex = self.textures.get(key);
        let (base, q) = Self::level_range(tex);
        if tex.immutable_levels.is_none() && tex.base_level > tex.max_level {
            return None;
        }
        let b = tex.image(0, base)?;
        if b.width == 0 || b.height == 0 || b.depth == 0 {
            return None;
        }
        let mip = mipmapped(p.min_filter);
        let last = if mip { q } else { base };
        if tex.target == gl::TEXTURE_CUBE_MAP && !Self::cube_complete(tex, base) {
            return None;
        }
        if mip && !Self::mipmap_complete(tex, base, last) {
            return None;
        }
        if tex.target != gl::TEXTURE_CUBE_MAP && !Self::mipmap_complete(tex, base, base) {
            return None;
        }
        let linear = p.mag_filter != gl::NEAREST || !matches!(p.min_filter, gl::NEAREST | gl::NEAREST_MIPMAP_NEAREST);
        let i = b.internal;
        let class = i.format.class();
        if linear {
            // Integer textures, depth textures without comparison, and
            // float textures the renderer cannot filter need NEAREST.
            if i.format.is_integer() {
                return None;
            }
            // Depth textures filter only their comparisons (percentage
            // closer filtering).
            if matches!(class, Class::Depth | Class::DepthStencil) {
                if p.compare_mode == gl::NONE {
                    return None;
                }
            } else if !i.filterable && !(class == Class::Float && self.backend.caps().float_linear) {
                return None;
            }
        }
        Some(Complete { base, last })
    }

    /// The view a sampler uses for the texture `key` with parameters `p`,
    /// or `None` if the texture is incomplete.
    pub(crate) fn texture_view(&mut self, key: Key, p: &SamplerParams) -> Option<View> {
        let c = self.completeness(key, p)?;
        let tex = self.textures.get(key);
        let target = resource_target(tex.target);
        let swizzle = tex.swizzle.map(|s| match s {
            gl::RED => 0,
            gl::GREEN => 1,
            gl::BLUE => 2,
            gl::ALPHA => 3,
            gl::ZERO => 4,
            _ => 5,
        });
        // A single level in a resource of its own is used as it is.
        if c.base == c.last
            && tex.target != gl::TEXTURE_CUBE_MAP
            && let Some(r) = tex.image(0, c.base).and_then(|i| i.own)
        {
            return Some(View { resource: r, target, base_level: 0, max_level: 0, swizzle });
        }
        let s = self.finalize(key, c.base, c.last)?;
        Some(View { resource: s.resource, target, base_level: c.base - s.base, max_level: c.last - s.base, swizzle })
    }

    // ---- Image specification ------------------------------------------------

    /// Checks a level and size for a texture image target; records the
    /// error and returns `false` if they are out of range.
    fn check_size(&mut self, tex_target: u32, level: i32, w: i32, h: i32, d: i32) -> bool {
        let caps = self.backend.caps();
        let (max_wh, max_d) = match tex_target {
            gl::TEXTURE_3D => (caps.max_3d_texture_size, caps.max_3d_texture_size),
            gl::TEXTURE_2D_ARRAY => (caps.max_texture_size, caps.max_array_layers),
            gl::TEXTURE_CUBE_MAP => (caps.max_cube_map_size, 1),
            _ => (caps.max_texture_size, 1),
        };
        let max_level = chain_levels(max_wh) as i32 - 1;
        if level < 0 || level > max_level || w < 0 || h < 0 || d < 0 {
            self.err(gl::INVALID_VALUE);
            return false;
        }
        let lim = (max_wh >> level).max(1) as i32;
        let dlim = if tex_target == gl::TEXTURE_3D { (max_d >> level).max(1) as i32 } else { max_d as i32 };
        if w > lim || h > lim || d > dlim || (tex_target == gl::TEXTURE_CUBE_MAP && w != h) {
            self.err(gl::INVALID_VALUE);
            return false;
        }
        true
    }

    /// The bytes of a pixel transfer source: the application's slice or
    /// a range of the unpack buffer. Checks the unpack rules (section
    /// 3.7.1) and records errors.
    fn unpack_source(
        &mut self,
        pixels: Pixels<'_>,
        layout: &pixels::Layout,
        type_bytes: usize,
    ) -> Result<Option<Vec<u8>>, ()> {
        let buffer = self.bound.pixel_unpack;
        match (pixels, buffer) {
            (Pixels::Data(d), None) => {
                if d.len() < layout.end {
                    self.err(gl::INVALID_OPERATION);
                    return Err(());
                }
                // Borrowed data is copied only as far as needed.
                Ok(Some(d[..layout.end].to_vec()))
            }
            (Pixels::None, None) => Ok(None),
            (Pixels::Offset(_), None) | (Pixels::Data(_), Some(_)) => {
                self.err(gl::INVALID_OPERATION);
                Err(())
            }
            (p, Some(k)) => {
                let offset = if let Pixels::Offset(o) = p { o } else { 0 };
                let (_, size) = self.buffer_store(k);
                let mapped = self.buffers.get(k).map.is_some();
                let end = offset.checked_add(layout.end);
                if mapped || end.is_none_or(|e| e > size) || !offset.is_multiple_of(type_bytes.max(1)) {
                    self.err(gl::INVALID_OPERATION);
                    return Err(());
                }
                match self.read_buffer_range(k, offset, layout.end) {
                    Some(v) => Ok(Some(v)),
                    None => {
                        self.err(gl::OUT_OF_MEMORY);
                        Err(())
                    }
                }
            }
        }
    }

    /// Converts unpacked application pixels to an image's texels.
    fn texels(
        &mut self,
        client: Client,
        src: &[u8],
        layout: &pixels::Layout,
        w: u32,
        h: u32,
        d: u32,
        i: &Internal,
    ) -> Option<Vec<u8>> {
        let t = pixels::unpack(client, src, layout, w, h, d, i.format);
        if t.is_none() {
            self.err(gl::OUT_OF_MEMORY);
        }
        t
    }

    /// `glTexImage2D`.
    pub fn tex_image_2d<'a>(
        &mut self,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        ty: u32,
        pixels: impl Into<Pixels<'a>>,
    ) {
        let pixels = pixels.into();
        let Some((tex_target, face)) =
            image_target(target).filter(|t| matches!(t.0, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP))
        else {
            return self.err(gl::INVALID_ENUM);
        };
        self.tex_image(tex_target, face, level, internalformat, width, height, 1, border, format, ty, pixels);
    }

    /// `glTexImage3D`.
    pub fn tex_image_3d<'a>(
        &mut self,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        format: u32,
        ty: u32,
        pixels: impl Into<Pixels<'a>>,
    ) {
        let pixels = pixels.into();
        if !matches!(target, gl::TEXTURE_3D | gl::TEXTURE_2D_ARRAY) {
            return self.err(gl::INVALID_ENUM);
        }
        self.tex_image(target, 0, level, internalformat, width, height, depth, border, format, ty, pixels);
    }

    fn tex_image(
        &mut self,
        tex_target: u32,
        face: usize,
        level: i32,
        internalformat: u32,
        w: i32,
        h: i32,
        d: i32,
        border: i32,
        format: u32,
        ty: u32,
        pixels: Pixels<'_>,
    ) {
        if !format::is_type(ty) || !format::is_format(format) {
            return self.err(gl::INVALID_ENUM);
        }
        if !self.check_size(tex_target, level, w, h, d) {
            return;
        }
        if border != 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let internal = match format::tex_format(internalformat, format, ty) {
            Ok(i) => i,
            Err(FormatError::Enum) => return self.err(gl::INVALID_ENUM),
            Err(FormatError::Value) => return self.err(gl::INVALID_VALUE),
            Err(FormatError::Operation) => return self.err(gl::INVALID_OPERATION),
        };
        if tex_target == gl::TEXTURE_3D && (internal.format.has_depth() || internal.format.has_stencil()) {
            return self.err(gl::INVALID_OPERATION);
        }
        let key = self.bound_texture(tex_target).unwrap();
        if self.textures.get(key).immutable_levels.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        let client = Client::new(format, ty);
        let unpack = self.state.unpack;
        let Some(layout) = pixels::layout(client, w as u32, h as u32, d as u32, &unpack) else {
            return self.err(gl::INVALID_VALUE);
        };
        let Ok(src) = self.unpack_source(pixels, &layout, client.type_bytes()) else { return };
        let (w, h, d) = (w as u32, h as u32, d as u32);
        let texels = match src {
            Some(s) => match self.texels(client, &s, &layout, w, h, d, &internal) {
                Some(t) => Some(t),
                None => return,
            },
            None => None,
        };
        let img = Image {
            internal,
            compressed: None,
            unsized_format: is_unsized(internalformat),
            width: w,
            height: h,
            depth: d,
            own: None,
        };
        if !self.define_image(key, face, level as u32, img, texels.as_deref()) {
            self.err(gl::OUT_OF_MEMORY);
        }
    }

    /// `glTexSubImage2D`.
    pub fn tex_sub_image_2d<'a>(
        &mut self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        pixels: impl Into<Pixels<'a>>,
    ) {
        let pixels = pixels.into();
        let Some((tex_target, face)) =
            image_target(target).filter(|t| matches!(t.0, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP))
        else {
            return self.err(gl::INVALID_ENUM);
        };
        self.tex_sub_image(tex_target, face, level, [xoffset, yoffset, 0], [width, height, 1], format, ty, pixels);
    }

    /// `glTexSubImage3D`.
    pub fn tex_sub_image_3d<'a>(
        &mut self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        ty: u32,
        pixels: impl Into<Pixels<'a>>,
    ) {
        let pixels = pixels.into();
        if !matches!(target, gl::TEXTURE_3D | gl::TEXTURE_2D_ARRAY) {
            return self.err(gl::INVALID_ENUM);
        }
        self.tex_sub_image(target, 0, level, [xoffset, yoffset, zoffset], [width, height, depth], format, ty, pixels);
    }

    /// The image a `*SubImage*` call updates, with the checks of its level
    /// and box; `None` after an error.
    fn sub_image(
        &mut self,
        tex_target: u32,
        face: usize,
        level: i32,
        off: [i32; 3],
        size: [i32; 3],
    ) -> Option<(Key, Image)> {
        if !self.check_size(tex_target, level, 0, 0, 0) {
            return None;
        }
        if size.iter().any(|&s| s < 0) || off.iter().any(|&o| o < 0) {
            self.err(gl::INVALID_VALUE);
            return None;
        }
        let key = self.bound_texture(tex_target).unwrap();
        let Some(img) = self.textures.get(key).image(face, level as u32).copied() else {
            self.err(gl::INVALID_OPERATION);
            return None;
        };
        let dims = [img.width, img.height, img.depth];
        if (0..3).any(|i| off[i] as i64 + size[i] as i64 > dims[i] as i64) {
            self.err(gl::INVALID_VALUE);
            return None;
        }
        Some((key, img))
    }

    #[allow(clippy::too_many_arguments)]
    fn tex_sub_image(
        &mut self,
        tex_target: u32,
        face: usize,
        level: i32,
        off: [i32; 3],
        size: [i32; 3],
        format: u32,
        ty: u32,
        pixels: Pixels<'_>,
    ) {
        if !format::is_type(ty) || !format::is_format(format) {
            return self.err(gl::INVALID_ENUM);
        }
        let Some((key, img)) = self.sub_image(tex_target, face, level, off, size) else { return };
        if img.compressed.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        // The data must be valid for the image's internal format.
        let valid = format::tex_format(img.internal.gl, format, ty).is_ok_and(|i| i.format == img.internal.format)
            || (img.unsized_format && format::tex_format(img.internal.base, format, ty).is_ok());
        if !valid {
            return self.err(gl::INVALID_OPERATION);
        }
        let client = Client::new(format, ty);
        let unpack = self.state.unpack;
        let [w, h, d] = size.map(|x| x as u32);
        let Some(layout) = pixels::layout(client, w, h, d, &unpack) else { return self.err(gl::INVALID_VALUE) };
        let Ok(src) = self.unpack_source(pixels, &layout, client.type_bytes()) else { return };
        let Some(src) = src else { return };
        if w == 0 || h == 0 || d == 0 {
            return;
        }
        let Some(texels) = self.texels(client, &src, &layout, w, h, d, &img.internal) else { return };
        let tex = self.textures.get(key);
        let Some((r, l, layer)) = self.image_location(tex, face, level as u32) else { return };
        let row = w as usize * img.internal.format.bytes();
        let region = Region::new(off[0] as u32, off[1] as u32, layer + off[2] as u32, w, h, d);
        self.backend.write(r, l, region, &texels, row, row * h as usize);
    }

    // ---- Compressed images ----------------------------------------------------

    /// `glCompressedTexImage2D` (`image_size` is the data's size, which an
    /// offset into the unpack buffer needs).
    pub fn compressed_tex_image_2d<'a>(
        &mut self,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        border: i32,
        image_size: usize,
        data: impl Into<Pixels<'a>>,
    ) {
        let data = data.into();
        let Some((tex_target, face)) =
            image_target(target).filter(|t| matches!(t.0, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP))
        else {
            return self.err(gl::INVALID_ENUM);
        };
        self.compressed_image(tex_target, face, level, internalformat, [width, height, 1], border, image_size, data);
    }

    /// `glCompressedTexImage3D`.
    pub fn compressed_tex_image_3d<'a>(
        &mut self,
        target: u32,
        level: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        depth: i32,
        border: i32,
        image_size: usize,
        data: impl Into<Pixels<'a>>,
    ) {
        let data = data.into();
        if !matches!(target, gl::TEXTURE_3D | gl::TEXTURE_2D_ARRAY) {
            return self.err(gl::INVALID_ENUM);
        }
        // ETC2/EAC images are two-dimensional: arrays of them, not volumes.
        if target == gl::TEXTURE_3D && etc::compressed(internalformat).is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        self.compressed_image(target, 0, level, internalformat, [width, height, depth], border, image_size, data);
    }

    /// The compressed bytes a call passes, checked against `image_size`.
    fn compressed_source(&mut self, data: Pixels<'_>, image_size: usize, expected: usize) -> Option<Vec<u8>> {
        if image_size != expected {
            self.err(gl::INVALID_VALUE);
            return None;
        }
        let layout = pixels::Layout { start: 0, row: 0, image: 0, pixel: 1, row_bytes: expected, end: expected };
        let data = match data {
            Pixels::Data(d) if d.len() != image_size => {
                self.err(gl::INVALID_VALUE);
                return None;
            }
            Pixels::None if self.bound.pixel_unpack.is_none() => {
                self.err(gl::INVALID_VALUE);
                return None;
            }
            d => d,
        };
        self.unpack_source(data, &layout, 1).ok().flatten()
    }

    #[allow(clippy::too_many_arguments)]
    fn compressed_image(
        &mut self,
        tex_target: u32,
        face: usize,
        level: i32,
        internalformat: u32,
        size: [i32; 3],
        border: i32,
        image_size: usize,
        data: Pixels<'_>,
    ) {
        let Some(c) = etc::compressed(internalformat) else { return self.err(gl::INVALID_ENUM) };
        let [w, h, d] = size;
        if !self.check_size(tex_target, level, w, h, d) {
            return;
        }
        if border != 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let key = self.bound_texture(tex_target).unwrap();
        if self.textures.get(key).immutable_levels.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        let (w, h, d) = (w as u32, h as u32, d as u32);
        let Some(expected) = c.image_size(w, h, d) else { return self.err(gl::INVALID_VALUE) };
        let Some(src) = self.compressed_source(data, image_size, expected) else { return };
        let Some(texels) = c.decode(&src, w, h, d) else { return self.err(gl::OUT_OF_MEMORY) };
        let img = Image {
            internal: c.internal(),
            compressed: Some(internalformat),
            unsized_format: false,
            width: w,
            height: h,
            depth: d,
            own: None,
        };
        if !self.define_image(key, face, level as u32, img, Some(&texels)) {
            self.err(gl::OUT_OF_MEMORY);
        }
    }

    /// `glCompressedTexSubImage2D`.
    pub fn compressed_tex_sub_image_2d<'a>(
        &mut self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        image_size: usize,
        data: impl Into<Pixels<'a>>,
    ) {
        let data = data.into();
        let Some((tex_target, face)) =
            image_target(target).filter(|t| matches!(t.0, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP))
        else {
            return self.err(gl::INVALID_ENUM);
        };
        self.compressed_sub_image(
            tex_target,
            face,
            level,
            [xoffset, yoffset, 0],
            [width, height, 1],
            format,
            image_size,
            data,
        );
    }

    /// `glCompressedTexSubImage3D`.
    pub fn compressed_tex_sub_image_3d<'a>(
        &mut self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        width: i32,
        height: i32,
        depth: i32,
        format: u32,
        image_size: usize,
        data: impl Into<Pixels<'a>>,
    ) {
        let data = data.into();
        if !matches!(target, gl::TEXTURE_3D | gl::TEXTURE_2D_ARRAY) {
            return self.err(gl::INVALID_ENUM);
        }
        self.compressed_sub_image(
            target,
            0,
            level,
            [xoffset, yoffset, zoffset],
            [width, height, depth],
            format,
            image_size,
            data,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn compressed_sub_image(
        &mut self,
        tex_target: u32,
        face: usize,
        level: i32,
        off: [i32; 3],
        size: [i32; 3],
        format: u32,
        image_size: usize,
        data: Pixels<'_>,
    ) {
        let Some(c) = etc::compressed(format) else { return self.err(gl::INVALID_ENUM) };
        let Some((key, img)) = self.sub_image(tex_target, face, level, off, size) else { return };
        if img.compressed != Some(format) {
            return self.err(gl::INVALID_OPERATION);
        }
        // Whole blocks, except at the image's right and bottom edges.
        let [w, h, d] = size.map(|x| x as u32);
        let [x, y, z] = off.map(|x| x as u32);
        let aligned = |o: u32, s: u32, full: u32| o.is_multiple_of(4) && (s.is_multiple_of(4) || o + s == full);
        if !aligned(x, w, img.width) || !aligned(y, h, img.height) {
            return self.err(gl::INVALID_OPERATION);
        }
        let Some(expected) = c.image_size(w, h, d) else { return self.err(gl::INVALID_VALUE) };
        let Some(src) = self.compressed_source(data, image_size, expected) else { return };
        if w == 0 || h == 0 || d == 0 {
            return;
        }
        let Some(texels) = c.decode(&src, w, h, d) else { return self.err(gl::OUT_OF_MEMORY) };
        let tex = self.textures.get(key);
        let Some((r, l, layer)) = self.image_location(tex, face, level as u32) else { return };
        let row = w as usize * c.format.bytes();
        self.backend.write(r, l, Region::new(x, y, layer + z, w, h, d), &texels, row, row * h as usize);
    }

    // ---- Immutable storage -------------------------------------------------------

    /// `glTexStorage2D`.
    pub fn tex_storage_2d(&mut self, target: u32, levels: i32, internalformat: u32, width: i32, height: i32) {
        if !matches!(target, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP) {
            return self.err(gl::INVALID_ENUM);
        }
        self.tex_storage(target, levels, internalformat, width, height, 1);
    }

    /// `glTexStorage3D`.
    pub fn tex_storage_3d(
        &mut self,
        target: u32,
        levels: i32,
        internalformat: u32,
        width: i32,
        height: i32,
        depth: i32,
    ) {
        if !matches!(target, gl::TEXTURE_3D | gl::TEXTURE_2D_ARRAY) {
            return self.err(gl::INVALID_ENUM);
        }
        self.tex_storage(target, levels, internalformat, width, height, depth);
    }

    fn tex_storage(&mut self, target: u32, levels: i32, internalformat: u32, w: i32, h: i32, d: i32) {
        let (internal, compressed) = match (format::sized(internalformat), etc::compressed(internalformat)) {
            (Some(i), _) => (*i, None),
            (None, Some(c)) => (c.internal(), Some(internalformat)),
            _ => return self.err(gl::INVALID_ENUM),
        };
        if target == gl::TEXTURE_3D
            && (compressed.is_some() || internal.format.has_depth() || internal.format.has_stencil())
        {
            return self.err(gl::INVALID_OPERATION);
        }
        if w < 1 || h < 1 || d < 1 || levels < 1 {
            return self.err(gl::INVALID_VALUE);
        }
        if !self.check_size(target, 0, w, h, d) {
            return;
        }
        let key = self.bound_texture(target).unwrap();
        if self.textures.get(key).immutable_levels.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        let (w, h, d) = (w as u32, h as u32, d as u32);
        let max_dim = if target == gl::TEXTURE_3D { w.max(h).max(d) } else { w.max(h) };
        if levels as u32 > chain_levels(max_dim) {
            return self.err(gl::INVALID_OPERATION);
        }
        let levels = levels as u32;
        let desc = ResourceDesc {
            target: resource_target(target),
            format: internal.format,
            width: w,
            height: h,
            depth: if target == gl::TEXTURE_CUBE_MAP { 6 } else { d },
            levels,
            samples: 0,
        };
        let Ok(resource) = self.backend.create_resource(&desc) else { return self.err(gl::OUT_OF_MEMORY) };
        // Zero every level: the contents are undefined, never stale.
        for l in 0..levels {
            let lw = minify(w, l);
            let lh = minify(h, l);
            let ld = match target {
                gl::TEXTURE_3D => minify(d, l),
                _ => desc.depth,
            };
            let row = lw as usize * internal.format.bytes();
            let Some(zero) = pixels::try_zeroed(row * lh as usize * ld as usize) else {
                self.backend.destroy_resource(resource);
                return self.err(gl::OUT_OF_MEMORY);
            };
            self.backend.write(resource, l, Region::new(0, 0, 0, lw, lh, ld), &zero, row, row * lh as usize);
        }
        let old = core::mem::replace(self.textures.get_mut(key), Texture::new(target));
        let mut tex = Texture {
            sampler: old.sampler,
            base_level: old.base_level,
            max_level: old.max_level,
            swizzle: old.swizzle,
            ..Texture::new(target)
        };
        self.destroy_texture(old);
        tex.immutable_levels = Some(levels);
        tex.storage = Some(Storage { resource, desc, base: 0 });
        for f in 0..tex.images.len() {
            for l in 0..levels {
                let depth = match target {
                    gl::TEXTURE_3D => minify(d, l),
                    gl::TEXTURE_2D_ARRAY => d,
                    _ => 1,
                };
                tex.images[f][l as usize] = Some(Image {
                    internal,
                    compressed,
                    unsized_format: false,
                    width: minify(w, l),
                    height: minify(h, l),
                    depth,
                    own: None,
                });
            }
        }
        *self.textures.get_mut(key) = tex;
    }

    // ---- Copies from the framebuffer ---------------------------------------------

    /// `glCopyTexImage2D`.
    pub fn copy_tex_image_2d(
        &mut self,
        target: u32,
        level: i32,
        internalformat: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        border: i32,
    ) {
        let Some((tex_target, face)) =
            image_target(target).filter(|t| matches!(t.0, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP))
        else {
            return self.err(gl::INVALID_ENUM);
        };
        if !self.check_size(tex_target, level, width, height, 1) {
            return;
        }
        if border != 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let src = match self.read_color() {
            Ok(s) => s,
            Err(e) => return self.err(e),
        };
        let internal = match copy_format(&src.internal, internalformat) {
            Ok(i) => i,
            Err(e) => return self.err(e),
        };
        let key = self.bound_texture(tex_target).unwrap();
        if self.textures.get(key).immutable_levels.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        let img = Image {
            internal,
            compressed: None,
            unsized_format: is_unsized(internalformat),
            width: width as u32,
            height: height as u32,
            depth: 1,
            own: None,
        };
        if !self.define_image(key, face, level as u32, img, None) {
            return self.err(gl::OUT_OF_MEMORY);
        }
        self.copy_from_read_buffer(&src, key, face, level as u32, [0, 0, 0], x, y, width, height);
    }

    /// `glCopyTexSubImage2D`.
    pub fn copy_tex_sub_image_2d(
        &mut self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        let Some((tex_target, face)) =
            image_target(target).filter(|t| matches!(t.0, gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP))
        else {
            return self.err(gl::INVALID_ENUM);
        };
        self.copy_tex_sub_image(tex_target, face, level, [xoffset, yoffset, 0], x, y, width, height);
    }

    /// `glCopyTexSubImage3D`.
    pub fn copy_tex_sub_image_3d(
        &mut self,
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        zoffset: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        if !matches!(target, gl::TEXTURE_3D | gl::TEXTURE_2D_ARRAY) {
            return self.err(gl::INVALID_ENUM);
        }
        self.copy_tex_sub_image(target, 0, level, [xoffset, yoffset, zoffset], x, y, width, height);
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_tex_sub_image(
        &mut self,
        tex_target: u32,
        face: usize,
        level: i32,
        off: [i32; 3],
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        let Some((key, img)) = self.sub_image(tex_target, face, level, off, [w, h, 1]) else { return };
        let src = match self.read_color() {
            Ok(s) => s,
            Err(e) => return self.err(e),
        };
        // The image's format must be one the read buffer can be copied to.
        let base = if img.unsized_format { img.internal.base } else { img.internal.gl };
        if img.compressed.is_some() || copy_format(&src.internal, base).is_err() {
            return self.err(gl::INVALID_OPERATION);
        }
        self.copy_from_read_buffer(&src, key, face, level as u32, off.map(|v| v as u32), x, y, w, h);
    }

    /// Copies the read buffer's rectangle `(x, y, w, h)` to an image at
    /// `off`; pixels outside the read buffer are left alone.
    #[allow(clippy::too_many_arguments)]
    fn copy_from_read_buffer(
        &mut self,
        src: &super::framebuffers::ReadColor,
        key: Key,
        face: usize,
        level: u32,
        off: [u32; 3],
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        let (x0, y0) = (x.max(0), y.max(0));
        let x1 = (x as i64 + w as i64).min(src.fb.width as i64) as i32;
        let y1 = (y as i64 + h as i64).min(src.fb.height as i64) as i32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let tex = self.textures.get(key);
        let Some((r, l, layer)) = self.image_location(tex, face, level) else { return };
        let dst = Surface { resource: r, level: l, layer: layer + off[2] };
        let rect = crate::backend::Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
        let (dx, dy) = (off[0] + (x0 - x) as u32, off[1] + (y0 - y) as u32);
        self.backend.copy_to_texture(&src.fb, src.index, rect, dst, dx, dy);
    }

    // ---- Parameters ------------------------------------------------------------

    /// Sets a sampler parameter shared by textures and sampler objects;
    /// `None` if `pname` is not one.
    fn set_sampler_param(p: &mut SamplerParams, pname: u32, v: Param) -> Option<Result<(), u32>> {
        let e = v.enum_();
        Some(match pname {
            gl::TEXTURE_MIN_FILTER if filter_valid(e, true) => {
                p.min_filter = e;
                Ok(())
            }
            gl::TEXTURE_MAG_FILTER if filter_valid(e, false) => {
                p.mag_filter = e;
                Ok(())
            }
            gl::TEXTURE_WRAP_S | gl::TEXTURE_WRAP_T | gl::TEXTURE_WRAP_R
                if matches!(e, gl::REPEAT | gl::CLAMP_TO_EDGE | gl::MIRRORED_REPEAT) =>
            {
                let i = match pname {
                    gl::TEXTURE_WRAP_S => 0,
                    gl::TEXTURE_WRAP_T => 1,
                    _ => 2,
                };
                p.wrap[i] = e;
                Ok(())
            }
            gl::TEXTURE_MIN_LOD => {
                p.min_lod = v.float();
                Ok(())
            }
            gl::TEXTURE_MAX_LOD => {
                p.max_lod = v.float();
                Ok(())
            }
            gl::TEXTURE_COMPARE_MODE if matches!(e, gl::NONE | gl::COMPARE_REF_TO_TEXTURE) => {
                p.compare_mode = e;
                Ok(())
            }
            gl::TEXTURE_COMPARE_FUNC if super::compare_func(e).is_some() => {
                p.compare_func = e;
                Ok(())
            }
            gl::TEXTURE_MAX_ANISOTROPY_EXT => {
                let f = v.float();
                if f.is_nan() || f < 1.0 {
                    Err(gl::INVALID_VALUE)
                } else {
                    p.max_anisotropy = f;
                    Ok(())
                }
            }
            gl::TEXTURE_MIN_FILTER
            | gl::TEXTURE_MAG_FILTER
            | gl::TEXTURE_WRAP_S
            | gl::TEXTURE_WRAP_T
            | gl::TEXTURE_WRAP_R
            | gl::TEXTURE_COMPARE_MODE
            | gl::TEXTURE_COMPARE_FUNC => Err(gl::INVALID_ENUM),
            _ => return None,
        })
    }

    fn tex_parameter(&mut self, target: u32, pname: u32, v: Param) {
        let Some(key) = self.bound_texture(target) else { return self.err(gl::INVALID_ENUM) };
        let max_aniso = self.backend.caps().max_anisotropy;
        let t = self.textures.get_mut(key);
        if let Some(r) = Self::set_sampler_param(&mut t.sampler, pname, v) {
            t.sampler.max_anisotropy = t.sampler.max_anisotropy.min(max_aniso);
            if let Err(e) = r {
                self.err(e);
            }
            return;
        }
        let e = v.enum_();
        match pname {
            gl::TEXTURE_BASE_LEVEL | gl::TEXTURE_MAX_LEVEL => {
                let i = v.int();
                if i < 0 {
                    return self.err(gl::INVALID_VALUE);
                }
                if pname == gl::TEXTURE_BASE_LEVEL {
                    t.base_level = i as u32;
                } else {
                    t.max_level = i as u32;
                }
            }
            gl::TEXTURE_SWIZZLE_R | gl::TEXTURE_SWIZZLE_G | gl::TEXTURE_SWIZZLE_B | gl::TEXTURE_SWIZZLE_A => {
                if !matches!(e, gl::RED | gl::GREEN | gl::BLUE | gl::ALPHA | gl::ZERO | gl::ONE) {
                    return self.err(gl::INVALID_ENUM);
                }
                t.swizzle[(pname - gl::TEXTURE_SWIZZLE_R) as usize] = e;
            }
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glTexParameteri`.
    pub fn tex_parameteri(&mut self, target: u32, pname: u32, param: i32) {
        self.tex_parameter(target, pname, Param::I(param));
    }

    /// `glTexParameterf`.
    pub fn tex_parameterf(&mut self, target: u32, pname: u32, param: f32) {
        self.tex_parameter(target, pname, Param::F(param));
    }

    /// The value of a texture or sampler parameter.
    fn sampler_param(p: &SamplerParams, pname: u32) -> Option<Param> {
        Some(match pname {
            gl::TEXTURE_MIN_FILTER => Param::I(p.min_filter as i32),
            gl::TEXTURE_MAG_FILTER => Param::I(p.mag_filter as i32),
            gl::TEXTURE_WRAP_S => Param::I(p.wrap[0] as i32),
            gl::TEXTURE_WRAP_T => Param::I(p.wrap[1] as i32),
            gl::TEXTURE_WRAP_R => Param::I(p.wrap[2] as i32),
            gl::TEXTURE_MIN_LOD => Param::F(p.min_lod),
            gl::TEXTURE_MAX_LOD => Param::F(p.max_lod),
            gl::TEXTURE_COMPARE_MODE => Param::I(p.compare_mode as i32),
            gl::TEXTURE_COMPARE_FUNC => Param::I(p.compare_func as i32),
            gl::TEXTURE_MAX_ANISOTROPY_EXT => Param::F(p.max_anisotropy),
            _ => return None,
        })
    }

    fn get_tex_parameter(&mut self, target: u32, pname: u32) -> Option<Param> {
        let Some(key) = self.bound_texture(target) else {
            self.err(gl::INVALID_ENUM);
            return None;
        };
        let t = self.textures.get(key);
        if let Some(p) = Self::sampler_param(&t.sampler, pname) {
            return Some(p);
        }
        Some(match pname {
            gl::TEXTURE_BASE_LEVEL => Param::I(t.base_level.min(i32::MAX as u32) as i32),
            gl::TEXTURE_MAX_LEVEL => Param::I(t.max_level.min(i32::MAX as u32) as i32),
            gl::TEXTURE_SWIZZLE_R | gl::TEXTURE_SWIZZLE_G | gl::TEXTURE_SWIZZLE_B | gl::TEXTURE_SWIZZLE_A => {
                Param::I(t.swizzle[(pname - gl::TEXTURE_SWIZZLE_R) as usize] as i32)
            }
            gl::TEXTURE_IMMUTABLE_FORMAT => Param::I(i32::from(t.immutable_levels.is_some())),
            gl::TEXTURE_IMMUTABLE_LEVELS => Param::I(t.immutable_levels.unwrap_or(0) as i32),
            _ => {
                self.err(gl::INVALID_ENUM);
                return None;
            }
        })
    }

    /// `glGetTexParameteriv` (one value).
    pub fn get_tex_parameteri(&mut self, target: u32, pname: u32) -> i32 {
        self.get_tex_parameter(target, pname).map_or(0, |p| match p {
            Param::I(i) => i,
            Param::F(f) => vmath::f32::round(f) as i32,
        })
    }

    /// `glGetTexParameterfv` (one value).
    pub fn get_tex_parameterf(&mut self, target: u32, pname: u32) -> f32 {
        self.get_tex_parameter(target, pname).map_or(0.0, Param::float)
    }

    // ---- Mipmaps ------------------------------------------------------------------

    /// `glGenerateMipmap`.
    pub fn generate_mipmap(&mut self, target: u32) {
        if target_index(target).is_none() {
            return self.err(gl::INVALID_ENUM);
        }
        let key = self.bound_texture(target).unwrap();
        let tex = self.textures.get(key);
        let (base, _) = Self::level_range(tex);
        let Some(b) = tex.image(0, base).copied() else { return self.err(gl::INVALID_OPERATION) };
        let i = b.internal;
        let renderable = i.renderable || (i.float_renderable && self.backend.caps().color_buffer_float);
        let filterable = i.filterable || (i.format.class() == Class::Float && self.backend.caps().float_linear);
        if !(b.unsized_format || (renderable && filterable)) || b.compressed.is_some() || i.format.has_depth() {
            return self.err(gl::INVALID_OPERATION);
        }
        if target == gl::TEXTURE_CUBE_MAP && !Self::cube_complete(tex, base) {
            return self.err(gl::INVALID_OPERATION);
        }
        if b.width == 0 || b.height == 0 || b.depth == 0 {
            return;
        }
        // Levels base + 1 to q are replaced.
        let max_dim = if target == gl::TEXTURE_3D { b.width.max(b.height).max(b.depth) } else { b.width.max(b.height) };
        let p = base + chain_levels(max_dim) - 1;
        let q = match tex.immutable_levels {
            Some(n) => p.min(n - 1).min(tex.max_level.max(base)),
            None => p.min(tex.max_level).min(MAX_LEVELS as u32 - 1),
        };
        if q <= base {
            return;
        }
        let faces = tex.images.len();
        let immutable = tex.immutable_levels.is_some();
        if !immutable {
            for f in 0..faces {
                for l in base + 1..=q {
                    let k = l - base;
                    let depth = if target == gl::TEXTURE_3D { minify(b.depth, k) } else { b.depth };
                    let img = Image { width: minify(b.width, k), height: minify(b.height, k), depth, own: None, ..b };
                    // Replace the level: in the storage if it fits there
                    // (the renderer fills it below), else as a new image.
                    let old = self.textures.get_mut(key).images[f][l as usize].take();
                    if let Some(r) = old.and_then(|i| i.own) {
                        self.backend.destroy_resource(r);
                    }
                    let tex = self.textures.get_mut(key);
                    if tex.storage.is_some_and(|s| Self::fits(&s, target, &img, l)) {
                        tex.images[f][l as usize] = Some(img);
                    } else if !self.define_image(key, f, l, img, None) {
                        return self.err(gl::OUT_OF_MEMORY);
                    }
                }
            }
        }
        let Some(s) = self.finalize(key, base, q) else { return self.err(gl::OUT_OF_MEMORY) };
        self.backend.generate_mipmap(s.resource, base - s.base, q - s.base);
    }

    // ---- Sampler objects ----------------------------------------------------------

    /// `glGenSamplers`.
    pub fn gen_samplers(&mut self, names: &mut [u32]) {
        for n in names {
            let name = self.samplers.generate();
            self.samplers.create(name, Sampler::default());
            *n = name;
        }
    }

    /// One sampler (created at once, as `glGenSamplers` does).
    pub fn gen_sampler(&mut self) -> u32 {
        let mut n = [0];
        self.gen_samplers(&mut n);
        n[0]
    }

    /// `glIsSampler`.
    pub fn is_sampler(&self, name: u32) -> bool {
        self.samplers.key(name).is_some()
    }

    /// `glDeleteSamplers`.
    pub fn delete_samplers(&mut self, names: &[u32]) {
        for &name in names {
            if let Some(key) = self.samplers.key(name) {
                for unit in &mut self.sampler_units {
                    if *unit == Some(key) {
                        *unit = None;
                    }
                }
            }
            let _ = self.samplers.delete(name);
        }
    }

    /// `glBindSampler`.
    pub fn bind_sampler(&mut self, unit: u32, sampler: u32) {
        if unit as usize >= TEXTURE_UNITS {
            return self.err(gl::INVALID_VALUE);
        }
        let key = if sampler == 0 {
            None
        } else {
            match self.samplers.key(sampler) {
                Some(k) => Some(k),
                None => return self.err(gl::INVALID_OPERATION),
            }
        };
        self.sampler_units[unit as usize] = key;
    }

    fn sampler_parameter(&mut self, sampler: u32, pname: u32, v: Param) {
        let Some(key) = self.samplers.key(sampler) else { return self.err(gl::INVALID_OPERATION) };
        let max_aniso = self.backend.caps().max_anisotropy;
        let s = self.samplers.get_mut(key);
        match Self::set_sampler_param(&mut s.params, pname, v) {
            Some(Ok(())) => s.params.max_anisotropy = s.params.max_anisotropy.min(max_aniso),
            Some(Err(e)) => self.err(e),
            None => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glSamplerParameteri`.
    pub fn sampler_parameteri(&mut self, sampler: u32, pname: u32, param: i32) {
        self.sampler_parameter(sampler, pname, Param::I(param));
    }

    /// `glSamplerParameterf`.
    pub fn sampler_parameterf(&mut self, sampler: u32, pname: u32, param: f32) {
        self.sampler_parameter(sampler, pname, Param::F(param));
    }

    fn get_sampler_param(&mut self, sampler: u32, pname: u32) -> Option<Param> {
        let Some(key) = self.samplers.key(sampler) else {
            self.err(gl::INVALID_OPERATION);
            return None;
        };
        let p = Self::sampler_param(&self.samplers.get(key).params, pname);
        if p.is_none() {
            self.err(gl::INVALID_ENUM);
        }
        p
    }

    /// `glGetSamplerParameteriv` (one value).
    pub fn get_sampler_parameteri(&mut self, sampler: u32, pname: u32) -> i32 {
        self.get_sampler_param(sampler, pname).map_or(0, |p| match p {
            Param::I(i) => i,
            Param::F(f) => vmath::f32::round(f) as i32,
        })
    }

    /// `glGetSamplerParameterfv` (one value).
    pub fn get_sampler_parameterf(&mut self, sampler: u32, pname: u32) -> f32 {
        self.get_sampler_param(sampler, pname).map_or(0.0, Param::float)
    }

    /// The texture names bound to a unit (for state queries).
    pub(crate) fn unit_binding(&self, target: u32) -> u32 {
        let t = target_index(target).unwrap_or(0);
        self.textures.name(self.texture_units[self.active_texture][t])
    }
}
