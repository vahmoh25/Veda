//! Buffer objects (OpenGL ES 3.0 section 2.10).

use alloc::vec::Vec;

use super::{Context, FEEDBACK_BINDINGS, IndexedBinding, Key, UNIFORM_BUFFER_BINDINGS};
use crate::backend::{Region, ResourceDesc, Target};
use crate::context::objects::{Buffer, Mapping};
use crate::format::Format;
use crate::gl;
use crate::pixels::try_zeroed;

/// `UNIFORM_BUFFER_OFFSET_ALIGNMENT`: what Direct3D 11 needs, so that the
/// virgl renderer can pass ranges through.
pub const UNIFORM_BUFFER_OFFSET_ALIGNMENT: usize = 256;

/// The largest buffer (`MAX_ELEMENT_INDEX`-like sanity limit): 2 GiB.
pub const MAX_BUFFER_SIZE: usize = 1 << 31;

fn usage_valid(usage: u32) -> bool {
    matches!(
        usage,
        gl::STREAM_DRAW
            | gl::STREAM_READ
            | gl::STREAM_COPY
            | gl::STATIC_DRAW
            | gl::STATIC_READ
            | gl::STATIC_COPY
            | gl::DYNAMIC_DRAW
            | gl::DYNAMIC_READ
            | gl::DYNAMIC_COPY
    )
}

impl Context {
    /// The buffer bound to `target`: `Err` for an unknown target.
    pub(crate) fn buffer_target(&self, target: u32) -> Result<Option<Key>, ()> {
        Ok(match target {
            gl::ARRAY_BUFFER => self.bound.array,
            gl::ELEMENT_ARRAY_BUFFER => self.vao().element_buffer,
            gl::COPY_READ_BUFFER => self.bound.copy_read,
            gl::COPY_WRITE_BUFFER => self.bound.copy_write,
            gl::PIXEL_PACK_BUFFER => self.bound.pixel_pack,
            gl::PIXEL_UNPACK_BUFFER => self.bound.pixel_unpack,
            gl::TRANSFORM_FEEDBACK_BUFFER => self.tf().generic,
            gl::UNIFORM_BUFFER => self.bound.uniform,
            _ => return Err(()),
        })
    }

    /// The buffer bound to `target`, recording `INVALID_ENUM` for an unknown
    /// target and `INVALID_OPERATION` if none is bound.
    fn bound_buffer(&mut self, target: u32) -> Option<Key> {
        match self.buffer_target(target) {
            Err(()) => {
                self.err(gl::INVALID_ENUM);
                None
            }
            Ok(None) => {
                self.err(gl::INVALID_OPERATION);
                None
            }
            Ok(Some(k)) => Some(k),
        }
    }

