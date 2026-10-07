//! Framebuffers and renderbuffers (OpenGL ES 3.0 chapter 4): attachments
//! and completeness, clearing, reading pixels, blits, and the default
//! framebuffer a window provides.

use alloc::vec::Vec;

use super::{Attachment, Context, DRAW_BUFFERS, FramebufferObject, Key, Renderbuffer};
use crate::backend::{Blit, Clear, Filter, Framebuffer, Rect, Region, ResourceDesc, ResourceId, Surface, Target};
use crate::format::{self, Class, Client, Format, Internal};
use crate::gl;
use crate::pixels::{self, PixelsMut};

/// The read buffer as a copy or `ReadPixels` sees it.
pub(crate) struct ReadColor {
    pub fb: Framebuffer,
    /// The color attachment read.
    pub index: u8,
    pub surface: Surface,
    pub internal: Internal,
}

/// What an attachment point holds, resolved: the image and its format.
#[derive(Clone, Copy)]
struct Resolved {
    surface: Surface,
    internal: Internal,
    width: u32,
    height: u32,
    samples: u32,
}

/// The `(format, type)` `ReadPixels` takes for a buffer besides the
/// required one (`IMPLEMENTATION_COLOR_READ_FORMAT`/`_TYPE`): the buffer's
/// own layout.
pub(crate) fn natural_read(i: &Internal) -> (u32, u32) {
    use Format as F;
    match i.format {
        F::R8Unorm => (gl::RED, gl::UNSIGNED_BYTE),
        F::Rg8Unorm => (gl::RG, gl::UNSIGNED_BYTE),
        F::Rgbx8Unorm | F::Rgbx8Srgb => (gl::RGB, gl::UNSIGNED_BYTE),
        F::B5G6R5Unorm => (gl::RGB, gl::UNSIGNED_SHORT_5_6_5),
        F::Rgba4Unorm => (gl::RGBA, gl::UNSIGNED_SHORT_4_4_4_4),
        F::Rgb5A1Unorm => (gl::RGBA, gl::UNSIGNED_SHORT_5_5_5_1),
        F::Rgb10A2Unorm => (gl::RGBA, gl::UNSIGNED_INT_2_10_10_10_REV),
        F::Rgb10A2Uint => (gl::RGBA_INTEGER, gl::UNSIGNED_INT_2_10_10_10_REV),
        F::R8Uint => (gl::RED_INTEGER, gl::UNSIGNED_BYTE),
        F::Rg8Uint => (gl::RG_INTEGER, gl::UNSIGNED_BYTE),
        F::Rgba8Uint => (gl::RGBA_INTEGER, gl::UNSIGNED_BYTE),
        F::R8Sint => (gl::RED_INTEGER, gl::BYTE),
        F::Rg8Sint => (gl::RG_INTEGER, gl::BYTE),
        F::Rgba8Sint => (gl::RGBA_INTEGER, gl::BYTE),
        F::R16Uint => (gl::RED_INTEGER, gl::UNSIGNED_SHORT),
        F::Rg16Uint => (gl::RG_INTEGER, gl::UNSIGNED_SHORT),
        F::Rgba16Uint => (gl::RGBA_INTEGER, gl::UNSIGNED_SHORT),
        F::R16Sint => (gl::RED_INTEGER, gl::SHORT),
        F::Rg16Sint => (gl::RG_INTEGER, gl::SHORT),
        F::Rgba16Sint => (gl::RGBA_INTEGER, gl::SHORT),
        F::R32Uint => (gl::RED_INTEGER, gl::UNSIGNED_INT),
        F::Rg32Uint => (gl::RG_INTEGER, gl::UNSIGNED_INT),
        F::R32Sint => (gl::RED_INTEGER, gl::INT),
        F::Rg32Sint => (gl::RG_INTEGER, gl::INT),
        F::R16Float => (gl::RED, gl::HALF_FLOAT),
        F::Rg16Float => (gl::RG, gl::HALF_FLOAT),
        F::Rgba16Float => (gl::RGBA, gl::HALF_FLOAT),
        F::R32Float => (gl::RED, gl::FLOAT),
        F::Rg32Float => (gl::RG, gl::FLOAT),
        F::R11G11B10Float => (gl::RGB, gl::UNSIGNED_INT_10F_11F_11F_REV),
        F::Rgba32Uint => (gl::RGBA_INTEGER, gl::UNSIGNED_INT),
        F::Rgba32Sint => (gl::RGBA_INTEGER, gl::INT),
        F::Rgba32Float => (gl::RGBA, gl::FLOAT),
        _ => (gl::RGBA, gl::UNSIGNED_BYTE),
    }
}

/// The required `ReadPixels` `(format, type)` for a buffer's class.
fn required_read(i: &Internal) -> (u32, u32) {
    match i.format.class() {
        Class::Uint => (gl::RGBA_INTEGER, gl::UNSIGNED_INT),
        Class::Sint => (gl::RGBA_INTEGER, gl::INT),
        Class::Float => (gl::RGBA, gl::FLOAT),
        _ => (gl::RGBA, gl::UNSIGNED_BYTE),
    }
}

/// The value a color attachment is cleared to (32-bit components as the
/// format's class reads them).
fn color_bits(f: Format, c: [f32; 4]) -> [u32; 4] {
    match f.class() {
        Class::Uint => c.map(|x| vmath::f32::round(x.max(0.0)) as u32),
        Class::Sint => c.map(|x| vmath::f32::round(x) as i32 as u32),
        _ => c.map(f32::to_bits),
    }
}

impl Context {
    // ---- The default framebuffer -------------------------------------------------

    /// (Re)creates the default framebuffer's buffers at `width` x `height`.
    pub(crate) fn create_default_framebuffer(&mut self, width: u32, height: u32) {
        for r in [self.default_fb.color.take(), self.default_fb.depth_stencil.take()].into_iter().flatten() {
            self.backend.destroy_resource(r);
        }
        let color = format::sized(self.config.color).or_else(|| format::sized(gl::RGBA8)).copied();
        let depth = match (self.config.depth_bits, self.config.stencil_bits) {
            (0, 0) => None,
            (0, _) => format::sized(gl::STENCIL_INDEX8),
            (16, 0) => format::sized(gl::DEPTH_COMPONENT16),
            (_, 0) => format::sized(gl::DEPTH_COMPONENT24),
            _ => format::sized(gl::DEPTH24_STENCIL8),
        }
        .copied();
        let samples = if self.config.samples > 0 && self.backend.caps().max_samples >= 4 { 4 } else { 0 };
        let (w, h) = (width.max(1), height.max(1));
        let make = |c: &mut Context, f: Format| {
            let desc = ResourceDesc {
                target: Target::Renderbuffer,
                format: f,
                width: w,
                height: h,
                depth: 1,
                levels: 1,
                samples,
            };
            c.backend.create_resource(&desc).ok()
        };
        let fb = &mut self.default_fb;
        fb.width = width;
        fb.height = height;
        fb.color_internal = color;
        fb.depth_internal = depth;
        if fb.draw_buffer == 0 {
            fb.draw_buffer = gl::BACK;
            fb.read_buffer = gl::BACK;
        }
        self.default_fb.color = color.and_then(|c| make(self, c.format));
        self.default_fb.depth_stencil = depth.and_then(|d| make(self, d.format));
        // What the renderer could not make the framebuffer has not, and the
        // GL's queries (DEPTH_BITS, ...) say so.
        if self.default_fb.color.is_none() {
            self.default_fb.color_internal = None;
        }
        if self.default_fb.depth_stencil.is_none() {
            self.default_fb.depth_internal = None;
        }
        self.default_samples = samples;
    }

