//! The GL's objects, and how names refer to them.
//!
//! Objects of one kind live in a [`Store`]. A name refers to an object, and
//! so do container objects: vertex arrays refer to buffers, framebuffers to
//! textures and renderbuffers, transform feedback objects to buffers.
//! Deleting a name frees the name at once, but the object lives on while a
//! container still refers to it (OpenGL ES 3.0, appendix D.1.3); the
//! context's own bindings to it are reset instead (appendix D.1.2). Inside
//! the library objects are known by a [`Key`], which is never reused, so a
//! container cannot end up referring to a different object that took the
//! name.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::backend::{ResourceDesc, ResourceId};
use crate::format::Internal;
use crate::gl;

use super::{DRAW_BUFFERS, VERTEX_ATTRIBS};

/// An object's identity inside the library.
pub type Key = u32;

struct Entry<T> {
    value: T,
    /// The name, until it is deleted.
    name: Option<u32>,
    /// One for the name, one for each container referring to the object.
    refs: u32,
}

/// The objects of one kind and their names.
pub struct Store<T> {
    /// Names in use: generated (`None` until the object is created) or
    /// naming an object.
    names: BTreeMap<u32, Option<Key>>,
    objects: BTreeMap<Key, Entry<T>>,
    next_key: Key,
    next_name: u32,
}

impl<T> Default for Store<T> {
    fn default() -> Store<T> {
        Store { names: BTreeMap::new(), objects: BTreeMap::new(), next_key: 1, next_name: 1 }
    }
}

impl<T> Store<T> {
    /// A name not in use (`Gen*`); it names no object until one is created.
    pub fn generate(&mut self) -> u32 {
        loop {
            let n = self.next_name;
            self.next_name = n.checked_add(1).unwrap_or(1);
            if let alloc::collections::btree_map::Entry::Vacant(e) = self.names.entry(n) {
                e.insert(None);
                return n;
            }
        }
    }

    /// Whether `name` was generated or names an object.
    pub fn is_reserved(&self, name: u32) -> bool {
        self.names.contains_key(&name)
    }

    /// The object `name` names.
    pub fn key(&self, name: u32) -> Option<Key> {
        self.names.get(&name).copied().flatten()
    }

    /// Creates an object without a name (the default objects, name 0).
    pub fn create_unnamed(&mut self, value: T) -> Key {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1).max(1);
        self.objects.insert(key, Entry { value, name: None, refs: 1 });
        key
    }

    /// Creates the object for `name` (generated or not).
    pub fn create(&mut self, name: u32, value: T) -> Key {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1).max(1);
        self.objects.insert(key, Entry { value, name: Some(name), refs: 1 });
        self.names.insert(name, Some(key));
        key
    }

    /// The object `name` names, created with `make` if the name names none
    /// yet (`Bind*` for the objects binding creates).
    pub fn get_or_create(&mut self, name: u32, make: impl FnOnce() -> T) -> Key {
        match self.key(name) {
            Some(k) => k,
            None => self.create(name, make()),
        }
    }

    pub fn get(&self, key: Key) -> &T {
        &self.objects[&key].value
    }

    pub fn get_mut(&mut self, key: Key) -> &mut T {
        &mut self.objects.get_mut(&key).expect("a live object").value
    }

    pub fn try_get(&self, key: Key) -> Option<&T> {
        self.objects.get(&key).map(|e| &e.value)
    }

    pub fn try_get_mut(&mut self, key: Key) -> Option<&mut T> {
        self.objects.get_mut(&key).map(|e| &mut e.value)
    }

    pub fn by_name(&self, name: u32) -> Option<&T> {
        self.key(name).map(|k| self.get(k))
    }

    /// The name of an object (0 once deleted).
    pub fn name(&self, key: Key) -> u32 {
        self.objects.get(&key).and_then(|e| e.name).unwrap_or(0)
    }

    /// A container starts referring to the object.
    pub fn retain(&mut self, key: Key) {
        if let Some(e) = self.objects.get_mut(&key) {
            e.refs += 1;
        }
    }

    /// A reference goes away; returns the object if it was the last.
    #[must_use]
    pub fn release(&mut self, key: Key) -> Option<T> {
        let e = self.objects.get_mut(&key)?;
        e.refs -= 1;
        if e.refs == 0 { self.objects.remove(&key).map(|e| e.value) } else { None }
    }

    /// Deletes a name (`Delete*`): returns the object's key, if it named
    /// one, and the object itself if nothing else refers to it.
    #[must_use]
    pub fn delete(&mut self, name: u32) -> Option<(Key, Option<T>)> {
        let key = self.names.remove(&name)??;
        if let Some(e) = self.objects.get_mut(&key) {
            e.name = None;
        }
        let dead = self.release(key);
        Some((key, dead))
    }

    /// All live objects (for teardown).
    pub fn drain(&mut self) -> Vec<T> {
        self.names.clear();
        core::mem::take(&mut self.objects).into_values().map(|e| e.value).collect()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Key, &mut T)> {
        self.objects.iter_mut().map(|(k, e)| (*k, &mut e.value))
    }

    pub fn iter(&self) -> impl Iterator<Item = (Key, &T)> {
        self.objects.iter().map(|(k, e)| (*k, &e.value))
    }
}