    /// `glGenBuffers`.
    pub fn gen_buffers(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.buffers.generate();
        }
    }

    /// One buffer name (`glGenBuffers` with `n` = 1).
    pub fn gen_buffer(&mut self) -> u32 {
        self.buffers.generate()
    }

    /// `glIsBuffer`.
    pub fn is_buffer(&self, name: u32) -> bool {
        self.buffers.key(name).is_some()
    }

    /// `glBindBuffer`.
    pub fn bind_buffer(&mut self, target: u32, name: u32) {
        if self.buffer_target(target).is_err() {
            return self.err(gl::INVALID_ENUM);
        }
        let key = if name == 0 { None } else { Some(self.buffers.get_or_create(name, Buffer::default)) };
        self.set_buffer_binding(target, key);
    }

    /// Points `target` at `key` (retaining it for container objects).
    fn set_buffer_binding(&mut self, target: u32, key: Option<Key>) {
        match target {
            gl::ARRAY_BUFFER => self.bound.array = key,
            gl::COPY_READ_BUFFER => self.bound.copy_read = key,
            gl::COPY_WRITE_BUFFER => self.bound.copy_write = key,
            gl::PIXEL_PACK_BUFFER => self.bound.pixel_pack = key,
            gl::PIXEL_UNPACK_BUFFER => self.bound.pixel_unpack = key,
            gl::UNIFORM_BUFFER => self.bound.uniform = key,
            gl::ELEMENT_ARRAY_BUFFER => {
                if let Some(k) = key {
                    self.buffers.retain(k);
                }
                let old = core::mem::replace(&mut self.vao_mut().element_buffer, key);
                self.release_buffer(old);
            }
            gl::TRANSFORM_FEEDBACK_BUFFER => {
                if let Some(k) = key {
                    self.buffers.retain(k);
                }
                let old = core::mem::replace(&mut self.tf_mut().generic, key);
                self.release_buffer(old);
            }
            _ => unreachable!("checked by the caller"),
        }
    }

    /// Drops a container's reference to a buffer, destroying it if that
    /// was the last.
    pub(crate) fn release_buffer(&mut self, key: Option<Key>) {
        if let Some(b) = key.and_then(|k| self.buffers.release(k)) {
            self.destroy_buffer(b);
        }
    }

    fn destroy_buffer(&mut self, b: Buffer) {
        if let Some(r) = b.resource {
            self.backend.destroy_resource(r);
        }
    }

    /// `glDeleteBuffers`.
    pub fn delete_buffers(&mut self, names: &[u32]) {
        for &name in names {
            let Some(key) = self.buffers.key(name) else {
                // A generated name that names no buffer yet becomes unused.
                let _ = self.buffers.delete(name);
                continue;
            };
            // Unbind it from the context and the bound containers
            // (appendix D.1.2).
            let b = &mut self.bound;
            for slot in [
                &mut b.array,
                &mut b.copy_read,
                &mut b.copy_write,
                &mut b.pixel_pack,
                &mut b.pixel_unpack,
                &mut b.uniform,
            ] {
                if *slot == Some(key) {
                    *slot = None;
                }
            }
            for i in &mut b.uniform_indexed {
                if i.buffer == Some(key) {
                    *i = IndexedBinding::default();
                }
            }
            let mut released = 0;
            let vao = self.vao_mut();
            if vao.element_buffer == Some(key) {
                vao.element_buffer = None;
                released += 1;
            }
            for a in &mut vao.attribs {
                if a.buffer == Some(key) {
                    a.buffer = None;
                    released += 1;
                }
            }
            let tf = self.tf_mut();
            if tf.generic == Some(key) {
                tf.generic = None;
                released += 1;
            }
            for b in &mut tf.bindings {
                if b.buffer == Some(key) {
                    b.buffer = None;
                    released += 1;
                }
            }
            for _ in 0..released {
                self.release_buffer(Some(key));
            }
            // A mapping ends with the buffer's name.
            if let Some(b) = self.buffers.try_get_mut(key) {
                b.map = None;
            }
            if let Some((_, Some(dead))) = self.buffers.delete(name) {
                self.destroy_buffer(dead);
            }
        }
    }

    /// `glBufferData` with data.
    pub fn buffer_data(&mut self, target: u32, data: &[u8], usage: u32) {
        self.buffer_data_impl(target, data.len(), Some(data), usage);
    }

    /// `glBufferData` without data (the contents are zero).
    pub fn buffer_data_size(&mut self, target: u32, size: usize, usage: u32) {
        self.buffer_data_impl(target, size, None, usage);
    }

    fn buffer_data_impl(&mut self, target: u32, size: usize, data: Option<&[u8]>, usage: u32) {
        if self.buffer_target(target).is_err() || !usage_valid(usage) {
            return self.err(gl::INVALID_ENUM);
        }
        if size > isize::MAX as usize {
            return self.err(gl::INVALID_VALUE);
        }
        let Some(key) = self.bound_buffer(target) else { return };
        if size > MAX_BUFFER_SIZE {
            return self.err(gl::OUT_OF_MEMORY);
        }
        let resource = if size == 0 {
            None
        } else {
            let desc = ResourceDesc {
                target: Target::Buffer,
                format: Format::R8Uint,
                width: size as u32,
                height: 1,
                depth: 1,
                levels: 1,
                samples: 0,
            };
            match self.backend.create_resource(&desc) {
                Ok(r) => Some(r),
                Err(_) => return self.err(gl::OUT_OF_MEMORY),
            }
        };
        if let (Some(r), Some(d)) = (resource, data) {
            self.backend.write(r, 0, Region::bytes(0, size), d, size, size);
        }
        let b = self.buffers.get_mut(key);
        let old = core::mem::replace(b, Buffer { resource, size, usage, map: None });
        if let Some(r) = old.resource {
            self.backend.destroy_resource(r);
        }
    }

    /// `glBufferSubData`.
    pub fn buffer_sub_data(&mut self, target: u32, offset: usize, data: &[u8]) {
        let Some(key) = self.bound_buffer(target) else { return };
        let b = self.buffers.get(key);
        if offset.checked_add(data.len()).is_none_or(|end| end > b.size) {
            return self.err(gl::INVALID_VALUE);
        }
        if b.map.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        if let (Some(r), false) = (b.resource, data.is_empty()) {
            self.backend.write(r, 0, Region::bytes(offset, data.len()), data, data.len(), data.len());
        }
    }

    /// `glCopyBufferSubData`.
    pub fn copy_buffer_sub_data(
        &mut self,
        read_target: u32,
        write_target: u32,
        read_offset: usize,
        write_offset: usize,
        size: usize,
    ) {
        if self.buffer_target(read_target).is_err() || self.buffer_target(write_target).is_err() {
            return self.err(gl::INVALID_ENUM);
        }
        let Some(src) = self.bound_buffer(read_target) else { return };
        let Some(dst) = self.bound_buffer(write_target) else { return };
        let (s, d) = (self.buffers.get(src), self.buffers.get(dst));
        let fits = |off: usize, total: usize| off.checked_add(size).is_some_and(|e| e <= total);
        if !fits(read_offset, s.size) || !fits(write_offset, d.size) {
            return self.err(gl::INVALID_VALUE);
        }
        if src == dst && read_offset < write_offset + size && write_offset < read_offset + size {
            return self.err(gl::INVALID_VALUE);
        }
        if s.map.is_some() || d.map.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        if let (Some(rs), Some(rd), true) = (s.resource, d.resource, size > 0) {
            self.backend.copy_buffer(rs, read_offset, rd, write_offset, size);
        }
    }

    /// `glMapBufferRange`: the mapped bytes (which [`Context::mapped_buffer`]
    /// also returns until the buffer is unmapped), or `None` after an
    /// error.
    pub fn map_buffer_range(&mut self, target: u32, offset: usize, length: usize, access: u32) -> Option<&mut [u8]> {
        let key = self.bound_buffer(target)?;
        let all = gl::MAP_READ_BIT
            | gl::MAP_WRITE_BIT
            | gl::MAP_INVALIDATE_RANGE_BIT
            | gl::MAP_INVALIDATE_BUFFER_BIT
            | gl::MAP_FLUSH_EXPLICIT_BIT
            | gl::MAP_UNSYNCHRONIZED_BIT;
        let b = self.buffers.get(key);
        if offset.checked_add(length).is_none_or(|e| e > b.size) || access & !all != 0 {
            self.err(gl::INVALID_VALUE);
            return None;
        }
        let read = access & gl::MAP_READ_BIT != 0;
        let write = access & gl::MAP_WRITE_BIT != 0;
        let discard = gl::MAP_INVALIDATE_RANGE_BIT | gl::MAP_INVALIDATE_BUFFER_BIT | gl::MAP_UNSYNCHRONIZED_BIT;
        if length == 0
            || b.map.is_some()
            || !(read || write)
            || (read && access & discard != 0)
            || (access & gl::MAP_FLUSH_EXPLICIT_BIT != 0 && !write)
        {
            self.err(gl::INVALID_OPERATION);
            return None;
        }
        let Some(mut data) = try_zeroed(length) else {
            self.err(gl::OUT_OF_MEMORY);
            return None;
        };
        // The current contents, unless the application discards them.
        let invalidate = access & (gl::MAP_INVALIDATE_RANGE_BIT | gl::MAP_INVALIDATE_BUFFER_BIT) != 0;
        if let (Some(r), false) = (b.resource, invalidate) {
            self.backend.read(r, 0, Region::bytes(offset, length), &mut data, length, length);
        }
        let b = self.buffers.get_mut(key);
        b.map = Some(Mapping { offset, access, data });
        b.map.as_mut().map(|m| &mut m.data[..])
    }

    /// The mapped range of the buffer bound to `target` (what
    /// `glMapBufferRange` returned), if it is mapped.
    pub fn mapped_buffer(&mut self, target: u32) -> Option<&mut [u8]> {
        let key = self.buffer_target(target).ok()??;
        self.buffers.get_mut(key).map.as_mut().map(|m| &mut m.data[..])
    }

    /// `glFlushMappedBufferRange` (`offset` within the mapping).
    pub fn flush_mapped_buffer_range(&mut self, target: u32, offset: usize, length: usize) {
        let Some(key) = self.bound_buffer(target) else { return };
        let b = self.buffers.get(key);
        let Some(m) = &b.map else { return self.err(gl::INVALID_OPERATION) };
        if m.access & gl::MAP_FLUSH_EXPLICIT_BIT == 0 {
            return self.err(gl::INVALID_OPERATION);
        }
        if offset.checked_add(length).is_none_or(|e| e > m.data.len()) {
            return self.err(gl::INVALID_VALUE);
        }
        if let (Some(r), true) = (b.resource, length > 0) {
            let at = m.offset + offset;
            self.backend.write(r, 0, Region::bytes(at, length), &m.data[offset..offset + length], length, length);
        }
    }

    /// `glUnmapBuffer`.
    pub fn unmap_buffer(&mut self, target: u32) -> bool {
        let Some(key) = self.bound_buffer(target) else { return false };
        let b = self.buffers.get_mut(key);
        let Some(m) = b.map.take() else {
            self.err(gl::INVALID_OPERATION);
            return false;
        };
        let explicit = m.access & gl::MAP_FLUSH_EXPLICIT_BIT != 0;
        if let (Some(r), true, false) = (b.resource, m.access & gl::MAP_WRITE_BIT != 0, explicit) {
            let n = m.data.len();
            self.backend.write(r, 0, Region::bytes(m.offset, n), &m.data, n, n);
        }
        true
    }

    /// `glBindBufferBase`.
    pub fn bind_buffer_base(&mut self, target: u32, index: u32, name: u32) {
        self.bind_indexed(target, index, name, 0, 0, false);
    }

    /// `glBindBufferRange`.
    pub fn bind_buffer_range(&mut self, target: u32, index: u32, name: u32, offset: usize, size: usize) {
        self.bind_indexed(target, index, name, offset, size, true);
    }

    fn bind_indexed(&mut self, target: u32, index: u32, name: u32, offset: usize, size: usize, range: bool) {
        let count = match target {
            gl::UNIFORM_BUFFER => UNIFORM_BUFFER_BINDINGS,
            gl::TRANSFORM_FEEDBACK_BUFFER => FEEDBACK_BINDINGS,
            _ => return self.err(gl::INVALID_ENUM),
        };
        if index as usize >= count {
            return self.err(gl::INVALID_VALUE);
        }
        if range && name != 0 {
            let aligned = match target {
                gl::UNIFORM_BUFFER => offset.is_multiple_of(UNIFORM_BUFFER_OFFSET_ALIGNMENT),
                _ => offset.is_multiple_of(4) && size.is_multiple_of(4),
            };
            if size == 0 || size > isize::MAX as usize || offset > isize::MAX as usize || !aligned {
                return self.err(gl::INVALID_VALUE);
            }
        }
        if target == gl::TRANSFORM_FEEDBACK_BUFFER && self.tf().active {
            return self.err(gl::INVALID_OPERATION);
        }
        let key = if name == 0 { None } else { Some(self.buffers.get_or_create(name, Buffer::default)) };
        let (offset, size) = if range && key.is_some() { (offset, size) } else { (0, 0) };
        self.set_buffer_binding(target, key);
        let i = index as usize;
        if target == gl::UNIFORM_BUFFER {
            self.bound.uniform_indexed[i] = IndexedBinding { buffer: key, offset, size };
        } else {
            if let Some(k) = key {
                self.buffers.retain(k);
            }
            let tf = self.tf_mut();
            let old = core::mem::replace(&mut tf.bindings[i], super::FeedbackBinding { buffer: key, offset, size });
            self.release_buffer(old.buffer);
        }
    }

    /// `glGetBufferParameteri64v` (one value).
    pub fn get_buffer_parameteri64(&mut self, target: u32, pname: u32) -> i64 {
        let Some(key) = self.bound_buffer(target) else { return 0 };
        let b = self.buffers.get(key);
        let m = b.map.as_ref();
        match pname {
            gl::BUFFER_SIZE => b.size as i64,
            gl::BUFFER_USAGE => i64::from(if b.usage == 0 { gl::STATIC_DRAW } else { b.usage }),
            gl::BUFFER_ACCESS_FLAGS => i64::from(m.map_or(0, |m| m.access)),
            gl::BUFFER_MAPPED => i64::from(m.is_some()),
            gl::BUFFER_MAP_OFFSET => m.map_or(0, |m| m.offset as i64),
            gl::BUFFER_MAP_LENGTH => m.map_or(0, |m| m.data.len() as i64),
            _ => {
                self.err(gl::INVALID_ENUM);
                0
            }
        }
    }

    /// `glGetBufferParameteriv` (one value).
    pub fn get_buffer_parameteri(&mut self, target: u32, pname: u32) -> i32 {
        let v = self.get_buffer_parameteri64(target, pname);
        v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }

    /// A buffer's resource and size (for draws and pixel transfers).
    pub(crate) fn buffer_store(&self, key: Key) -> (Option<crate::backend::ResourceId>, usize) {
        let b = self.buffers.get(key);
        (b.resource, b.size)
    }

    /// Reads a range of a buffer (bounds checked by the caller).
    pub(crate) fn read_buffer_range(&mut self, key: Key, offset: usize, len: usize) -> Option<Vec<u8>> {
        let mut out = try_zeroed(len)?;
        if let (Some(r), true) = (self.buffers.get(key).resource, len > 0) {
            self.backend.read(r, 0, Region::bytes(offset, len), &mut out, len, len);
        }
        Some(out)
    }
}