    /// Resizes the default framebuffer (the window changed size). Its
    /// contents become undefined.
    pub fn resize(&mut self, width: u32, height: u32) {
        if (width, height) != (self.default_fb.width, self.default_fb.height) {
            self.create_default_framebuffer(width, height);
        }
    }

    /// The default framebuffer's color buffer and size, for presenting it.
    pub fn default_color_buffer(&self) -> Option<(ResourceId, Format, u32, u32)> {
        let fb = &self.default_fb;
        Some((fb.color?, fb.color_internal?.format, fb.width, fb.height))
    }

    /// The default framebuffer as a renderer sees it.
    fn default_framebuffer(&self, draw: bool) -> Framebuffer {
        let fb = &self.default_fb;
        let mut out = Framebuffer {
            width: fb.width.max(1),
            height: fb.height.max(1),
            samples: self.default_samples,
            ..Default::default()
        };
        out.colors[0] = fb.color.map(|r| Surface { resource: r, level: 0, layer: 0 });
        let ds = fb.depth_stencil.map(|r| Surface { resource: r, level: 0, layer: 0 });
        let di = fb.depth_internal;
        out.depth = ds.filter(|_| di.is_some_and(|i| i.format.has_depth()));
        out.stencil = ds.filter(|_| di.is_some_and(|i| i.format.has_stencil()));
        let buffer = if draw { fb.draw_buffer } else { fb.read_buffer };
        out.draw_buffers[0] = (buffer == gl::BACK).then_some(0);
        out
    }

    // ---- Framebuffer objects -------------------------------------------------------