// ---- Buffers -------------------------------------------------------------------

/// A buffer object.
#[derive(Debug, Default)]
pub struct Buffer {
    /// The storage (none while the size is 0).
    pub resource: Option<ResourceId>,
    pub size: usize,
    pub usage: u32,
    pub map: Option<Mapping>,
}

/// A mapped range of a buffer (`MapBufferRange`): a copy the application
/// reads and writes, written back when flushed or unmapped.
#[derive(Debug)]
pub struct Mapping {
    pub offset: usize,
    pub access: u32,
    pub data: Vec<u8>,
}

// ---- Vertex arrays ---------------------------------------------------------------

/// A generic vertex attribute array's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttribArray {
    pub enabled: bool,
    pub size: u8,
    pub ty: u32,
    pub normalized: bool,
    /// `VertexAttribIPointer`.
    pub integer: bool,
    /// As specified (0: tightly packed).
    pub stride: u32,
    pub offset: usize,
    pub divisor: u32,
    pub buffer: Option<Key>,
}

impl Default for AttribArray {
    fn default() -> AttribArray {
        AttribArray {
            enabled: false,
            size: 4,
            ty: gl::FLOAT,
            normalized: false,
            integer: false,
            stride: 0,
            offset: 0,
            divisor: 0,
            buffer: None,
        }
    }
}

/// A vertex array object.
#[derive(Clone, Debug, Default)]
pub struct VertexArray {
    pub attribs: [AttribArray; VERTEX_ATTRIBS],
    pub element_buffer: Option<Key>,
}

// ---- Textures and samplers -------------------------------------------------------

/// Sampler state, as texture and sampler objects hold it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SamplerParams {
    pub min_filter: u32,
    pub mag_filter: u32,
    /// S, T and R.
    pub wrap: [u32; 3],
    pub min_lod: f32,
    pub max_lod: f32,
    pub compare_mode: u32,
    pub compare_func: u32,
    pub max_anisotropy: f32,
}

impl Default for SamplerParams {
    fn default() -> SamplerParams {
        SamplerParams {
            min_filter: gl::NEAREST_MIPMAP_LINEAR,
            mag_filter: gl::LINEAR,
            wrap: [gl::REPEAT; 3],
            min_lod: -1000.0,
            max_lod: 1000.0,
            compare_mode: gl::NONE,
            compare_func: gl::LEQUAL,
            max_anisotropy: 1.0,
        }
    }
}

/// The most mipmap levels a texture has (sizes up to 2^15).
pub const MAX_LEVELS: usize = 16;

/// One image of a texture (a level of a face).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Image {
    pub internal: Internal,
    /// The compressed format it was specified in, if any (it is stored
    /// decompressed).
    pub compressed: Option<u32>,
    /// It was specified with an unsized internal format (`GL_RGBA`...),
    /// which accepts more `(format, type)` pairs in `TexSubImage*`.
    pub unsized_format: bool,
    pub width: u32,
    pub height: u32,
    /// Depth (3D) or layers (2D arrays); 1 otherwise.
    pub depth: u32,
    /// The image's storage when it is not in the texture's main storage:
    /// a resource of its own, one level deep.
    pub own: Option<ResourceId>,
}

/// A texture's main storage: a resource holding a mipmap chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Storage {
    pub resource: ResourceId,
    pub desc: ResourceDesc,
    /// The level the resource's level 0 holds.
    pub base: u32,
}

/// A texture object.
#[derive(Clone, Debug)]
pub struct Texture {
    /// `TEXTURE_2D`, `TEXTURE_3D`, `TEXTURE_2D_ARRAY` or `TEXTURE_CUBE_MAP`.
    pub target: u32,
    pub sampler: SamplerParams,
    pub base_level: u32,
    pub max_level: u32,
    /// `TEXTURE_SWIZZLE_R` to `_A`.
    pub swizzle: [u32; 4],
    /// The levels of `TexStorage*`, for immutable textures.
    pub immutable_levels: Option<u32>,
    /// Images by face (one face but for cube maps), then level.
    pub images: Vec<[Option<Image>; MAX_LEVELS]>,
    pub storage: Option<Storage>,
}