    /// `glGenFramebuffers`.
    pub fn gen_framebuffers(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.framebuffers.generate();
        }
    }

    /// One framebuffer name.
    pub fn gen_framebuffer(&mut self) -> u32 {
        self.framebuffers.generate()
    }

    /// `glIsFramebuffer`.
    pub fn is_framebuffer(&self, name: u32) -> bool {
        self.framebuffers.key(name).is_some()
    }

    /// `glBindFramebuffer`.
    pub fn bind_framebuffer(&mut self, target: u32, name: u32) {
        if !matches!(target, gl::FRAMEBUFFER | gl::DRAW_FRAMEBUFFER | gl::READ_FRAMEBUFFER) {
            return self.err(gl::INVALID_ENUM);
        }
        let key =
            if name == 0 { None } else { Some(self.framebuffers.get_or_create(name, FramebufferObject::default)) };
        if target != gl::READ_FRAMEBUFFER {
            self.draw_framebuffer = key;
        }
        if target != gl::DRAW_FRAMEBUFFER {
            self.read_framebuffer = key;
        }
    }

    /// `glDeleteFramebuffers`.
    pub fn delete_framebuffers(&mut self, names: &[u32]) {
        for &name in names {
            let key = self.framebuffers.key(name);
            if key.is_some() {
                if self.draw_framebuffer == key {
                    self.draw_framebuffer = None;
                }
                if self.read_framebuffer == key {
                    self.read_framebuffer = None;
                }
            }
            if let Some((_, Some(fb))) = self.framebuffers.delete(name) {
                for a in fb.attachments() {
                    self.release_attachment(a);
                }
            }
        }
    }

    fn release_attachment(&mut self, a: Attachment) {
        match a {
            Attachment::None => {}
            Attachment::Renderbuffer(k) => {
                if let Some(rb) = self.renderbuffers.release(k) {
                    self.destroy_renderbuffer(rb);
                }
            }
            Attachment::Texture { key, .. } => self.release_texture(key),
        }
    }

    fn retain_attachment(&mut self, a: Attachment) {
        match a {
            Attachment::None => {}
            Attachment::Renderbuffer(k) => self.renderbuffers.retain(k),
            Attachment::Texture { key, .. } => self.textures.retain(key),
        }
    }

    /// Detaches what `pred` matches from the bound framebuffers (deleting a
    /// texture or renderbuffer does so, appendix D.1.2).
    pub(crate) fn detach_from_bound_framebuffers(&mut self, pred: impl Fn(Attachment) -> bool) {
        let mut keys: Vec<Key> = self.draw_framebuffer.into_iter().collect();
        if self.read_framebuffer != self.draw_framebuffer {
            keys.extend(self.read_framebuffer);
        }
        for k in keys {
            let fb = self.framebuffers.get_mut(k);
            let mut released = Vec::new();
            for a in fb.colors.iter_mut().chain([&mut fb.depth, &mut fb.stencil]) {
                if pred(*a) {
                    released.push(core::mem::take(a));
                }
            }
            for a in released {
                self.release_attachment(a);
            }
        }
    }

    /// The framebuffer bound to `target`: `Err` for an unknown target,
    /// `Ok(None)` for the default framebuffer.
    fn framebuffer_target(&self, target: u32) -> Result<Option<Key>, ()> {
        match target {
            gl::FRAMEBUFFER | gl::DRAW_FRAMEBUFFER => Ok(self.draw_framebuffer),
            gl::READ_FRAMEBUFFER => Ok(self.read_framebuffer),
            _ => Err(()),
        }
    }

    /// The attachment points `attachment` names: indices into the colors,
    /// or 8 (depth), 9 (stencil), 10 (both).
    fn attachment_point(&mut self, attachment: u32) -> Option<usize> {
        match attachment {
            gl::DEPTH_ATTACHMENT => Some(8),
            gl::STENCIL_ATTACHMENT => Some(9),
            gl::DEPTH_STENCIL_ATTACHMENT => Some(10),
            a if (gl::COLOR_ATTACHMENT0..=gl::COLOR_ATTACHMENT15).contains(&a) => {
                let i = (a - gl::COLOR_ATTACHMENT0) as usize;
                if i < DRAW_BUFFERS {
                    Some(i)
                } else {
                    self.err(gl::INVALID_OPERATION);
                    None
                }
            }
            _ => {
                self.err(gl::INVALID_ENUM);
                None
            }
        }
    }

    /// Sets an attachment point of the framebuffer bound to `target`.
    fn attach(&mut self, key: Key, point: usize, a: Attachment) {
        let n = if point == 10 { 2 } else { 1 };
        for _ in 0..n {
            self.retain_attachment(a);
        }
        let fb = self.framebuffers.get_mut(key);
        let old: Vec<Attachment> = match point {
            8 => alloc::vec![core::mem::replace(&mut fb.depth, a)],
            9 => alloc::vec![core::mem::replace(&mut fb.stencil, a)],
            10 => alloc::vec![core::mem::replace(&mut fb.depth, a), core::mem::replace(&mut fb.stencil, a)],
            i => alloc::vec![core::mem::replace(&mut fb.colors[i], a)],
        };
        for o in old {
            self.release_attachment(o);
        }
    }

    /// The framebuffer object an attaching call changes, with its errors.
    fn attaching(&mut self, target: u32, attachment: u32) -> Option<(Key, usize)> {
        let Ok(fb) = self.framebuffer_target(target) else {
            self.err(gl::INVALID_ENUM);
            return None;
        };
        let point = self.attachment_point(attachment)?;
        let Some(fb) = fb else {
            self.err(gl::INVALID_OPERATION);
            return None;
        };
        Some((fb, point))
    }

    /// `glFramebufferTexture2D`.
    pub fn framebuffer_texture_2d(&mut self, target: u32, attachment: u32, textarget: u32, texture: u32, level: i32) {
        let Some((fb, point)) = self.attaching(target, attachment) else { return };
        if texture == 0 {
            return self.attach(fb, point, Attachment::None);
        }
        let face = match textarget {
            gl::TEXTURE_2D => None,
            gl::TEXTURE_CUBE_MAP_POSITIVE_X..=gl::TEXTURE_CUBE_MAP_NEGATIVE_Z => {
                Some(textarget - gl::TEXTURE_CUBE_MAP_POSITIVE_X)
            }
            _ => return self.err(gl::INVALID_ENUM),
        };
        let Some(key) = self.textures.key(texture) else { return self.err(gl::INVALID_OPERATION) };
        let t = self.textures.get(key).target;
        if (face.is_some() && t != gl::TEXTURE_CUBE_MAP) || (face.is_none() && t != gl::TEXTURE_2D) {
            return self.err(gl::INVALID_OPERATION);
        }
        let caps = self.backend.caps();
        let max = if face.is_some() { caps.max_cube_map_size } else { caps.max_texture_size };
        if level < 0 || level as u32 >= super::textures::chain_levels(max) {
            return self.err(gl::INVALID_VALUE);
        }
        self.attach(fb, point, Attachment::Texture { key, level: level as u32, layer: face.unwrap_or(0) });
    }

    /// `glFramebufferTextureLayer`.
    pub fn framebuffer_texture_layer(&mut self, target: u32, attachment: u32, texture: u32, level: i32, layer: i32) {
        let Some((fb, point)) = self.attaching(target, attachment) else { return };
        if texture == 0 {
            return self.attach(fb, point, Attachment::None);
        }
        let Some(key) = self.textures.key(texture) else { return self.err(gl::INVALID_OPERATION) };
        let t = self.textures.get(key).target;
        let caps = self.backend.caps();
        let (max_level_size, max_layer) = match t {
            gl::TEXTURE_3D => (caps.max_3d_texture_size, caps.max_3d_texture_size),
            gl::TEXTURE_2D_ARRAY => (caps.max_texture_size, caps.max_array_layers),
            _ => return self.err(gl::INVALID_OPERATION),
        };
        if level < 0
            || level as u32 >= super::textures::chain_levels(max_level_size)
            || layer < 0
            || layer as u32 >= max_layer
        {
            return self.err(gl::INVALID_VALUE);
        }
        self.attach(fb, point, Attachment::Texture { key, level: level as u32, layer: layer as u32 });
    }

    /// `glFramebufferRenderbuffer`.
    pub fn framebuffer_renderbuffer(
        &mut self,
        target: u32,
        attachment: u32,
        renderbuffer_target: u32,
        renderbuffer: u32,
    ) {
        if renderbuffer_target != gl::RENDERBUFFER {
            return self.err(gl::INVALID_ENUM);
        }
        let Some((fb, point)) = self.attaching(target, attachment) else { return };
        if renderbuffer == 0 {
            return self.attach(fb, point, Attachment::None);
        }
        let Some(key) = self.renderbuffers.key(renderbuffer) else { return self.err(gl::INVALID_OPERATION) };
        self.attach(fb, point, Attachment::Renderbuffer(key));
    }

    // ---- Completeness ------------------------------------------------------------

    /// An attachment's image, if it is attachment complete for the point
    /// (`Err` if it is not).
    fn resolve(&mut self, a: Attachment, point: usize) -> Result<Option<Resolved>, ()> {
        let caps_float = self.backend.caps().color_buffer_float;
        let r = match a {
            Attachment::None => return Ok(None),
            Attachment::Renderbuffer(k) => {
                let rb = self.renderbuffers.get(k);
                let (Some(internal), Some(resource)) = (rb.internal, rb.resource) else { return Err(()) };
                Resolved {
                    surface: Surface { resource, level: 0, layer: 0 },
                    internal,
                    width: rb.width,
                    height: rb.height,
                    samples: rb.samples,
                }
            }
            Attachment::Texture { key, level, layer } => {
                let tex = self.textures.get(key);
                let cube = tex.target == gl::TEXTURE_CUBE_MAP;
                let face = if cube { layer as usize } else { 0 };
                let Some(img) = tex.image(face, level).copied() else { return Err(()) };
                if !cube && tex.target != gl::TEXTURE_2D && layer >= img.depth {
                    return Err(());
                }
                // A mutable texture's level must be in its range, and other
                // than the base level only if the texture is mipmap (and
                // cube) complete.
                if tex.immutable_levels.is_none() {
                    let (base, q) = Self::level_range(tex);
                    if level < base || level > q {
                        return Err(());
                    }
                    if level != base && !Self::mipmap_complete(tex, base, q) {
                        return Err(());
                    }
                }
                let Some((resource, l, first)) = self.image_location(tex, face, level) else { return Err(()) };
                let layer = if cube { first } else { first + layer };
                Resolved {
                    surface: Surface { resource, level: l, layer },
                    internal: img.internal,
                    width: img.width,
                    height: img.height,
                    samples: 0,
                }
            }
        };
        if r.width == 0 || r.height == 0 {
            return Err(());
        }
        let f = r.internal.format;
        let ok = match point {
            8 => f.has_depth(),
            9 => f.has_stencil(),
            _ => r.internal.renderable || (r.internal.float_renderable && caps_float),
        };
        if ok { Ok(Some(r)) } else { Err(()) }
    }

    /// The renderer's view of a framebuffer object, or its completeness
    /// status (`CheckFramebufferStatus`) if it is not complete.
    fn fbo_state(&mut self, key: Key, draw: bool) -> Result<Framebuffer, u32> {
        let fbo = self.framebuffers.get(key).clone();
        let mut out = Framebuffer::default();
        let mut size: Option<(u32, u32)> = None;
        let mut samples: Option<u32> = None;
        let mut renderbuffers = false;
        let mut textures = false;
        let mut any = false;
        let mut images = [None; DRAW_BUFFERS + 2];
        for (i, a) in fbo.attachments().enumerate() {
            let point = if i < DRAW_BUFFERS { i } else { 8 + (i - DRAW_BUFFERS) };
            let Ok(r) = self.resolve(a, point) else { return Err(gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT) };
            let Some(r) = r else { continue };
            any = true;
            renderbuffers |= matches!(a, Attachment::Renderbuffer(_));
            textures |= matches!(a, Attachment::Texture { .. });
            if samples.is_some_and(|s| s != r.samples) {
                return Err(gl::FRAMEBUFFER_INCOMPLETE_MULTISAMPLE);
            }
            samples = Some(r.samples);
            size = Some(match size {
                Some((w, h)) => (w.min(r.width), h.min(r.height)),
                None => (r.width, r.height),
            });
            images[i] = Some(r);
        }
        if !any {
            return Err(gl::FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT);
        }
        if renderbuffers && textures && samples != Some(0) {
            return Err(gl::FRAMEBUFFER_INCOMPLETE_MULTISAMPLE);
        }
        // Depth and stencil must be one image (both backends keep them in
        // one resource).
        let (d, s) = (images[DRAW_BUFFERS], images[DRAW_BUFFERS + 1]);
        if let (Some(d), Some(s)) = (d, s)
            && d.surface != s.surface
        {
            return Err(gl::FRAMEBUFFER_UNSUPPORTED);
        }
        for (i, img) in images.iter().take(DRAW_BUFFERS).enumerate() {
            out.colors[i] = img.map(|r| r.surface);
        }
        out.depth = d.map(|r| r.surface);
        out.stencil = s.map(|r| r.surface);
        let (w, h) = size.unwrap_or((0, 0));
        out.width = w;
        out.height = h;
        out.samples = samples.unwrap_or(0);
        if draw {
            for (i, &b) in fbo.draw_buffers.iter().enumerate() {
                if b != gl::NONE {
                    out.draw_buffers[i] = Some((b - gl::COLOR_ATTACHMENT0) as u8);
                }
            }
        } else if fbo.read_buffer != gl::NONE {
            out.draw_buffers[0] = Some((fbo.read_buffer - gl::COLOR_ATTACHMENT0) as u8);
        }
        Ok(out)
    }

    /// The draw framebuffer as the renderer sees it, or its status.
    pub(crate) fn draw_state_framebuffer(&mut self) -> Result<Framebuffer, u32> {
        match self.draw_framebuffer {
            Some(k) => self.fbo_state(k, true),
            None => Ok(self.default_framebuffer(true)),
        }
    }

    /// The read framebuffer (its read buffer as draw buffer 0), or its
    /// status.
    fn read_state_framebuffer(&mut self) -> Result<Framebuffer, u32> {
        match self.read_framebuffer {
            Some(k) => self.fbo_state(k, false),
            None => Ok(self.default_framebuffer(false)),
        }
    }

    /// `glCheckFramebufferStatus`.
    pub fn check_framebuffer_status(&mut self, target: u32) -> u32 {
        let r = match target {
            gl::FRAMEBUFFER | gl::DRAW_FRAMEBUFFER => self.draw_state_framebuffer(),
            gl::READ_FRAMEBUFFER => self.read_state_framebuffer(),
            _ => {
                self.err(gl::INVALID_ENUM);
                return 0;
            }
        };
        match r {
            Ok(_) => gl::FRAMEBUFFER_COMPLETE,
            Err(status) => status,
        }
    }

    /// The internal format of a draw framebuffer's color attachment.
    fn color_internal(&self, fbo: Option<Key>, index: usize) -> Option<Internal> {
        match fbo {
            None => (index == 0).then_some(self.default_fb.color_internal).flatten(),
            Some(k) => self.attachment_internal(self.framebuffers.get(k).colors.get(index).copied()?),
        }
    }

    fn attachment_internal(&self, a: Attachment) -> Option<Internal> {
        match a {
            Attachment::None => None,
            Attachment::Renderbuffer(k) => self.renderbuffers.get(k).internal,
            Attachment::Texture { key, level, layer } => {
                let tex = self.textures.get(key);
                let face = if tex.target == gl::TEXTURE_CUBE_MAP { layer as usize } else { 0 };
                tex.image(face, level).map(|i| i.internal)
            }
        }
    }

    /// The read buffer, with the errors of reading it.
    pub(crate) fn read_color(&mut self) -> Result<ReadColor, u32> {
        let fb = self.read_state_framebuffer().map_err(|_| gl::INVALID_FRAMEBUFFER_OPERATION)?;
        let Some(index) = fb.draw_buffers[0] else { return Err(gl::INVALID_OPERATION) };
        let surface = fb.colors[index as usize].ok_or(gl::INVALID_OPERATION)?;
        if self.read_framebuffer.is_some() && fb.samples > 0 {
            return Err(gl::INVALID_OPERATION);
        }
        let internal = self.color_internal(self.read_framebuffer, index as usize).ok_or(gl::INVALID_OPERATION)?;
        Ok(ReadColor { fb, index, surface, internal })
    }

    // ---- Draw and read buffers -------------------------------------------------------

    /// `glDrawBuffers`.
    pub fn draw_buffers(&mut self, bufs: &[u32]) {
        if bufs.len() > DRAW_BUFFERS {
            return self.err(gl::INVALID_VALUE);
        }
        match self.draw_framebuffer {
            None => {
                if bufs.len() != 1 || !matches!(bufs[0], gl::BACK | gl::NONE) {
                    let known = bufs.iter().all(|&b| {
                        matches!(b, gl::BACK | gl::NONE)
                            || (gl::COLOR_ATTACHMENT0..=gl::COLOR_ATTACHMENT15).contains(&b)
                    });
                    return self.err(if known { gl::INVALID_OPERATION } else { gl::INVALID_ENUM });
                }
                self.default_fb.draw_buffer = bufs[0];
            }
            Some(k) => {
                for (i, &b) in bufs.iter().enumerate() {
                    if b == gl::NONE || b == gl::COLOR_ATTACHMENT0 + i as u32 {
                        continue;
                    }
                    let known = b == gl::BACK || (gl::COLOR_ATTACHMENT0..=gl::COLOR_ATTACHMENT15).contains(&b);
                    return self.err(if known { gl::INVALID_OPERATION } else { gl::INVALID_ENUM });
                }
                let fb = self.framebuffers.get_mut(k);
                fb.draw_buffers = [gl::NONE; DRAW_BUFFERS];
                fb.draw_buffers[..bufs.len()].copy_from_slice(bufs);
            }
        }
    }

    /// `glReadBuffer`.
    pub fn read_buffer(&mut self, src: u32) {
        let color = (gl::COLOR_ATTACHMENT0..=gl::COLOR_ATTACHMENT15).contains(&src);
        if !(color || matches!(src, gl::BACK | gl::NONE)) {
            return self.err(gl::INVALID_ENUM);
        }
        match self.read_framebuffer {
            None => {
                if color {
                    return self.err(gl::INVALID_OPERATION);
                }
                self.default_fb.read_buffer = src;
            }
            Some(k) => {
                if src == gl::BACK || (color && (src - gl::COLOR_ATTACHMENT0) as usize >= DRAW_BUFFERS) {
                    return self.err(gl::INVALID_OPERATION);
                }
                self.framebuffers.get_mut(k).read_buffer = src;
            }
        }
    }

    /// `glInvalidateFramebuffer` (a hint: checked, then ignored).
    pub fn invalidate_framebuffer(&mut self, target: u32, attachments: &[u32]) {
        let Ok(fb) = self.framebuffer_target(target) else { return self.err(gl::INVALID_ENUM) };
        for &a in attachments {
            let ok = match fb {
                None => matches!(a, gl::COLOR | gl::DEPTH | gl::STENCIL),
                Some(_) => {
                    if (gl::COLOR_ATTACHMENT0..=gl::COLOR_ATTACHMENT15).contains(&a) {
                        if (a - gl::COLOR_ATTACHMENT0) as usize >= DRAW_BUFFERS {
                            return self.err(gl::INVALID_OPERATION);
                        }
                        true
                    } else {
                        matches!(a, gl::DEPTH_ATTACHMENT | gl::STENCIL_ATTACHMENT | gl::DEPTH_STENCIL_ATTACHMENT)
                    }
                }
            };
            if !ok {
                return self.err(gl::INVALID_ENUM);
            }
        }
    }

    /// `glInvalidateSubFramebuffer` (a hint).
    pub fn invalidate_sub_framebuffer(
        &mut self,
        target: u32,
        attachments: &[u32],
        _x: i32,
        _y: i32,
        width: i32,
        height: i32,
    ) {
        if width < 0 || height < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        self.invalidate_framebuffer(target, attachments);
    }

    // ---- Clearing ---------------------------------------------------------------------

    fn scissor_rect(&self) -> Option<Rect> {
        let s = self.state.scissor;
        self.state.scissor_test.then_some(Rect { x: s[0], y: s[1], w: s[2], h: s[3] })
    }

    /// `glClear`.
    pub fn clear(&mut self, mask: u32) {
        if mask & !(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT) != 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let fb = match self.draw_state_framebuffer() {
            Ok(fb) => fb,
            Err(_) => return self.err(gl::INVALID_FRAMEBUFFER_OPERATION),
        };
        if self.state.rasterizer_discard {
            return;
        }
        let mut c = Clear {
            scissor: self.scissor_rect(),
            color_mask: self.state.color_mask,
            stencil_mask: self.state.stencil_front.write_mask,
            ..Default::default()
        };
        if mask & gl::COLOR_BUFFER_BIT != 0 {
            for i in 0..DRAW_BUFFERS {
                let Some(att) = fb.draw_buffers[i] else { continue };
                let Some(internal) = self.color_internal(self.draw_framebuffer, att as usize) else { continue };
                // Clearing integer buffers with Clear is undefined; leave
                // them alone.
                if !internal.format.is_integer() && fb.colors[att as usize].is_some() {
                    c.colors[att as usize] = Some(color_bits(internal.format, self.state.clear_color));
                }
            }
        }
        if mask & gl::DEPTH_BUFFER_BIT != 0 && self.state.depth_write && fb.depth.is_some() {
            c.depth = Some(self.state.clear_depth);
        }
        if mask & gl::STENCIL_BUFFER_BIT != 0 && fb.stencil.is_some() {
            c.stencil = Some(self.state.clear_stencil);
        }
        if c.colors.iter().any(Option::is_some) || c.depth.is_some() || c.stencil.is_some() {
            self.backend.clear(&fb, &c);
        }
    }

    /// `glClearBuffer*`: `kind` is the scalar class the call passes
    /// (`Float`, `Sint`, `Uint`).
    fn clear_buffer(
        &mut self,
        buffer: u32,
        drawbuffer: i32,
        value: [u32; 4],
        kind: Class,
        depth: Option<f32>,
        stencil: Option<i32>,
    ) {
        let fb = match self.draw_state_framebuffer() {
            Ok(fb) => fb,
            Err(_) => return self.err(gl::INVALID_FRAMEBUFFER_OPERATION),
        };
        if self.state.rasterizer_discard {
            return;
        }
        let mut c = Clear {
            scissor: self.scissor_rect(),
            color_mask: self.state.color_mask,
            stencil_mask: self.state.stencil_front.write_mask,
            ..Default::default()
        };
        match buffer {
            gl::COLOR => {
                let Some(att) = fb.draw_buffers[drawbuffer as usize] else { return };
                let Some(internal) = self.color_internal(self.draw_framebuffer, att as usize) else { return };
                let class = match internal.format.class() {
                    Class::Uint => Class::Uint,
                    Class::Sint => Class::Sint,
                    _ => Class::Float,
                };
                // A value of another kind than the buffer's is undefined:
                // nothing is written.
                if class != kind || fb.colors[att as usize].is_none() {
                    return;
                }
                c.colors[att as usize] = Some(value);
            }
            _ => {
                c.depth = depth.filter(|_| fb.depth.is_some() && self.state.depth_write).map(super::clamp01);
                c.stencil = stencil.filter(|_| fb.stencil.is_some());
            }
        }
        self.backend.clear(&fb, &c);
    }

    /// `glClearBufferfv`: `COLOR` (four values) or `DEPTH` (one).
    pub fn clear_bufferfv(&mut self, buffer: u32, drawbuffer: i32, value: &[f32]) {
        match buffer {
            gl::COLOR if (0..DRAW_BUFFERS as i32).contains(&drawbuffer) && value.len() >= 4 => {
                let v = [value[0], value[1], value[2], value[3]].map(f32::to_bits);
                self.clear_buffer(buffer, drawbuffer, v, Class::Float, None, None);
            }
            gl::DEPTH if drawbuffer == 0 && !value.is_empty() => {
                self.clear_buffer(buffer, 0, [0; 4], Class::Float, Some(value[0]), None);
            }
            gl::COLOR | gl::DEPTH => self.err(gl::INVALID_VALUE),
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glClearBufferiv`: `COLOR` (four values) or `STENCIL` (one).
    pub fn clear_bufferiv(&mut self, buffer: u32, drawbuffer: i32, value: &[i32]) {
        match buffer {
            gl::COLOR if (0..DRAW_BUFFERS as i32).contains(&drawbuffer) && value.len() >= 4 => {
                let v = [value[0], value[1], value[2], value[3]].map(|x| x as u32);
                self.clear_buffer(buffer, drawbuffer, v, Class::Sint, None, None);
            }
            gl::STENCIL if drawbuffer == 0 && !value.is_empty() => {
                self.clear_buffer(buffer, 0, [0; 4], Class::Sint, None, Some(value[0]));
            }
            gl::COLOR | gl::STENCIL => self.err(gl::INVALID_VALUE),
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glClearBufferuiv`: `COLOR` only.
    pub fn clear_bufferuiv(&mut self, buffer: u32, drawbuffer: i32, value: &[u32]) {
        match buffer {
            gl::COLOR if (0..DRAW_BUFFERS as i32).contains(&drawbuffer) && value.len() >= 4 => {
                let v = [value[0], value[1], value[2], value[3]];
                self.clear_buffer(buffer, drawbuffer, v, Class::Uint, None, None);
            }
            gl::COLOR => self.err(gl::INVALID_VALUE),
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    /// `glClearBufferfi`: `DEPTH_STENCIL`.
    pub fn clear_bufferfi(&mut self, buffer: u32, drawbuffer: i32, depth: f32, stencil: i32) {
        if buffer != gl::DEPTH_STENCIL {
            return self.err(gl::INVALID_ENUM);
        }
        if drawbuffer != 0 {
            return self.err(gl::INVALID_VALUE);
        }
        self.clear_buffer(buffer, 0, [0; 4], Class::Float, Some(depth), Some(stencil));
    }

    // ---- Reading pixels ------------------------------------------------------------------

    /// `glReadPixels`.
    pub fn read_pixels<'a>(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        ty: u32,
        out: impl Into<PixelsMut<'a>>,
    ) {
        let out = out.into();
        if !format::is_format(format) || !format::is_type(ty) {
            return self.err(gl::INVALID_ENUM);
        }
        if width < 0 || height < 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let src = match self.read_color() {
            Ok(s) => s,
            Err(e) => return self.err(e),
        };
        let pair = (format, ty);
        if pair != required_read(&src.internal)
            && pair != natural_read(&src.internal)
            && !(src.internal.format == Format::Rgb10A2Unorm && pair == (gl::RGBA, gl::UNSIGNED_INT_2_10_10_10_REV))
        {
            return self.err(gl::INVALID_OPERATION);
        }
        let client = Client::new(format, ty);
        let pack = self.state.pack;
        let Some(layout) = pixels::layout(client, width as u32, height as u32, 1, &pack) else {
            return self.err(gl::INVALID_VALUE);
        };
        // Where the pixels go: the application's memory or the pack buffer.
        let buffer = self.bound.pixel_pack;
        let target = match (out, buffer) {
            (PixelsMut::Data(d), None) => {
                if d.len() < layout.end {
                    return self.err(gl::INVALID_OPERATION);
                }
                Ok(d)
            }
            (PixelsMut::Offset(o), Some(k)) => {
                let (_, size) = self.buffer_store(k);
                let mapped = self.buffers.get(k).map.is_some();
                if mapped
                    || o.checked_add(layout.end).is_none_or(|e| e > size)
                    || !o.is_multiple_of(client.type_bytes())
                {
                    return self.err(gl::INVALID_OPERATION);
                }
                Err((k, o))
            }
            _ => return self.err(gl::INVALID_OPERATION),
        };
        // Only pixels inside the read buffer are read.
        let fb = &src.fb;
        let (x0, y0) = (x.max(0), y.max(0));
        let x1 = (x as i64 + width as i64).min(fb.width as i64) as i32;
        let y1 = (y as i64 + height as i64).min(fb.height as i64) as i32;
        let (cw, ch) = ((x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32);
        let f = src.internal.format;
        let row = cw as usize * f.bytes();
        let Some(mut texels) = pixels::try_zeroed(row * ch as usize) else { return self.err(gl::OUT_OF_MEMORY) };
        if cw > 0 && ch > 0 {
            let s = src.surface;
            let region = Region::new(x0 as u32, y0 as u32, s.layer, cw, ch, 1);
            self.backend.read(s.resource, s.level, region, &mut texels, row, row * ch as usize);
        }
        let mut sub = layout;
        sub.start += (x0 - x) as usize * layout.pixel + (y0 - y) as usize * layout.row;
        sub.row_bytes = cw as usize * layout.pixel;
        match target {
            Ok(d) => {
                if cw > 0 && ch > 0 {
                    pixels::pack(f, &texels, cw, ch, client, &sub, d);
                }
            }
            Err((k, offset)) => {
                // Read-modify-write the range, so that bytes between rows
                // keep their values.
                let Some(mut range) = self.read_buffer_range(k, offset, layout.end) else {
                    return self.err(gl::OUT_OF_MEMORY);
                };
                if cw > 0 && ch > 0 {
                    pixels::pack(f, &texels, cw, ch, client, &sub, &mut range);
                }
                if let (Some(r), false) = (self.buffers.get(k).resource, range.is_empty()) {
                    let n = range.len();
                    self.backend.write(r, 0, Region::bytes(offset, n), &range, n, n);
                }
            }
        }
    }

    // ---- Blits ------------------------------------------------------------------------------

    /// `glBlitFramebuffer`.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_framebuffer(
        &mut self,
        src_x0: i32,
        src_y0: i32,
        src_x1: i32,
        src_y1: i32,
        dst_x0: i32,
        dst_y0: i32,
        dst_x1: i32,
        dst_y1: i32,
        mask: u32,
        filter: u32,
    ) {
        if mask & !(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT) != 0 {
            return self.err(gl::INVALID_VALUE);
        }
        let filter = match filter {
            gl::NEAREST => Filter::Nearest,
            gl::LINEAR => Filter::Linear,
            _ => return self.err(gl::INVALID_ENUM),
        };
        if filter == Filter::Linear && mask & (gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT) != 0 {
            return self.err(gl::INVALID_OPERATION);
        }
        let (Ok(src), Ok(dst)) = (self.read_state_framebuffer(), self.draw_state_framebuffer()) else {
            return self.err(gl::INVALID_FRAMEBUFFER_OPERATION);
        };
        if dst.samples > 0 {
            return self.err(gl::INVALID_OPERATION);
        }
        let src_rect = [src_x0, src_y0, src_x1, src_y1];
        let dst_rect = [dst_x0, dst_y0, dst_x1, dst_y1];
        let same_size =
            (src_x1 - src_x0).abs() == (dst_x1 - dst_x0).abs() && (src_y1 - src_y0).abs() == (dst_y1 - dst_y0).abs();
        let mut color = mask & gl::COLOR_BUFFER_BIT != 0;
        let mut depth = mask & gl::DEPTH_BUFFER_BIT != 0;
        let mut stencil = mask & gl::STENCIL_BUFFER_BIT != 0;
        let read = src.draw_buffers[0];
        let read_internal = read.and_then(|i| self.color_internal(self.read_framebuffer, i as usize));
        if color {
            match (read, read_internal) {
                (Some(ri), Some(rf)) if src.colors[ri as usize].is_some() => {
                    let rclass = rf.format.class();
                    for i in 0..DRAW_BUFFERS {
                        let Some(att) = dst.draw_buffers[i] else { continue };
                        let Some(df) = self.color_internal(self.draw_framebuffer, att as usize) else { continue };
                        let dclass = df.format.class();
                        let int = |c: Class| matches!(c, Class::Uint | Class::Sint);
                        if int(rclass) != int(dclass) || (int(rclass) && rclass != dclass) {
                            return self.err(gl::INVALID_OPERATION);
                        }
                        if int(rclass) && filter == Filter::Linear {
                            return self.err(gl::INVALID_OPERATION);
                        }
                        if src.samples > 0 && (!same_size || rf.format != df.format) {
                            return self.err(gl::INVALID_OPERATION);
                        }
                        if src.colors[ri as usize] == dst.colors[att as usize] {
                            return self.err(gl::INVALID_OPERATION);
                        }
                    }
                }
                // A buffer missing from either framebuffer is ignored.
                _ => color = false,
            }
        }
        let ds_internal = |c: &Context, read: bool, depth: bool| -> Option<Internal> {
            let fbo = if read { c.read_framebuffer } else { c.draw_framebuffer };
            match fbo {
                None => c.default_fb.depth_internal,
                Some(k) => {
                    let fb = c.framebuffers.get(k);
                    c.attachment_internal(if depth { fb.depth } else { fb.stencil })
                }
            }
        };
        for (on, is_depth, s, d) in
            [(&mut depth, true, src.depth, dst.depth), (&mut stencil, false, src.stencil, dst.stencil)]
        {
            if !*on {
                continue;
            }
            if s.is_none() || d.is_none() {
                *on = false;
                continue;
            }
            let (sf, df) = (ds_internal(self, true, is_depth), ds_internal(self, false, is_depth));
            if sf.map(|f| f.format) != df.map(|f| f.format) || s == d {
                return self.err(gl::INVALID_OPERATION);
            }
            if src.samples > 0 && !same_size {
                return self.err(gl::INVALID_OPERATION);
            }
        }
        if !(color || depth || stencil) {
            return;
        }
        let blit = Blit {
            src,
            src_color: if color { read } else { None },
            dst,
            src_rect,
            dst_rect,
            color,
            depth,
            stencil,
            filter,
            scissor: self.scissor_rect(),
        };
        self.backend.blit(&blit);
    }

    // ---- Renderbuffers ------------------------------------------------------------------

    /// `glGenRenderbuffers`.
    pub fn gen_renderbuffers(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.renderbuffers.generate();
        }
    }

    /// One renderbuffer name.
    pub fn gen_renderbuffer(&mut self) -> u32 {
        self.renderbuffers.generate()
    }

    /// `glIsRenderbuffer`.
    pub fn is_renderbuffer(&self, name: u32) -> bool {
        self.renderbuffers.key(name).is_some()
    }

    /// `glBindRenderbuffer`.
    pub fn bind_renderbuffer(&mut self, target: u32, name: u32) {
        if target != gl::RENDERBUFFER {
            return self.err(gl::INVALID_ENUM);
        }
        self.renderbuffer =
            if name == 0 { None } else { Some(self.renderbuffers.get_or_create(name, Renderbuffer::default)) };
    }

    /// `glDeleteRenderbuffers`.
    pub fn delete_renderbuffers(&mut self, names: &[u32]) {
        for &name in names {
            if let Some(key) = self.renderbuffers.key(name) {
                if self.renderbuffer == Some(key) {
                    self.renderbuffer = None;
                }
                self.detach_from_bound_framebuffers(|a| a == Attachment::Renderbuffer(key));
            }
            if let Some((_, Some(dead))) = self.renderbuffers.delete(name) {
                self.destroy_renderbuffer(dead);
            }
        }
    }

    fn destroy_renderbuffer(&mut self, rb: Renderbuffer) {
        if let Some(r) = rb.resource {
            self.backend.destroy_resource(r);
        }
    }

    /// The sample counts the renderer supports for a format (descending).
    pub(crate) fn sample_counts(&self, i: &Internal) -> &'static [u32] {
        let renderable = i.renderable || i.float_renderable || i.format.has_depth() || i.format.has_stencil();
        if !renderable || i.format.is_integer() || self.backend.caps().max_samples < 4 { &[] } else { &[4] }
    }

    /// `glRenderbufferStorage`.
    pub fn renderbuffer_storage(&mut self, target: u32, internalformat: u32, width: i32, height: i32) {
        self.renderbuffer_storage_multisample(target, 0, internalformat, width, height);
    }

    /// `glRenderbufferStorageMultisample`.
    pub fn renderbuffer_storage_multisample(
        &mut self,
        target: u32,
        samples: i32,
        internalformat: u32,
        width: i32,
        height: i32,
    ) {
        if target != gl::RENDERBUFFER {
            return self.err(gl::INVALID_ENUM);
        }
        let caps_float = self.backend.caps().color_buffer_float;
        let Some(i) = format::sized(internalformat).copied().filter(|i| {
            i.renderable || (i.float_renderable && caps_float) || i.format.has_depth() || i.format.has_stencil()
        }) else {
            return self.err(gl::INVALID_ENUM);
        };
        let max = self.backend.caps().max_renderbuffer_size as i32;
        if samples < 0 || width < 0 || height < 0 || width > max || height > max {
            return self.err(gl::INVALID_VALUE);
        }
        if samples > 0 && i.format.is_integer() {
            return self.err(gl::INVALID_OPERATION);
        }
        let counts = self.sample_counts(&i);
        let samples = if samples == 0 {
            0
        } else {
            match counts.iter().rev().find(|&&c| c >= samples as u32) {
                Some(&c) => c,
                None => return self.err(gl::INVALID_OPERATION),
            }
        };
        let Some(key) = self.renderbuffer else { return self.err(gl::INVALID_OPERATION) };
        let (w, h) = (width as u32, height as u32);
        let resource = if w > 0 && h > 0 {
            let desc = ResourceDesc {
                target: Target::Renderbuffer,
                format: i.format,
                width: w,
                height: h,
                depth: 1,
                levels: 1,
                samples,
            };
            match self.backend.create_resource(&desc) {
                Ok(r) => Some(r),
                Err(_) => return self.err(gl::OUT_OF_MEMORY),
            }
        } else {
            None
        };
        let rb = self.renderbuffers.get_mut(key);
        let old = core::mem::replace(rb, Renderbuffer { internal: Some(i), width: w, height: h, samples, resource });
        self.destroy_renderbuffer(old);
    }

    /// `glGetRenderbufferParameteriv` (one value).
    pub fn get_renderbuffer_parameteri(&mut self, target: u32, pname: u32) -> i32 {
        if target != gl::RENDERBUFFER {
            self.err(gl::INVALID_ENUM);
            return 0;
        }
        let Some(key) = self.renderbuffer else {
            self.err(gl::INVALID_OPERATION);
            return 0;
        };
        let rb = *self.renderbuffers.get(key);
        let bits = rb.internal.map_or([0; 6], |i| i.bits);
        match pname {
            gl::RENDERBUFFER_WIDTH => rb.width as i32,
            gl::RENDERBUFFER_HEIGHT => rb.height as i32,
            gl::RENDERBUFFER_INTERNAL_FORMAT => rb.internal.map_or(gl::RGBA4, |i| i.gl) as i32,
            gl::RENDERBUFFER_SAMPLES => rb.samples as i32,
            gl::RENDERBUFFER_RED_SIZE => i32::from(bits[0]),
            gl::RENDERBUFFER_GREEN_SIZE => i32::from(bits[1]),
            gl::RENDERBUFFER_BLUE_SIZE => i32::from(bits[2]),
            gl::RENDERBUFFER_ALPHA_SIZE => i32::from(bits[3]),
            gl::RENDERBUFFER_DEPTH_SIZE => i32::from(bits[4]),
            gl::RENDERBUFFER_STENCIL_SIZE => i32::from(bits[5]),
            _ => {
                self.err(gl::INVALID_ENUM);
                0
            }
        }
    }

    /// `glGetInternalformativ`.
    pub fn get_internalformativ(&mut self, target: u32, internalformat: u32, pname: u32, out: &mut [i32]) {
        if target != gl::RENDERBUFFER {
            return self.err(gl::INVALID_ENUM);
        }
        let caps_float = self.backend.caps().color_buffer_float;
        let Some(i) = format::sized(internalformat).copied().filter(|i| {
            i.renderable || (i.float_renderable && caps_float) || i.format.has_depth() || i.format.has_stencil()
        }) else {
            return self.err(gl::INVALID_ENUM);
        };
        let counts: Vec<i32> = self.sample_counts(&i).iter().map(|&c| c as i32).collect();
        match pname {
            gl::NUM_SAMPLE_COUNTS => super::vertex::fill(out, &[counts.len() as i32]),
            gl::SAMPLES => super::vertex::fill(out, &counts),
            _ => self.err(gl::INVALID_ENUM),
        }
    }

    // ---- Attachment queries -------------------------------------------------------------

    /// `glGetFramebufferAttachmentParameteriv` (one value).
    pub fn get_framebuffer_attachment_parameteri(&mut self, target: u32, attachment: u32, pname: u32) -> i32 {
        let Ok(fbo) = self.framebuffer_target(target) else {
            self.err(gl::INVALID_ENUM);
            return 0;
        };
        // What is attached, and its format.
        let (kind, name, a, internal) = match fbo {
            None => {
                let fb = self.default_fb;
                let i = match attachment {
                    gl::BACK => fb.color_internal,
                    gl::DEPTH => fb.depth_internal.filter(|i| i.format.has_depth()),
                    gl::STENCIL => fb.depth_internal.filter(|i| i.format.has_stencil()),
                    _ => {
                        self.err(gl::INVALID_OPERATION);
                        return 0;
                    }
                };
                let kind = if i.is_some() { gl::FRAMEBUFFER_DEFAULT } else { gl::NONE };
                (kind, 0, Attachment::None, i)
            }
            Some(k) => {
                let fb = self.framebuffers.get(k).clone();
                let a = match attachment {
                    gl::DEPTH_ATTACHMENT => fb.depth,
                    gl::STENCIL_ATTACHMENT => fb.stencil,
                    gl::DEPTH_STENCIL_ATTACHMENT => {
                        if fb.depth != fb.stencil {
                            self.err(gl::INVALID_OPERATION);
                            return 0;
                        }
                        fb.depth
                    }
                    a if (gl::COLOR_ATTACHMENT0..=gl::COLOR_ATTACHMENT15).contains(&a) => {
                        match fb.colors.get((a - gl::COLOR_ATTACHMENT0) as usize) {
                            Some(&c) => c,
                            None => {
                                self.err(gl::INVALID_OPERATION);
                                return 0;
                            }
                        }
                    }
                    _ => {
                        self.err(gl::INVALID_ENUM);
                        return 0;
                    }
                };
                let (kind, name) = match a {
                    Attachment::None => (gl::NONE, 0),
                    Attachment::Renderbuffer(k) => (gl::RENDERBUFFER, self.renderbuffers.name(k)),
                    Attachment::Texture { key, .. } => (gl::TEXTURE, self.textures.name(key)),
                };
                (kind, name, a, self.attachment_internal(a))
            }
        };
        if pname == gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE {
            return kind as i32;
        }
        if kind == gl::NONE {
            if pname != gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME {
                self.err(gl::INVALID_OPERATION);
            }
            return 0;
        }
        let bits = internal.map_or([0; 6], |i| i.bits);
        match pname {
            gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME if kind != gl::FRAMEBUFFER_DEFAULT => name as i32,
            gl::FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL if kind == gl::TEXTURE => match a {
                Attachment::Texture { level, .. } => level as i32,
                _ => 0,
            },
            gl::FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE if kind == gl::TEXTURE => match a {
                Attachment::Texture { key, layer, .. } if self.textures.get(key).target == gl::TEXTURE_CUBE_MAP => {
                    (gl::TEXTURE_CUBE_MAP_POSITIVE_X + layer) as i32
                }
                _ => 0,
            },
            gl::FRAMEBUFFER_ATTACHMENT_TEXTURE_LAYER if kind == gl::TEXTURE => match a {
                Attachment::Texture { key, layer, .. } if self.textures.get(key).target != gl::TEXTURE_CUBE_MAP => {
                    layer as i32
                }
                _ => 0,
            },
            gl::FRAMEBUFFER_ATTACHMENT_RED_SIZE => i32::from(bits[0]),
            gl::FRAMEBUFFER_ATTACHMENT_GREEN_SIZE => i32::from(bits[1]),
            gl::FRAMEBUFFER_ATTACHMENT_BLUE_SIZE => i32::from(bits[2]),
            gl::FRAMEBUFFER_ATTACHMENT_ALPHA_SIZE => i32::from(bits[3]),
            gl::FRAMEBUFFER_ATTACHMENT_DEPTH_SIZE => i32::from(bits[4]),
            gl::FRAMEBUFFER_ATTACHMENT_STENCIL_SIZE => i32::from(bits[5]),
            gl::FRAMEBUFFER_ATTACHMENT_COMPONENT_TYPE => {
                if attachment == gl::DEPTH_STENCIL_ATTACHMENT {
                    self.err(gl::INVALID_OPERATION);
                    return 0;
                }
                internal.map_or(gl::NONE, |i| match i.format.class() {
                    Class::Stencil => gl::UNSIGNED_INT,
                    _ => i.component.gl(),
                }) as i32
            }
            gl::FRAMEBUFFER_ATTACHMENT_COLOR_ENCODING => {
                (if internal.is_some_and(|i| i.format.is_srgb()) { gl::SRGB } else { gl::LINEAR }) as i32
            }
            _ => {
                self.err(gl::INVALID_ENUM);
                0
            }
        }
    }

    /// The draw framebuffer's color attachment 0 format (state queries:
    /// `RED_BITS`...), and its depth/stencil format.
    pub(crate) fn draw_formats(&self) -> (Option<Internal>, Option<Internal>, Option<Internal>) {
        match self.draw_framebuffer {
            None => {
                let d = self.default_fb.depth_internal;
                (
                    self.default_fb.color_internal,
                    d.filter(|i| i.format.has_depth()),
                    d.filter(|i| i.format.has_stencil()),
                )
            }
            Some(k) => {
                let fb = self.framebuffers.get(k);
                let color = fb.draw_buffers[0];
                let c = (color != gl::NONE)
                    .then(|| fb.colors.get((color - gl::COLOR_ATTACHMENT0) as usize).copied())
                    .flatten()
                    .and_then(|a| self.attachment_internal(a));
                (c, self.attachment_internal(fb.depth), self.attachment_internal(fb.stencil))
            }
        }
    }

    /// The read buffer's `IMPLEMENTATION_COLOR_READ_FORMAT`/`_TYPE`.
    pub(crate) fn color_read_pair(&mut self) -> Option<(u32, u32)> {
        let fbo = self.read_framebuffer;
        let index = match fbo {
            None => 0,
            Some(k) => {
                let r = self.framebuffers.get(k).read_buffer;
                if r == gl::NONE {
                    return None;
                }
                (r - gl::COLOR_ATTACHMENT0) as usize
            }
        };
        self.color_internal(fbo, index).map(|i| natural_read(&i))
    }
}

impl Context {
    /// Finishes rendering and shows the default framebuffer in a window's
    /// pixels, scaled to their size.
    pub fn present_to(&mut self, dst: &mut crate::backend::Present<'_>) {
        let Some((color, _, w, h)) = self.default_color_buffer() else { return };
        self.backend.present(color, w, h, dst);
    }
}