impl Texture {
    pub fn new(target: u32) -> Texture {
        let faces = if target == gl::TEXTURE_CUBE_MAP { 6 } else { 1 };
        Texture {
            target,
            sampler: SamplerParams::default(),
            base_level: 0,
            max_level: 1000,
            swizzle: [gl::RED, gl::GREEN, gl::BLUE, gl::ALPHA],
            immutable_levels: None,
            images: alloc::vec![[None; MAX_LEVELS]; faces],
            storage: None,
        }
    }

    pub fn image(&self, face: usize, level: u32) -> Option<&Image> {
        self.images.get(face)?.get(level as usize)?.as_ref()
    }
}

/// A sampler object.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sampler {
    pub params: SamplerParams,
}

// ---- Renderbuffers and framebuffers ----------------------------------------------

/// A renderbuffer object.
#[derive(Clone, Copy, Debug, Default)]
pub struct Renderbuffer {
    pub internal: Option<Internal>,
    pub width: u32,
    pub height: u32,
    pub samples: u32,
    pub resource: Option<ResourceId>,
}

/// What a framebuffer attachment point holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Attachment {
    #[default]
    None,
    Renderbuffer(Key),
    /// A texture image: its level and its cube face, array layer or 3D
    /// slice.
    Texture {
        key: Key,
        level: u32,
        layer: u32,
    },
}

/// A framebuffer object.
#[derive(Clone, Debug)]
pub struct FramebufferObject {
    pub colors: [Attachment; DRAW_BUFFERS],
    pub depth: Attachment,
    pub stencil: Attachment,
    /// `DrawBuffers`: per fragment output, `COLOR_ATTACHMENTi` or `NONE`.
    pub draw_buffers: [u32; DRAW_BUFFERS],
    pub read_buffer: u32,
}

impl Default for FramebufferObject {
    fn default() -> FramebufferObject {
        let mut draw_buffers = [gl::NONE; DRAW_BUFFERS];
        draw_buffers[0] = gl::COLOR_ATTACHMENT0;
        FramebufferObject {
            colors: [Attachment::None; DRAW_BUFFERS],
            depth: Attachment::None,
            stencil: Attachment::None,
            draw_buffers,
            read_buffer: gl::COLOR_ATTACHMENT0,
        }
    }
}

impl FramebufferObject {
    /// Every attachment point and what it holds.
    pub fn attachments(&self) -> impl Iterator<Item = Attachment> + '_ {
        self.colors.iter().copied().chain([self.depth, self.stencil])
    }
}

/// The window's framebuffer (name 0).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultFramebuffer {
    pub color: Option<ResourceId>,
    /// Depth and stencil share one resource.
    pub depth_stencil: Option<ResourceId>,
    pub color_internal: Option<Internal>,
    pub depth_internal: Option<Internal>,
    pub width: u32,
    pub height: u32,
    /// `BACK` or `NONE`.
    pub draw_buffer: u32,
    pub read_buffer: u32,
}

// ---- Queries, transform feedback, sync ---------------------------------------------

/// A query object.
#[derive(Clone, Copy, Debug)]
pub struct Query {
    /// The target it was first begun with.
    pub target: u32,
    /// The renderer's query for the last `BeginQuery`.
    pub backend: Option<crate::backend::QueryId>,
    /// The result, once known.
    pub result: Option<u64>,
    pub active: bool,
    /// The context's count of primitives written by transform feedback
    /// when the query began.
    pub start: u64,
}

/// A transform feedback buffer binding.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FeedbackBinding {
    pub buffer: Option<Key>,
    pub offset: usize,
    /// 0: to the end of the buffer (`BindBufferBase`).
    pub size: usize,
}

/// A transform feedback object.
#[derive(Clone, Debug, Default)]
pub struct TransformFeedback {
    pub bindings: [FeedbackBinding; super::FEEDBACK_BINDINGS],
    /// The generic `TRANSFORM_FEEDBACK_BUFFER` binding.
    pub generic: Option<Key>,
    pub active: bool,
    pub paused: bool,
    /// `POINTS`, `LINES` or `TRIANGLES`.
    pub primitive_mode: u32,
    /// The program in use when feedback began (its executable).
    pub program: Option<u32>,
    /// Vertices recorded since `BeginTransformFeedback`.
    pub vertices: u64,
}

/// A sync object.
#[derive(Clone, Copy, Debug)]
pub struct Sync {
    /// The renderer's fence.
    pub fence: u64,
    pub signaled: bool,
}
